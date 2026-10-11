//! Feed pipeline persistence: per-user state, sources, seen keys, daily
//! engagement, and price watches.

use super::http::Stats;
use super::rank::{Engagement, Topic, words};
use crate::util::{new_id, now_ms};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

pub const DEFAULT_DAILY_SIZE: u32 = 10;
pub const MIN_DAILY_SIZE: u32 = 5;
pub const MAX_DAILY_SIZE: u32 = 30;

/// What the daily interests call produced.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Interests {
    pub topics: Vec<Topic>,
    /// Where the user lives, if their notes say (for nearby events).
    #[serde(default)]
    pub location: Option<String>,
    pub generated_at: i64,
    #[serde(default)]
    pub generated_day: String,
    /// False when built without a model (no engine available).
    #[serde(default)]
    pub by_model: bool,
    /// ISO 639-1 code of the language the user reads (default English).
    #[serde(default)]
    pub language: Option<String>,
}

/// Outcome of the last pipeline run (shown in the app for transparency).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunReport {
    pub at: i64,
    pub sources_checked: u32,
    pub items_found: u32,
    pub published: u32,
    pub summarized: u32,
    pub model_used: bool,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub requests: u32,
    #[serde(default)]
    pub robots_blocked: u32,
}

impl RunReport {
    pub fn with_stats(mut self, s: Stats) -> Self {
        self.requests = s.requests;
        self.robots_blocked = s.robots_blocked;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedState {
    pub daily_size: u32,
    #[serde(default)]
    pub size_day: Option<String>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub paused_at: Option<i64>,
    /// Paused from the app rather than automatically.
    #[serde(default)]
    pub paused_by_user: bool,
    #[serde(default)]
    pub resumed_at: Option<i64>,
    #[serde(default)]
    pub last_open_at: Option<i64>,
    #[serde(default)]
    pub first_batch_at: Option<i64>,
    #[serde(default)]
    pub last_batch_at: Option<i64>,
    #[serde(default)]
    pub last_batch_day: Option<String>,
    #[serde(default)]
    pub last_batch_count: u32,
    #[serde(default)]
    pub interests: Option<Interests>,
    #[serde(default)]
    pub last_run: Option<RunReport>,
    #[serde(default)]
    pub urgent_day: Option<String>,
    #[serde(default)]
    pub urgent_count: u32,
    /// Local days on which the daily model calls were used.
    #[serde(default)]
    pub interests_call_day: Option<String>,
    #[serde(default)]
    pub summary_call_day: Option<String>,
    /// The last (lowest-ranked) post of the latest batch.
    #[serde(default)]
    pub last_batch_end_id: Option<String>,
}

impl Default for FeedState {
    fn default() -> Self {
        Self {
            daily_size: DEFAULT_DAILY_SIZE,
            size_day: None,
            paused: false,
            paused_at: None,
            paused_by_user: false,
            resumed_at: None,
            last_open_at: None,
            first_batch_at: None,
            last_batch_at: None,
            last_batch_day: None,
            last_batch_count: 0,
            interests: None,
            last_run: None,
            urgent_day: None,
            urgent_count: 0,
            interests_call_day: None,
            summary_call_day: None,
            last_batch_end_id: None,
        }
    }
}

fn state_key(user_id: &str) -> String {
    format!("feed.state:{user_id}")
}

pub fn get_state(conn: &Connection, user_id: &str) -> rusqlite::Result<FeedState> {
    Ok(crate::db::get_setting(conn, &state_key(user_id))?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default())
}

/// Read-modify-write the state in one call (the connection is exclusive
/// for the duration, so concurrent updates never overwrite each other).
pub fn update_state<R>(
    conn: &Connection,
    user_id: &str,
    f: impl FnOnce(&mut FeedState) -> R,
) -> rusqlite::Result<R> {
    let mut st = get_state(conn, user_id)?;
    let r = f(&mut st);
    let raw = serde_json::to_string(&st).unwrap_or_else(|_| "{}".into());
    crate::db::set_setting(conn, &state_key(user_id), &raw)?;
    Ok(r)
}

// ------------------------------------------------------------------ sources

#[derive(Debug, Clone, Serialize)]
pub struct Source {
    pub id: String,
    pub kind: String,
    pub value: String,
    pub label: Option<String>,
    pub topic: Option<String>,
    pub origin: String,
    pub enabled: bool,
    /// The feed's own title, once fetched.
    pub title: Option<String>,
    #[serde(skip)]
    pub state: Value,
    pub last_fetch_at: Option<i64>,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
    pub fail_count: i64,
    pub created_at: i64,
}

fn source_from_row(r: &rusqlite::Row) -> rusqlite::Result<Source> {
    let state: Value = serde_json::from_str(&r.get::<_, String>("state")?).unwrap_or(Value::Null);
    Ok(Source {
        title: state["title"].as_str().map(str::to_string),
        id: r.get("id")?,
        kind: r.get("kind")?,
        value: r.get("value")?,
        label: r.get("label")?,
        topic: r.get("topic")?,
        origin: r.get("origin")?,
        enabled: r.get("enabled")?,
        state,
        last_fetch_at: r.get("last_fetch_at")?,
        last_ok_at: r.get("last_ok_at")?,
        last_error: r.get("last_error")?,
        fail_count: r.get("fail_count")?,
        created_at: r.get("created_at")?,
    })
}

pub fn list_sources(conn: &Connection, user_id: &str) -> rusqlite::Result<Vec<Source>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM feed_sources WHERE user_id = ?1 AND removed = 0
         ORDER BY origin = 'user' DESC, created_at",
    )?;
    stmt.query_map([user_id], source_from_row)?.collect()
}

pub fn get_source(conn: &Connection, user_id: &str, id: &str) -> rusqlite::Result<Option<Source>> {
    conn.query_row(
        "SELECT * FROM feed_sources WHERE id = ?1 AND user_id = ?2 AND removed = 0",
        params![id, user_id],
        source_from_row,
    )
    .optional()
}

/// Insert a source unless the same (kind, value) exists. Returns the new
/// row, or None when it was already there.
#[allow(clippy::too_many_arguments)]
pub fn add_source(
    conn: &Connection,
    user_id: &str,
    kind: &str,
    value: &str,
    label: Option<&str>,
    topic: Option<&str>,
    origin: &str,
) -> rusqlite::Result<Option<Source>> {
    let id = new_id();
    let now = now_ms();
    let n = conn.execute(
        "INSERT OR IGNORE INTO feed_sources (id, user_id, kind, value, label, topic, origin,
                                             created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![id, user_id, kind, value, label, topic, origin, now],
    )?;
    if n == 0 {
        return Ok(None);
    }
    get_source(conn, user_id, &id)
}

pub fn set_source_enabled(
    conn: &Connection,
    user_id: &str,
    id: &str,
    enabled: bool,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE feed_sources SET enabled = ?3, fail_count = CASE WHEN ?3 THEN 0 ELSE fail_count END,
                updated_at = ?4 WHERE id = ?1 AND user_id = ?2",
        params![id, user_id, enabled, now_ms()],
    )? > 0)
}

/// Remove a source. The user's own are deleted; suggested and imported
/// ones are hidden instead, so the next suggestion or import does not add
/// them back.
pub fn delete_source(conn: &Connection, user_id: &str, id: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM feed_sources WHERE id = ?1 AND user_id = ?2 AND origin = 'user'",
        params![id, user_id],
    )?;
    if n > 0 {
        return Ok(true);
    }
    Ok(conn.execute(
        "UPDATE feed_sources SET removed = 1, enabled = 0, updated_at = ?3
         WHERE id = ?1 AND user_id = ?2 AND removed = 0",
        params![id, user_id, now_ms()],
    )? > 0)
}

/// Add a source the user typed: new, or an existing (even removed) one
/// made theirs and turned on.
pub fn add_user_source(
    conn: &Connection,
    user_id: &str,
    kind: &str,
    value: &str,
    topic: Option<&str>,
) -> rusqlite::Result<Option<Source>> {
    if add_source(conn, user_id, kind, value, None, topic, "user")?.is_none() {
        conn.execute(
            "UPDATE feed_sources SET origin = 'user', enabled = 1, removed = 0, fail_count = 0,
                    updated_at = ?4
             WHERE user_id = ?1 AND kind = ?2 AND value = ?3",
            params![user_id, kind, value, now_ms()],
        )?;
    }
    Ok(list_sources(conn, user_id)?
        .into_iter()
        .find(|s| s.kind == kind && s.value == value))
}

/// Record a fetch. Suggested and Google sources that keep failing are
/// turned off (the user can turn them back on); the user's own are kept.
pub fn record_fetch(
    conn: &Connection,
    id: &str,
    result: Result<Option<&Value>, &str>,
) -> rusqlite::Result<()> {
    let now = now_ms();
    match result {
        Ok(state) => {
            conn.execute(
                "UPDATE feed_sources SET last_fetch_at = ?2, last_ok_at = ?2, last_error = NULL,
                        fail_count = 0, state = COALESCE(?3, state), updated_at = ?2 WHERE id = ?1",
                params![id, now, state.map(|s| s.to_string())],
            )?;
        }
        Err(e) => {
            conn.execute(
                "UPDATE feed_sources SET last_fetch_at = ?2, last_error = ?3,
                        fail_count = fail_count + 1,
                        enabled = CASE WHEN origin != 'user' AND fail_count + 1 >= 3 THEN 0 ELSE enabled END,
                        updated_at = ?2 WHERE id = ?1",
                params![id, now, crate::util::truncate_chars(e, 200)],
            )?;
        }
    }
    Ok(())
}

/// Keep at most `max` suggested sources: drop disabled ones first, then
/// the oldest.
pub fn trim_suggested(conn: &Connection, user_id: &str, max: usize) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM feed_sources WHERE id IN (
             SELECT id FROM feed_sources WHERE user_id = ?1 AND origin = 'suggested' AND removed = 0
             ORDER BY enabled DESC, created_at DESC LIMIT -1 OFFSET ?2)",
        params![user_id, max as i64],
    )
}

// --------------------------------------------------------------------- seen

/// Links and titles already shown: recorded keys plus every post still in
/// the feed (including ones the agent wrote or that predate the pipeline).
pub fn seen_keys(conn: &Connection, user_id: &str) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT key FROM feed_seen WHERE user_id = ?1")?;
    let mut keys: HashSet<String> = stmt
        .query_map([user_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut stmt = conn.prepare("SELECT title, source_url FROM feed_posts WHERE user_id = ?1")?;
    let rows = stmt.query_map([user_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (title, url) = row?;
        keys.insert(super::rank::title_key(&title));
        if let Some(u) = url.filter(|u| !u.is_empty()) {
            keys.insert(super::rank::url_key(&u));
        }
    }
    Ok(keys)
}

pub fn add_seen(conn: &Connection, user_id: &str, keys: &[String]) -> rusqlite::Result<()> {
    let now = now_ms();
    let mut stmt = conn
        .prepare("INSERT OR REPLACE INTO feed_seen (user_id, key, seen_at) VALUES (?1, ?2, ?3)")?;
    for k in keys {
        stmt.execute(params![user_id, k, now])?;
    }
    Ok(())
}

/// Forget seen keys after 120 days, and unliked, undiscussed pipeline
/// posts after 90 days (their photos are pruned with them). Posts the
/// agent wrote (no `source_kind`) are never pruned.
pub fn prune(conn: &Connection, user_id: &str) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "DELETE FROM feed_seen WHERE user_id = ?1 AND seen_at < ?2",
        params![user_id, now - 120 * 86_400_000],
    )?;
    conn.execute(
        "DELETE FROM feed_posts WHERE user_id = ?1 AND liked = 0 AND source_kind IS NOT NULL
             AND discuss_conversation_id IS NULL AND created_at < ?2",
        params![user_id, now - 90 * 86_400_000],
    )?;
    conn.execute(
        "DELETE FROM feed_days WHERE user_id = ?1 AND day < ?2",
        params![user_id, crate::jobs::local_day(now - 60 * 86_400_000)],
    )?;
    prune_watches(conn, user_id)?;
    Ok(())
}

/// Cached image files any post still uses.
pub fn referenced_images(conn: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT cached FROM feed_posts WHERE cached != '{}'")?;
    let mut out = HashSet::new();
    for raw in stmt.query_map([], |r| r.get::<_, String>(0))? {
        if let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, String>>(&raw?) {
            out.extend(map.into_values());
        }
    }
    Ok(out)
}

// --------------------------------------------------------------------- days

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DayStat {
    pub day: String,
    pub published: i64,
    pub opened: i64,
    pub reached_end: i64,
}

fn bump_day(
    conn: &Connection,
    user_id: &str,
    day: &str,
    column: &str,
    by: i64,
) -> rusqlite::Result<()> {
    let sql = format!(
        "INSERT INTO feed_days (user_id, day, {column}) VALUES (?1, ?2, ?3)
         ON CONFLICT (user_id, day) DO UPDATE SET {column} = {column} + ?3"
    );
    conn.execute(&sql, params![user_id, day, by])?;
    Ok(())
}

pub fn record_published(
    conn: &Connection,
    user_id: &str,
    day: &str,
    n: i64,
) -> rusqlite::Result<()> {
    bump_day(conn, user_id, day, "published", n)
}

pub fn record_opened(conn: &Connection, user_id: &str, day: &str) -> rusqlite::Result<()> {
    bump_day(conn, user_id, day, "opened", 1)
}

pub fn record_reached_end(conn: &Connection, user_id: &str, day: &str) -> rusqlite::Result<()> {
    bump_day(conn, user_id, day, "reached_end", 1)
}

/// The most recent `n` days with any row, newest first, excluding `today`.
pub fn recent_days(
    conn: &Connection,
    user_id: &str,
    today: &str,
    n: usize,
) -> rusqlite::Result<Vec<DayStat>> {
    let mut stmt = conn.prepare(
        "SELECT day, published, opened, reached_end FROM feed_days
         WHERE user_id = ?1 AND day < ?2 ORDER BY day DESC LIMIT ?3",
    )?;
    stmt.query_map(params![user_id, today, n as i64], |r| {
        Ok(DayStat {
            day: r.get(0)?,
            published: r.get(1)?,
            opened: r.get(2)?,
            reached_end: r.get(3)?,
        })
    })?
    .collect()
}

// --------------------------------------------------------------- engagement

/// Likes and discussions (topics, sources, title words) over the last 60
/// days, and topics whose posts were shown at least two days ago and never
/// touched.
pub fn engagement(conn: &Connection, user_id: &str) -> rusqlite::Result<Engagement> {
    let now = now_ms();
    let mut e = Engagement::default();
    let mut stmt = conn.prepare(
        "SELECT title, topic, source_name, liked, discuss_conversation_id IS NOT NULL, created_at
         FROM feed_posts WHERE user_id = ?1 AND created_at > ?2",
    )?;
    let rows = stmt.query_map(params![user_id, now - 60 * 86_400_000], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, bool>(3)?,
            r.get::<_, bool>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    for row in rows {
        let (title, topic, source, liked, discussed, created) = row?;
        let w = if liked { 1.0 } else { 0.0 } + if discussed { 0.5 } else { 0.0 };
        let topic = topic.map(|t| t.to_lowercase());
        if w > 0.0 {
            if let Some(t) = &topic {
                *e.topic_likes.entry(t.clone()).or_default() += w;
            }
            if let Some(s) = source {
                *e.source_likes.entry(s).or_default() += w;
            }
            for word in words(&title) {
                *e.liked_words.entry(word).or_default() += w;
            }
        } else if now - created > 2 * 86_400_000
            && let Some(t) = topic
        {
            *e.topic_ignored.entry(t).or_default() += 1.0;
        }
    }
    Ok(e)
}

/// Titles and topics of recently liked posts (newest first).
pub fn liked_posts(
    conn: &Connection,
    user_id: &str,
    limit: usize,
) -> rusqlite::Result<Vec<(String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT title, topic FROM feed_posts WHERE user_id = ?1 AND liked = 1
         ORDER BY updated_at DESC LIMIT ?2",
    )?;
    stmt.query_map(params![user_id, limit as i64], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?
    .collect()
}

/// What the user said in their own chats since `since` (newest first).
/// Shared chats are left out: other people's words are not the user's
/// interests.
pub fn recent_user_messages(
    conn: &Connection,
    user_id: &str,
    since: i64,
    limit: usize,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT m.content FROM messages m JOIN conversations c ON c.id = m.conversation_id
         WHERE m.role = 'user' AND m.kind = 'text' AND m.author_user_id = ?1
           AND c.kind IN ('main', 'side') AND m.created_at > ?2 AND length(m.content) > 0
         ORDER BY m.created_at DESC LIMIT ?3",
    )?;
    stmt.query_map(params![user_id, since, limit as i64], |r| r.get(0))?
        .collect()
}

// ------------------------------------------------------------------ watches

#[derive(Debug, Clone)]
pub struct Watch {
    pub id: String,
    pub url: String,
    pub title: String,
    pub price: f64,
    pub currency: String,
}

/// Start watching a liked post's price (no-op when it has none).
pub fn watch_post(conn: &Connection, user_id: &str, post_id: &str) -> rusqlite::Result<bool> {
    let row: Option<(String, Option<String>, String)> = conn
        .query_row(
            "SELECT title, source_url, facts FROM feed_posts WHERE id = ?1 AND user_id = ?2",
            params![post_id, user_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((title, Some(url), facts)) = row else {
        return Ok(false);
    };
    let facts: Value = serde_json::from_str(&facts).unwrap_or(Value::Null);
    let Some(price) = facts["price"]["amount"].as_f64().filter(|p| *p > 0.0) else {
        return Ok(false);
    };
    let currency = facts["price"]["currency"]
        .as_str()
        .unwrap_or("")
        .to_string();
    conn.execute(
        "INSERT OR IGNORE INTO feed_watches (id, user_id, post_id, url, title, price, currency, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![new_id(), user_id, post_id, url, title, price, currency, now_ms()],
    )?;
    Ok(true)
}

/// Stop watching the product of an unliked post, unless another liked post
/// (say, its price-drop post) still links to it.
pub fn unwatch_post(conn: &Connection, user_id: &str, post_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM feed_watches WHERE user_id = ?1
            AND url = (SELECT source_url FROM feed_posts WHERE id = ?2 AND user_id = ?1)
            AND NOT EXISTS (SELECT 1 FROM feed_posts p WHERE p.user_id = ?1
                            AND p.source_url = feed_watches.url AND p.liked = 1)",
        params![user_id, post_id],
    )?;
    Ok(())
}

/// Drop watches no liked post links to any more (posts deleted).
pub fn prune_watches(conn: &Connection, user_id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM feed_watches WHERE user_id = ?1 AND NOT EXISTS (
             SELECT 1 FROM feed_posts p WHERE p.user_id = ?1
             AND p.source_url = feed_watches.url AND p.liked = 1)",
        [user_id],
    )
}

/// Watches not checked in the last 20 hours, oldest check first.
pub fn due_watches(conn: &Connection, user_id: &str, limit: usize) -> rusqlite::Result<Vec<Watch>> {
    let mut stmt = conn.prepare(
        "SELECT id, url, title, price, currency FROM feed_watches w
         WHERE user_id = ?1 AND (checked_at IS NULL OR checked_at < ?2)
           AND EXISTS (SELECT 1 FROM feed_posts p WHERE p.user_id = ?1
                       AND p.source_url = w.url AND p.liked = 1)
         ORDER BY COALESCE(checked_at, 0) LIMIT ?3",
    )?;
    stmt.query_map(
        params![user_id, now_ms() - 20 * 3_600_000, limit as i64],
        |r| {
            Ok(Watch {
                id: r.get(0)?,
                url: r.get(1)?,
                title: r.get(2)?,
                price: r.get(3)?,
                currency: r.get(4)?,
            })
        },
    )?
    .collect()
}

/// The watch still exists and a liked post still links to its product.
pub fn watch_active(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM feed_watches w WHERE w.id = ?1 AND EXISTS (
             SELECT 1 FROM feed_posts p WHERE p.user_id = w.user_id
             AND p.source_url = w.url AND p.liked = 1))",
        [id],
        |r| r.get(0),
    )
}

/// Record a check; `price` replaces the reference price when given.
pub fn checked_watch(conn: &Connection, id: &str, price: Option<f64>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE feed_watches SET checked_at = ?2, price = COALESCE(?3, price) WHERE id = ?1",
        params![id, now_ms(), price],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn state_sources_days_and_watches() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let u = crate::db::owner_user_id(c)?.unwrap();
            let u = u.as_str();
            assert_eq!(get_state(c, u)?.daily_size, DEFAULT_DAILY_SIZE);
            update_state(c, u, |s| s.paused = true)?;
            assert!(get_state(c, u)?.paused);

            let s = add_source(
                c,
                u,
                "rss",
                "https://a.example/feed",
                None,
                Some("Space"),
                "suggested",
            )?
            .unwrap();
            assert!(
                add_source(c, u, "rss", "https://a.example/feed", None, None, "user")?.is_none()
            );
            for _ in 0..3 {
                record_fetch(c, &s.id, Err("HTTP 404"))?;
            }
            let s2 = get_source(c, u, &s.id)?.unwrap();
            assert!(!s2.enabled, "failing suggested sources turn off");
            assert_eq!(s2.fail_count, 3);
            let mine = add_source(c, u, "reddit", "space", None, None, "user")?.unwrap();
            for _ in 0..5 {
                record_fetch(c, &mine.id, Err("HTTP 500"))?;
            }
            assert!(
                get_source(c, u, &mine.id)?.unwrap().enabled,
                "the user's own stay on"
            );
            record_fetch(c, &mine.id, Ok(Some(&serde_json::json!({"title": "x"}))))?;
            let ok = get_source(c, u, &mine.id)?.unwrap();
            assert_eq!((ok.fail_count, ok.title.as_deref()), (0, Some("x")));
            for i in 0..5 {
                add_source(c, u, "reddit", &format!("sub{i}"), None, None, "suggested")?;
            }
            trim_suggested(c, u, 2)?;
            let n = list_sources(c, u)?
                .iter()
                .filter(|s| s.origin == "suggested")
                .count();
            assert_eq!(n, 2);

            // Removing an imported source sticks; adding it by hand revives it.
            let ch = "UCabcdefghijklmnopqrstuv";
            let g = add_source(c, u, "youtube", ch, None, None, "google")?.unwrap();
            assert!(delete_source(c, u, &g.id)?);
            assert!(add_source(c, u, "youtube", ch, None, None, "google")?.is_none());
            assert!(!list_sources(c, u)?.iter().any(|s| s.id == g.id));
            let back = add_user_source(c, u, "youtube", ch, None)?.unwrap();
            assert_eq!(back.id, g.id);
            assert_eq!((back.origin.as_str(), back.enabled), ("user", true));
            assert!(delete_source(c, u, &g.id)?);
            assert!(
                get_source(c, u, &g.id)?.is_none(),
                "the user's own are deleted"
            );

            add_seen(c, u, &["u:a".into(), "t:b".into()])?;
            assert!(seen_keys(c, u)?.contains("t:b"));

            record_published(c, u, "2026-10-01", 10)?;
            record_opened(c, u, "2026-10-01")?;
            record_opened(c, u, "2026-10-01")?;
            record_reached_end(c, u, "2026-10-02")?;
            let days = recent_days(c, u, "2026-10-03", 7)?;
            assert_eq!(days.len(), 2);
            assert_eq!(
                days[1],
                DayStat {
                    day: "2026-10-01".into(),
                    published: 10,
                    opened: 2,
                    reached_end: 0
                }
            );

            let post = crate::entities::insert_pipeline_post(
                c,
                u,
                &crate::entities::PipelinePost {
                    title: "Trail shoe".into(),
                    body: "b".into(),
                    source_url: "https://shop.example/p".into(),
                    facts: serde_json::json!({"price": {"amount": 120.0, "currency": "USD"}}),
                    topic: Some("Running".into()),
                    source_name: "Shop".into(),
                    ..Default::default()
                },
            )?;
            assert!(watch_post(c, u, &post.id)?);
            assert!(due_watches(c, u, 10)?.is_empty(), "only while liked");
            crate::entities::set_feed_liked(c, u, &post.id, true)?;
            let due = due_watches(c, u, 10)?;
            assert_eq!(due.len(), 1);
            checked_watch(c, &due[0].id, Some(99.0))?;
            assert!(due_watches(c, u, 10)?.is_empty());
            // A second saved post for the same product (its price-drop
            // post) keeps the watch when the first is unliked.
            let drop = crate::entities::insert_pipeline_post(
                c,
                u,
                &crate::entities::PipelinePost {
                    title: "Price drop: Trail shoe".into(),
                    body: "b".into(),
                    source_url: "https://shop.example/p".into(),
                    source_kind: "price_drop".into(),
                    ..Default::default()
                },
            )?;
            crate::entities::set_feed_liked(c, u, &drop.id, true)?;
            crate::entities::set_feed_liked(c, u, &post.id, false)?;
            unwatch_post(c, u, &post.id)?;
            assert_eq!(prune_watches(c, u)?, 0);
            crate::entities::set_feed_liked(c, u, &drop.id, false)?;
            assert_eq!(prune_watches(c, u)?, 1, "no saved post left");
            crate::entities::set_feed_liked(c, u, &post.id, true)?;
            let e = engagement(c, u)?;
            assert_eq!(e.topic_likes.get("running"), Some(&1.0));
            assert_eq!(e.source_likes.get("Shop"), Some(&1.0));
            assert!(e.liked_words.contains_key("trail"));
            unwatch_post(c, u, &post.id)?;
            Ok(())
        })
        .unwrap();
    }
}
