//! The teammate harness: the agent's working notes (its own picture of the
//! domains it looks after) and the scorecard of what it said on its own
//! initiative. Both go into the heartbeat prompt, so a check-in compares
//! the plan with reality instead of running a checklist from scratch.
//!
//! Working notes are not owner memory. Memory is what the agent knows about
//! the owner; a working note is the agent's plan and current state for one
//! domain ("Household budget", "Kitchen remodel"), written by the agent on
//! every check-in and readable (and editable) by the owner in the app.

use crate::db;
use crate::util::{new_id, now_ms, truncate_chars};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

/// Largest note body, in characters.
pub const MAX_NOTE_BODY: usize = 8_000;
pub const MAX_NOTE_TITLE: usize = 120;
pub const MAX_DOMAIN: usize = 60;
/// Most notes one owner can have.
pub const MAX_NOTES: usize = 40;
/// Earlier versions kept per note.
const HISTORY_PER_NOTE: usize = 20;
/// Characters of notes inlined in a chat system prompt (the heartbeat gets
/// them all, up to `HEARTBEAT_NOTES_CHARS`).
pub const PROMPT_NOTES_CHARS: usize = 12_000;
pub const HEARTBEAT_NOTES_CHARS: usize = 40_000;
/// Days the scorecard looks back.
pub const SCORECARD_DAYS: i64 = 14;
/// An owner message in Main this soon after a proactive message counts as a
/// reply to it.
pub const REPLY_WINDOW_MS: i64 = 6 * 60 * 60 * 1000;
/// Held messages older than this are no longer shown to the next run.
pub const HELD_FOR_MS: i64 = 2 * 24 * 60 * 60 * 1000;
/// Characters of a message kept in the scorecard.
const EXCERPT_CHARS: usize = 300;

// ------------------------------------------------------------ working notes

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WorkingNote {
    pub id: String,
    pub domain: String,
    pub title: String,
    pub body: String,
    /// `agent`, `heartbeat`, or `owner`.
    pub updated_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteVersion {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub updated_by: String,
    pub replaced_at: i64,
}

/// The domain key: lowercase words joined by dashes (`household-budget`).
/// `None` when nothing usable is left.
pub fn slug(domain: &str) -> Option<String> {
    let mut out = String::new();
    let mut dash = false;
    for ch in domain.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
        if out.len() >= MAX_DOMAIN {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    (!out.is_empty()).then_some(out)
}

/// A title from a domain the agent gave without one.
fn title_from(domain: &str) -> String {
    let mut words: Vec<String> = domain
        .split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    if let Some(first) = words.first_mut()
        && let Some(c) = first.get_mut(0..1)
    {
        c.make_ascii_uppercase();
    }
    words.join(" ")
}

pub fn check_note(title: Option<&str>, body: &str) -> Result<(), String> {
    if let Some(t) = title {
        crate::entities::check_text("title", t, MAX_NOTE_TITLE, true)?;
        if t.contains(['\n', '\r']) {
            return Err("title must be one line".into());
        }
    }
    crate::entities::check_text("body", body, MAX_NOTE_BODY, false)
}

fn note_from_row(r: &rusqlite::Row) -> rusqlite::Result<WorkingNote> {
    Ok(WorkingNote {
        id: r.get("id")?,
        domain: r.get("domain")?,
        title: r.get("title")?,
        body: r.get("body")?,
        updated_by: r.get("updated_by")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

pub fn list_notes(conn: &Connection, user_id: &str) -> rusqlite::Result<Vec<WorkingNote>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM working_notes WHERE user_id = ?1 ORDER BY updated_at DESC, domain",
    )?;
    stmt.query_map([user_id], note_from_row)?.collect()
}

pub fn get_note(
    conn: &Connection,
    user_id: &str,
    domain: &str,
) -> rusqlite::Result<Option<WorkingNote>> {
    conn.query_row(
        "SELECT * FROM working_notes WHERE user_id = ?1 AND domain = ?2",
        params![user_id, domain],
        note_from_row,
    )
    .optional()
}

/// Why an upsert was refused.
#[derive(Debug, PartialEq)]
pub enum NoteError {
    Invalid(String),
    TooMany,
}

impl std::fmt::Display for NoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoteError::Invalid(m) => f.write_str(m),
            NoteError::TooMany => write!(
                f,
                "I already keep {MAX_NOTES} working notes; delete or merge one before adding another"
            ),
        }
    }
}

/// Create or replace the note for `domain`. The previous version goes to
/// history (capped). A missing title keeps the current one, or is made from
/// the domain for a new note.
pub fn upsert_note(
    conn: &Connection,
    user_id: &str,
    domain: &str,
    title: Option<&str>,
    body: &str,
    updated_by: &str,
) -> rusqlite::Result<Result<WorkingNote, NoteError>> {
    let Some(key) = slug(domain) else {
        return Ok(Err(NoteError::Invalid("domain is required".into())));
    };
    if let Err(e) = check_note(title, body) {
        return Ok(Err(NoteError::Invalid(e)));
    }
    let body = body.trim();
    let now = now_ms();
    db::write_tx(conn, |c| {
        let existing = get_note(c, user_id, &key)?;
        let note = match existing {
            Some(old) => {
                let title = title.map(str::trim).unwrap_or(&old.title);
                if old.title == title && old.body == body {
                    return Ok(Ok(old));
                }
                c.execute(
                    "INSERT INTO working_note_history (note_id, title, body, updated_by, replaced_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![old.id, old.title, old.body, old.updated_by, now],
                )?;
                c.execute(
                    "DELETE FROM working_note_history WHERE note_id = ?1 AND id NOT IN (
                         SELECT id FROM working_note_history WHERE note_id = ?1
                         ORDER BY replaced_at DESC, id DESC LIMIT ?2)",
                    params![old.id, HISTORY_PER_NOTE as i64],
                )?;
                c.execute(
                    "UPDATE working_notes SET title = ?2, body = ?3, updated_by = ?4, updated_at = ?5
                     WHERE id = ?1",
                    params![old.id, title, body, updated_by, now],
                )?;
                get_note(c, user_id, &key)?.expect("updated")
            }
            None => {
                let n: i64 = c.query_row(
                    "SELECT COUNT(*) FROM working_notes WHERE user_id = ?1",
                    [user_id],
                    |r| r.get(0),
                )?;
                if n as usize >= MAX_NOTES {
                    return Ok(Err(NoteError::TooMany));
                }
                let title = title
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| title_from(&key));
                let id = new_id();
                c.execute(
                    "INSERT INTO working_notes (id, user_id, domain, title, body, updated_by, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    params![id, user_id, key, title, body, updated_by, now],
                )?;
                get_note(c, user_id, &key)?.expect("inserted")
            }
        };
        Ok(Ok(note))
    })
}

pub fn delete_note(conn: &Connection, user_id: &str, domain: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM working_notes WHERE user_id = ?1 AND domain = ?2",
        params![user_id, domain],
    )?;
    Ok(n > 0)
}

pub fn note_history(
    conn: &Connection,
    user_id: &str,
    domain: &str,
) -> rusqlite::Result<Vec<NoteVersion>> {
    let mut stmt = conn.prepare(
        "SELECT h.id, h.title, h.body, h.updated_by, h.replaced_at
         FROM working_note_history h JOIN working_notes n ON n.id = h.note_id
         WHERE n.user_id = ?1 AND n.domain = ?2
         ORDER BY h.replaced_at DESC, h.id DESC",
    )?;
    stmt.query_map(params![user_id, domain], |r| {
        Ok(NoteVersion {
            id: r.get(0)?,
            title: r.get(1)?,
            body: r.get(2)?,
            updated_by: r.get(3)?,
            replaced_at: r.get(4)?,
        })
    })?
    .collect()
}

/// The notes as Markdown for a prompt, newest first, within `cap`
/// characters (whole notes; the rest are listed by title only).
pub fn render_notes(notes: &[WorkingNote], cap: usize) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut left_out = Vec::new();
    for n in notes {
        let block = format!(
            "\n<working_note domain=\"{}\" updated=\"{}\">\n# {}\n\n{}\n</working_note>\n",
            n.domain,
            crate::util::rfc3339(n.updated_at / 1000),
            n.title,
            if n.body.is_empty() {
                "(empty)"
            } else {
                &n.body
            }
        );
        if out.chars().count() + block.chars().count() > cap {
            left_out.push(n);
            continue;
        }
        out.push_str(&block);
    }
    if !left_out.is_empty() {
        out.push_str("\nNot shown in full (read them with `get_working_note`):\n");
        for n in left_out {
            out.push_str(&format!("- `{}` {}\n", n.domain, n.title));
        }
    }
    out
}

// ------------------------------------------------------- proactive messages

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProactiveMessage {
    pub id: String,
    pub message_id: Option<String>,
    /// Which job sent it (`heartbeat`).
    pub source: String,
    pub excerpt: String,
    pub outcome: String,
    pub created_at: i64,
    pub outcome_at: Option<i64>,
}

fn pm_from_row(r: &rusqlite::Row) -> rusqlite::Result<ProactiveMessage> {
    Ok(ProactiveMessage {
        id: r.get("id")?,
        message_id: r.get("message_id")?,
        source: r.get("source")?,
        excerpt: r.get("excerpt")?,
        outcome: r.get("outcome")?,
        created_at: r.get("created_at")?,
        outcome_at: r.get("outcome_at")?,
    })
}

/// Record a message the agent sent on its own (`message_id` set, outcome
/// `posted`) or held back for the budget (`held`, no message).
pub fn record(
    conn: &Connection,
    user_id: &str,
    message_id: Option<&str>,
    source: &str,
    text: &str,
    outcome: &str,
) -> rusqlite::Result<ProactiveMessage> {
    let id = new_id();
    conn.execute(
        "INSERT INTO proactive_messages (id, user_id, message_id, source, excerpt, outcome, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            user_id,
            message_id,
            source,
            truncate_chars(text.trim(), EXCERPT_CHARS),
            outcome,
            now_ms()
        ],
    )?;
    conn.query_row(
        "SELECT * FROM proactive_messages WHERE id = ?1",
        [&id],
        pm_from_row,
    )
}

/// The owner's verdict on a posted message: `useful` or `not_needed`.
/// `None` when no proactive message was posted as `message_id`.
pub fn set_outcome(
    conn: &Connection,
    user_id: &str,
    message_id: &str,
    outcome: &str,
) -> rusqlite::Result<Option<ProactiveMessage>> {
    conn.execute(
        "UPDATE proactive_messages SET outcome = ?3, outcome_at = ?4
         WHERE user_id = ?1 AND message_id = ?2 AND outcome <> 'held'",
        params![user_id, message_id, outcome, now_ms()],
    )?;
    conn.query_row(
        "SELECT * FROM proactive_messages WHERE user_id = ?1 AND message_id = ?2",
        params![user_id, message_id],
        pm_from_row,
    )
    .optional()
}

/// Messages posted on the same local day as `now` (held ones do not count).
pub fn posted_today(conn: &Connection, user_id: &str, now: i64) -> rusqlite::Result<i64> {
    let day = crate::jobs::local_day(now);
    let mut stmt = conn.prepare(
        "SELECT created_at FROM proactive_messages
         WHERE user_id = ?1 AND outcome <> 'held' AND created_at >= ?2",
    )?;
    let since = now - 2 * 24 * 60 * 60 * 1000;
    let n = stmt
        .query_map(params![user_id, since], |r| r.get::<_, i64>(0))?
        .filter_map(Result::ok)
        .filter(|t| crate::jobs::local_day(*t) == day)
        .count();
    Ok(n as i64)
}

/// Messages held back recently, oldest first.
pub fn held_recent(
    conn: &Connection,
    user_id: &str,
    now: i64,
) -> rusqlite::Result<Vec<ProactiveMessage>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM proactive_messages
         WHERE user_id = ?1 AND outcome = 'held' AND created_at >= ?2
         ORDER BY created_at",
    )?;
    stmt.query_map(params![user_id, now - HELD_FOR_MS], pm_from_row)?
        .collect()
}

/// What the owner did with recent proactive messages.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Scorecard {
    pub days: i64,
    pub sent: i64,
    pub useful: i64,
    pub not_needed: i64,
    /// Posted messages with no verdict that the owner wrote back to soon after.
    pub replied: i64,
    /// Posted messages with no verdict and no reply.
    pub ignored: i64,
    pub held: i64,
    /// Excerpts the owner marked not needed, newest first (at most five).
    pub not_needed_examples: Vec<String>,
}

pub fn scorecard(conn: &Connection, user_id: &str, now: i64) -> rusqlite::Result<Scorecard> {
    let since = now - SCORECARD_DAYS * 24 * 60 * 60 * 1000;
    let main = db::ensure_main_conversation(conn, user_id)?;
    let mut stmt = conn.prepare(
        "SELECT p.*, EXISTS (
             SELECT 1 FROM messages m WHERE m.conversation_id = ?3 AND m.role = 'user'
             AND m.author_user_id = ?1 AND m.kind = 'text'
             AND m.created_at > p.created_at AND m.created_at <= p.created_at + ?4
         ) AS replied
         FROM proactive_messages p
         WHERE p.user_id = ?1 AND p.created_at >= ?2
         ORDER BY p.created_at DESC",
    )?;
    let rows = stmt.query_map(params![user_id, since, main, REPLY_WINDOW_MS], |r| {
        Ok((pm_from_row(r)?, r.get::<_, bool>("replied")?))
    })?;
    let mut s = Scorecard {
        days: SCORECARD_DAYS,
        ..Default::default()
    };
    for row in rows {
        let (m, replied) = row?;
        match m.outcome.as_str() {
            "held" => s.held += 1,
            "useful" => {
                s.sent += 1;
                s.useful += 1;
            }
            "not_needed" => {
                s.sent += 1;
                s.not_needed += 1;
                if s.not_needed_examples.len() < 5 {
                    s.not_needed_examples.push(m.excerpt);
                }
            }
            _ => {
                s.sent += 1;
                if replied {
                    s.replied += 1;
                } else {
                    s.ignored += 1;
                }
            }
        }
    }
    Ok(s)
}

impl Scorecard {
    /// One paragraph for the heartbeat prompt.
    pub fn render(&self) -> String {
        if self.sent == 0 && self.held == 0 {
            return format!(
                "No messages on my own initiative in the last {} days.",
                self.days
            );
        }
        let mut out = format!(
            "Last {} days: {} sent ({} marked useful, {} marked not needed, {} replied to, {} with no reply), {} held back for the daily budget.",
            self.days,
            self.sent,
            self.useful,
            self.not_needed,
            self.replied,
            self.ignored,
            self.held
        );
        if !self.not_needed_examples.is_empty() {
            out.push_str("\nMarked not needed (do not send this kind again):");
            for e in &self.not_needed_examples {
                out.push_str(&format!("\n- {}", e.replace('\n', " ")));
            }
        }
        out
    }

    pub fn json(&self) -> Value {
        json!({
            "days": self.days,
            "sent": self.sent,
            "useful": self.useful,
            "not_needed": self.not_needed,
            "replied": self.replied,
            "ignored": self.ignored,
            "held": self.held,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, NewMessage, insert_message, owner_user_id};

    fn with_db(f: impl FnOnce(&Connection, &str) -> rusqlite::Result<()>) {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            f(c, &owner)
        })
        .unwrap();
    }

    #[test]
    fn slugs_and_titles() {
        assert_eq!(
            slug("Household budget").as_deref(),
            Some("household-budget")
        );
        assert_eq!(
            slug("  Kitchen -- remodel! ").as_deref(),
            Some("kitchen-remodel")
        );
        assert_eq!(slug("***"), None);
        assert_eq!(title_from("household-budget"), "Household budget");
        let long = "a".repeat(200);
        assert!(slug(&long).unwrap().len() <= MAX_DOMAIN);
    }

    #[test]
    fn notes_upsert_history_and_cap() {
        with_db(|c, u| {
            let n =
                upsert_note(c, u, "Household budget", None, "Plan: pay bills.", "agent")?.unwrap();
            assert_eq!(n.domain, "household-budget");
            assert_eq!(n.title, "Household budget");
            // Unchanged content writes nothing.
            let same = upsert_note(
                c,
                u,
                "household-budget",
                None,
                "Plan: pay bills.",
                "heartbeat",
            )?
            .unwrap();
            assert_eq!(same.updated_by, "agent");
            assert!(note_history(c, u, "household-budget")?.is_empty());
            let n2 = upsert_note(
                c,
                u,
                "household-budget",
                Some("Budget"),
                "Plan: pay bills.\nState: paid.",
                "heartbeat",
            )?
            .unwrap();
            assert_eq!(n2.title, "Budget");
            assert_eq!(n2.updated_by, "heartbeat");
            let hist = note_history(c, u, "household-budget")?;
            assert_eq!(hist.len(), 1);
            assert_eq!(hist[0].body, "Plan: pay bills.");
            for i in 0..30 {
                upsert_note(c, u, "household-budget", None, &format!("v{i}"), "agent")?.unwrap();
            }
            assert_eq!(
                note_history(c, u, "household-budget")?.len(),
                HISTORY_PER_NOTE
            );
            assert_eq!(
                upsert_note(c, u, "x", Some("Line\nbreak"), "", "agent")?,
                Err(NoteError::Invalid("title must be one line".into()))
            );
            assert!(matches!(
                upsert_note(c, u, "x", None, &"b".repeat(MAX_NOTE_BODY + 1), "agent")?,
                Err(NoteError::Invalid(_))
            ));
            for i in 1..MAX_NOTES {
                upsert_note(c, u, &format!("d{i}"), None, "", "agent")?.unwrap();
            }
            assert_eq!(list_notes(c, u)?.len(), MAX_NOTES);
            assert_eq!(
                upsert_note(c, u, "one-more", None, "", "agent")?,
                Err(NoteError::TooMany)
            );
            assert!(delete_note(c, u, "d1")?);
            assert!(!delete_note(c, u, "d1")?);
            assert!(get_note(c, u, "d1")?.is_none());
            // Another user sees nothing.
            assert!(list_notes(c, "someone-else")?.is_empty());
            Ok(())
        })
    }

    #[test]
    fn notes_render_within_cap() {
        with_db(|c, u| {
            upsert_note(c, u, "small", None, "tiny", "agent")?.unwrap();
            upsert_note(c, u, "big", None, &"x".repeat(500), "agent")?.unwrap();
            let notes = list_notes(c, u)?;
            let full = render_notes(&notes, 10_000);
            assert!(full.contains("<working_note domain=\"big\""));
            assert!(full.contains("tiny"));
            let capped = render_notes(&notes, 200);
            assert!(!capped.contains(&"x".repeat(500)));
            assert!(capped.contains("- `big` Big"));
            assert!(capped.contains("tiny"));
            assert_eq!(render_notes(&[], 100), "");
            Ok(())
        })
    }

    #[test]
    fn scorecard_counts_verdicts_replies_and_held() {
        with_db(|c, u| {
            let main = db::ensure_main_conversation(c, u)?;
            let post = |c: &Connection, text: &str| -> rusqlite::Result<String> {
                let m = insert_message(
                    c,
                    &NewMessage {
                        conversation_id: &main,
                        role: "assistant",
                        content: text,
                        ..Default::default()
                    },
                )?;
                record(c, u, Some(&m.id), "heartbeat", text, "posted")?;
                Ok(m.id)
            };
            let a = post(c, "Rent is due Friday and the account is short.")?;
            let b = post(c, "Nothing changed today.")?;
            let d = post(c, "Package arrives at 3.")?;
            record(c, u, None, "heartbeat", "Held for later", "held")?;
            assert_eq!(set_outcome(c, u, &a, "useful")?.unwrap().outcome, "useful");
            assert_eq!(
                set_outcome(c, u, &b, "not_needed")?.unwrap().outcome,
                "not_needed"
            );
            assert!(set_outcome(c, u, "no-such-message", "useful")?.is_none());
            // The owner wrote back right after the third one.
            insert_message(
                c,
                &NewMessage {
                    conversation_id: &main,
                    role: "user",
                    author_user_id: Some(u),
                    content: "thanks",
                    ..Default::default()
                },
            )?;
            let s = scorecard(c, u, now_ms() + 1)?;
            assert_eq!(
                (s.sent, s.useful, s.not_needed, s.replied, s.ignored, s.held),
                (3, 1, 1, 1, 0, 1)
            );
            assert_eq!(
                s.not_needed_examples,
                vec!["Nothing changed today.".to_string()]
            );
            assert!(s.render().contains("do not send this kind again"));
            assert_eq!(posted_today(c, u, now_ms())?, 3);
            let held = held_recent(c, u, now_ms())?;
            assert_eq!(held.len(), 1);
            assert_eq!(held[0].excerpt, "Held for later");
            assert!(held_recent(c, u, now_ms() + HELD_FOR_MS + 1)?.is_empty());
            let _ = d;
            Ok(())
        })
    }
}
