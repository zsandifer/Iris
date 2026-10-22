//! Scheduled background jobs. They are the owner's own individual use, so
//! any signed-in engine may run them.
//!
//! - heartbeat: a `Purpose::Scheduled` turn on the `background` slot reads
//!   HEARTBEAT.md and, only when something needs attention, posts a message
//!   in the owner's Main chat (and pushes it).
//! - dream: bounded private-memory consolidation through a tool-free CLI
//!   helper, with history and morning-card data.
//! - feed: the For you pipeline (`crate::feed`): mostly code, plus at most
//!   two small model calls a day. Publishes one batch per local day.
//! - routines: the owner's own scheduled jobs (`crate::routines`), checked
//!   on the same minute tick.

use crate::config::{JobConfig, parse_hhmm};
use crate::db::{self, NewMessage};
use crate::entities;
use crate::router::Purpose;
use crate::state::AppState;
use crate::turn::{TurnInput, run_turn};
use crate::util::{now_ms, truncate_chars};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const HEARTBEAT_OK: &str = "HEARTBEAT_OK";

static HEARTBEAT_RUNNING: AtomicBool = AtomicBool::new(false);
static DREAM_RUNNING: AtomicBool = AtomicBool::new(false);
static FEED_RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Heartbeat,
    Feed,
    Dream,
}

impl Job {
    pub const ALL: [Job; 3] = [Job::Heartbeat, Job::Feed, Job::Dream];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "heartbeat" => Some(Job::Heartbeat),
            "feed" => Some(Job::Feed),
            "dream" => Some(Job::Dream),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Job::Heartbeat => "heartbeat",
            Job::Feed => "feed",
            Job::Dream => "dream",
        }
    }
    async fn effective_cfg(self, state: &AppState) -> JobConfig {
        match self {
            Job::Heartbeat => state.heartbeat_config().await,
            Job::Feed => {
                let mut cfg = state.cfg.feed.clone();
                cfg.enabled = crate::feed::enabled(state).await;
                cfg
            }
            Job::Dream => JobConfig {
                enabled: state.cfg.dream.enabled
                    && state.setting(crate::memory::dream::PAUSED).await.as_deref() != Some("true"),
                interval_mins: 1440,
                active_start: format!("{:02}:00", state.cfg.dream.hour),
                active_end: format!("{:02}:00", (state.cfg.dream.hour + 1) % 24),
            },
        }
    }
    fn flag(self) -> &'static AtomicBool {
        match self {
            Job::Heartbeat => &HEARTBEAT_RUNNING,
            Job::Feed => &FEED_RUNNING,
            Job::Dream => &DREAM_RUNNING,
        }
    }
    fn last_run_key(self) -> &'static str {
        match self {
            Job::Heartbeat => "job.heartbeat.last_run",
            Job::Feed => "job.feed.last_run",
            Job::Dream => "job.dream.last_run",
        }
    }
}

/// Start a job in the background unless it is already running. `manual`
/// runs (from the app) skip the feed's once-a-day check.
pub fn trigger(state: &AppState, job: Job, manual: bool) -> bool {
    if job.flag().swap(true, Ordering::SeqCst) {
        return false;
    }
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = run_job(&state, job, manual).await {
            crate::warn!("{} job failed: {e}", job.name());
        }
        job.flag().store(false, Ordering::SeqCst);
        if job == Job::Feed {
            // Tell open apps the run finished (the feed screen shows it).
            state.hub.send(
                crate::events::Audience::All,
                json!({"type": "entities.changed", "kinds": ["feed"]}),
            );
        }
    });
    true
}

pub fn is_running(job: Job) -> bool {
    job.flag().load(Ordering::SeqCst)
}

pub async fn status(state: &AppState) -> Vec<Value> {
    let mut out = Vec::new();
    for job in Job::ALL {
        let cfg = job.effective_cfg(state).await;
        let last = state
            .setting(job.last_run_key())
            .await
            .and_then(|s| s.parse::<i64>().ok());
        out.push(json!({
            "name": job.name(),
            "enabled": cfg.enabled,
            "interval_mins": cfg.interval_mins,
            "active_start": cfg.active_start,
            "active_end": cfg.active_end,
            "last_run_at": last,
            "running": job.flag().load(Ordering::SeqCst),
        }));
    }
    out
}

/// Minutes since local midnight.
pub fn local_minutes_now() -> u32 {
    let t = (now_ms() / 1000) as libc::time_t;
    // SAFETY: localtime_r writes into the provided struct only.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok {
        (tm.tm_hour as u32) * 60 + tm.tm_min as u32
    } else {
        ((now_ms() / 60_000) % 1440) as u32
    }
}

/// Local calendar day of `ms`, e.g. "2026-10-11".
pub fn local_day(ms: i64) -> String {
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes into the provided struct only.
    if unsafe { libc::localtime_r(&t, &mut tm).is_null() } {
        return crate::util::rfc3339(ms / 1000)[..10].to_string();
    }
    format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

/// Local date and time, e.g. "2026-10-11 14:05 (local time)".
fn local_timestamp() -> String {
    let t = (now_ms() / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::localtime_r(&t, &mut tm).is_null() } {
        return crate::util::rfc3339(now_ms() / 1000);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} (local time)",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// Whether `now` (minutes since midnight) is inside [start, end); windows
/// may wrap past midnight.
pub fn in_window(now: u32, start: u32, end: u32) -> bool {
    if start == end {
        true
    } else if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

fn due(cfg: &JobConfig, last_run: Option<i64>, now: i64, local_minutes: u32) -> bool {
    if !cfg.enabled {
        return false;
    }
    let (Some(start), Some(end)) = (parse_hhmm(&cfg.active_start), parse_hhmm(&cfg.active_end))
    else {
        return false;
    };
    in_window(local_minutes, start, end)
        && last_run.is_none_or(|l| now - l >= cfg.interval_mins as i64 * 60_000)
}

/// Whether the scheduler should start `job` now. Reads the effective
/// settings on every tick, so app toggles apply without a restart.
async fn job_due(state: &AppState, job: Job, now: i64, local_minutes: u32) -> bool {
    let last = state
        .setting(job.last_run_key())
        .await
        .and_then(|s| s.parse::<i64>().ok());
    let cfg = job.effective_cfg(state).await;
    if job == Job::Dream {
        crate::memory::dream::due(&state.cfg.dream, last, now, local_minutes) && cfg.enabled
    } else {
        due(&cfg, last, now, local_minutes)
    }
}

/// A periodic engine probe is running.
static PROBING: AtomicBool = AtomicBool::new(false);

/// Background loop: checks jobs every minute and re-probes harness sign-in
/// state every 15 minutes.
pub fn spawn_scheduler(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            n += 1;
            if n.is_multiple_of(15) && !PROBING.swap(true, Ordering::SeqCst) {
                // Probes can take a while (each command has its own
                // timeout); never hold up due jobs for them.
                let st = state.clone();
                tokio::spawn(async move {
                    st.refresh_harnesses().await;
                    PROBING.store(false, Ordering::SeqCst);
                });
            }
            // New models (codex cache, CLI lists) and sign-in changes move
            // automatic slots; announce it.
            state.check_recommendations().await;
            // Updates download for minutes; never hold up jobs or the next
            // tick (at most one runs a day, and each engine only once).
            let st = state.clone();
            tokio::spawn(async move { crate::setup::maybe_auto_update(&st).await });
            crate::gateway::reap_idle(&state).await;
            let paths = state.paths.clone();
            match state
                .db
                .call(move |c| crate::memory::lifecycle::maintain(c, &paths, now_ms()))
                .await
            {
                Ok(n) if n > 0 => state.hub.send(
                    crate::events::Audience::Owner,
                    json!({"type":"entities.changed","kinds":["memory"]}),
                ),
                Err(_) => crate::warn!("memory housekeeping failed"),
                _ => (),
            }
            if state.is_paused().await {
                continue;
            }
            // Notifications held during quiet hours go out once they end.
            crate::notify::spawn_flush_quiet(&state);
            crate::routines::run::tick(&state).await;
            for job in Job::ALL {
                if job_due(&state, job, now_ms(), local_minutes_now()).await {
                    trigger(&state, job, false);
                }
            }
        }
    });
}

/// What one check-in compares: the agent's working notes, the open goals
/// with their targets, what it held back last time, and the scorecard.
#[derive(Debug, Default)]
pub struct HeartbeatContext {
    pub notes: Vec<crate::proactive::WorkingNote>,
    pub goals: Vec<crate::entities::Goal>,
    pub held: Vec<crate::proactive::ProactiveMessage>,
    pub scorecard: crate::proactive::Scorecard,
    pub max_per_day: u64,
    pub sent_today: i64,
}

pub fn heartbeat_context(
    c: &rusqlite::Connection,
    owner: &str,
    now: i64,
    max_per_day: u64,
) -> rusqlite::Result<HeartbeatContext> {
    Ok(HeartbeatContext {
        notes: crate::proactive::list_notes(c, owner)?,
        goals: crate::entities::list_goals(c, owner, false)?,
        held: crate::proactive::held_recent(c, owner, now)?,
        scorecard: crate::proactive::scorecard(c, owner, now)?,
        max_per_day,
        sent_today: crate::proactive::posted_today(c, owner, now)?,
    })
}

pub fn heartbeat_prompt(state: &AppState, ctx: &HeartbeatContext) -> String {
    let checklist =
        std::fs::read_to_string(state.paths.persona_dir().join("HEARTBEAT.md")).unwrap_or_default();
    let mut out = format!(
        "Scheduled check-in at {}.\n\nThis is a comparison of plan against reality, \
         not a status report. Follow the procedure below. Use your working notes, \
         memory and connected tools as needed.\n\n<heartbeat>\n{}\n</heartbeat>\n",
        local_timestamp(),
        truncate_chars(checklist.trim(), 20_000)
    );
    out.push_str("\n## Your working notes (the plan)\n");
    if ctx.notes.is_empty() {
        out.push_str(
            "\nYou have no working notes yet. Start one per domain the user has asked you \
             to look after (see PROACTIVE_PREFERENCES.md and your memory) with \
             `update_working_note`, even if all it holds for now is the plan and what \
             you will watch.\n",
        );
    } else {
        out.push_str(&crate::proactive::render_notes(
            &ctx.notes,
            crate::proactive::HEARTBEAT_NOTES_CHARS,
        ));
    }
    out.push_str("\n## Open goals\n");
    if ctx.goals.is_empty() {
        out.push_str("\n(No open goals.)\n");
    } else {
        for g in ctx.goals.iter().take(50) {
            out.push_str(&format!("\n- **{}**", g.title));
            if g.suggested {
                out.push_str(" (suggested, not confirmed yet)");
            }
            if let Some(d) = &g.due {
                out.push_str(&format!(" · due {d}"));
            }
            if !g.target.is_empty() {
                out.push_str(&format!("\n  Target: {}", truncate_chars(&g.target, 400)));
            }
            if !g.status_text.is_empty() {
                out.push_str(&format!(
                    "\n  Status: {}",
                    truncate_chars(&g.status_text, 400)
                ));
            }
        }
        out.push('\n');
    }
    if !ctx.held.is_empty() {
        out.push_str(
            "\n## Held back\n\nThese messages were not sent because the day's budget was \
             spent. Raise one again only if it still matters now.\n",
        );
        for h in &ctx.held {
            out.push_str(&format!("\n- {}", h.excerpt.replace('\n', " ")));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "\n## Scorecard\n\n{}\nToday: {} of {} messages used.\n",
        ctx.scorecard.render(),
        ctx.sent_today,
        ctx.max_per_day
    ));
    out.push_str(&format!(
        "\n## Reply\n\nFirst update your working notes with `update_working_note` (every \
         run). Then, if nothing needs the user, reply with exactly {HEARTBEAT_OK} and \
         nothing else. Otherwise reply with the one short message you want to send \
         (it appears in their Main chat and as a notification): what you did, what \
         you noticed, and what you recommend, answerable with yes or no."
    ));
    out
}

pub async fn run_job(state: &AppState, job: Job, manual: bool) -> crate::util::Result<()> {
    if state.is_paused().await || state.stopping.load(Ordering::SeqCst) {
        return Ok(());
    }
    let owner = state
        .db
        .call(db::owner_user_id)
        .await?
        .ok_or("no owner user")?;
    if job == Job::Feed {
        // Recorded even when the run fails or finds nothing to do, so the
        // scheduler checks again after `interval_mins`, not every minute.
        let started = now_ms();
        let key = job.last_run_key();
        state
            .db
            .call(move |c| db::set_setting(c, key, &started.to_string()))
            .await?;
        crate::feed::run(state, &owner, manual).await?;
        return Ok(());
    }
    if job == Job::Dream {
        return crate::memory::dream::run(state, &owner, manual).await;
    }
    let max_per_day = state.heartbeat_max_per_day().await;
    let (o, now) = (owner.clone(), now_ms());
    let hb = state
        .db
        .call(move |c| heartbeat_context(c, &o, now, max_per_day))
        .await?;
    let prompt = heartbeat_prompt(state, &hb);
    let (o, name) = (owner.clone(), job.name());
    let (conv_id, user_msg, assistant_msg) = state
        .db
        .call(move |c| {
            let conv = db::ensure_job_conversation(c, &o, name)?;
            // Keep hidden job transcripts short.
            c.execute(
                "DELETE FROM messages WHERE conversation_id = ?1 AND id NOT IN (
                     SELECT id FROM messages WHERE conversation_id = ?1
                     ORDER BY created_at DESC LIMIT 20)",
                [&conv],
            )?;
            let u = db::insert_message(
                c,
                &NewMessage {
                    conversation_id: &conv,
                    role: "user",
                    content: &prompt,
                    meta: Some(&json!({"job": name})),
                    ..Default::default()
                },
            )?;
            let a = db::insert_message(
                c,
                &NewMessage {
                    conversation_id: &conv,
                    role: "assistant",
                    content: "",
                    status: "streaming",
                    ..Default::default()
                },
            )?;
            Ok((conv, u, a))
        })
        .await?;
    crate::info!("{} job started", job.name());
    let started = now_ms();
    let result = run_turn(
        state,
        TurnInput {
            conversation_id: conv_id,
            user_message: user_msg,
            assistant_message: assistant_msg,
            purpose: Purpose::Scheduled,
            slot: crate::ranking::Slot::Background,
            owner_user_id: owner.clone(),
            continuity: false,
            prepare_only: false,
            may_act: false,
        },
    )
    .await;
    let key = job.last_run_key();
    state
        .db
        .call(move |c| db::set_setting(c, key, &started.to_string()))
        .await?;
    crate::info!(
        "{} job finished: {}",
        job.name(),
        if result.ok { "ok" } else { "error" }
    );
    if !result.ok {
        return Ok(());
    }
    let text = result.message.content.trim().to_string();
    if text.is_empty() || text.contains(HEARTBEAT_OK) {
        return Ok(());
    }
    deliver_proactive(
        state,
        &owner,
        &text,
        &result.message,
        "heartbeat",
        max_per_day,
    )
    .await
}

/// Post a message the agent wrote on its own initiative, within the daily
/// budget: over it, the message is held for the next run instead (and its
/// cards stay in the job conversation).
pub async fn deliver_proactive(
    state: &AppState,
    owner: &str,
    text: &str,
    source: &db::Message,
    initiated_by: &str,
    max_per_day: u64,
) -> crate::util::Result<()> {
    let (o, now) = (owner.to_string(), now_ms());
    let sent_today = state
        .db
        .call(move |c| crate::proactive::posted_today(c, &o, now))
        .await?;
    if sent_today >= max_per_day as i64 {
        let (o, t, by) = (
            owner.to_string(),
            text.to_string(),
            initiated_by.to_string(),
        );
        state
            .db
            .call(move |c| crate::proactive::record(c, &o, None, &by, &t, "held"))
            .await?;
        crate::info!("{initiated_by}: message held, {sent_today} of {max_per_day} sent today");
        // Settings shows the scorecard; keep open apps current.
        state.broadcast_settings().await;
        return Ok(());
    }
    let posted = post_to_main(state, owner, text, source, initiated_by).await?;
    let (o, t, by, id) = (
        owner.to_string(),
        text.to_string(),
        initiated_by.to_string(),
        posted.id.clone(),
    );
    state
        .db
        .call(move |c| crate::proactive::record(c, &o, Some(&id), &by, &t, "posted"))
        .await?;
    move_cards_to_main(state, owner, source).await?;
    state.broadcast_settings().await;
    Ok(())
}

/// Copy the cards a job turn created (after its reply placeholder, in the
/// hidden job conversation) into the user's Main chat, and broadcast them.
pub async fn move_cards_to_main(
    state: &AppState,
    user_id: &str,
    reply: &db::Message,
) -> crate::util::Result<()> {
    let (uid, job_conv, after) = (
        user_id.to_string(),
        reply.conversation_id.clone(),
        reply.created_at,
    );
    let moved = state
        .db
        .call(move |c| {
            let main = db::ensure_main_conversation(c, &uid)?;
            let mut moved = Vec::new();
            for m in db::messages_between(c, &job_conv, after, i64::MAX)? {
                if let (true, Some(card)) = (m.kind == "card", m.card) {
                    moved.push(entities::insert_card(c, &main, &card, "assistant")?);
                }
            }
            Ok(moved)
        })
        .await?;
    for m in moved {
        state.hub.to_users(
            &[user_id.to_string()],
            json!({"type": "message.created", "conversation_id": m.conversation_id, "message": m}),
        );
    }
    Ok(())
}

/// Post an agent-initiated message into the user's Main chat, broadcast it,
/// and send a push notification.
pub async fn post_to_main(
    state: &AppState,
    user_id: &str,
    text: &str,
    source: &db::Message,
    initiated_by: &str,
) -> crate::util::Result<db::Message> {
    post_to_main_opts(state, user_id, text, source, initiated_by, true).await
}

/// [`post_to_main`], pushing a notification only when `push` is set.
pub async fn post_to_main_opts(
    state: &AppState,
    user_id: &str,
    text: &str,
    source: &db::Message,
    initiated_by: &str,
    push: bool,
) -> crate::util::Result<db::Message> {
    post_note_to_main(state, user_id, text, Some(source), initiated_by, push).await
}

/// [`post_to_main_opts`] for a note no engine wrote (`source` None).
/// Returns the Main chat message.
pub async fn post_note_to_main(
    state: &AppState,
    user_id: &str,
    text: &str,
    source: Option<&db::Message>,
    initiated_by: &str,
    push: bool,
) -> crate::util::Result<db::Message> {
    let (uid, text_s) = (user_id.to_string(), text.to_string());
    let meta = json!({"initiated_by": initiated_by, "routing": source.and_then(|s| s.meta.get("routing"))});
    let (harness, model, effort) = (
        source.and_then(|s| s.harness.clone()),
        source.and_then(|s| s.model.clone()),
        source.and_then(|s| s.effort.clone()),
    );
    let msg = state
        .db
        .call(move |c| {
            let main = db::ensure_main_conversation(c, &uid)?;
            db::insert_message(
                c,
                &NewMessage {
                    conversation_id: &main,
                    role: "assistant",
                    content: &text_s,
                    harness: harness.as_deref(),
                    model: model.as_deref(),
                    effort: effort.as_deref(),
                    meta: Some(&meta),
                    ..Default::default()
                },
            )
        })
        .await?;
    state.hub.to_users(
        &[user_id.to_string()],
        json!({"type": "message.created", "conversation_id": msg.conversation_id, "message": msg}),
    );
    if !push {
        return Ok(msg);
    }
    let title = state.agent_name().await;
    crate::notify::push_users(
        state,
        &[user_id.to_string()],
        json!({
            "title": title,
            "body": truncate_chars(text, 180),
            "conversation_id": msg.conversation_id,
            "message_id": msg.id,
            "kind": initiated_by,
        }),
    );
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_including_overnight() {
        assert!(in_window(9 * 60, 8 * 60, 22 * 60));
        assert!(!in_window(23 * 60, 8 * 60, 22 * 60));
        assert!(in_window(23 * 60, 22 * 60, 6 * 60));
        assert!(in_window(60, 22 * 60, 6 * 60));
        assert!(!in_window(12 * 60, 22 * 60, 6 * 60));
        assert!(in_window(0, 0, 0));
    }

    #[test]
    fn due_respects_enabled_interval_and_window() {
        let mut cfg = JobConfig::default();
        let now = 10_000_000_000;
        let noon = 12 * 60;
        assert!(!due(&cfg, None, now, noon), "disabled by default");
        cfg.enabled = true;
        assert!(due(&cfg, None, now, noon));
        assert!(!due(&cfg, Some(now - 59 * 60_000), now, noon));
        assert!(due(&cfg, Some(now - 60 * 60_000), now, noon));
        assert!(!due(&cfg, None, now, 3 * 60), "outside active hours");
    }

    fn test_state() -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = crate::config::Config::default();
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        (AppState::new(cfg, paths, db).unwrap(), tmp)
    }

    async fn set(state: &AppState, key: &'static str, value: Option<&'static str>) {
        state
            .db
            .call(move |c| match value {
                Some(v) => db::set_setting(c, key, v),
                None => db::delete_setting(c, key),
            })
            .await
            .unwrap();
    }

    async fn listed_heartbeat(state: &AppState) -> Value {
        status(state)
            .await
            .into_iter()
            .find(|j| j["name"] == "heartbeat")
            .unwrap()
    }

    #[tokio::test]
    async fn proactive_messages_stop_at_the_daily_budget_and_are_held() {
        let (state, _tmp) = test_state();
        let owner = state.db.with(db::owner_user_id).unwrap().unwrap();
        let (source, main) = state
            .db
            .with(|c| {
                let job = db::ensure_job_conversation(c, &owner, "heartbeat")?;
                let m = db::insert_message(
                    c,
                    &db::NewMessage {
                        conversation_id: &job,
                        role: "assistant",
                        content: "x",
                        ..Default::default()
                    },
                )?;
                Ok((m, db::ensure_main_conversation(c, &owner)?))
            })
            .unwrap();
        for text in ["Rent is due Friday.", "The package slipped a day."] {
            deliver_proactive(&state, &owner, text, &source, "heartbeat", 2)
                .await
                .unwrap();
        }
        deliver_proactive(&state, &owner, "One more thing.", &source, "heartbeat", 2)
            .await
            .unwrap();
        let posted: Vec<String> = state
            .db
            .with(|c| db::list_messages(c, &main, None, 10))
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert!(posted.contains(&"Rent is due Friday.".to_string()));
        assert!(
            !posted.contains(&"One more thing.".to_string()),
            "{posted:?}"
        );
        let hb = state
            .db
            .with(|c| heartbeat_context(c, &owner, now_ms(), 2))
            .unwrap();
        assert_eq!(hb.sent_today, 2);
        assert_eq!(hb.held.len(), 1);
        assert_eq!(hb.held[0].excerpt, "One more thing.");
        let prompt = heartbeat_prompt(&state, &hb);
        assert!(prompt.contains("## Held back"));
        assert!(prompt.contains("One more thing."));
        assert!(prompt.contains("Today: 2 of 2 messages used."));
        assert!(prompt.contains("You have no working notes yet."));
        assert!(prompt.contains(HEARTBEAT_OK));
        // Notes and goals with targets show up in the plan.
        state
            .db
            .with(|c| {
                crate::proactive::upsert_note(
                    c,
                    &owner,
                    "garden",
                    None,
                    "Water Tuesdays.",
                    "agent",
                )?
                .unwrap();
                crate::entities::create_goal_with(
                    c,
                    &owner,
                    &crate::entities::NewGoal {
                        title: "Paint the fence",
                        target: "Both coats done",
                        due: Some("2026-05-01"),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let hb = state
            .db
            .with(|c| heartbeat_context(c, &owner, now_ms(), 2))
            .unwrap();
        let prompt = heartbeat_prompt(&state, &hb);
        assert!(prompt.contains("<working_note domain=\"garden\""));
        assert!(prompt.contains("**Paint the fence** · due 2026-05-01\n  Target: Both coats done"));
    }

    #[tokio::test]
    async fn heartbeat_follows_app_settings_without_restart() {
        use crate::state::{HEARTBEAT_ENABLED, HEARTBEAT_END, HEARTBEAT_INTERVAL, HEARTBEAT_START};
        let (state, _tmp) = test_state();
        let (now, noon) = (10_000_000_000, 12 * 60);
        // Nothing saved: off, hourly, 08:00 to 22:00.
        assert_eq!(state.heartbeat_config().await, JobConfig::default());
        assert!(!job_due(&state, Job::Heartbeat, now, noon).await);
        assert_eq!(listed_heartbeat(&state).await["enabled"], false);

        set(&state, HEARTBEAT_ENABLED, Some("true")).await;
        assert!(job_due(&state, Job::Heartbeat, now, noon).await);
        assert_eq!(listed_heartbeat(&state).await["enabled"], true);
        assert!(
            !job_due(&state, Job::Heartbeat, now, 7 * 60).await,
            "before 08:00"
        );

        // A new window and interval apply on the next tick.
        set(&state, HEARTBEAT_START, Some("06:30")).await;
        set(&state, HEARTBEAT_END, Some("07:30")).await;
        set(&state, HEARTBEAT_INTERVAL, Some("240")).await;
        assert!(job_due(&state, Job::Heartbeat, now, 7 * 60).await);
        assert!(!job_due(&state, Job::Heartbeat, now, noon).await);
        let listed = listed_heartbeat(&state).await;
        assert_eq!(
            (
                &listed["interval_mins"],
                &listed["active_start"],
                &listed["active_end"]
            ),
            (&json!(240), &json!("06:30"), &json!("07:30"))
        );
        let last = (now - 3 * 60 * 60_000).to_string();
        state
            .db
            .call(move |c| db::set_setting(c, "job.heartbeat.last_run", &last))
            .await
            .unwrap();
        assert!(
            !job_due(&state, Job::Heartbeat, now, 7 * 60).await,
            "3 of 4 hours"
        );
        assert!(job_due(&state, Job::Heartbeat, now + 60 * 60_000, 7 * 60).await);

        set(&state, HEARTBEAT_ENABLED, Some("false")).await;
        assert!(!job_due(&state, Job::Heartbeat, now + 60 * 60_000, 7 * 60).await);
    }

    #[tokio::test]
    async fn unreadable_heartbeat_settings_use_defaults() {
        use crate::state::{HEARTBEAT_END, HEARTBEAT_INTERVAL, HEARTBEAT_START};
        let (state, _tmp) = test_state();
        set(&state, HEARTBEAT_INTERVAL, Some("5")).await;
        set(&state, HEARTBEAT_START, Some("25:00")).await;
        set(&state, HEARTBEAT_END, Some("9:5")).await;
        let cfg = state.heartbeat_config().await;
        assert_eq!(cfg.interval_mins, 60);
        assert_eq!(cfg.active_start, "08:00");
        assert_eq!(cfg.active_end, "09:05");
    }

    #[test]
    fn legacy_heartbeat_is_imported_once_and_never_overwrites() {
        use crate::state::{HEARTBEAT_ENABLED, HEARTBEAT_INTERVAL, import_legacy_heartbeat};
        let (state, _tmp) = test_state();
        let legacy = JobConfig {
            enabled: true,
            interval_mins: 5,
            active_start: "7:00".into(),
            active_end: "nope".into(),
        };
        let got = |k| state.db.with(|c| db::get_setting(c, k)).unwrap();
        assert!(
            state
                .db
                .with(|c| import_legacy_heartbeat(c, &legacy))
                .unwrap()
        );
        assert_eq!(got(HEARTBEAT_ENABLED).as_deref(), Some("true"));
        // Clamped to the app's bounds; bad times keep their defaults.
        assert_eq!(got(HEARTBEAT_INTERVAL).as_deref(), Some("15"));
        assert_eq!(got(crate::state::HEARTBEAT_START).as_deref(), Some("07:00"));
        assert_eq!(got(crate::state::HEARTBEAT_END).as_deref(), Some("22:00"));
        // Changed in the app afterwards: a restart does not import again.
        state
            .db
            .with(|c| db::set_setting(c, HEARTBEAT_ENABLED, "false"))
            .unwrap();
        assert!(
            !state
                .db
                .with(|c| import_legacy_heartbeat(c, &legacy))
                .unwrap()
        );
        assert_eq!(got(HEARTBEAT_ENABLED).as_deref(), Some("false"));

        // Any check-in setting already saved (say, the toggle from an
        // earlier version) blocks the import entirely.
        let (state, _tmp) = test_state();
        state
            .db
            .with(|c| db::set_setting(c, HEARTBEAT_ENABLED, "false"))
            .unwrap();
        assert!(
            !state
                .db
                .with(|c| import_legacy_heartbeat(c, &legacy))
                .unwrap()
        );
        let got = |k| state.db.with(|c| db::get_setting(c, k)).unwrap();
        assert_eq!(got(HEARTBEAT_ENABLED).as_deref(), Some("false"));
        assert_eq!(got(HEARTBEAT_INTERVAL), None);
    }

    #[test]
    fn job_names_round_trip() {
        for j in Job::ALL {
            assert_eq!(Job::parse(j.name()), Some(j));
        }
        assert_eq!(Job::parse("nope"), None);
        assert!(local_minutes_now() < 1440);
    }
}
