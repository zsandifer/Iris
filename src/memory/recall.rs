//! Bounded recall and history-backed mutations shared by MCP and REST.
use super::{Actor, COLUMNS, Item, Topic, from_row, get, record};
use crate::config::Paths;
use crate::util::{now_ms, truncate_chars};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub const TOOLS: &[&str] = &[
    "search_memory",
    "get_memory",
    "remember",
    "update_memory",
    "forget",
    "search_conversations",
];

pub fn invalid(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(message.into().into())
}

/// Search words literally, without exposing FTS operators or syntax errors.
pub fn query(text: &str) -> rusqlite::Result<String> {
    if text.chars().count() > 500 {
        return Err(invalid("I can search up to 500 characters at a time."));
    }
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .take(32)
        .map(|s| format!("\"{s}\""))
        .collect();
    if words.is_empty() {
        return Err(invalid("Please give me a few words to search for."));
    }
    Ok(words.join(" OR "))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub query: Option<String>,
    pub topic: Option<String>,
    pub since: Option<i64>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

fn list_sql(search: bool, used: bool) -> String {
    let columns = COLUMNS
        .split(',')
        .map(|c| format!("m.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT {columns} FROM {} WHERE m.archived = 0 AND m.superseded_by IS NULL
        AND (m.valid_to IS NULL OR m.valid_to > ?1) AND (?2 IS NULL OR m.topic = ?2)
        AND (?3 IS NULL OR m.updated_at >= ?3) AND ?4 >= 0 AND {}
        ORDER BY m.pinned DESC, {} m.updated_at DESC, m.id DESC LIMIT ?6 OFFSET ?4",
        if search {
            "memory_items_fts JOIN memory_items m ON m.id = memory_items_fts.rowid"
        } else {
            "memory_items m"
        },
        if search {
            "memory_items_fts MATCH ?5"
        } else {
            "?5 IS NULL"
        },
        if search && used { "rank," } else { "" }
    )
}

pub fn list(c: &Connection, f: &Filter, used: bool) -> rusqlite::Result<Value> {
    if f.topic
        .as_deref()
        .is_some_and(|t| Topic::parse(t).is_none())
    {
        return Err(invalid("Please choose You, People, Projects or Routines."));
    }
    let q = f.query.as_deref().map(query).transpose()?;
    let limit = f.limit.unwrap_or(30).clamp(1, 100);
    let sql = list_sql(q.is_some(), used);
    let mut items: Vec<Item> = c
        .prepare(&sql)?
        .query_map(
            params![
                now_ms(),
                f.topic,
                f.since,
                f.offset.unwrap_or(0).min(100_000) as i64,
                q,
                limit as i64 + 1
            ],
            from_row,
        )?
        .collect::<rusqlite::Result<_>>()?;
    let more = items.len() > limit;
    items.truncate(limit);
    if used {
        for i in &mut items {
            touch(c, i.id)?;
            i.last_used_at = Some(now_ms());
            i.use_count += 1;
        }
        let hits: Vec<Value> = items.iter().map(|i| json!({"id": i.id, "text": truncate_chars(&i.text.replace('\n', " "), 240), "topic": i.topic, "kind": i.kind, "observed_at": i.observed_at, "updated_at": i.updated_at})).collect();
        Ok(json!({"hits": hits, "has_more": more}))
    } else {
        Ok(json!({"items": items, "has_more": more}))
    }
}

fn touch(c: &Connection, id: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE memory_items SET last_used_at = ?2, use_count = use_count + 1 WHERE id = ?1",
        params![id, now_ms()],
    )?;
    Ok(())
}

pub fn history(c: &Connection, id: i64) -> rusqlite::Result<Vec<Value>> {
    c.prepare("WITH RECURSIVE versions(id) AS (SELECT ?1 UNION SELECT m.id FROM memory_items m JOIN versions v ON m.superseded_by = v.id) SELECT id, op, before, after, actor, batch_id, at, item_id FROM memory_history WHERE item_id IN (SELECT id FROM versions) ORDER BY id DESC LIMIT 100")?
        .query_map([id], |r| {
            let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str::<Value>(&s).ok());
            Ok(json!({"id": r.get::<_, i64>(0)?, "op": r.get::<_, String>(1)?, "before": parse(r.get(2)?), "after": parse(r.get(3)?), "actor": r.get::<_, String>(4)?, "batch_id": r.get::<_, Option<String>>(5)?, "at": r.get::<_, i64>(6)?, "item_id": r.get::<_, i64>(7)?}))
        })?.collect()
}

pub fn detail(c: &Connection, id: i64, used: bool) -> rusqlite::Result<Value> {
    let mut item = get(c, id)?.ok_or_else(|| invalid("I couldn't find that memory."))?;
    if used {
        if item.archived
            || item.superseded_by.is_some()
            || item.valid_to.is_some_and(|t| t <= now_ms())
        {
            return Err(invalid("That memory is no longer current."));
        }
        touch(c, id)?;
        item.last_used_at = Some(now_ms());
        item.use_count += 1;
    }
    Ok(json!({"item": item, "history": history(c, id)?}))
}

pub fn conversations(c: &Connection, text: &str) -> rusqlite::Result<Value> {
    let q = query(text)?;
    let hits: Vec<Value> = c.prepare("SELECT p.id, p.conversation_id, p.role, p.created_at,
        snippet(conversation_messages_fts, 2, '', '', '…', 32), c.title
        FROM conversation_messages_fts JOIN memory_private_messages p ON p.message_rowid = conversation_messages_fts.rowid
        JOIN conversations c ON c.id = p.conversation_id
        WHERE conversation_messages_fts MATCH ?1 ORDER BY rank, p.created_at DESC LIMIT 30")?
        .query_map([q], |r| Ok(json!({"message_id": r.get::<_, String>(0)?, "conversation_id": r.get::<_, String>(1)?, "role": r.get::<_, String>(2)?, "created_at": r.get::<_, i64>(3)?, "text": truncate_chars(&r.get::<_, String>(4)?, 500), "title": r.get::<_, String>(5)?})))?.collect::<rusqlite::Result<_>>()?;
    Ok(json!({"hits": hits}))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub text: Option<String>,
    pub kind: Option<String>,
    pub importance: Option<i64>,
    #[serde(default, deserialize_with = "nullable_time")]
    pub observed_at: Option<Option<i64>>,
    #[serde(default, deserialize_with = "nullable_time")]
    pub valid_to: Option<Option<i64>>,
    pub pinned: Option<bool>,
}

// Missing retains the timestamp; an explicit null clears it.
fn nullable_time<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<i64>>, D::Error> {
    Option::<i64>::deserialize(d).map(Some)
}

fn fields(edit: &Edit, before: Option<&Item>) -> rusqlite::Result<(String, String, Topic, i64)> {
    let text = super::gate::scrub(
        edit.text
            .as_deref()
            .or(before.map(|i| i.text.as_str()))
            .ok_or_else(|| invalid("Please give me a note to remember."))?,
    )
    .map_err(invalid)?;
    let kind = edit
        .kind
        .as_deref()
        .or(before.map(|i| i.kind.as_str()))
        .unwrap_or("fact");
    let topic = match kind {
        "profile" | "preference" | "fact" | "event" => Topic::You,
        "person" => Topic::People,
        "project" => Topic::Projects,
        "routine" => Topic::Routines,
        _ => return Err(invalid("Please choose a supported memory kind.")),
    };
    // Imports use kind=fact with a topic inferred from Markdown headings.
    // Editing or pinning must keep that topic unless the kind changes.
    let topic = before.filter(|i| i.kind == kind).map_or(topic, |i| i.topic);
    let importance = edit
        .importance
        .or(before.map(|i| i.importance))
        .unwrap_or(3);
    if !(1..=5).contains(&importance) {
        return Err(invalid("Please choose an importance between 1 and 5."));
    }
    Ok((text, kind.into(), topic, importance))
}

/// Advisory overlap only; it never changes another note.
fn similar(c: &Connection, text: &str, except: i64) -> rusqlite::Result<Option<i64>> {
    use std::collections::HashSet;
    let words = |s: &str| {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|s| s.chars().count() > 2)
            .map(str::to_lowercase)
            .collect::<HashSet<_>>()
    };
    let a = words(text);
    if a.len() < 3 {
        return Ok(None);
    }
    let q = query(&text.chars().take(500).collect::<String>())?;
    let candidates: Vec<(i64, String)> = c.prepare("SELECT m.id, m.text FROM memory_items_fts f JOIN memory_items m ON m.id = f.rowid WHERE memory_items_fts MATCH ?1 AND m.archived = 0 AND m.superseded_by IS NULL AND m.id != ?2 AND (m.valid_to IS NULL OR m.valid_to > ?3) ORDER BY rank LIMIT 20")?.query_map(params![q, except, now_ms()], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(candidates.into_iter().find_map(|(id, t)| {
        let b = words(&t);
        (a.intersection(&b).count() as f64 / a.union(&b).count().max(1) as f64 >= 0.6).then_some(id)
    }))
}

pub fn save(
    c: &Connection,
    edit: &Edit,
    id: Option<i64>,
    actor: Actor,
    source: &Value,
    batch: &str,
) -> rusqlite::Result<Value> {
    let before = id
        .map(|id| get(c, id)?.ok_or_else(|| invalid("I couldn't find that memory.")))
        .transpose()?;
    if before
        .as_ref()
        .is_some_and(|i| i.archived || i.superseded_by.is_some())
    {
        return Err(invalid(
            "That memory was set aside. Please undo its last change first.",
        ));
    }
    let (text, kind, topic, importance) = fields(edit, before.as_ref())?;
    let hash = super::content_hash(&text);
    if let Some(duplicate) = super::live_by_hash(c, &hash)?
        && Some(duplicate.id) != id
    {
        if duplicate.valid_to.is_some_and(|t| t <= now_ms()) {
            if actor == Actor::Dream && duplicate.pinned {
                return Err(invalid(
                    "I keep pinned notes always in mind. Please edit this one in Memory.",
                ));
            }
            super::archive(c, &duplicate, actor, batch)?;
        } else {
            return Ok(json!({"item": duplicate, "duplicate": true, "changed": false}));
        }
    }
    let hint = similar(c, &text, id.unwrap_or(0))?;
    let now = now_ms();
    let replace = before
        .as_ref()
        .is_some_and(|i| actor == Actor::Agent && i.content_hash != hash);
    let saved_id;
    if let Some(old) = before.as_ref().filter(|_| !replace) {
        saved_id = old.id;
        c.execute("UPDATE memory_items SET kind = ?2, topic = ?3, text = ?4, importance = ?5, pinned = ?6, observed_at = ?7, valid_to = ?8, content_hash = ?9, updated_at = ?10, source = ?11, mentioned_at = ?12 WHERE id = ?1",
            params![old.id, kind, topic.name(), text, importance, edit.pinned.unwrap_or(old.pinned), edit.observed_at.unwrap_or(old.observed_at), edit.valid_to.unwrap_or(old.valid_to), hash, now, if actor == Actor::Dream { source } else { &old.source }.to_string(), if actor == Actor::Dream { source["asserted_at"].as_i64().or(Some(now)) } else { old.mentioned_at }])?;
        record(
            c,
            old.id,
            "update",
            Some(old),
            get(c, old.id)?.as_ref(),
            actor,
            batch,
        )?;
    } else {
        c.execute("INSERT INTO memory_items(id, kind, topic, text, importance, pinned, created_at, observed_at, mentioned_at, valid_to, source, content_hash, updated_at) VALUES ((SELECT MAX(id) + 1 FROM (SELECT COALESCE(MAX(id), 0) AS id FROM memory_items UNION ALL SELECT COALESCE(MAX(item_id), 0) FROM memory_history)), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?11, ?8, ?9, ?10, ?6)",
            params![kind, topic.name(), text, importance, edit.pinned.or(before.as_ref().map(|i| i.pinned)).unwrap_or(false), now, edit.observed_at.unwrap_or_else(|| before.as_ref().and_then(|i| i.observed_at)), edit.valid_to.unwrap_or_else(|| before.as_ref().and_then(|i| i.valid_to)), source.to_string(), hash, if actor == Actor::Dream { source["asserted_at"].as_i64().unwrap_or(now) } else { now }])?;
        saved_id = c.last_insert_rowid();
        let after = get(c, saved_id)?.unwrap();
        record(c, saved_id, "add", None, Some(&after), actor, batch)?;
        if let Some(old) = &before {
            c.execute("UPDATE memory_items SET superseded_by = ?2, archived = 1, valid_to = ?3, updated_at = ?3 WHERE id = ?1", params![old.id, saved_id, now])?;
            record(
                c,
                old.id,
                "supersede",
                Some(old),
                get(c, old.id)?.as_ref(),
                actor,
                batch,
            )?;
        }
    }
    let history_id: i64 = c.query_row("SELECT MAX(id) FROM memory_history", [], |r| r.get(0))?;
    Ok(
        json!({"item": get(c, saved_id)?, "changed": true, "history_id": history_id, "similar_to": hint, "hint": hint.map(|i| format!("Similar to #{i} — update instead?"))}),
    )
}

pub fn forget(c: &Connection, id: i64, actor: Actor, batch: &str) -> rusqlite::Result<Value> {
    let old = get(c, id)?.ok_or_else(|| invalid("I couldn't find that memory."))?;
    if actor == Actor::Agent && old.pinned {
        return Err(invalid(
            "I keep pinned notes always in mind. You can forget this one in Memory.",
        ));
    }
    // Do not silently unlink a superseded row's replacement chain.
    if old.superseded_by.is_some() {
        return Err(invalid(
            "That note was replaced. Please forget its current replacement.",
        ));
    }
    let predecessors: Vec<Item> = c
        .prepare(&format!(
            "SELECT {COLUMNS} FROM memory_items WHERE superseded_by = ?1"
        ))?
        .query_map([id], from_row)?
        .collect::<rusqlite::Result<_>>()?;
    for p in predecessors {
        c.execute("UPDATE memory_items SET superseded_by = NULL, archived = 1, updated_at = ?2 WHERE id = ?1", params![p.id, now_ms()])?;
        record(
            c,
            p.id,
            "archive",
            Some(&p),
            get(c, p.id)?.as_ref(),
            actor,
            batch,
        )?;
    }
    c.execute("DELETE FROM memory_items WHERE id = ?1", [id])?;
    record(c, id, "delete", Some(&old), None, actor, batch)?;
    Ok(json!({"forgotten": id, "history_id": c.last_insert_rowid()}))
}

fn restore_snapshot(c: &Connection, old: &Item) -> rusqlite::Result<()> {
    if !old.archived
        && old.superseded_by.is_none()
        && super::live_by_hash(c, &old.content_hash)?.is_some_and(|i| i.id != old.id)
    {
        return Err(invalid(
            "Another note has this text. Please edit it instead.",
        ));
    }
    c.execute("INSERT INTO memory_items(id,kind,topic,text,importance,pinned,created_at,observed_at,mentioned_at,valid_to,superseded_by,source,last_used_at,use_count,content_hash,archived,updated_at)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
        ON CONFLICT(id) DO UPDATE SET kind=excluded.kind,topic=excluded.topic,text=excluded.text,importance=excluded.importance,pinned=excluded.pinned,observed_at=excluded.observed_at,mentioned_at=excluded.mentioned_at,valid_to=excluded.valid_to,superseded_by=excluded.superseded_by,source=excluded.source,content_hash=excluded.content_hash,archived=excluded.archived,updated_at=excluded.updated_at",
            params![old.id,old.kind,old.topic.name(),old.text,old.importance,old.pinned,old.created_at,old.observed_at,old.mentioned_at,old.valid_to,old.superseded_by,old.source.to_string(),old.last_used_at,old.use_count,old.content_hash,old.archived,now_ms()])?;
    Ok(())
}

pub fn undo(c: &Connection, history_id: i64, batch: &str) -> rusqlite::Result<Value> {
    type UndoRow = (i64, String, Option<String>, Option<String>, String);
    let row: Option<UndoRow> = c.query_row("SELECT item_id, op, before, after, COALESCE(batch_id, '') FROM memory_history WHERE id = ?1", [history_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
    let (id, op, before, _after, prior_batch) =
        row.ok_or_else(|| invalid("I couldn't find that change."))?;
    let latest: i64 = c.query_row(
        "SELECT MAX(id) FROM memory_history WHERE item_id = ?1",
        [id],
        |r| r.get(0),
    )?;
    if latest != history_id {
        return Err(invalid(
            "That note changed again. Please undo its newest change first.",
        ));
    }
    if op == "add"
        && c.prepare("SELECT 1 FROM memory_items WHERE superseded_by = ?1")?
            .exists([id])?
    {
        return Err(invalid(
            "This note corrected an earlier version. Please undo the correction instead.",
        ));
    }
    let current = get(c, id)?;
    if let Some(snapshot) = before {
        let old: Item = serde_json::from_str(&snapshot)
            .map_err(|_| invalid("I couldn't restore that change."))?;
        // Reverse the associated replacement as well as its predecessor.
        if let Some(replacement) = current.as_ref().and_then(|i| i.superseded_by)
            && Some(replacement) != old.superseded_by
        {
            let newest: Option<(String, String)> = c.query_row("SELECT op, COALESCE(batch_id, '') FROM memory_history WHERE item_id = ?1 ORDER BY id DESC LIMIT 1", [replacement], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
            if newest.is_none_or(|(op, batch)| {
                !matches!(op.as_str(), "add" | "restore") || batch != prior_batch
            }) {
                return Err(invalid(
                    "The replacement changed again. Please undo its newest change first.",
                ));
            }
            forget(c, replacement, Actor::Owner, batch)?;
        }
        if let Some(replacement) = old.superseded_by
            && get(c, replacement)?.is_none()
        {
            let deleted: Option<(String, String, Option<String>)> = c.query_row("SELECT op, COALESCE(batch_id, ''), before FROM memory_history WHERE item_id = ?1 ORDER BY id DESC LIMIT 1", [replacement], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let Some((op, prior, Some(snapshot))) = deleted else {
                return Err(invalid("I couldn't restore the replacement."));
            };
            if op != "delete" || prior != prior_batch {
                return Err(invalid(
                    "The replacement changed again. Please undo its newest change first.",
                ));
            }
            let item: Item = serde_json::from_str(&snapshot)
                .map_err(|_| invalid("I couldn't restore the replacement."))?;
            restore_snapshot(c, &item)?;
            record(
                c,
                replacement,
                "restore",
                None,
                get(c, replacement)?.as_ref(),
                Actor::Owner,
                batch,
            )?;
        }
        restore_snapshot(c, &old)?;
        if op == "delete" {
            let predecessors: Vec<(i64, i64, String)> = c.prepare("SELECT id, item_id, before FROM memory_history WHERE batch_id = ?1 AND op = 'archive' AND json_extract(before, '$.superseded_by') = ?2")?.query_map(params![prior_batch, id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            for (change, predecessor, snap) in predecessors {
                let latest: i64 = c.query_row(
                    "SELECT MAX(id) FROM memory_history WHERE item_id = ?1",
                    [predecessor],
                    |r| r.get(0),
                )?;
                if change != latest {
                    return Err(invalid(
                        "An earlier version changed again. Please undo its newest change first.",
                    ));
                }
                let prior: Item = serde_json::from_str(&snap)
                    .map_err(|_| invalid("I couldn't restore the earlier version."))?;
                let current = get(c, predecessor)?;
                c.execute("UPDATE memory_items SET superseded_by = ?2, archived = ?3, updated_at = ?4 WHERE id = ?1", params![predecessor, id, prior.archived, now_ms()])?;
                record(
                    c,
                    predecessor,
                    "restore",
                    current.as_ref(),
                    get(c, predecessor)?.as_ref(),
                    Actor::Owner,
                    batch,
                )?;
            }
        }
        record(
            c,
            id,
            "restore",
            current.as_ref(),
            get(c, id)?.as_ref(),
            Actor::Owner,
            batch,
        )?;
    } else {
        forget(c, id, Actor::Owner, batch)?;
    }
    Ok(json!({"undone": history_id, "history_id": c.last_insert_rowid(), "item": get(c, id)?}))
}

/// Run a mutation and render its export under the same database lock/transaction.
pub fn mutate(
    c: &Connection,
    paths: &Paths,
    action: impl FnOnce(&Connection) -> rusqlite::Result<Value>,
) -> rusqlite::Result<Value> {
    let owns_transaction = c.is_autocommit();
    let out = crate::db::write_tx(c, |c| {
        let out = action(c)?;
        super::files::render_export(c, paths)?;
        Ok(out)
    });
    if owns_transaction && out.is_err() {
        // Repair any partially written files after the database rolls back.
        let _ = crate::db::write_tx(c, |c| super::files::render_export(c, paths));
    }
    out
}

/// Durable changes a turn made. IDs allow the UI to edit or undo each change.
pub fn noted(c: &Connection, batch: &str) -> rusqlite::Result<Vec<Value>> {
    type ChangeRow = (i64, i64, String, Option<String>, Option<String>);
    let rows: Vec<ChangeRow> = c.prepare("SELECT id, item_id, op, before, after FROM memory_history WHERE batch_id = ?1 AND actor = 'agent' ORDER BY id LIMIT 200")?.query_map([batch], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<rusqlite::Result<_>>()?;
    let parse =
        |s: Option<&str>| serde_json::from_str::<Value>(s.unwrap_or("null")).unwrap_or(Value::Null);
    let mut out = Vec::new();
    for (change, original_id, op, before, after) in &rows {
        let mut id = *original_id;
        let b = parse(before.as_deref());
        let a = parse(after.as_deref());
        if op == "add"
            && rows
                .iter()
                .any(|r| r.2 == "supersede" && parse(r.4.as_deref())["superseded_by"] == id)
        {
            continue;
        }
        if op == "archive"
            && b["superseded_by"]
                .as_i64()
                .is_some_and(|p| rows.iter().any(|r| r.1 == p && r.2 == "delete"))
        {
            continue;
        }
        let mut snap = if after.is_some() { a } else { b };
        if op == "supersede"
            && let Some(replacement) = snap["superseded_by"].as_i64()
        {
            id = replacement;
            if let Some(r) = rows.iter().find(|r| r.1 == replacement && r.2 == "add") {
                snap = parse(r.4.as_deref());
            }
        }
        out.push(json!({"history_id":change,"item_id":id,"op":op,"text":truncate_chars(snap["text"].as_str().unwrap_or(""),240)}));
        if out.len() == 100 {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, Db, write_tx};
    fn edit(text: &str) -> Edit {
        Edit {
            text: Some(text.into()),
            kind: Some("fact".into()),
            ..Default::default()
        }
    }
    fn save_note(c: &Connection, text: &str, batch: &str) -> rusqlite::Result<Value> {
        save(
            c,
            &edit(text),
            None,
            Actor::Agent,
            &json!({"type":"owner"}),
            batch,
        )
    }

    #[test]
    fn failed_export_rolls_back_mutation_and_repairs_core() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = Db::open(&paths.db_file()).unwrap();
        db.with(|c| {
            mutate(c, &paths, |c| save_note(c, "Original note", "a"))?;
            let original = std::fs::read_to_string(paths.memory_dir().join("MEMORY.md")).unwrap();
            std::fs::create_dir(paths.memory_dir().join("people.md")).unwrap();
            let result = mutate(c, &paths, |c| {
                save(
                    c,
                    &Edit {
                        text: Some("Friend climbs".into()),
                        kind: Some("person".into()),
                        ..Default::default()
                    },
                    None,
                    Actor::Owner,
                    &json!({}),
                    "b",
                )
            });
            assert!(result.is_err());
            assert_eq!(super::super::live(c)?.len(), 1);
            assert_eq!(
                std::fs::read_to_string(paths.memory_dir().join("MEMORY.md")).unwrap(),
                original
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn long_notes_and_expired_duplicates_can_be_remembered() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            write_tx(c, |c| {
                let text = "Practice long note paragraph ".repeat(50);
                let long = save_note(c, &text, "long")?;
                assert_eq!(long["changed"], true);
                let expired = save(
                    c,
                    &Edit {
                        text: Some("Likes tea".into()),
                        valid_to: Some(Some(0)),
                        ..Default::default()
                    },
                    None,
                    Actor::Owner,
                    &json!({}),
                    "expired",
                )?;
                let new = save_note(c, "Likes tea", "again")?;
                assert_eq!(new["changed"], true);
                assert_ne!(new["item"]["id"], expired["item"]["id"]);
                assert_eq!(
                    detail(c, new["item"]["id"].as_i64().unwrap(), true)?["item"]["text"],
                    "Likes tea"
                );
                assert!(
                    get(c, expired["item"]["id"].as_i64().unwrap())?
                        .unwrap()
                        .archived
                );
                Ok(())
            })
        })
        .unwrap();
    }

    #[test]
    fn pinned_notes_require_owner_forget_and_search_has_one_fts_cursor() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let note = save(
                c,
                &Edit {
                    text: Some("Likes tea".into()),
                    pinned: Some(true),
                    ..Default::default()
                },
                None,
                Actor::Owner,
                &json!({}),
                "pinned",
            )?;
            let id = note["item"]["id"].as_i64().unwrap();
            assert!(forget(c, id, Actor::Agent, "agent").is_err());
            assert!(get(c, id)?.is_some());
            let plan: Vec<String> = c
                .prepare(&format!("EXPLAIN QUERY PLAN {}", list_sql(true, true)))?
                .query_map(
                    params![
                        now_ms(),
                        Option::<String>::None,
                        Option::<i64>::None,
                        0,
                        "tea",
                        30
                    ],
                    |r| r.get(3),
                )?
                .collect::<rusqlite::Result<_>>()?;
            assert!(!plan.iter().any(|p| p.contains("CORRELATED")), "{plan:?}");
            forget(c, id, Actor::Owner, "owner")?;
            assert!(get(c, id)?.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn duplicate_correction_undo_rolls_back_notes_history_links_and_export() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = Db::open(&paths.db_file()).unwrap();
        db.with(|c| {
            let original = mutate(c,&paths,|c| save_note(c,"Likes tea","a"))?["item"]["id"].as_i64().unwrap();
            let correction = mutate(c,&paths,|c| save(c,&edit("Likes coffee"),Some(original),Actor::Agent,&json!({}),"b"))?;
            mutate(c,&paths,|c| save_note(c,"Likes tea","duplicate"))?;
            let before = json!({"notes":super::super::live(c)?,"old":get(c,original)?,"history":history(c,original)?});
            let export = std::fs::read_to_string(paths.memory_dir().join("MEMORY.md")).unwrap();
            assert!(mutate(c,&paths,|c| undo(c,correction["history_id"].as_i64().unwrap(),"undo")).is_err());
            assert_eq!(json!({"notes":super::super::live(c)?,"old":get(c,original)?,"history":history(c,original)?}),before);
            assert_eq!(std::fs::read_to_string(paths.memory_dir().join("MEMORY.md")).unwrap(),export);
            Ok(())
        }).unwrap();
    }

    #[test]
    fn stale_core_and_topic_writes_do_not_resurrect_forget_or_owner_edits() {
        for path in ["MEMORY.md", "you.md"] {
            for edit_owner in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let paths = Paths {
                    config_file: tmp.path().join("config.toml"),
                    data_dir: tmp.path().join("data"),
                };
                crate::persona::seed(&paths).unwrap();
                let db = Db::open(&paths.db_file()).unwrap();
                let id = db
                    .with(|c| mutate(c, &paths, |c| save_note(c, "Likes tea", "a")))
                    .unwrap()["item"]["id"]
                    .as_i64()
                    .unwrap();
                let stale = std::fs::read_to_string(paths.memory_dir().join(path)).unwrap();
                let started = now_ms();
                db.with(|c| {
                    mutate(c, &paths, |c| {
                        if edit_owner {
                            save(
                                c,
                                &edit("Likes coffee"),
                                Some(id),
                                Actor::Owner,
                                &json!({}),
                                "owner-edit",
                            )
                        } else {
                            forget(c, id, Actor::Owner, "owner-forget")
                        }
                    })
                })
                .unwrap();
                let written = super::super::files::write(
                    &db,
                    &paths,
                    path,
                    &stale,
                    &json!({"type":"owner"}),
                    started,
                    "stale-turn",
                )
                .unwrap();
                assert_eq!(
                    written.not_restored,
                    ["Likes tea"],
                    "{path} edit={edit_owner}"
                );
                let notes = db.with(super::super::live).unwrap();
                assert!(notes.iter().all(|n| n.text != "Likes tea"));
                assert_eq!(notes.len(), usize::from(edit_owner));
            }
        }
    }

    #[test]
    fn recall_dedup_similarity_use_history_and_literal_queries() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            write_tx(c, |c| {
                let a = save_note(
                    c,
                    "Walks the dog every morning <private>synthetic-canary</private>",
                    "turn-one",
                )?;
                let id = a["item"]["id"].as_i64().unwrap();
                let same = save_note(c, "WALKS the dog every morning.", "turn-two")?;
                assert_eq!(same["duplicate"], true);
                assert_eq!(history(c, id)?.len(), 1);
                let near = save_note(c, "Walks the dog every evening", "turn-two")?;
                assert_eq!(near["similar_to"], id);
                assert_eq!(super::super::live(c)?.len(), 2);
                let found = list(
                    c,
                    &Filter {
                        query: Some("walking morning' OR (".into()),
                        ..Default::default()
                    },
                    true,
                )?;
                assert_eq!(found["hits"].as_array().unwrap().len(), 2);
                assert!(found.to_string().find("synthetic-canary").is_none());
                let full = detail(c, id, true)?;
                assert_eq!(full["item"]["use_count"], 2);
                assert_eq!(full["history"][0]["actor"], "agent");
                assert_eq!(noted(c, "turn-one")?.len(), 1);
                assert!(query("***").is_err());
                assert!(save_note(c, "password: synthetic-canary", "secret").is_err());
                assert_eq!(super::super::live(c)?.len(), 2);
                Ok(())
            })
        })
        .unwrap();
    }

    #[test]
    fn correction_supersedes_undo_is_safe_and_deleted_ids_are_not_reused() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            write_tx(c, |c| {
                let a = save_note(c, "Prefers tea", "a")?;
                let id = a["item"]["id"].as_i64().unwrap();
                let b = save(
                    c,
                    &edit("Prefers coffee"),
                    Some(id),
                    Actor::Agent,
                    &json!({"type":"owner"}),
                    "b",
                )?;
                let replacement = b["item"]["id"].as_i64().unwrap();
                let change = b["history_id"].as_i64().unwrap();
                assert_eq!(get(c, id)?.unwrap().superseded_by, Some(replacement));
                assert!(detail(c, id, true).is_err());
                assert_eq!(undo(c, change, "u")?["item"]["text"], "Prefers tea");
                assert!(get(c, replacement)?.is_none());
                assert!(undo(c, change, "u2").is_err());
                let deleted = forget(c, id, Actor::Owner, "f")?["history_id"]
                    .as_i64()
                    .unwrap();
                let later = save_note(c, "Prefers water", "n")?["item"]["id"]
                    .as_i64()
                    .unwrap();
                assert!(later > replacement);
                assert_eq!(undo(c, deleted, "restore")?["item"]["id"], id);
                let changed = save(
                    c,
                    &Edit {
                        pinned: Some(true),
                        ..Default::default()
                    },
                    Some(id),
                    Actor::Owner,
                    &json!({"type":"owner"}),
                    "pin",
                )?;
                assert_eq!(changed["item"]["pinned"], true);
                let pin = changed["history_id"].as_i64().unwrap();
                save(
                    c,
                    &edit("Prefers cocoa"),
                    Some(id),
                    Actor::Owner,
                    &json!({"type":"owner"}),
                    "new",
                )?;
                assert!(undo(c, pin, "stale").is_err());
                assert_eq!(get(c, id)?.unwrap().text, "Prefers cocoa");
                Ok(())
            })
        })
        .unwrap();
    }

    #[test]
    fn forgetting_a_correction_keeps_prior_notes_archived_and_undo_restores_links() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            write_tx(c, |c| {
                let id = save_note(c, "Likes tea", "turn-one")?["item"]["id"]
                    .as_i64()
                    .unwrap();
                let correction = save(
                    c,
                    &edit("Likes coffee"),
                    Some(id),
                    Actor::Agent,
                    &json!({}),
                    "turn-two",
                )?;
                let new = correction["item"]["id"].as_i64().unwrap();
                let note = noted(c, "turn-two")?;
                assert_eq!(note.len(), 1);
                assert_eq!(note[0]["item_id"], new);
                assert_eq!(note[0]["text"], "Likes coffee");
                assert_eq!(note[0]["history_id"], correction["history_id"]);
                assert_eq!(detail(c, new, false)?["history"][0]["op"], "supersede");
                let add = c.query_row(
                    "SELECT MAX(id) FROM memory_history WHERE item_id=?1",
                    [new],
                    |r| r.get::<_, i64>(0),
                )?;
                assert!(undo(c, add, "wrong-undo").is_err());
                let deleted = forget(c, new, Actor::Agent, "turn-three")?["history_id"]
                    .as_i64()
                    .unwrap();
                assert!(super::super::live(c)?.is_empty());
                assert_eq!(noted(c, "turn-three")?.len(), 1);
                assert!(get(c, id)?.unwrap().archived);
                undo(c, deleted, "undo-delete")?;
                assert_eq!(get(c, id)?.unwrap().superseded_by, Some(new));
                assert_eq!(super::super::live(c)?.len(), 1);
                Ok(())
            })
        })
        .unwrap();
    }

    #[test]
    fn edits_preserve_imported_topic_and_source() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let src = json!({"type":"import","file":"people.md"});
            super::super::add(
                c,
                &super::super::NewItem {
                    kind: "fact",
                    topic: Topic::People,
                    text: "Friend likes hiking",
                    importance: 3,
                    source: &src,
                },
                Actor::Import,
                "import",
            )?;
            let old = super::super::live(c)?.pop().unwrap();
            let saved = save(
                c,
                &Edit {
                    pinned: Some(true),
                    ..Default::default()
                },
                Some(old.id),
                Actor::Owner,
                &json!({"type":"owner"}),
                "pin",
            )?;
            assert_eq!(saved["item"]["topic"], "people");
            assert_eq!(saved["item"]["source"], src);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn list_filters_expiry_topic_recent_changes_and_paginates_pinned_first() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let a = save_note(c, "Likes tea", "a")?["item"]["id"]
                .as_i64()
                .unwrap();
            let b = save_note(c, "Likes coffee", "b")?["item"]["id"]
                .as_i64()
                .unwrap();
            save(
                c,
                &Edit {
                    pinned: Some(true),
                    ..Default::default()
                },
                Some(a),
                Actor::Owner,
                &json!({}),
                "pin",
            )?;
            save(
                c,
                &Edit {
                    text: Some("Visits the park".into()),
                    kind: Some("routine".into()),
                    valid_to: Some(Some(0)),
                    ..Default::default()
                },
                None,
                Actor::Owner,
                &json!({}),
                "expired",
            )?;
            let page = list(
                c,
                &Filter {
                    limit: Some(1),
                    ..Default::default()
                },
                false,
            )?;
            assert_eq!(page["items"][0]["id"], a);
            assert_eq!(page["has_more"], true);
            let page = list(
                c,
                &Filter {
                    limit: Some(1),
                    offset: Some(1),
                    ..Default::default()
                },
                false,
            )?;
            assert_eq!(page["items"][0]["id"], b);
            assert_eq!(page["has_more"], false);
            assert!(
                list(
                    c,
                    &Filter {
                        topic: Some("routines".into()),
                        ..Default::default()
                    },
                    false
                )?["items"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert!(
                list(
                    c,
                    &Filter {
                        since: Some(i64::MAX),
                        ..Default::default()
                    },
                    false
                )?["items"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(get(c, a)?.unwrap().use_count, 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn replacement_edit_blocks_undo() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let id = save_note(c, "Old preference", "a")?["item"]["id"]
                .as_i64()
                .unwrap();
            let b = save(
                c,
                &edit("New preference"),
                Some(id),
                Actor::Agent,
                &json!({}),
                "b",
            )?;
            let rid = b["item"]["id"].as_i64().unwrap();
            save(
                c,
                &edit("Edited replacement"),
                Some(rid),
                Actor::Owner,
                &json!({}),
                "c",
            )?;
            assert!(write_tx(c, |c| undo(c, b["history_id"].as_i64().unwrap(), "u")).is_err());
            assert_eq!(get(c, id)?.unwrap().superseded_by, Some(rid));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn conversation_index_tracks_completion_and_purges_shared_and_deleted_chats() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = db::owner_user_id(c)?.unwrap();
            let conv = db::create_conversation(c, &owner, "side", "Practice trip", None)?.id;
            let msg = db::insert_message(c, &db::NewMessage { conversation_id: &conv, role:"assistant", content:"Practice orchard", status:"streaming", ..Default::default() })?;
            assert!(conversations(c, "orchard")?["hits"].as_array().unwrap().is_empty());
            db::finish_message(c, &msg.id, "Practice orchard", "complete", &json!({}))?;
            assert_eq!(conversations(c, "orchard")?["hits"][0]["message_id"], msg.id);
            c.execute("INSERT INTO users(id,display_name,created_at) VALUES ('guest','Guest',0)", [])?;
            let shared = db::create_conversation(c, &owner, "group", "Shared", None)?.id;
            c.execute("INSERT INTO conversation_members(conversation_id,user_id,joined_at) VALUES (?1,'guest',0)", [&shared])?;
            for chat in [&shared, &db::ensure_job_conversation(c, &owner, "heartbeat")?] {
                db::insert_message(c, &db::NewMessage { conversation_id: chat, role:"assistant", content:"Private orchard", ..Default::default() })?;
            }
            assert_eq!(conversations(c, "orchard")?["hits"].as_array().unwrap().len(), 1);
            c.execute("INSERT INTO conversation_members(conversation_id,user_id,joined_at) VALUES (?1,'guest',0)", [&conv])?;
            assert!(conversations(c, "orchard")?["hits"].as_array().unwrap().is_empty());
            let count: i64 = c.query_row("SELECT COUNT(*) FROM conversation_messages_fts", [], |r| r.get(0))?;
            assert_eq!(count, 0);
            c.execute("DELETE FROM conversation_members WHERE user_id='guest' AND conversation_id=?1", [&conv])?;
            db::insert_message(c, &db::NewMessage { conversation_id: &conv, role:"assistant", content:"Private orchard", ..Default::default() })?;
            assert!(conversations(c, "orchard")?["hits"].as_array().unwrap().is_empty());
            let new = db::create_conversation(c, &owner, "side", "New", None)?.id;
            db::insert_message(c, &db::NewMessage { conversation_id: &new, role:"user", author_user_id: Some(&owner), content:"Orchard", ..Default::default() })?;
            assert_eq!(conversations(c, "orchard")?["hits"].as_array().unwrap().len(), 1);
            c.execute("DELETE FROM conversations WHERE id=?1", [&new])?;
            assert!(conversations(c, "orchard")?["hits"].as_array().unwrap().is_empty());
            Ok(())
        }).unwrap();
    }
}
