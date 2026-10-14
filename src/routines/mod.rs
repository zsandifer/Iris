//! Routines: scheduled jobs the owner sets up ("order dinner every
//! Saturday", "rent reminder on the 1st", "gift ideas on Oct 29 at 8").
//! They are separate from the built-in heartbeat and feed jobs (`jobs.rs`).
//!
//! - The owner creates and edits routines in the app; those changes apply
//!   at once.
//! - Iris proposes routines (and changes to them) through the `iris` MCP
//!   bridge. A proposal does nothing until the owner taps its chat card;
//!   pending proposals are stored, so they survive restarts.
//! - A due routine runs as a fresh hidden job conversation (`run.rs`), the
//!   owner's own individual use: it never involves a shared conversation.
//!
//! Plain SQL shared by the HTTP API, the scheduler, and the MCP bridge.

pub mod docs;
pub mod run;
pub mod schedule;

use crate::db::{self, Message};
use crate::entities;
use crate::util::{new_id, now_ms, truncate_chars};
use rusqlite::{Connection, OptionalExtension, params};
use schedule::{Plan, Rule, When};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_TITLE: usize = 120;
pub const MAX_INSTRUCTIONS: usize = 10_000;
/// Settings key: the time zone the owner last saved a routine in (the
/// default for new ones).
pub const TZ_SETTING: &str = "routines.tz";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Once,
    Rule,
    Cron,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Once => "once",
            Kind::Rule => "rule",
            Kind::Cron => "cron",
        }
    }
}

/// What to do with a run that was due while Iris was offline or paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Misfire {
    /// Never run late (routines that buy or send things).
    Skip,
    /// Run the latest missed one if it is less than an hour late.
    #[default]
    RunLatestInWindow,
}

impl Misfire {
    pub fn name(self) -> &'static str {
        match self {
            Misfire::Skip => "skip",
            Misfire::RunLatestInWindow => "run_latest_in_window",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    PendingApproval,
    Active,
    Paused,
    Done,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::PendingApproval => "pending_approval",
            State::Active => "active",
            State::Paused => "paused",
            State::Done => "done",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "pending_approval" => State::PendingApproval,
            "paused" => State::Paused,
            "done" => State::Done,
            _ => State::Active,
        }
    }
}

fn yes() -> bool {
    true
}
fn default_timeout() -> i64 {
    120
}
fn default_retries() -> i64 {
    2
}

/// The fields the owner (or Iris) sets on a routine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub title: String,
    #[serde(default)]
    pub emoji: String,
    /// What Iris does on every run (the prompt).
    pub instructions: String,
    pub schedule_kind: Kind,
    /// once: local "YYYY-MM-DDTHH:MM".
    #[serde(default)]
    pub run_at: Option<String>,
    #[serde(default)]
    pub rule: Option<Rule>,
    #[serde(default)]
    pub cron_expr: Option<String>,
    /// IANA time zone; empty means the owner's default.
    #[serde(default)]
    pub tz: String,
    /// Local dates "YYYY-MM-DD" (the end date is the last day it may run).
    #[serde(default)]
    pub starts_at: Option<String>,
    #[serde(default)]
    pub end_at: Option<String>,
    #[serde(default)]
    pub max_runs: Option<i64>,
    #[serde(default = "yes")]
    pub ask_before_acting: bool,
    #[serde(default = "yes")]
    pub notify_when_done: bool,
    #[serde(default = "default_timeout")]
    pub approval_timeout_mins: i64,
    #[serde(default)]
    pub misfire: Misfire,
    #[serde(default = "default_retries")]
    pub max_retries: i64,
}

/// Keys of `Spec` (patches may only touch these, plus `state`).
pub const SPEC_KEYS: &[&str] = &[
    "title",
    "emoji",
    "instructions",
    "schedule_kind",
    "run_at",
    "rule",
    "cron_expr",
    "tz",
    "starts_at",
    "end_at",
    "max_runs",
    "ask_before_acting",
    "notify_when_done",
    "approval_timeout_mins",
    "misfire",
    "max_retries",
];

fn blank(v: &Option<String>) -> Option<String> {
    v.as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl Spec {
    /// Read a spec from JSON (API body or tool arguments).
    pub fn from_json(v: &Value, default_tz: &str) -> Result<Self, String> {
        let mut obj = v
            .as_object()
            .cloned()
            .ok_or("I need the routine as an object.")?;
        obj.retain(|k, _| SPEC_KEYS.contains(&k.as_str()));
        let mut spec: Spec = serde_json::from_value(Value::Object(obj))
            .map_err(|e| format!("I couldn't read that routine: {e}."))?;
        if spec.tz.trim().is_empty() {
            spec.tz = default_tz.to_string();
        }
        Ok(spec)
    }

    /// Tidy the fields and check them; returns the schedule.
    pub fn validate(&mut self, now: i64) -> Result<Plan, String> {
        self.title = self.title.trim().to_string();
        self.emoji = self.emoji.trim().to_string();
        self.instructions = self.instructions.trim().to_string();
        if self.title.is_empty() {
            return Err("Give the routine a name.".into());
        }
        if self.title.chars().count() > MAX_TITLE {
            return Err(format!("Keep the name under {MAX_TITLE} characters."));
        }
        if self.emoji.chars().count() > 8 {
            return Err("The icon should be a single emoji.".into());
        }
        if self.instructions.is_empty() {
            return Err("Tell me what to do on each run.".into());
        }
        if self.instructions.chars().count() > MAX_INSTRUCTIONS {
            return Err(format!(
                "Keep the instructions under {MAX_INSTRUCTIONS} characters; attach longer \
                 notes as a document."
            ));
        }
        self.tz = self.tz.trim().to_string();
        self.run_at = blank(&self.run_at);
        self.cron_expr = blank(&self.cron_expr);
        self.starts_at = blank(&self.starts_at);
        self.end_at = blank(&self.end_at);
        match self.schedule_kind {
            Kind::Once => {
                self.rule = None;
                self.cron_expr = None;
                self.starts_at = None;
                self.end_at = None;
                self.max_runs = None;
            }
            Kind::Rule => {
                self.run_at = None;
                self.cron_expr = None;
            }
            Kind::Cron => {
                self.run_at = None;
                self.rule = None;
            }
        }
        if let Some(n) = self.max_runs
            && !(1..=1000).contains(&n)
        {
            return Err("\"After N runs\" has to be between 1 and 1000.".into());
        }
        if !(5..=1440).contains(&self.approval_timeout_mins) {
            return Err("I can wait between 5 minutes and 24 hours for your answer.".into());
        }
        if !(0..=3).contains(&self.max_retries) {
            return Err("I can retry a failed run up to 3 times.".into());
        }
        let tz = schedule::parse_tz(&self.tz)?;
        // An interval needs a first date to count from: the start date, or
        // today.
        if let Some(rule) = self.rule.as_mut()
            && rule.every > 1
            && rule.anchor.is_none()
        {
            rule.anchor = Some(match &self.starts_at {
                Some(s) => s.clone(),
                None => schedule::zoned(&tz, now)
                    .map(|z| z.date().to_string())
                    .unwrap_or_default(),
            });
        }
        let plan = self.plan()?;
        if let (Some(s), Some(e)) = (plan.starts, plan.ends)
            && e < s
        {
            return Err("The end date is before the start date.".into());
        }
        if matches!(plan.when, When::Cron(_)) {
            schedule::check_cron_gap(&plan, self.cron_expr.as_deref().unwrap_or(""), now)?;
        }
        Ok(plan)
    }

    /// The schedule (without the other checks in `validate`).
    pub fn plan(&self) -> Result<Plan, String> {
        let tz = schedule::parse_tz(&self.tz)?;
        let when = match self.schedule_kind {
            Kind::Once => When::Once(schedule::parse_local(
                self.run_at
                    .as_deref()
                    .ok_or("A one-time routine needs a date and time.")?,
            )?),
            Kind::Rule => When::Rule(schedule::ParsedRule::parse(
                self.rule
                    .as_ref()
                    .ok_or("A repeating routine needs a repeat rule.")?,
            )?),
            Kind::Cron => When::Cron(Box::new(schedule::parse_cron(
                self.cron_expr
                    .as_deref()
                    .ok_or("A custom routine needs a cron pattern.")?,
            )?)),
        };
        let date = |field: &str, v: &Option<String>| {
            v.as_deref()
                .map(|s| schedule::parse_date(field, s))
                .transpose()
        };
        Ok(Plan {
            when,
            tz,
            starts: date("the start date", &self.starts_at)?,
            ends: date("the end date", &self.end_at)?,
        })
    }

    /// One line for people, e.g. "Every Sat · 5:30 PM".
    pub fn schedule_text(&self) -> String {
        let Ok(plan) = self.plan() else {
            return "Not scheduled".into();
        };
        let mut text = match &plan.when {
            When::Once(dt) => schedule::describe_once(*dt),
            When::Rule(r) => schedule::describe_rule(r),
            When::Cron(c) => c.describe().trim_end_matches('.').to_string(),
        };
        if let Some(n) = self.max_runs {
            text.push_str(&format!(" · {n} time{}", if n == 1 { "" } else { "s" }));
        }
        text
    }

    fn timeout_text(&self) -> String {
        match self.approval_timeout_mins {
            m if m % 60 == 0 => {
                let h = m / 60;
                format!("{h} hour{}", if h == 1 { "" } else { "s" })
            }
            m => format!("{m} minutes"),
        }
    }
}

/// `next` slot and state after a run (or an edit) at `after`.
pub fn schedule_next(spec: &Spec, plan: &Plan, run_count: i64, after: i64) -> (State, Option<i64>) {
    if spec.max_runs.is_some_and(|m| run_count >= m) {
        return (State::Done, None);
    }
    match plan.next_after(after) {
        Some(t) => (State::Active, Some(t)),
        None => (State::Done, None),
    }
}

/// The owner's default time zone for new routines.
pub fn default_tz(conn: &Connection) -> String {
    db::get_setting(conn, TZ_SETTING)
        .ok()
        .flatten()
        .filter(|tz| schedule::parse_tz(tz).is_ok())
        .or_else(|| {
            jiff::tz::TimeZone::try_system()
                .ok()
                .and_then(|tz| tz.iana_name().map(String::from))
        })
        .unwrap_or_else(|| "UTC".into())
}

// ------------------------------------------------------------------ storage

#[derive(Debug, Clone)]
pub struct Routine {
    pub id: String,
    pub owner: String,
    pub spec: Spec,
    pub run_count: i64,
    pub state: State,
    pub next_run_at: Option<i64>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

fn routine_from_row(r: &rusqlite::Row) -> rusqlite::Result<Routine> {
    let rule: Option<String> = r.get("rule")?;
    let kind: String = r.get("schedule_kind")?;
    let misfire: String = r.get("misfire")?;
    Ok(Routine {
        id: r.get("id")?,
        owner: r.get("owner")?,
        spec: Spec {
            title: r.get("title")?,
            emoji: r.get("emoji")?,
            instructions: r.get("instructions")?,
            schedule_kind: match kind.as_str() {
                "once" => Kind::Once,
                "cron" => Kind::Cron,
                _ => Kind::Rule,
            },
            run_at: r.get("run_at")?,
            rule: rule.and_then(|s| serde_json::from_str(&s).ok()),
            cron_expr: r.get("cron_expr")?,
            tz: r.get("tz")?,
            starts_at: r.get("starts_at")?,
            end_at: r.get("end_at")?,
            max_runs: r.get("max_runs")?,
            ask_before_acting: r.get("ask_before_acting")?,
            notify_when_done: r.get("notify_when_done")?,
            approval_timeout_mins: r.get("approval_timeout_mins")?,
            misfire: if misfire == "skip" {
                Misfire::Skip
            } else {
                Misfire::RunLatestInWindow
            },
            max_retries: r.get("max_retries")?,
        },
        run_count: r.get("run_count")?,
        state: State::parse(&r.get::<_, String>("state")?),
        next_run_at: r.get("next_run_at")?,
        created_by: r.get("created_by")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

pub fn insert_routine(
    conn: &Connection,
    owner: &str,
    spec: &Spec,
    state: State,
    next_run_at: Option<i64>,
    created_by: &str,
) -> rusqlite::Result<Routine> {
    let id = new_id();
    let now = now_ms();
    conn.execute(
        "INSERT INTO routines (id, owner, title, emoji, instructions, schedule_kind, run_at,
             rule, cron_expr, tz, starts_at, end_at, max_runs, state, next_run_at,
             ask_before_acting, notify_when_done, approval_timeout_mins, misfire, max_retries,
             created_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20, ?21, ?22, ?22)",
        params![
            id,
            owner,
            spec.title,
            spec.emoji,
            spec.instructions,
            spec.schedule_kind.name(),
            spec.run_at,
            spec.rule.as_ref().map(|r| json!(r).to_string()),
            spec.cron_expr,
            spec.tz,
            spec.starts_at,
            spec.end_at,
            spec.max_runs,
            state.name(),
            next_run_at,
            spec.ask_before_acting,
            spec.notify_when_done,
            spec.approval_timeout_mins,
            spec.misfire.name(),
            spec.max_retries,
            created_by,
            now
        ],
    )?;
    get_routine(conn, owner, &id).map(|r| r.expect("just inserted"))
}

pub fn get_routine(conn: &Connection, owner: &str, id: &str) -> rusqlite::Result<Option<Routine>> {
    conn.query_row(
        "SELECT * FROM routines WHERE id = ?1 AND owner = ?2",
        params![id, owner],
        routine_from_row,
    )
    .optional()
}

pub fn get_routine_any(conn: &Connection, id: &str) -> rusqlite::Result<Option<Routine>> {
    conn.query_row(
        "SELECT * FROM routines WHERE id = ?1",
        [id],
        routine_from_row,
    )
    .optional()
}

pub fn list_routines(conn: &Connection, owner: &str) -> rusqlite::Result<Vec<Routine>> {
    let mut stmt =
        conn.prepare("SELECT * FROM routines WHERE owner = ?1 ORDER BY created_at, id")?;
    stmt.query_map([owner], routine_from_row)?.collect()
}

/// Save a routine's fields, state, and next run.
pub fn save_routine(
    conn: &Connection,
    id: &str,
    spec: &Spec,
    state: State,
    next_run_at: Option<i64>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE routines SET title = ?2, emoji = ?3, instructions = ?4, schedule_kind = ?5,
             run_at = ?6, rule = ?7, cron_expr = ?8, tz = ?9, starts_at = ?10, end_at = ?11,
             max_runs = ?12, state = ?13, next_run_at = ?14, ask_before_acting = ?15,
             notify_when_done = ?16, approval_timeout_mins = ?17, misfire = ?18,
             max_retries = ?19, updated_at = ?20
         WHERE id = ?1",
        params![
            id,
            spec.title,
            spec.emoji,
            spec.instructions,
            spec.schedule_kind.name(),
            spec.run_at,
            spec.rule.as_ref().map(|r| json!(r).to_string()),
            spec.cron_expr,
            spec.tz,
            spec.starts_at,
            spec.end_at,
            spec.max_runs,
            state.name(),
            next_run_at,
            spec.ask_before_acting,
            spec.notify_when_done,
            spec.approval_timeout_mins,
            spec.misfire.name(),
            spec.max_retries,
            now_ms()
        ],
    )?;
    Ok(())
}

/// What deleting a routine leaves to do.
#[derive(Debug, Default)]
pub struct Deleted {
    /// Cards it closed, to broadcast.
    pub cards: Vec<Message>,
    /// Its runs' conversations: a turn still going in one must stop now
    /// (`run::stop_turns`).
    pub stop: Vec<String>,
}

/// Delete a routine, its runs' hidden conversations, and close any
/// approval card still waiting on one of its runs.
pub fn delete_routine(
    conn: &Connection,
    owner: &str,
    id: &str,
) -> rusqlite::Result<Option<Deleted>> {
    db::write_tx(conn, |c| {
        if get_routine(c, owner, id)?.is_none() {
            return Ok(None);
        }
        let mut cards = Vec::new();
        let open: Vec<String> = c
            .prepare(
                "SELECT card_message_id FROM routine_runs
                 WHERE routine_id = ?1 AND status = 'waiting' AND card_message_id IS NOT NULL",
            )?
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for card in open {
            if let Some(m) = run::close_card(c, &card, "cancelled")? {
                cards.push(m);
            }
        }
        for m in set_change_cards(c, id, None, "cancelled")? {
            cards.push(m);
        }
        let stop: Vec<String> = c
            .prepare(
                "SELECT conversation_id FROM routine_runs
                 WHERE routine_id = ?1 AND conversation_id IS NOT NULL",
            )?
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        c.execute(
            "DELETE FROM conversations WHERE kind = 'job' AND id IN (
                 SELECT conversation_id FROM routine_runs WHERE routine_id = ?1)",
            [id],
        )?;
        c.execute("DELETE FROM routines WHERE id = ?1", [id])?;
        Ok(Some(Deleted { cards, stop }))
    })
}

// --------------------------------------------------------------------- docs

#[derive(Debug, Clone, Serialize)]
pub struct Doc {
    pub id: String,
    pub title: String,
    pub mime: String,
    pub size: i64,
    /// Characters of extracted text.
    pub chars: i64,
}

pub fn list_docs(conn: &Connection, routine_id: &str) -> rusqlite::Result<Vec<Doc>> {
    let mut stmt = conn.prepare(
        "SELECT l.id, l.title, l.mime, l.size, length(COALESCE(l.extracted_text, ''))
         FROM routine_docs d JOIN library_items l ON l.id = d.library_item_id
         WHERE d.routine_id = ?1 ORDER BY d.created_at, l.id",
    )?;
    stmt.query_map([routine_id], |r| {
        Ok(Doc {
            id: r.get(0)?,
            title: r.get(1)?,
            mime: r.get(2)?,
            size: r.get(3)?,
            chars: r.get(4)?,
        })
    })?
    .collect()
}

/// (title, text) of each document, in order.
pub fn doc_texts(conn: &Connection, routine_id: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT l.title, COALESCE(l.extracted_text, '')
         FROM routine_docs d JOIN library_items l ON l.id = d.library_item_id
         WHERE d.routine_id = ?1 ORDER BY d.created_at, l.id",
    )?;
    stmt.query_map([routine_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// A document may be attached: the owner's Library item, with text read
/// when it was uploaded.
pub fn check_doc(conn: &Connection, owner: &str, id: &str) -> Result<(), String> {
    let has_text: Option<bool> = conn
        .query_row(
            "SELECT extracted_text IS NOT NULL FROM library_items WHERE id = ?1 AND user_id = ?2",
            params![id, owner],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    match has_text {
        None => Err("I couldn't find that document.".into()),
        Some(false) => Err("I can only attach documents uploaded for routines.".into()),
        Some(true) => Ok(()),
    }
}

/// Make the routine's documents exactly `ids`.
/// Check documents can be attached (each an uploaded document of the
/// owner's still in the Library).
pub fn check_docs(conn: &Connection, owner: &str, ids: &[String]) -> Result<(), String> {
    for id in ids {
        check_doc(conn, owner, id)?;
    }
    Ok(())
}

pub fn set_docs(
    conn: &Connection,
    owner: &str,
    routine_id: &str,
    ids: &[String],
) -> Result<(), String> {
    check_docs(conn, owner, ids)?;
    db::write_tx(conn, |c| {
        for d in list_docs(c, routine_id)? {
            if !ids.contains(&d.id) {
                unlink_doc(c, routine_id, &d.id)?;
            }
        }
        for id in ids {
            c.execute(
                "INSERT OR IGNORE INTO routine_docs (routine_id, library_item_id, created_at)
                 VALUES (?1, ?2, ?3)",
                params![routine_id, id, now_ms()],
            )?;
        }
        Ok(())
    })
    .map_err(|e| e.to_string())
}

pub fn unlink_doc(conn: &Connection, routine_id: &str, doc_id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM routine_docs WHERE routine_id = ?1 AND library_item_id = ?2",
        params![routine_id, doc_id],
    )? > 0)
}

// ------------------------------------------------------------------ changes

#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub id: String,
    pub routine_id: String,
    pub action: String,
    pub patch: Value,
    pub summary: String,
    pub state: String,
    pub created_at: i64,
}

fn change_from_row(r: &rusqlite::Row) -> rusqlite::Result<Change> {
    let patch: String = r.get("patch")?;
    Ok(Change {
        id: r.get("id")?,
        routine_id: r.get("routine_id")?,
        action: r.get("action")?,
        patch: serde_json::from_str(&patch).unwrap_or_else(|_| json!({})),
        summary: r.get("summary")?,
        state: r.get("state")?,
        created_at: r.get("created_at")?,
    })
}

pub fn get_change(conn: &Connection, id: &str) -> rusqlite::Result<Option<Change>> {
    conn.query_row(
        "SELECT * FROM routine_changes WHERE id = ?1",
        [id],
        change_from_row,
    )
    .optional()
}

pub fn pending_change(conn: &Connection, routine_id: &str) -> rusqlite::Result<Option<Change>> {
    conn.query_row(
        "SELECT * FROM routine_changes WHERE routine_id = ?1 AND state = 'pending'
         ORDER BY created_at DESC LIMIT 1",
        [routine_id],
        change_from_row,
    )
    .optional()
}

/// Apply a patch (JSON object of spec fields, plus optional `state`
/// "active" or "paused") to a spec.
pub fn apply_patch(spec: &Spec, patch: &Value) -> Result<(Spec, Option<State>), String> {
    let obj = patch
        .as_object()
        .ok_or("I need the changes as an object.")?;
    let mut merged = serde_json::to_value(spec).map_err(|e| e.to_string())?;
    let mut state = None;
    for (k, v) in obj {
        match k.as_str() {
            "state" => {
                state = Some(match v.as_str() {
                    Some("active") => State::Active,
                    Some("paused") => State::Paused,
                    _ => return Err("A routine can only be set to active or paused.".into()),
                })
            }
            k if SPEC_KEYS.contains(&k) => merged[k] = v.clone(),
            other => return Err(format!("I don't know the routine field \"{other}\".")),
        }
    }
    let spec =
        serde_json::from_value(merged).map_err(|e| format!("I couldn't read that change: {e}."))?;
    Ok((spec, state))
}

/// What a change does, in a few words: "Time 5:30 PM → 6:00 PM, adds
/// Sundays".
pub fn diff_summary(old: &Spec, new: &Spec, old_state: State, new_state: State) -> String {
    let mut parts: Vec<String> = Vec::new();
    if old.title != new.title {
        parts.push(format!("name “{}” → “{}”", old.title, new.title));
    }
    if old.emoji != new.emoji && parts.is_empty() {
        parts.push("new icon".into());
    }
    if old.instructions != new.instructions {
        parts.push("updates the instructions".into());
    }
    let sched = |s: &Spec| s.schedule_text();
    match (&old.rule, &new.rule) {
        (Some(a), Some(b))
            if old.schedule_kind == Kind::Rule && new.schedule_kind == Kind::Rule =>
        {
            if a.time != b.time {
                let t = |r: &Rule| {
                    schedule::parse_time(&r.time)
                        .map(schedule::time_text)
                        .unwrap_or_else(|_| r.time.clone())
                };
                parts.push(format!("time {} → {}", t(a), t(b)));
            }
            let same_shape = a.every == b.every
                && a.anchor == b.anchor
                && std::mem::discriminant(&a.repeat) == std::mem::discriminant(&b.repeat);
            match (&a.repeat, &b.repeat) {
                (schedule::Repeat::Weekly { days: da }, schedule::Repeat::Weekly { days: db })
                    if same_shape =>
                {
                    let name = |d: &u8| format!("{}s", schedule::day_name(*d));
                    let added: Vec<String> =
                        db.iter().filter(|d| !da.contains(d)).map(name).collect();
                    let removed: Vec<String> =
                        da.iter().filter(|d| !db.contains(d)).map(name).collect();
                    if !added.is_empty() {
                        parts.push(format!("adds {}", added.join(" and ")));
                    }
                    if !removed.is_empty() {
                        parts.push(format!("drops {}", removed.join(" and ")));
                    }
                }
                (ra, rb) if ra != rb || !same_shape => {
                    parts.push(format!("schedule {} → {}", sched(old), sched(new)));
                }
                _ => {}
            }
        }
        _ => {
            if old.schedule_kind != new.schedule_kind
                || old.run_at != new.run_at
                || old.cron_expr != new.cron_expr
            {
                parts.push(format!("schedule {} → {}", sched(old), sched(new)));
            }
        }
    }
    if old.tz != new.tz {
        parts.push(format!("time zone {} → {}", old.tz, new.tz));
    }
    if old.starts_at != new.starts_at {
        parts.push(match &new.starts_at {
            Some(d) => format!("starts {d}"),
            None => "starts right away".into(),
        });
    }
    let ends = |s: &Spec| match (&s.end_at, s.max_runs) {
        (Some(d), _) => format!("on {d}"),
        (None, Some(n)) => format!("after {n} run{}", if n == 1 { "" } else { "s" }),
        (None, None) => "never".into(),
    };
    if (old.end_at.clone(), old.max_runs) != (new.end_at.clone(), new.max_runs) {
        parts.push(format!("ends {} → {}", ends(old), ends(new)));
    }
    if old.ask_before_acting != new.ask_before_acting {
        parts.push(if new.ask_before_acting {
            "asks you before acting".into()
        } else {
            "acts without asking you first".into()
        });
    }
    if old.notify_when_done != new.notify_when_done {
        parts.push(if new.notify_when_done {
            "tells you when done".into()
        } else {
            "stops telling you when done".into()
        });
    }
    if old.approval_timeout_mins != new.approval_timeout_mins {
        parts.push(format!(
            "waits {} → {} for your answer",
            old.timeout_text(),
            new.timeout_text()
        ));
    }
    if old.misfire != new.misfire {
        parts.push(match new.misfire {
            Misfire::Skip => "skips runs missed while I was offline".into(),
            Misfire::RunLatestInWindow => "catches up on a run missed within the hour".into(),
        });
    }
    if old.max_retries != new.max_retries {
        parts.push(format!(
            "retries {} → {} times",
            old.max_retries, new.max_retries
        ));
    }
    if old_state != new_state {
        match new_state {
            State::Paused => parts.push("pauses it".into()),
            State::Active => parts.push("resumes it".into()),
            _ => {}
        }
    }
    let mut out = parts.join(", ");
    if let Some(first) = out.get(..1) {
        out = first.to_uppercase() + &out[1..];
    }
    out
}

fn create_summary(spec: &Spec) -> String {
    format!(
        "{}{}",
        spec.schedule_text(),
        if spec.ask_before_acting {
            " · asks before acting"
        } else {
            ""
        }
    )
}

/// The chat card for a proposed change.
fn change_card(routine: &Routine, spec: &Spec, change: &Change, owner: &str, now: i64) -> Value {
    let next = spec
        .plan()
        .ok()
        .and_then(|p| {
            let remaining = spec.max_runs.map(|m| m - routine.run_count);
            p.upcoming(now, 1, remaining)
                .first()
                .map(|t| p.local_text(*t))
        })
        .unwrap_or_default();
    json!({
        "type": "routine",
        "change_id": change.id,
        "routine_id": routine.id,
        "action": change.action,
        "title": spec.title,
        "emoji": spec.emoji,
        "body": change.summary,
        "schedule": spec.schedule_text(),
        "next": next,
        "asks_first": spec.ask_before_acting,
        "approver_user_id": owner,
        "state": "pending",
    })
}

fn insert_change(
    conn: &Connection,
    routine_id: &str,
    action: &str,
    patch: &Value,
    summary: &str,
) -> rusqlite::Result<Change> {
    // A newer proposal replaces an older one for the same routine.
    set_change_cards(conn, routine_id, Some("pending"), "superseded")?;
    conn.execute(
        "UPDATE routine_changes SET state = 'superseded', decided_at = ?2
         WHERE routine_id = ?1 AND state = 'pending'",
        params![routine_id, now_ms()],
    )?;
    let id = new_id();
    conn.execute(
        "INSERT INTO routine_changes (id, routine_id, action, patch, summary, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, routine_id, action, patch.to_string(), summary, now_ms()],
    )?;
    get_change(conn, &id).map(|c| c.expect("just inserted"))
}

/// Set the state of the cards of a routine's changes (all, or those in
/// `only` state); returns the updated card messages.
fn set_change_cards(
    conn: &Connection,
    routine_id: &str,
    only: Option<&str>,
    to: &str,
) -> rusqlite::Result<Vec<Message>> {
    let ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM messages WHERE kind = 'card'
               AND json_extract(card, '$.type') = 'routine'
               AND json_extract(card, '$.routine_id') = ?1
               AND json_extract(card, '$.state') = 'pending'
               AND (?2 IS NULL OR json_extract(card, '$.change_id') IN (
                   SELECT id FROM routine_changes WHERE routine_id = ?1 AND state = ?2))",
        )?
        .query_map(params![routine_id, only], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    set_cards(conn, &ids, to)
}

fn set_cards(conn: &Connection, ids: &[String], to: &str) -> rusqlite::Result<Vec<Message>> {
    let mut out = Vec::new();
    for id in ids {
        if let Some(m) = db::get_message(conn, id)? {
            let mut card = m.card.unwrap_or_default();
            card["state"] = to.into();
            card["decided_at"] = now_ms().into();
            out.push(db::update_message_card(conn, id, &card)?);
        }
    }
    Ok(out)
}

fn change_card_ids(conn: &Connection, change_id: &str) -> rusqlite::Result<Vec<String>> {
    conn.prepare(
        "SELECT id FROM messages WHERE kind = 'card'
           AND json_extract(card, '$.type') = 'routine'
           AND json_extract(card, '$.change_id') = ?1",
    )?
    .query_map([change_id], |r| r.get(0))?
    .collect()
}

/// A proposal from Iris and the card that asks the owner about it.
pub struct Proposal {
    pub routine: Routine,
    pub change: Change,
    pub card: Message,
}

/// Where a proposal's card goes and how it is posted (`pending` cards in
/// the owner's own chat are published with the turn's reply).
pub struct CardTarget<'a> {
    pub conversation_id: &'a str,
    pub status: &'a str,
}

fn post_change_card(
    conn: &Connection,
    target: &CardTarget,
    routine: &Routine,
    spec: &Spec,
    change: &Change,
) -> rusqlite::Result<Message> {
    let card = change_card(routine, spec, change, &routine.owner, now_ms());
    entities::insert_card_with_status(
        conn,
        target.conversation_id,
        &card,
        "assistant",
        target.status,
    )
}

/// Iris proposes a new routine: stored as `pending_approval` until the
/// owner taps "Set it up".
pub fn propose_create(
    conn: &Connection,
    owner: &str,
    mut spec: Spec,
    target: &CardTarget,
) -> Result<Proposal, String> {
    spec.validate(now_ms())?;
    db::write_tx(conn, |c| {
        let routine = insert_routine(c, owner, &spec, State::PendingApproval, None, "iris")?;
        let change = insert_change(c, &routine.id, "create", &json!({}), &create_summary(&spec))?;
        let card = post_change_card(c, target, &routine, &spec, &change)?;
        Ok(Proposal {
            routine,
            change,
            card,
        })
    })
    .map_err(|e| e.to_string())
}

/// Iris proposes changing a routine (`patch`: spec fields and optionally
/// `state`). A routine still waiting for its first OK is replaced by a new
/// create proposal.
pub fn propose_update(
    conn: &Connection,
    owner: &str,
    id: &str,
    patch: &Value,
    target: &CardTarget,
) -> Result<Proposal, String> {
    let now = now_ms();
    let routine = find(conn, owner, id)?;
    let (mut spec, state) = apply_patch(&routine.spec, patch)?;
    spec.validate(now)?;
    if routine.state == State::PendingApproval {
        return db::write_tx(conn, |c| {
            save_routine(c, &routine.id, &spec, State::PendingApproval, None)?;
            let routine = get_routine(c, owner, id)?.expect("exists");
            let change = insert_change(c, id, "create", &json!({}), &create_summary(&spec))?;
            let card = post_change_card(c, target, &routine, &spec, &change)?;
            Ok(Proposal {
                routine,
                change,
                card,
            })
        })
        .map_err(|e| e.to_string());
    }
    let new_state = match (routine.state, state) {
        (_, Some(s)) => s,
        (s, None) => s,
    };
    let summary = diff_summary(&routine.spec, &spec, routine.state, new_state);
    if summary.is_empty() {
        return Err("That wouldn't change anything.".into());
    }
    // Store only what changes, so edits the owner makes meanwhile are kept.
    let mut stored = serde_json::Map::new();
    let new_json = json!(spec);
    let old_json = json!(routine.spec);
    for k in SPEC_KEYS {
        if new_json[k] != old_json[k] {
            stored.insert(k.to_string(), new_json[k].clone());
        }
    }
    if new_state != routine.state {
        stored.insert("state".into(), new_state.name().into());
    }
    db::write_tx(conn, |c| {
        let change = insert_change(c, id, "update", &Value::Object(stored.clone()), &summary)?;
        let card = post_change_card(c, target, &routine, &spec, &change)?;
        Ok(Proposal {
            routine: routine.clone(),
            change,
            card,
        })
    })
    .map_err(|e| e.to_string())
}

/// Iris proposes deleting a routine.
pub fn propose_delete(
    conn: &Connection,
    owner: &str,
    id: &str,
    target: &CardTarget,
) -> Result<Proposal, String> {
    let routine = find(conn, owner, id)?;
    db::write_tx(conn, |c| {
        let change = insert_change(
            c,
            id,
            "delete",
            &json!({}),
            "Stops it for good and removes its history.",
        )?;
        let card = post_change_card(c, target, &routine, &routine.spec, &change)?;
        Ok(Proposal {
            routine: routine.clone(),
            change,
            card,
        })
    })
    .map_err(|e| e.to_string())
}

fn find(conn: &Connection, owner: &str, id: &str) -> Result<Routine, String> {
    get_routine(conn, owner, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "I couldn't find that routine.".to_string())
}

/// The result of deciding a change: the routine afterwards (None when it
/// was deleted) and the updated cards to broadcast.
pub struct Decided {
    pub routine: Option<Routine>,
    pub cards: Vec<Message>,
    /// Conversations whose turns must stop (a deleted routine's runs).
    pub stop: Vec<String>,
}

/// The owner approves a proposed change. `edited` replaces a proposed new
/// routine's fields (the owner edited it before setting it up).
pub fn approve_change(
    conn: &Connection,
    owner: &str,
    change_id: &str,
    edited: Option<Spec>,
    now: i64,
) -> Result<Decided, String> {
    let fail = |e: rusqlite::Error| e.to_string();
    let change = get_change(conn, change_id)
        .map_err(fail)?
        .ok_or("I couldn't find that suggestion.")?;
    if change.state != "pending" {
        return Err("This was already decided or replaced by a newer suggestion.".into());
    }
    let routine = find(conn, owner, &change.routine_id)?;
    let (spec, state) = match change.action.as_str() {
        "create" => (edited.unwrap_or_else(|| routine.spec.clone()), None),
        "update" => apply_patch(&routine.spec, &change.patch)?,
        _ => (routine.spec.clone(), None),
    };
    let mut spec = spec;
    if change.action != "delete" {
        spec.validate(now)?;
    }
    db::write_tx(conn, |c| {
        c.execute(
            "UPDATE routine_changes SET state = 'approved', decided_at = ?2 WHERE id = ?1",
            params![change.id, now],
        )?;
        let cards = set_cards(c, &change_card_ids(c, &change.id)?, "approved")?;
        if change.action == "delete" {
            let mut cards = cards;
            let deleted = delete_routine(c, owner, &routine.id)?.unwrap_or_default();
            cards.extend(deleted.cards);
            return Ok(Decided {
                routine: None,
                cards,
                stop: deleted.stop,
            });
        }
        let target = match (change.action.as_str(), state, routine.state) {
            ("create", _, _) => State::Active,
            (_, Some(s), _) => s,
            (_, None, s) => s,
        };
        let (state, next) = settle(&spec, target, routine.run_count, now);
        save_routine(c, &routine.id, &spec, state, next)?;
        Ok(Decided {
            routine: get_routine(c, owner, &routine.id)?,
            cards,
            stop: Vec::new(),
        })
    })
    .map_err(fail)
}

/// The owner declines a proposal. A declined new routine is removed.
pub fn decline_change(conn: &Connection, owner: &str, change_id: &str) -> Result<Decided, String> {
    let fail = |e: rusqlite::Error| e.to_string();
    let change = get_change(conn, change_id)
        .map_err(fail)?
        .ok_or("I couldn't find that suggestion.")?;
    if change.state != "pending" {
        return Err("This was already decided or replaced by a newer suggestion.".into());
    }
    let routine = find(conn, owner, &change.routine_id)?;
    db::write_tx(conn, |c| {
        c.execute(
            "UPDATE routine_changes SET state = 'declined', decided_at = ?2 WHERE id = ?1",
            params![change.id, now_ms()],
        )?;
        let cards = set_cards(c, &change_card_ids(c, &change.id)?, "declined")?;
        if change.action == "create" && routine.state == State::PendingApproval {
            c.execute("DELETE FROM routines WHERE id = ?1", [&routine.id])?;
            return Ok(Decided {
                routine: None,
                cards,
                stop: Vec::new(),
            });
        }
        Ok(Decided {
            routine: Some(routine.clone()),
            cards,
            stop: Vec::new(),
        })
    })
    .map_err(fail)
}

/// State and next run for a routine that should be in `want` state now.
pub fn settle(spec: &Spec, want: State, run_count: i64, now: i64) -> (State, Option<i64>) {
    match want {
        State::Active | State::Done => match spec.plan() {
            Ok(plan) => schedule_next(spec, &plan, run_count, now),
            Err(_) => (State::Paused, None),
        },
        other => (other, None),
    }
}

// --------------------------------------------------------------------- JSON

/// A routine as the API and the agent see it.
pub fn routine_json(conn: &Connection, r: &Routine, now: i64) -> rusqlite::Result<Value> {
    let mut v = json!(r.spec);
    let plan = r.spec.plan().ok();
    let next = r.next_run_at.or_else(|| {
        // Not yet approved: show when it would run.
        (r.state == State::PendingApproval)
            .then(|| plan.as_ref().and_then(|p| p.next_after(now)))
            .flatten()
    });
    let upcoming: Vec<Value> = match (&plan, next) {
        (Some(p), Some(first)) if r.state != State::Paused => {
            let remaining = r.spec.max_runs.map(|m| m - r.run_count);
            p.upcoming(first - 1, 3, remaining)
                .into_iter()
                .map(|t| json!({"at": t, "text": p.local_text(t)}))
                .collect()
        }
        _ => Vec::new(),
    };
    let last: Option<Value> = conn
        .query_row(
            "SELECT status, reason, summary, scheduled_for, finished_at FROM routine_runs
             WHERE routine_id = ?1 AND status IN ('ok', 'skipped', 'failed')
             ORDER BY scheduled_for DESC LIMIT 1",
            [&r.id],
            |row| {
                Ok(json!({
                    "status": row.get::<_, String>(0)?,
                    "reason": row.get::<_, String>(1)?,
                    "summary": row.get::<_, String>(2)?,
                    "scheduled_for": row.get::<_, i64>(3)?,
                    "finished_at": row.get::<_, Option<i64>>(4)?,
                }))
            },
        )
        .optional()?;
    let open: Option<Value> = conn
        .query_row(
            "SELECT id, status, card_message_id, expires_at FROM routine_runs
             WHERE routine_id = ?1 AND status IN ('running', 'waiting')
             ORDER BY scheduled_for DESC LIMIT 1",
            [&r.id],
            |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "status": row.get::<_, String>(1)?,
                    "card_message_id": row.get::<_, Option<String>>(2)?,
                    "expires_at": row.get::<_, Option<i64>>(3)?,
                }))
            },
        )
        .optional()?;
    let pending = pending_change(conn, &r.id)?.map(|c| {
        json!({"id": c.id, "action": c.action, "summary": c.summary, "created_at": c.created_at})
    });
    v["id"] = r.id.clone().into();
    v["state"] = r.state.name().into();
    v["next_run_at"] = json!(r.next_run_at);
    v["next_run_text"] = json!(next.zip(plan.as_ref()).map(|(t, p)| p.local_text(t)));
    v["upcoming"] = upcoming.into();
    v["schedule_text"] = r.spec.schedule_text().into();
    v["run_count"] = r.run_count.into();
    v["created_by"] = r.created_by.clone().into();
    v["created_at"] = r.created_at.into();
    v["updated_at"] = r.updated_at.into();
    v["docs"] = json!(list_docs(conn, &r.id)?);
    v["last_run"] = json!(last);
    v["open_run"] = json!(open);
    v["pending_change"] = json!(pending);
    Ok(v)
}

/// A short view of a routine for the agent (`list_routines`).
pub fn agent_json(conn: &Connection, r: &Routine, now: i64) -> rusqlite::Result<Value> {
    let full = routine_json(conn, r, now)?;
    Ok(json!({
        "id": r.id,
        "title": r.spec.title,
        "emoji": r.spec.emoji,
        "state": r.state.name(),
        "schedule": r.spec.schedule_text(),
        "next_run": full["next_run_text"],
        "instructions": truncate_chars(&r.spec.instructions, 600),
        "fields": json!(r.spec),
        "documents": full["docs"].as_array().map(|d| d.iter().map(|x| x["title"].clone()).collect::<Vec<_>>()),
        "last_run": full["last_run"],
        "pending_change": full["pending_change"],
    }))
}

#[cfg(test)]
mod tests;
