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
            let _ = g.cancel.send(true);
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
}

type Slot = Arc<tokio::sync::Mutex<Option<Arc<Session>>>>;

#[derive(Default)]
pub struct Gateway {
    grants: Mutex<HashMap<String, Arc<Grant>>>,
    /// Running connector sessions, by connector id.
    slots: Mutex<HashMap<String, Slot>>,
    /// Tool lists by connector id, with the fingerprint they belong to.
    tools: Mutex<HashMap<String, (String, Vec<Value>)>>,
    /// Approval cards waiting for the owner, by card message id.
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    /// Last start failure per connector id (owner diagnostics).
    errors: Mutex<HashMap<String, String>>,
}

impl Gateway {
    fn slot(&self, id: &str) -> Slot {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    fn grant(&self, token: &str) -> Option<Arc<Grant>> {
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
                connectors: names.clone(),
                cancel,
            }),
        );
    // The CLI must wait for the owner's answer: allow the approval window
    // plus the call itself.
    let timeout = state.cfg.safety.approval_timeout_secs + CALL_TIMEOUT.as_secs();
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
                "{label} (the `{}` tools) is connected as {account}; when one of its tools asks \
                 for the user's email address, use that one.",
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

fn launch_for(state: &AppState, con: &Connector, bearer: Option<&str>) -> Result<Launch, String> {
    let mut env = open_secrets(state, con, "env", &con.config.env)?;
    let mut headers = open_secrets(state, con, "headers", &con.config.headers)?;
    let cwd = connector_dir(state, &con.id);
    crate::config::ensure_private_dir(&cwd).map_err(|e| e.to_string())?;
    let token_env = con.config.oauth.as_ref().and_then(|o| o.token_env.clone());
    match (con.transport.as_str(), &con.config.command) {
        ("stdio", Some(command)) => {
            if let (Some(k), Some(t)) = (token_env, bearer) {
                env.insert(k, t.to_string());
            }
            Ok(Launch::Stdio {
                command: command.clone(),
                args: con.config.args.clone(),
                env,
                cwd,
            })
        }
        ("http", command) => {
            let url = con.config.url.clone().ok_or("no web address saved")?;
            if let Some(t) = bearer {
                headers.insert("Authorization".into(), format!("Bearer {t}"));
            }
            match command {
                Some(command) => Ok(Launch::Serve {
                    command: command.clone(),
                    args: con.config.args.clone(),
                    env,
                    cwd,
                    url,
                    headers,
                }),
                None => Ok(Launch::Http { url, headers }),
            }
        }
        _ => Err("this connection is missing its program".into()),
    }
}

/// The running session for a connector, started (or restarted) as needed.
async fn session(state: &AppState, con: &Connector) -> Result<Arc<Session>, CallError> {
    let bearer = crate::oauth::bearer(state, con).await?;
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
            // It stopped on its own: keep its last words for the owner.
            let tail = s.upstream.stderr_tail();
            if let Some(last) = tail.lines().rev().find(|l| !l.trim().is_empty()) {
                state
                    .gateway
                    .errors
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(con.id.clone(), truncate_chars(last.trim(), 300));
            }
        }
        if s.upstream.alive() && s.fingerprint == fp && (same_bearer || !token_env) {
            if !same_bearer {
                if let Some(t) = &bearer {
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
    let launch = launch_for(state, con, bearer.as_deref()).map_err(CallError::Other)?;
    crate::info!("connector {}: starting", con.name);
    let upstream = match Upstream::start(launch, START_TIMEOUT).await {
        Ok(u) => u,
        Err(e) => {
            let msg = truncate_chars(&e.to_string(), 300);
            state
                .gateway
                .errors
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(con.id.clone(), msg.clone());
            crate::warn!("connector {}: failed to start", con.name);
            return Err(match e {
                UpstreamError::Auth(_) => CallError::Auth,
                _ => CallError::Other(format!("{} could not start: {msg}", con.name)),
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
    });
    *guard = Some(s.clone());
    Ok(s)
}

/// Stop a connector's program or session (it was edited, turned off, or
/// removed). The next call starts it again if allowed.
pub async fn stop(state: &AppState, connector_id: &str) {
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
        let idle = guard
            .as_ref()
            .is_some_and(|s| s.last_used.load(Ordering::SeqCst) < cutoff || !s.upstream.alive());
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

impl From<crate::oauth::NeedsSignIn> for CallError {
    fn from(_: crate::oauth::NeedsSignIn) -> Self {
        CallError::Auth
    }
}

/// The connector's tools, from cache or the running server.
async fn upstream_tools(state: &AppState, con: &Connector) -> Result<Vec<Value>, CallError> {
    let fp = fingerprint(con);
    if let Some((f, tools)) = state
        .gateway
        .tools
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&con.id)
        && *f == fp
    {
        return Ok(tools.clone());
    }
    let s = session(state, con).await?;
    let tools = match s.upstream.list_tools(START_TIMEOUT).await {
        Ok(t) => t,
        Err(UpstreamError::Auth(_)) => return Err(CallError::Auth),
        Err(e) => return Err(CallError::Other(e.to_string())),
    };
    state
        .gateway
        .tools
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(con.id.clone(), (fp, tools.clone()));
    Ok(tools)
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
    let tools = match upstream_tools(state, con).await {
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
                    Verdict::Allow(_) => if cat == Category::Send { "asks unless trusted" } else { "runs" },
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

/// Run one tool call through the gate. Returns an MCP `CallToolResult`.
pub async fn call_tool(
    state: &AppState,
    grant: &Grant,
    con: &Connector,
    tool: &str,
    args: &Value,
) -> Value {
    if state.is_paused().await {
        return tool_error(
            "Iris is paused by the owner, so nothing was done. Tell the user it will work \
             again once they resume Iris.",
        );
    }
    if !con.enabled {
        return tool_error("This connection is turned off, so nothing was done.");
    }
    if let Some(reason) = &con.paused_reason {
        return tool_error(&format!(
            "This connection is paused ({reason}), so nothing was done. The owner can resume \
             it in the app."
        ));
    }
    let tools = upstream_tools(state, con).await;
    let annotations = tools
        .as_ref()
        .ok()
        .and_then(|t| t.iter().find(|x| x["name"] == tool))
        .map(|t| t["annotations"].clone())
        .unwrap_or(Value::Null);
    let category = policy::classify(tool, &annotations);
    let recipients = policy::recipients(args);
    let label = service_label(con);
    let preview = policy::preview(&label, tool, category, args);
    let trusted = state.trusted_recipients().await;
    let known = state
        .db
        .call(entities::known_recipients)
        .await
        .unwrap_or_default();
    let verdict = policy::decide(
        &con.policy,
        tool,
        category,
        &recipients,
        &|r| trusted.iter().any(|t| t == r),
        &|r| known.iter().any(|k| k == r),
    );
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
        created_at: now_ms(),
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
    if let Some(trip) = tripwire(state, con, category).await {
        call.decision = "blocked".into();
        call.reason = trip.clone();
        record(state, &call).await;
        pause_connector(state, con, &trip).await;
        return tool_error(&format!(
            "Not done: Iris paused this connection because {trip}. The owner was notified and \
             can resume it in the app. Do not try again."
        ));
    }

    let decided_by = match verdict {
        Verdict::Allow(AllowedBy::Trusted) => "trusted",
        Verdict::Allow(_) => "auto",
        Verdict::Ask(reason) => {
            call.reason = reason.clone();
            record(state, &call).await;
            match ask(state, grant, con, &call, &preview, category).await {
                Answer::Approved => {
                    let r = recipients.clone();
                    let _ = state
                        .db
                        .call(move |c| entities::add_known_recipients(c, &r))
                        .await;
                    "approved"
                }
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
        call.decision = decided_by.into();
        record(state, &call).await;
    }
    let result = forward(state, con, tool, args).await;
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

/// Forward a call to the connector, retrying once on a stale session or a
/// refreshed token.
async fn forward(
    state: &AppState,
    con: &Connector,
    tool: &str,
    args: &Value,
) -> Result<Value, CallError> {
    for attempt in 0..2 {
        let s = session(state, con).await?;
        s.last_used.store(now_ms(), Ordering::SeqCst);
        match s.upstream.call_tool(tool, args, CALL_TIMEOUT).await {
            Ok(v) => {
                if s.upstream.tools_changed() {
                    state
                        .gateway
                        .tools
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&con.id);
                }
                return Ok(v);
            }
            Err(UpstreamError::SessionExpired) if attempt == 0 => {
                stop(state, &con.id).await;
            }
            Err(UpstreamError::Auth(_)) if attempt == 0 && con.config.oauth.is_some() => {
                // The token may have been revoked early: refresh and retry.
                crate::oauth::invalidate(state, con).await;
                stop(state, &con.id).await;
            }
            Err(UpstreamError::Auth(_)) => return Err(CallError::Auth),
            Err(UpstreamError::Timeout) => {
                return Err(CallError::Other("it did not answer in time".into()));
            }
            Err(e) => return Err(CallError::Other(e.to_string())),
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

/// A tripwire that this call would cross, in plain words.
async fn tripwire(state: &AppState, con: &Connector, category: Category) -> Option<String> {
    let id = con.id.clone();
    let since = now_ms() - HOUR_MS;
    let (calls, sends, bytes) = state
        .db
        .call(move |c| entities::connector_usage_since(c, &id, since))
        .await
        .ok()?;
    let s = &state.cfg.safety;
    if category == Category::Send && sends >= s.max_sends_per_hour as i64 {
        return Some(format!(
            "it tried to send more than {} messages in an hour",
            s.max_sends_per_hour
        ));
    }
    if calls >= s.max_calls_per_hour as i64 {
        return Some(format!(
            "it was used more than {} times in an hour",
            s.max_calls_per_hour
        ));
    }
    if bytes >= s.max_read_mb_per_hour as i64 * 1024 * 1024 {
        return Some(format!(
            "it read more than {} MB of data in an hour, which looks like a bulk export",
            s.max_read_mb_per_hour
        ));
    }
    None
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
    let timeout = Duration::from_secs(state.cfg.safety.approval_timeout_secs);
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
    let conv = grant.card_conversation_id.clone();
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
        .insert(msg.id.clone(), tx);
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
    let answer = tokio::select! {
        r = rx => match r {
            Ok(true) => Answer::Approved,
            Ok(false) => Answer::Denied,
            Err(_) => Answer::Cancelled,
        },
        _ = tokio::time::sleep(timeout) => Answer::Expired,
        _ = cancel.wait_for(|c| *c) => Answer::Cancelled,
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
    let (id, d) = (call.id.clone(), final_state.clone());
    let _ = state
        .db
        .call(move |c| entities::set_call_decision(c, &id, &d, None))
        .await;
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
    if let Some(tx) = state
        .gateway
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(card_id)
    {
        let _ = tx.send(approve);
    }
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
    let ids: Vec<String> = state
        .gateway
        .slots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .keys()
        .cloned()
        .collect();
    for id in ids {
        stop(state, &id).await;
    }
    cancel_all_grants(state);
    crate::warn!("all connectors disconnected by the owner ({n} were on)");
    n
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
        let _ = g.cancel.send(true);
    }
}

#[cfg(test)]
mod tests;
