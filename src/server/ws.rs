//! WebSocket event stream (`GET /api/ws`).
//!
//! Authenticate with an `Authorization: Bearer` header, or (for browsers,
//! which cannot set headers on WebSocket requests) by sending
//! `{"type":"auth","token":"..."}` as the first frame within 10 seconds.
//! The server then sends `{"type":"ready",...}` followed by events.
//!
//! The owner's app can also watch Iris's browser on the same socket:
//! `{"type":"browser.watch","mode":"peek"|"full"|"off"}` starts or stops
//! `browser.frame` events, and while the owner has taken over,
//! `{"type":"browser.input",...}` sends taps, scrolls, keys, and text.

use super::{AuthUser, auth_from_token};
use crate::auth;
use crate::state::AppState;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

/// A socket watching the browser.
struct Watching {
    _guard: crate::browser::WatchGuard,
    rx: tokio::sync::watch::Receiver<Option<Arc<crate::browser::Frame>>>,
    mode: crate::browser::Mode,
    last_sent: Option<Instant>,
    /// A newer frame is waiting for its turn (throttling never drops the
    /// latest frame, so a still page always ends up on screen).
    pending: bool,
}

impl Watching {
    fn due(&self) -> Instant {
        self.last_sent
            .map(|t| t + self.mode.min_interval())
            .unwrap_or_else(Instant::now)
    }
}

pub async fn handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(auth::bearer)
        .map(String::from);
    let pre = match token {
        Some(t) => auth_from_token(&state, &t).await,
        None => None,
    };
    ws.max_message_size(64 * 1024)
        .on_upgrade(move |socket| run(state, socket, pre))
}

/// How often the server pings, and how long a client may stay silent
/// (no pong or other frame) before its socket is dropped. A phone that
/// vanished without closing must not keep counting as online, since that
/// suppresses reply notifications.
const PING_EVERY: Duration = Duration::from_secs(30);
const SILENCE_LIMIT: Duration = Duration::from_secs(75);
/// A send that cannot complete (the peer stopped reading) ends the socket.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

async fn send(socket: &mut WebSocket, msg: Message) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, socket.send(msg)).await,
        Ok(Ok(()))
    )
}

async fn send_json(socket: &mut WebSocket, v: &Value) -> bool {
    send(socket, Message::Text(v.to_string().into())).await
}

/// The client has been silent too long (it answers pings automatically).
fn gone_quiet(last_heard: tokio::time::Instant, now: tokio::time::Instant) -> bool {
    now.duration_since(last_heard) > SILENCE_LIMIT
}

async fn first_frame_auth(state: &AppState, socket: &mut WebSocket) -> Option<AuthUser> {
    let msg = tokio::time::timeout(Duration::from_secs(10), socket.recv())
        .await
        .ok()??
        .ok()?;
    let Message::Text(text) = msg else {
        return None;
    };
    let v: Value = serde_json::from_str(&text).ok()?;
    if v["type"] != "auth" {
        return None;
    }
    auth_from_token(state, v["token"].as_str()?).await
}

async fn device_active(state: &AppState, device_id: &str) -> bool {
    let id = device_id.to_string();
    state
        .db
        .call(move |c| {
            c.query_row(
                "SELECT revoked_at IS NULL FROM devices WHERE id = ?1",
                [&id],
                |r| r.get::<_, bool>(0),
            )
        })
        .await
        .unwrap_or(false)
}

async fn run(state: AppState, mut socket: WebSocket, pre: Option<AuthUser>) {
    let user = match pre {
        Some(u) => Some(u),
        None => first_frame_auth(&state, &mut socket).await,
    };
    let Some(user) = user else {
        let _ = send_json(
            &mut socket,
            &json!({"type": "error", "code": "unauthorized", "message": "authenticate first"}),
        )
        .await;
        let _ = send(&mut socket, Message::Close(None)).await;
        return;
    };
    let uid = user.id().to_string();
    // Subscribe before sending `ready` so no event can slip in between.
    let mut rx = state.hub.subscribe();
    let _online = state.hub.connect(&uid);
    let ready = json!({
        "type": "ready",
        "user_id": uid,
        "device_id": user.device.id,
        "settings": state.settings_json(user.user.is_owner).await,
        "harnesses": super::admin::harnesses_for(&state, &user),
    });
    if !send_json(&mut socket, &ready).await {
        return;
    }
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut ticks: u32 = 0;
    let is_owner = user.user.is_owner;
    let mut watching: Option<Watching> = None;
    // Owner input runs in order on its own task, so a slow page never
    // stalls this socket's live view or events.
    let (input_err_tx, mut input_err) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut input_tx: Option<tokio::sync::mpsc::Sender<Value>> = None;
    // Aborted with the socket: queued input never outlives it.
    let mut input_worker: Option<tokio::task::JoinHandle<()>> = None;
    let mut revoked = false;
    let mut last_heard = tokio::time::Instant::now();
    loop {
        let due = watching.as_ref().filter(|w| w.pending).map(Watching::due);
        tokio::select! {
            incoming = socket.recv() => {
                last_heard = tokio::time::Instant::now();
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        let reply = match v["type"].as_str().unwrap_or("") {
                            "ping" => Some(json!({"type": "pong"})),
                            "browser.watch" | "browser.input" if !is_owner => Some(json!({
                                "type": "error", "code": "forbidden",
                                "message": "only the owner can use Iris's computer"})),
                            "browser.watch" => {
                                watching = crate::browser::Mode::parse(v["mode"].as_str().unwrap_or(""))
                                    .map(|mode| {
                                        // Replaces (and so ends) any earlier watch.
                                        let (guard, rx) = state.browser.watch(mode);
                                        let mut rx = rx;
                                        rx.mark_changed();
                                        Watching { _guard: guard, rx, mode, last_sent: None, pending: false }
                                    });
                                None
                            }
                            "browser.input" => {
                                let tx = input_tx.get_or_insert_with(|| {
                                    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(64);
                                    let (st, dev, errs) = (state.clone(), user.device.id.clone(), input_err_tx.clone());
                                    input_worker = Some(tokio::spawn(async move {
                                        while let Some(ev) = rx.recv().await {
                                            if let Err(e) = st.browser.user_input(&dev, &ev).await {
                                                let _ = errs.send(e.to_string());
                                            }
                                        }
                                    }));
                                    tx
                                });
                                match tx.try_send(v) {
                                    Ok(()) => None,
                                    Err(_) => Some(json!({"type": "browser.input_error",
                                        "message": "the browser is busy; try again in a moment"})),
                                }
                            }
                            _ => None,
                        };
                        if let Some(r) = reply && !send_json(&mut socket, &r).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            changed = async {
                match watching.as_mut() {
                    Some(w) if !w.pending => w.rx.changed().await,
                    _ => std::future::pending().await,
                }
            } => {
                match (changed, watching.as_mut()) {
                    (Ok(()), Some(w)) => w.pending = true,
                    _ => watching = None,
                }
            }
            Some(msg) = input_err.recv() => {
                if !send_json(&mut socket, &json!({"type": "browser.input_error", "message": msg})).await {
                    break;
                }
            }
            _ = async {
                match due {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                // Peek watchers get about one frame a second (full screen
                // more), always ending on the newest frame.
                let Some(w) = watching.as_mut() else { continue };
                w.pending = false;
                w.last_sent = Some(Instant::now());
                let frame = w.rx.borrow_and_update().clone();
                if let Some(frame) = frame
                    && !send(&mut socket, Message::Text(frame.json.clone().into())).await
                {
                    break;
                }
            }
            ev = rx.recv() => {
                match ev {
                    Ok(env) => {
                        if env.audience.reaches(&uid, is_owner)
                            && !send(&mut socket, Message::Text(env.json.clone().into())).await
                        {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        // Too slow to keep up; tell the client to refetch.
                        if !send_json(&mut socket, &json!({"type": "resync"})).await {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            _ = ping.tick() => {
                ticks += 1;
                if gone_quiet(last_heard, tokio::time::Instant::now())
                    || !send(&mut socket, Message::Ping(Vec::new().into())).await
                {
                    break;
                }
                // Drop sockets of revoked devices within about a minute.
                if ticks.is_multiple_of(2) && !device_active(&state, &user.device.id).await {
                    let _ = send_json(&mut socket, &json!({"type": "error", "code": "unauthorized", "message": "device revoked"})).await;
                    revoked = true;
                    break;
                }
            }
        }
    }
    if let Some(w) = input_worker {
        w.abort();
    }
    // A revoked device loses control of Iris's browser at once.
    if revoked
        && matches!(state.browser.control(), crate::browser::Control::User { device_id, .. } if device_id == user.device.id)
    {
        state.browser.hand_back();
    }
    let _ = send(&mut socket, Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_clients_are_dropped_after_missed_pings() {
        let start = tokio::time::Instant::now();
        assert!(!gone_quiet(start, start + PING_EVERY * 2));
        assert!(gone_quiet(start, start + PING_EVERY * 3));
        // Pings go out more often than the limit, so a live client (whose
        // browser answers every ping) is never dropped.
        assert!(PING_EVERY * 2 < SILENCE_LIMIT);
    }
}
