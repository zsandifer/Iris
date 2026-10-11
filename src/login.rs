//! Engine sign-in from the app. When a CLI the owner uses is signed out,
//! Iris posts a "Reconnect <engine>" card in the Main chat and sends a push
//! notification. Tapping it runs the CLI's own official login command on
//! the server:
//!
//! - Claude Code (`claude auth login --claudeai`): prints an authorize URL,
//!   then reads the code the user copies after approving. Iris relays the
//!   URL to the card and writes the code the user types to the process's
//!   stdin.
//! - Codex (`codex login --device-auth`): prints a URL and a one-time code
//!   and finishes on its own once the user approves.
//! - Gemini CLI and OpenCode cannot sign in without a terminal; their card
//!   explains the one server step instead.
//!
//! Compliance: the code is passed straight to the CLI and never stored or
//! logged. Iris never reads the credential files the CLI writes; success is
//! judged by the CLI's own status command. The card moves through
//! `needs_reconnect` -> `waiting_for_code` / `waiting_for_approval` ->
//! `verifying` -> `connected` (or `failed`, with retry).

use crate::db;
use crate::harness::{AuthState, LoginFlow};
use crate::state::AppState;
use crate::util::{Result, new_id, now_ms};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};

/// Give up on a login nobody finished.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoginStatus {
    pub id: String,
    pub harness: String,
    pub flow: LoginFlow,
    /// starting | waiting_for_code | waiting_for_approval | verifying |
    /// connected | failed | cancelled
    pub state: &'static str,
    /// Where the user approves (authorize page or device page).
    pub url: Option<String>,
    /// Device flow: the one-time code to enter on that page.
    pub user_code: Option<String>,
    /// Friendly reason when the last attempt failed.
    pub error: Option<String>,
    pub started_at: i64,
}

impl LoginStatus {
    pub fn finished(&self) -> bool {
        matches!(self.state, "connected" | "failed" | "cancelled")
    }
}

struct Session {
    status: LoginStatus,
    stdin: Option<Arc<tokio::sync::Mutex<ChildStdin>>>,
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
pub struct LoginManager {
    sessions: Mutex<HashMap<String, Session>>,
}

impl LoginManager {
    pub fn status(&self, harness: &str) -> Option<LoginStatus> {
        self.lock().get(harness).map(|s| s.status.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn update(&self, harness: &str, f: impl FnOnce(&mut LoginStatus)) -> Option<LoginStatus> {
        let mut map = self.lock();
        let s = map.get_mut(harness)?;
        f(&mut s.status);
        Some(s.status.clone())
    }

    /// Update only if `id` is still this harness's session, so a finished
    /// attempt's last steps never touch a newer one.
    fn update_session(
        &self,
        harness: &str,
        id: &str,
        f: impl FnOnce(&mut LoginStatus),
    ) -> Option<LoginStatus> {
        let mut map = self.lock();
        let s = map.get_mut(harness).filter(|s| s.status.id == id)?;
        f(&mut s.status);
        Some(s.status.clone())
    }
}

/// Remove ANSI escape sequences (colors, cursor moves, OSC titles).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            if c != '\r' {
                out.push(c);
            }
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '~' {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// First `https://` URL in a line.
pub fn find_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let url: String = line[start..]
        .chars()
        .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '<' | '>' | ')'))
        .collect();
    (url.len() > "https://".len()).then_some(url)
}

/// A device code such as `ABCD-12345` on its own line.
pub fn find_device_code(line: &str) -> Option<String> {
    let t = line.trim();
    let (a, b) = t.split_once('-')?;
    let ok = |p: &str, min: usize, max: usize| {
        (min..=max).contains(&p.len())
            && p.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    };
    (ok(a, 4, 5) && ok(b, 4, 6)).then(|| t.to_string())
}

/// Text that means the pasted code was rejected.
fn rejected_code(line: &str) -> bool {
    let l = line.to_lowercase();
    l.contains("invalid code")
        || l.contains("invalid authorization code")
        || l.contains("code expired")
}

/// Start (or return the running) login for a harness.
pub async fn start(state: &AppState, harness: &str) -> Result<LoginStatus> {
    let entry = state
        .harness(harness)
        .ok_or_else(|| format!("unknown engine {harness}"))?;
    let spec = entry
        .harness
        .login()
        .ok_or_else(|| entry.harness.manual_login_hint())?;
    let program = entry
        .program()
        .ok_or_else(|| format!("{harness} is not installed"))?;
    // Reserve the session before spawning, under one lock, so two requests
    // at once cannot both start the CLI's login.
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    let status = LoginStatus {
        id: new_id(),
        harness: harness.to_string(),
        flow: spec.flow,
        state: "starting",
        url: None,
        user_code: None,
        error: None,
        started_at: now_ms(),
    };
    {
        let mut map = state.logins.lock();
        if let Some(s) = map.get(harness)
            && !s.status.finished()
        {
            return Ok(s.status.clone());
        }
        map.insert(
            harness.to_string(),
            Session {
                status: status.clone(),
                stdin: None,
                cancel: cancel_tx,
            },
        );
    }
    let session_id = status.id.clone();
    let mut cmd = Command::new(&program);
    cmd.args(&spec.args)
        .current_dir(state.paths.workspace_dir())
        .env("PWD", state.paths.workspace_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
        // Never pop a browser on the server; the user approves on the phone.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("BROWSER", "true")
        .env("NO_COLOR", "1");
    for k in state.strip_env(entry) {
        cmd.env_remove(k);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            state.logins.update_session(harness, &session_id, |s| {
                s.state = "failed";
                s.error = Some("I couldn't start the sign-in. Try again.".into());
            });
            publish(state, harness).await;
            return Err(e.into());
        }
    };
    let stdin = child
        .stdin
        .take()
        .map(|s| Arc::new(tokio::sync::Mutex::new(s)));
    if let Some(s) = state
        .logins
        .lock()
        .get_mut(harness)
        .filter(|s| s.status.id == session_id)
    {
        s.stdin = stdin;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    for out in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            // Prompts such as "Paste code here >" end without a newline, so
            // split on newlines and on prompt-ending '>' / ':' too.
            let mut r = BufReader::new(out);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let _ = tx.send(strip_ansi(&String::from_utf8_lossy(&buf)));
                    }
                }
            }
        });
    }
    drop(tx);
    crate::info!("login {harness}: started");
    publish(state, harness).await;

    let st = state.clone();
    let name = harness.to_string();
    let pid = child.id();
    tokio::spawn(async move {
        let deadline = tokio::time::sleep(LOGIN_TIMEOUT);
        tokio::pin!(deadline);
        let mut exit_ok = false;
        let mut reason: Option<String> = None;
        let mut outcome = "exited";
        loop {
            tokio::select! {
                line = rx.recv() => {
                    let Some(line) = line else {
                        // Output closed: wait for the exit status.
                        exit_ok = tokio::time::timeout(Duration::from_secs(30), child.wait())
                            .await
                            .ok()
                            .and_then(|r| r.ok())
                            .is_some_and(|s| s.success());
                        break;
                    };
                    on_line(&st, &name, &session_id, spec.flow, &line).await;
                    let l = line.to_lowercase();
                    if l.contains("error") || l.contains("failed") {
                        reason = Some(crate::util::truncate_chars(line.trim(), 160));
                    }
                }
                _ = &mut deadline => { outcome = "timed out"; break; }
                _ = cancel_rx.changed() => { outcome = "cancelled"; break; }
            }
        }
        if outcome != "exited"
            && let Some(pid) = pid.and_then(|p| i32::try_from(p).ok())
        {
            // SAFETY: plain syscall on the process group we created.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
        }
        let _ = child.kill().await;
        finish(&st, &name, &session_id, outcome, exit_ok, reason).await;
    });
    Ok(status)
}

async fn on_line(state: &AppState, harness: &str, id: &str, flow: LoginFlow, line: &str) {
    let url = find_url(line);
    let code = (flow == LoginFlow::DeviceCode)
        .then(|| find_device_code(line))
        .flatten();
    let rejected = rejected_code(line);
    if url.is_none() && code.is_none() && !rejected {
        return;
    }
    let changed = state.logins.update_session(harness, id, |s| {
        if let Some(u) = url
            && s.url.is_none()
        {
            s.url = Some(u);
        }
        if let Some(c) = code {
            s.user_code = Some(c);
        }
        if rejected {
            s.error = Some("That code didn't work. Copy the whole code and try again.".into());
        }
        s.state = match flow {
            LoginFlow::CodePaste if s.url.is_some() => "waiting_for_code",
            LoginFlow::DeviceCode if s.url.is_some() && s.user_code.is_some() => {
                "waiting_for_approval"
            }
            _ => s.state,
        };
    });
    if changed.is_some() {
        publish(state, harness).await;
    }
}

async fn finish(
    state: &AppState,
    harness: &str,
    id: &str,
    outcome: &str,
    exit_ok: bool,
    reason: Option<String>,
) {
    if outcome == "cancelled" {
        state.logins.update_session(harness, id, |s| {
            s.state = "cancelled";
            s.url = None;
            s.user_code = None;
        });
        publish(state, harness).await;
        return;
    }
    state
        .logins
        .update_session(harness, id, |s| s.state = "verifying");
    publish(state, harness).await;
    // Judge success only by the CLI's own status command.
    let auth = state.probe_auth(harness).await;
    let connected = auth == AuthState::Ok;
    crate::info!("login {harness}: {outcome}, exit_ok={exit_ok}, signed_in={connected}");
    state.logins.update_session(harness, id, |s| {
        s.url = None;
        s.user_code = None;
        if connected {
            s.state = "connected";
            s.error = None;
        } else {
            s.state = "failed";
            s.error = Some(match outcome {
                "timed out" => {
                    "The sign-in took too long, so I stopped it. Try again when you're ready."
                        .into()
                }
                _ => s
                    .error
                    .clone()
                    .or(reason)
                    .unwrap_or_else(|| "Sign-in didn't finish. Try again.".into()),
            });
        }
    });
    publish(state, harness).await;
}

/// Pass the code the user typed to the waiting login process. The code is
/// never stored or logged.
pub async fn submit_code(state: &AppState, harness: &str, code: &str) -> Result<LoginStatus> {
    let code = code.trim();
    if code.is_empty() || code.len() > 4096 || code.contains(['\n', '\r']) {
        return Err("invalid code".into());
    }
    let stdin = {
        let map = state.logins.lock();
        let s = map.get(harness).ok_or("no sign-in in progress")?;
        if s.status.state != "waiting_for_code" {
            return Err("this sign-in is not waiting for a code".into());
        }
        s.stdin.clone().ok_or("no sign-in in progress")?
    };
    {
        let mut w = stdin.lock().await;
        w.write_all(code.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
    }
    let status = state
        .logins
        .update(harness, |s| {
            s.state = "verifying";
            s.error = None;
        })
        .ok_or("no sign-in in progress")?;
    publish(state, harness).await;
    Ok(status)
}

pub async fn cancel(state: &AppState, harness: &str) -> bool {
    state
        .logins
        .lock()
        .get(harness)
        .is_some_and(|s| !s.status.finished() && s.cancel.send(true).is_ok())
}

/// Push the login state to the owner's card and the Settings screen.
async fn publish(state: &AppState, harness: &str) {
    let Some(status) = state.logins.status(harness) else {
        return;
    };
    if let Ok(Some(owner)) = state.db.call(db::owner_user_id).await {
        state.hub.to_users(
            std::slice::from_ref(&owner),
            json!({"type": "harness.login", "login": status}),
        );
    }
    update_card(state, harness, move |card| {
        card["state"] = status.state.into();
        card["url"] = status.url.clone().map_or(Value::Null, Value::from);
        card["user_code"] = status.user_code.clone().map_or(Value::Null, Value::from);
        card["error"] = status.error.clone().map_or(Value::Null, Value::from);
        card["login_id"] = status.id.clone().into();
    })
    .await;
}

fn card_key(harness: &str) -> String {
    format!("signin_card.{harness}")
}

pub fn engine_label(harness: &str) -> &'static str {
    match harness {
        "claude" => "Claude",
        "codex" => "Codex (ChatGPT)",
        "gemini" => "Gemini",
        "opencode" => "OpenCode",
        _ => "AI engine",
    }
}

/// Update the stored sign-in card for a harness and broadcast it.
async fn update_card(state: &AppState, harness: &str, f: impl FnOnce(&mut Value) + Send + 'static) {
    let key = card_key(harness);
    let updated = state
        .db
        .call(move |c| {
            let Some(id) = db::get_setting(c, &key)? else {
                return Ok(None);
            };
            let Some(msg) = db::get_message(c, &id)? else {
                return Ok(None);
            };
            let mut card = msg.card.clone().unwrap_or(Value::Null);
            f(&mut card);
            db::update_message_card(c, &id, &card).map(Some)
        })
        .await
        .ok()
        .flatten();
    if let Some(m) = updated {
        broadcast_updated(state, &m).await;
    }
}

pub(crate) async fn broadcast_updated(state: &AppState, m: &db::Message) {
    let conv = m.conversation_id.clone();
    let members = state
        .db
        .call(move |c| db::conversation_member_ids(c, &conv))
        .await
        .unwrap_or_default();
    state.hub.to_users(
        &members,
        json!({"type": "message.updated", "conversation_id": m.conversation_id, "message": m}),
    );
}

/// A harness the owner relies on went from signed in to signed out: post
/// (or refresh) its reconnect card in the Main chat and push a
/// notification, saying honestly what Iris uses meanwhile. Once per outage.
pub async fn on_signed_out(state: &AppState, harness: &str) {
    post_signin_card(state, harness, false).await;
}

/// Post the sign-in card for a harness: "Reconnect X" after an outage, or
/// "Sign in to X" right after installing it.
pub async fn post_signin_card(state: &AppState, harness: &str, first_time: bool) {
    if !state.is_serving() {
        return;
    }
    let Some(entry) = state.harness(harness) else {
        return;
    };
    let flow = entry.harness.login().map(|s| s.flow);
    let hint = entry.harness.manual_login_hint();
    let label = engine_label(harness);
    let (fallback, _) = state.resolve_slot(crate::ranking::Slot::Chat, false).await;
    let catalog = state.catalog();
    let meanwhile = match fallback.effective.as_deref().and_then(|id| catalog.get(id)) {
        Some(m) => format!(
            "Until then I'm using {} instead, so replies may feel a bit different.",
            m.label
        ),
        None => "Until then I can't reply, because no other AI engine is available.".into(),
    };
    // Engines without a headless sign-in take an API key from the app.
    let key_options = crate::keys::options(harness);
    let api_key = flow.is_none() && !key_options.is_empty();
    let (title, body) = if first_time && api_key {
        (
            format!("Set up {label}"),
            if harness == "opencode" {
                "OpenCode is installed. Add a key for one of its providers to start using it \
                 (or point it at a local model on the server)."
                    .to_string()
            } else {
                format!("{label} is installed. Add an API key to start using it.")
            },
        )
    } else if first_time {
        (
            format!("Sign in to {label}"),
            format!("{label} is installed. Sign in with your account to start using it."),
        )
    } else {
        (
            format!("Reconnect {label}"),
            format!("{label} on your Iris server is signed out. {meanwhile}"),
        )
    };
    let card = json!({
        "type": "engine_signin",
        "title": title,
        "body": body,
        "harness": harness,
        "flow": match flow {
            Some(LoginFlow::CodePaste) => "code_paste",
            Some(LoginFlow::DeviceCode) => "device_code",
            None if api_key => "api_key",
            None => "manual",
        },
        "key_options": if api_key { serde_json::to_value(key_options).unwrap_or(Value::Null) } else { Value::Null },
        "instructions": if flow.is_none() { Value::from(hint) } else { Value::Null },
        "state": "needs_reconnect",
        "action_label": match (flow.is_some(), first_time) {
            _ if api_key => "Save key",
            (true, true) => "Sign in",
            (true, false) => "Reconnect",
            (false, _) => "Check again",
        },
        "owner_only": true,
    });
    let key = card_key(harness);
    let b = body.clone();
    let t = title.clone();
    let result = state
        .db
        .call(move |c| {
            let Some(owner) = db::owner_user_id(c)? else {
                return Ok(None);
            };
            // Reuse an open card from this outage instead of stacking more.
            if let Some(id) = db::get_setting(c, &key)?
                && let Some(m) = db::get_message(c, &id)?
                && m.card.as_ref().is_some_and(|k| k["state"] != "connected")
            {
                let mut k = m.card.clone().unwrap_or(Value::Null);
                k["body"] = b.into();
                k["title"] = t.into();
                k["state"] = "needs_reconnect".into();
                for f in ["flow", "key_options", "instructions", "action_label"] {
                    k[f] = card[f].clone();
                }
                return db::update_message_card(c, &id, &k).map(|m| Some((owner, m, false)));
            }
            let main = db::ensure_main_conversation(c, &owner)?;
            let m = crate::entities::insert_card(c, &main, &card, "assistant")?;
            db::set_setting(c, &key, &m.id)?;
            Ok(Some((owner, m, true)))
        })
        .await
        .ok()
        .flatten();
    let Some((owner, msg, is_new)) = result else {
        return;
    };
    if is_new {
        state.hub.to_users(
            std::slice::from_ref(&owner),
            json!({"type": "message.created", "conversation_id": msg.conversation_id, "message": msg}),
        );
        crate::notify::push_users(
            state,
            std::slice::from_ref(&owner),
            json!({
                "title": title,
                "body": body,
                "conversation_id": msg.conversation_id,
                "message_id": msg.id,
                "kind": "engine_signin",
            }),
        );
    } else {
        broadcast_updated(state, &msg).await;
    }
}

/// A harness is signed in again: close its card.
pub async fn on_signed_in(state: &AppState, harness: &str) {
    let label = engine_label(harness);
    update_card(state, harness, move |card| {
        if card["state"] != "connected" {
            card["state"] = "connected".into();
            card["body"] = format!("{label} is connected again.").into();
            card["url"] = Value::Null;
            card["user_code"] = Value::Null;
            card["error"] = Value::Null;
        }
    })
    .await;
}

/// A connector's tool call was refused with what looks like an expired or
/// revoked sign-in: post a reconnect card in the Main chat (and push),
/// once until the owner updates the connector.
pub async fn on_connector_auth_error(state: &AppState, server: &str) {
    let name = server.to_string();
    let result = state
        .db
        .call(move |c| {
            let Some(owner) = db::owner_user_id(c)? else {
                return Ok(None);
            };
            let Some(con) = crate::entities::list_connectors(c, false)?
                .into_iter()
                .find(|x| x.name == name)
            else {
                return Ok(None);
            };
            let key = format!("connector_card.{}", con.id);
            if let Some(id) = db::get_setting(c, &key)?
                && let Some(m) = db::get_message(c, &id)?
                && m.card.as_ref().is_some_and(|k| k["state"] != "connected")
            {
                return Ok(None);
            }
            let label = crate::gateway::service_label(&con);
            let oauth = con.config.oauth.is_some();
            let card = json!({
                "type": "connector_reconnect",
                "title": format!("Reconnect {label}"),
                "body": if oauth {
                    format!(
                        "{label}'s sign-in expired or was revoked, so I can't use it right now. \
                         Sign in again and I'll pick up where I left off."
                    )
                } else {
                    format!(
                        "{label} refused my request, so its sign-in has probably expired or been \
                         revoked. Update its credentials and I'll keep using it."
                    )
                },
                "connector_id": con.id,
                "service": con.service,
                "auth": if oauth { "oauth" } else { "secrets" },
                "state": "needs_reconnect",
                "action_label": if oauth { "Sign in again" } else { "Update" },
                "owner_only": true,
            });
            let main = db::ensure_main_conversation(c, &owner)?;
            let m = crate::entities::insert_card(c, &main, &card, "assistant")?;
            db::set_setting(c, &key, &m.id)?;
            Ok(Some((owner, m)))
        })
        .await
        .ok()
        .flatten();
    let Some((owner, msg)) = result else {
        return;
    };
    crate::info!("connector {server}: sign-in rejected; reconnect card posted");
    state.hub.to_users(
        std::slice::from_ref(&owner),
        json!({"type": "message.created", "conversation_id": msg.conversation_id, "message": msg}),
    );
    crate::notify::push_users(
        state,
        std::slice::from_ref(&owner),
        json!({
            "title": msg.content,
            "body": msg.card.as_ref().and_then(|c| c["body"].as_str()).unwrap_or(""),
            "conversation_id": msg.conversation_id,
            "message_id": msg.id,
            "kind": "connector_reconnect",
        }),
    );
}

/// The owner updated a connector's settings: close its reconnect card.
pub async fn on_connector_updated(state: &AppState, connector_id: &str) {
    let key = format!("connector_card.{connector_id}");
    let updated = state
        .db
        .call(move |c| {
            let Some(id) = db::get_setting(c, &key)? else {
                return Ok(None);
            };
            let Some(m) = db::get_message(c, &id)? else {
                return Ok(None);
            };
            let mut card = m.card.clone().unwrap_or(Value::Null);
            if card["state"] == "connected" {
                return Ok(None);
            }
            card["state"] = "connected".into();
            card["body"] = "Connected again. I'll use it from now on.".into();
            db::update_message_card(c, &id, &card).map(Some)
        })
        .await
        .ok()
        .flatten();
    if let Some(m) = updated {
        broadcast_updated(state, &m).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_and_finds_codex_device_code() {
        // Synthetic sample modeled on `codex login --device-auth` output.
        let raw = "\u{1b}[90mOpenAI's command-line coding agent\u{1b}[0m\r\n\
            1. Open this link in your browser and sign in to your account\n\
            \u{20}  \u{1b}[94mhttps://auth.example.com/codex/device\u{1b}[0m\n\
            2. Enter this one-time code \u{1b}[90m(expires in 15 minutes)\u{1b}[0m\n\
            \u{20}  \u{1b}[94mWXYZ-12AB3\u{1b}[0m\n";
        let lines: Vec<String> = raw.lines().map(strip_ansi).collect();
        assert!(!lines.iter().any(|l| l.contains('\u{1b}')));
        assert_eq!(
            lines.iter().find_map(|l| find_url(l)).as_deref(),
            Some("https://auth.example.com/codex/device")
        );
        assert_eq!(
            lines.iter().find_map(|l| find_device_code(l)).as_deref(),
            Some("WXYZ-12AB3")
        );
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}ok"), "ok");
    }

    #[test]
    fn finds_claude_authorize_url() {
        // Synthetic sample modeled on `claude auth login` output.
        let line = "If the browser didn't open, visit: https://login.example.com/oauth/authorize?code=true&client_id=abc&state=xyz";
        assert_eq!(
            find_url(line).as_deref(),
            Some("https://login.example.com/oauth/authorize?code=true&client_id=abc&state=xyz")
        );
        assert_eq!(find_url("no link here"), None);
        assert_eq!(find_device_code("Paste code here if prompted >"), None);
        assert_eq!(find_device_code("2026-10-11"), None);
        assert!(rejected_code(
            "Invalid code. Please make sure the full code was copied."
        ));
    }
}
