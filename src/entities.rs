//! Product entities: feed posts, ideas, goals, library items, cards,
//! connectors, credentials (metadata), and push subscriptions. Plain SQL
//! shared by the HTTP API and the `iris mcp` bridge.

use crate::db::{Message, NewMessage, insert_message};
use crate::util::{new_id, now_ms};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const MAX_TITLE: usize = 300;
pub const MAX_BODY: usize = 50_000;

/// Validation error message for user/agent supplied values.
pub fn check_text(field: &str, value: &str, max: usize, required: bool) -> Result<(), String> {
    if required && value.trim().is_empty() {
        return Err(format!("{field} is required"));
    }
    if value.chars().count() > max {
        return Err(format!("{field} is longer than {max} characters"));
    }
    Ok(())
}

pub fn check_url(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.starts_with("https://") || value.starts_with("http://") {
        check_text(field, value, 2000, false)
    } else {
        Err(format!("{field} must be an http(s) URL"))
    }
}

// --------------------------------------------------------------------- feed

#[derive(Debug, Clone, Serialize)]
pub struct FeedPost {
    pub id: String,
    pub title: String,
    pub body: String,
    pub topic: Option<String>,
    pub source_url: Option<String>,
    /// Display name of the source (site or creator), e.g. "The Verge".
    pub source_name: Option<String>,
    /// Cover photo (the first of `images`).
    pub image_url: Option<String>,
    /// The source page's original photos, in order.
    pub images: Vec<String>,
    /// What the app shows for `images`, in order: the local copy
    /// (`/api/feed/images/<file>`) when Iris cached it; otherwise the
    /// original, except for pipeline posts, which show cached copies only.
    pub photos: Vec<String>,
    /// One line on why the user would like this post.
    pub why: Option<String>,
    /// A short line for the story viewer (pipeline posts with a summary).
    pub hook: Option<String>,
    /// The source's own icon (original URL).
    pub favicon_url: Option<String>,
    /// What the app shows for the icon: the local copy when cached.
    pub favicon: Option<String>,
    /// rss | youtube | reddit | hn | bluesky | mastodon | price_drop, or
    /// null for posts the agent wrote.
    pub source_kind: Option<String>,
    /// Facts read from the page: `{"price": {"amount", "currency"}}`,
    /// `{"event": {"start_ms", "location"}}`, or `{}`.
    pub facts: Value,
    pub liked: bool,
    pub discuss_conversation_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Local URL of a cached feed image.
pub fn feed_image_path(file: &str) -> String {
    format!("/api/feed/images/{file}")
}

fn feed_from_row(r: &rusqlite::Row) -> rusqlite::Result<FeedPost> {
    let images: Vec<String> =
        serde_json::from_str(&r.get::<_, String>("images")?).unwrap_or_default();
    let cached: BTreeMap<String, String> =
        serde_json::from_str(&r.get::<_, String>("cached")?).unwrap_or_default();
    let source_kind: Option<String> = r.get("source_kind")?;
    // Pipeline posts only ever show copies Iris fetched and checked; posts
    // the agent wrote show their original https URLs.
    let pipeline = source_kind.is_some();
    let shown = |u: &String| match cached.get(u) {
        Some(f) => Some(feed_image_path(f)),
        None => (!pipeline).then(|| u.clone()),
    };
    let favicon_url: Option<String> = r.get("favicon_url")?;
    Ok(FeedPost {
        id: r.get("id")?,
        title: r.get("title")?,
        body: r.get("body")?,
        topic: r.get("topic")?,
        source_url: r.get("source_url")?,
        source_name: r.get("source_name")?,
        image_url: r.get("image_url")?,
        photos: images.iter().filter_map(shown).collect(),
        images,
        why: r.get("why")?,
        hook: r.get("hook")?,
        favicon: favicon_url.as_ref().and_then(shown),
        favicon_url,
        source_kind,
        facts: serde_json::from_str(&r.get::<_, String>("facts")?).unwrap_or_else(|_| json!({})),
        liked: r.get("liked")?,
        discuss_conversation_id: r.get("discuss_conversation_id")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

/// A post built by the For you pipeline (`src/feed`).
#[derive(Debug, Clone, Default)]
pub struct PipelinePost {
    pub title: String,
    pub body: String,
    pub hook: Option<String>,
    pub why: Option<String>,
    pub topic: Option<String>,
    pub source_url: String,
    pub source_name: String,
    pub source_kind: String,
    pub url_key: String,
    /// Every key identifying the story (link, title, redirect and
    /// canonical URLs), for the pipeline's duplicate checks; not stored.
    pub seen_keys: Vec<String>,
    pub images: Vec<String>,
    pub favicon_url: Option<String>,
    /// Original URL -> cached file name.
    pub cached: BTreeMap<String, String>,
    pub facts: Value,
}

/// Insert a pipeline post with a unique timestamp after every existing post
/// (so `before=<created_at>` paging never skips one).
pub fn insert_pipeline_post(
    conn: &Connection,
    user_id: &str,
    p: &PipelinePost,
) -> rusqlite::Result<FeedPost> {
    crate::db::write_tx(conn, |c| insert_pipeline_post_locked(c, user_id, p))
}

fn insert_pipeline_post_locked(
    conn: &Connection,
    user_id: &str,
    p: &PipelinePost,
) -> rusqlite::Result<FeedPost> {
    let id = new_id();
    let created_at = crate::db::next_timestamp(conn, "feed_posts")?;
    let cut = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let opt = |v: &Option<String>, n: usize| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| cut(s, n))
    };
    let images: Vec<String> = p.images.iter().take(MAX_FEED_IMAGES).cloned().collect();
    let facts = if p.facts.is_object() {
        p.facts.clone()
    } else {
        json!({})
    };
    conn.execute(
        "INSERT INTO feed_posts (id, user_id, title, body, topic, source_url, image_url, images,
                                 source_name, why, hook, favicon_url, cached, source_kind, url_key,
                                 facts, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?17)",
        params![
            id,
            user_id,
            cut(p.title.trim(), MAX_TITLE),
            cut(p.body.trim(), MAX_BODY),
            opt(&p.topic, 100),
            p.source_url,
            images.first(),
            serde_json::to_string(&images).unwrap_or_else(|_| "[]".into()),
            cut(p.source_name.trim(), 100),
            opt(&p.why, 300),
            opt(&p.hook, 200),
            p.favicon_url,
            serde_json::to_string(&p.cached).unwrap_or_else(|_| "{}".into()),
            p.source_kind,
            p.url_key,
            facts.to_string(),
            created_at
        ],
    )?;
    get_feed_post(conn, user_id, &id).map(|p| p.expect("inserted"))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewFeedPost {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub images: Vec<String>,
    #[serde(default)]
    pub source_name: Option<String>,
    #[serde(default)]
    pub why: Option<String>,
}

pub const MAX_FEED_IMAGES: usize = 10;

impl NewFeedPost {
    pub fn validate(&self) -> Result<(), String> {
        check_text("title", &self.title, MAX_TITLE, true)?;
        check_text("body", &self.body, MAX_BODY, true)?;
        check_text("topic", self.topic.as_deref().unwrap_or(""), 100, false)?;
        check_text(
            "source_name",
            self.source_name.as_deref().unwrap_or(""),
            100,
            false,
        )?;
        check_text("why", self.why.as_deref().unwrap_or(""), 300, false)?;
        check_url("source_url", self.source_url.as_deref().unwrap_or(""))?;
        check_url("image_url", self.image_url.as_deref().unwrap_or(""))?;
        if self.images.len() > MAX_FEED_IMAGES {
            return Err(format!("at most {MAX_FEED_IMAGES} images"));
        }
        for url in &self.images {
            if url.is_empty() {
                return Err("images must be http(s) URLs".into());
            }
            check_url("images", url)?;
        }
        Ok(())
    }

    /// All photos in order: `image_url` (if not already listed) first.
    pub fn all_images(&self) -> Vec<String> {
        let mut out = Vec::new();
        let cover = self.image_url.as_deref().filter(|u| !u.is_empty());
        if let Some(u) = cover {
            out.push(u.to_string());
        }
        for u in &self.images {
            if !out.contains(u) {
                out.push(u.clone());
            }
        }
        out
    }
}

fn non_blank(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

pub fn create_feed_post(
    conn: &Connection,
    user_id: &str,
    p: &NewFeedPost,
) -> rusqlite::Result<FeedPost> {
    crate::db::write_tx(conn, |conn| create_feed_post_locked(conn, user_id, p))
}

fn create_feed_post_locked(
    conn: &Connection,
    user_id: &str,
    p: &NewFeedPost,
) -> rusqlite::Result<FeedPost> {
    let id = new_id();
    // Unique, so `before=<created_at>` paging never skips a post.
    let now = crate::db::next_timestamp(conn, "feed_posts")?;
    let images = p.all_images();
    conn.execute(
        "INSERT INTO feed_posts (id, user_id, title, body, topic, source_url, image_url, images,
                                 source_name, why, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
        params![
            id,
            user_id,
            p.title.trim(),
            p.body.trim(),
            non_blank(&p.topic),
            non_blank(&p.source_url),
            images.first(),
            serde_json::to_string(&images).unwrap_or_else(|_| "[]".into()),
            non_blank(&p.source_name),
            non_blank(&p.why),
            now
        ],
    )?;
    get_feed_post(conn, user_id, &id).map(|p| p.expect("inserted"))
}

pub fn get_feed_post(
    conn: &Connection,
    user_id: &str,
    id: &str,
) -> rusqlite::Result<Option<FeedPost>> {
    conn.query_row(
        "SELECT * FROM feed_posts WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
        feed_from_row,
    )
    .optional()
}

pub fn list_feed(
    conn: &Connection,
    user_id: &str,
    before: Option<i64>,
    limit: usize,
) -> rusqlite::Result<Vec<FeedPost>> {
    list_feed_filtered(conn, user_id, before, limit, false)
}

/// Newest first; `liked_only` keeps only saved (liked) posts.
pub fn list_feed_filtered(
    conn: &Connection,
    user_id: &str,
    before: Option<i64>,
    limit: usize,
    liked_only: bool,
) -> rusqlite::Result<Vec<FeedPost>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM feed_posts WHERE user_id = ?1 AND created_at < ?2 AND (?4 = 0 OR liked = 1)
         ORDER BY created_at DESC LIMIT ?3",
    )?;
    stmt.query_map(
        params![
            user_id,
            before.unwrap_or(i64::MAX),
            limit as i64,
            liked_only
        ],
        feed_from_row,
    )?
    .collect()
}

pub fn set_feed_liked(
    conn: &Connection,
    user_id: &str,
    id: &str,
    liked: bool,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE feed_posts SET liked = ?3, updated_at = ?4 WHERE id = ?1 AND user_id = ?2",
        params![id, user_id, liked, now_ms()],
    )? > 0)
}

pub fn set_feed_discussion(
    conn: &Connection,
    id: &str,
    conversation_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE feed_posts SET discuss_conversation_id = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, conversation_id, now_ms()],
    )?;
    Ok(())
}

pub fn delete_feed_post(conn: &Connection, user_id: &str, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM feed_posts WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
    )? > 0)
}

// -------------------------------------------------------------------- ideas

#[derive(Debug, Clone, Serialize)]
pub struct Idea {
    pub id: String,
    pub title: String,
    pub description: String,
    /// What to send to the agent when the user taps the idea.
    pub prompt: Option<String>,
    /// open | started | done | dismissed
    pub status: String,
    pub conversation_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn idea_from_row(r: &rusqlite::Row) -> rusqlite::Result<Idea> {
    Ok(Idea {
        id: r.get("id")?,
        title: r.get("title")?,
        description: r.get("description")?,
        prompt: r.get("prompt")?,
        status: r.get("status")?,
        conversation_id: r.get("conversation_id")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

pub const IDEA_STATUSES: &[&str] = &["open", "started", "done", "dismissed"];

pub fn create_idea(
    conn: &Connection,
    user_id: &str,
    title: &str,
    description: &str,
    prompt: Option<&str>,
) -> rusqlite::Result<Idea> {
    let id = new_id();
    let now = now_ms();
    conn.execute(
        "INSERT INTO ideas (id, user_id, title, description, prompt, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![id, user_id, title.trim(), description.trim(), prompt, now],
    )?;
    get_idea(conn, user_id, &id).map(|i| i.expect("inserted"))
}

pub fn get_idea(conn: &Connection, user_id: &str, id: &str) -> rusqlite::Result<Option<Idea>> {
    conn.query_row(
        "SELECT * FROM ideas WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
        idea_from_row,
    )
    .optional()
}

pub fn list_ideas(
    conn: &Connection,
    user_id: &str,
    status: Option<&str>,
) -> rusqlite::Result<Vec<Idea>> {
    list_ideas_limited(conn, user_id, status, 200)
}

/// `status` may list several, comma-separated (`open,started`), so done
/// and dismissed ideas never crowd out the ones still to act on.
pub fn list_ideas_limited(
    conn: &Connection,
    user_id: &str,
    status: Option<&str>,
    limit: usize,
) -> rusqlite::Result<Vec<Idea>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM ideas WHERE user_id = ?1
           AND (?2 IS NULL OR instr(',' || ?2 || ',', ',' || status || ',') > 0)
         ORDER BY created_at DESC LIMIT ?3",
    )?;
    stmt.query_map(params![user_id, status, limit as i64], idea_from_row)?
        .collect()
}

pub fn update_idea(
    conn: &Connection,
    user_id: &str,
    id: &str,
    status: Option<&str>,
    conversation_id: Option<&str>,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE ideas SET status = COALESCE(?3, status),
                          conversation_id = COALESCE(?4, conversation_id),
                          updated_at = ?5
         WHERE id = ?1 AND user_id = ?2",
        params![id, user_id, status, conversation_id, now_ms()],
    )?;
    Ok(n > 0)
}

pub fn delete_idea(conn: &Connection, user_id: &str, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM ideas WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
    )? > 0)
}

// -------------------------------------------------------------------- goals

#[derive(Debug, Clone, Serialize)]
pub struct Goal {
    pub id: String,
    pub title: String,
    /// Free-text progress note the agent keeps current.
    pub status_text: String,
    /// Suggested by the agent and not yet confirmed by the user.
    pub suggested: bool,
    pub done: bool,
    pub done_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn goal_from_row(r: &rusqlite::Row) -> rusqlite::Result<Goal> {
    Ok(Goal {
        id: r.get("id")?,
        title: r.get("title")?,
        status_text: r.get("status_text")?,
        suggested: r.get("suggested")?,
        done: r.get("done")?,
        done_at: r.get("done_at")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

pub fn create_goal(
    conn: &Connection,
    user_id: &str,
    title: &str,
    status_text: &str,
    suggested: bool,
) -> rusqlite::Result<Goal> {
    let id = new_id();
    let now = now_ms();
    conn.execute(
        "INSERT INTO goals (id, user_id, title, status_text, suggested, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![
            id,
            user_id,
            title.trim(),
            status_text.trim(),
            suggested,
            now
        ],
    )?;
    get_goal(conn, user_id, &id).map(|g| g.expect("inserted"))
}

pub fn get_goal(conn: &Connection, user_id: &str, id: &str) -> rusqlite::Result<Option<Goal>> {
    conn.query_row(
        "SELECT * FROM goals WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
        goal_from_row,
    )
    .optional()
}

pub fn list_goals(
    conn: &Connection,
    user_id: &str,
    include_done: bool,
) -> rusqlite::Result<Vec<Goal>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM goals WHERE user_id = ?1 AND (?2 OR done = 0)
         ORDER BY done, suggested DESC, created_at DESC LIMIT 500",
    )?;
    stmt.query_map(params![user_id, include_done], goal_from_row)?
        .collect()
}

/// Fields to change on a goal; `None` keeps the current value.
#[derive(Debug, Default)]
pub struct GoalChanges<'a> {
    pub title: Option<&'a str>,
    pub status_text: Option<&'a str>,
    pub done: Option<bool>,
    pub suggested: Option<bool>,
}

pub fn update_goal(
    conn: &Connection,
    user_id: &str,
    id: &str,
    ch: &GoalChanges,
) -> rusqlite::Result<Option<Goal>> {
    let now = now_ms();
    let n = conn.execute(
        "UPDATE goals SET title = COALESCE(?3, title),
                          status_text = COALESCE(?4, status_text),
                          done = COALESCE(?5, done),
                          done_at = CASE WHEN ?5 IS NULL THEN done_at WHEN ?5 THEN ?6 ELSE NULL END,
                          suggested = COALESCE(?7, suggested),
                          updated_at = ?6
         WHERE id = ?1 AND user_id = ?2",
        params![
            id,
            user_id,
            ch.title.map(str::trim),
            ch.status_text.map(str::trim),
            ch.done,
            now,
            ch.suggested
        ],
    )?;
    if n == 0 {
        return Ok(None);
    }
    get_goal(conn, user_id, id)
}

pub fn delete_goal(conn: &Connection, user_id: &str, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM goals WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
    )? > 0)
}

// ------------------------------------------------------------------ library

#[derive(Debug, Clone, Serialize)]
pub struct LibraryItem {
    pub id: String,
    /// artifact | media
    pub kind: String,
    pub title: String,
    pub description: String,
    pub mime: String,
    #[serde(skip)]
    pub file_name: String,
    pub size: i64,
    pub conversation_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Library columns. Items saved during a background job keep no chat link:
/// job conversations are hidden, so there is nothing to open.
const LIBRARY_SELECT: &str = "SELECT l.id, l.kind, l.title, l.description, l.mime, l.file_name,
        l.size, CASE WHEN c.kind = 'job' THEN NULL ELSE l.conversation_id END AS conversation_id,
        l.created_at, l.updated_at
    FROM library_items l LEFT JOIN conversations c ON c.id = l.conversation_id";

fn library_from_row(r: &rusqlite::Row) -> rusqlite::Result<LibraryItem> {
    Ok(LibraryItem {
        id: r.get("id")?,
        kind: r.get("kind")?,
        title: r.get("title")?,
        description: r.get("description")?,
        mime: r.get("mime")?,
        file_name: r.get("file_name")?,
        size: r.get("size")?,
        conversation_id: r.get("conversation_id")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

pub struct NewLibraryItem<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub mime: &'a str,
    pub file_name: &'a str,
    pub size: i64,
    pub conversation_id: Option<&'a str>,
}

pub fn insert_library_item(
    conn: &Connection,
    user_id: &str,
    it: &NewLibraryItem,
) -> rusqlite::Result<LibraryItem> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO library_items (id, user_id, kind, title, description, mime, file_name, size,
                                    conversation_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            it.id,
            user_id,
            it.kind,
            it.title.trim(),
            it.description.trim(),
            it.mime,
            it.file_name,
            it.size,
            it.conversation_id,
            now
        ],
    )?;
    get_library_item(conn, user_id, it.id).map(|i| i.expect("inserted"))
}

pub fn get_library_item(
    conn: &Connection,
    user_id: &str,
    id: &str,
) -> rusqlite::Result<Option<LibraryItem>> {
    conn.query_row(
        &format!("{LIBRARY_SELECT} WHERE l.id = ?1 AND l.user_id = ?2"),
        params![id, user_id],
        library_from_row,
    )
    .optional()
}

pub fn list_library(
    conn: &Connection,
    user_id: &str,
    kind: Option<&str>,
) -> rusqlite::Result<Vec<LibraryItem>> {
    list_library_page(conn, user_id, kind, None, 500)
}

/// Newest first by (created_at, id); `before` is the previous page's last
/// item as `(created_at, id)`.
pub fn list_library_page(
    conn: &Connection,
    user_id: &str,
    kind: Option<&str>,
    before: Option<(i64, &str)>,
    limit: usize,
) -> rusqlite::Result<Vec<LibraryItem>> {
    let (before_at, before_id) = before.unwrap_or((i64::MAX, ""));
    let mut stmt = conn.prepare(&format!(
        "{LIBRARY_SELECT} WHERE l.user_id = ?1 AND (?2 IS NULL OR l.kind = ?2)
           AND (l.created_at < ?3 OR (l.created_at = ?3 AND (?4 = '' OR l.id < ?4)))
         ORDER BY l.created_at DESC, l.id DESC LIMIT ?5"
    ))?;
    stmt.query_map(
        params![user_id, kind, before_at, before_id, limit as i64],
        library_from_row,
    )?
    .collect()
}

/// Returns the deleted item's file name so the caller can remove the file.
pub fn delete_library_item(
    conn: &Connection,
    user_id: &str,
    id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "DELETE FROM library_items WHERE id = ?1 AND user_id = ?2 RETURNING file_name",
        params![id, user_id],
        |r| r.get(0),
    )
    .optional()
}

// -------------------------------------------------------------------- cards

pub const CARD_TYPES: &[&str] = &[
    "link",
    "connect",
    "info",
    "feed_post",
    "idea",
    "goal",
    "result",
];

/// Validate an agent-supplied card. Cards are rendered by the client; the
/// schema is intentionally small: `type`, `title`, optional `body`, `url`,
/// `image_url`, `action_label`, `service`, `ref_id`.
pub fn validate_card(card: &Value) -> Result<Value, String> {
    let obj = card.as_object().ok_or("card must be an object")?;
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or("card.type is required")?;
    if !CARD_TYPES.contains(&ty) {
        return Err(format!(
            "card.type must be one of {}",
            CARD_TYPES.join(", ")
        ));
    }
    let s = |k: &str| obj.get(k).and_then(Value::as_str).unwrap_or("");
    check_text("card.title", s("title"), MAX_TITLE, true)?;
    check_text("card.body", s("body"), 4000, false)?;
    check_text("card.action_label", s("action_label"), 60, false)?;
    check_text("card.service", s("service"), 60, false)?;
    check_url("card.url", s("url"))?;
    check_url("card.image_url", s("image_url"))?;
    // A connect card for a service in Iris's catalog opens Iris's own
    // setup; any other needs the url of the service's page.
    let catalog_service =
        ty == "connect" && crate::connectors::catalog_entry(&s("service").to_lowercase()).is_some();
    if matches!(ty, "link" | "connect") && s("url").is_empty() && !catalog_service {
        return Err(format!("a {ty} card needs a url"));
    }
    let mut out = json!({"type": ty, "title": s("title").trim()});
    if catalog_service {
        out["service"] = s("service").to_lowercase().into();
    }
    for k in [
        "body",
        "url",
        "image_url",
        "action_label",
        "service",
        "ref_id",
    ] {
        if !s(k).is_empty() && !(k == "service" && catalog_service) {
            out[k] = s(k).into();
        }
    }
    Ok(out)
}

/// Insert a card message (role assistant) into a conversation.
pub fn insert_card(
    conn: &Connection,
    conversation_id: &str,
    card: &Value,
    role: &str,
) -> rusqlite::Result<Message> {
    insert_card_with_status(conn, conversation_id, card, role, "complete")
}

/// `pending` cards (made during a turn with the owner's private context)
/// are hidden until the turn is saved, which publishes or deletes them.
pub fn insert_card_with_status(
    conn: &Connection,
    conversation_id: &str,
    card: &Value,
    role: &str,
    status: &str,
) -> rusqlite::Result<Message> {
    let fallback = card["title"].as_str().unwrap_or("").to_string();
    insert_message(
        conn,
        &NewMessage {
            conversation_id,
            role,
            kind: "card",
            content: &fallback,
            card: Some(card),
            status,
            ..Default::default()
        },
    )
}

// ---------------------------------------------------------------- approvals

/// What an approval card shows: built by Iris from the real action, never
/// from agent-written text, so a confused agent cannot disguise it.
#[derive(Debug, Clone, Default)]
pub struct ApprovalRequest<'a> {
    /// e.g. "Send email from your Gmail".
    pub title: &'a str,
    /// Service the action goes through, e.g. "gmail".
    pub service: &'a str,
    /// Recipient(s) or target, e.g. "sam@example.com".
    pub to: &'a str,
    pub subject: &'a str,
    /// The exact text that would be sent (or a purchase summary).
    pub preview: &'a str,
    /// Approve button label, e.g. "Send" or "Buy".
    pub action_label: &'a str,
    /// User who may decide (the conversation's creator).
    pub approver_user_id: &'a str,
}

/// Server card asking the user to approve one action.
pub fn approval_card(r: &ApprovalRequest) -> Value {
    let cut = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    json!({
        "type": "approval",
        "title": cut(r.title, MAX_TITLE),
        "service": cut(r.service, 60),
        "preview": {
            "to": cut(r.to, 300),
            "subject": cut(r.subject, 300),
            "body": cut(r.preview, 4000),
        },
        "action_label": if r.action_label.is_empty() { "Approve".to_string() } else { cut(r.action_label, 60) },
        "approver_user_id": r.approver_user_id,
        "owner_only": false,
        "state": "pending",
        "decided_at": null,
    })
}

// --------------------------------------------------------------- connectors

/// OAuth state of a connector (MCP authorization spec, or a catalog
/// provider such as Google with the owner's own client). Secret fields
/// (`client_secret`, `access_token`, `refresh_token`) are sealed with the
/// vault (base64) and only opened in memory by the gateway.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// `discover`: endpoints come from the MCP server's metadata (and the
    /// client may be registered dynamically). `preset`: endpoints come
    /// from the catalog and the owner supplies the client.
    #[serde(default)]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// RFC 8707 resource indicator (the MCP server's URL), discover mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// Extra authorization parameters (e.g. `access_type=offline`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_params: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// `dcr` (registered by Iris) or `owner` (pasted in the app).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_source: String,
    /// Redirect URI the client was registered (or used) with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// stdio connectors: environment variable that receives the access
    /// token (http connectors get `Authorization: Bearer`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// When the owner last finished signing in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected_at: Option<i64>,
    /// The service is on this machine or the owner's network (sign-in may
    /// then use private addresses).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
    /// The account signed in (email from the OpenID ID token), shown to
    /// the owner and told to the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// Stored connector configuration. Values in `env` and `headers` are sealed
/// with the vault (base64) and only opened by the gateway.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectorConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Connector {
    pub id: String,
    pub name: String,
    pub description: String,
    /// stdio | http
    pub transport: String,
    #[serde(skip)]
    pub config: ConnectorConfig,
    pub enabled: bool,
    /// Approval rules (see `gateway::policy::Policy`).
    #[serde(skip)]
    pub policy: crate::gateway::policy::Policy,
    /// Catalog entry it was created from (`google`, `github`, `custom`).
    pub service: String,
    /// Set when a tripwire paused it.
    pub paused_reason: Option<String>,
    pub paused_at: Option<i64>,
    /// ok | needs_sign_in | needs_reconnect
    pub auth_state: String,
    /// Saved before the gateway: its program runs in the workspace.
    #[serde(skip)]
    pub workspace_cwd: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Connector {
    /// API view: command/url and secret *names* only.
    pub fn public_json(&self) -> Value {
        let oauth = self.config.oauth.as_ref().map(|o| {
            json!({
                "mode": o.mode,
                "client_configured": o.client_id.is_some(),
                "client_source": o.client_source,
                "connected": o.refresh_token.is_some() || o.access_token.is_some(),
                "connected_at": o.connected_at,
                "account": o.account,
                "scopes": o.scopes,
                "redirect_uri": o.redirect_uri,
            })
        });
        json!({
            "id": self.id,
            "name": self.name,
            "description": self.description,
            "transport": self.transport,
            "service": self.service,
            "enabled": self.enabled,
            "command": self.config.command,
            "args": self.config.args,
            "url": self.config.url,
            "env_keys": self.config.env.keys().collect::<Vec<_>>(),
            "header_keys": self.config.headers.keys().collect::<Vec<_>>(),
            "auth": if self.config.oauth.is_some() { "oauth" } else { "secrets" },
            "oauth": oauth,
            "auth_state": self.auth_state,
            "policy": self.policy,
            "rules": self.policy.mode.describe(),
            "paused": self.paused_reason.is_some(),
            "paused_reason": self.paused_reason,
            "paused_at": self.paused_at,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
    }

    /// Usable by turns right now (on, not paused, signed in).
    pub fn active(&self) -> bool {
        self.enabled && self.paused_reason.is_none() && self.auth_state == "ok"
    }
}

fn connector_from_row(r: &rusqlite::Row) -> rusqlite::Result<Connector> {
    let config: String = r.get("config")?;
    let policy: String = r.get("policy")?;
    Ok(Connector {
        id: r.get("id")?,
        name: r.get("name")?,
        description: r.get("description")?,
        transport: r.get("transport")?,
        config: serde_json::from_str(&config).unwrap_or_default(),
        enabled: r.get("enabled")?,
        policy: crate::gateway::policy::Policy::parse(&policy),
        service: r.get("service")?,
        paused_reason: r.get("paused_reason")?,
        paused_at: r.get("paused_at")?,
        auth_state: r.get("auth_state")?,
        workspace_cwd: r.get("workspace_cwd")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

pub fn insert_connector(
    conn: &Connection,
    id: &str,
    name: &str,
    description: &str,
    transport: &str,
    config: &ConnectorConfig,
    enabled: bool,
) -> rusqlite::Result<Connector> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO connectors (id, name, description, transport, config, enabled, created_at, updated_at,
                                 workspace_cwd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, 0)",
        params![
            id,
            name,
            description,
            transport,
            serde_json::to_string(config).unwrap_or_default(),
            enabled,
            now
        ],
    )?;
    get_connector(conn, id).map(|c| c.expect("inserted"))
}

pub fn get_connector(conn: &Connection, id: &str) -> rusqlite::Result<Option<Connector>> {
    conn.query_row(
        "SELECT * FROM connectors WHERE id = ?1",
        [id],
        connector_from_row,
    )
    .optional()
}

pub fn get_connector_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<Connector>> {
    conn.query_row(
        "SELECT * FROM connectors WHERE name = ?1",
        [name],
        connector_from_row,
    )
    .optional()
}

pub fn list_connectors(conn: &Connection, only_enabled: bool) -> rusqlite::Result<Vec<Connector>> {
    let mut stmt =
        conn.prepare("SELECT * FROM connectors WHERE (?1 = 0 OR enabled = 1) ORDER BY name")?;
    stmt.query_map([only_enabled], connector_from_row)?
        .collect()
}

pub fn update_connector(
    conn: &Connection,
    id: &str,
    description: Option<&str>,
    config: Option<&ConnectorConfig>,
    enabled: Option<bool>,
) -> rusqlite::Result<Option<Connector>> {
    let n = conn.execute(
        "UPDATE connectors SET description = COALESCE(?2, description),
                               config = COALESCE(?3, config),
                               enabled = COALESCE(?4, enabled),
                               updated_at = ?5
         WHERE id = ?1",
        params![
            id,
            description,
            config.map(|c| serde_json::to_string(c).unwrap_or_default()),
            enabled,
            now_ms()
        ],
    )?;
    if n == 0 {
        return Ok(None);
    }
    get_connector(conn, id)
}

/// Gateway-side fields (rules, service, pause, sign-in state). `None`
/// keeps a value; `paused` uses `Some(None)` to clear.
#[derive(Default)]
pub struct ConnectorState<'a> {
    pub policy: Option<&'a crate::gateway::policy::Policy>,
    pub service: Option<&'a str>,
    pub paused: Option<Option<&'a str>>,
    pub auth_state: Option<&'a str>,
}

pub fn set_connector_state(
    conn: &Connection,
    id: &str,
    s: &ConnectorState,
) -> rusqlite::Result<Option<Connector>> {
    if let Some(p) = s.policy {
        conn.execute(
            "UPDATE connectors SET policy = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, serde_json::to_string(p).unwrap_or_default(), now_ms()],
        )?;
    }
    if let Some(svc) = s.service {
        conn.execute(
            "UPDATE connectors SET service = ?2 WHERE id = ?1",
            params![id, svc],
        )?;
    }
    if let Some(paused) = s.paused {
        conn.execute(
            "UPDATE connectors SET paused_reason = ?2, paused_at = ?3 WHERE id = ?1",
            params![id, paused, paused.map(|_| now_ms())],
        )?;
    }
    if let Some(a) = s.auth_state {
        conn.execute(
            "UPDATE connectors SET auth_state = ?2 WHERE id = ?1",
            params![id, a],
        )?;
    }
    get_connector(conn, id)
}

/// Turn every connector off. Returns how many were on.
pub fn disable_all_connectors(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE connectors SET enabled = 0, updated_at = ?1 WHERE enabled = 1",
        [now_ms()],
    )
}

pub fn delete_connector(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM connectors WHERE id = ?1", [id])? > 0)
}

// ---------------------------------------------------------- connector calls

/// One gated call, as recorded in the activity log.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConnectorCall {
    pub id: String,
    pub connector_id: Option<String>,
    pub connector_name: String,
    pub tool: String,
    /// read | draft | write | send | purchase | delete | export
    pub category: String,
    /// auto | trusted | approved | denied | expired | cancelled | blocked |
    /// paused | pending
    pub decision: String,
    /// ok | error | null (not run, or still running)
    pub outcome: Option<String>,
    pub reason: String,
    pub summary: String,
    pub conversation_id: Option<String>,
    pub message_id: Option<String>,
    pub card_message_id: Option<String>,
    pub result_bytes: i64,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

fn call_from_row(r: &rusqlite::Row) -> rusqlite::Result<ConnectorCall> {
    Ok(ConnectorCall {
        id: r.get("id")?,
        connector_id: r.get("connector_id")?,
        connector_name: r.get("connector_name")?,
        tool: r.get("tool")?,
        category: r.get("category")?,
        decision: r.get("decision")?,
        outcome: r.get("outcome")?,
        reason: r.get("reason")?,
        summary: r.get("summary")?,
        conversation_id: r.get("conversation_id")?,
        message_id: r.get("message_id")?,
        card_message_id: r.get("card_message_id")?,
        result_bytes: r.get("result_bytes")?,
        created_at: r.get("created_at")?,
        finished_at: r.get("finished_at")?,
    })
}

/// Record a call. Its `created_at` is assigned here, unique and increasing
/// (it is the activity log's page cursor).
pub fn insert_connector_call(conn: &Connection, c: &ConnectorCall) -> rusqlite::Result<()> {
    let last: Option<i64> =
        conn.query_row("SELECT MAX(created_at) FROM connector_calls", [], |r| {
            r.get(0)
        })?;
    let created_at = now_ms().max(last.map_or(0, |l| l + 1));
    conn.execute(
        "INSERT INTO connector_calls (id, connector_id, connector_name, tool, category, decision,
             outcome, reason, summary, conversation_id, message_id, card_message_id, result_bytes,
             created_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            c.id,
            c.connector_id,
            c.connector_name,
            c.tool,
            c.category,
            c.decision,
            c.outcome,
            c.reason,
            c.summary,
            c.conversation_id,
            c.message_id,
            c.card_message_id,
            c.result_bytes,
            created_at,
            c.finished_at
        ],
    )?;
    Ok(())
}

/// Record the decision (and card) for a call.
pub fn set_call_decision(
    conn: &Connection,
    id: &str,
    decision: &str,
    card_message_id: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE connector_calls SET decision = ?2,
             card_message_id = COALESCE(?3, card_message_id) WHERE id = ?1",
        params![id, decision, card_message_id],
    )?;
    Ok(())
}

/// Record how a call ended.
pub fn finish_connector_call(
    conn: &Connection,
    id: &str,
    outcome: Option<&str>,
    result_bytes: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE connector_calls SET outcome = ?2, result_bytes = ?3, finished_at = ?4 WHERE id = ?1",
        params![id, outcome, result_bytes, now_ms()],
    )?;
    Ok(())
}

/// Recent use of one connector, for the tripwires: (calls that ran,
/// sends that ran, bytes returned) since `since`.
pub fn connector_usage_since(
    conn: &Connection,
    connector_id: &str,
    since: i64,
) -> rusqlite::Result<(i64, i64, i64)> {
    conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(category = 'send'), 0),
                COALESCE(SUM(result_bytes), 0)
         FROM connector_calls
         WHERE connector_id = ?1 AND admitted_at >= ?2
           AND decision IN ('auto', 'trusted', 'approved')",
        params![connector_id, since],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
}

pub fn list_connector_calls(
    conn: &Connection,
    connector_id: Option<&str>,
    before: Option<i64>,
    limit: usize,
) -> rusqlite::Result<Vec<ConnectorCall>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM connector_calls
         WHERE (?1 IS NULL OR connector_id = ?1) AND created_at < ?2
         ORDER BY created_at DESC LIMIT ?3",
    )?;
    stmt.query_map(
        params![connector_id, before.unwrap_or(i64::MAX), limit as i64],
        call_from_row,
    )?
    .collect()
}

/// Calls still waiting for an answer (after a restart nothing waits).
pub fn expire_pending_calls(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE connector_calls SET decision = 'expired', finished_at = ?1 WHERE decision = 'pending'",
        [now_ms()],
    )
}

pub fn add_known_recipients(conn: &Connection, addresses: &[String]) -> rusqlite::Result<()> {
    for a in addresses {
        conn.execute(
            "INSERT OR IGNORE INTO known_recipients (address, created_at) VALUES (?1, ?2)",
            params![a, now_ms()],
        )?;
    }
    Ok(())
}

pub fn known_recipients(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT address FROM known_recipients")?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

// --------------------------------------------------------------- engine keys

pub fn set_engine_key(
    conn: &Connection,
    harness: &str,
    env: &str,
    sealed: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO engine_keys (harness, env, ciphertext, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (harness, env) DO UPDATE SET ciphertext = excluded.ciphertext,
             created_at = excluded.created_at",
        params![harness, env, sealed, now_ms()],
    )?;
    Ok(())
}

pub fn delete_engine_key(conn: &Connection, harness: &str, env: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM engine_keys WHERE harness = ?1 AND env = ?2",
        params![harness, env],
    )? > 0)
}

/// (env name, sealed value, created_at) for one harness.
pub fn engine_keys(
    conn: &Connection,
    harness: &str,
) -> rusqlite::Result<Vec<(String, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT env, ciphertext, created_at FROM engine_keys WHERE harness = ?1 ORDER BY env",
    )?;
    stmt.query_map([harness], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect()
}

// -------------------------------------------------------------- credentials

#[derive(Debug, Clone, Serialize)]
pub struct Credential {
    pub id: String,
    /// login | card
    pub kind: String,
    pub label: String,
    /// Non-secret display data (site, username, card brand, last4, expiry).
    pub metadata: Value,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
}

fn credential_from_row(r: &rusqlite::Row) -> rusqlite::Result<Credential> {
    let metadata: String = r.get("metadata")?;
    Ok(Credential {
        id: r.get("id")?,
        kind: r.get("kind")?,
        label: r.get("label")?,
        metadata: serde_json::from_str(&metadata).unwrap_or(Value::Null),
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        last_used_at: r.get("last_used_at")?,
    })
}

const CREDENTIAL_COLUMNS: &str = "id, kind, label, metadata, created_at, updated_at, last_used_at";

pub fn insert_credential(
    conn: &Connection,
    id: &str,
    user_id: &str,
    kind: &str,
    label: &str,
    metadata: &Value,
    ciphertext: &[u8],
) -> rusqlite::Result<Credential> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO credentials (id, user_id, kind, label, metadata, ciphertext, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![id, user_id, kind, label, metadata.to_string(), ciphertext, now],
    )?;
    get_credential(conn, user_id, id).map(|c| c.expect("inserted"))
}

pub fn get_credential(
    conn: &Connection,
    user_id: &str,
    id: &str,
) -> rusqlite::Result<Option<Credential>> {
    conn.query_row(
        &format!("SELECT {CREDENTIAL_COLUMNS} FROM credentials WHERE id = ?1 AND user_id = ?2"),
        params![id, user_id],
        credential_from_row,
    )
    .optional()
}

pub fn list_credentials(conn: &Connection, user_id: &str) -> rusqlite::Result<Vec<Credential>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CREDENTIAL_COLUMNS} FROM credentials WHERE user_id = ?1 ORDER BY kind, label"
    ))?;
    stmt.query_map([user_id], credential_from_row)?.collect()
}

/// Encrypted secret of a credential (for in-memory merge on update only).
pub fn get_credential_ciphertext(
    conn: &Connection,
    user_id: &str,
    id: &str,
) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row(
        "SELECT ciphertext FROM credentials WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
        |r| r.get(0),
    )
    .optional()
}

pub fn update_credential(
    conn: &Connection,
    user_id: &str,
    id: &str,
    label: Option<&str>,
    metadata: Option<&Value>,
    ciphertext: Option<&[u8]>,
) -> rusqlite::Result<Option<Credential>> {
    let n = conn.execute(
        "UPDATE credentials SET label = COALESCE(?3, label),
                                metadata = COALESCE(?4, metadata),
                                ciphertext = COALESCE(?5, ciphertext),
                                updated_at = ?6
         WHERE id = ?1 AND user_id = ?2",
        params![
            id,
            user_id,
            label,
            metadata.map(|m| m.to_string()),
            ciphertext,
            now_ms()
        ],
    )?;
    if n == 0 {
        return Ok(None);
    }
    get_credential(conn, user_id, id)
}

pub fn delete_credential(conn: &Connection, user_id: &str, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM credentials WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
    )? > 0)
}

// --------------------------------------------------------- push subscriptions

#[derive(Debug, Clone, Serialize)]
pub struct PushSubscription {
    pub id: String,
    pub user_id: String,
    pub device_id: Option<String>,
    pub endpoint: String,
    #[serde(skip)]
    pub p256dh: String,
    #[serde(skip)]
    pub auth: String,
    pub created_at: i64,
    pub last_success_at: Option<i64>,
    pub failure_count: i64,
}

fn push_from_row(r: &rusqlite::Row) -> rusqlite::Result<PushSubscription> {
    Ok(PushSubscription {
        id: r.get("id")?,
        user_id: r.get("user_id")?,
        device_id: r.get("device_id")?,
        endpoint: r.get("endpoint")?,
        p256dh: r.get("p256dh")?,
        auth: r.get("auth")?,
        created_at: r.get("created_at")?,
        last_success_at: r.get("last_success_at")?,
        failure_count: r.get("failure_count")?,
    })
}

/// Insert or re-bind a subscription (endpoints are unique per browser).
pub fn upsert_push_subscription(
    conn: &Connection,
    user_id: &str,
    device_id: &str,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
) -> rusqlite::Result<Option<PushSubscription>> {
    // One statement: the device must still be active (not revoked) when the
    // row is written, so a revocation cannot be undone by a racing subscribe.
    let n = conn.execute(
        "INSERT INTO push_subscriptions (id, user_id, device_id, endpoint, p256dh, auth, created_at)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
         WHERE EXISTS (SELECT 1 FROM devices WHERE id = ?3 AND user_id = ?2 AND revoked_at IS NULL)
         ON CONFLICT(endpoint) DO UPDATE SET user_id = excluded.user_id,
             device_id = excluded.device_id, p256dh = excluded.p256dh, auth = excluded.auth,
             failure_count = 0",
        params![new_id(), user_id, device_id, endpoint, p256dh, auth, now_ms()],
    )?;
    if n == 0 {
        return Ok(None);
    }
    conn.query_row(
        "SELECT * FROM push_subscriptions WHERE endpoint = ?1",
        [endpoint],
        push_from_row,
    )
    .optional()
}

pub fn list_push_subscriptions(
    conn: &Connection,
    user_id: &str,
) -> rusqlite::Result<Vec<PushSubscription>> {
    let mut stmt = conn.prepare(
        "SELECT s.* FROM push_subscriptions s JOIN devices d ON d.id = s.device_id
         WHERE s.user_id = ?1 AND d.revoked_at IS NULL ORDER BY s.created_at",
    )?;
    stmt.query_map([user_id], push_from_row)?.collect()
}

pub fn delete_push_subscription(
    conn: &Connection,
    user_id: &str,
    id_or_endpoint: &str,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM push_subscriptions WHERE user_id = ?1 AND (id = ?2 OR endpoint = ?2)",
        params![user_id, id_or_endpoint],
    )? > 0)
}

pub fn record_push_result(conn: &Connection, id: &str, ok: bool) -> rusqlite::Result<()> {
    if ok {
        conn.execute(
            "UPDATE push_subscriptions SET last_success_at = ?2, failure_count = 0 WHERE id = ?1",
            params![id, now_ms()],
        )?;
    } else {
        conn.execute(
            "UPDATE push_subscriptions SET failure_count = failure_count + 1 WHERE id = ?1",
            [id],
        )?;
        // Give up on subscriptions that keep failing.
        conn.execute(
            "DELETE FROM push_subscriptions WHERE id = ?1 AND failure_count >= 10",
            [id],
        )?;
    }
    Ok(())
}

pub fn delete_push_by_id(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM push_subscriptions WHERE id = ?1", [id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, create_conversation, list_messages, owner_user_id};

    fn with_db(f: impl FnOnce(&Connection, &str) -> rusqlite::Result<()>) {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            f(c, &owner)
        })
        .unwrap();
    }

    #[test]
    fn hourly_usage_is_an_index_lookup() {
        with_db(|c, _| {
            let plan: Vec<String> = c
                .prepare(
                    "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM connector_calls
                     WHERE connector_id = ?1 AND admitted_at >= ?2
                       AND decision IN ('auto', 'trusted', 'approved')",
                )?
                .query_map(params!["c", 0], |r| r.get::<_, String>(3))?
                .collect::<rusqlite::Result<_>>()?;
            assert!(
                plan.iter().any(|p| p.contains("connector_calls_admitted")
                    && p.contains("connector_id=? AND admitted_at>?")),
                "{plan:?}"
            );
            Ok(())
        });
    }

    #[test]
    fn open_ideas_are_never_crowded_out_by_dismissed_ones() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            let old = create_idea(c, &owner, "Old but open", "", None)?;
            for i in 0..250 {
                let idea = create_idea(c, &owner, &format!("dismissed {i}"), "", None)?;
                update_idea(c, &owner, &idea.id, Some("dismissed"), None)?;
            }
            let all = list_ideas(c, &owner, None)?;
            assert!(
                all.iter().all(|i| i.id != old.id),
                "the cap hides it in the full list"
            );
            let active = list_ideas(c, &owner, Some("open,started"))?;
            assert_eq!(active.len(), 1);
            assert_eq!(active[0].id, old.id);
            assert_eq!(list_ideas(c, &owner, Some("dismissed"))?.len(), 200);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn job_artifacts_have_no_chat_link() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            let main = crate::db::ensure_main_conversation(c, &owner)?;
            let job = crate::db::ensure_job_conversation(c, &owner, "heartbeat")?;
            for (id, conv) in [("a1", &main), ("a2", &job)] {
                insert_library_item(
                    c,
                    &owner,
                    &NewLibraryItem {
                        id,
                        kind: "artifact",
                        title: id,
                        description: "",
                        mime: "text/markdown",
                        file_name: &format!("{id}.md"),
                        size: 1,
                        conversation_id: Some(conv),
                    },
                )?;
            }
            let items = list_library(c, &owner, None)?;
            let link = |id: &str| {
                items
                    .iter()
                    .find(|i| i.id == id)
                    .unwrap()
                    .conversation_id
                    .clone()
            };
            assert_eq!(link("a1").as_deref(), Some(main.as_str()));
            assert_eq!(link("a2"), None);
            assert_eq!(
                get_library_item(c, &owner, "a2")?.unwrap().conversation_id,
                None
            );
            // Paging by (created_at, id) walks every item once, ties included.
            c.execute("UPDATE library_items SET created_at = 42", [])?;
            let first = list_library_page(c, &owner, None, None, 1)?;
            let last = first.last().unwrap();
            let second = list_library_page(c, &owner, None, Some((last.created_at, &last.id)), 1)?;
            assert_eq!(first.len() + second.len(), 2);
            assert_ne!(first[0].id, second[0].id);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn goals_lifecycle() {
        with_db(|c, u| {
            let g = create_goal(c, u, " Run a 10k ", "", false)?;
            assert_eq!(g.title, "Run a 10k");
            let g2 = update_goal(
                c,
                u,
                &g.id,
                &GoalChanges {
                    status_text: Some("3 runs this week"),
                    done: Some(true),
                    ..Default::default()
                },
            )?
            .unwrap();
            assert!(g2.done && g2.done_at.is_some());
            assert_eq!(g2.status_text, "3 runs this week");
            assert!(list_goals(c, u, false)?.is_empty());
            assert_eq!(list_goals(c, u, true)?.len(), 1);
            let g3 = update_goal(
                c,
                u,
                &g.id,
                &GoalChanges {
                    done: Some(false),
                    suggested: Some(true),
                    ..Default::default()
                },
            )?
            .unwrap();
            assert!(g3.suggested);
            assert!(!g3.done && g3.done_at.is_none());
            let other = GoalChanges {
                title: Some("x"),
                ..Default::default()
            };
            assert!(update_goal(c, "other", &g.id, &other)?.is_none());
            assert!(delete_goal(c, u, &g.id)?);
            Ok(())
        });
    }

    #[test]
    fn feed_and_ideas() {
        with_db(|c, u| {
            let p = create_feed_post(
                c,
                u,
                &NewFeedPost {
                    title: "Comet visible tonight".into(),
                    body: "Look north after 9pm.".into(),
                    topic: Some("astronomy".into()),
                    ..Default::default()
                },
            )?;
            assert!(set_feed_liked(c, u, &p.id, true)?);
            assert!(get_feed_post(c, u, &p.id)?.unwrap().liked);
            assert_eq!(list_feed(c, u, None, 10)?.len(), 1);
            assert!(get_feed_post(c, "other", &p.id)?.is_none());
            let i = create_idea(
                c,
                u,
                "Plan a picnic",
                "Saturday looks sunny",
                Some("Plan a picnic for Saturday"),
            )?;
            assert_eq!(i.status, "open");
            assert!(update_idea(c, u, &i.id, Some("started"), None)?);
            assert_eq!(list_ideas(c, u, Some("started"))?.len(), 1);
            assert_eq!(list_ideas(c, u, Some("open"))?.len(), 0);
            Ok(())
        });
        assert!(
            NewFeedPost {
                title: "t".into(),
                body: "b".into(),
                source_url: Some("javascript:alert(1)".into()),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn cards_validate_and_insert() {
        assert!(validate_card(&json!({"type": "link", "title": "Google"})).is_err());
        assert!(validate_card(&json!({"type": "evil", "title": "x"})).is_err());
        assert!(
            validate_card(&json!({"type": "link", "title": "x", "url": "file:///etc/passwd"}))
                .is_err()
        );
        let card = validate_card(&json!({
            "type": "connect", "title": "Connect Google", "url": "https://accounts.google.com/",
            "service": "google", "extra": "dropped"
        }))
        .unwrap();
        assert!(card.get("extra").is_none());
        with_db(|c, u| {
            let conv = create_conversation(c, u, "side", "", None)?;
            let m = insert_card(c, &conv.id, &card, "assistant")?;
            assert_eq!(
                (m.kind.as_str(), m.content.as_str()),
                ("card", "Connect Google")
            );
            assert_eq!(
                list_messages(c, &conv.id, None, 10)?[0]
                    .card
                    .as_ref()
                    .unwrap()["service"],
                "google"
            );
            Ok(())
        });
    }

    #[test]
    fn connectors_and_credentials_hide_secrets() {
        with_db(|c, u| {
            let mut cfg = ConnectorConfig {
                command: Some("npx".into()),
                args: vec!["server".into()],
                ..Default::default()
            };
            cfg.env.insert("API_TOKEN".into(), "sealed-value".into());
            let con = insert_connector(c, "c1", "calendar", "", "stdio", &cfg, true)?;
            let pj = con.public_json().to_string();
            assert!(pj.contains("API_TOKEN") && !pj.contains("sealed-value"));
            assert_eq!(list_connectors(c, true)?.len(), 1);
            update_connector(c, "c1", None, None, Some(false))?;
            assert_eq!(list_connectors(c, true)?.len(), 0);
            let cred = insert_credential(
                c,
                "k1",
                u,
                "login",
                "Bank",
                &json!({"site": "bank.example"}),
                b"ciphertext",
            )?;
            let s = serde_json::to_string(&cred).unwrap();
            assert!(!s.contains("ciphertext"));
            assert_eq!(list_credentials(c, u)?.len(), 1);
            Ok(())
        });
    }

    #[test]
    fn push_subscriptions_upsert_and_prune() {
        with_db(|c, u| {
            let d = crate::db::insert_device(c, u, "phone", "hash")?;
            let a =
                upsert_push_subscription(c, u, &d.id, "https://push.example/1", "k", "a")?.unwrap();
            let b = upsert_push_subscription(c, u, &d.id, "https://push.example/1", "k2", "a2")?
                .unwrap();
            assert_eq!(a.id, b.id);
            assert_eq!(b.p256dh, "k2");
            for _ in 0..10 {
                record_push_result(c, &a.id, false)?;
            }
            assert!(list_push_subscriptions(c, u)?.is_empty());
            Ok(())
        });
    }

    #[test]
    fn push_subscribe_refused_for_revoked_device() {
        with_db(|c, u| {
            let d = crate::db::insert_device(c, u, "phone", "hash")?;
            assert!(crate::db::revoke_device(c, u, &d.id)?);
            let r = upsert_push_subscription(c, u, &d.id, "https://push.example/2", "k", "a")?;
            assert!(r.is_none());
            assert!(list_push_subscriptions(c, u)?.is_empty());
            // A row that somehow survives revocation is not listed either.
            c.execute(
                "INSERT INTO push_subscriptions (id, user_id, device_id, endpoint, p256dh, auth, created_at)
                 VALUES ('x', ?1, ?2, 'https://push.example/3', 'k', 'a', 1)",
                params![u, d.id],
            )?;
            assert!(list_push_subscriptions(c, u)?.is_empty());
            Ok(())
        });
    }
}
