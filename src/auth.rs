//! Device pairing and bearer-token authentication.
//!
//! `iris pair` (or an owner via the API) creates a short one-time code. A
//! device exchanges it at `POST /api/pair` for a long-lived random token.
//! Only SHA-256 hashes of codes and tokens are stored, so a copy of the
//! database cannot be used to impersonate a device.

use crate::db::{self, Db, Device};
use crate::util::{Result, hex, random_bytes, sha256_hex};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Crockford-style alphabet without look-alikes (no 0/O, 1/I/L, U).
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const CODE_LEN: usize = 8;
pub const CODE_TTL: Duration = Duration::from_secs(10 * 60);
pub const TOKEN_PREFIX: &str = "iris_";

/// Generate a random pairing code, e.g. "7KQM-3XPA" (~39 bits).
pub fn generate_code() -> String {
    // Rejection sampling keeps the distribution uniform.
    let limit = 256 - (256 % CODE_ALPHABET.len());
    let mut chars = String::with_capacity(CODE_LEN);
    while chars.len() < CODE_LEN {
        for b in random_bytes::<16>() {
            if (b as usize) < limit && chars.len() < CODE_LEN {
                chars.push(CODE_ALPHABET[b as usize % CODE_ALPHABET.len()] as char);
            }
        }
    }
    format!("{}-{}", &chars[..CODE_LEN / 2], &chars[CODE_LEN / 2..])
}

/// Canonical form used for hashing: separators and spaces removed,
/// uppercased, so "7kqm 3xpa" matches "7KQM-3XPA".
pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

pub fn hash_code(code: &str) -> String {
    sha256_hex(&format!("iris-pair:{}", normalize_code(code)))
}

pub fn generate_token() -> String {
    format!("{TOKEN_PREFIX}{}", hex(&random_bytes::<32>()))
}

pub fn hash_token(token: &str) -> String {
    sha256_hex(&format!("iris-token:{token}"))
}

/// Create a pairing code for `user_id`; returns (code, expires_at_ms).
pub fn create_pairing_code(conn: &rusqlite::Connection, user_id: &str) -> Result<(String, i64)> {
    let code = generate_code();
    let ttl = CODE_TTL.as_millis() as i64;
    db::insert_pairing_code(conn, &hash_code(&code), user_id, ttl)?;
    conn.execute("INSERT INTO settings (key, value, updated_at) VALUES ('pairing.generation', '1', ?1)
        ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1, updated_at = excluded.updated_at", [crate::util::now_ms()])?;
    Ok((code, crate::util::now_ms() + ttl))
}

/// Throttles pairing attempts from one source for the rest of the window.
/// Generating a new code resets failures without invalidating other codes.
/// Attempts still being checked count against the limit too, so concurrent
/// requests cannot queue more guesses.
pub struct PairLimiter {
    state: Mutex<Limits>,
    max_failures: u32,
}

#[derive(Default)]
struct Limits {
    failures: u32,
    in_flight: u32,
    window_start: Option<Instant>,
    generation: u64,
}

const MAX_FAILURES: u32 = 5;
const WINDOW: Duration = Duration::from_secs(15 * 60);

impl Default for PairLimiter {
    fn default() -> Self {
        Self {
            state: Mutex::new(Limits::default()),
            max_failures: MAX_FAILURES,
        }
    }
}

/// One pairing attempt holding its place under the limit until resolved.
pub struct Attempt<'a> {
    limiter: &'a PairLimiter,
    done: bool,
    generation: u64,
}

impl Attempt<'_> {
    pub fn succeeded(mut self) {
        self.done = true;
        let mut s = self.limiter.lock();
        s.in_flight -= 1;
        if s.generation == self.generation {
            s.failures = 0;
            s.window_start = None;
        }
    }

    /// Returns true if this failure tripped the lock.
    pub fn failed(mut self) -> bool {
        self.done = true;
        let mut s = self.limiter.lock();
        s.in_flight -= 1;
        if s.generation != self.generation {
            return false;
        }
        if s.window_start.is_none() {
            s.window_start = Some(Instant::now());
        }
        s.failures += 1;
        s.failures == self.limiter.max_failures
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.limiter.lock().in_flight -= 1;
        }
    }
}

impl PairLimiter {
    fn lock(&self) -> std::sync::MutexGuard<'_, Limits> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn reset_generation(&self, generation: u64) {
        let mut s = self.lock();
        if generation > s.generation {
            s.generation = generation;
            s.failures = 0;
            s.window_start = None;
        }
    }

    /// Start an attempt, or None while locked (or while enough attempts
    /// are already being checked to reach the limit).
    pub fn begin(&self) -> Option<Attempt<'_>> {
        let mut s = self.lock();
        if s.window_start.is_some_and(|t| t.elapsed() > WINDOW) {
            s.failures = 0;
            s.window_start = None;
        }
        if s.failures + s.in_flight >= self.max_failures {
            return None;
        }
        s.in_flight += 1;
        Some(Attempt {
            limiter: self,
            done: false,
            generation: s.generation,
        })
    }

    #[cfg(test)]
    fn locked(&self) -> bool {
        self.begin().is_none()
    }
}

/// Peer IPs come from the listener, never forwarded headers. The bounded
/// table falls back to a much higher global limit when no peer is available.
pub struct PairLimiters {
    sources: Mutex<std::collections::HashMap<std::net::IpAddr, std::sync::Arc<PairLimiter>>>,
    fallback: std::sync::Arc<PairLimiter>,
}

impl Default for PairLimiters {
    fn default() -> Self {
        Self {
            sources: Mutex::default(),
            fallback: std::sync::Arc::new(PairLimiter {
                max_failures: 500,
                ..PairLimiter::default()
            }),
        }
    }
}

impl PairLimiters {
    pub fn for_source(&self, source: Option<std::net::IpAddr>) -> std::sync::Arc<PairLimiter> {
        let Some(source) = source else {
            return self.fallback.clone();
        };
        let mut sources = self.sources.lock().unwrap_or_else(|p| p.into_inner());
        sources.retain(|_, limiter| {
            let s = limiter.lock();
            std::sync::Arc::strong_count(limiter) > 1
                || s.in_flight > 0
                || s.window_start.is_some_and(|t| t.elapsed() <= WINDOW)
        });
        if let Some(limiter) = sources.get(&source) {
            return limiter.clone();
        }
        if sources.len() >= 1024 {
            return self.fallback.clone();
        }
        sources.entry(source).or_default().clone()
    }
}

pub enum PairError {
    Locked,
    Invalid,
    Internal(crate::util::BoxError),
}

/// Exchange a pairing code for a device token.
pub async fn pair_device(
    db: &Db,
    limiter: &PairLimiter,
    code: &str,
    device_name: &str,
) -> std::result::Result<(String, Device), PairError> {
    let generation = db
        .call(|c| db::get_setting(c, "pairing.generation"))
        .await
        .map_err(PairError::Internal)?;
    limiter.reset_generation(generation.and_then(|s| s.parse().ok()).unwrap_or(0));
    let Some(attempt) = limiter.begin() else {
        return Err(PairError::Locked);
    };
    let code_hash = hash_code(code);
    let name = device_name.trim().chars().take(80).collect::<String>();
    let name = if name.is_empty() {
        "Device".to_string()
    } else {
        name
    };
    let token = generate_token();
    let token_hash = hash_token(&token);
    let res = db
        .call(move |c| {
            let Some(user_id) = db::consume_pairing_code(c, &code_hash)? else {
                return Ok(None);
            };
            db::insert_device(c, &user_id, &name, &token_hash).map(Some)
        })
        .await;
    match res {
        Ok(Some(device)) => {
            attempt.succeeded();
            Ok((token, device))
        }
        Ok(None) => {
            attempt.failed();
            Err(PairError::Invalid)
        }
        Err(e) => Err(PairError::Internal(e)),
    }
}

/// Resolve a bearer token to its device.
pub async fn authenticate(db: &Db, token: &str) -> Result<Option<Device>> {
    if !token.starts_with(TOKEN_PREFIX) || token.len() > 200 {
        return Ok(None);
    }
    let hash = hash_token(token);
    db.call(move |c| {
        let d = db::device_by_token_hash(c, &hash)?;
        if let Some(d) = &d {
            db::touch_device(c, &d.id)?;
        }
        Ok(d)
    })
    .await
}

/// Extract `Bearer <token>` from an Authorization header value.
pub fn bearer(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_well_formed_and_normalize() {
        for _ in 0..50 {
            let c = generate_code();
            assert_eq!(c.len(), CODE_LEN + 1);
            assert_eq!(&c[4..5], "-");
            assert!(
                c.bytes()
                    .filter(|b| *b != b'-')
                    .all(|b| CODE_ALPHABET.contains(&b))
            );
        }
        assert_eq!(normalize_code(" 7kqm-3xpa "), "7KQM3XPA");
        assert_eq!(hash_code("7kqm 3xpa"), hash_code("7KQM-3XPA"));
        assert_ne!(generate_code(), generate_code());
    }

    #[test]
    fn tokens_and_bearer_parsing() {
        let t = generate_token();
        assert!(t.starts_with(TOKEN_PREFIX) && t.len() == TOKEN_PREFIX.len() + 64);
        assert_ne!(hash_token(&t), t);
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
    }

    #[tokio::test]
    async fn pairing_flow_is_one_time() {
        let db = Db::open_in_memory().unwrap();
        let limiter = PairLimiter::default();
        let (code, _) = db
            .with(|c| {
                let owner = db::owner_user_id(c)?.unwrap();
                Ok(create_pairing_code(c, &owner))
            })
            .unwrap()
            .unwrap();
        let (token, device) = pair_device(&db, &limiter, &code.to_lowercase(), "Phone")
            .await
            .ok()
            .unwrap();
        assert_eq!(device.name, "Phone");
        assert_eq!(
            authenticate(&db, &token).await.unwrap().unwrap().id,
            device.id
        );
        assert!(authenticate(&db, "iris_wrong").await.unwrap().is_none());
        assert!(authenticate(&db, "no-prefix").await.unwrap().is_none());
        // Second use of the same code fails.
        assert!(matches!(
            pair_device(&db, &limiter, &code, "Again").await,
            Err(PairError::Invalid)
        ));
        // Revoked devices can no longer authenticate.
        db.with(|c| db::revoke_device(c, &device.user_id, &device.id))
            .unwrap();
        assert!(authenticate(&db, &token).await.unwrap().is_none());
        // No plaintext secrets in the database.
        let leaked = db
            .with(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM devices WHERE token_hash = ?1",
                    [&token],
                    |r| r.get::<_, i64>(0),
                )
            })
            .unwrap();
        assert_eq!(leaked, 0);
    }

    #[tokio::test]
    async fn new_pairing_code_resets_lockout() {
        let db = Db::open_in_memory().unwrap();
        let limiter = PairLimiter::default();
        let (code, _) = db
            .with(|c| {
                let owner = db::owner_user_id(c)?.unwrap();
                Ok(create_pairing_code(c, &owner))
            })
            .unwrap()
            .unwrap();
        for _ in 0..MAX_FAILURES {
            assert!(matches!(
                pair_device(&db, &limiter, "ZZZZ-ZZZZ", "x").await,
                Err(PairError::Invalid)
            ));
        }
        assert!(matches!(
            pair_device(&db, &limiter, &code, "x").await,
            Err(PairError::Locked)
        ));
        // A new code from the CLI resets this source without a daemon restart.
        let (fresh, _) = db
            .with(|c| Ok(create_pairing_code(c, &db::owner_user_id(c)?.unwrap())))
            .unwrap()
            .unwrap();
        assert!(pair_device(&db, &limiter, &fresh, "x").await.is_ok());
        // Other outstanding codes are still valid.
        assert!(pair_device(&db, &limiter, &code, "x").await.is_ok());
    }

    #[test]
    fn pairing_limits_are_scoped_to_peer_ip() {
        let limits = PairLimiters::default();
        let a = limits.for_source(Some("192.0.2.1".parse().unwrap()));
        for _ in 0..MAX_FAILURES {
            a.begin().unwrap().failed();
        }
        assert!(
            limits
                .for_source(Some("192.0.2.1".parse().unwrap()))
                .begin()
                .is_none()
        );
        assert!(
            limits
                .for_source(Some("192.0.2.2".parse().unwrap()))
                .begin()
                .is_some()
        );
        let fallback = limits.for_source(None);
        for _ in 0..MAX_FAILURES {
            fallback.begin().unwrap().failed();
        }
        assert!(fallback.begin().is_some());
    }

    #[test]
    fn attempts_being_checked_count_against_the_limit() {
        let limiter = PairLimiter::default();
        // Many requests at once: only MAX_FAILURES get to guess.
        let attempts: Vec<_> = (0..20).filter_map(|_| limiter.begin()).collect();
        assert_eq!(attempts.len(), MAX_FAILURES as usize);
        assert!(limiter.locked());
        let tripped: Vec<bool> = attempts.into_iter().map(|a| a.failed()).collect();
        assert_eq!(tripped.iter().filter(|t| **t).count(), 1);
        assert!(limiter.locked());
        // An attempt that ends without a verdict (an internal error) frees
        // its place; a success clears the count.
        let limiter = PairLimiter::default();
        drop(limiter.begin());
        let a = limiter.begin().unwrap();
        let b = limiter.begin().unwrap();
        b.failed();
        a.succeeded();
        assert_eq!(limiter.lock().failures, 0);
        assert_eq!(limiter.lock().in_flight, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_guesses_stop_at_the_limit() {
        let db = Db::open_in_memory().unwrap();
        let limiter = std::sync::Arc::new(PairLimiter::default());
        let guesses: Vec<_> = (0..30)
            .map(|i| {
                let (db, limiter) = (db.clone(), limiter.clone());
                tokio::spawn(async move {
                    pair_device(&db, &limiter, &format!("ZZZZ-{i:04}"), "x").await
                })
            })
            .collect();
        let (mut invalid, mut locked) = (0, 0);
        for g in guesses {
            match g.await.unwrap() {
                Err(PairError::Invalid) => invalid += 1,
                Err(PairError::Locked) => locked += 1,
                Ok(_) => panic!("a wrong code paired a device"),
                Err(PairError::Internal(e)) => panic!("internal error: {e}"),
            }
        }
        assert_eq!(
            invalid, MAX_FAILURES,
            "exactly the allowed guesses were checked"
        );
        assert_eq!(locked, 30 - MAX_FAILURES);
    }
}
