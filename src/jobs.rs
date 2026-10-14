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
            Job::Heartbeat => state.cfg.heartbeat.clone(),
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
                let last = state
                    .setting(job.last_run_key())
                    .await
                    .and_then(|s| s.parse::<i64>().ok());
                let cfg = job.effective_cfg(&state).await;
                let ready = if job == Job::Dream {
                    crate::memory::dream::due(&state.cfg.dream, last, now_ms(), local_minutes_now())
                        && cfg.enabled
                } else {
                    due(&cfg, last, now_ms(), local_minutes_now())
                };
                if ready {
                    trigger(&state, job, false);
                }
            }
        }
    });
}

fn heartbeat_prompt(state: &AppState) -> String {
    let checklist =
        std::fs::read_to_string(state.paths.persona_dir().join("HEARTBEAT.md")).unwrap_or_default();
    format!(
        "Scheduled heartbeat check at {}.\n\nWork through this checklist. Use your \
         memory files and connected tools as needed.\n\n<heartbeat>\n{}\n</heartbeat>\n\n\
         If nothing needs the user's attention, reply with exactly {HEARTBEAT_OK} and \
         nothing else. Otherwise reply with the short message you want to send the \
         user (it will appear in their Main chat and as a notification).",
        local_timestamp(),
        truncate_chars(checklist.trim(), 20_000)
    )
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
    let prompt = heartbeat_prompt(state);
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
    post_to_main(state, &owner, &text, &result.message, "heartbeat").await?;
    move_cards_to_main(state, &owner, &result.message).await?;
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
) -> crate::util::Result<()> {
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
) -> crate::util::Result<()> {
    let (uid, text_s) = (user_id.to_string(), text.to_string());
    let meta = json!({"initiated_by": initiated_by, "routing": source.meta.get("routing")});
    let (harness, model, effort) = (
        source.harness.clone(),
        source.model.clone(),
        source.effort.clone(),
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
        return Ok(());
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
    Ok(())
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

    #[test]
    fn job_names_round_trip() {
        for j in Job::ALL {
            assert_eq!(Job::parse(j.name()), Some(j));
        }
        assert_eq!(Job::parse("nope"), None);
        assert!(local_minutes_now() < 1440);
    }
}
