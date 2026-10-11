//! The connector gateway. Every connector (MCP server the owner connected)
//! is reached through Iris instead of being handed to the CLI:
//!
//! - Each turn gets one small stdio proxy per connector (`iris
//!   mcp-gateway`, see `proxy.rs`) under the connector's own name, so tool
//!   names stay `<connector>/<tool>` for every CLI (Claude Code, Codex,
//!   Gemini CLI, OpenCode). The proxy forwards `tools/list` and
//!   `tools/call` over a private Unix socket to the daemon, with a token
//!   that is only valid for that turn.
//! - The daemon runs the real MCP servers (stdio programs with their
//!   secrets in the environment, or remote HTTP servers with their headers
//!   or OAuth token). The agent never sees a connector secret.
//! - The approval gate classifies each call from its real name and
//!   arguments (`policy.rs`). Sending, buying, deleting, exporting, and
//!   reaching someone new wait for the owner: Iris posts an approval card
//!   built from the arguments, pushes a notification, and holds the call
//!   until the owner decides, the wait times out (denied), or the turn
//!   ends. Tripwires (too many sends or too much data in an hour) pause the
//!   connector and tell the owner. Every call is recorded.
//!
//! Iris does not decide what to call or run any tool itself; it forwards
//! the CLI's calls and enforces the owner's rules on them.

pub mod policy;
pub mod proxy;
pub mod upstream;

use crate::db;
use crate::entities::{self, Connector, ConnectorCall, ConnectorState};
use crate::harness::{McpServer, McpTransport};
use crate::state::AppState;
use crate::util::{new_id, now_ms, truncate_chars};
use policy::{AllowedBy, Category, Verdict};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, watch};
use upstream::{Launch, Upstream, UpstreamError};

/// Environment variable carrying the per-turn gateway token to the proxy.
pub const TOKEN_ENV: &str = "IRIS_GATEWAY_TOKEN";
/// Largest request a proxy may send (tool arguments).
const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
/// How long a connector may take to start and list its tools (the first
/// start can download the server).
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// How long one forwarded tool call may run.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
const HOUR_MS: i64 = 60 * 60 * 1000;

/// One turn's permission to use connectors.
pub struct Grant {
    pub owner_user_id: String,
    pub conversation_id: String,
    /// Where approval cards go (the turn's chat, or the Main chat for
    /// scheduled jobs, which run in hidden conversations).
    pub card_conversation_id: String,
    pub message_id: String,
    /// The turn's conversation, which must stay the owner's alone (None
    /// for scheduled jobs).
    private_conversation: Option<String>,
    /// Connector names this turn may use.
    pub connectors: Vec<String>,
    cancel: watch::Sender<bool>,
}

/// Removes the grant (and cancels its pending approvals) when the turn
/// attempt ends.
pub struct GrantGuard {
    state: AppState,
    token: String,
}

impl Drop for GrantGuard {
    fn drop(&mut self) {
        let grant = self
            .state
            .gateway
            .grants
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.token);
        if let Some(g) = grant {
            // Sticky: a call that subscribes later still sees it.
            g.cancel.send_replace(true);
        }
    }
}

struct Session {
    upstream: Upstream,
    /// Fingerprint of the launch settings it was started with.
    fingerprint: String,
    /// Bearer token it currently uses (OAuth connectors).
    bearer: Mutex<Option<String>>,
    last_used: AtomicI64,
    /// Calls running right now (an idle reaper must not stop them).
    inflight: std::sync::atomic::AtomicUsize,
    /// Secret values it was given (including every bearer token swapped
    /// in since), to scrub from diagnostics.
    secrets: Mutex<Vec<String>>,
}

impl Session {
    fn scrub(&self, text: &str) -> String {
        redact(
            text,
            &self.secrets.lock().unwrap_or_else(|p| p.into_inner()),
        )
    }

    /// Scrub every string in a JSON value (a tool's error result, which
    /// often echoes its configuration).
    fn scrub_value(&self, v: &mut Value) {
        match v {
            Value::String(s) => *s = self.scrub(s),
            Value::Array(a) => a.iter_mut().for_each(|x| self.scrub_value(x)),
            Value::Object(m) => m.values_mut().for_each(|x| self.scrub_value(x)),
            _ => {}
        }
    }
}

type Slot = Arc<tokio::sync::Mutex<Option<Arc<Session>>>>;

#[derive(Default)]
pub struct Gateway {
    grants: Mutex<HashMap<String, Arc<Grant>>>,
    /// Running connector sessions, by connector id.
    slots: Mutex<HashMap<String, Slot>>,
    /// Tool lists by connector id, with the fingerprint they belong to.
    tools: Mutex<HashMap<String, (String, Vec<Value>)>>,
    /// Approval cards waiting for the owner, by card message id, with
    /// their connector id.
    pending: Mutex<HashMap<String, (String, oneshot::Sender<bool>)>>,
    /// Last start failure per connector id (owner diagnostics).
    errors: Mutex<HashMap<String, String>>,
    /// Wakes a connector start that `stop` wants abandoned.
    stops: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    /// Counts `cancel_approvals` (all, and per connector id), so a card
    /// posted while its approvals were being cancelled is cancelled too.
    cancel_epochs: Mutex<(u64, HashMap<String, u64>)>,
}

impl Gateway {
    fn cancel_epoch(&self, connector_id: &str) -> (u64, u64) {
        let e = self.cancel_epochs.lock().unwrap_or_else(|p| p.into_inner());
        (e.0, e.1.get(connector_id).copied().unwrap_or(0))
    }

    fn slot(&self, id: &str) -> Slot {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    fn stopper(&self, id: &str) -> Arc<tokio::sync::Notify> {
        self.stops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    pub(crate) fn grant(&self, token: &str) -> Option<Arc<Grant>> {
        self.grants
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(token)
            .cloned()
    }

    /// Why the connector's program last failed to start, if it did.
    pub fn last_error(&self, connector_id: &str) -> Option<String> {
        self.errors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(connector_id)
            .cloned()
    }

    /// Number of approval cards currently waiting.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

pub fn socket_path(paths: &crate::config::Paths) -> PathBuf {
    paths.run_dir().join("gateway.sock")
}

/// Private working directory of one connector's program (outside the
/// agent's workspace).
pub fn connector_dir(state: &AppState, id: &str) -> PathBuf {
    state.paths.data_dir.join("connectors").join(id)
}

// --------------------------------------------------------------- per turn

/// The connectors a turn may use, as MCP servers for the CLI (one gateway
/// proxy each), plus the guard that ends the grant. Only the owner's own
/// conversations and jobs get connectors; shared chats never do.
pub async fn grant_for_turn(
    state: &AppState,
    user_id: &str,
    conversation_id: &str,
    conversation_kind: &str,
    message_id: &str,
) -> Option<(Vec<McpServer>, GrantGuard)> {
    if state.is_paused().await {
        return None;
    }
    let uid = user_id.to_string();
    let (owner, connectors) = state
        .db
        .call(move |c| {
            let owner = db::owner_user_id(c)?;
            Ok((owner, entities::list_connectors(c, true)?))
        })
        .await
        .ok()?;
    let owner = owner?;
    if owner != uid {
        return None;
    }
    let names: Vec<String> = connectors
        .iter()
        .filter(|c| c.active())
        .map(|c| c.name.clone())
        .collect();
    if names.is_empty() {
        return None;
    }
    let card_conversation_id = if conversation_kind == "job" {
        let o = owner.clone();
        state
            .db
            .call(move |c| db::ensure_main_conversation(c, &o))
            .await
            .ok()?
    } else {
        conversation_id.to_string()
    };
    let token = crate::util::hex(&crate::util::random_bytes::<32>());
    let (cancel, _) = watch::channel(false);
    state
        .gateway
        .grants
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(
            token.clone(),
            Arc::new(Grant {
                owner_user_id: owner,
                conversation_id: conversation_id.to_string(),
                card_conversation_id,
                message_id: message_id.to_string(),
                private_conversation: (conversation_kind != "job")
                    .then(|| conversation_id.to_string()),
                connectors: names.clone(),
                cancel,
            }),
        );
    // The CLI must wait for the owner's answer: allow the approval window
    // plus the call itself.
    let timeout = state.cfg.approval_timeout_secs() + CALL_TIMEOUT.as_secs();
    let servers = names
        .into_iter()
        .map(|name| McpServer {
            name: name.clone(),
            transport: McpTransport::Stdio {
                command: state.exe.to_string_lossy().into_owned(),
                args: vec![
                    "mcp-gateway".into(),
                    "--data-dir".into(),
                    state.paths.data_dir.to_string_lossy().into_owned(),
                    "--connector".into(),
                    name,
                ],
                env: BTreeMap::from([(TOKEN_ENV.to_string(), token.clone())]),
            },
            timeout_secs: Some(timeout),
        })
        .collect();
    Some((
        servers,
        GrantGuard {
            state: state.clone(),
            token,
        },
    ))
}

/// Notes for the system prompt about the owner's connected services.
pub async fn connector_notices(state: &AppState, user_id: &str) -> Vec<String> {
    let uid = user_id.to_string();
    let Ok(Some(list)) = state
        .db
        .call(move |c| {
            (db::owner_user_id(c)?.as_deref() == Some(uid.as_str()))
                .then(|| entities::list_connectors(c, true))
                .transpose()
        })
        .await
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for con in &list {
        let label = service_label(con);
        if let Some(reason) = &con.paused_reason {
            out.push(format!(
                "{label} is paused because {reason}. The owner can resume it in the app."
            ));
        } else if con.auth_state != "ok" {
            out.push(format!(
                "{label} needs the owner to sign in again; there is a Reconnect card in the Main \
                 chat."
            ));
        } else if let Some(account) = con.config.oauth.as_ref().and_then(|o| o.account.as_ref()) {
            out.push(format!(
                "{label} (the `{}` tools) is available now, connected as {account}; when one of \
                 its tools asks for the user's email address, use that one.",
                con.name
            ));
        } else {
            // The current state, so an earlier pause or sign-out in the
            // conversation is not mistaken for the present.
            out.push(format!(
                "{label} (the `{}` tools) is available now.",
                con.name
            ));
        }
    }
    out
}

// ----------------------------------------------------------------- socket

/// Serve proxy requests on the private Unix socket.
pub fn listen(state: AppState) -> crate::util::Result<()> {
    let path = socket_path(&state.paths);
    crate::config::ensure_private_dir(&state.paths.run_dir())?;
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let st = state.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut line = Vec::new();
                let mut r = tokio::io::BufReader::new(r).take(MAX_REQUEST_BYTES);
                if r.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let reply = match serde_json::from_slice::<Value>(&line) {
                    Ok(req) => handle(&st, &req).await,
                    Err(_) => rpc_error(Value::Null, -32700, "parse error"),
                };
                let mut out = reply.to_string().into_bytes();
                out.push(b'\n');
                let _ = w.write_all(&out).await;
                let _ = w.shutdown().await;
            });
        }
    });
    Ok(())
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// A tool result the agent reads as a failure.
pub fn tool_error(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

/// Handle one proxied request: `{"token", "connector", "request"}`.
pub async fn handle(state: &AppState, msg: &Value) -> Value {
    let req = &msg["request"];
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let token = msg["token"].as_str().unwrap_or("");
    let name = msg["connector"].as_str().unwrap_or("");
    let Some(grant) = state.gateway.grant(token) else {
        return rpc_error(id, -32001, "this turn has ended");
    };
    if !grant.connectors.iter().any(|c| c == name) {
        return rpc_error(id, -32001, "this connection is not available in this turn");
    }
    let n = name.to_string();
    let con = match state
        .db
        .call(move |c| entities::get_connector_by_name(c, &n))
        .await
    {
        Ok(Some(c)) => c,
        _ => return rpc_error(id, -32001, "this connection was removed"),
    };
    let result = match req["method"].as_str().unwrap_or("") {
        // Listing never starts a connector that may not run now.
        "tools/list" if refusal(state, &grant, &con).await.is_some() => json!({"tools": []}),
        "tools/list" => match list_tools(state, &con).await {
            Ok(tools) => json!({"tools": tools}),
            Err(e) => return rpc_error(id, -32002, &e),
        },
        "tools/call" => {
            let tool = req["params"]["name"].as_str().unwrap_or("").to_string();
            let args = req["params"]
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call_tool(state, &grant, &con, &tool, &args).await
        }
        _ => return rpc_error(id, -32601, "method not found"),
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

// --------------------------------------------------------------- sessions

/// Open the connector's sealed secrets in memory.
fn open_secrets(
    state: &AppState,
    con: &Connector,
    field: &str,
    m: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    m.iter()
        .map(|(k, v)| {
            state
                .vault
                .open_str(v, &crate::server::connector_aad(&con.id, field, k))
                .map(|plain| (k.clone(), plain))
                .map_err(|_| "a saved secret could not be decrypted".to_string())
        })
        .collect()
}

/// Settings that require a restart when they change (not OAuth tokens,
/// which are swapped in place).
fn fingerprint(con: &Connector) -> String {
    let c = &con.config;
    crate::util::sha256_hex(
        &json!([
            con.transport,
            c.command,
            c.args,
            c.env,
            c.url,
            c.headers,
            c.oauth.as_ref().map(|o| (&o.token_env, &o.client_id))
        ])
        .to_string(),
    )
}

/// Replace secret values in text shown to the owner or the agent.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets.iter().filter(|s| s.len() >= 4) {
        out = out.replace(s.as_str(), "[hidden]");
    }
    out
}

fn launch_for(
    state: &AppState,
    con: &Connector,
    bearer: Option<&str>,
) -> Result<(Launch, Vec<String>), String> {
    let mut env = open_secrets(state, con, "env", &con.config.env)?;
    let mut headers = open_secrets(state, con, "headers", &con.config.headers)?;
    let mut secrets: Vec<String> = env.values().chain(headers.values()).cloned().collect();
    secrets.extend(bearer.map(String::from));
    if let Some(o) = &con.config.oauth
        && let Some(cs) = &o.client_secret
        && let Ok(plain) = state
            .vault
            .open_str(cs, &format!("connector:{}:oauth:client_secret", con.id))
    {
        secrets.push(plain);
    }
    // Connectors saved before the gateway ran in the agent's workspace and
    // may use paths relative to it; new ones get a private folder.
    let cwd = if con.workspace_cwd {
        state.paths.workspace_dir()
    } else {
        connector_dir(state, &con.id)
    };
    crate::config::ensure_private_dir(&cwd).map_err(|e| e.to_string())?;
    let token_env = con.config.oauth.as_ref().and_then(|o| o.token_env.clone());
    match (con.transport.as_str(), &con.config.command) {
        ("stdio", Some(command)) => {
            if let (Some(k), Some(t)) = (token_env, bearer) {
                env.insert(k, t.to_string());
            }
            Ok((
                Launch::Stdio {
                    command: command.clone(),
                    args: con.config.args.clone(),
                    env,
                    cwd,
                },
                secrets,
            ))
        }
        ("http", command) => {
            let url = con.config.url.clone().ok_or("no web address saved")?;
            if let Some(t) = bearer {
                headers.insert("Authorization".into(), format!("Bearer {t}"));
            }
            let launch = match command {
                Some(command) => Launch::Serve {
                    command: command.clone(),
                    args: con.config.args.clone(),
                    env,
                    cwd,
                    url,
                    headers,
                },
                None => Launch::Http { url, headers },
            };
            Ok((launch, secrets))
        }
        _ => Err("this connection is missing its program".into()),
    }
}

/// The running session for a connector, started (or restarted) as needed.
/// A token the server refuses at startup is refreshed and tried once more.
async fn session(state: &AppState, con: &Connector) -> Result<Arc<Session>, CallError> {
    let mut con = con.clone();
    for attempt in 0..2 {
        match session_once(state, &con).await {
            Err(Started::Refused(used)) if attempt == 0 && con.config.oauth.is_some() => {
                crate::oauth::invalidate(state, &con.id, used.as_deref()).await;
                con = current(state, &con.id)
                    .await
                    .ok_or_else(|| CallError::Other("the connection was removed".into()))?;
            }
            Err(Started::Refused(_)) => return Err(CallError::Auth),
            Err(Started::Failed(e)) => return Err(e),
            Ok(s) => return Ok(s),
        }
    }
    Err(CallError::Auth)
}

enum Started {
    /// The server refused this bearer token at startup.
    Refused(Option<String>),
    Failed(CallError),
}

async fn session_once(state: &AppState, con: &Connector) -> Result<Arc<Session>, Started> {
    let bearer = crate::oauth::bearer(state, con)
        .await
        .map_err(|e| Started::Failed(e.into()))?;
    let fp = fingerprint(con);
    let slot = state.gateway.slot(&con.id);
    let mut guard = slot.lock().await;
    if let Some(s) = guard.as_ref() {
        let token_env = con
            .config
            .oauth
            .as_ref()
            .is_some_and(|o| o.token_env.is_some());
        let same_bearer = *s.bearer.lock().unwrap_or_else(|p| p.into_inner()) == bearer;
        if !s.upstream.alive() {
            // It stopped on its own: keep its last words (scrubbed) for
            // the owner.
            let tail = s.upstream.stderr_tail();
            if let Some(last) = tail.lines().rev().find(|l| !l.trim().is_empty()) {
                state
                    .gateway
                    .errors
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(con.id.clone(), truncate_chars(&s.scrub(last.trim()), 300));
            }
        }
        if s.upstream.alive() && s.fingerprint == fp && (same_bearer || !token_env) {
            if !same_bearer {
                if let Some(t) = &bearer {
                    s.secrets
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(t.clone());
                    s.upstream
                        .set_header("Authorization", &format!("Bearer {t}"));
                }
                *s.bearer.lock().unwrap_or_else(|p| p.into_inner()) = bearer.clone();
            }
            s.last_used.store(now_ms(), Ordering::SeqCst);
            return Ok(s.clone());
        }
        s.upstream.close().await;
        *guard = None;
    }
    let (launch, secrets) = launch_for(state, con, bearer.as_deref())
        .map_err(|e| Started::Failed(CallError::Other(e)))?;
    crate::info!("connector {}: starting", con.name);
    let stopper = state.gateway.stopper(&con.id);
    let started = tokio::select! {
        r = Upstream::start(launch, START_TIMEOUT) => r,
        // `stop` (turned off, paused, removed) abandons a slow start; the
        // half-started program is stopped when the future is dropped.
        _ = stopper.notified() => Err(UpstreamError::Transport("stopped while starting".into())),
    };
    let upstream = match started {
        Ok(u) => u,
        Err(e) => {
            // The detail (which may quote the program's output) is only
            // for the owner, and scrubbed of the connector's secrets.
            let msg = truncate_chars(&redact(&e.to_string(), &secrets), 300);
            state
                .gateway
                .errors
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(con.id.clone(), msg);
            crate::warn!("connector {}: failed to start", con.name);
            return Err(match e {
                UpstreamError::Auth(_) => Started::Refused(bearer),
                _ => Started::Failed(CallError::Other(format!(
                    "{} could not start (the owner can see why on its page in the app)",
                    con.name
                ))),
            });
        }
    };
    state
        .gateway
        .errors
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&con.id);
    let s = Arc::new(Session {
        upstream,
        fingerprint: fp,
        bearer: Mutex::new(bearer),
        last_used: AtomicI64::new(now_ms()),
        inflight: Default::default(),
        secrets: Mutex::new(secrets),
    });
    *guard = Some(s.clone());
    Ok(s)
}

/// Stop this exact session if it is still the connector's current one (a
/// late failure must not tear down a newer session).
async fn stop_session(state: &AppState, connector_id: &str, s: &Arc<Session>) {
    let slot = state.gateway.slot(connector_id);
    let mut guard = slot.lock().await;
    if guard.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, s)) {
        guard.take();
        drop(guard);
        s.upstream.close().await;
    }
}

/// Stop a connector's program or session (it was edited, turned off, or
/// removed). The next call starts it again if allowed.
pub async fn stop(state: &AppState, connector_id: &str) {
    // Abandon a start in progress first (it holds the slot).
    state.gateway.stopper(connector_id).notify_waiters();
    let slot = state.gateway.slot(connector_id);
    let old = slot.lock().await.take();
    if let Some(s) = old {
        s.upstream.close().await;
    }
    state
        .gateway
        .tools
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(connector_id);
}

/// Stop sessions idle longer than `safety.idle_minutes` (called by the
/// minute scheduler), so idle connectors cost no memory.
pub async fn reap_idle(state: &AppState) {
    let cutoff = now_ms() - state.cfg.safety.idle_minutes as i64 * 60_000;
    let slots: Vec<(String, Slot)> = state
        .gateway
        .slots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (id, slot) in slots {
        let Ok(mut guard) = slot.try_lock() else {
            continue;
        };
        let idle = guard.as_ref().is_some_and(|s| {
            s.inflight.load(Ordering::SeqCst) == 0
                && (s.last_used.load(Ordering::SeqCst) < cutoff || !s.upstream.alive())
        });
        if idle && let Some(s) = guard.take() {
            s.upstream.close().await;
            crate::info!("connector {id}: stopped (idle)");
        }
    }
}

#[derive(Debug)]
pub enum CallError {
    /// The connector needs the owner to sign in (again).
    Auth,
    Other(String),
}

impl From<crate::oauth::TokenError> for CallError {
    fn from(e: crate::oauth::TokenError) -> Self {
        match e {
            crate::oauth::TokenError::NeedsSignIn => CallError::Auth,
            crate::oauth::TokenError::Unavailable(e) => {
                CallError::Other(format!("the sign-in service could not be reached ({e})"))
            }
        }
    }
}

/// Cached tools for the connector's current settings, if any.
fn cached_tools(state: &AppState, con: &Connector) -> Option<Vec<Value>> {
    let fp = fingerprint(con);
    state
        .gateway
        .tools
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&con.id)
        .filter(|(f, _)| *f == fp)
        .map(|(_, t)| t.clone())
}

/// The connector's tools, from cache or the running server. A token
/// refused while listing is refreshed and tried once more.
async fn upstream_tools(state: &AppState, con: &Connector) -> Result<Vec<Value>, CallError> {
    if let Some(t) = cached_tools(state, con) {
        return Ok(t);
    }
    let mut con = con.clone();
    for attempt in 0..2 {
        let s = session(state, &con).await?;
        match s.upstream.list_tools(START_TIMEOUT).await {
            Ok(tools) => {
                state
                    .gateway
                    .tools
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(con.id.clone(), (fingerprint(&con), tools.clone()));
                return Ok(tools);
            }
            Err(UpstreamError::Auth(_)) if attempt == 0 && con.config.oauth.is_some() => {
                let used = s.bearer.lock().unwrap_or_else(|p| p.into_inner()).clone();
                crate::oauth::invalidate(state, &con.id, used.as_deref()).await;
                stop_session(state, &con.id, &s).await;
                con = current(state, &con.id)
                    .await
                    .ok_or_else(|| CallError::Other("the connection was removed".into()))?;
            }
            Err(UpstreamError::Auth(_)) => return Err(CallError::Auth),
            Err(e) => {
                // Details (scrubbed) for the owner only.
                state
                    .gateway
                    .errors
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(
                        con.id.clone(),
                        truncate_chars(&s.scrub(&e.to_string()), 300),
                    );
                return Err(CallError::Other(format!(
                    "{} could not list its tools (the owner can see why on its page in the app)",
                    con.name
                )));
            }
        }
    }
    Err(CallError::Auth)
}

/// Tools as the agent sees them: refused ones left out, ones that wait for
/// the owner marked in their description.
pub async fn list_tools(state: &AppState, con: &Connector) -> Result<Vec<Value>, String> {
    let tools = match upstream_tools(state, con).await {
        Ok(t) => t,
        Err(CallError::Auth) => {
            needs_sign_in(state, con).await;
            return Err(format!("{} needs to be signed in again", con.name));
        }
        Err(CallError::Other(e)) => return Err(e),
    };
    let none = |_: &str| false;
    Ok(tools
        .into_iter()
        .filter_map(|mut t| {
            let name = t["name"].as_str()?.to_string();
            let cat = policy::classify(&name, &t["annotations"]);
            match policy::decide(&con.policy, &name, cat, &[], &none, &none) {
                Verdict::Block(_) => None,
                Verdict::Ask(_) => {
                    let d = t["description"].as_str().unwrap_or("").trim().to_string();
                    t["description"] = format!(
                        "{d}{}(Iris asks the user to approve this before it runs. Wait for \
                         the result; if they say no, do not retry.)",
                        if d.is_empty() { "" } else { " " }
                    )
                    .into();
                    Some(t)
                }
                Verdict::Allow(_) => Some(t),
            }
        })
        .collect())
}

/// Tool list for the owner's connector page (with each tool's category
/// and what the rules do with it). Starts the connector if needed.
pub async fn describe_tools(state: &AppState, con: &Connector) -> Result<Vec<Value>, String> {
    let blocked = if state.is_paused().await {
        Some("Iris is paused")
    } else if !con.enabled {
        Some("This connection is off")
    } else if con.paused_reason.is_some() {
        Some("This connection is paused")
    } else if con.auth_state != "ok" {
        Some("sign in first")
    } else {
        None
    };
    let found = match (blocked, cached_tools(state, con)) {
        (None, _) => upstream_tools(state, con).await,
        (Some(_), Some(t)) => Ok(t),
        (Some(why), None) => return Err(why.into()),
    };
    let tools = match found {
        Ok(t) => t,
        Err(CallError::Auth) => return Err("sign in first".into()),
        Err(CallError::Other(e)) => return Err(e),
    };
    let none = |_: &str| false;
    Ok(tools
        .iter()
        .filter_map(|t| {
            let name = t["name"].as_str()?;
            let cat = policy::classify(name, &t["annotations"]);
            let verdict = policy::decide(&con.policy, name, cat, &[], &none, &none);
            Some(json!({
                "name": name,
                "label": policy::humanize(name),
                "description": truncate_chars(t["description"].as_str().unwrap_or(""), 300),
                "category": cat,
                "rule": con.policy.tools.get(name),
"effect": match verdict {
                    Verdict::Ask(_)
                        if cat == Category::Send
                            && matches!(con.policy.mode, policy::Mode::Standard | policy::Mode::Careful)
                            && !con.policy.tools.contains_key(name) =>
                    {
                        "asks unless trusted"
                    }
                    Verdict::Allow(_) => "runs",
                    Verdict::Ask(_) => "asks",
                    Verdict::Block(_) => "off",
                },
            }))
        })
        .collect())
}

// ------------------------------------------------------------------- calls

/// A short, plain summary for the activity log (never the message body).
fn summary(p: &policy::Preview) -> String {
    let mut parts = Vec::new();
    if !p.to.is_empty() {
        parts.push(format!("to {}", truncate_chars(&p.to, 120)));
    }
    if !p.subject.is_empty() {
        parts.push(truncate_chars(&p.subject, 120));
    }
    parts.join(" · ")
}

/// Why a call may not run right now (Iris paused, the turn ended, the
/// connection off, paused, or signed out), or None.
async fn refusal(state: &AppState, grant: &Grant, con: &Connector) -> Option<String> {
    if state.is_paused().await {
        return Some(
            "Iris is paused by the owner, so nothing was done. Tell the user it will work again \
             once they resume Iris."
                .into(),
        );
    }
    if *grant.cancel.borrow() {
        return Some("This request was cancelled, so nothing was done.".into());
    }
    if let Some(conv) = &grant.private_conversation {
        let (c2, owner) = (conv.clone(), grant.owner_user_id.clone());
        let alone = state
            .db
            .call(move |c| Ok(db::conversation_member_ids(c, &c2)? == [owner]))
            .await
            .unwrap_or(false);
        if !alone {
            grant.cancel.send_replace(true);
            return Some(
                "Someone else joined this chat, so the owner's connections are off here and \
                 nothing was done."
                    .into(),
            );
        }
    }
    if !con.enabled {
        return Some("This connection is turned off, so nothing was done.".into());
    }
    if let Some(reason) = &con.paused_reason {
        return Some(format!(
            "This connection is paused ({reason}), so nothing was done. The owner can resume it \
             in the app."
        ));
    }
    if con.auth_state != "ok" {
        return Some(format!(
            "{} needs the owner to sign in again, so nothing was done.",
            con.name
        ));
    }
    None
}

/// What an approval is bound to: where the call goes and as whom. Token
/// refreshes keep it; a new sign-in, another client, or edited settings
/// change it.
fn target(con: &Connector) -> String {
    let o = con.config.oauth.as_ref();
    crate::util::sha256_hex(
        &json!([
            fingerprint(con),
            o.and_then(|o| o.client_id.clone()),
            o.and_then(|o| o.account.clone()),
            o.and_then(|o| o.connected_at),
        ])
        .to_string(),
    )
}

/// Everything needed to decide a call again right before it runs.
struct Gate<'a> {
    tool: &'a str,
    category: Category,
    recipients: &'a [String],
    decided_by: &'static str,
    target: String,
}

/// Re-decide a call against the connector and rules as they are now.
/// `check_paused`: also refuse a tripwire-paused connector (not for calls
/// already admitted within the limits).
async fn gate_refusal(
    state: &AppState,
    grant: &Grant,
    connector_id: &str,
    gate: &Gate<'_>,
    check_paused: bool,
) -> Option<String> {
    let Some(now) = current(state, connector_id).await else {
        return Some("This connection was removed, so nothing was done.".into());
    };
    let probe = if check_paused {
        now.clone()
    } else {
        Connector {
            paused_reason: None,
            ..now.clone()
        }
    };
    if let Some(r) = refusal(state, grant, &probe).await {
        return Some(r);
    }
    if gate.decided_by == "approved" && target(&now) != gate.target {
        return Some(
            "This connection was changed or signed in again after the owner approved, so \
             nothing was done. Ask again if it is still wanted."
                .into(),
        );
    }
    let trusted = state.trusted_recipients().await;
    let known = state
        .db
        .call(entities::known_recipients)
        .await
        .unwrap_or_default();
    match policy::decide(
        &now.policy,
        gate.tool,
        gate.category,
        gate.recipients,
        &|r| trusted.iter().any(|t| t == r),
        &|r| known.iter().any(|k| k == r),
    ) {
        Verdict::Block(reason) => Some(format!("Not done: {reason}.")),
        Verdict::Ask(_) if gate.decided_by != "approved" => {
            Some("Not done: this connection's rules changed and now ask first. Try again.".into())
        }
        _ => None,
    }
}

/// The connector as it is now (it may have been edited, turned off, or
/// removed while a call waited).
async fn current(state: &AppState, id: &str) -> Option<Connector> {
    let id = id.to_string();
    state
        .db
        .call(move |c| entities::get_connector(c, &id))
        .await
        .ok()
        .flatten()
}

/// Run one tool call through the gate. Returns an MCP `CallToolResult`.
pub async fn call_tool(
    state: &AppState,
    grant: &Grant,
    con: &Connector,
    tool: &str,
    args: &Value,
) -> Value {
    if let Some(r) = refusal(state, grant, con).await {
        // Recorded too (without starting the connector to learn more).
        let category = policy::classify_call(tool, &Value::Null, args);
        let preview = policy::preview(&service_label(con), tool, category, args);
        record(
            state,
            &ConnectorCall {
                id: new_id(),
                connector_id: Some(con.id.clone()),
                connector_name: con.name.clone(),
                tool: tool.to_string(),
                category: category.name().into(),
                decision: "blocked".into(),
                reason: r.clone(),
                summary: summary(&preview),
                conversation_id: Some(grant.conversation_id.clone()),
                message_id: Some(grant.message_id.clone()),
                ..ConnectorCall::default()
            },
        )
        .await;
        return tool_error(&r);
    }
    let bound_target = target(con);
    let tools = upstream_tools(state, con).await;
    let annotations = tools
        .as_ref()
        .ok()
        .and_then(|t| t.iter().find(|x| x["name"] == tool))
        .map(|t| t["annotations"].clone())
        .unwrap_or(Value::Null);
    let category = policy::classify_call(tool, &annotations, args);
    let recipients = policy::audience(tool, args);
    let label = service_label(con);
    let preview = policy::preview(&label, tool, category, args);
    let trusted = state.trusted_recipients().await;
    let known = state
        .db
        .call(entities::known_recipients)
        .await
        .unwrap_or_default();
    let judge = |policy: &policy::Policy| {
        policy::decide(
            policy,
            tool,
            category,
            &recipients,
            &|r| trusted.iter().any(|t| t == r),
            &|r| known.iter().any(|k| k == r),
        )
    };
    let mut verdict = judge(&con.policy);
    if !matches!(verdict, Verdict::Block(_))
        && policy::request_text(args).chars().count() > policy::MAX_DETAILS
    {
        // Never approve (or send) what the owner could not read in full.
        verdict = Verdict::Block(
            "the request is too large to show the owner in full; split it into smaller pieces"
                .into(),
        );
    }
    let mut call = ConnectorCall {
        id: new_id(),
        connector_id: Some(con.id.clone()),
        connector_name: con.name.clone(),
        tool: tool.to_string(),
        category: category.name().into(),
        decision: "pending".into(),
        summary: summary(&preview),
        conversation_id: Some(grant.conversation_id.clone()),
        message_id: Some(grant.message_id.clone()),
        ..ConnectorCall::default()
    };

    if let Verdict::Block(reason) = &verdict {
        call.decision = "blocked".into();
        call.reason = reason.clone();
        record(state, &call).await;
        return tool_error(&format!(
            "Not done: {reason}. Tell the user; they can change this connection's rules in the app."
        ));
    }
    // Early warning only; the binding check is `admit` at dispatch.
    if let Some(trip) = tripwire_now(state, con, category).await {
        call.decision = "blocked".into();
        call.reason = trip.clone();
        record(state, &call).await;
        pause_connector(state, con, &trip).await;
        return tool_error(&tripped(&trip));
    }

    let decided_by = match verdict {
        Verdict::Allow(AllowedBy::Trusted) => "trusted",
        Verdict::Allow(_) => "auto",
        Verdict::Ask(reason) => {
            call.reason = reason.clone();
            record(state, &call).await;
            match ask(state, grant, con, &call, &preview, category).await {
                Answer::Approved => "approved",
                Answer::Denied => {
                    return tool_error(
                        "The user denied this action, so it was not done. Tell them you did \
                         not do it, and do not try again unless they ask.",
                    );
                }
                Answer::Expired => {
                    return tool_error(
                        "The user did not answer the approval request in time, so it was not \
                         done. Tell them, and offer to try again later.",
                    );
                }
                Answer::Cancelled => {
                    return tool_error("The request was cancelled, so it was not done.");
                }
            }
        }
        Verdict::Block(_) => unreachable!("handled above"),
    };
    if decided_by != "approved" {
        record(state, &call).await;
    }

    // Dispatch gate: things may have changed while the call waited. The
    // connector must still exist, be on, unpaused, signed in, the same
    // target the owner approved, allowed by its current rules, the turn
    // still running, and Iris not paused.
    let gate = Gate {
        tool,
        category,
        recipients: &recipients,
        decided_by,
        target: bound_target,
    };
    if let Some(r) = gate_refusal(state, grant, &con.id, &gate, true).await {
        set_decision(state, &call.id, "cancelled").await;
        return tool_error(&r);
    }
    // Reserve the use against the tripwires atomically with recording it.
    if let Some(trip) = admit(state, con, &call.id, category, decided_by).await {
        pause_connector(state, con, &trip).await;
        return tool_error(&tripped(&trip));
    }
    if decided_by == "approved" {
        let r: Vec<String> = recipients
            .iter()
            .filter(|r| !policy::is_placeholder(r))
            .cloned()
            .collect();
        let _ = state
            .db
            .call(move |c| entities::add_known_recipients(c, &r))
            .await;
    }

    let result = forward(state, grant, &con.id, &gate, args).await;
    let (outcome, bytes) = match &result {
        Ok(v) => (
            if v["isError"] == true { "error" } else { "ok" },
            v.to_string().len() as i64,
        ),
        Err(_) => ("error", 0),
    };
    let id = call.id.clone();
    let _ = state
        .db
        .call(move |c| entities::finish_connector_call(c, &id, Some(outcome), bytes))
        .await;
    crate::info!(
        "connector {}: {tool} ({}) {decided_by}, {outcome}, {bytes} bytes",
        con.name,
        category.name()
    );
    // A large result may have crossed the data tripwire: stop now rather
    // than at the next call.
    if let Some(trip) = volume_tripped(state, con).await {
        pause_connector(state, con, &trip).await;
    }
    match result {
        Ok(v) => v,
        Err(CallError::Auth) => {
            needs_sign_in(state, con).await;
            tool_error(&format!(
                "{} refused the sign-in, so nothing was done. I posted a Reconnect card for the \
                 user; ask them to tap it.",
                con.name
            ))
        }
        Err(CallError::Other(e)) => tool_error(&format!("The connection failed: {e}")),
    }
}

fn tripped(trip: &str) -> String {
    format!(
        "Not done: Iris paused this connection because {trip}. The owner was notified and can \
         resume it in the app. Do not try again."
    )
}

async fn set_decision(state: &AppState, call_id: &str, decision: &'static str) {
    let id = call_id.to_string();
    let _ = state
        .db
        .call(move |c| entities::set_call_decision(c, &id, decision, None))
        .await;
}

/// Forward a call to the connector, retrying once on a stale session or a
/// rejected token (after refreshing it). Right before each attempt the
/// call is decided again (starting the connector can take a while), and
/// it gives up as soon as the turn is cancelled or Iris is paused.
async fn forward(
    state: &AppState,
    grant: &Grant,
    connector_id: &str,
    gate: &Gate<'_>,
    args: &Value,
) -> Result<Value, CallError> {
    for attempt in 0..2 {
        // Fresh state every attempt (a refresh may have rotated tokens).
        let con = current(state, connector_id)
            .await
            .ok_or_else(|| CallError::Other("the connection was removed".into()))?;
        let s = session(state, &con).await?;
        // A tripwire pause by another call does not stop one already
        // admitted within the limits.
        if let Some(r) = gate_refusal(state, grant, connector_id, gate, false).await {
            return Err(CallError::Other(r));
        }
        let used = s.bearer.lock().unwrap_or_else(|p| p.into_inner()).clone();
        s.last_used.store(now_ms(), Ordering::SeqCst);
        s.inflight.fetch_add(1, Ordering::SeqCst);
        let mut cancel = grant.cancel.subscribe();
        let result = tokio::select! {
            r = s.upstream.call_tool(gate.tool, args, CALL_TIMEOUT) => r,
            _ = cancel.wait_for(|c| *c) => {
                Err(UpstreamError::Transport("the request was cancelled".into()))
            }
        };
        s.inflight.fetch_sub(1, Ordering::SeqCst);
        s.last_used.store(now_ms(), Ordering::SeqCst);
        match result {
            Ok(mut v) => {
                if v["isError"] == true {
                    s.scrub_value(&mut v);
                }
                if s.upstream.tools_changed() {
                    state
                        .gateway
                        .tools
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(connector_id);
                }
                return Ok(v);
            }
            Err(UpstreamError::SessionExpired) if attempt == 0 => {
                stop_session(state, connector_id, &s).await;
            }
            Err(UpstreamError::Auth(_)) if attempt == 0 && con.config.oauth.is_some() => {
                // The token may have been revoked early: refresh and retry.
                crate::oauth::invalidate(state, connector_id, used.as_deref()).await;
                stop_session(state, connector_id, &s).await;
            }
            Err(UpstreamError::Auth(_)) => return Err(CallError::Auth),
            Err(UpstreamError::Timeout) => {
                return Err(CallError::Other("it did not answer in time".into()));
            }
            Err(e) => return Err(CallError::Other(s.scrub(&e.to_string()))),
        }
    }
    Err(CallError::Auth)
}

async fn record(state: &AppState, call: &ConnectorCall) {
    let c = call.clone();
    let existing = c.id.clone();
    let _ = state
        .db
        .call(move |db| {
            let n: i64 = db.query_row(
                "SELECT COUNT(*) FROM connector_calls WHERE id = ?1",
                [&existing],
                |r| r.get(0),
            )?;
            if n == 0 {
                entities::insert_connector_call(db, &c)
            } else {
                entities::set_call_decision(db, &c.id, &c.decision, c.card_message_id.as_deref())?;
                db.execute(
                    "UPDATE connector_calls SET reason = ?2 WHERE id = ?1",
                    rusqlite::params![c.id, c.reason],
                )?;
                Ok(())
            }
        })
        .await;
}

/// Display name of the service a connector reaches.
pub fn service_label(con: &Connector) -> String {
    crate::connectors::catalog_entry(&con.service)
        .filter(|e| e.id != "custom")
        .map(|e| e.label.to_string())
        .unwrap_or_else(|| con.name.clone())
}

// --------------------------------------------------------------- tripwires

/// Which tripwire a call would cross, given the last hour's usage.
fn tripwire(
    cfg: &crate::config::SafetyConfig,
    usage: (i64, i64, i64),
    category: Category,
) -> Option<String> {
    let (calls, sends, bytes) = usage;
    if category == Category::Send && sends >= cfg.max_sends_per_hour as i64 {
        return Some(format!(
            "it tried to send more than {} messages in an hour",
            cfg.max_sends_per_hour
        ));
    }
    if calls >= cfg.max_calls_per_hour as i64 {
        return Some(format!(
            "it was used more than {} times in an hour",
            cfg.max_calls_per_hour
        ));
    }
    if bytes >= cfg.max_read_mb_per_hour as i64 * 1024 * 1024 {
        return Some(format!(
            "it read more than {} MB of data in an hour, which looks like a bulk export",
            cfg.max_read_mb_per_hour
        ));
    }
    None
}

/// Early check before asking (so the owner is not asked for a call that
/// cannot run).
async fn tripwire_now(state: &AppState, con: &Connector, category: Category) -> Option<String> {
    let id = con.id.clone();
    let since = now_ms() - HOUR_MS;
    let usage = state
        .db
        .call(move |c| entities::connector_usage_since(c, &id, since))
        .await
        .ok()?;
    tripwire(&state.cfg.safety, usage, category)
}

/// Check the tripwires and record the call as running in one database
/// step, so concurrent calls (and approvals released together) cannot all
/// pass against the same count. Returns the tripwire crossed, if any (the
/// call is then recorded as blocked).
async fn admit(
    state: &AppState,
    con: &Connector,
    call_id: &str,
    category: Category,
    decided_by: &'static str,
) -> Option<String> {
    let (id, cid) = (con.id.clone(), call_id.to_string());
    let cfg = state.cfg.safety.clone();
    let since = now_ms() - HOUR_MS;
    state
        .db
        .call(move |c| {
            let usage = entities::connector_usage_since(c, &id, since)?;
            match tripwire(&cfg, usage, category) {
                Some(trip) => {
                    entities::set_call_decision(c, &cid, "blocked", None)?;
                    c.execute(
                        "UPDATE connector_calls SET reason = ?2 WHERE id = ?1",
                        rusqlite::params![cid, trip],
                    )?;
                    Ok(Some(trip))
                }
                None => {
                    entities::set_call_decision(c, &cid, decided_by, None)?;
                    c.execute(
                        "UPDATE connector_calls SET admitted_at = ?2 WHERE id = ?1",
                        rusqlite::params![cid, now_ms()],
                    )?;
                    Ok(None)
                }
            }
        })
        .await
        .unwrap_or_else(|_| Some("its activity could not be checked".into()))
}

/// The data tripwire, checked after a result was counted.
async fn volume_tripped(state: &AppState, con: &Connector) -> Option<String> {
    let id = con.id.clone();
    let since = now_ms() - HOUR_MS;
    let (_, _, bytes) = state
        .db
        .call(move |c| entities::connector_usage_since(c, &id, since))
        .await
        .ok()?;
    let max = state.cfg.safety.max_read_mb_per_hour as i64 * 1024 * 1024;
    (bytes >= max).then(|| {
        format!(
            "it read more than {} MB of data in an hour, which looks like a bulk export",
            state.cfg.safety.max_read_mb_per_hour
        )
    })
}

/// Pause a connector after a tripwire and tell the owner (card + push).
pub async fn pause_connector(state: &AppState, con: &Connector, reason: &str) {
    let (id, r) = (con.id.clone(), reason.to_string());
    let _ = state
        .db
        .call(move |c| {
            entities::set_connector_state(
                c,
                &id,
                &ConnectorState {
                    paused: Some(Some(&r)),
                    ..Default::default()
                },
            )
        })
        .await;
    crate::warn!("connector {}: paused by a tripwire", con.name);
    // Calls still waiting for approval are cancelled; ones already
    // admitted (within the limits) finish.
    cancel_approvals(state, Some(&con.id));
    crate::connectors::broadcast_changed(state).await;
    let label = service_label(con);
    let card = json!({
        "type": "action",
        "title": format!("I paused {label}"),
        "body": format!(
            "I stopped using {label} because {reason}. That can mean something is going \
             wrong, so nothing more goes out until you look. Check the activity log, then \
             resume it if everything looks right."
        ),
        "action_label": "Resume",
        "action": {"method": "POST", "path": format!("/api/connectors/{}/resume", con.id), "body": null},
        "owner_only": true,
        "connector_id": con.id,
    });
    post_owner_card(state, card, "safety").await;
}

/// Post a server card in the owner's Main chat and push it.
pub async fn post_owner_card(state: &AppState, card: Value, kind: &'static str) {
    let posted = state
        .db
        .call(move |c| {
            let Some(owner) = db::owner_user_id(c)? else {
                return Ok(None);
            };
            let main = db::ensure_main_conversation(c, &owner)?;
            let m = entities::insert_card(c, &main, &card, "assistant")?;
            Ok(Some((owner, m)))
        })
        .await
        .ok()
        .flatten();
    let Some((owner, msg)) = posted else {
        return;
    };
    state.hub.to_users(
        std::slice::from_ref(&owner),
        json!({"type": "message.created", "conversation_id": msg.conversation_id, "message": msg}),
    );
    crate::notify::push_users(
        state,
        std::slice::from_ref(&owner),
        json!({
            "title": msg.content,
            "body": msg.card.as_ref().and_then(|c| c["body"].as_str()).map(|b| truncate_chars(b, 180)).unwrap_or_default(),
            "conversation_id": msg.conversation_id,
            "message_id": msg.id,
            "kind": kind,
        }),
    );
}

/// The connector refused its sign-in: mark it and post a Reconnect card.
pub async fn needs_sign_in(state: &AppState, con: &Connector) {
    if con.config.oauth.is_some() {
        let id = con.id.clone();
        let _ = state
            .db
            .call(move |c| {
                entities::set_connector_state(
                    c,
                    &id,
                    &ConnectorState {
                        auth_state: Some("needs_reconnect"),
                        ..Default::default()
                    },
                )
            })
            .await;
    }
    stop(state, &con.id).await;
    crate::connectors::broadcast_changed(state).await;
    crate::login::on_connector_auth_error(state, &con.name).await;
}

// ---------------------------------------------------------------- approvals

enum Answer {
    Approved,
    Denied,
    Expired,
    Cancelled,
}

/// Post the approval card, push it, and hold the call until the owner
/// decides, the wait times out, or the turn ends.
async fn ask(
    state: &AppState,
    grant: &Grant,
    con: &Connector,
    call: &ConnectorCall,
    p: &policy::Preview,
    category: Category,
) -> Answer {
    let timeout = Duration::from_secs(state.cfg.approval_timeout_secs());
    let expires_at = now_ms() + timeout.as_millis() as i64;
    let mut card = entities::approval_card(&entities::ApprovalRequest {
        title: &p.title,
        service: &service_label(con),
        to: &p.to,
        subject: &p.subject,
        preview: &p.body,
        action_label: &p.action_label,
        approver_user_id: &grant.owner_user_id,
    });
    card["reason"] = call.reason.clone().into();
    card["preview"]["fields"] = p.fields_json();
    card["details"] = p.details.clone().into();
    card["tool"] = call.tool.clone().into();
    card["category"] = category.name().into();
    card["connector_id"] = con.id.clone().into();
    card["expires_at"] = expires_at.into();
    if grant.card_conversation_id == grant.conversation_id {
        // Shown above the reply of the turn that asked.
        card["turn_message_id"] = grant.message_id.clone().into();
    }
    let conv = grant.card_conversation_id.clone();
    let epoch = state.gateway.cancel_epoch(&con.id);
    let inserted = state
        .db
        .call(move |c| {
            let m = entities::insert_card(c, &conv, &card, "assistant")?;
            Ok((
                m.clone(),
                db::conversation_member_ids(c, &m.conversation_id)?,
            ))
        })
        .await;
    let Ok((msg, members)) = inserted else {
        return Answer::Cancelled;
    };
    let (tx, rx) = oneshot::channel();
    state
        .gateway
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(msg.id.clone(), (con.id.clone(), tx));
    // Between posting the card and waiting on it, the owner may already
    // have decided it, or its approvals may have been cancelled.
    let id = msg.id.clone();
    let early = match state
        .db
        .call(move |c| Ok(db::get_message(c, &id)?.and_then(|m| m.card)))
        .await
    {
        Ok(Some(card)) if card["state"] == "approved" => Some(Answer::Approved),
        Ok(Some(card)) if card["state"] == "denied" => Some(Answer::Denied),
        Ok(Some(card)) if card["state"] == "pending" => None,
        _ => Some(Answer::Cancelled),
    }
    .or_else(|| (state.gateway.cancel_epoch(&con.id) != epoch).then_some(Answer::Cancelled));
    let mut c = call.clone();
    c.card_message_id = Some(msg.id.clone());
    record(state, &c).await;
    state.hub.to_users(
        &members,
        json!({"type": "message.created", "conversation_id": msg.conversation_id, "message": msg}),
    );
    crate::notify::push_users(
        state,
        std::slice::from_ref(&grant.owner_user_id),
        json!({
            "title": format!("{} needs your OK", state.agent_name().await),
            "body": truncate_chars(&p.title, 180),
            "conversation_id": msg.conversation_id,
            "message_id": msg.id,
            "kind": "approval",
        }),
    );
    crate::info!("connector {}: {} waiting for approval", con.name, call.tool);
    let mut cancel = grant.cancel.subscribe();
    let answer = match early {
        Some(a) => a,
        None => tokio::select! {
            r = rx => match r {
                Ok(true) => Answer::Approved,
                Ok(false) => Answer::Denied,
                Err(_) => Answer::Cancelled,
            },
            _ = tokio::time::sleep(timeout) => Answer::Expired,
            _ = cancel.wait_for(|c| *c) => Answer::Cancelled,
        },
    };
    state
        .gateway
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&msg.id);
    // The card is the source of truth: a tap that landed just before the
    // timeout still counts, and a late tap cannot revive an expired card.
    let final_state = match answer {
        Answer::Expired => close_card(state, &msg.id, "expired").await,
        Answer::Cancelled => close_card(state, &msg.id, "cancelled").await,
        Answer::Approved => "approved".to_string(),
        Answer::Denied => "denied".to_string(),
    };
    let decision = match final_state.as_str() {
        "approved" => Answer::Approved,
        "denied" => Answer::Denied,
        "cancelled" => Answer::Cancelled,
        _ => Answer::Expired,
    };
    // An approval is recorded when the call is admitted to run.
    if final_state != "approved" {
        let (id, d) = (call.id.clone(), final_state.clone());
        let _ = state
            .db
            .call(move |c| entities::set_call_decision(c, &id, &d, None))
            .await;
    }
    decision
}

/// Move a still-pending card to `to`; returns the card's final state.
async fn close_card(state: &AppState, card_id: &str, to: &'static str) -> String {
    let id = card_id.to_string();
    let result = state
        .db
        .call(move |c| {
            let Some(m) = db::get_message(c, &id)? else {
                return Ok((to.to_string(), None));
            };
            let mut card = m.card.clone().unwrap_or_default();
            if card["state"] != "pending" {
                return Ok((card["state"].as_str().unwrap_or(to).to_string(), None));
            }
            card["state"] = to.into();
            card["decided_at"] = now_ms().into();
            Ok((
                to.to_string(),
                Some(db::update_message_card(c, &id, &card)?),
            ))
        })
        .await;
    match result {
        Ok((s, Some(m))) => {
            crate::login::broadcast_updated(state, &m).await;
            s
        }
        Ok((s, None)) => s,
        Err(_) => to.to_string(),
    }
}

/// The owner decided an approval card (`POST /api/messages/{id}/decision`):
/// release the held call.
pub fn decided(state: &AppState, card_id: &str, approve: bool) {
    if let Some((_, tx)) = state
        .gateway
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(card_id)
    {
        let _ = tx.send(approve);
    }
}

/// Cancel approvals waiting on one connector (or all): their cards become
/// `cancelled` and the calls never run.
pub fn cancel_approvals(state: &AppState, connector_id: Option<&str>) {
    {
        let mut e = state
            .gateway
            .cancel_epochs
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match connector_id {
            None => e.0 += 1,
            Some(id) => *e.1.entry(id.to_string()).or_default() += 1,
        }
    }
    let mut pending = state
        .gateway
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    // Dropping the sender wakes the waiter with "cancelled".
    pending.retain(|_, (cid, _)| connector_id.is_some_and(|c| c != cid));
}

/// After a restart nothing waits for old approval cards: close them.
pub async fn expire_stale_approvals(state: &AppState) {
    let closed = state
        .db
        .call(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM messages WHERE kind = 'card'
                   AND json_extract(card, '$.type') = 'approval'
                   AND json_extract(card, '$.state') = 'pending'",
            )?;
            let ids: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for id in &ids {
                if let Some(m) = db::get_message(c, id)? {
                    let mut card = m.card.unwrap_or_default();
                    card["state"] = "expired".into();
                    card["decided_at"] = now_ms().into();
                    db::update_message_card(c, id, &card)?;
                }
            }
            entities::expire_pending_calls(c)?;
            Ok(ids.len())
        })
        .await
        .unwrap_or(0);
    if closed > 0 {
        crate::info!("{closed} approval card(s) from before the restart expired");
    }
}

// ---------------------------------------------------------------- safety

/// "Disconnect everything": turn every connector off, stop their programs,
/// and drop waiting approvals. Returns how many were on.
pub async fn disconnect_all(state: &AppState) -> usize {
    let n = state
        .db
        .call(entities::disable_all_connectors)
        .await
        .unwrap_or(0);
    stop_all(state).await;
    cancel_approvals(state, None);
    crate::oauth::forget_flows(state, None);
    crate::warn!("all connectors disconnected by the owner ({n} were on)");
    n
}

/// Stop every connector's program or session.
pub async fn stop_all(state: &AppState) {
    let ids: Vec<String> = state
        .gateway
        .slots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .keys()
        .cloned()
        .collect();
    // All at once: a remote service slow to end its session must not hold
    // up stopping the others.
    for id in &ids {
        state.gateway.stopper(id).notify_waiters();
    }
    let mut stops = tokio::task::JoinSet::new();
    for id in ids {
        let st = state.clone();
        stops.spawn(async move { stop(&st, &id).await });
    }
    while stops.join_next().await.is_some() {}
}

/// End connector access for the turns in one conversation (stopped, or
/// no longer the owner's alone); their waiting approvals are cancelled.
pub fn cancel_conversation_grants(state: &AppState, conversation_id: &str) {
    for g in state
        .gateway
        .grants
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .filter(|g| g.conversation_id == conversation_id)
    {
        g.cancel.send_replace(true);
    }
}

/// End every turn's connector access (their waiting approvals are
/// cancelled).
pub fn cancel_all_grants(state: &AppState) {
    for g in state
        .gateway
        .grants
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
    {
        g.cancel.send_replace(true);
    }
}

#[cfg(test)]
mod tests;
