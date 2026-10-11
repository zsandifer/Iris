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
use crate::persona::{self, PromptContext, Scope};
use crate::ranking::Slot;
use crate::router::{EngineEnv, NoEngine, Purpose, RouteDecision, RouteInput, select_engine};
use crate::state::AppState;
use crate::util::{new_id, truncate_chars};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
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
    // Connector access ends now, not when the CLI has finished exiting: an
    // approval tapped meanwhile must not run.
    crate::gateway::cancel_conversation_grants(state, conversation_id);
    state
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(conversation_id)
        .map(|tx| tx.send(true).is_ok())
        .unwrap_or(false)
}

struct Ctx {
    /// False when the conversation was deleted while this turn waited.
    exists: bool,
    kind: String,
    members: Vec<String>,
    names: HashMap<String, String>,
    /// The server owner's user id.
    owner: Option<String>,
}

impl Ctx {
    /// Only the owner alone sees their private files, memory, and
    /// connectors; anyone else's chat (or a chat with several people) gets
    /// none of them.
    fn scope(&self) -> Scope {
        match self.members.as_slice() {
            [only] if self.owner.as_deref() == Some(only.as_str()) => Scope::Private,
            [] | [_] => Scope::Personal,
            _ => Scope::Shared,
        }
    }
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
                exists: conv.is_some(),
                kind: conv.map(|c| c.kind).unwrap_or_else(|| "side".into()),
                members,
                names,
                owner: db::owner_user_id(c)?,
            })
        })
        .await
        .unwrap_or(Ctx {
            exists: false,
            kind: "side".into(),
            members: Vec::new(),
            names: HashMap::new(),
            owner: None,
        })
}

/// Call after someone joins or leaves a conversation, or it is deleted.
pub fn membership_changed(state: &AppState) {
    state.membership_generation.fetch_add(1, Ordering::SeqCst);
}

/// The people a turn streams to: its conversation's members when it
/// started, minus anyone who has left since. Nobody is added mid-turn (a
/// newcomer sees the finished reply through the API).
struct Audience {
    conv_id: String,
    users: Vec<String>,
    generation: u64,
    /// The owner, when the turn runs with the owner's private context.
    private_owner: Option<String>,
    /// Someone joined a private turn's conversation: its reply must not be
    /// streamed or kept.
    privacy_lost: bool,
}

impl Audience {
    /// `generation` must be read before `users` was loaded, so a removal
    /// that lands in between is still noticed.
    fn new(conv_id: &str, users: Vec<String>, generation: u64) -> Self {
        Self {
            conv_id: conv_id.to_string(),
            users,
            generation,
            private_owner: None,
            privacy_lost: false,
        }
    }

    /// Narrow to a fresher member list read after `generation`.
    fn refresh(&mut self, members: &[String], generation: u64) {
        self.users.retain(|u| members.contains(u));
        if let Some(owner) = &self.private_owner
            && members != std::slice::from_ref(owner)
        {
            self.privacy_lost = true;
            self.users.clear();
        }
        self.generation = generation;
    }

    async fn current(&mut self, state: &AppState) -> &[String] {
        let generation = state.membership_generation.load(Ordering::SeqCst);
        if generation != self.generation && !self.users.is_empty() {
            let id = self.conv_id.clone();
            match state
                .db
                .call(move |c| db::conversation_member_ids(c, &id))
                .await
            {
                Ok(members) => self.refresh(&members, generation),
                // Fail closed: stop streaming rather than risk a leak.
                Err(_) => {
                    self.users.clear();
                    self.generation = generation;
                }
            }
        }
        &self.users
    }
}

async fn run_locked(
    state: &AppState,
    input: &TurnInput,
    mut cancel: watch::Receiver<bool>,
) -> TurnResult {
    let conv_id = input.conversation_id.as_str();
    let msg_id = input.assistant_message.id.as_str();
    let generation = state.membership_generation.load(Ordering::SeqCst);
    let ctx = load_ctx(state, conv_id).await;
    if !ctx.exists {
        // Deleted while this turn waited in line: never run the CLI for it.
        crate::info!("turn {msg_id}: conversation is gone; skipped");
        return TurnResult {
            message: input.assistant_message.clone(),
            ok: false,
        };
    }
    // Job conversations are hidden; their turns are not streamed to anyone.
    let mut audience = Audience::new(
        conv_id,
        if ctx.kind == "job" {
            Vec::new()
        } else {
            ctx.members.clone()
        },
        generation,
    );

    if state.is_paused().await {
        return finish(
            state,
            input,
            &mut audience,
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
            audience.current(state).await,
            json!({"type": "turn.status", "conversation_id": conv_id, "message_id": msg_id, "status": "queued"}),
        );
    }
    let _permit = tokio::select! {
        p = state.limiter.acquire() => p,
        _ = async { let _ = cancel.wait_for(|c| *c).await; } => {
            return finish(state, input, &mut audience, &Acc::default(), json!(null), Outcome::Cancelled).await;
        }
    };
    // Waiting for a slot can take long: decide who is in the conversation
    // (privacy scope, engine policy) only now that the CLI is about to run.
    let generation = state.membership_generation.load(Ordering::SeqCst);
    let ctx = load_ctx(state, conv_id).await;
    if !ctx.exists {
        crate::info!("turn {msg_id}: conversation is gone; skipped");
        return TurnResult {
            message: input.assistant_message.clone(),
            ok: false,
        };
    }
    audience.refresh(&ctx.members, generation);
    if let Some(author) = &input.user_message.author_user_id
        && !ctx.members.contains(author)
    {
        // The sender left (or was removed) before the turn started; never
        // answer them, least of all with whatever context remains.
        crate::info!("turn {msg_id}: sender is no longer a member; skipped");
        return finish(
            state,
            input,
            &mut audience,
            &Acc::default(),
            json!(null),
            Outcome::Cancelled,
        )
        .await;
    }
    // Engine policy: more than one human. Privacy: see `Ctx::scope`.
    let shared = ctx.members.len() > 1;
    let scope = ctx.scope();
    if scope == Scope::Private {
        audience.private_owner = ctx.owner.clone();
    }
    state.hub.to_users(
        audience.current(state).await,
        json!({"type": "turn.status", "conversation_id": conv_id, "message_id": msg_id, "status": "running"}),
    );

    let catalog = state.catalog();
    let pins = state.slot_pins().await;
    let allow_shared = state.allow_claude_for_shared().await;
    let has_images = !input.user_message.attachments.is_empty();
    let assessment = assess_turn(state, input, shared).await;
    let mut excluded: Vec<String> = Vec::new();
    let mut fresh_session = !input.continuity;
    let mut routing_notes: Vec<String> = Vec::new();
    let mut acc = Acc::default();
    let mut decision: Option<RouteDecision> = None;
    let mut outcome = Outcome::Failed {
        kind: "no_engine",
        detail: "no engine available".into(),
    };

    for _attempt in 0..MAX_ATTEMPTS {
        // Someone joined a private turn's chat (or it was cancelled): no
        // further attempt may run with the owner's context.
        audience.current(state).await;
        if audience.privacy_lost || *cancel.borrow() {
            outcome = Outcome::Cancelled;
            break;
        }
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
        let persona_hash = {
            let (paths, name, kind) = (state.paths.clone(), agent_name.clone(), ctx.kind.clone());
            tokio::task::spawn_blocking(move || persona::persona_hash(&paths, &name, &kind, scope))
                .await
                .unwrap_or_default()
        };
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
        if let Some(s) = &session
            && s.persona_hash != persona_hash
        {
            if persona::hash_scope(&s.persona_hash) != Some(scope.name()) {
                // Who is in the conversation changed (for example a group
                // got its second person): the old session may hold the
                // owner's private context, so never resume it.
                routing_notes.push("people changed; started a new session".into());
                session = None;
            } else if !entry.harness.resume_updates_system_prompt() {
                // The CLI would keep the old persona; start over with replay.
                routing_notes.push("persona changed; started a new session".into());
                session = None;
            }
        }
        let replay = if input.continuity {
            let after = session.as_ref().map_or(0, |s| s.synced_at);
            let (c, before) = (conv_id.to_string(), input.user_message.created_at);
            state
                .db
                .call(move |db| db::replay_messages(db, &c, after, before, MAX_REPLAY_MESSAGES))
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
        let (mcp_servers, _grant) = if caps.mcp {
            mcp_servers(
                state,
                &input.owner_user_id,
                conv_id,
                &ctx.kind,
                msg_id,
                scope,
            )
            .await
        } else {
            (Vec::new(), None)
        };
        if scope == Scope::Private {
            notices.extend(crate::gateway::connector_notices(state, &input.owner_user_id).await);
        }
        let keys = crate::keys::env_for(state, &harness_name).await;
        let participants: Vec<String> = ctx.names.values().cloned().collect();
        // Reads persona files and indexes the memory tree: blocking I/O.
        let system_prompt = {
            let (paths, name, kind, purpose, app_tools) = (
                state.paths.clone(),
                agent_name.clone(),
                ctx.kind.clone(),
                input.purpose,
                !mcp_servers.is_empty(),
            );
            let notices = notices.clone();
            tokio::task::spawn_blocking(move || {
                persona::assemble(
                    &paths,
                    &PromptContext {
                        agent_name: &name,
                        conversation_kind: &kind,
                        purpose,
                        participants: &participants,
                        app_tools,
                        scope,
                        notices: &notices,
                    },
                )
            })
            .await
            .unwrap_or_default()
        };
        let mut spec = TurnSpec {
            turn_id: new_id(),
            prompt,
            images,
            system_prompt,
            model: d.cli_model().to_string(),
            effort: d.effort.clone(),
            resume: session.as_ref().map(|s| s.session_id.clone()),
            mcp_servers,
            cwd: state.paths.workspace_dir(),
            // The owner's memory is only writable in the owner's own chats.
            writable_dirs: if scope == Scope::Private {
                vec![state.paths.memory_dir()]
            } else {
                Vec::new()
            },
            run_dir: state.paths.run_dir(),
            scope: conv_id.to_string(),
            key_env: keys.iter().map(|(k, _)| k.clone()).collect(),
            isolated: scope != Scope::Private,
            own_mcp_servers: Vec::new(),
        };
        // MCP servers configured in the CLI itself are off in every turn
        // (they would bypass Iris's gateway and approvals; in someone
        // else's chat they are also the owner's).
        match own_mcp_servers(state, entry, &program).await {
            Some(names) => spec.own_mcp_servers = names,
            None => {
                // Cannot tell which of the CLI's own tools to switch off:
                // do not use it rather than leave them on.
                crate::warn!("turn {msg_id}: could not list {harness_name}'s own MCP servers");
                routing_notes.push(format!("{} could not be isolated", d.model));
                excluded.push(harness_name);
                continue;
            }
        }
        let invocation = match build_invocation(state, &harness_name, &spec).await {
            Ok(mut i) => {
                if harness_name == "claude" && crate::harness::is_managed(&program) {
                    // Iris updates the engines it installed; the CLI's own
                    // updater could not write there anyway.
                    i.env.push(("DISABLE_AUTOUPDATER".into(), "1".into()));
                }
                // API keys the owner added in the app (after inherited
                // ones were stripped).
                i.env.extend(keys);
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
        // As the CLI knows them (renamed where its own config has the name).
        let server_names: Vec<String> = spec
            .mcp_servers
            .iter()
            .map(|s| crate::harness::unclaimed_name(&s.name, &spec.own_mcp_servers))
            .collect();
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
        let consume_fut = consume(state, &mut audience, conv_id, msg_id, rx);
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
    finish(state, input, &mut audience, &acc, routing, outcome).await
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
/// `shared` applies the individual-use policy to the optional model
/// classifier too: it sees the message, so it is an engine use.
async fn assess_turn(state: &AppState, input: &TurnInput, shared: bool) -> Assessment {
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
        && let Some(effort) = refine_with_model(state, &input.user_message.content, shared).await
    {
        a.signals.push("model_classifier");
        a.effort = effort;
        a.escalate = a.escalate || effort == Effort::High && a.score >= classifier::HIGH_AT;
    }
    a
}

/// Optional: ask the `classifier` slot's model about an ambiguous message.
/// One short, tool-less, session-less run; None on any failure.
async fn refine_with_model(state: &AppState, text: &str, shared: bool) -> Option<Effort> {
    let (res, pin) = state.resolve_slot(Slot::Classifier, shared).await;
    let model_id = res.effective?;
    let catalog = state.catalog();
    let m = catalog.get(&model_id)?;
    let entry = state.harness(&m.harness)?;
    let program = entry.program()?;
    let effort = pin
        .and_then(|p| p.effort)
        .or_else(|| crate::ranking::default_effort(m));
    let keys = crate::keys::env_for(state, &m.harness).await;
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
        key_env: keys.iter().map(|(k, _)| k.clone()).collect(),
        // A one-word answer needs none of the CLI's own tools or memory.
        isolated: true,
        own_mcp_servers: own_mcp_servers(state, entry, &program).await?,
    };
    let mut invocation = build_invocation(state, &m.harness, &spec).await.ok()?;
    invocation.env.extend(keys);
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

/// Build the command line off the async workers: adapters read and encode
/// images and write private files.
async fn build_invocation(
    state: &AppState,
    harness: &str,
    spec: &TurnSpec,
) -> crate::util::Result<crate::harness::Invocation> {
    let (st, name, spec) = (state.clone(), harness.to_string(), spec.clone());
    tokio::task::spawn_blocking(move || match st.harness(&name) {
        Some(e) => e.harness.build(&spec),
        None => Err(format!("unknown harness {name}").into()),
    })
    .await
    .map_err(|e| format!("build task failed: {e}"))?
}

/// MCP servers configured in the CLI itself, for CLIs that switch them off
/// by name (empty for the others). None if the CLI could not be asked.
pub(crate) async fn own_mcp_servers(
    state: &AppState,
    entry: &crate::state::HarnessEntry,
    program: &std::path::Path,
) -> Option<Vec<String>> {
    let Some(args) = entry.harness.own_mcp_servers_args() else {
        return Some(Vec::new());
    };
    let strip = state.strip_env(entry);
    match runner::probe(program, &args, &state.paths.workspace_dir(), &strip, &[]).await {
        Ok((true, out, _)) => entry.harness.parse_own_mcp_servers(&out),
        _ => None,
    }
}

/// In a shared conversation where Claude is the only engine, explain once
/// (per conversation) with a one-tap opt-in card for the owner.
async fn post_policy_card_once(state: &AppState, conv_id: &str) {
    let key = format!("notice.policy_card.{conv_id}");
    let c = conv_id.to_string();
    let posted = state
        .db
        .call(move |db| {
            if db::get_setting(db, &key)?.is_some() {
                return Ok(None);
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
            let m = crate::entities::insert_card(db, &c, &card, "assistant")?;
            db::set_setting(db, &key, &crate::util::now_ms().to_string())?;
            Ok(Some((m, db::conversation_member_ids(db, &c)?)))
        })
        .await;
    // Sent live (turns only re-send the agent's own cards).
    if let Ok(Some((m, members))) = posted {
        state.hub.to_users(
            &members,
            json!({"type": "message.created", "conversation_id": m.conversation_id, "message": m}),
        );
    }
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
    audience: &mut Audience,
    conv_id: &str,
    msg_id: &str,
    mut rx: mpsc::UnboundedReceiver<HarnessEvent>,
) -> Acc {
    let mut acc = Acc::default();
    let mut stopped = false;
    while let Some(ev) = rx.recv().await {
        audience.current(state).await;
        if audience.privacy_lost && !stopped {
            // Someone joined while the owner's private context is in use.
            crate::info!("turn {msg_id}: people changed; stopping");
            cancel(state, conv_id);
            stopped = true;
        }
        match ev {
            HarnessEvent::Session { id } => acc.session = Some(id),
            HarnessEvent::TextDelta { text } => {
                acc.text.push_str(&text);
                state.hub.to_users(
                    audience.current(state).await,
                    json!({"type": "message.delta", "conversation_id": conv_id, "message_id": msg_id, "text": text}),
                );
            }
            HarnessEvent::Activity(a) => {
                let merged = match acc.activities.iter_mut().find(|x| x.id == a.id) {
                    Some(existing) => {
                        merge_activity(existing, &a);
                        existing.clone()
                    }
                    None => {
                        acc.activities.push(a.clone());
                        a
                    }
                };
                state.hub.to_users(
                    audience.current(state).await,
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

/// Fold a later update of the same tool call into what was shown so far.
fn merge_activity(existing: &mut Activity, update: &Activity) {
    existing.status = update.status.clone();
    if !update.summary.is_empty() {
        existing.summary = update.summary.clone();
    }
    if !update.name.is_empty() {
        existing.name = update.name.clone();
    }
    // A connector refusing its sign-in is only known when the call fails.
    existing.auth_error |= update.auth_error;
}

/// Newest message this harness session has now seen: everything up to this
/// turn's reply, plus cards the agent created during the turn. Messages
/// that arrived meanwhile (queued user messages, heartbeat posts) stay
/// unsynced so they are replayed later (see `db::synced_watermark`).
async fn synced_through(state: &AppState, conv_id: &str, placeholder_created_at: i64) -> i64 {
    let c = conv_id.to_string();
    state
        .db
        .call(move |db| db::synced_watermark(db, &c, placeholder_created_at))
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
                    "gemini" => Some("add a Gemini API key in Settings, AI engine"),
                    "opencode" => Some("add a provider key for OpenCode in Settings, AI engine"),
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
        "paused" => "I'm paused right now, so I didn't do anything. You can resume me from \
            my computer screen in the app whenever you're ready."
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
    audience: &mut Audience,
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
    // A reply made with the owner's private context is never kept once
    // someone else has joined the conversation.
    audience.current(state).await;
    let outcome = if audience.privacy_lost {
        meta["withheld"] = "people changed during a private reply".into();
        meta["activity"] = json!([]);
        // Cards the agent showed during this turn came from the same
        // private context.
        let (c, after) = (conv_id.clone(), input.assistant_message.created_at);
        let _ = state
            .db
            .call(move |db| {
                db.execute(
                    "DELETE FROM messages WHERE conversation_id = ?1 AND kind = 'card'
                     AND role = 'assistant' AND created_at > ?2",
                    rusqlite::params![c, after],
                )
            })
            .await;
        Outcome::Cancelled
    } else {
        outcome
    };
    let text = if audience.privacy_lost {
        ""
    } else {
        acc.text.trim()
    };
    let (status, content, ok) = match &outcome {
        Outcome::Complete => ("complete", text.to_string(), true),
        Outcome::Cancelled => ("cancelled", text.to_string(), false),
        Outcome::Failed { kind, detail } => {
            meta["error"] = json!({"kind": kind, "detail": truncate_chars(detail, 500)});
            let text = if text.is_empty() {
                failure_text(kind, detail)
            } else {
                text.to_string()
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
    let audience = audience.current(state).await.to_vec();
    state.hub.to_users(
        &audience,
        json!({"type": "message.completed", "conversation_id": conv_id, "message": message}),
    );

    // Cards the agent created through the Iris MCP bridge during the turn.
    let (c, after) = (conv_id.clone(), input.assistant_message.created_at);
    if let Ok(new) = state
        .db
        .call(move |db| db::messages_between(db, &c, after, i64::MAX))
        .await
    {
        // Agent cards only: server cards (approvals, notices) were sent
        // live when they were posted.
        let agent_card = |m: &Message| {
            m.card.as_ref().is_some_and(|c| {
                crate::entities::CARD_TYPES.contains(&c["type"].as_str().unwrap_or(""))
            })
        };
        for m in new
            .into_iter()
            .filter(|m| m.kind == "card" && agent_card(m))
        {
            state.hub.to_users(
                &audience,
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

/// The Iris MCP bridge plus one gateway proxy per connector the turn may
/// use (with the guard that ends that access when the attempt ends).
///
/// Connectors are the owner's accounts, so they are only attached to the
/// owner's own chats and jobs (the gateway checks this again). In shared
/// conversations (more than one person) the bridge also only offers cards,
/// so other members cannot reach the owner's goals, feed, or library
/// through the agent.
async fn mcp_servers(
    state: &AppState,
    user_id: &str,
    conv_id: &str,
    conv_kind: &str,
    message_id: &str,
    scope: Scope,
) -> (Vec<McpServer>, Option<crate::gateway::GrantGuard>) {
    let shared = scope == Scope::Shared;
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
        timeout_secs: None,
    }];
    if scope != Scope::Private {
        return (servers, None);
    }
    match crate::gateway::grant_for_turn(state, user_id, conv_id, conv_kind, message_id).await {
        Some((connectors, guard)) => {
            servers.extend(connectors);
            (servers, Some(guard))
        }
        None => (servers, None),
    }
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
            exists: true,
            kind: kind.into(),
            members: vec!["u1".into(), "u2".into()],
            names,
            owner: Some("u1".into()),
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
    fn a_failed_call_keeps_its_sign_in_error() {
        let mut shown = Activity {
            id: "t1".into(),
            kind: "mcp".into(),
            name: "notes/search".into(),
            status: "started".into(),
            summary: "q".into(),
            auth_error: false,
        };
        let failed = Activity {
            status: "failed".into(),
            summary: String::new(),
            auth_error: true,
            ..shown.clone()
        };
        merge_activity(&mut shown, &failed);
        assert!(shown.auth_error && shown.status == "failed" && shown.summary == "q");
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
