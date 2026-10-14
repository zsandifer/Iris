//! The owner's long-term memory.
//!
//! SQLite rows (`memory_items`) are the truth: one row per note, with its
//! kind, topic, importance, provenance and validity. Every change writes a
//! `memory_history` row (before and after snapshots, who changed it, and a
//! batch id), so any change can be undone. An FTS5 table over the note text
//! is kept in sync by triggers.
//!
//! The Markdown in `memory/` is generated from the rows after every change
//! (see [`files`]) and is read-only for the owner: `MEMORY.md` holds the
//! notes that are always in the prompt, packed under a budget (see
//! [`packing`]), and one file per topic lists every note of that topic.
//! Memory is only ever reachable from the owner's private chats.

pub mod files;
pub mod gate;
pub mod markdown;
pub mod packing;
pub mod recall;

use crate::util::{now_ms, sha256_hex};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How notes are grouped in the app and in the export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Topic {
    You,
    People,
    Projects,
    Routines,
}

impl Topic {
    pub const ALL: [Topic; 4] = [Topic::You, Topic::People, Topic::Projects, Topic::Routines];

    pub fn name(self) -> &'static str {
        match self {
            Topic::You => "you",
            Topic::People => "people",
            Topic::Projects => "projects",
            Topic::Routines => "routines",
        }
    }

    pub fn parse(s: &str) -> Option<Topic> {
        Topic::ALL.into_iter().find(|t| t.name() == s)
    }

    /// The export file listing this topic's notes.
    pub fn file(self) -> &'static str {
        match self {
            Topic::You => "you.md",
            Topic::People => "people.md",
            Topic::Projects => "projects.md",
            Topic::Routines => "routines.md",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Topic::You => "About you",
            Topic::People => "People",
            Topic::Projects => "Projects",
            Topic::Routines => "Routines",
        }
    }
}

/// Kind of a note. The schema also allows profile, preference, event,
/// person, project and routine; notes from Markdown are facts.
pub const KIND_FACT: &str = "fact";

/// Kinds that say who the owner is: packed into the core right after
/// pinned notes, and part of the persona hash.
pub const CORE_KINDS: &[&str] = &["profile", "preference"];

/// Who made a change (`memory_history.actor`). The schema also allows
/// `dream` (the nightly tidy-up) and `owner` (edits in the app).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// The agent, through a memory tool in the owner's private chat.
    Agent,
    /// Imported from Markdown notes.
    Import,
    Owner,
}

impl Actor {
    fn name(self) -> &'static str {
        match self {
            Actor::Agent => "agent",
            Actor::Import => "import",
            Actor::Owner => "owner",
        }
    }
}

/// One note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: i64,
    pub kind: String,
    pub topic: Topic,
    pub text: String,
    /// 1 (trivia) to 5 (essential).
    pub importance: i64,
    pub pinned: bool,
    pub created_at: i64,
    /// When the fact happened or became true.
    pub observed_at: Option<i64>,
    /// When it was said.
    pub mentioned_at: Option<i64>,
    /// When it stops being true.
    pub valid_to: Option<i64>,
    pub superseded_by: Option<i64>,
    /// `{"type": "conversation", "conversation_id", "message_ids"}`,
    /// `{"type": "import", "file"}`, or `{"type": "owner"}`.
    pub source: Value,
    pub last_used_at: Option<i64>,
    pub use_count: i64,
    pub content_hash: String,
    pub archived: bool,
    pub updated_at: i64,
    /// When the note last joined the always-in-prompt core; None while it
    /// is not in it. Derived from the other fields when the export is
    /// written, so it is not part of history snapshots.
    #[serde(skip)]
    pub core_since: Option<i64>,
    /// Last batch that explicitly wrote this note to MEMORY.md. Packing
    /// alone does not count as the turn having seen the note there.
    #[serde(skip)]
    pub core_written_batch: Option<String>,
}

const COLUMNS: &str = "id, kind, topic, text, importance, pinned, created_at, observed_at, \
    mentioned_at, valid_to, superseded_by, source, last_used_at, use_count, content_hash, \
    archived, updated_at, core_since, core_written_batch";

fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Item> {
    let topic: String = r.get(2)?;
    let source: String = r.get(11)?;
    Ok(Item {
        id: r.get(0)?,
        kind: r.get(1)?,
        topic: Topic::parse(&topic).unwrap_or(Topic::You),
        text: r.get(3)?,
        importance: r.get(4)?,
        pinned: r.get(5)?,
        created_at: r.get(6)?,
        observed_at: r.get(7)?,
        mentioned_at: r.get(8)?,
        valid_to: r.get(9)?,
        superseded_by: r.get(10)?,
        source: serde_json::from_str(&source).unwrap_or(Value::Null),
        last_used_at: r.get(12)?,
        use_count: r.get(13)?,
        content_hash: r.get(14)?,
        archived: r.get(15)?,
        updated_at: r.get(16)?,
        core_since: r.get(17)?,
        core_written_batch: r.get(18)?,
    })
}

/// Identity of a note's text: case, spacing and a final full stop do not
/// make a different note.
pub fn content_hash(text: &str) -> String {
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    let joined = words.join(" ");
    sha256_hex(joined.trim_end_matches('.'))
}

/// A note to add.
pub struct NewItem<'a> {
    pub kind: &'a str,
    pub topic: Topic,
    pub text: &'a str,
    pub importance: i64,
    pub source: &'a Value,
}

/// Result of [`add`].
pub enum Added {
    New,
    /// A live note with the same text already exists; nothing was added.
    Existing(Box<Item>),
}

pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Item>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM memory_items WHERE id = ?1"),
        [id],
        from_row,
    )
    .optional()
}

pub(crate) fn live_by_hash(conn: &Connection, hash: &str) -> rusqlite::Result<Option<Item>> {
    conn.query_row(
        &format!(
            "SELECT {COLUMNS} FROM memory_items
             WHERE content_hash = ?1 AND archived = 0 AND superseded_by IS NULL"
        ),
        [hash],
        from_row,
    )
    .optional()
}

/// The batch of the latest change to a note.
pub fn last_batch(conn: &Connection, id: i64) -> rusqlite::Result<Option<String>> {
    conn.prepare_cached(
        "SELECT batch_id FROM memory_history WHERE item_id = ?1 ORDER BY id DESC LIMIT 1",
    )?
    .query_row([id], |r| r.get(0))
    .optional()
    .map(Option::flatten)
}

/// Whether a note with this text was archived or superseded at or after
/// `since` (ms) by a change outside `batch`.
pub fn set_aside_since(
    conn: &Connection,
    hash: &str,
    since: i64,
    batch: &str,
) -> rusqlite::Result<bool> {
    conn.prepare_cached(
        "SELECT 1 FROM memory_items m WHERE m.content_hash = ?1
         AND (m.archived = 1 OR m.superseded_by IS NOT NULL) AND m.updated_at >= ?2
         AND COALESCE((SELECT h.batch_id FROM memory_history h WHERE h.item_id = m.id
                       ORDER BY h.id DESC LIMIT 1), '') != ?3",
    )?
    .exists(params![hash, since, batch])
}

/// Every live note (not archived, not superseded), oldest first.
pub fn live(conn: &Connection) -> rusqlite::Result<Vec<Item>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM memory_items
         WHERE archived = 0 AND superseded_by IS NULL ORDER BY created_at, id"
    ))?;
    stmt.query_map([], from_row)?.collect()
}

fn snapshot(item: &Item) -> String {
    serde_json::to_string(item).unwrap_or_default()
}

fn record(
    conn: &Connection,
    item_id: i64,
    op: &str,
    before: Option<&Item>,
    after: Option<&Item>,
    actor: Actor,
    batch: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_history (item_id, op, before, after, actor, batch_id, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            item_id,
            op,
            before.map(snapshot),
            after.map(snapshot),
            actor.name(),
            batch,
            now_ms()
        ],
    )?;
    Ok(())
}

/// Add a note unless a live note with the same text exists. Call inside a
/// write transaction.
pub fn add(conn: &Connection, new: &NewItem, actor: Actor, batch: &str) -> rusqlite::Result<Added> {
    let text = gate::scrub_document(new.text);
    if text.is_empty() {
        return Err(recall::invalid(
            "I couldn't save a note containing only private or secret information.",
        ));
    }
    let hash = content_hash(&text);
    if let Some(existing) = live_by_hash(conn, &hash)? {
        return Ok(Added::Existing(Box::new(existing)));
    }
    let now = now_ms();
    conn.execute(
        "INSERT INTO memory_items
             (id, kind, topic, text, importance, created_at, source, content_hash, updated_at)
         VALUES ((SELECT MAX(id) + 1 FROM (SELECT COALESCE(MAX(id), 0) AS id FROM memory_items UNION ALL SELECT COALESCE(MAX(item_id), 0) FROM memory_history)), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?5)",
        params![
            new.kind,
            new.topic.name(),
            text,
            new.importance.clamp(1, 5),
            now,
            new.source.to_string(),
            hash
        ],
    )?;
    let item = get(conn, conn.last_insert_rowid())?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    record(conn, item.id, "add", None, Some(&item), actor, batch)?;
    Ok(Added::New)
}

/// Raise a note's importance and move it to `topic`, recording history.
/// Does nothing when neither changes.
pub fn regroup(
    conn: &Connection,
    item: &Item,
    importance: i64,
    topic: Topic,
    actor: Actor,
    batch: &str,
) -> rusqlite::Result<()> {
    let importance = importance.clamp(1, 5).max(item.importance);
    if importance == item.importance && topic == item.topic {
        return Ok(());
    }
    conn.execute(
        "UPDATE memory_items SET importance = ?2, topic = ?3, updated_at = ?4 WHERE id = ?1",
        params![item.id, importance, topic.name(), now_ms()],
    )?;
    let after = get(conn, item.id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    record(
        conn,
        item.id,
        "update",
        Some(item),
        Some(&after),
        actor,
        batch,
    )
}

/// Set a note aside. It stays in the database (and its history) so the
/// change can be undone.
pub fn archive(conn: &Connection, item: &Item, actor: Actor, batch: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE memory_items SET archived = 1, updated_at = ?2 WHERE id = ?1",
        params![item.id, now_ms()],
    )?;
    let after = get(conn, item.id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    record(
        conn,
        item.id,
        "archive",
        Some(item),
        Some(&after),
        actor,
        batch,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::{Db, write_tx};
    use serde_json::json;

    /// A note for rendering tests (not stored).
    pub(crate) fn item(id: i64, text: &str, topic: Topic) -> Item {
        Item {
            id,
            kind: KIND_FACT.into(),
            topic,
            text: text.into(),
            importance: 3,
            pinned: false,
            created_at: id,
            observed_at: None,
            mentioned_at: None,
            valid_to: None,
            superseded_by: None,
            source: Value::Null,
            last_used_at: None,
            use_count: 0,
            content_hash: content_hash(text),
            archived: false,
            updated_at: id,
            core_since: None,
            core_written_batch: None,
        }
    }

    fn new<'a>(text: &'a str, source: &'a Value) -> NewItem<'a> {
        NewItem {
            kind: KIND_FACT,
            topic: Topic::You,
            text,
            importance: 3,
            source,
        }
    }

    /// item_id, op, before, after, actor, batch_id
    type HistoryRow = (i64, String, Option<String>, Option<String>, String, String);

    fn newest(c: &Connection) -> Item {
        let id: i64 = c
            .query_row("SELECT MAX(id) FROM memory_items", [], |r| r.get(0))
            .unwrap();
        get(c, id).unwrap().unwrap()
    }

    fn fts(c: &Connection, q: &str) -> Vec<i64> {
        let mut s = c
            .prepare(
                "SELECT rowid FROM memory_items_fts WHERE memory_items_fts MATCH ?1 ORDER BY rowid",
            )
            .unwrap();
        s.query_map([q], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn hash_ignores_case_spacing_and_final_stop() {
        assert_eq!(
            content_hash("Prefers  tea in the\nmorning."),
            content_hash("prefers tea in the morning")
        );
        assert_ne!(content_hash("Prefers tea"), content_hash("Prefers coffee"));
    }

    #[test]
    fn add_dedups_live_notes_and_records_history() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            write_tx(c, |c| {
                let src = json!({"type": "import", "file": "MEMORY.md"});
                let Added::New = add(c, &new("Likes tea.", &src), Actor::Import, "b1")? else {
                    panic!("first add is new");
                };
                let a = newest(c);
                assert_eq!(a.text, "Likes tea.");
                assert_eq!(a.source["file"], "MEMORY.md");
                let Added::Existing(same) = add(c, &new("likes  TEA", &src), Actor::Agent, "b2")?
                else {
                    panic!("same text is a duplicate");
                };
                assert_eq!(same.id, a.id);
                // The unique index backs the check up.
                let dup = c.execute(
                    "INSERT INTO memory_items (kind, topic, text, created_at, content_hash, updated_at)
                     VALUES ('fact', 'you', 'x', 0, ?1, 0)",
                    [&a.content_hash],
                );
                assert!(dup.is_err());
                // Archived notes do not block the same text coming back.
                archive(c, &a, Actor::Agent, "b3")?;
                let Added::New = add(c, &new("Likes tea", &src), Actor::Agent, "b4")? else {
                    panic!("archived text can be added again");
                };
                let again = newest(c);
                assert_ne!(again.id, a.id);
                regroup(c, &again, 4, Topic::People, Actor::Agent, "b5")?;
                regroup(c, &newest(c), 2, Topic::You, Actor::Agent, "b6")?;
                // No change, no history.
                regroup(c, &newest(c), 1, Topic::You, Actor::Agent, "b7")?;
                let history: Vec<HistoryRow> = c
                    .prepare("SELECT item_id, op, before, after, actor, batch_id FROM memory_history ORDER BY id")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                let ops: Vec<(&str, &str, &str)> = history
                    .iter()
                    .map(|h| (h.1.as_str(), h.4.as_str(), h.5.as_str()))
                    .collect();
                assert_eq!(
                    ops,
                    [
                        ("add", "import", "b1"),
                        ("archive", "agent", "b3"),
                        ("add", "agent", "b4"),
                        ("update", "agent", "b5"),
                        ("update", "agent", "b6"),
                    ]
                );
                assert!(history[0].2.is_none());
                let before: Value = serde_json::from_str(history[1].2.as_deref().unwrap()).unwrap();
                let after: Value = serde_json::from_str(history[1].3.as_deref().unwrap()).unwrap();
                assert_eq!((before["archived"].clone(), after["archived"].clone()), (json!(false), json!(true)));
                let moved: Value = serde_json::from_str(history[4].3.as_deref().unwrap()).unwrap();
                // Importance never goes down through regroup.
                assert_eq!((moved["importance"].clone(), moved["topic"].clone()), (json!(4), json!("you")));
                assert_eq!(live(c)?.len(), 1);
                Ok(())
            })
        })
        .unwrap();
    }

    #[test]
    fn keyword_search_follows_inserts_updates_and_deletes() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let src = json!({});
            add(
                c,
                &new("Walks the dog every morning", &src),
                Actor::Agent,
                "b",
            )?;
            let a = newest(c);
            add(c, &new("Sister lives by the sea", &src), Actor::Agent, "b")?;
            // Porter stemming: "walking" finds "Walks".
            assert_eq!(fts(c, "walking"), [a.id]);
            c.execute(
                "UPDATE memory_items SET text = 'Runs every evening' WHERE id = ?1",
                [a.id],
            )?;
            assert!(fts(c, "walking").is_empty());
            assert_eq!(fts(c, "evening"), [a.id]);
            c.execute("DELETE FROM memory_items WHERE id = ?1", [a.id])?;
            assert!(fts(c, "evening").is_empty());
            assert_eq!(fts(c, "sea").len(), 1);
            Ok(())
        })
        .unwrap();
    }
}
