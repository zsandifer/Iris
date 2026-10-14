//! The always-in-prompt core: which notes `MEMORY.md` holds.
//!
//! The core is small and curated: pinned notes, profile and preference
//! notes (who the owner is), notes of importance 4 or more (what the agent
//! writes to `MEMORY.md`), and notes whose score shows heavy recent use.
//! They are packed whole under a character budget in three tiers: pinned,
//! then profile and preference, then the rest by score. A note that does
//! not fit is skipped and smaller ones after it may still fit. Notes past
//! their `valid_to` are left out. The result depends only on the notes and
//! the time, so the same notes always pack the same way. Every other note
//! is in its topic file, where the agent reads it when it comes up.
//!
//! The score is plain arithmetic in the spirit of Generative Agents (Park
//! et al., 2023): recency + importance + use, each scaled to 0..1 and
//! weighted equally. There is no relevance term: the core is packed once
//! per change, not per message, and recall by topic is the agent's job.
//!
//! Which notes are in the core is stored (`memory_items.core_since`) when
//! the export is written, so a `MEMORY.md` write acts on exactly the notes
//! the file shows and can tell a note that joined it after a turn began.

use super::markdown::{self, LeftOut};
use super::{CORE_KINDS, Item};
use crate::util::sha256_hex;
use rusqlite::{Connection, params};
use std::collections::HashSet;

/// Characters the core's notes and section headings may take. The file's
/// title and the left-out counts add a few dozen more.
pub const BUDGET_CHARS: usize = 11_000;
/// Recency halves every 30 days since a note was saved or last used.
const RECENCY_HALF_LIFE_MS: f64 = 30.0 * 24.0 * 3_600_000.0;
/// Uses at which the use term reaches its maximum (log scale).
const USE_SATURATION: f64 = 20.0;
/// Importance that keeps a note in mind: notes written to `MEMORY.md` (or
/// imported from it) get it.
pub const CORE_IMPORTANCE: i64 = 4;
/// Score at which any note joins the core: an importance 3 note used about
/// four times lately, or an importance 2 note used about ten times.
const PROMOTE_SCORE: f64 = 2.0;

/// A note's score at `now` (ms): recency + importance + use, each 0..1.
///
/// - recency: `0.5 ^ (age / 30 days)`, age since it was saved or last used;
/// - importance: `(importance - 1) / 4`;
/// - use: `ln(1 + use_count) / ln(21)`, capped at 1.
pub fn score(item: &Item, now: i64) -> f64 {
    let last = item
        .last_used_at
        .map_or(item.created_at, |u| u.max(item.created_at));
    let age = now.saturating_sub(last).max(0) as f64;
    let recency = 0.5_f64.powf(age / RECENCY_HALF_LIFE_MS);
    let importance = (item.importance.clamp(1, 5) - 1) as f64 / 4.0;
    let used = ((item.use_count.max(0) as f64).ln_1p() / USE_SATURATION.ln_1p()).min(1.0);
    recency + importance + used
}

/// Packing tier: pinned, then who the owner is, then the rest.
fn tier(item: &Item) -> u8 {
    if item.pinned {
        0
    } else if CORE_KINDS.contains(&item.kind.as_str()) {
        1
    } else {
        2
    }
}

/// Whether a note still holds at `now`.
fn current(item: &Item, now: i64) -> bool {
    item.valid_to.is_none_or(|t| t > now)
}

/// Whether a note with this `score` belongs in the core when it fits.
fn candidate(item: &Item, score: f64) -> bool {
    tier(item) < 2 || item.importance >= CORE_IMPORTANCE || score >= PROMOTE_SCORE
}

/// The packed core.
#[derive(Debug)]
pub struct Packed<'a> {
    /// The notes in the core, in packing order.
    pub items: Vec<&'a Item>,
    pub left_out: LeftOut,
}

/// Pack `live` notes (not archived, not superseded) at `now` (ms).
pub fn pack(live: &[Item], now: i64) -> Packed<'_> {
    let mut ranked: Vec<(u8, f64, &Item)> = live
        .iter()
        .filter(|i| current(i, now))
        .map(|i| (tier(i), score(i, now), i))
        .filter(|(_, score, i)| candidate(i, *score))
        .collect();
    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1.total_cmp(&a.1))
            .then(a.2.id.cmp(&b.2.id))
    });
    let mut used = 0;
    let mut headings: Vec<&str> = Vec::new();
    let mut items = Vec::new();
    let mut left_out = LeftOut::default();
    for (_, _, item) in ranked {
        let heading = markdown::core_heading(item);
        let new_heading = !headings.contains(&heading);
        let cost = markdown::bullet_chars(&item.text)
            + if new_heading {
                markdown::heading_chars(heading)
            } else {
                0
            };
        if used + cost <= BUDGET_CHARS {
            used += cost;
            if new_heading {
                headings.push(heading);
            }
            items.push(item);
        } else if item.pinned {
            left_out.pinned += 1;
        }
    }
    left_out.other = live.len() - items.len() - left_out.pinned;
    Packed { items, left_out }
}

/// Store which notes are in the core: `core_since` is set to `now` for a
/// note that just joined it and cleared for one that left it (or was set
/// aside). Call in the transaction that writes the export.
pub fn record(conn: &Connection, live: &[Item], core: &[&Item], now: i64) -> rusqlite::Result<()> {
    let ids: HashSet<i64> = core.iter().map(|i| i.id).collect();
    let mut set = conn.prepare_cached("UPDATE memory_items SET core_since = ?2 WHERE id = ?1")?;
    for item in live {
        match (ids.contains(&item.id), item.core_since) {
            (true, None) => set.execute(params![item.id, now])?,
            (false, Some(_)) => set.execute(params![item.id, None::<i64>])?,
            _ => 0,
        };
    }
    conn.execute(
        "UPDATE memory_items SET core_since = NULL
         WHERE core_since IS NOT NULL AND (archived = 1 OR superseded_by IS NOT NULL)",
        [],
    )?;
    Ok(())
}

/// Hash of the part of the core that says who the owner is: its pinned,
/// profile and preference notes. Part of the persona hash, so a CLI that
/// cannot take a new system prompt in a resumed session starts a fresh one
/// when this changes, and not for routine notes.
pub fn identity(conn: &Connection) -> rusqlite::Result<String> {
    let kinds = CORE_KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT id, pinned, text FROM memory_items
         WHERE core_since IS NOT NULL AND archived = 0 AND superseded_by IS NULL
           AND (pinned = 1 OR kind IN ({kinds}))
         ORDER BY id"
    ))?;
    let mut acc = String::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, bool>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (id, pinned, text) = row?;
        acc.push_str(&format!("{id}\0{}\0{text}\0", u8::from(pinned)));
    }
    Ok(sha256_hex(&acc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Topic;
    use crate::memory::tests::item;

    const DAY: i64 = 24 * 3_600_000;
    const NOW: i64 = 1_000 * DAY;

    fn note(id: i64, text: &str) -> Item {
        let mut i = item(id, text, Topic::You);
        i.created_at = NOW;
        i
    }

    fn ids(p: &Packed) -> Vec<i64> {
        p.items.iter().map(|i| i.id).collect()
    }

    fn rendered_body_chars(p: &Packed) -> usize {
        // What the notes and headings add after the file's title.
        let mut sorted = p.items.clone();
        sorted.sort_by_key(|i| i.id);
        let full = markdown::render_core(&sorted, LeftOut::default());
        let base = markdown::render_core(&[], LeftOut::default());
        let title = base.find("\n<!--").unwrap();
        full.chars().count() - base[..title].chars().count()
    }

    #[test]
    fn score_adds_recency_importance_and_use() {
        let mut n = note(1, "x");
        n.importance = 3;
        assert!((score(&n, NOW) - 1.5).abs() < 1e-9);
        // Recency halves in 30 days and counts from the last use.
        assert!((score(&n, NOW + 30 * DAY) - 1.0).abs() < 1e-9);
        n.last_used_at = Some(NOW + 30 * DAY);
        assert!((score(&n, NOW + 30 * DAY) - 1.5).abs() < 1e-9);
        // A use time before the note existed counts from when it was saved.
        n.last_used_at = Some(0);
        assert!((score(&n, NOW + 30 * DAY) - 1.0).abs() < 1e-9);
        n.importance = 5;
        n.use_count = 20;
        assert!((score(&n, NOW) - 3.0).abs() < 1e-9);
        n.use_count = 10_000;
        assert!((score(&n, NOW) - 3.0).abs() < 1e-9, "use is capped");
        n.importance = 1;
        n.use_count = 0;
        assert!(score(&n, NOW + 10_000 * DAY) < 1e-9);
        // A clock behind the note's time does not inflate recency.
        assert!((score(&n, NOW - DAY) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn tiers_come_before_score() {
        let mut live: Vec<Item> = (1..=6).map(|i| note(i, &format!("note {i}"))).collect();
        live[0].importance = 1;
        live[0].created_at = 0;
        live[0].pinned = true;
        live[1].kind = "preference".into();
        live[1].importance = 1;
        live[1].created_at = 0;
        live[2].kind = "profile".into();
        live[2].importance = 2;
        live[3].importance = 5;
        live[4].importance = 4;
        live[4].use_count = 1;
        live[5].importance = 4;
        live[5].created_at = NOW - DAY;
        let p = pack(&live, NOW);
        assert_eq!(ids(&p), [1, 3, 2, 4, 5, 6]);
        assert_eq!(p.left_out, LeftOut::default());
        // Equal scores fall back to the id, whatever the input order.
        live[4].use_count = 0;
        live[5].created_at = NOW;
        live.reverse();
        assert_eq!(ids(&pack(&live, NOW)), [1, 3, 2, 4, 5, 6]);
    }

    #[test]
    fn ordinary_notes_join_only_when_used_a_lot() {
        let mut live = vec![note(1, "Has a bike"), note(2, "Cousin in Leeds")];
        live[1].importance = 2;
        let p = pack(&live, NOW);
        assert!(p.items.is_empty());
        assert_eq!(p.left_out.other, 2);
        live[0].use_count = 4;
        assert_eq!(ids(&pack(&live, NOW)), [1]);
        // Use that is not recent fades.
        assert!(pack(&live, NOW + 60 * DAY).items.is_empty());
        live[0].last_used_at = Some(NOW + 60 * DAY);
        live[1].use_count = 10;
        live[1].last_used_at = Some(NOW + 60 * DAY);
        // Higher score first.
        assert_eq!(ids(&pack(&live, NOW + 60 * DAY)), [2, 1]);
    }

    #[test]
    fn budget_is_respected_with_whole_notes() {
        let mut live = Vec::new();
        for i in 1..=40 {
            let mut n = note(i, &format!("{i} {}", "x".repeat(900)));
            n.importance = if i % 2 == 0 { 4 } else { 3 };
            n.topic = Topic::ALL[(i % 4) as usize];
            live.push(n);
        }
        let mut short = note(41, "short");
        short.importance = 4;
        live.push(short);
        live[39].pinned = true;
        let p = pack(&live, NOW);
        assert_eq!(p.items[0].id, 40, "pinned first");
        assert!(rendered_body_chars(&p) <= BUDGET_CHARS);
        // Importance 3 notes stay in their topic files; the short note
        // still fits after the big ones stop fitting.
        assert!(p.items[1..12].iter().all(|i| i.importance == 4));
        assert_eq!(p.items.last().unwrap().id, 41);
        assert_eq!(p.items.len(), 13);
        assert_eq!(
            p.left_out,
            LeftOut {
                pinned: 0,
                other: 28
            }
        );
        // The same notes at the same time always pack the same way.
        assert_eq!(ids(&p), ids(&pack(&live, NOW)));
        let mut reversed = live.clone();
        reversed.reverse();
        assert_eq!(ids(&p), ids(&pack(&reversed, NOW)));
    }

    #[test]
    fn pinned_notes_that_do_not_fit_are_counted() {
        let mut live: Vec<Item> = (1..=15)
            .map(|i| {
                let mut n = note(i, &format!("{i} {}", "p".repeat(1_000)));
                n.pinned = true;
                n
            })
            .collect();
        live.push(note(16, "small pinned"));
        live[15].pinned = true;
        let mut big = note(17, &"o".repeat(1_000));
        big.importance = 5;
        live.push(big);
        let p = pack(&live, NOW);
        assert!(p.items.iter().all(|i| i.pinned));
        assert!(rendered_body_chars(&p) <= BUDGET_CHARS);
        assert_eq!(p.items.len(), 11);
        assert_eq!(
            p.left_out,
            LeftOut {
                pinned: 5,
                other: 1
            }
        );
        // A single note larger than the budget never fits.
        let mut huge = [note(1, &"h".repeat(BUDGET_CHARS + 1))];
        huge[0].pinned = true;
        let p = pack(&huge, NOW);
        assert!(p.items.is_empty());
        assert_eq!(p.left_out.pinned, 1);
    }

    #[test]
    fn expired_notes_are_left_out() {
        let mut live = vec![note(1, "Staying in Lisbon this week"), note(2, "Likes tea")];
        live[0].valid_to = Some(NOW);
        live[0].pinned = true;
        live[1].kind = "preference".into();
        let p = pack(&live, NOW);
        assert_eq!(ids(&p), [2]);
        assert_eq!(
            p.left_out,
            LeftOut {
                pinned: 0,
                other: 1
            }
        );
        assert_eq!(ids(&pack(&live, NOW - 1)), [1, 2]);
    }
}
