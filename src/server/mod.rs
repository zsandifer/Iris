//! HTTP API and WebSocket server (axum). Every route except the embedded
//! app (`/`), the test console (`/console`), `/api/health`, and `/api/pair`
//! requires a device token.

pub(crate) mod admin;
mod chat;
mod connect;
mod content;
mod web;
mod ws;

pub use chat::sniff_image;

use crate::auth::{self, PairError};
use crate::db::{self, Device, User};
use crate::state::AppState;
use crate::util::Result;
use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;

// ------------------------------------------------------------------ errors

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{what} not found"),
        )
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid device token",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            axum::Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

impl From<crate::util::BoxError> for ApiError {
    fn from(e: crate::util::BoxError) -> Self {
        crate::warn!("internal error: {e}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal server error",
        )
    }
}

impl From<String> for ApiError {
    fn from(message: String) -> Self {
        Self::bad_request(message)
    }
}

pub type ApiResult<T = axum::Json<Value>> = std::result::Result<T, ApiError>;

pub fn ok(v: Value) -> ApiResult {
    Ok(axum::Json(v))
}

/// JSON body extractor with consistent error responses.
pub struct JsonBody<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> std::result::Result<Self, ApiError> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Self(v)),
            Err(e) => Err(ApiError::bad_request(e.body_text())),
        }
    }
}

// -------------------------------------------------------------------- auth

/// The authenticated device and its user.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub device: Device,
    pub user: User,
}

impl AuthUser {
    pub fn id(&self) -> &str {
        &self.user.id
    }
    pub fn require_owner(&self) -> std::result::Result<(), ApiError> {
        if self.user.is_owner {
            Ok(())
        } else {
            Err(ApiError::forbidden("only the owner can do this"))
        }
    }
}

pub async fn auth_from_token(state: &AppState, token: &str) -> Option<AuthUser> {
    let device = auth::authenticate(&state.db, token).await.ok()??;
    let uid = device.user_id.clone();
    let user = state.db.call(move |c| db::get_user(c, &uid)).await.ok()??;
    Some(AuthUser { device, user })
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, ApiError> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(auth::bearer)
            .ok_or_else(ApiError::unauthorized)?;
        auth_from_token(state, token)
            .await
            .ok_or_else(ApiError::unauthorized)
    }
}

// ------------------------------------------------------------------ router

pub fn router(state: AppState) -> Router {
    let upload_limit = state.cfg.max_upload_bytes + 64 * 1024;
    let api = Router::new()
        .route("/health", get(health))
        .route("/pair", post(pair))
        .route("/me", get(me).put(update_me))
        .route("/me/quiet-hours", axum::routing::put(put_quiet_hours))
        .route("/devices", get(list_devices))
        .route("/devices/{id}", delete(revoke_device))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}/pairing-code", post(user_pairing_code))
        .merge(chat::routes())
        .merge(content::routes())
        .merge(admin::routes())
        .merge(connect::routes())
        .route("/ws", get(ws::handler))
        .layer(DefaultBodyLimit::max(upload_limit));
    Router::new()
        .route("/console", get(console))
        .nest("/api", api)
        .fallback(get(web::serve).fallback(|| async { ApiError::not_found("route") }))
        .with_state(state)
}

const CONSOLE_HTML: &str = include_str!("console.html");

async fn console() -> Response {
    let mut res = Html(CONSOLE_HTML).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
             connect-src 'self'; img-src 'self' blob: data:",
        ),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    res
}

async fn health() -> ApiResult {
    ok(json!({"ok": true, "name": "iris", "version": env!("CARGO_PKG_VERSION")}))
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
    #[serde(default)]
    device_name: String,
}

async fn pair(State(state): State<AppState>, JsonBody(body): JsonBody<PairBody>) -> ApiResult {
    match auth::pair_device(
        &state.db,
        &state.pair_limiter,
        &body.code,
        &body.device_name,
    )
    .await
    {
        Ok((token, device)) => {
            crate::info!("paired new device {}", device.id);
            ok(json!({"token": token, "device": device}))
        }
        Err(PairError::Invalid) => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_code",
            "pairing code is invalid or expired",
        )),
        Err(PairError::Locked) => Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "locked",
            "too many failed attempts; create a new code with `iris pair` later",
        )),
        Err(PairError::Internal(e)) => Err(e.into()),
    }
}

async fn me(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    let uid = user.id().to_string();
    let main = state
        .db
        .call(move |c| db::ensure_main_conversation(c, &uid))
        .await?;
    let quiet = crate::notify::quiet_hours(&state, user.id()).await;
    ok(json!({
        "user": user.user,
        "device": user.device,
        "main_conversation_id": main,
        "quiet_hours": quiet,
    }))
}

#[derive(Deserialize)]
struct QuietHours {
    start: String,
    end: String,
}

#[derive(Deserialize)]
struct QuietHoursBody {
    quiet_hours: Option<QuietHours>,
}

/// Set (or clear with `null`) the caller's quiet hours for push notifications.
async fn put_quiet_hours(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<QuietHoursBody>,
) -> ApiResult {
    let key = crate::notify::quiet_hours_key(user.id());
    let value = match &body.quiet_hours {
        Some(q) => {
            let (Some(_), Some(_)) = (
                crate::config::parse_hhmm(&q.start),
                crate::config::parse_hhmm(&q.end),
            ) else {
                return Err(ApiError::bad_request("start and end must be HH:MM"));
            };
            Some(json!({"start": q.start.trim(), "end": q.end.trim()}))
        }
        None => None,
    };
    let stored = value.clone();
    state
        .db
        .call(move |c| match &stored {
            Some(v) => db::set_setting(c, &key, &v.to_string()),
            None => db::delete_setting(c, &key),
        })
        .await?;
    ok(json!({"quiet_hours": value}))
}

#[derive(Deserialize)]
struct UpdateMe {
    display_name: String,
}

async fn update_me(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<UpdateMe>,
) -> ApiResult {
    crate::entities::check_text("display_name", &body.display_name, 80, true)?;
    let (uid, name) = (user.id().to_string(), body.display_name.trim().to_string());
    state
        .db
        .call(move |c| db::rename_user(c, &uid, &name))
        .await?;
    ok(json!({"ok": true}))
}

async fn list_devices(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    let uid = user.id().to_string();
    let devices = state.db.call(move |c| db::list_devices(c, &uid)).await?;
    ok(json!({"devices": devices, "current_device_id": user.device.id}))
}

async fn revoke_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let uid = user.id().to_string();
    if !state
        .db
        .call(move |c| db::revoke_device(c, &uid, &id))
        .await?
    {
        return Err(ApiError::not_found("device"));
    }
    ok(json!({"ok": true}))
}

async fn list_users(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    let users = state
        .db
        .call(|c| {
            let mut stmt = c.prepare("SELECT id FROM users ORDER BY created_at")?;
            let ids: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            ids.iter()
                .filter_map(|id| db::get_user(c, id).transpose())
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    ok(json!({"users": users}))
}

#[derive(Deserialize)]
struct CreateUser {
    display_name: String,
}

/// Create another person (for group conversations). They pair their own
/// device with a code from `POST /api/users/{id}/pairing-code`.
async fn create_user(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<CreateUser>,
) -> ApiResult {
    user.require_owner()?;
    crate::entities::check_text("display_name", &body.display_name, 80, true)?;
    let name = body.display_name.trim().to_string();
    let created = state
        .db
        .call(move |c| {
            let id = crate::util::new_id();
            c.execute(
                "INSERT INTO users (id, display_name, is_owner, created_at) VALUES (?1, ?2, 0, ?3)",
                rusqlite::params![id, name, crate::util::now_ms()],
            )?;
            db::ensure_main_conversation(c, &id)?;
            db::get_user(c, &id)
        })
        .await?;
    ok(json!({"user": created}))
}

async fn user_pairing_code(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    let res = state
        .db
        .call(move |c| {
            if db::get_user(c, &id)?.is_none() {
                return Ok(None);
            }
            Ok(Some(auth::create_pairing_code(c, &id)))
        })
        .await?;
    match res {
        None => Err(ApiError::not_found("user")),
        Some(Err(e)) => Err(e.into()),
        Some(Ok((code, expires_at))) => ok(json!({"code": code, "expires_at": expires_at})),
    }
}

// -------------------------------------------------------------- connectors

/// Reserved MCP server names (the built-in bridge).
pub const RESERVED_CONNECTOR_NAMES: &[&str] = &["iris"];

pub(crate) fn connector_aad(id: &str, field: &str, key: &str) -> String {
    format!("connector:{id}:{field}:{key}")
}

/// Seal plaintext env/header values for storage.
pub fn seal_connector_secrets(
    state: &AppState,
    id: &str,
    field: &str,
    values: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|(k, v)| {
            Ok((
                k.clone(),
                state.vault.seal_str(v, &connector_aad(id, field, k))?,
            ))
        })
        .collect()
}

// ------------------------------------------------------------------- serve

pub async fn serve(state: AppState) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(&state.cfg.bind)
        .await
        .map_err(|e| format!("cannot bind {}: {e}", state.cfg.bind))?;
    let addr = listener.local_addr()?;
    crate::info!("listening on http://{addr}");
    if !addr.ip().is_loopback() {
        crate::warn!(
            "bound to non-loopback address {addr}; make sure it is a private network (e.g. Tailscale)"
        );
    }
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(shutdown_signal(state.clone()))
        .await?;
    // Cancelled turns are still stopping their CLIs' process groups (TERM,
    // grace, KILL); the runtime must outlive that cleanup.
    wait_for_turns(&state, std::time::Duration::from_secs(9)).await;
    // Same for connector programs that were asked to stop.
    crate::gateway::upstream::wait_for_reaping(std::time::Duration::from_secs(4)).await;
    Ok(())
}

/// Wait until no turn is running, at most `limit`.
async fn wait_for_turns(state: &AppState, limit: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline
        && !state
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty()
    {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn shutdown_signal(state: AppState) {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
    crate::info!("shutting down");
    // Stop running turns (kills their CLI process groups); open WebSockets
    // would otherwise keep graceful shutdown waiting forever.
    for tx in state
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
    {
        let _ = tx.send(true);
    }
    // Connector programs Iris started run in their own process groups:
    // end them (TERM, then KILL shortly after) before exiting.
    crate::gateway::cancel_all_grants(&state);
    crate::gateway::cancel_approvals(&state, None);
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        crate::gateway::stop_all(&state),
    )
    .await;
    // Last resort if something still hangs (turn cleanup takes at most
    // about five seconds).
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    struct Harness {
        state: AppState,
        app: Router,
        _tmp: tempfile::TempDir,
    }

    fn setup() -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = Config::default();
        // Never touch real CLIs from tests.
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        let state = AppState::new(cfg, paths, db).unwrap();
        Harness {
            app: router(state.clone()),
            state,
            _tmp: tmp,
        }
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = HttpRequest::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Pair a device for `user_id` (owner when None) and return its token.
    async fn pair_as(h: &Harness, user_id: Option<&str>) -> String {
        let uid = user_id.map(String::from);
        let (code, _) = h
            .state
            .db
            .with(|c| {
                let u = match uid {
                    Some(u) => u,
                    None => db::owner_user_id(c)?.unwrap(),
                };
                Ok(auth::create_pairing_code(c, &u))
            })
            .unwrap()
            .unwrap();
        let (s, body) = send(
            &h.app,
            "POST",
            "/api/pair",
            None,
            Some(json!({"code": code, "device_name": "test"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        body["token"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn routes_require_auth_except_public_ones() {
        let h = setup();
        for (m, uri) in [
            ("GET", "/api/conversations"),
            ("GET", "/api/settings"),
            ("GET", "/api/goals"),
            ("GET", "/api/credentials"),
            ("GET", "/api/harnesses"),
            ("POST", "/api/jobs/feed/run"),
            ("GET", "/api/avatar"),
        ] {
            let (s, body) = send(&h.app, m, uri, None, None).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{m} {uri}");
            assert_eq!(body["error"]["code"], "unauthorized");
            let (s, _) = send(&h.app, m, uri, Some("iris_bogus"), None).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{m} {uri} bogus token");
        }
        let (s, body) = send(&h.app, "GET", "/api/health", None, None).await;
        assert_eq!((s, body["ok"].as_bool()), (StatusCode::OK, Some(true)));
        let (s, _) = send(&h.app, "GET", "/api/nope", None, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pairing_and_basic_chat_listing() {
        let h = setup();
        let t = pair_as(&h, None).await;
        let (s, me) = send(&h.app, "GET", "/api/me", Some(&t), None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(me["user"]["is_owner"].as_bool().unwrap());
        let (_, convs) = send(&h.app, "GET", "/api/conversations", Some(&t), None).await;
        assert_eq!(convs["conversations"][0]["kind"], "main");
        let (s, _) = send(
            &h.app,
            "POST",
            "/api/pair",
            None,
            Some(json!({"code": "ZZZZ-ZZZZ"})),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn credential_partial_update_keeps_other_secret_fields() {
        let h = setup();
        let t = pair_as(&h, None).await;
        let (s, c) = send(
            &h.app,
            "POST",
            "/api/credentials",
            Some(&t),
            Some(
                json!({"kind": "login", "label": "Bank", "site": "bank.example",
                        "username": "someone", "password": "pw-1"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{c}");
        assert!(!c.to_string().contains("pw-1"));
        let id = c["credential"]["id"].as_str().unwrap().to_string();
        let (s, u) = send(
            &h.app,
            "PATCH",
            &format!("/api/credentials/{id}"),
            Some(&t),
            Some(json!({"notes": "security question: blue"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{u}");
        assert_eq!(u["credential"]["metadata"]["site"], "bank.example");
        let uid = u["credential"]["id"].as_str().unwrap().to_string();
        let owner = h.state.db.with(db::owner_user_id).unwrap().unwrap();
        let ct = h
            .state
            .db
            .with(|c| crate::entities::get_credential_ciphertext(c, &owner, &uid))
            .unwrap()
            .unwrap();
        let secret: Value =
            serde_json::from_slice(&h.state.vault.decrypt(&ct, &uid).unwrap()).unwrap();
        assert_eq!(
            secret,
            json!({"password": "pw-1", "notes": "security question: blue"})
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_credential_updates_keep_every_field() {
        let h = setup();
        let t = pair_as(&h, None).await;
        let (_, c) = send(
            &h.app,
            "POST",
            "/api/credentials",
            Some(&t),
            Some(
                json!({"kind": "login", "label": "Bank", "site": "bank.example",
                        "username": "someone", "password": "pw-0"}),
            ),
        )
        .await;
        let id = c["credential"]["id"].as_str().unwrap().to_string();
        let uri = format!("/api/credentials/{id}");
        let owner = h.state.db.with(db::owner_user_id).unwrap().unwrap();
        for i in 1..=25 {
            let (a, b) = tokio::join!(
                send(
                    &h.app,
                    "PATCH",
                    &uri,
                    Some(&t),
                    Some(json!({"password": format!("pw-{i}")}))
                ),
                send(
                    &h.app,
                    "PATCH",
                    &uri,
                    Some(&t),
                    Some(json!({"notes": format!("n-{i}")}))
                ),
            );
            assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
            let ct = h
                .state
                .db
                .with(|c| crate::entities::get_credential_ciphertext(c, &owner, &id))
                .unwrap()
                .unwrap();
            let secret: Value =
                serde_json::from_slice(&h.state.vault.decrypt(&ct, &id).unwrap()).unwrap();
            assert_eq!(
                secret,
                json!({"password": format!("pw-{i}"), "notes": format!("n-{i}")}),
                "round {i}"
            );
        }
    }

    #[tokio::test]
    async fn group_permissions_and_owner_only_routes() {
        let h = setup();
        let owner_t = pair_as(&h, None).await;
        let (_, u) = send(
            &h.app,
            "POST",
            "/api/users",
            Some(&owner_t),
            Some(json!({"display_name": "Sam"})),
        )
        .await;
        let sam_id = u["user"]["id"].as_str().unwrap().to_string();
        let sam_t = pair_as(&h, Some(&sam_id)).await;
        let (s, g) = send(
            &h.app,
            "POST",
            "/api/conversations",
            Some(&owner_t),
            Some(json!({"kind": "group", "title": "Family", "member_ids": [sam_id]})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{g}");
        let gid = g["conversation"]["id"].as_str().unwrap().to_string();
        assert_eq!(g["conversation"]["member_count"], 2);
        // Sam can read the group but not delete or rename it.
        let (s, _) = send(
            &h.app,
            "GET",
            &format!("/api/conversations/{gid}"),
            Some(&sam_t),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = send(
            &h.app,
            "DELETE",
            &format!("/api/conversations/{gid}"),
            Some(&sam_t),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = send(
            &h.app,
            "PATCH",
            &format!("/api/conversations/{gid}"),
            Some(&sam_t),
            Some(json!({"title": "Mine"})),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (_, m) = send(
            &h.app,
            "GET",
            &format!("/api/conversations/{gid}/members"),
            Some(&sam_t),
            None,
        )
        .await;
        assert_eq!(m["members"].as_array().unwrap().len(), 2);
        // Owner-only areas.
        for (method, uri) in [
            ("GET", "/api/credentials"),
            ("GET", "/api/connectors"),
            ("GET", "/api/files"),
            ("POST", "/api/harnesses/refresh"),
            ("POST", "/api/jobs/feed/run"),
        ] {
            let (s, _) = send(&h.app, method, uri, Some(&sam_t), None).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}");
        }
        // Sam's own goals are separate from the owner's.
        send(
            &h.app,
            "POST",
            "/api/goals",
            Some(&owner_t),
            Some(json!({"title": "Owner goal"})),
        )
        .await;
        let (_, goals) = send(&h.app, "GET", "/api/goals", Some(&sam_t), None).await;
        assert_eq!(goals["goals"].as_array().unwrap().len(), 0);
        // Members can leave; the creator stays.
        let (s, _) = send(
            &h.app,
            "DELETE",
            &format!("/api/conversations/{gid}/members/{sam_id}"),
            Some(&sam_t),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn feed_like_without_body_and_message_paging() {
        let h = setup();
        let t = pair_as(&h, None).await;
        let owner = h.state.db.with(db::owner_user_id).unwrap().unwrap();
        let (post_id, main) = h
            .state
            .db
            .with(|c| {
                let p = crate::entities::create_feed_post(
                    c,
                    &owner,
                    &crate::entities::NewFeedPost {
                        title: "T".into(),
                        body: "B".into(),
                        ..Default::default()
                    },
                )?;
                let main = db::ensure_main_conversation(c, &owner)?;
                for i in 0..3 {
                    db::insert_message(
                        c,
                        &db::NewMessage {
                            conversation_id: &main,
                            role: "user",
                            content: &format!("m{i}"),
                            ..Default::default()
                        },
                    )?;
                }
                Ok((p.id, main))
            })
            .unwrap();
        let (s, b) = send(
            &h.app,
            "POST",
            &format!("/api/feed/{post_id}/like"),
            Some(&t),
            None,
        )
        .await;
        assert_eq!((s, b["liked"].as_bool()), (StatusCode::OK, Some(true)));
        let (_, b) = send(
            &h.app,
            "POST",
            &format!("/api/feed/{post_id}/like"),
            Some(&t),
            Some(json!({"liked": false})),
        )
        .await;
        assert_eq!(b["liked"], false);
        let (_, page) = send(
            &h.app,
            "GET",
            &format!("/api/conversations/{main}/messages?limit=2"),
            Some(&t),
            None,
        )
        .await;
        let texts: Vec<_> = page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].clone())
            .collect();
        assert_eq!(texts, [json!("m1"), json!("m2")]);
        assert_eq!(page["has_more"], true);
        let (_, page) = send(
            &h.app,
            "GET",
            &format!("/api/conversations/{main}/messages?limit=3"),
            Some(&t),
            None,
        )
        .await;
        assert_eq!(page["has_more"], false);
    }

    #[tokio::test]
    async fn uploads_are_validated() {
        let h = setup();
        let t = pair_as(&h, None).await;
        let req = |body: &'static [u8]| {
            HttpRequest::builder()
                .method("POST")
                .uri("/api/attachments")
                .header("authorization", format!("Bearer {t}"))
                .body(Body::from(body))
                .unwrap()
        };
        let res = h.app.clone().oneshot(req(b"<svg onload=x>")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let res = h
            .app
            .clone()
            .oneshot(req(b"\x89PNG\r\n\x1a\n0000"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// A stand-in for `claude` that implements `--version`, `auth status`
    /// and `auth login` the way the real CLI behaves headless.
    fn fake_claude(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let marker = dir.join("signed-in");
        let script = dir.join("fake-claude");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
case "$1 $2" in
  "--version "*) echo "9.9.9 (Fake Claude)";;
  "auth status") if [ -f "{m}" ]; then echo '{{"loggedIn": true}}'; else echo '{{"loggedIn": false}}'; exit 1; fi;;
  "auth login")
    echo started >> "{starts}"
    echo "Opening browser to sign in..."
    echo "If the browser didn't open, visit: https://login.example.com/oauth/authorize?code=true&state=s1"
    printf "Paste code here if prompted > "
    while read code; do
      if [ "$code" = "good-code" ]; then touch "{m}"; echo "Login successful."; exit 0; fi
      echo "Invalid code. Please make sure the full code was copied."
    done
    exit 1;;
esac
"#,
                m = marker.display(),
                starts = dir.join("login-starts").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    async fn wait_login(h: &Harness, t: &str, want: &str) -> Value {
        for _ in 0..100 {
            let (_, b) = send(&h.app, "GET", "/api/harnesses/claude/login", Some(t), None).await;
            if b["login"]["state"] == want {
                return b;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("login never reached {want}");
    }

    #[tokio::test]
    async fn signed_out_engine_gets_a_reconnect_card_and_app_login() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = Config::default();
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        let claude = cfg.harness.get_mut("claude").unwrap();
        claude.enabled = true;
        claude.command = fake_claude(tmp.path()).to_string_lossy().into_owned();
        let state = AppState::new(cfg, paths.clone(), db).unwrap();
        state.set_serving();
        let h = Harness {
            app: router(state.clone()),
            state,
            _tmp: tmp,
        };
        let t = pair_as(&h, None).await;
        // Seen signed in once, then the probe says signed out.
        h.state
            .note_auth("claude", crate::harness::AuthState::Ok, None)
            .await;
        h.state.refresh_harnesses().await;
        let (_, list) = send(&h.app, "GET", "/api/harnesses", Some(&t), None).await;
        let claude = list["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "claude")
            .unwrap()
            .clone();
        assert_eq!(claude["auth"], "auth_required");
        assert_eq!(claude["login_flow"], "code_paste");
        let (_, convs) = send(&h.app, "GET", "/api/conversations", Some(&t), None).await;
        let main = convs["conversations"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let msgs_uri = format!("/api/conversations/{main}/messages");
        let (_, msgs) = send(&h.app, "GET", &msgs_uri, Some(&t), None).await;
        let card = msgs["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["card"]["type"] == "engine_signin")
            .expect("reconnect card")
            .clone();
        assert_eq!(card["card"]["state"], "needs_reconnect");
        assert!(
            card["card"]["body"]
                .as_str()
                .unwrap()
                .contains("can't reply")
        );
        // A second probe in the same outage does not stack another card.
        h.state.refresh_harnesses().await;
        let (_, msgs) = send(&h.app, "GET", &msgs_uri, Some(&t), None).await;
        let n = msgs["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["card"]["type"] == "engine_signin")
            .count();
        assert_eq!(n, 1);

        // Reconnect from the app: URL relayed, wrong code, then right code.
        let (s, b) = send(
            &h.app,
            "POST",
            "/api/harnesses/claude/login",
            Some(&t),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let b = wait_login(&h, &t, "waiting_for_code").await;
        assert!(
            b["login"]["url"]
                .as_str()
                .unwrap()
                .starts_with("https://login.example.com/")
        );
        let code_uri = "/api/harnesses/claude/login/code";
        let (s, b) = send(
            &h.app,
            "POST",
            code_uri,
            Some(&t),
            Some(json!({"code": "bad-code"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["login"]["state"], "verifying");
        // The CLI rejects it: back to waiting, with a friendly error.
        let mut rejected = Value::Null;
        for _ in 0..100 {
            let (_, b) = send(&h.app, "GET", "/api/harnesses/claude/login", Some(&t), None).await;
            if b["login"]["state"] == "waiting_for_code" && b["login"]["error"].is_string() {
                rejected = b;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            rejected["login"]["error"]
                .as_str()
                .is_some_and(|e| e.contains("didn't work")),
            "rejected code was reported: {rejected}"
        );
        let (s, _) = send(
            &h.app,
            "POST",
            code_uri,
            Some(&t),
            Some(json!({"code": "good-code"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        wait_login(&h, &t, "connected").await;
        assert_eq!(
            h.state.harness("claude").unwrap().status().auth,
            crate::harness::AuthState::Ok
        );
        let (_, msgs) = send(&h.app, "GET", &msgs_uri, Some(&t), None).await;
        let card = msgs["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["card"]["type"] == "engine_signin")
            .unwrap()
            .clone();
        assert_eq!(card["card"]["state"], "connected");
        assert!(card["card"]["url"].is_null());
        // The code never reaches the database.
        let dump = h
            .state
            .db
            .with(|c| {
                let mut out = String::new();
                for table in ["messages", "settings"] {
                    let mut st = c.prepare(&format!("SELECT * FROM {table}"))?;
                    let n = st.column_count();
                    let mut rows = st.query([])?;
                    while let Some(r) = rows.next()? {
                        for i in 0..n {
                            if let Ok(v) = r.get::<_, String>(i) {
                                out.push_str(&v);
                            }
                        }
                    }
                }
                Ok(out)
            })
            .unwrap();
        assert!(!dump.contains("good-code") && !dump.contains("bad-code"));
        // Non-owners cannot drive sign-in.
        let (s, _) = send(&h.app, "POST", "/api/harnesses/claude/login", None, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_probe_never_overwrites_a_newer_sign_in_report() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = Config::default();
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        // A slow `--version`, so a turn can report in the middle.
        let script = tmp.path().join("fake-gemini");
        std::fs::write(&script, "#!/bin/sh\nsleep 0.5\necho 0.0.1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let gemini = cfg.harness.get_mut("gemini").unwrap();
        gemini.enabled = true;
        gemini.command = script.to_string_lossy().into_owned();
        let state = AppState::new(cfg, paths, db).unwrap();
        let st = state.clone();
        let probe = tokio::spawn(async move { st.refresh_harnesses().await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        state
            .note_auth("gemini", crate::harness::AuthState::AuthRequired, None)
            .await;
        probe.await.unwrap();
        let status = state.harness("gemini").unwrap().status();
        assert_eq!(status.auth, crate::harness::AuthState::AuthRequired);
        assert_eq!(status.version.as_deref(), Some("0.0.1"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_older_auth_probe_never_overwrites_a_newer_report() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = Config::default();
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        // `auth status` saw a signed-out CLI, but answers slowly.
        let script = tmp.path().join("fake-claude");
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$1 $2\" in\n  \"--version \"*) echo 9.9.9;;\n  \"auth status\") sleep 0.5; echo '{\"loggedIn\": false}'; exit 1;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let claude = cfg.harness.get_mut("claude").unwrap();
        claude.enabled = true;
        claude.command = script.to_string_lossy().into_owned();
        let state = AppState::new(cfg, paths, db).unwrap();
        let st = state.clone();
        let probe = tokio::spawn(async move { st.refresh_harnesses().await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // Meanwhile a turn (or the in-app sign-in) found it signed in.
        state
            .note_auth("claude", crate::harness::AuthState::Ok, None)
            .await;
        probe.await.unwrap();
        assert_eq!(
            state.harness("claude").unwrap().status().auth,
            crate::harness::AuthState::Ok
        );
        // A probe that starts afterwards is applied as usual.
        state.refresh_harnesses().await;
        assert_eq!(
            state.harness("claude").unwrap().status().auth,
            crate::harness::AuthState::AuthRequired
        );
    }

    #[tokio::test]
    async fn push_is_unavailable_without_a_contact() {
        let h = setup();
        let t = pair_as(&h, None).await;
        let (_, b) = send(&h.app, "GET", "/api/push/vapid-public-key", Some(&t), None).await;
        assert_eq!(b["enabled"], false, "{b}");
        assert!(b["reason"].as_str().unwrap().contains("vapid_subject"));
        let sub = json!({"endpoint": "https://fcm.googleapis.com/fcm/send/x",
                         "keys": {"p256dh": "x", "auth": "y"}});
        let (s, _) = send(
            &h.app,
            "POST",
            "/api/push/subscriptions",
            Some(&t),
            Some(sub),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn shutdown_waits_for_turns_to_stop() {
        let h = setup();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        h.state.running.lock().unwrap().insert("c1".into(), tx);
        let st = h.state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            st.running.lock().unwrap().remove("c1");
        });
        let start = std::time::Instant::now();
        wait_for_turns(&h.state, std::time::Duration::from_secs(5)).await;
        let waited = start.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(150),
            "{waited:?}"
        );
        assert!(waited < std::time::Duration::from_secs(2));
        assert!(h.state.running.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reconnect_clears_a_stale_gemini_sign_in_failure() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = Config::default();
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        let script = tmp.path().join("fake-gemini");
        std::fs::write(&script, "#!/bin/sh\necho 0.0.1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let gemini = cfg.harness.get_mut("gemini").unwrap();
        gemini.enabled = true;
        gemini.command = script.to_string_lossy().into_owned();
        let state = AppState::new(cfg, paths, db).unwrap();
        let h = Harness {
            app: router(state.clone()),
            state,
            _tmp: tmp,
        };
        let t = pair_as(&h, None).await;
        // A turn found Gemini signed out; the owner then signed in on the
        // server. Periodic probes cannot tell (Gemini has no status
        // command), so the failure sticks...
        h.state
            .note_auth("gemini", crate::harness::AuthState::AuthRequired, None)
            .await;
        h.state.refresh_harnesses().await;
        assert_eq!(
            h.state.harness("gemini").unwrap().status().auth,
            crate::harness::AuthState::AuthRequired
        );
        // ...until the owner presses Reconnect.
        let (s, b) = send(
            &h.app,
            "POST",
            "/api/harnesses/gemini/login",
            Some(&t),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["auth"], "unknown");
        assert!(b["manual"].is_string());
        assert_eq!(
            h.state.harness("gemini").unwrap().status().auth,
            crate::harness::AuthState::Unknown
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_login_requests_share_one_session() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = Config::default();
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        let claude = cfg.harness.get_mut("claude").unwrap();
        claude.enabled = true;
        claude.command = fake_claude(tmp.path()).to_string_lossy().into_owned();
        let state = AppState::new(cfg, paths, db).unwrap();
        let starts = tmp.path().join("login-starts");
        // Truly parallel requests (two worker threads).
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(4));
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let (st, barrier) = (state.clone(), barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    crate::login::start(&st, "claude").await.unwrap().id
                })
            })
            .collect();
        let mut ids = Vec::new();
        for t in tasks {
            ids.push(t.await.unwrap());
        }
        assert!(ids.iter().all(|i| *i == ids[0]), "{ids:?}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let n = std::fs::read_to_string(&starts)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(n, 1, "the CLI's login ran once");
        assert!(crate::login::cancel(&state, "claude").await);
        for _ in 0..100 {
            if state.logins.status("claude").unwrap().state == "cancelled" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let first = state.logins.status("claude").unwrap();
        assert_eq!(first.state, "cancelled");
        // A new attempt gets a fresh session the old one cannot touch.
        let next = crate::login::start(&state, "claude").await.unwrap();
        assert_ne!(next.id, first.id);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(state.logins.status("claude").unwrap().id, next.id);
        assert!(!state.logins.status("claude").unwrap().finished());
        crate::login::cancel(&state, "claude").await;
    }

    #[tokio::test]
    async fn engines_api_setup_card_and_helper() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = Config::default();
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        // A fake privileged helper: records its arguments, "installs" by
        // writing a launcher, prints the JSON report.
        let engines = tmp.path().join("engines");
        let log = tmp.path().join("helper.log");
        let helper = tmp.path().join("helper.sh");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{log}'\nmkdir -p \"$4/bin\"\nprintf '#!/bin/sh\\n' > \"$4/bin/$2\"\nchmod +x \"$4/bin/$2\"\necho '{{\"name\":\"'$2'\",\"version\":\"1.2.3\",\"changed\":true}}'\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        cfg.engines.dir = engines.to_string_lossy().into_owned();
        cfg.engines.helper = vec![helper.to_string_lossy().into_owned()];
        let state = AppState::new(cfg, paths, db).unwrap();
        state.set_serving();
        let h = Harness {
            app: router(state.clone()),
            state,
            _tmp: tmp,
        };
        let t = pair_as(&h, None).await;

        // First run with no engine: one setup card in Main.
        crate::setup::post_setup_card_if_needed(&h.state).await;
        crate::setup::post_setup_card_if_needed(&h.state).await;
        let (_, convs) = send(&h.app, "GET", "/api/conversations", Some(&t), None).await;
        let main = convs["conversations"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (_, msgs) = send(
            &h.app,
            "GET",
            &format!("/api/conversations/{main}/messages"),
            Some(&t),
            None,
        )
        .await;
        let setup: Vec<&Value> = msgs["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["card"]["type"] == "engine_setup")
            .collect();
        assert_eq!(setup.len(), 1);
        assert_eq!(setup[0]["card"]["options"].as_array().unwrap().len(), 4);

        let (s, list) = send(&h.app, "GET", "/api/engines", Some(&t), None).await;
        assert_eq!(s, StatusCode::OK, "{list}");
        assert_eq!(list["engines"].as_array().unwrap().len(), 4);
        assert_eq!(list["privileged_helper"], true);
        // Unknown engines and removing what Iris did not install.
        let (s, _) = send(&h.app, "POST", "/api/engines/evil/install", Some(&t), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = send(&h.app, "DELETE", "/api/engines/codex", Some(&t), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // Install goes through the helper.
        let (s, b) = send(&h.app, "POST", "/api/engines/codex/install", Some(&t), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        for _ in 0..100 {
            if h.state
                .engine_ops
                .get("codex")
                .is_some_and(|o| o.state != "running")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        let op = h.state.engine_ops.get("codex").unwrap();
        assert_eq!((op.state, op.version.as_deref()), ("done", Some("1.2.3")));
        let args = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            args.trim(),
            format!("install codex --dir {}", engines.display())
        );
        assert!(crate::engines::is_installed(&engines, "codex"));
        // Now it may be removed.
        let (s, _) = send(&h.app, "DELETE", "/api/engines/codex", Some(&t), None).await;
        assert_eq!(s, StatusCode::OK);
    }

    async fn get_raw(
        app: &Router,
        uri: &str,
        etag: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut req = HttpRequest::builder()
            .method("GET")
            .uri(uri)
            .header("host", "iris.example.ts.net");
        if let Some(e) = etag {
            req = req.header("if-none-match", e);
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
            .await
            .unwrap();
        (status, headers, bytes.to_vec())
    }

    #[tokio::test]
    async fn serves_the_app_and_console() {
        let h = setup();
        let (s, hd, body) = get_raw(&h.app, "/", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(
            hd[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert!(String::from_utf8_lossy(&body).contains("<div id=\"app\">"));
        let csp = hd[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.contains("script-src 'self';") && csp.contains("wss://iris.example.ts.net"));
        let etag = hd[header::ETAG].to_str().unwrap().to_string();
        let (s, _, body) = get_raw(&h.app, "/", Some(&etag)).await;
        assert_eq!((s, body.len()), (StatusCode::NOT_MODIFIED, 0));
        let (s, hd, _) = get_raw(&h.app, "/js/app.js", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(
            hd[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/javascript")
        );
        let (s, _, body) = get_raw(&h.app, "/console", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).contains("<html"));
        for missing in [
            "/js/nope.js",
            "/../Cargo.toml",
            "/web/index.html",
            "/vendor/LICENSES.md",
        ] {
            let (s, _, _) = get_raw(&h.app, missing, None).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{missing}");
        }
    }

    #[tokio::test]
    async fn feed_photos_saved_posts_and_suggested_goals() {
        let h = setup();
        let t = pair_as(&h, None).await;
        let owner = h.state.db.with(db::owner_user_id).unwrap().unwrap();
        let (liked_id, plain_id) = h
            .state
            .db
            .with(|c| {
                let new = |title: &str, images: Vec<String>| crate::entities::NewFeedPost {
                    title: title.into(),
                    body: "Body".into(),
                    images,
                    source_name: Some("Example Daily".into()),
                    why: Some("Because you liked bikes".into()),
                    image_url: Some("https://img.example/cover.jpg".into()),
                    ..Default::default()
                };
                let a = crate::entities::create_feed_post(
                    c,
                    &owner,
                    &new(
                        "A",
                        vec![
                            "https://img.example/2.jpg".into(),
                            "https://img.example/cover.jpg".into(),
                        ],
                    ),
                )?;
                let b = crate::entities::create_feed_post(c, &owner, &new("B", vec![]))?;
                crate::entities::set_feed_liked(c, &owner, &a.id, true)?;
                Ok((a.id, b.id))
            })
            .unwrap();
        let (_, feed) = send(&h.app, "GET", "/api/feed", Some(&t), None).await;
        let a = feed["posts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == liked_id.as_str())
            .unwrap()
            .clone();
        assert_eq!(
            a["images"],
            json!(["https://img.example/cover.jpg", "https://img.example/2.jpg"])
        );
        assert_eq!(a["image_url"], "https://img.example/cover.jpg");
        assert_eq!(
            (a["source_name"].as_str(), a["why"].as_str()),
            (Some("Example Daily"), Some("Because you liked bikes"))
        );
        let (_, saved) = send(&h.app, "GET", "/api/feed?liked=true", Some(&t), None).await;
        let ids: Vec<_> = saved["posts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].clone())
            .collect();
        assert_eq!(ids, [json!(liked_id)]);
        assert_ne!(liked_id, plain_id);
        let bad = crate::entities::NewFeedPost {
            title: "x".into(),
            body: "y".into(),
            images: vec!["javascript:alert(1)".into()],
            ..Default::default()
        };
        assert!(bad.validate().is_err());

        // Suggested goals are confirmed with `suggested: false`.
        let g = h
            .state
            .db
            .with(|c| crate::entities::create_goal(c, &owner, "Run a 5k", "", true))
            .unwrap();
        let (_, goals) = send(&h.app, "GET", "/api/goals", Some(&t), None).await;
        assert_eq!(goals["goals"][0]["suggested"], true);
        let (s, b) = send(
            &h.app,
            "PATCH",
            &format!("/api/goals/{}", g.id),
            Some(&t),
            Some(json!({"suggested": false})),
        )
        .await;
        assert_eq!(
            (s, b["goal"]["suggested"].as_bool()),
            (StatusCode::OK, Some(false))
        );
        let (_, b) = send(
            &h.app,
            "POST",
            "/api/goals",
            Some(&t),
            Some(json!({"title": "Mine"})),
        )
        .await;
        assert_eq!(b["goal"]["suggested"], false);
    }

    #[tokio::test]
    async fn sent_photos_activity_log_and_quiet_hours() {
        let h = setup();
        let t = pair_as(&h, None).await;
        let owner = h.state.db.with(db::owner_user_id).unwrap().unwrap();
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/api/attachments")
            .header("authorization", format!("Bearer {t}"))
            .body(Body::from(b"\x89PNG\r\n\x1a\nfake".to_vec()))
            .unwrap();
        let res = h.app.clone().oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let att: Value = serde_json::from_slice(&bytes).unwrap();
        let att_id = att["attachment"]["id"].as_str().unwrap().to_string();
        // Uploaded but not sent yet: not in the Library.
        let (_, b) = send(&h.app, "GET", "/api/attachments", Some(&t), None).await;
        assert_eq!(b["attachments"], json!([]));
        let (main, job) = h
            .state
            .db
            .with(|c| {
                let main = db::ensure_main_conversation(c, &owner)?;
                db::insert_message(c, &db::NewMessage {
                    conversation_id: &main,
                    role: "user",
                    author_user_id: Some(&owner),
                    content: "look",
                    attachments: std::slice::from_ref(&att_id),
                    ..Default::default()
                })?;
                let meta = json!({"activity": [{"id": "1", "kind": "web_search", "name": "WebSearch", "status": "completed", "summary": "q"}],
                                  "routing": {"model": "claude:haiku"}, "duration_ms": 1200});
                db::insert_message(c, &db::NewMessage {
                    conversation_id: &main,
                    role: "assistant",
                    content: "answer",
                    meta: Some(&meta),
                    ..Default::default()
                })?;
                db::insert_message(c, &db::NewMessage { conversation_id: &main, role: "assistant", content: "no tools", ..Default::default() })?;
                let job = db::ensure_job_conversation(c, &owner, "heartbeat")?;
                db::insert_message(c, &db::NewMessage { conversation_id: &job, role: "assistant", content: "x", meta: Some(&meta), ..Default::default() })?;
                Ok((main, job))
            })
            .unwrap();
        let (_, b) = send(&h.app, "GET", "/api/attachments", Some(&t), None).await;
        assert_eq!(b["attachments"][0]["id"], att_id.as_str());
        assert_eq!(b["attachments"][0]["conversation_id"], main.as_str());
        let (_, b) = send(&h.app, "GET", "/api/activity", Some(&t), None).await;
        let items = b["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "{b}");
        assert!(
            items
                .iter()
                .any(|i| i["conversation_id"] == job.as_str() && i["conversation_kind"] == "job")
        );
        assert_eq!(items[0]["activity"][0]["name"], "WebSearch");
        assert!(items[0].get("routing").is_none() && !b.to_string().contains("claude:haiku"));

        // A second person sees none of it.
        let (_, u) = send(
            &h.app,
            "POST",
            "/api/users",
            Some(&t),
            Some(json!({"display_name": "Sam"})),
        )
        .await;
        let sam = pair_as(&h, u["user"]["id"].as_str()).await;
        let (_, b) = send(&h.app, "GET", "/api/activity", Some(&sam), None).await;
        assert_eq!(b["items"], json!([]));
        let (_, b) = send(&h.app, "GET", "/api/attachments", Some(&sam), None).await;
        assert_eq!(b["attachments"], json!([]));

        let (s, _) = send(
            &h.app,
            "PUT",
            "/api/me/quiet-hours",
            Some(&t),
            Some(json!({"quiet_hours": {"start": "25:00", "end": "07:00"}})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, b) = send(
            &h.app,
            "PUT",
            "/api/me/quiet-hours",
            Some(&t),
            Some(json!({"quiet_hours": {"start": "22:00", "end": "07:00"}})),
        )
        .await;
        assert_eq!(
            (s, b["quiet_hours"]["start"].as_str()),
            (StatusCode::OK, Some("22:00"))
        );
        let (_, me) = send(&h.app, "GET", "/api/me", Some(&t), None).await;
        assert_eq!(me["quiet_hours"]["end"], "07:00");
        let (_, me) = send(&h.app, "GET", "/api/me", Some(&sam), None).await;
        assert_eq!(me["quiet_hours"], Value::Null);
        send(
            &h.app,
            "PUT",
            "/api/me/quiet-hours",
            Some(&t),
            Some(json!({"quiet_hours": null})),
        )
        .await;
        let (_, me) = send(&h.app, "GET", "/api/me", Some(&t), None).await;
        assert_eq!(me["quiet_hours"], Value::Null);
    }

    #[tokio::test]
    async fn approval_cards_are_decided_once_by_their_approver() {
        let h = setup();
        let t = pair_as(&h, None).await;
        let owner = h.state.db.with(db::owner_user_id).unwrap().unwrap();
        let (_, u) = send(
            &h.app,
            "POST",
            "/api/users",
            Some(&t),
            Some(json!({"display_name": "Sam"})),
        )
        .await;
        let sam_id = u["user"]["id"].as_str().unwrap().to_string();
        let sam = pair_as(&h, Some(&sam_id)).await;
        let (_, g) = send(
            &h.app,
            "POST",
            "/api/conversations",
            Some(&t),
            Some(json!({"kind": "group", "title": "Us", "member_ids": [sam_id]})),
        )
        .await;
        let group = g["conversation"]["id"].as_str().unwrap().to_string();
        let (main, info, card_id) = h
            .state
            .db
            .with(|c| {
                let main = db::ensure_main_conversation(c, &owner)?;
                let card = crate::entities::approval_card(&crate::entities::ApprovalRequest {
                    title: "Send email from your mail",
                    service: "mail",
                    to: "friend@example.com",
                    subject: "Dinner",
                    preview: "Friday at 7?",
                    action_label: "Send",
                    approver_user_id: &owner,
                });
                let m = crate::entities::insert_card(c, &group, &card, "assistant")?;
                let info = crate::entities::insert_card(
                    c,
                    &main,
                    &json!({"type": "info", "title": "Hi"}),
                    "assistant",
                )?;
                Ok((main, info.id, m.id))
            })
            .unwrap();
        let _ = main;
        let path = format!("/api/messages/{card_id}/decision");
        let (s, _) = send(
            &h.app,
            "POST",
            &path,
            Some(&sam),
            Some(json!({"approve": true})),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "only the approver decides");
        let (s, _) = send(
            &h.app,
            "POST",
            &format!("/api/messages/{info}/decision"),
            Some(&t),
            Some(json!({"approve": true})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = send(
            &h.app,
            "POST",
            &format!("/api/messages/{info}/decision"),
            Some(&sam),
            Some(json!({"approve": true})),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "not visible to non-members");
        let (s, b) = send(
            &h.app,
            "POST",
            &path,
            Some(&t),
            Some(json!({"approve": false})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["message"]["card"]["state"], "denied");
        assert!(b["message"]["card"]["decided_at"].is_i64());
        assert_eq!(b["message"]["card"]["preview"]["to"], "friend@example.com");
        let (s, _) = send(
            &h.app,
            "POST",
            &path,
            Some(&t),
            Some(json!({"approve": true})),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
    }

    // ------------------------------------------------- turns with a fake CLI

    /// A stand-in for `claude` that records each run (argv, system prompt,
    /// MCP config) under `dir/runs/<n>/` and replies "ok". While
    /// `dir/hold` exists it waits before replying.
    fn recording_claude(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-claude");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
case "$1 $2" in
  "--version "*) echo "9.9.9 (Fake Claude)"; exit 0;;
  "auth status") echo '{{"loggedIn": true}}'; exit 0;;
esac
mkdir -p "{d}/runs"
n=0
while ! mkdir "{d}/runs/$n" 2>/dev/null; do n=$((n+1)); done
r="{d}/runs/$n"
printf '%s\n' "$@" > "$r/args"
prev=""
for a in "$@"; do
  case "$prev" in
    --append-system-prompt-file) cp "$a" "$r/system.md";;
    --mcp-config) cp "$a" "$r/mcp.json";;
  esac
  prev="$a"
done
cat > "$r/stdin"
touch "$r/started"
while [ -f "{d}/hold" ]; do sleep 0.02; done
echo '{{"type":"system","subtype":"init","session_id":"sess-'$n'"}}'
echo '{{"type":"assistant","message":{{"id":"m1","content":[{{"type":"text","text":"ok"}}]}}}}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"ok"}}'
"#,
                d = dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    struct TurnHarness {
        h: Harness,
        dir: std::path::PathBuf,
        owner_t: String,
        guest_id: String,
        guest_t: String,
    }

    impl TurnHarness {
        fn run_dir(&self, n: usize) -> std::path::PathBuf {
            self.dir.join("runs").join(n.to_string())
        }

        fn runs(&self) -> usize {
            std::fs::read_dir(self.dir.join("runs"))
                .map(|r| r.count())
                .unwrap_or(0)
        }

        fn read(&self, n: usize, file: &str) -> String {
            std::fs::read_to_string(self.run_dir(n).join(file)).unwrap_or_default()
        }

        async fn wait_for(&self, what: &str, mut f: impl FnMut() -> bool) {
            for _ in 0..250 {
                if f() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("timed out waiting for {what}");
        }

        /// Send a message and wait for the reply to finish.
        async fn chat(&self, token: &str, conv: &str, text: &str) -> Value {
            let (s, b) = send(
                &self.h.app,
                "POST",
                &format!("/api/conversations/{conv}/messages"),
                Some(token),
                Some(json!({"text": text})),
            )
            .await;
            assert_eq!(s, StatusCode::ACCEPTED, "{b}");
            let id = b["assistant_message"]["id"].as_str().unwrap().to_string();
            for _ in 0..250 {
                let (_, msgs) = send(
                    &self.h.app,
                    "GET",
                    &format!("/api/conversations/{conv}/messages"),
                    Some(token),
                    None,
                )
                .await;
                if let Some(m) = msgs["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|m| m["id"] == id.as_str() && m["status"] != "streaming")
                {
                    return m.clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("reply never finished");
        }

        async fn main_of(&self, token: &str) -> String {
            let (_, b) = send(
                &self.h.app,
                "GET",
                "/api/conversations/main",
                Some(token),
                None,
            )
            .await;
            b["conversation"]["id"].as_str().unwrap().to_string()
        }

        async fn create(&self, kind: &str, members: &[&str]) -> String {
            let (s, b) = send(
                &self.h.app,
                "POST",
                "/api/conversations",
                Some(&self.owner_t),
                Some(json!({"kind": kind, "member_ids": members})),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{b}");
            b["conversation"]["id"].as_str().unwrap().to_string()
        }
    }

    /// Claude as the only engine (a recording fake), the owner's private
    /// files filled in, a connector, and a second person.
    async fn turn_setup(edit: impl FnOnce(&mut Config)) -> TurnHarness {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let paths = Paths {
            config_file: dir.join("config.toml"),
            data_dir: dir.join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        std::fs::write(paths.persona_dir().join("USER.md"), "owner-user-fact").unwrap();
        std::fs::write(paths.memory_dir().join("MEMORY.md"), "owner-memory-fact").unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = Config::default();
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        let claude = cfg.harness.get_mut("claude").unwrap();
        claude.enabled = true;
        claude.command = recording_claude(&dir).to_string_lossy().into_owned();
        cfg.push.notify_replies_when_offline = false;
        edit(&mut cfg);
        let state = AppState::new(cfg, paths, db).unwrap();
        state.refresh_harnesses().await;
        let h = Harness {
            app: router(state.clone()),
            state,
            _tmp: tmp,
        };
        let owner_t = pair_as(&h, None).await;
        let (s, c) = send(
            &h.app,
            "POST",
            "/api/connectors",
            Some(&owner_t),
            Some(
                json!({"name": "notes", "transport": "stdio", "command": "notes-mcp",
                        "env": {"NOTES_TOKEN": "connector-secret"}}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{c}");
        let (_, u) = send(
            &h.app,
            "POST",
            "/api/users",
            Some(&owner_t),
            Some(json!({"display_name": "Sam"})),
        )
        .await;
        let guest_id = u["user"]["id"].as_str().unwrap().to_string();
        let guest_t = pair_as(&h, Some(&guest_id)).await;
        TurnHarness {
            h,
            dir,
            owner_t,
            guest_id,
            guest_t,
        }
    }

    #[tokio::test]
    async fn only_the_owner_alone_gets_private_context() {
        let t = turn_setup(|_| {}).await;
        let memory = t.h.state.paths.memory_dir().to_string_lossy().into_owned();
        // Another person's own Main chat: none of the owner's files,
        // memory folder, or connectors.
        let guest_main = t.main_of(&t.guest_t).await;
        let reply = t.chat(&t.guest_t, &guest_main, "hi").await;
        assert_eq!(reply["status"], "complete", "{reply}");
        let system = t.read(0, "system.md");
        assert!(!system.is_empty());
        assert!(!system.contains("owner-user-fact") && !system.contains("owner-memory-fact"));
        assert!(!system.contains(&memory));
        assert!(!t.read(0, "args").contains(&memory), "memory not writable");
        assert!(
            t.read(0, "args").contains("--strict-mcp-config"),
            "no CLI-level MCP"
        );
        let mcp: Value = serde_json::from_str(&t.read(0, "mcp.json")).unwrap();
        let servers: Vec<&String> = mcp["mcpServers"].as_object().unwrap().keys().collect();
        assert_eq!(servers, ["iris"]);
        // The owner's own chat has all of it.
        let owner_main = t.main_of(&t.owner_t).await;
        t.chat(&t.owner_t, &owner_main, "hi").await;
        let system = t.read(1, "system.md");
        assert!(system.contains("owner-user-fact") && system.contains("owner-memory-fact"));
        assert!(t.read(1, "args").contains(&memory));
        // Only Iris's MCP servers there too (the CLI's own would bypass the
        // connector gateway).
        assert!(t.read(1, "args").contains("--strict-mcp-config"));
        let mcp: Value = serde_json::from_str(&t.read(1, "mcp.json")).unwrap();
        assert!(mcp["mcpServers"]["notes"].is_object());
    }

    #[tokio::test]
    async fn a_group_gets_a_new_session_when_it_becomes_shared() {
        let t = turn_setup(|c| c.router.allow_claude_for_shared = true).await;
        let gid = t.create("group", &[]).await;
        t.chat(&t.owner_t, &gid, "hi").await;
        assert!(t.read(0, "system.md").contains("owner-memory-fact"));
        t.chat(&t.owner_t, &gid, "again").await;
        assert!(
            t.read(1, "args").contains("--resume\nsess-0"),
            "same people: resumed"
        );
        let (s, _) = send(
            &t.h.app,
            "POST",
            &format!("/api/conversations/{gid}/members"),
            Some(&t.owner_t),
            Some(json!({"user_id": t.guest_id})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        t.chat(&t.owner_t, &gid, "now with Sam").await;
        assert!(
            !t.read(2, "args").contains("--resume"),
            "never resume a private session"
        );
        let system = t.read(2, "system.md");
        assert!(!system.contains("owner-memory-fact") && !system.contains("owner-user-fact"));
    }

    #[tokio::test]
    async fn the_model_classifier_follows_the_shared_chat_policy() {
        let t = turn_setup(|c| c.router.classifier_model = true).await;
        let gid = t.create("group", &[&t.guest_id]).await;
        let reply = t.chat(&t.owner_t, &gid, "Let's plan the week").await;
        assert_eq!(reply["meta"]["error"]["kind"], "paused_policy", "{reply}");
        assert_eq!(t.runs(), 0, "Claude never saw the shared message");
        // In a personal chat the same message may be classified by Claude.
        let main = t.main_of(&t.owner_t).await;
        t.chat(&t.owner_t, &main, "Let's plan the week").await;
        assert_eq!(t.runs(), 2, "classifier run plus the reply");
    }

    #[tokio::test]
    async fn queued_turns_of_a_deleted_conversation_never_run() {
        let t = turn_setup(|_| {}).await;
        let side = t.create("side", &[]).await;
        std::fs::write(t.dir.join("hold"), "").unwrap();
        for text in ["first", "second"] {
            let (s, _) = send(
                &t.h.app,
                "POST",
                &format!("/api/conversations/{side}/messages"),
                Some(&t.owner_t),
                Some(json!({"text": text})),
            )
            .await;
            assert_eq!(s, StatusCode::ACCEPTED);
        }
        let started = t.run_dir(0).join("started");
        t.wait_for("first run", || started.exists()).await;
        let (s, _) = send(
            &t.h.app,
            "DELETE",
            &format!("/api/conversations/{side}"),
            Some(&t.owner_t),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        std::fs::remove_file(t.dir.join("hold")).unwrap();
        let lock = t.h.state.conversation_lock(&side);
        t.wait_for("queued turn", || {
            lock.try_lock().is_ok() && t.h.state.running.lock().unwrap().is_empty()
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(t.runs(), 1, "the queued message never reached the CLI");
    }

    #[tokio::test]
    async fn a_queued_turn_sees_who_is_in_the_chat_when_it_starts() {
        let t = turn_setup(|c| {
            c.max_concurrent_turns = 1;
            c.router.allow_claude_for_shared = true;
        })
        .await;
        // The only CLI slot is busy with another chat.
        let side = t.create("side", &[]).await;
        std::fs::write(t.dir.join("hold"), "").unwrap();
        let (s, _) = send(
            &t.h.app,
            "POST",
            &format!("/api/conversations/{side}/messages"),
            Some(&t.owner_t),
            Some(json!({"text": "busy"})),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let started = t.run_dir(0).join("started");
        t.wait_for("first run", || started.exists()).await;
        // A message in the owner's private group waits for the slot...
        let gid = t.create("group", &[]).await;
        let mut rx = t.h.state.hub.subscribe();
        let (s, b) = send(
            &t.h.app,
            "POST",
            &format!("/api/conversations/{gid}/messages"),
            Some(&t.owner_t),
            Some(json!({"text": "queued"})),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let queued = b["assistant_message"]["id"].as_str().unwrap().to_string();
        // It has read the members (owner only) and is waiting.
        loop {
            let env = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("queued status")
                .unwrap();
            let v: Value = serde_json::from_str(&env.json).unwrap();
            if v["type"] == "turn.status" && v["message_id"] == queued.as_str() {
                assert_eq!(v["status"], "queued");
                break;
            }
        }
        // ...while someone joins.
        let (s, _) = send(
            &t.h.app,
            "POST",
            &format!("/api/conversations/{gid}/members"),
            Some(&t.owner_t),
            Some(json!({"user_id": t.guest_id})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        std::fs::remove_file(t.dir.join("hold")).unwrap();
        let second = t.run_dir(1).join("started");
        t.wait_for("queued run", || second.exists()).await;
        let system = t.read(1, "system.md");
        assert!(!system.contains("owner-memory-fact") && !system.contains("owner-user-fact"));
        let mcp: Value = serde_json::from_str(&t.read(1, "mcp.json")).unwrap();
        assert!(mcp["mcpServers"]["notes"].is_null(), "no connectors");
        let reply = loop {
            let (_, msgs) = send(
                &t.h.app,
                "GET",
                &format!("/api/conversations/{gid}/messages"),
                Some(&t.owner_t),
                None,
            )
            .await;
            let m = msgs["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == queued.as_str())
                .cloned()
                .unwrap();
            if m["status"] != "streaming" {
                break m;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(reply["status"], "complete");
    }

    #[tokio::test]
    async fn joining_during_a_private_reply_withholds_it() {
        let t = turn_setup(|c| c.router.allow_claude_for_shared = true).await;
        let gid = t.create("group", &[]).await;
        std::fs::write(t.dir.join("hold"), "").unwrap();
        let (s, b) = send(
            &t.h.app,
            "POST",
            &format!("/api/conversations/{gid}/messages"),
            Some(&t.owner_t),
            Some(json!({"text": "what's in my memory?"})),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let reply_id = b["assistant_message"]["id"].as_str().unwrap().to_string();
        let started = t.run_dir(0).join("started");
        t.wait_for("private run", || started.exists()).await;
        assert!(t.read(0, "system.md").contains("owner-memory-fact"));
        let (s, _) = send(
            &t.h.app,
            "POST",
            &format!("/api/conversations/{gid}/members"),
            Some(&t.owner_t),
            Some(json!({"user_id": t.guest_id})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        std::fs::remove_file(t.dir.join("hold")).unwrap();
        let reply = loop {
            let (_, msgs) = send(
                &t.h.app,
                "GET",
                &format!("/api/conversations/{gid}/messages"),
                Some(&t.guest_t),
                None,
            )
            .await;
            let m = msgs["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == reply_id.as_str())
                .cloned()
                .unwrap();
            if m["status"] != "streaming" {
                break m;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(reply["status"], "cancelled", "{reply}");
        assert_eq!(reply["content"], "");
        assert_eq!(reply["meta"]["activity"], json!([]));
        assert_eq!(t.runs(), 1, "no retry with the private context");
    }

    #[tokio::test]
    async fn a_removed_sender_s_queued_message_is_not_answered() {
        let t = turn_setup(|c| c.router.allow_claude_for_shared = true).await;
        let gid = t.create("group", &[&t.guest_id]).await;
        std::fs::write(t.dir.join("hold"), "").unwrap();
        let (s, _) = send(
            &t.h.app,
            "POST",
            &format!("/api/conversations/{gid}/messages"),
            Some(&t.owner_t),
            Some(json!({"text": "first"})),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let started = t.run_dir(0).join("started");
        t.wait_for("first run", || started.exists()).await;
        // The guest's message queues behind the running turn, then the
        // guest is removed; only the owner would be left.
        let (s, b) = send(
            &t.h.app,
            "POST",
            &format!("/api/conversations/{gid}/messages"),
            Some(&t.guest_t),
            Some(json!({"text": "what is in the owner's memory?"})),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let reply_id = b["assistant_message"]["id"].as_str().unwrap().to_string();
        let (s, _) = send(
            &t.h.app,
            "DELETE",
            &format!("/api/conversations/{gid}/members/{}", t.guest_id),
            Some(&t.owner_t),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        std::fs::remove_file(t.dir.join("hold")).unwrap();
        let mut status = String::new();
        for _ in 0..250 {
            let (_, msgs) = send(
                &t.h.app,
                "GET",
                &format!("/api/conversations/{gid}/messages"),
                Some(&t.owner_t),
                None,
            )
            .await;
            status = msgs["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == reply_id.as_str())
                .map(|m| m["status"].as_str().unwrap().to_string())
                .unwrap_or_default();
            if status != "streaming" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(status, "cancelled");
        assert_eq!(
            t.runs(),
            1,
            "the CLI never saw the removed sender's message"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_slot_and_connector_changes_are_all_kept() {
        let t = turn_setup(|_| {}).await;
        let app = &t.h.app;
        let tok = Some(t.owner_t.as_str());
        for round in 0..10 {
            let pin = |m: &str| json!({"mode": "pinned", "model": m});
            let (a, b) = tokio::join!(
                send(
                    app,
                    "PUT",
                    "/api/models/slots/chat",
                    tok,
                    Some(pin("claude:haiku"))
                ),
                send(
                    app,
                    "PUT",
                    "/api/models/slots/feed",
                    tok,
                    Some(pin("claude:sonnet"))
                ),
            );
            assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK), "{}", a.1);
            let pins = t.h.state.slot_pins().await;
            assert_eq!(pins.len(), 2, "round {round}: {pins:?}");
            let auto = json!({"mode": "automatic"});
            let (a, b) = tokio::join!(
                send(
                    app,
                    "PUT",
                    "/api/models/slots/chat",
                    tok,
                    Some(auto.clone())
                ),
                send(
                    app,
                    "PUT",
                    "/api/models/slots/feed",
                    tok,
                    Some(auto.clone())
                ),
            );
            assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
            assert!(t.h.state.slot_pins().await.is_empty(), "round {round}");
        }
        let (_, list) = send(app, "GET", "/api/connectors", tok, None).await;
        let id = list["connectors"][0]["id"].as_str().unwrap().to_string();
        let uri = format!("/api/connectors/{id}");
        for round in 0..10 {
            let (a, b) = tokio::join!(
                send(
                    app,
                    "PATCH",
                    &uri,
                    tok,
                    Some(json!({"args": [format!("--n={round}")]}))
                ),
                send(
                    app,
                    "PATCH",
                    &uri,
                    tok,
                    Some(json!({"env": {"NOTES_TOKEN": format!("s-{round}")}}))
                ),
            );
            assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK), "{}", a.1);
            let con =
                t.h.state
                    .db
                    .with(|c| crate::entities::get_connector(c, &id))
                    .unwrap()
                    .unwrap();
            assert_eq!(
                con.config.args,
                vec![format!("--n={round}")],
                "round {round}"
            );
            let sealed = &con.config.env["NOTES_TOKEN"];
            let secret =
                t.h.state
                    .vault
                    .open_str(sealed, &connector_aad(&id, "env", "NOTES_TOKEN"))
                    .unwrap();
            assert_eq!(secret, format!("s-{round}"), "round {round}");
        }
    }

    #[tokio::test]
    async fn removed_members_stop_receiving_a_running_turn() {
        let t = turn_setup(|c| c.router.allow_claude_for_shared = true).await;
        let gid = t.create("group", &[&t.guest_id]).await;
        let mut rx = t.h.state.hub.subscribe();
        std::fs::write(t.dir.join("hold"), "").unwrap();
        let (s, _) = send(
            &t.h.app,
            "POST",
            &format!("/api/conversations/{gid}/messages"),
            Some(&t.owner_t),
            Some(json!({"text": "hello both"})),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let started = t.run_dir(0).join("started");
        t.wait_for("run", || started.exists()).await;
        let (s, _) = send(
            &t.h.app,
            "DELETE",
            &format!("/api/conversations/{gid}/members/{}", t.guest_id),
            Some(&t.owner_t),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        while rx.try_recv().is_ok() {}
        std::fs::remove_file(t.dir.join("hold")).unwrap();
        let mut seen = Vec::new();
        loop {
            let env = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("turn events")
                .unwrap();
            let v: Value = serde_json::from_str(&env.json).unwrap();
            if v["conversation_id"] != gid.as_str() {
                continue;
            }
            assert!(
                !env.audience.includes(&t.guest_id),
                "{} still sent to a removed member",
                v["type"]
            );
            seen.push(v["type"].as_str().unwrap_or("").to_string());
            if v["type"] == "message.completed" {
                break;
            }
        }
        assert!(seen.contains(&"message.delta".to_string()), "{seen:?}");
    }
}
