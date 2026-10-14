//! Deterministic forgetting and owner control. No CLI or model calls.
use super::{Actor, COLUMNS, CORE_KINDS, files, from_row, packing, recall};
use crate::config::Paths;
use crate::db::{self, write_tx};
use crate::util::{new_id, now_ms};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

pub const RESET_AFTER: &str = "memory.reset_after";
const SCAN_CURSOR: &str = "memory.forgetting.cursor";
const DAY: i64 = 24 * 3_600_000;
const SCAN_LIMIT: i64 = 100;
const EXPIRY_LIMIT: i64 = 50;
// Force the selective index: the supersession index is deliberately broad
// for FK deletion and can otherwise win SQLite's planner without ANALYZE.
fn expiry_sql() -> String {
    format!(
        "SELECT {COLUMNS} FROM memory_items INDEXED BY memory_expiry
        WHERE archived = 0 AND superseded_by IS NULL AND valid_to IS NOT NULL
        AND valid_to <= ?1 ORDER BY valid_to, id LIMIT ?2"
    )
}

/// A reset invalidates writes from turns that already had the old memory.
pub fn write_allowed(c: &Connection, started: i64) -> rusqlite::Result<()> {
    let after = db::get_setting(c, RESET_AFTER)?
        .map(|v| {
            v.parse::<i64>()
                .map_err(|_| recall::invalid("I couldn't read my memory reset progress."))
        })
        .transpose()?;
    if after.is_some_and(|after| started <= after) {
        return Err(recall::invalid(
            "I've cleared my memory. Please start a new message before asking me to save a note.",
        ));
    }
    Ok(())
}

/// Inspect bounded pages; expiration gets its own indexed page so a large
/// store cannot indefinitely delay expiry behind score-decay candidates.
pub fn maintain(c: &Connection, paths: &Paths, now: i64) -> rusqlite::Result<usize> {
    let out = write_tx(c, |c| {
        let cursor = db::get_setting(c, SCAN_CURSOR)?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let mut expired: Vec<_> = c
            .prepare(&expiry_sql())?
            .query_map(params![now, EXPIRY_LIMIT], from_row)?
            .collect::<rusqlite::Result<_>>()?;
        let page: Vec<_> = c
            .prepare(&format!(
                "SELECT {COLUMNS} FROM memory_items WHERE id > ?1 ORDER BY id LIMIT ?2"
            ))?
            .query_map(params![cursor, SCAN_LIMIT], from_row)?
            .collect::<rusqlite::Result<_>>()?;
        let next = if page.len() < SCAN_LIMIT as usize {
            0
        } else {
            page.last().unwrap().id
        };
        db::set_setting(c, SCAN_CURSOR, &next.to_string())?;
        for item in page {
            let last = item
                .last_used_at
                .unwrap_or(item.created_at)
                .max(item.created_at)
                .max(item.updated_at);
            if !item.archived
                && item.superseded_by.is_none()
                && !item.pinned
                && !CORE_KINDS.contains(&item.kind.as_str())
                && item.importance < 4
                && now.saturating_sub(last) >= 90 * DAY
                && packing::score(&item, now) < 0.6
                && !expired.iter().any(|i| i.id == item.id)
            {
                expired.push(item);
            }
        }
        if expired.is_empty() {
            return Ok(0);
        }
        // With no semantic matching, a global source boundary is the safe
        // way to keep old discussions from re-learning a paraphrased note.
        // Skip unrelated backlog conservatively; fresh assertions still learn.
        super::dream::exclude_prior(c)?;
        let batch = format!("forgetting-{}", new_id());
        for item in &expired {
            super::archive(c, item, Actor::Dream, &batch)?;
        }
        files::render_export(c, paths)?;
        Ok(expired.len())
    });
    if out.is_err() {
        let _ = write_tx(c, |c| files::render_export(c, paths));
    }
    out
}

/// Deliberately irreversible reset of learned memory, not chat transcripts.
/// The evidence boundary survives history deletion and also rejects in-flight Dream.
pub fn forget_everything(c: &Connection, paths: &Paths) -> rusqlite::Result<Value> {
    recall::mutate(c, paths, |c| {
        super::dream::exclude_prior(c)?;
        let after = db::get_setting(c, super::dream::EVIDENCE_AFTER)?.unwrap();
        db::set_setting(c, RESET_AFTER, &after)?;
        let count: i64 = c.query_row("SELECT COUNT(*) FROM memory_items", [], |r| r.get(0))?;
        c.execute("DELETE FROM memory_items", [])?;
        c.execute("DELETE FROM memory_history", [])?;
        db::set_setting(c, SCAN_CURSOR, "0")?;
        // Old cards and Noted chips contain snapshots and undo IDs. Keep the
        // chat messages but remove these derived memory payloads.
        c.execute("UPDATE messages SET meta = json_remove(meta, '$.noted') WHERE json_type(meta,'$.noted') IS NOT NULL", [])?;
        c.execute("UPDATE messages SET card = NULL, kind = 'text', content = 'I cleared my notes.' WHERE json_extract(card,'$.type') = 'memory_morning'", [])?;
        c.execute("DELETE FROM harness_sessions WHERE conversation_id IN (SELECT id FROM conversations WHERE created_by = (SELECT id FROM users WHERE is_owner = 1))", [])?;
        Ok(json!({"forgotten":count}))
    })
}

/// A complete portable Markdown download, including archived and replaced
/// notes. Render from SQLite under its lock; never trust stale disk files.
pub fn export(c: &Connection) -> rusqlite::Result<String> {
    let items = c
        .prepare(&format!(
            "SELECT {COLUMNS} FROM memory_items ORDER BY topic, created_at, id"
        ))?
        .query_map([], from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = String::from("# Iris memory\n\n");
    let now = now_ms();
    for current in [true, false] {
        out.push_str(if current {
            "## Current notes\n\n"
        } else {
            "## Archived, expired and replaced notes\n\n"
        });
        for topic in super::Topic::ALL {
            let notes: Vec<_> = items
                .iter()
                .filter(|i| {
                    i.topic == topic
                        && (!i.archived
                            && i.superseded_by.is_none()
                            && i.valid_to.is_none_or(|t| t > now))
                            == current
                })
                .collect();
            if notes.is_empty() {
                continue;
            }
            out.push_str(&format!("### {}\n\n", topic.title()));
            for item in notes {
                out.push_str(&format!("- {}\n", item.text.replace('\n', "\n  ")));
                out.push_str(&format!("  <!-- id: {}; kind: {}; pinned: {}; observed_at: {:?}; valid_to: {:?}; superseded_by: {:?} -->\n", item.id, item.kind, item.pinned, item.observed_at, item.valid_to, item.superseded_by));
            }
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
