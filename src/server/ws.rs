//! WebSocket event stream (`GET /api/ws`).
//!
//! Authenticate with an `Authorization: Bearer` header, or (for browsers,
//! which cannot set headers on WebSocket requests) by sending
//! `{"type":"auth","token":"..."}` as the first frame within 10 seconds.
//! The server then sends `{"type":"ready",...}` followed by events.

use super::{AuthUser, auth_from_token};
use crate::auth;
use crate::state::AppState;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

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
        "settings": state.settings_json().await,
        "harnesses": super::admin::harnesses_for(&state, &user),
    });
    if !send_json(&mut socket, &ready).await {
        return;
    }
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut ticks: u32 = 0;
    let mut last_heard = tokio::time::Instant::now();
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                last_heard = tokio::time::Instant::now();
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        if v["type"] == "ping" && !send_json(&mut socket, &json!({"type": "pong"})).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            ev = rx.recv() => {
                match ev {
                    Ok(env) => {
                        if env.audience.includes(&uid)
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
                    break;
                }
            }
        }
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
