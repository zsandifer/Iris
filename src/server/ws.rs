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
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;

/// A socket watching the browser.
struct Watching {
    _guard: crate::browser::WatchGuard,
    rx: tokio::sync::watch::Receiver<Option<Arc<crate::browser::Frame>>>,
    mode: crate::browser::Mode,
    last_sent: Option<Instant>,
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

async fn send_json(socket: &mut WebSocket, v: &Value) -> bool {
    socket
        .send(Message::Text(v.to_string().into()))
        .await
        .is_ok()
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
        let _ = socket.send(Message::Close(None)).await;
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
        "settings": state.settings_json().await,
        "harnesses": super::admin::harnesses_for(&state, &user),
    });
    if !send_json(&mut socket, &ready).await {
        return;
    }
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.tick().await;
    let mut ticks: u32 = 0;
    let is_owner = user.user.is_owner;
    let mut watching: Option<Watching> = None;
    loop {
        tokio::select! {
            incoming = socket.recv() => {
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
                                        Watching { _guard: guard, rx, mode, last_sent: None }
                                    });
                                None
                            }
                            "browser.input" => match state.browser.user_input(&user.device.id, &v).await {
                                Ok(()) => None,
                                Err(e) => Some(json!({"type": "browser.input_error", "message": e.to_string()})),
                            },
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
                    Some(w) => w.rx.changed().await,
                    None => std::future::pending().await,
                }
            } => {
                if changed.is_err() {
                    watching = None;
                    continue;
                }
                let Some(w) = watching.as_mut() else { continue };
                let frame = w.rx.borrow_and_update().clone();
                let Some(frame) = frame else { continue };
                // Peek watchers get about one frame a second even when
                // someone else watches full screen.
                if w.last_sent.is_some_and(|t| t.elapsed() < w.mode.min_interval()) {
                    continue;
                }
                w.last_sent = Some(Instant::now());
                if socket.send(Message::Text(frame.json.clone().into())).await.is_err() {
                    break;
                }
            }
            ev = rx.recv() => {
                match ev {
                    Ok(env) => {
                        if env.audience.reaches(&uid, is_owner)
                            && socket.send(Message::Text(env.json.clone().into())).await.is_err()
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
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
                // Drop sockets of revoked devices within about a minute.
                if ticks % 2 == 0 && !device_active(&state, &user.device.id).await {
                    let _ = send_json(&mut socket, &json!({"type": "error", "code": "unauthorized", "message": "device revoked"})).await;
                    break;
                }
            }
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}
