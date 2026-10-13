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
//! `{"type":"browser.input",...}` sends taps, scrolls, keys, text, and
//! fills of saved logins and cards.

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

pub(super) async fn event_visible(state: &AppState, uid: &str, raw: &str) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    if event["conversation_id"].is_string() {
        let mid = event["message_id"]
            .as_str()
            .or_else(|| event["message"]["id"].as_str());
        if let Some(mid) = mid {
            let (uid, mid) = (uid.to_string(), mid.to_string());
            return state
                .db
                .call(move |c| crate::db::message_visible(c, &uid, &mid))
                .await
                .unwrap_or(false);
        }
    }
    true
}

async fn browser_input(state: &AppState, device_id: &str, ev: &Value) -> crate::util::Result<()> {
    if !device_active(state, device_id).await {
        return Err("device revoked".into());
    }
    if ev["kind"] == "fill" {
        return crate::browser::fill::fill(
            state,
            device_id,
            ev["credential_id"].as_str().unwrap_or(""),
            ev["field"].as_str().unwrap_or(""),
        )
        .await;
    }
    state.browser.user_input(device_id, ev).await
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
    let mut device_revoked = state.hub.watch_device(&user.device.id);
    if *device_revoked.borrow() || !device_active(&state, &user.device.id).await {
        return;
    }
    let _online = state.hub.connect(&uid);
    let mut input_worker: Option<tokio::task::JoinHandle<()>> = None;
    // Cancellation wraps the whole connection, including slow sends and
    // database reads, so revocation never waits for the ping or send timeout.
    let revoked = tokio::select! {
        biased;
        _ = device_revoked.changed() => true,
        revoked = pump(&state, &mut socket, &user, &mut input_worker) => revoked,
    };
    if let Some(w) = input_worker {
        w.abort();
    }
    if revoked {
        state.browser.revoke_device(&user.device.id);
    }
    let _ = send(&mut socket, Message::Close(None)).await;
}

/// Drive an authenticated socket. The caller can cancel this entire future
/// on revocation, even while a send or a database read is awaiting completion.
async fn pump(
    state: &AppState,
    socket: &mut WebSocket,
    user: &AuthUser,
    input_worker: &mut Option<tokio::task::JoinHandle<()>>,
) -> bool {
    let uid = user.id().to_string();
    let mut rx = state.hub.subscribe();
    let ready = json!({
        "type": "ready",
        "user_id": uid,
        "device_id": user.device.id,
        "settings": state.settings_json(user.user.is_owner).await,
        "harnesses": super::admin::harnesses_for(state, user),
    });
    if !send_json(socket, &ready).await {
        return false;
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
                                    let (st, dev, errs) = ((*state).clone(), user.device.id.clone(), input_err_tx.clone());
                                    *input_worker = Some(tokio::spawn(async move {
                                        while let Some(ev) = rx.recv().await {
                                            if let Err(e) = browser_input(&st, &dev, &ev).await {
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
                        if let Some(r) = reply && !send_json(socket, &r).await {
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
                if !send_json(socket, &json!({"type": "browser.input_error", "message": msg})).await {
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
                    && !send(socket, Message::Text(frame.json.clone().into())).await
                {
                    break;
                }
            }
            ev = rx.recv() => {
                match ev {
                    Ok(env) => {
                        if env.audience.reaches(&uid, is_owner)
                            && event_visible(state, &uid, &env.json).await
                            && !send(socket, Message::Text(env.json.clone().into())).await
                        {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        // Too slow to keep up; tell the client to refetch.
                        if !send_json(socket, &json!({"type": "resync"})).await {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            _ = ping.tick() => {
                ticks += 1;
                if gone_quiet(last_heard, tokio::time::Instant::now())
                    || !send(socket, Message::Ping(Vec::new().into())).await
                {
                    break;
                }
                // Also catch revocation performed directly in the database.
                if ticks.is_multiple_of(2) && !device_active(state, &user.device.id).await {
                    let _ = send_json(socket, &json!({"type": "error", "code": "unauthorized", "message": "device revoked"})).await;
                    revoked = true;
                    break;
                }
            }
        }
    }
    revoked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn revocation_closes_a_live_websocket_without_waiting_for_a_ping() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let state = AppState::new(
            crate::config::Config::default(),
            paths,
            crate::db::Db::open_in_memory().unwrap(),
        )
        .unwrap();
        let token = auth::generate_token();
        let device = state
            .db
            .with(|c| {
                let owner = crate::db::owner_user_id(c)?.unwrap();
                crate::db::insert_device(c, &owner, "test", &auth::hash_token(&token))
            })
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = super::super::router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(format!("GET /api/ws HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAuthorization: Bearer {token}\r\n\r\n").as_bytes()).await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(stream.read_u8().await.unwrap());
        }
        assert!(
            String::from_utf8(headers)
                .unwrap()
                .starts_with("HTTP/1.1 101")
        );
        async fn frame(stream: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
            let opcode = stream.read_u8().await.unwrap() & 15;
            let mut len = (stream.read_u8().await.unwrap() & 127) as usize;
            if len == 126 {
                len = stream.read_u16().await.unwrap() as usize;
            } else if len == 127 {
                len = stream.read_u64().await.unwrap() as usize;
            }
            assert!(len <= 64 * 1024);
            let mut body = vec![0; len];
            stream.read_exact(&mut body).await.unwrap();
            (opcode, body)
        }
        let (opcode, ready) = frame(&mut stream).await;
        assert_eq!(opcode, 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&ready).unwrap()["type"],
            "ready"
        );
        let user = auth_from_token(&state, &token).await.unwrap();
        let _ =
            super::super::revoke_device(State(state.clone()), user, axum::extract::Path(device.id))
                .await
                .unwrap();
        let (opcode, _) = tokio::time::timeout(Duration::from_secs(1), frame(&mut stream))
            .await
            .unwrap();
        assert_eq!(opcode, 8, "revoked sockets close immediately");
        server.abort();
    }

    #[tokio::test]
    async fn every_browser_input_revalidates_its_device() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let state = AppState::new(
            crate::config::Config::default(),
            paths,
            crate::db::Db::open_in_memory().unwrap(),
        )
        .unwrap();
        let device = state
            .db
            .with(|c| {
                let owner = crate::db::owner_user_id(c)?.unwrap();
                crate::db::insert_device(c, &owner, "test", "synthetic-hash")
            })
            .unwrap();
        assert!(device_active(&state, &device.id).await);
        state
            .db
            .with(|c| crate::db::revoke_device(c, &device.user_id, &device.id))
            .unwrap();
        for kind in ["navigate", "tap", "text", "key", "scroll", "fill"] {
            assert_eq!(
                browser_input(&state, &device.id, &json!({"kind":kind}))
                    .await
                    .unwrap_err()
                    .to_string(),
                "device revoked"
            );
        }
    }

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
