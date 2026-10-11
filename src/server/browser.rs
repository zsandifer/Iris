//! Iris's computer: browser status, start/stop, take over / hand back, and
//! installing Chromium from the app. Owner only. The live view and the
//! owner's input travel over the WebSocket (`browser.watch`,
//! `browser.input`; see `ws.rs`).

use super::{ApiError, ApiResult, AuthUser, JsonBody, ok};
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/browser", get(status))
        .route("/browser/start", post(start))
        .route("/browser/stop", post(stop))
        .route("/browser/takeover", post(take_over))
        .route("/browser/handback", post(hand_back))
        .route("/browser/navigate", post(navigate))
        .route("/browser/install", post(install))
}

/// Status plus how to get a browser if there is none.
pub fn status_json(state: &AppState) -> Value {
    let mut v = state.browser.state_json();
    let root = crate::setup::engines_root(state);
    v["managed"] = crate::engines::is_installed(&root, "browser").into();
    v["managed_version"] = crate::engines::installed_version(&root, "browser").into();
    v["requires"] = if crate::harness::resolve_command("npm").is_none() {
        "nodejs".into()
    } else {
        Value::Null
    };
    v["install"] = json!(state.engine_ops.get("browser"));
    v
}

/// Send the full status (with install state) to the owner's apps.
pub fn broadcast(state: &AppState) {
    state.hub.send(
        crate::events::Audience::Owner,
        json!({"type": "browser.state", "browser": status_json(state)}),
    );
}

async fn status(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    ok(json!({"browser": status_json(&state)}))
}

fn not_running(e: crate::util::BoxError) -> ApiError {
    ApiError::new(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "browser_unavailable",
        e.to_string(),
    )
}

async fn start(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    state.browser.ensure().await.map_err(not_running)?;
    ok(json!({"browser": status_json(&state)}))
}

async fn stop(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    state.browser.close().await;
    ok(json!({"browser": status_json(&state)}))
}

async fn take_over(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    state
        .browser
        .take_over(user.id(), &user.device.id)
        .await
        .map_err(not_running)?;
    ok(json!({"browser": status_json(&state)}))
}

async fn hand_back(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    state.browser.hand_back();
    ok(json!({"browser": status_json(&state)}))
}

#[derive(Deserialize)]
struct NavigateBody {
    url: String,
}

/// Open a page while the owner has control (for example a sign-in page).
async fn navigate(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<NavigateBody>,
) -> ApiResult {
    user.require_owner()?;
    state
        .browser
        .user_input(
            &user.device.id,
            &json!({"kind": "navigate", "url": body.url}),
        )
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    ok(json!({"ok": true}))
}

/// Install (or update) Chromium from Playwright's official download into
/// the engines directory.
async fn install(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    let started = crate::setup::start_op(&state, "browser", "install")?;
    ok(json!({"started": started, "browser": status_json(&state)}))
}
