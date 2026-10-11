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
    Ok((code, crate::util::now_ms() + ttl))
}

/// Throttles pairing attempts: after `MAX_FAILURES` bad codes within the
/// window, every outstanding code is burned and new attempts are refused
/// for the rest of the window. Attempts still being checked count against
/// the limit too, so concurrent requests cannot queue more guesses.
pub struct PairLimiter {
    state: Mutex<Limits>,
}

#[derive(Default)]
struct Limits {
    failures: u32,
    in_flight: u32,
    window_start: Option<Instant>,
}

const MAX_FAILURES: u32 = 5;
const WINDOW: Duration = Duration::from_secs(15 * 60);

impl Default for PairLimiter {
    fn default() -> Self {
        Self {
            state: Mutex::new(Limits::default()),
        }
    }
}

/// One pairing attempt holding its place under the limit until resolved.
pub struct Attempt<'a> {
    limiter: &'a PairLimiter,
    done: bool,
}

impl Attempt<'_> {
    pub fn succeeded(mut self) {
        self.done = true;
        let mut s = self.limiter.lock();
        s.in_flight -= 1;
        s.failures = 0;
        s.window_start = None;
    }

    /// Returns true if this failure tripped the lock.
    pub fn failed(mut self) -> bool {
        self.done = true;
        let mut s = self.limiter.lock();
        s.in_flight -= 1;
        if s.window_start.is_none() {
            s.window_start = Some(Instant::now());
        }
        s.failures += 1;
        s.failures == MAX_FAILURES
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

    /// Start an attempt, or None while locked (or while enough attempts
    /// are already being checked to reach the limit).
    pub fn begin(&self) -> Option<Attempt<'_>> {
        let mut s = self.lock();
        if s.window_start.is_some_and(|t| t.elapsed() > WINDOW) {
            s.failures = 0;
            s.window_start = None;
        }
        if s.failures + s.in_flight >= MAX_FAILURES {
            return None;
        }
        s.in_flight += 1;
        Some(Attempt {
            limiter: self,
            done: false,
        })
    }

    #[cfg(test)]
    fn locked(&self) -> bool {
        self.begin().is_none()
    }

    #[cfg(test)]
    fn record_success(&self) {
        let mut s = self.lock();
        s.failures = 0;
        s.window_start = None;
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
            if attempt.failed() {
                let _ = db.call(db::burn_pairing_codes).await;
                crate::warn!("too many failed pairing attempts; outstanding codes revoked");
            }
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
    async fn repeated_failures_lock_and_burn_codes() {
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
        // Even after the lock clears, the burned code no longer works.
        limiter.record_success();
        assert!(matches!(
            pair_device(&db, &limiter, &code, "x").await,
            Err(PairError::Invalid)
        ));
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
        let mut invalid = 0;
        for g in guesses {
            if matches!(g.await.unwrap(), Err(PairError::Invalid)) {
                invalid += 1;
            }
        }
        assert!(invalid <= MAX_FAILURES, "{invalid} guesses were checked");
    }
}
