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
        "settings": state.settings_json_for(&user.user).await,
        "harnesses": super::admin::harnesses_for(&state, &user),
    });
    if !send_json(&mut socket, &ready).await {
        return;
    }
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.tick().await;
    let mut ticks: u32 = 0;
    loop {
        tokio::select! {
            incoming = socket.recv() => {
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
