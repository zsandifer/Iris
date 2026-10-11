//! The turn pipeline: one user (or job) message in, one streamed assistant
//! reply out. Iris only orchestrates; the harness CLI does all reasoning and
//! tool use.
//!
//! 1. Serialize per conversation and wait for a CLI slot (global cap).
//! 2. Pick an engine (router + individual-use policy + harness health).
//! 3. Build the prompt: replay messages this harness session has not seen
//!    (e.g. after switching harness, or heartbeat posts), then the message.
//! 4. Build the system prompt and MCP config (Iris bridge + connectors).
//! 5. Spawn the CLI, stream normalized events to WebSocket clients.
//! 6. On "not signed in", mark the harness and retry on another engine; on
//!    a failed resume, retry once with a fresh session.
//! 7. Persist the reply, session id, and metadata; notify and push.

use crate::classifier::{self, Assessment, Effort};
use crate::db::{self, Message};
use crate::harness::runner::{self, RunRequest};
use crate::harness::{
    Activity, AuthState, HarnessEvent, ImageInput, McpServer, McpTransport, TurnSpec,
};
use crate::persona::{self, PromptContext};
use crate::ranking::Slot;
use crate::router::{EngineEnv, NoEngine, Purpose, RouteDecision, RouteInput, select_engine};
use crate::state::AppState;
use crate::util::{new_id, truncate_chars};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

const MAX_REPLAY_MESSAGES: usize = 40;
const MAX_REPLAY_CHARS: usize = 24_000;
const MAX_ATTEMPTS: usize = 3;

pub struct TurnInput {
    pub conversation_id: String,
    /// The message being answered (already stored).
    pub user_message: Message,
    /// Placeholder assistant message (status `streaming`, already stored).
    pub assistant_message: Message,
    pub purpose: Purpose,
    /// Base slot: `Chat` for conversations, `Background`/`Feed` for jobs.
    pub slot: Slot,
    /// Whose goals/feed/library the Iris MCP bridge operates on.
    pub owner_user_id: String,
    /// Resume the harness session and replay unseen messages. Jobs run
    /// fresh every time.
    pub continuity: bool,
}

pub struct TurnResult {
    pub message: Message,
    pub ok: bool,
}

#[derive(Default)]
struct Acc {
    text: String,
    session: Option<String>,
    activities: Vec<Activity>,
    usage: Option<Value>,
    terminal: Option<Terminal>,
}

#[derive(Debug, Clone)]
enum Terminal {
    Done,
    Auth(String),
    Error(String),
}

enum Outcome {
    Complete,
    Cancelled,
    Failed { kind: &'static str, detail: String },
}

/// Run one turn to completion. Never panics on harness failure; the
/// assistant message always ends in a terminal status.
pub async fn run_turn(state: &AppState, input: TurnInput) -> TurnResult {
    let conv_id = input.conversation_id.clone();
    let lock = state.conversation_lock(&conv_id);
    let _turn_guard = lock.lock().await;
    let (cancel_tx, cancel_rx) = watch::channel(false);
    state
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(conv_id.clone(), cancel_tx);
    let result = run_locked(state, &input, cancel_rx).await;
    state
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&conv_id);
    result
}

/// Request cancellation of the running turn in a conversation.
pub fn cancel(state: &AppState, conversation_id: &str) -> bool {
    state
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(conversation_id)
        .map(|tx| tx.send(true).is_ok())
        .unwrap_or(false)
}

struct Ctx {
    kind: String,
    members: Vec<String>,
    names: HashMap<String, String>,
}

async fn load_ctx(state: &AppState, conv_id: &str) -> Ctx {
    let id = conv_id.to_string();
    state
        .db
        .call(move |c| {
            let conv = db::get_conversation_any(c, &id)?;
            let members = db::conversation_member_ids(c, &id)?;
            let mut names = HashMap::new();
            for m in &members {
                if let Some(u) = db::get_user(c, m)? {
                    names.insert(u.id, u.display_name);
                }
            }
            Ok(Ctx {
                kind: conv.map(|c| c.kind).unwrap_or_else(|| "side".into()),
                members,
                names,
            })
        })
        .await
        .unwrap_or(Ctx {
            kind: "side".into(),
            members: Vec::new(),
            names: HashMap::new(),
        })
}

async fn run_locked(
    state: &AppState,
    input: &TurnInput,
    mut cancel: watch::Receiver<bool>,
) -> TurnResult {
    let conv_id = input.conversation_id.as_str();
    let msg_id = input.assistant_message.id.as_str();
    let ctx = load_ctx(state, conv_id).await;
    // Job conversations are hidden; their turns are not streamed to anyone.
    let audience = if ctx.kind == "job" {
        Vec::new()
    } else {
        ctx.members.clone()
    };
    let shared = ctx.members.len() > 1;
    if state.browser.is_paused() {
        return finish(
            state,
            input,
            &audience,
            &Acc::default(),
            json!(null),
            Outcome::Failed {
                kind: "paused",
                detail: "the owner paused Iris".into(),
            },
        )
        .await;
    }

    if state.limiter.saturated() {
        state.hub.to_users(
            &audience,
            json!({"type": "turn.status", "conversation_id": conv_id, "message_id": msg_id, "status": "queued"}),
        );
    }
    let _permit = tokio::select! {
        p = state.limiter.acquire() => p,
        _ = async { let _ = cancel.wait_for(|c| *c).await; } => {
            return finish(state, input, &audience, &Acc::default(), json!(null), Outcome::Cancelled).await;
        }
    };
    state.hub.to_users(
        &audience,
        json!({"type": "turn.status", "conversation_id": conv_id, "message_id": msg_id, "status": "running"}),
    );

    let catalog = state.catalog();
    let pins = state.slot_pins().await;
    let allow_shared = state.allow_claude_for_shared().await;
    let has_images = !input.user_message.attachments.is_empty();
    let assessment = assess_turn(state, input).await;
    let mut excluded: Vec<String> = Vec::new();
    let mut fresh_session = !input.continuity;
    let mut routing_notes: Vec<String> = Vec::new();
    let mut acc = Acc::default();
    let mut decision: Option<RouteDecision> = None;
    let mut outcome = Outcome::Failed {
        kind: "no_engine",
        detail: "no engine available".into(),
    };
    // Iris's browser: only in the owner's own conversations (its profile
    // holds the owner's sign-ins). Dropping the guard ends the browser peek.
    let browser_turn = if browser_allowed(state, &input.owner_user_id, shared).await {
        Some(
            state
                .browser
                .begin_turn(&input.owner_user_id, conv_id, msg_id, &audience),
        )
    } else {
        None
    };

    for _attempt in 0..MAX_ATTEMPTS {
        // Routing borrows non-Send closures; keep them out of any await.
        let routed = {
            let available = |m: &crate::catalog::ModelEntry| {
                !excluded.iter().any(|x| x == &m.harness) && state.model_available(m)
            };
            let env = EngineEnv {
                rankings: &state.rankings,
                catalog: &catalog,
                pins: &pins,
                available: &available,
                allow_claude_for_shared: allow_shared,
            };
            let route_input = RouteInput {
                slot: input.slot,
                has_images,
                shared,
                assessment: &assessment,
            };
            select_engine(&route_input, &env)
        };
        let d = match routed {
            Ok(d) => d,
            Err(NoEngine::PausedPolicy) => {
                post_policy_card_once(state, conv_id).await;
                outcome = Outcome::Failed {
                    kind: "paused_policy",
                    detail: "only Claude is available and this conversation is shared".into(),
                };
                break;
            }
            Err(NoEngine::NoModel) => {
                let (kind, detail) = no_engine_reason(state, shared, allow_shared);
                outcome = Outcome::Failed { kind, detail };
                break;
            }
        };
        let harness_name = d.harness().to_string();
        let Some(entry) = state.harness(&harness_name) else {
            excluded.push(harness_name);
            continue;
        };
        let Some(program) = entry.program() else {
            entry
                .status
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .installed = false;
            excluded.push(harness_name);
            continue;
        };

        // Session continuity and replay of unseen messages.
        let agent_name = state.agent_name().await;
        let persona_hash = persona::persona_hash(&state.paths, &agent_name, &ctx.kind);
        let mut session = if fresh_session {
            None
        } else {
            let (c, h) = (conv_id.to_string(), harness_name.clone());
            state
                .db
                .call(move |db| db::get_harness_session(db, &c, &h))
                .await
                .ok()
                .flatten()
        };
        if session
            .as_ref()
            .is_some_and(|s| s.persona_hash != persona_hash)
            && !entry.harness.resume_updates_system_prompt()
        {
            // The CLI would keep the old persona; start over with replay.
            routing_notes.push("persona changed; started a new session".into());
            session = None;
        }
        let replay = if input.continuity {
            let after = session.as_ref().map_or(0, |s| s.synced_at);
            let (c, before) = (conv_id.to_string(), input.user_message.created_at);
            state
                .db
                .call(move |db| db::messages_between(db, &c, after, before))
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let prompt = render_prompt(&ctx, &agent_name, &replay, &input.user_message);
        let mut notices = state.harness_notices(&harness_name);
        let caps = entry.harness.capabilities();
        let sees_images =
            caps.images && catalog.get(&d.model).and_then(|m| m.images).unwrap_or(true);
        let mut images = load_images(state, &input.user_message).await;
        if !images.is_empty() && !sees_images {
            images.clear();
            routing_notes.push(format!("{} cannot see images; image left out", d.model));
            notices.push(
                "The user attached an image, but the engine answering right now cannot see \
                 images. Say so briefly and offer to help if they describe it."
                    .into(),
            );
        }
        let browser_token = browser_turn.as_ref().map(|g| g.token());
        let mcp_servers = if caps.mcp {
            mcp_servers(state, &input.owner_user_id, conv_id, shared, browser_token).await
        } else {
            Vec::new()
        };
        let browser = mcp_servers
            .iter()
            .any(|s| s.name == crate::browser::tools::SERVER);
        let participants: Vec<String> = ctx.names.values().cloned().collect();
        let system_prompt = persona::assemble(
            &state.paths,
            &PromptContext {
                agent_name: &agent_name,
                conversation_kind: &ctx.kind,
                purpose: input.purpose,
                participants: &participants,
                app_tools: !mcp_servers.is_empty(),
                browser,
                shared,
                notices: &notices,
            },
        );
        let spec = TurnSpec {
            turn_id: new_id(),
            prompt,
            images,
            system_prompt,
            model: d.cli_model().to_string(),
            effort: d.effort.clone(),
            resume: session.as_ref().map(|s| s.session_id.clone()),
            mcp_servers,
            cwd: state.paths.workspace_dir(),
            // The owner's memory is only writable in personal conversations.
            writable_dirs: if shared {
                Vec::new()
            } else {
                vec![state.paths.memory_dir()]
            },
            run_dir: state.paths.run_dir(),
            scope: conv_id.to_string(),
        };
        let invocation = match entry.harness.build(&spec) {
            Ok(mut i) => {
                if harness_name == "claude" && crate::harness::is_managed(&program) {
                    // Iris updates the engines it installed; the CLI's own
                    // updater could not write there anyway.
                    i.env.push(("DISABLE_AUTOUPDATER".into(), "1".into()));
                }
                i
            }
            Err(e) => {
                outcome = Outcome::Failed {
                    kind: "internal",
                    detail: format!("could not build command: {e}"),
                };
                break;
            }
        };
        let routing = routing_json(&d, &routing_notes);
        let (m, h, model, effort, meta) = (
            msg_id.to_string(),
            harness_name.clone(),
            d.model.clone(),
            d.effort.clone(),
            json!({"routing": routing}),
        );
        let _ = state
            .db
            .call(move |c| db::set_message_engine(c, &m, &h, &model, effort.as_deref(), &meta))
            .await;
        crate::info!(
            "turn {msg_id}: {} effort={} resume={} replay={}",
            d.model,
            d.effort.as_deref().unwrap_or("default"),
            spec.resume.is_some(),
            replay.len()
        );
        decision = Some(d.clone());

        let strip = state.strip_env(entry);
        let server_names: Vec<String> = spec.mcp_servers.iter().map(|s| s.name.clone()).collect();
        let (tx, rx) = mpsc::unbounded_channel();
        let run_fut = runner::run(
            RunRequest {
                program: &program,
                invocation: &invocation,
                cwd: &spec.cwd,
                env_remove: &strip,
                timeout: Duration::from_secs(state.cfg.turn_timeout_secs),
            },
            entry.harness.parser(&server_names),
            cancel.clone(),
            tx,
        );
        let consume_fut = consume(state, &audience, conv_id, msg_id, rx);
        let (run_res, attempt) = tokio::join!(run_fut, consume_fut);
        for f in &invocation.temp_files {
            let _ = std::fs::remove_file(f);
        }

        let run_out = match run_res {
            Ok(o) => o,
            Err(e) => {
                // Spawn failure: binary vanished or is not executable.
                crate::warn!("turn {msg_id}: failed to start {harness_name}: {e}");
                excluded.push(harness_name);
                routing_notes.push(format!("{} failed to start", d.model));
                continue;
            }
        };
        let resumed = spec.resume.is_some();
        acc = attempt;
        if run_out.cancelled {
            outcome = Outcome::Cancelled;
            remember_session(
                state,
                conv_id,
                &harness_name,
                &acc,
                input.user_message.created_at,
                &persona_hash,
            )
            .await;
            break;
        }
        if run_out.timed_out {
            outcome = Outcome::Failed {
                kind: "timeout",
                detail: format!("no result after {}s", state.cfg.turn_timeout_secs),
            };
            break;
        }
        match acc.terminal.clone() {
            Some(Terminal::Done) => {
                state.note_auth(&harness_name, AuthState::Ok, None).await;
                let synced =
                    synced_through(state, conv_id, input.assistant_message.created_at).await;
                remember_session(state, conv_id, &harness_name, &acc, synced, &persona_hash).await;
                outcome = Outcome::Complete;
                break;
            }
            Some(Terminal::Auth(message)) => {
                crate::warn!("turn {msg_id}: {harness_name} is not signed in");
                state
                    .note_auth(
                        &harness_name,
                        AuthState::AuthRequired,
                        Some(message.clone()),
                    )
                    .await;
                outcome = Outcome::Failed {
                    kind: "auth_required",
                    detail: harness_name.clone(),
                };
                if acc.text.is_empty() {
                    routing_notes.push(format!("{} signed out; rerouted", d.model));
                    excluded.push(harness_name);
                    continue;
                }
                break;
            }
            Some(Terminal::Error(message)) if resumed && acc.text.is_empty() && !fresh_session => {
                // Typically a session the CLI no longer has; start fresh and
                // replay the conversation instead.
                crate::warn!("turn {msg_id}: resume failed, retrying with a fresh session");
                let (c, h) = (conv_id.to_string(), harness_name.clone());
                let _ = state
                    .db
                    .call(move |db| db::clear_harness_session(db, &c, &h))
                    .await;
                routing_notes.push(format!("resume failed: {}", truncate_chars(&message, 120)));
                fresh_session = true;
                continue;
            }
            Some(Terminal::Error(message)) => {
                outcome = Outcome::Failed {
                    kind: "harness_error",
                    detail: message,
                };
                break;
            }
            None => {
                outcome = Outcome::Failed {
                    kind: "harness_error",
                    detail: "the CLI ended without a result".into(),
                };
                break;
            }
        }
    }

    let routing = decision
        .as_ref()
        .map(|d| routing_json(d, &routing_notes))
        .unwrap_or(json!(null));
    finish(state, input, &audience, &acc, routing, outcome).await
}

/// Why no engine could run the turn: harnesses allowed here that are
/// signed out (named in the detail), or none installed at all.
fn no_engine_reason(state: &AppState, shared: bool, allow_shared: bool) -> (&'static str, String) {
    let signed_out: Vec<String> = state
        .harnesses
        .values()
        .filter(|e| e.cfg.enabled)
        .filter(|e| crate::router::policy_allows(e.harness.name(), shared, allow_shared))
        .filter(|e| e.status().auth == AuthState::AuthRequired)
        .map(|e| e.harness.name().to_string())
        .collect();
    if signed_out.is_empty() {
        ("no_engine", "no usable engine".into())
    } else {
        ("auth_required", signed_out.join(","))
    }
}

/// Classify the message (chat) or use the job default (low effort).
async fn assess_turn(state: &AppState, input: &TurnInput) -> Assessment {
    if input.purpose != Purpose::Chat {
        return Assessment {
            effort: Effort::Low,
            escalate: false,
            score: 0,
            signals: vec!["scheduled_job"],
            ambiguous: false,
        };
    }
    let (c, before) = (input.conversation_id.clone(), input.user_message.created_at);
    let history = state
        .db
        .call(move |db| db::list_messages(db, &c, Some(before), 10))
        .await
        .unwrap_or_default();
    let turns: Vec<classifier::Turn> = history
        .iter()
        .filter(|m| m.kind == "text" && matches!(m.role.as_str(), "user" | "assistant"))
        .map(|m| classifier::Turn {
            role: &m.role,
            content: &m.content,
            failed: matches!(m.status.as_str(), "error" | "interrupted"),
            effort: m.effort.as_deref().and_then(Effort::parse),
        })
        .collect();
    let thresholds = classifier::Thresholds {
        long_chars: state.cfg.router.long_chars,
        very_long_chars: state.cfg.router.very_long_chars,
    };
    let mut a = classifier::assess(&input.user_message.content, &turns, &thresholds);
    if state.cfg.router.classifier_model
        && a.ambiguous
        && let Some(effort) = refine_with_model(state, &input.user_message.content).await
    {
        a.signals.push("model_classifier");
        a.effort = effort;
        a.escalate = a.escalate || effort == Effort::High && a.score >= classifier::HIGH_AT;
    }
    a
}

/// Optional: ask the `classifier` slot's model about an ambiguous message.
/// One short, tool-less, session-less run; None on any failure.
async fn refine_with_model(state: &AppState, text: &str) -> Option<Effort> {
    let (res, pin) = state.resolve_slot(Slot::Classifier, false).await;
    let model_id = res.effective?;
    let catalog = state.catalog();
    let m = catalog.get(&model_id)?;
    let entry = state.harness(&m.harness)?;
    let program = entry.program()?;
    let effort = pin
        .and_then(|p| p.effort)
        .or_else(|| crate::ranking::default_effort(m));
    let spec = TurnSpec {
        turn_id: new_id(),
        prompt: classifier::model_prompt(text),
        images: Vec::new(),
        system_prompt: "You classify requests. Answer with one word.".into(),
        model: m.model.clone(),
        effort,
        resume: None,
        mcp_servers: Vec::new(),
        cwd: state.paths.workspace_dir(),
        writable_dirs: Vec::new(),
        run_dir: state.paths.run_dir(),
        scope: "classifier".into(),
    };
    let invocation = entry.harness.build(&spec).ok()?;
    let strip = state.strip_env(entry);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let run = runner::run(
        RunRequest {
            program: &program,
            invocation: &invocation,
            cwd: &spec.cwd,
            env_remove: &strip,
            timeout: Duration::from_secs(30),
        },
        entry.harness.parser(&[]),
        cancel_rx,
        tx,
    )
    .await;
    for f in &invocation.temp_files {
        let _ = std::fs::remove_file(f);
    }
    run.ok()?;
    let mut reply = String::new();
    while let Ok(ev) = rx.try_recv() {
        if let HarnessEvent::TextDelta { text } = ev {
            reply.push_str(&text);
        }
    }
    classifier::parse_model_reply(&reply)
}

/// In a shared conversation where Claude is the only engine, explain once
/// (per conversation) with a one-tap opt-in card for the owner.
async fn post_policy_card_once(state: &AppState, conv_id: &str) {
    let key = format!("notice.policy_card.{conv_id}");
    let c = conv_id.to_string();
    let _ = state
        .db
        .call(move |db| {
            if db::get_setting(db, &key)?.is_some() {
                return Ok(());
            }
            let card = json!({
                "type": "action",
                "title": "Allow Claude in group chats",
                "body": "Claude is the only AI engine on this server, and Claude \
                    subscriptions are meant for one person, so I stay out of chats with \
                    more than one person. If your plan allows shared use, the owner can turn \
                    this on. Another engine (Codex, Gemini, or OpenCode) also fixes it.",
                "action_label": "Allow",
                "action": {"method": "PUT", "path": "/api/settings",
                           "body": {"allow_claude_for_shared": true}},
                "owner_only": true,
            });
            crate::entities::insert_card(db, &c, &card, "assistant")?;
            db::set_setting(db, &key, &crate::util::now_ms().to_string())
        })
        .await;
}

fn routing_json(d: &RouteDecision, notes: &[String]) -> Value {
    json!({
        "router": d.router,
        "model": d.model,
        "effort": d.effort,
        "slot": d.slot,
        "escalated": d.escalated,
        "score": d.score,
        "signals": d.signals,
        "reason": d.reason,
        "notes": notes,
    })
}

async fn consume(
    state: &AppState,
    audience: &[String],
    conv_id: &str,
    msg_id: &str,
    mut rx: mpsc::UnboundedReceiver<HarnessEvent>,
) -> Acc {
    let mut acc = Acc::default();
    while let Some(ev) = rx.recv().await {
        match ev {
            HarnessEvent::Session { id } => acc.session = Some(id),
            HarnessEvent::TextDelta { text } => {
                acc.text.push_str(&text);
                state.hub.to_users(
                    audience,
                    json!({"type": "message.delta", "conversation_id": conv_id, "message_id": msg_id, "text": text}),
                );
            }
            HarnessEvent::Activity(a) => {
                let merged = match acc.activities.iter_mut().find(|x| x.id == a.id) {
                    Some(existing) => {
                        existing.status = a.status.clone();
                        if !a.summary.is_empty() {
                            existing.summary = a.summary.clone();
                        }
                        if !a.name.is_empty() {
                            existing.name = a.name.clone();
                        }
                        existing.clone()
                    }
                    None => {
                        acc.activities.push(a.clone());
                        a
                    }
                };
                state.hub.to_users(
                    audience,
                    json!({"type": "message.activity", "conversation_id": conv_id, "message_id": msg_id, "activity": merged}),
                );
            }
            HarnessEvent::Usage { usage } => acc.usage = Some(usage),
            HarnessEvent::AuthRequired { message } => acc.terminal = Some(Terminal::Auth(message)),
            HarnessEvent::Error { message } => acc.terminal = Some(Terminal::Error(message)),
            HarnessEvent::Done => acc.terminal = Some(Terminal::Done),
        }
    }
    acc
}

/// Newest message this harness session has now seen: everything up to this
/// turn's reply, plus cards the agent created during the turn. Messages
/// that arrived meanwhile (queued user messages, heartbeat posts) stay
/// unsynced so they are replayed if another harness answers them.
async fn synced_through(state: &AppState, conv_id: &str, placeholder_created_at: i64) -> i64 {
    let c = conv_id.to_string();
    state
        .db
        .call(move |db| {
            db.query_row(
                "SELECT COALESCE(MAX(created_at), 0) FROM messages
                 WHERE conversation_id = ?1 AND (created_at <= ?2 OR kind = 'card')",
                rusqlite::params![c, placeholder_created_at],
                |r| r.get(0),
            )
        })
        .await
        .unwrap_or(placeholder_created_at)
}

async fn remember_session(
    state: &AppState,
    conv_id: &str,
    harness: &str,
    acc: &Acc,
    synced_at: i64,
    persona_hash: &str,
) {
    let Some(session_id) = acc.session.clone() else {
        return;
    };
    let (c, h) = (conv_id.to_string(), harness.to_string());
    let s = db::HarnessSession {
        session_id,
        synced_at,
        persona_hash: persona_hash.to_string(),
    };
    let _ = state
        .db
        .call(move |db| db::set_harness_session(db, &c, &h, &s))
        .await;
}

/// Friendly, first-person text for failures. The UI shows it as the
/// agent's reply; details stay in metadata.
fn failure_text(kind: &str, detail: &str) -> String {
    match kind {
        "auth_required" => {
            let fixes: Vec<&str> = detail
                .split(',')
                .filter_map(|h| match h.trim() {
                    "claude" => Some("run `claude` on the Iris server and sign in with /login"),
                    "codex" => Some("run `codex login` on the Iris server"),
                    "gemini" => Some("run `gemini` on the Iris server and sign in"),
                    "opencode" => Some("run `opencode auth login` on the Iris server"),
                    _ => None,
                })
                .collect();
            let fix = if fixes.is_empty() {
                "sign the AI command-line tools on the Iris server back in".to_string()
            } else {
                fixes.join(", or ")
            };
            format!(
                "I can't think right now because my AI engine on the server is signed out. \
                 To fix it, {fix}, then send your message again."
            )
        }
        "paused_policy" => "I can't reply in this group chat yet. The only AI engine on the \
            server is Claude, and Claude subscriptions are for one person, so I stay out of \
            shared conversations unless the owner turns that on."
            .into(),
        "timeout" => "That took too long, so I stopped. Want me to try again?".into(),
        "paused" => "I'm paused right now, so I'm not doing anything. You can turn me back on \
            in my computer settings whenever you want me again."
            .into(),
        "no_engine" => "I can't reach any AI engine on the server right now. One of Claude, \
            Codex, Gemini, or OpenCode needs to be installed and signed in; I can walk you \
            through it in Settings."
            .into(),
        _ => "Something went wrong on my end. Want me to try again?".into(),
    }
}

async fn finish(
    state: &AppState,
    input: &TurnInput,
    audience: &[String],
    acc: &Acc,
    routing: Value,
    outcome: Outcome,
) -> TurnResult {
    let conv_id = input.conversation_id.clone();
    let msg_id = input.assistant_message.id.clone();
    let mut meta = json!({
        "routing": routing,
        "purpose": input.purpose,
        "activity": acc.activities,
    });
    if let Some(u) = &acc.usage {
        meta["usage"] = u.clone();
    }
    let duration_ms = crate::util::now_ms() - input.assistant_message.created_at;
    meta["duration_ms"] = duration_ms.into();
    let (status, content, ok) = match &outcome {
        Outcome::Complete => ("complete", acc.text.trim().to_string(), true),
        Outcome::Cancelled => ("cancelled", acc.text.trim().to_string(), false),
        Outcome::Failed { kind, detail } => {
            meta["error"] = json!({"kind": kind, "detail": truncate_chars(detail, 500)});
            let text = if acc.text.trim().is_empty() {
                failure_text(kind, detail)
            } else {
                acc.text.trim().to_string()
            };
            ("error", text, false)
        }
    };
    let (m, c, s, mt) = (msg_id.clone(), content, status, meta);
    let message = match state
        .db
        .call(move |db| db::finish_message(db, &m, &c, s, &mt))
        .await
    {
        Ok(m) => m,
        Err(e) => {
            crate::warn!("turn {msg_id}: failed to save reply: {e}");
            input.assistant_message.clone()
        }
    };
    crate::info!("turn {msg_id}: {} in {duration_ms}ms", message.status);
    state.hub.to_users(
        audience,
        json!({"type": "message.completed", "conversation_id": conv_id, "message": message}),
    );

    // Cards the agent created through the Iris MCP bridge during the turn.
    let (c, after) = (conv_id.clone(), input.assistant_message.created_at);
    if let Ok(new) = state
        .db
        .call(move |db| db::messages_between(db, &c, after, i64::MAX))
        .await
    {
        for m in new.into_iter().filter(|m| m.kind == "card") {
            state.hub.to_users(
                audience,
                json!({"type": "message.created", "conversation_id": conv_id, "message": m}),
            );
        }
    }
    let mut refused: Vec<&str> = acc
        .activities
        .iter()
        .filter(|a| a.kind == "mcp" && a.auth_error)
        .filter_map(|a| a.name.split_once('/').map(|(server, _)| server))
        .filter(|server| *server != "iris")
        .collect();
    refused.dedup();
    for server in refused {
        crate::login::on_connector_auth_error(state, server).await;
    }
    let kinds = changed_entity_kinds(&acc.activities);
    if !kinds.is_empty() {
        state.hub.to_users(
            std::slice::from_ref(&input.owner_user_id),
            json!({"type": "entities.changed", "kinds": kinds}),
        );
    }
    // A failed turn tells offline members Iris got stuck, so nobody is left
    // waiting; the app offers a retry on the message.
    let stuck = matches!(outcome, Outcome::Failed { kind, .. } if kind != "paused");
    if (ok || stuck) && input.purpose == Purpose::Chat && state.cfg.push.notify_replies_when_offline
    {
        let offline: Vec<String> = audience
            .iter()
            .filter(|u| !state.hub.is_online(u))
            .cloned()
            .collect();
        if !offline.is_empty() {
            let title = state.agent_name().await;
            crate::notify::push_users(
                state,
                &offline,
                json!({
                    "title": title,
                    "body": if stuck {
                        "I got stuck on that. Tap to try again.".to_string()
                    } else {
                        truncate_chars(&message.content, 180)
                    },
                    "conversation_id": message.conversation_id,
                    "message_id": message.id,
                    "kind": if stuck { "stuck" } else { "reply" },
                }),
            );
        }
    }
    TurnResult { message, ok }
}

/// Map Iris MCP tool activity to entity kinds the UI should refresh.
fn changed_entity_kinds(activities: &[Activity]) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    for a in activities.iter().filter(|a| a.kind == "mcp") {
        let Some(tool) = a.name.strip_prefix("iris/") else {
            continue;
        };
        if a.status != "completed" || tool.starts_with("list_") {
            continue;
        }
        let kind = match tool {
            t if t.contains("goal") => "goals",
            t if t.contains("idea") => "ideas",
            t if t.contains("feed") => "feed",
            t if t.contains("library") => "library",
            _ => continue,
        };
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    kinds
}

/// Build the text sent to the harness as the user message.
fn render_prompt(ctx: &Ctx, agent_name: &str, replay: &[Message], current: &Message) -> String {
    let group = ctx.kind == "group";
    let name_of = |m: &Message| -> String {
        m.author_user_id
            .as_ref()
            .and_then(|u| ctx.names.get(u))
            .cloned()
            .unwrap_or_else(|| "User".into())
    };
    let mut lines: Vec<String> = Vec::new();
    for m in replay.iter().rev() {
        if m.content.trim().is_empty() || m.status == "streaming" {
            continue;
        }
        let line = match (m.role.as_str(), m.kind.as_str()) {
            (_, "card") => format!("[{agent_name} showed a card: {}]", m.content.trim()),
            ("user", _) => format!("{}: {}", name_of(m), m.content.trim()),
            ("assistant", _) => format!("{agent_name}: {}", m.content.trim()),
            _ => format!("[context] {}", m.content.trim()),
        };
        lines.push(line);
        if lines.len() >= MAX_REPLAY_MESSAGES
            || lines.iter().map(|l| l.len()).sum::<usize>() > MAX_REPLAY_CHARS
        {
            break;
        }
    }
    lines.reverse();
    let mut out = String::new();
    if !lines.is_empty() {
        out.push_str(
            "<conversation_history>\nEarlier messages in this conversation, included for \
             context (you may not have seen them in this session):\n\n",
        );
        out.push_str(&truncate_chars(&lines.join("\n\n"), MAX_REPLAY_CHARS));
        out.push_str("\n</conversation_history>\n\n");
    }
    if group && current.role == "user" {
        out.push_str(&format!("{}: ", name_of(current)));
    }
    out.push_str(current.content.trim());
    if !current.attachments.is_empty() && current.content.trim().is_empty() {
        out.push_str("(The user sent an image.)");
    }
    out
}

async fn load_images(state: &AppState, m: &Message) -> Vec<ImageInput> {
    let ids = m.attachments.clone();
    let uploads = state.paths.uploads_dir();
    state
        .db
        .call(move |c| {
            let mut out = Vec::new();
            for id in &ids {
                if let Some(a) = db::get_attachment(c, id)?
                    && a.mime.starts_with("image/")
                {
                    out.push(ImageInput {
                        path: uploads.join(&a.file_name),
                        mime: a.mime,
                    });
                }
            }
            Ok(out)
        })
        .await
        .unwrap_or_default()
}

/// The Iris MCP bridge plus every enabled connector (secrets unsealed).
///
/// In shared conversations (more than one person) the owner's connectors
/// are withheld and the bridge only offers cards, so other members cannot
/// reach the owner's accounts, goals, feed, or library through the agent.
async fn mcp_servers(
    state: &AppState,
    user_id: &str,
    conv_id: &str,
    shared: bool,
    browser_token: Option<&str>,
) -> Vec<McpServer> {
    let mut args = vec![
        "mcp".into(),
        "--data-dir".into(),
        state.paths.data_dir.to_string_lossy().into_owned(),
        "--user".into(),
        user_id.into(),
        "--conversation".into(),
        conv_id.into(),
    ];
    if shared {
        args.push("--shared".into());
    }
    let mut servers = vec![McpServer {
        name: "iris".into(),
        transport: McpTransport::Stdio {
            command: state.exe.to_string_lossy().into_owned(),
            args,
            env: BTreeMap::new(),
        },
    }];
    if shared {
        return servers;
    }
    if let Some(token) = browser_token {
        servers.push(McpServer {
            name: crate::browser::tools::SERVER.into(),
            transport: McpTransport::Stdio {
                command: state.exe.to_string_lossy().into_owned(),
                args: vec![
                    "mcp".into(),
                    "--browser".into(),
                    "--data-dir".into(),
                    state.paths.data_dir.to_string_lossy().into_owned(),
                ],
                // Identifies this turn to the server; not a secret of the
                // user's (it stops working when the turn ends).
                env: BTreeMap::from([("IRIS_BROWSER_TOKEN".to_string(), token.to_string())]),
            },
        });
    }
    let connectors = state
        .db
        .call(|c| crate::entities::list_connectors(c, true))
        .await
        .unwrap_or_default();
    for con in connectors {
        if servers.iter().any(|s| s.name == con.name) {
            crate::warn!("connector {} skipped: the name is taken by Iris", con.name);
            continue;
        }
        match crate::server::connector_to_mcp(state, &con) {
            Ok(s) => servers.push(s),
            Err(e) => crate::warn!("connector {} skipped: {e}", con.name),
        }
    }
    servers
}

/// Whether this turn gets the browser tools: enabled, not paused, a
/// browser is installed, and the conversation is the owner's alone.
async fn browser_allowed(state: &AppState, user_id: &str, shared: bool) -> bool {
    if shared || !state.browser.enabled() || state.browser.is_paused() {
        return false;
    }
    let owner = state.db.call(db::owner_user_id).await.ok().flatten();
    owner.as_deref() == Some(user_id) && state.browser.executable().is_some()
}

/// Store a user message and an assistant placeholder, broadcast both, and
/// run the turn in the background. Returns the two stored messages.
pub async fn start_chat_turn(
    state: &AppState,
    conversation_id: &str,
    author: Option<&str>,
    text: &str,
    attachments: &[String],
    purpose: Purpose,
    owner_user_id: &str,
) -> crate::util::Result<(Message, Message)> {
    let (conv, author_s, text_s, att) = (
        conversation_id.to_string(),
        author.map(String::from),
        text.to_string(),
        attachments.to_vec(),
    );
    let (user_msg, assistant_msg, members) = state
        .db
        .call(move |c| {
            let u = db::insert_message(
                c,
                &db::NewMessage {
                    conversation_id: &conv,
                    role: "user",
                    author_user_id: author_s.as_deref(),
                    content: &text_s,
                    attachments: &att,
                    ..Default::default()
                },
            )?;
            let a = db::insert_message(
                c,
                &db::NewMessage {
                    conversation_id: &conv,
                    role: "assistant",
                    content: "",
                    status: "streaming",
                    ..Default::default()
                },
            )?;
            Ok((u, a, db::conversation_member_ids(c, &conv)?))
        })
        .await?;
    for m in [&user_msg, &assistant_msg] {
        state.hub.to_users(
            &members,
            json!({"type": "message.created", "conversation_id": conversation_id, "message": m}),
        );
    }
    let input = TurnInput {
        conversation_id: conversation_id.to_string(),
        user_message: user_msg.clone(),
        assistant_message: assistant_msg.clone(),
        purpose,
        slot: Slot::Chat,
        owner_user_id: owner_user_id.to_string(),
        continuity: true,
    };
    let st = state.clone();
    tokio::spawn(async move {
        run_turn(&st, input).await;
    });
    Ok((user_msg, assistant_msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, kind: &str, author: Option<&str>, content: &str) -> Message {
        Message {
            id: new_id(),
            conversation_id: "c".into(),
            role: role.into(),
            author_user_id: author.map(String::from),
            kind: kind.into(),
            content: content.into(),
            attachments: vec![],
            card: None,
            status: "complete".into(),
            harness: None,
            model: None,
            effort: None,
            meta: json!({}),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn ctx(kind: &str) -> Ctx {
        let mut names = HashMap::new();
        names.insert("u1".to_string(), "Sam".to_string());
        names.insert("u2".to_string(), "Riley".to_string());
        Ctx {
            kind: kind.into(),
            members: vec!["u1".into(), "u2".into()],
            names,
        }
    }

    #[test]
    fn prompt_without_history_is_just_the_message() {
        let cur = msg("user", "text", Some("u1"), "  hello  ");
        assert_eq!(render_prompt(&ctx("main"), "Iris", &[], &cur), "hello");
    }

    #[test]
    fn prompt_replays_unseen_messages() {
        let replay = vec![
            msg("user", "text", Some("u1"), "what's up"),
            msg("assistant", "text", None, "not much"),
            msg("assistant", "card", None, "Connect Google"),
            msg("system", "text", None, "Feed post: Comets"),
            msg("assistant", "text", None, "   "),
        ];
        let cur = msg("user", "text", Some("u1"), "next");
        let p = render_prompt(&ctx("main"), "Nova", &replay, &cur);
        assert!(p.starts_with("<conversation_history>"));
        assert!(p.contains("Sam: what's up\n\nNova: not much\n\n[Nova showed a card: Connect Google]\n\n[context] Feed post: Comets"));
        assert!(p.ends_with("</conversation_history>\n\nnext"));
    }

    #[test]
    fn group_prompts_name_the_sender() {
        let cur = msg("user", "text", Some("u2"), "hi all");
        assert_eq!(
            render_prompt(&ctx("group"), "Iris", &[], &cur),
            "Riley: hi all"
        );
    }

    #[test]
    fn replay_is_capped_to_recent_messages() {
        let replay: Vec<Message> = (0..100)
            .map(|i| msg("user", "text", Some("u1"), &format!("m{i}")))
            .collect();
        let cur = msg("user", "text", Some("u1"), "now");
        let p = render_prompt(&ctx("main"), "Iris", &replay, &cur);
        assert!(p.contains("Sam: m99"));
        assert!(!p.contains("Sam: m10\n"));
        assert_eq!(p.matches("Sam: m").count(), MAX_REPLAY_MESSAGES);
    }

    #[test]
    fn entity_kinds_from_mcp_activity() {
        let a = |name: &str| Activity {
            id: name.into(),
            kind: "mcp".into(),
            name: name.into(),
            status: "completed".into(),
            summary: String::new(),
            auth_error: false,
        };
        let kinds = changed_entity_kinds(&[
            a("iris/list_ideas"),
            a("iris/create_goal"),
            a("iris/update_goal"),
            a("iris/create_feed_post"),
            a("other/create_goal"),
            a("iris/create_card"),
        ]);
        assert_eq!(kinds, vec!["goals", "feed"]);
    }

    #[test]
    fn failure_texts_are_actionable() {
        assert!(failure_text("auth_required", "claude").contains("/login"));
        assert!(failure_text("auth_required", "codex").contains("codex login"));
        assert!(failure_text("timeout", "").contains("try again"));
        let both = failure_text("auth_required", "claude,codex");
        assert!(
            both.contains("/login") && both.contains("codex login"),
            "{both}"
        );
    }
}
