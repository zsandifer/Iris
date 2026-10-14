//! Running routines. The scheduler's minute tick (`jobs.rs`) calls
//! [`tick`]: it expires unanswered approvals, starts due retries, and
//! claims due slots. Each run is a fresh hidden job conversation with only
//! the owner in it (`Purpose::Scheduled`), through the same `run_turn`,
//! post-to-Main and move-cards path as the heartbeat.
//!
//! - One row per slot (`UNIQUE (routine_id, scheduled_for)`), claimed in
//!   the same database step that moves `next_run_at` on: a slot never runs
//!   twice. A routine never runs twice at once: a slot that comes up while
//!   its last run is still going (or waiting for the owner) is skipped.
//! - Late slots (Iris was offline or paused): `skip` records them as
//!   skipped; `run_latest_in_window` runs the latest one if it is less than
//!   an hour late.
//! - Ask-before-acting routines run in two steps: Iris prepares the work
//!   and ends with a `NEEDS_OK:` line; the owner gets an approval card; on
//!   their OK the same conversation continues and does it. No answer within
//!   `approval_timeout_mins` makes the run skipped. Nothing is held open
//!   meanwhile, so waiting survives restarts.
//! - Failed runs are retried after 1, 5, then 30 minutes (up to
//!   `max_retries`), within the missed-run rule. An attempt that may have
//!   acted (the acting step, or a routine that does not ask first and used
//!   tools) is never retried automatically.
//! - A run that was going when Iris stopped is recovered at startup: retried
//!   if it was only preparing, otherwise recorded as failed.

use super::schedule::Plan;
use super::{Misfire, Routine, State, docs, get_routine_any, schedule_next};
use crate::db::{self, Message, NewMessage};
use crate::entities;
use crate::router::Purpose;
use crate::state::AppState;
use crate::turn::{TurnInput, run_turn};
use crate::util::{new_id, now_ms, truncate_chars};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// A slot this late still runs on time (the tick is every minute).
pub const GRACE_MS: i64 = 5 * 60_000;
/// `run_latest_in_window`: how late a missed slot may still run.
pub const MISFIRE_WINDOW_MS: i64 = 60 * 60_000;
/// Waits before retries 1, 2, 3.
pub const BACKOFF_MINS: [i64; 3] = [1, 5, 30];
/// Last line of a prepared run that needs the owner's OK.
pub const NEEDS_OK: &str = "NEEDS_OK:";
/// Hidden conversations kept per routine (older runs keep their history
/// row, not the transcript).
const KEEP_RUN_CHATS: i64 = 10;

#[derive(Debug, Clone, PartialEq)]
pub enum Due {
    Run,
    Skip(String),
}

/// What to do with a routine whose `next_run_at` has come.
#[derive(Debug, Clone, PartialEq)]
pub struct DuePlan {
    /// The latest slot that has come (earlier missed ones are passed over).
    pub slot: i64,
    /// Earlier slots passed over.
    pub missed: u32,
    pub due: Due,
}

pub fn plan_due(plan: &Plan, next_run_at: i64, now: i64, misfire: Misfire) -> DuePlan {
    let mut slot = next_run_at;
    let mut missed = 0u32;
    while missed < 100_000 {
        match plan.next_after(slot) {
            Some(n) if n <= now => {
                slot = n;
                missed += 1;
            }
            _ => break,
        }
    }
    let late = now - slot;
    let due = if late <= GRACE_MS
        || (misfire == Misfire::RunLatestInWindow && late <= MISFIRE_WINDOW_MS)
    {
        Due::Run
    } else {
        let mut why = "I was offline or paused at the time, so I skipped it.".to_string();
        if missed > 0 {
            why = format!(
                "I was offline or paused at the time, so I skipped it (and {missed} earlier \
                 time{}).",
                if missed == 1 { "" } else { "s" }
            );
        }
        Due::Skip(why)
    };
    DuePlan { slot, missed, due }
}

#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: String,
    pub routine_id: String,
    pub scheduled_for: i64,
    pub manual: bool,
    pub status: String,
    pub reason: String,
    pub summary: String,
    pub attempts: i64,
    pub retry_at: Option<i64>,
    pub conversation_id: Option<String>,
    pub card_message_id: Option<String>,
    pub expires_at: Option<i64>,
    pub approved_at: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

fn run_from_row(r: &rusqlite::Row) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: r.get("id")?,
        routine_id: r.get("routine_id")?,
        scheduled_for: r.get("scheduled_for")?,
        manual: r.get("manual")?,
        status: r.get("status")?,
        reason: r.get("reason")?,
        summary: r.get("summary")?,
        attempts: r.get("attempts")?,
        retry_at: r.get("retry_at")?,
        conversation_id: r.get("conversation_id")?,
        card_message_id: r.get("card_message_id")?,
        expires_at: r.get("expires_at")?,
        approved_at: r.get("approved_at")?,
        started_at: r.get("started_at")?,
        finished_at: r.get("finished_at")?,
    })
}

pub fn run_json(r: &RunRow) -> Value {
    json!({
        "id": r.id,
        "scheduled_for": r.scheduled_for,
        "manual": r.manual,
        "status": r.status,
        "reason": r.reason,
        "summary": r.summary,
        "attempts": r.attempts,
        "card_message_id": r.card_message_id,
        "retry_at": r.retry_at,
        "expires_at": r.expires_at,
        "approved_at": r.approved_at,
        "started_at": r.started_at,
        "finished_at": r.finished_at,
    })
}

pub fn get_run(conn: &Connection, id: &str) -> rusqlite::Result<Option<RunRow>> {
    conn.query_row(
        "SELECT * FROM routine_runs WHERE id = ?1",
        [id],
        run_from_row,
    )
    .optional()
}

/// Newest first.
pub fn list_runs(
    conn: &Connection,
    routine_id: &str,
    limit: usize,
) -> rusqlite::Result<Vec<RunRow>> {
    conn.prepare(
        "SELECT * FROM routine_runs WHERE routine_id = ?1 ORDER BY scheduled_for DESC LIMIT ?2",
    )?
    .query_map(params![routine_id, limit as i64], run_from_row)?
    .collect()
}

fn open_run(conn: &Connection, routine_id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM routine_runs WHERE routine_id = ?1
                        AND status IN ('running', 'waiting'))",
        [routine_id],
        |r| r.get(0),
    )
}

/// Record a slot. Returns the run id, or None when the slot already has
/// a row (it never runs twice).
fn insert_run(
    conn: &Connection,
    routine_id: &str,
    slot: i64,
    manual: bool,
    status: &str,
    reason: &str,
    now: i64,
) -> rusqlite::Result<Option<String>> {
    let id = new_id();
    let running = status == "running";
    let n = conn.execute(
        "INSERT OR IGNORE INTO routine_runs (id, routine_id, scheduled_for, manual, status,
             reason, attempts, started_at, finished_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            id,
            routine_id,
            slot,
            manual,
            status,
            reason,
            running as i64,
            running.then_some(now),
            (!running).then_some(now),
            now
        ],
    )?;
    Ok((n > 0).then_some(id))
}

/// Start due retries and claim due slots; returns the runs to start.
pub fn claim_due(conn: &Connection, now: i64) -> rusqlite::Result<Vec<String>> {
    claim_due_changes(conn, now).map(|(runs, _)| runs)
}

/// [`claim_due`], and whether anything changed (slots skipped, retries
/// given up, routines finished) even when nothing starts.
pub fn claim_due_changes(conn: &Connection, now: i64) -> rusqlite::Result<(Vec<String>, bool)> {
    db::write_tx(conn, |c| {
        let mut out = Vec::new();
        let mut changed = false;
        let retries: Vec<(String, String, i64, String)> = c
            .prepare(
                "SELECT r.id, r.routine_id, r.retry_at, t.misfire
                 FROM routine_runs r JOIN routines t ON t.id = r.routine_id
                 WHERE r.status = 'failed' AND r.retry_at IS NOT NULL AND r.retry_at <= ?1
                   AND t.state IN ('active', 'done')
                 ORDER BY r.retry_at",
            )?
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        changed |= !retries.is_empty();
        for (run, routine, retry_at, misfire) in retries {
            // A retry missed while Iris was offline or paused follows the
            // routine's missed-run rule, like a missed slot.
            let late = now - retry_at;
            let window = if misfire == Misfire::RunLatestInWindow.name() {
                MISFIRE_WINDOW_MS
            } else {
                GRACE_MS
            };
            if late > window {
                finish_run(
                    c,
                    &run,
                    "failed",
                    "I was offline or paused when I meant to try again, so I stopped.",
                    None,
                    now,
                )?;
                continue;
            }
            if open_run(c, &routine)? {
                continue;
            }
            c.execute(
                "UPDATE routine_runs SET status = 'running', retry_at = NULL, reason = '',
                     attempts = attempts + 1, started_at = ?2, updated_at = ?2
                 WHERE id = ?1",
                params![run, now],
            )?;
            out.push(run);
        }
        let due: Vec<Routine> = c
            .prepare(
                "SELECT * FROM routines WHERE state = 'active' AND next_run_at IS NOT NULL
                   AND next_run_at <= ?1 ORDER BY next_run_at",
            )?
            .query_map([now], super::routine_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        changed |= !due.is_empty();
        for r in due {
            let Ok(plan) = r.spec.plan() else {
                // Cannot be scheduled any more (e.g. a time zone removed
                // from the system): stop it rather than retry every minute.
                c.execute(
                    "UPDATE routines SET state = 'paused', next_run_at = NULL, updated_at = ?2
                     WHERE id = ?1",
                    params![r.id, now],
                )?;
                continue;
            };
            let d = plan_due(&plan, r.next_run_at.unwrap_or(now), now, r.spec.misfire);
            let mut run_count = r.run_count;
            match d.due {
                Due::Run if open_run(c, &r.id)? => {
                    insert_run(
                        c,
                        &r.id,
                        d.slot,
                        false,
                        "skipped",
                        "The last run was still going, so I skipped this one.",
                        now,
                    )?;
                }
                Due::Run => {
                    if let Some(id) = insert_run(c, &r.id, d.slot, false, "running", "", now)? {
                        run_count += 1;
                        out.push(id);
                    }
                }
                Due::Skip(why) => {
                    insert_run(c, &r.id, d.slot, false, "skipped", &why, now)?;
                }
            }
            let (state, next) = schedule_next(&r.spec, &plan, run_count, d.slot);
            c.execute(
                "UPDATE routines SET run_count = ?2, state = ?3, next_run_at = ?4, updated_at = ?5
                 WHERE id = ?1",
                params![r.id, run_count, state.name(), next, now],
            )?;
        }
        Ok((out, changed))
    })
}

/// "Run now": a manual run, unless one is already going.
pub fn claim_manual(
    conn: &Connection,
    owner: &str,
    routine_id: &str,
    now: i64,
) -> Result<String, String> {
    db::write_tx(conn, |c| {
        let Some(r) = super::get_routine(c, owner, routine_id)? else {
            return Ok(Err("I couldn't find that routine.".to_string()));
        };
        if r.state == State::PendingApproval {
            return Ok(Err("Set it up first, then I can run it.".into()));
        }
        if open_run(c, routine_id)? {
            return Ok(Err("It's already running.".into()));
        }
        let mut slot = now;
        loop {
            if let Some(id) = insert_run(c, routine_id, slot, true, "running", "", now)? {
                return Ok(Ok(id));
            }
            slot += 1;
        }
    })
    .map_err(|e| e.to_string())?
}

/// Close an approval card if it is still pending; returns the updated
/// message then.
pub fn close_card(conn: &Connection, card_id: &str, to: &str) -> rusqlite::Result<Option<Message>> {
    let Some(m) = db::get_message(conn, card_id)? else {
        return Ok(None);
    };
    let mut card = m.card.unwrap_or_default();
    if card["state"] != "pending" {
        return Ok(None);
    }
    card["state"] = to.into();
    card["decided_at"] = now_ms().into();
    Ok(Some(db::update_message_card(conn, card_id, &card)?))
}

/// Runs whose approval card went unanswered become skipped. Returns the
/// closed cards.
pub fn expire_waiting(conn: &Connection, now: i64) -> rusqlite::Result<Vec<Message>> {
    db::write_tx(conn, |c| {
        // A decision moves its run on in the same step (`decide_in_tx`), so
        // a run still waiting here was not answered in time.
        let waiting: Vec<(String, Option<String>, i64)> = c
            .prepare(
                "SELECT r.id, r.card_message_id, t.approval_timeout_mins
                 FROM routine_runs r JOIN routines t ON t.id = r.routine_id
                 WHERE r.status = 'waiting' AND (r.expires_at IS NULL OR r.expires_at <= ?1)",
            )?
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut cards = Vec::new();
        for (run, card, mins) in waiting {
            if let Some(id) = &card
                && let Some(m) = close_card(c, id, "expired")?
            {
                cards.push(m);
            }
            finish_run(c, &run, "skipped", &timeout_reason(mins), None, now)?;
        }
        Ok(cards)
    })
}

fn timeout_reason(mins: i64) -> String {
    format!(
        "You didn't answer within {}, so I didn't do it.",
        wait_text(mins)
    )
}

fn wait_text(mins: i64) -> String {
    match mins {
        m if m % 60 == 0 && m >= 60 => {
            let h = m / 60;
            format!("{h} hour{}", if h == 1 { "" } else { "s" })
        }
        m => format!("{m} minutes"),
    }
}

fn finish_run(
    conn: &Connection,
    run_id: &str,
    status: &str,
    reason: &str,
    summary: Option<&str>,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE routine_runs SET status = ?2, reason = ?3, summary = COALESCE(?4, summary),
             retry_at = NULL, finished_at = ?5, updated_at = ?5
         WHERE id = ?1",
        params![run_id, status, reason, summary, now],
    )?;
    Ok(())
}

/// After a restart: runs that were going cannot be finished. Retry them
/// (the preparing step, within the hour) or record the failure.
pub fn recover(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    db::write_tx(conn, |c| {
        let running: Vec<RunRow> = c
            .prepare("SELECT * FROM routine_runs WHERE status = 'running'")?
            .query_map([], run_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        for run in &running {
            let routine = get_routine_any(c, &run.routine_id)?;
            let retries = routine.as_ref().map_or(0, |r| r.spec.max_retries);
            // An attempt allowed to act (the step after the owner's OK, or a
            // routine that does not ask first) may have bought or sent
            // something before the restart: never do it again on its own.
            let may_have_acted = run.approved_at.is_some()
                || routine.as_ref().is_none_or(|r| !r.spec.ask_before_acting);
            if may_have_acted {
                finish_run(
                    c,
                    &run.id,
                    "failed",
                    "I was restarted partway through doing this, so I stopped. Check what \
                     happened before running it again.",
                    None,
                    now,
                )?;
            } else if run.attempts <= retries && now - run.scheduled_for <= MISFIRE_WINDOW_MS {
                c.execute(
                    "UPDATE routine_runs SET status = 'failed', retry_at = ?2, updated_at = ?3,
                         reason = 'I was restarted partway through, so I am trying again.'
                     WHERE id = ?1",
                    params![run.id, now + BACKOFF_MINS[0] * 60_000, now],
                )?;
            } else {
                finish_run(
                    c,
                    &run.id,
                    "failed",
                    "I was restarted partway through and it was too late to try again.",
                    None,
                    now,
                )?;
            }
        }
        Ok(running.len())
    })
}

/// Split a prepared reply into what to tell the owner and, when Iris needs
/// their OK, what she will do.
pub fn split_needs_ok(text: &str) -> (String, Option<String>) {
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate().rev() {
        let clean = line
            .trim()
            .trim_matches(|c| c == '*' || c == '`' || c == '_')
            .trim();
        if clean.is_empty() {
            continue;
        }
        if let Some(rest) = clean.strip_prefix(NEEDS_OK) {
            let what = rest.trim().trim_matches(|c| c == '*' || c == '`').trim();
            if what.is_empty() {
                break;
            }
            let mut body = lines[..i].to_vec();
            body.extend(&lines[i + 1..]);
            return (body.join("\n").trim().to_string(), Some(what.to_string()));
        }
        break;
    }
    (text.trim().to_string(), None)
}

/// The prompt for a routine's run (the preparing step).
pub fn build_prompt(
    r: &Routine,
    docs: &[(String, String)],
    scheduled_for: i64,
    manual: bool,
) -> String {
    let when = r
        .spec
        .plan()
        .map(|p| p.local_text(scheduled_for))
        .unwrap_or_default();
    let mut out = if manual {
        format!(
            "The owner asked you to run their routine “{}” now ({when}, {}).\n\n",
            r.spec.title, r.spec.tz
        )
    } else {
        format!(
            "Scheduled run of the owner's routine “{}” ({when}, {}).\n\n",
            r.spec.title, r.spec.tz
        )
    };
    out.push_str(&format!(
        "Do what the routine's instructions say.\n\n<instructions>\n{}\n</instructions>\n\n",
        r.spec.instructions.trim()
    ));
    if !docs.is_empty() {
        out.push_str("The owner attached these reference documents to the routine. Use them:\n\n");
        out.push_str(&docs::prompt_sections(docs, docs::MAX_DOC_CHARS));
    }
    if r.spec.ask_before_acting {
        out.push_str(&format!(
            "This routine checks with the owner before acting. In this run, prepare \
             everything (look things up, fill a cart, draft the message) but do not buy, \
             order, book, pay, send, post, delete, or submit anything. Connected services \
             refuse those actions in this step, and the browser can only open, read and \
             scroll pages (no clicking or typing) until the owner says yes. If the task \
             needs one of those actions, end your reply with a last line that starts with \
             `{NEEDS_OK}` followed by one short sentence saying exactly what you will do once \
             they say yes, for example `{NEEDS_OK} Order Pad See Ew from Thai Orchid for $31 \
             with tip.` The owner gets an approval card, and you continue in this \
             conversation after they approve. If no such action is needed, just do the task \
             and leave that line out.\n\n"
        ));
    } else {
        out.push_str(
            "This routine may act without checking first. Connected services still ask the \
             owner before anything they set to ask, such as sending a message or making a \
             purchase.\n\n",
        );
    }
    out.push_str(
        "Your reply goes to the owner's Main chat. Keep it short: what you did or found, and \
         anything they need to do.",
    );
    out
}

fn act_prompt(summary: &str) -> String {
    format!(
        "The owner approved: “{summary}”. Go ahead and do exactly that now, nothing more. \
         Connected services may still ask them to confirm a send or a purchase; that is \
         expected. Then reply with a short note for the owner on how it went (it goes to \
         their Main chat)."
    )
}

/// The run's hidden conversation: a new one per attempt, with only the
/// owner in it. Older runs' transcripts are pruned.
pub fn start_conversation(
    conn: &Connection,
    run_id: &str,
    owner: &str,
    title: &str,
    prompt: &str,
) -> rusqlite::Result<(String, Message, Message)> {
    db::write_tx(conn, |c| {
        let run = get_run(c, run_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if let Some(old) = &run.conversation_id {
            c.execute(
                "DELETE FROM conversations WHERE id = ?1 AND kind = 'job'",
                [old],
            )?;
        }
        let conv = db::create_conversation(
            c,
            owner,
            "job",
            &truncate_chars(&format!("Routine: {title}"), 200),
            None,
        )?;
        c.execute(
            "UPDATE routine_runs SET conversation_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, conv.id, now_ms()],
        )?;
        c.execute(
            "DELETE FROM conversations WHERE kind = 'job' AND id IN (
                 SELECT conversation_id FROM routine_runs
                 WHERE routine_id = ?1 AND conversation_id IS NOT NULL
                   AND status NOT IN ('running', 'waiting')
                 ORDER BY scheduled_for DESC LIMIT -1 OFFSET ?2)",
            params![run.routine_id, KEEP_RUN_CHATS],
        )?;
        let (u, a) = insert_turn(c, &conv.id, prompt, run_id)?;
        Ok((conv.id, u, a))
    })
}

fn insert_turn(
    c: &Connection,
    conv: &str,
    prompt: &str,
    run_id: &str,
) -> rusqlite::Result<(Message, Message)> {
    let u = db::insert_message(
        c,
        &NewMessage {
            conversation_id: conv,
            role: "user",
            content: prompt,
            meta: Some(&json!({"job": "routine", "routine_run_id": run_id})),
            ..Default::default()
        },
    )?;
    let a = db::insert_message(
        c,
        &NewMessage {
            conversation_id: conv,
            role: "assistant",
            content: "",
            status: "streaming",
            ..Default::default()
        },
    )?;
    Ok((u, a))
}

// ------------------------------------------------------------------- async

fn changed(state: &AppState, owner: &str) {
    state.hub.to_users(
        &[owner.to_string()],
        json!({"type": "entities.changed", "kinds": ["routines"]}),
    );
}

async fn broadcast_cards(state: &AppState, cards: &[Message]) {
    for m in cards {
        crate::login::broadcast_updated(state, m).await;
    }
}

/// Stop the turns still going in a deleted routine's run conversations:
/// their connector and browser access ends at once (held approvals are
/// cancelled) and the CLI is stopped.
pub fn stop_turns(state: &AppState, conversations: &[String]) {
    for conv in conversations {
        crate::turn::cancel(state, conv);
    }
}

/// The scheduler's minute tick (not called while Iris is paused).
pub async fn tick(state: &AppState) {
    let now = now_ms();
    match state.db.call(move |c| expire_waiting(c, now)).await {
        Ok(cards) if !cards.is_empty() => {
            broadcast_cards(state, &cards).await;
            if let Ok(Some(owner)) = state.db.call(db::owner_user_id).await {
                changed(state, &owner);
            }
        }
        Ok(_) => {}
        Err(e) => crate::warn!("routines: expiring approvals failed: {e}"),
    }
    match state.db.call(move |c| claim_due_changes(c, now)).await {
        Ok((claimed, any)) => {
            // Skipped slots and finished routines change the list too.
            if any && let Ok(Some(owner)) = state.db.call(db::owner_user_id).await {
                changed(state, &owner);
            }
            for id in claimed {
                spawn_run(state, id, Step::Prepare);
            }
        }
        Err(e) => crate::warn!("routines: claiming due runs failed: {e}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Do the task (or prepare it, when the routine asks first).
    Prepare,
    /// The owner said yes: do what was prepared.
    Act,
}

pub fn spawn_run(state: &AppState, run_id: String, step: Step) {
    let st = state.clone();
    tokio::spawn(async move {
        if let Err(e) = execute(&st, &run_id, step).await {
            crate::warn!("routine run {run_id} failed: {e}");
            let id = run_id.clone();
            let _ = st
                .db
                .call(move |c| {
                    finish_run(
                        c,
                        &id,
                        "failed",
                        "Something went wrong on my side.",
                        None,
                        now_ms(),
                    )
                })
                .await;
        }
    });
}

struct RunCtx {
    run: RunRow,
    routine: Routine,
    docs: Vec<(String, String)>,
}

async fn execute(state: &AppState, run_id: &str, step: Step) -> crate::util::Result<()> {
    let id = run_id.to_string();
    let ctx = state
        .db
        .call(move |c| {
            let Some(run) = get_run(c, &id)? else {
                return Ok(None);
            };
            let Some(routine) = get_routine_any(c, &run.routine_id)? else {
                return Ok(None);
            };
            let docs = super::doc_texts(c, &routine.id)?;
            Ok(Some(RunCtx { run, routine, docs }))
        })
        .await?;
    let Some(ctx) = ctx else { return Ok(()) };
    if ctx.run.status != "running" {
        return Ok(());
    }
    let owner = ctx.routine.owner.clone();
    if state.is_paused().await {
        let id = run_id.to_string();
        state
            .db
            .call(move |c| {
                finish_run(
                    c,
                    &id,
                    "skipped",
                    "I was paused, so I skipped this one.",
                    None,
                    now_ms(),
                )
            })
            .await?;
        changed(state, &owner);
        return Ok(());
    }
    let (conv, user_msg, assistant_msg) = match step {
        Step::Prepare => {
            let prompt = build_prompt(
                &ctx.routine,
                &ctx.docs,
                ctx.run.scheduled_for,
                ctx.run.manual,
            );
            let (rid, o, t) = (
                run_id.to_string(),
                owner.clone(),
                ctx.routine.spec.title.clone(),
            );
            state
                .db
                .call(move |c| start_conversation(c, &rid, &o, &t, &prompt))
                .await?
        }
        Step::Act => {
            let Some(conv) = ctx.run.conversation_id.clone() else {
                return Err("the run's conversation is gone".into());
            };
            let (prompt, rid, cv) = (
                act_prompt(&ctx.run.summary),
                run_id.to_string(),
                conv.clone(),
            );
            let (u, a) = state
                .db
                .call(move |c| insert_turn(c, &cv, &prompt, &rid))
                .await?;
            (conv, u, a)
        }
    };
    crate::info!("routine run {run_id} started ({step:?})");
    let result = run_turn(
        state,
        TurnInput {
            conversation_id: conv,
            user_message: user_msg,
            assistant_message: assistant_msg,
            purpose: Purpose::Scheduled,
            slot: crate::ranking::Slot::Chat,
            owner_user_id: owner.clone(),
            continuity: step == Step::Act,
            prepare_only: step == Step::Prepare && ctx.routine.spec.ask_before_acting,
        },
    )
    .await;
    crate::info!(
        "routine run {run_id} finished: {}",
        if result.ok { "ok" } else { "error" }
    );
    after_turn(state, &ctx, step, result).await?;
    changed(state, &owner);
    Ok(())
}

fn failure_reason(m: &Message) -> &'static str {
    match m.meta["error"]["kind"].as_str().unwrap_or("") {
        "auth_required" => "My AI engine was signed out",
        "no_engine" => "No AI engine was available",
        "timeout" => "It took too long",
        _ => "Something went wrong on my side",
    }
}

async fn after_turn(
    state: &AppState,
    ctx: &RunCtx,
    step: Step,
    result: crate::turn::TurnResult,
) -> crate::util::Result<()> {
    let r = &ctx.routine;
    let owner = r.owner.clone();
    let run_id = ctx.run.id.clone();
    let now = now_ms();
    let label = format!(
        "{}{}",
        if r.spec.emoji.is_empty() {
            String::new()
        } else {
            format!("{} ", r.spec.emoji)
        },
        r.spec.title
    );
    if result.ok {
        let (body, needs) = split_needs_ok(&result.message.content);
        if let (Step::Prepare, true, Some(what)) = (step, r.spec.ask_before_acting, needs) {
            if !body.is_empty() {
                crate::jobs::post_to_main_opts(
                    state,
                    &owner,
                    &body,
                    &result.message,
                    "routine",
                    false,
                )
                .await?;
            }
            crate::jobs::move_cards_to_main(state, &owner, &result.message).await?;
            let expires_at = now + r.spec.approval_timeout_mins * 60_000;
            let mut card = entities::approval_card(&entities::ApprovalRequest {
                title: &format!("{label} is ready"),
                service: "routine",
                preview: &what,
                action_label: "Go ahead",
                approver_user_id: &owner,
                ..Default::default()
            });
            card["reason"] = format!(
                "You asked me to check with you before I act. I'll wait {} for your answer.",
                wait_text(r.spec.approval_timeout_mins)
            )
            .into();
            card["deny_label"] = "Not this time".into();
            card["routine_id"] = r.id.clone().into();
            card["routine_run_id"] = run_id.clone().into();
            card["expires_at"] = expires_at.into();
            let (o, rid, w) = (owner.clone(), run_id.clone(), what.clone());
            let msg = state
                .db
                .call(move |c| {
                    db::write_tx(c, |c| {
                        let main = db::ensure_main_conversation(c, &o)?;
                        let m = entities::insert_card(c, &main, &card, "assistant")?;
                        c.execute(
                            "UPDATE routine_runs SET status = 'waiting', summary = ?2,
                                 card_message_id = ?3, expires_at = ?4, updated_at = ?5,
                                 reason = 'Waiting for your OK.'
                             WHERE id = ?1",
                            params![rid, w, m.id, expires_at, now],
                        )?;
                        Ok(m)
                    })
                })
                .await?;
            state.hub.to_users(
                std::slice::from_ref(&owner),
                json!({"type": "message.created", "conversation_id": msg.conversation_id, "message": msg}),
            );
            crate::notify::push_users(
                state,
                std::slice::from_ref(&owner),
                json!({
                    "title": format!("{} needs your OK", state.agent_name().await),
                    "body": truncate_chars(&format!("{label}: {what}"), 180),
                    "conversation_id": msg.conversation_id,
                    "message_id": msg.id,
                    "kind": "approval",
                }),
            );
            return Ok(());
        }
        let summary = body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|l| truncate_chars(l.trim_start_matches('#').trim(), 160))
            .unwrap_or_else(|| "Done".into());
        if !body.is_empty() {
            crate::jobs::post_to_main_opts(
                state,
                &owner,
                &body,
                &result.message,
                "routine",
                r.spec.notify_when_done,
            )
            .await?;
        }
        crate::jobs::move_cards_to_main(state, &owner, &result.message).await?;
        let id = run_id.clone();
        state
            .db
            .call(move |c| finish_run(c, &id, "ok", "", Some(&summary), now))
            .await?;
        return Ok(());
    }
    // Did not finish.
    let m = &result.message;
    let kind = m.meta["error"]["kind"].as_str().unwrap_or("");
    let id = run_id.clone();
    if kind == "paused" {
        state
            .db
            .call(move |c| {
                finish_run(
                    c,
                    &id,
                    "skipped",
                    "I was paused, so I skipped this one.",
                    None,
                    now,
                )
            })
            .await?;
        return Ok(());
    }
    if m.status == "cancelled" {
        state
            .db
            .call(move |c| {
                finish_run(
                    c,
                    &id,
                    "failed",
                    "It was stopped before it finished.",
                    None,
                    now,
                )
            })
            .await?;
        return Ok(());
    }
    let why = failure_reason(m);
    let attempts = ctx.run.attempts;
    let used_tools = m.meta["activity"].as_array().is_some_and(|a| !a.is_empty());
    let retry = retry_after(
        step,
        r.spec.ask_before_acting,
        used_tools,
        attempts,
        r.spec.max_retries,
    );
    let unsafe_to_retry = retry == Retry::MayHaveActed;
    if let Retry::After(wait) = retry {
        let reason = format!(
            "{why}. I'll try again in {wait} minute{}.",
            if wait == 1 { "" } else { "s" }
        );
        state
            .db
            .call(move |c| {
                c.execute(
                    "UPDATE routine_runs SET status = 'failed', reason = ?2, retry_at = ?3,
                         updated_at = ?4
                     WHERE id = ?1",
                    params![id, reason, now + wait * 60_000, now],
                )
            })
            .await?;
        return Ok(());
    }
    let reason = match step {
        Step::Act => format!("{why} after you said yes."),
        Step::Prepare if unsafe_to_retry => format!(
            "{why} partway through, so I didn't try again in case part of it was already done."
        ),
        Step::Prepare if attempts > 1 => format!("{why}; I tried {attempts} times."),
        Step::Prepare => format!("{why}."),
    };
    let (rs, rid) = (reason.clone(), run_id.clone());
    state
        .db
        .call(move |c| finish_run(c, &rid, "failed", &rs, None, now))
        .await?;
    let text = format!(
        "I couldn't finish your routine “{}”. {reason}{}",
        r.spec.title,
        match (step, r.spec.schedule_kind) {
            (Step::Act, _) => " Nothing else will happen until its next run; check what got done.",
            _ if unsafe_to_retry =>
                " Check what got done; I'll run it again at its next scheduled time.",
            (_, super::Kind::Once) => "",
            _ => " I'll try again at its next scheduled time.",
        }
    );
    crate::jobs::post_to_main_opts(state, &owner, &text, m, "routine", r.spec.notify_when_done)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Try again after this many minutes.
    After(i64),
    /// Out of retries (or the acting step, which is never retried).
    No,
    /// It was allowed to act and used tools: it may have bought or sent
    /// something already, so it is not done again on its own.
    MayHaveActed,
}

/// Whether a failed attempt is tried again. An attempt allowed to act (the
/// step after the owner's OK, or a routine that does not ask first) is
/// retried only when it used no tools at all. Preparing for an ask-first
/// routine cannot act (`TurnInput::prepare_only`), so it is retried.
pub fn retry_after(
    step: Step,
    ask_first: bool,
    used_tools: bool,
    attempts: i64,
    max_retries: i64,
) -> Retry {
    if step == Step::Act {
        return Retry::No;
    }
    if !ask_first && used_tools {
        return Retry::MayHaveActed;
    }
    if attempts > max_retries {
        return Retry::No;
    }
    Retry::After(BACKOFF_MINS[(attempts.max(1) as usize - 1).min(BACKOFF_MINS.len() - 1)])
}

/// What deciding a run's approval card does to the run.
#[derive(Debug, PartialEq)]
pub enum RunDecision {
    /// The run moved on (to its acting step, or skipped on a no); the
    /// routine's owner.
    Proceed(String),
    /// Too late (past its deadline, now skipped) or no longer waiting: the
    /// card closes without doing anything.
    Closed,
}

/// The owner decided a run's approval card (`POST /api/messages/{id}/decision`),
/// inside the transaction that records the card's decision.
pub fn decide_in_tx(
    c: &Connection,
    run_id: &str,
    approve: bool,
    now: i64,
) -> rusqlite::Result<RunDecision> {
    let Some(run) = get_run(c, run_id)? else {
        return Ok(RunDecision::Closed);
    };
    if run.status != "waiting" {
        return Ok(RunDecision::Closed);
    }
    let Some(routine) = get_routine_any(c, &run.routine_id)? else {
        return Ok(RunDecision::Closed);
    };
    if run.expires_at.is_none_or(|t| t <= now) {
        finish_run(
            c,
            run_id,
            "skipped",
            &timeout_reason(routine.spec.approval_timeout_mins),
            None,
            now,
        )?;
        return Ok(RunDecision::Closed);
    }
    if approve {
        c.execute(
            "UPDATE routine_runs SET status = 'running', approved_at = ?2, reason = '',
                 updated_at = ?2
             WHERE id = ?1",
            params![run_id, now],
        )?;
    } else {
        finish_run(
            c,
            run_id,
            "skipped",
            "You said no, so I didn't do it.",
            None,
            now,
        )?;
    }
    Ok(RunDecision::Proceed(routine.owner))
}

/// After the decision is saved: tell the app, and do the approved work.
/// `owner` is None when the card closed without a decision.
pub async fn after_decision(state: &AppState, run_id: &str, owner: Option<String>, approve: bool) {
    let owner = match owner {
        Some(o) => o,
        None => match state.db.call(db::owner_user_id).await {
            Ok(Some(o)) => o,
            _ => return,
        },
    };
    changed(state, &owner);
    if approve {
        spawn_run(state, run_id.to_string(), Step::Act);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routines::schedule::{self, ParsedRule, Repeat, Rule, When};

    fn daily_plan() -> Plan {
        Plan {
            when: When::Rule(
                ParsedRule::parse(&Rule {
                    repeat: Repeat::Daily,
                    every: 1,
                    time: "09:00".into(),
                    anchor: None,
                })
                .unwrap(),
            ),
            tz: schedule::parse_tz("UTC").unwrap(),
            starts: None,
            ends: None,
        }
    }

    const DAY: i64 = 86_400_000;
    const NINE: i64 = 9 * 3_600_000;

    #[test]
    fn misfires_run_late_only_within_the_window() {
        let p = daily_plan();
        let slot = 100 * DAY + NINE;
        // On time (within the grace): runs whatever the policy.
        for m in [Misfire::Skip, Misfire::RunLatestInWindow] {
            assert_eq!(plan_due(&p, slot, slot + 60_000, m).due, Due::Run);
        }
        // 30 minutes late: only run_latest_in_window runs it.
        assert_eq!(
            plan_due(&p, slot, slot + 30 * 60_000, Misfire::RunLatestInWindow).due,
            Due::Run
        );
        assert!(matches!(
            plan_due(&p, slot, slot + 30 * 60_000, Misfire::Skip).due,
            Due::Skip(_)
        ));
        // Two hours late: both skip.
        assert!(matches!(
            plan_due(&p, slot, slot + 2 * 3_600_000, Misfire::RunLatestInWindow).due,
            Due::Skip(_)
        ));
        // Down for three days: the latest slot counts, the others are passed over.
        let d = plan_due(
            &p,
            slot,
            slot + 3 * DAY + 10 * 60_000,
            Misfire::RunLatestInWindow,
        );
        assert_eq!(d.slot, slot + 3 * DAY);
        assert_eq!(d.missed, 3);
        assert_eq!(d.due, Due::Run);
        let d = plan_due(
            &p,
            slot,
            slot + 3 * DAY + 2 * 3_600_000,
            Misfire::RunLatestInWindow,
        );
        let Due::Skip(why) = d.due else { panic!() };
        assert!(why.contains("3 earlier times"), "{why}");
    }

    #[test]
    fn attempts_that_may_have_acted_are_not_retried() {
        use Retry::*;
        // Preparing for an ask-first routine cannot act: retried with backoff.
        assert_eq!(retry_after(Step::Prepare, true, true, 1, 2), After(1));
        assert_eq!(retry_after(Step::Prepare, true, false, 2, 2), After(5));
        assert_eq!(retry_after(Step::Prepare, true, true, 3, 2), No);
        // A routine that acts without asking: only when it used no tools.
        assert_eq!(retry_after(Step::Prepare, false, false, 1, 2), After(1));
        assert_eq!(retry_after(Step::Prepare, false, true, 1, 2), MayHaveActed);
        // The step after the owner's OK: never.
        assert_eq!(retry_after(Step::Act, true, false, 1, 2), No);
    }

    #[test]
    fn needs_ok_line_is_split_off() {
        let (body, what) = split_needs_ok(
            "Thai Orchid is open. Cart: Pad See Ew ($31 with tip).\n\nNEEDS_OK: Order Pad See Ew from Thai Orchid for $31.",
        );
        assert_eq!(
            body,
            "Thai Orchid is open. Cart: Pad See Ew ($31 with tip)."
        );
        assert_eq!(
            what.as_deref(),
            Some("Order Pad See Ew from Thai Orchid for $31.")
        );
        let (_, what) = split_needs_ok("Ready.\n**NEEDS_OK: Send the rent reminder to Sam**\n");
        assert_eq!(what.as_deref(), Some("Send the rent reminder to Sam"));
        // Only a last line counts; an empty one does not.
        assert_eq!(split_needs_ok("NEEDS_OK: x\nmore text").1, None);
        assert_eq!(split_needs_ok("Done.\nNEEDS_OK:").1, None);
        assert_eq!(
            split_needs_ok("Your rent is due Friday.").0,
            "Your rent is due Friday."
        );
    }
}
