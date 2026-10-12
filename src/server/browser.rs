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
        .route("/browser/sandbox", post(sandbox))
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
    v["sandbox_fix"] = if v["sandbox_unavailable"] == true {
        sandbox_fix(state, &root)
    } else {
        Value::Null
    };
    v
}

/// The exact step that lets Chrome's sandbox start on this machine (see
/// `browser::apparmor`). Trusted owners of the browser's files: root, and
/// with the privilege split the engines admin user.
fn sandbox_fix(state: &AppState, root: &std::path::Path) -> Value {
    use crate::browser::apparmor;
    use crate::engines;
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    let split = !state.cfg.engines.helper.is_empty();
    let mut trusted = vec![0];
    if split
        && let Ok(m) = std::fs::metadata(root)
        && m.uid() != me
    {
        trusted.push(m.uid());
    }
    // Iris's own browser from before the fixed path: its directory changes
    // with every update, so it is moved (updated), never approved in place.
    let app = engines::browser_app_dir(root).canonicalize().ok();
    let downloads = engines::install_dir(root, "browser").canonicalize().ok();
    let old_layout = |p: &std::path::Path| {
        downloads.as_ref().is_some_and(|d| p.starts_with(d))
            && !app.as_ref().is_some_and(|a| p.starts_with(a))
    };
    let managed = engines::is_installed(root, "browser");
    let managed_moved = engines::launcher(root, "browser")
        .canonicalize()
        .ok()
        .is_some_and(|p| !old_layout(&p));
    let user = apparmor::current_user()
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "IRIS_USER".into());
    // The setup reserves this account for engine installs and refuses to
    // run for it.
    let account_conflict =
        user == apparmor::ENGINE_ADMIN || state.cfg.agent.user == apparmor::ENGINE_ADMIN;
    let setup = apparmor::setup_command(
        &user,
        apparmor::root_owned_iris().as_deref(),
        split.then_some(root),
    );
    let mut fix = apparmor::sandbox_fix(
        apparmor::userns_restricted(),
        state.browser.executable().as_deref(),
        &|p| !old_layout(p) && apparmor::protected(p, &trusted),
        &setup,
    );
    if fix["kind"] == "setup" {
        // `browser.command` always wins, so it has to go first.
        fix["configured_command"] = (!state.cfg.browser.command.trim().is_empty()).into();
        // With the privilege split in place, installing (or moving) Iris's
        // own browser into the protected engines directory is the step the
        // app can offer.
        fix["install_browser"] = (split && (!managed || !managed_moved)).into();
        fix["account_conflict"] = account_conflict.into();
    }
    fix
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
    let _switch = state.browser.switch_lock.lock().await;
    state
        .db
        .call(|c| crate::db::delete_setting(c, "browser.disconnected"))
        .await?;
    state.browser.set_disconnected(false);
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

#[derive(Deserialize)]
struct SandboxBody {
    allow_without: bool,
}

/// Where Chrome's sandbox cannot start (for example Ubuntu's AppArmor
/// restriction on user namespaces), the owner may choose to run the browser
/// without it. Off by default; saved; `false` takes it back.
async fn sandbox(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<SandboxBody>,
) -> ApiResult {
    user.require_owner()?;
    let allow = body.allow_without;
    // Saved and live choice change together, in request order.
    let _switch = state.browser.switch_lock.lock().await;
    state
        .db
        .call(move |c| {
            if allow {
                crate::db::set_setting(c, ALLOW_NO_SANDBOX, "true")
            } else {
                crate::db::delete_setting(c, ALLOW_NO_SANDBOX)
            }
        })
        .await?;
    crate::info!(
        "owner {} running the browser without Chrome's sandbox",
        if allow { "allowed" } else { "disallowed" }
    );
    // Taking it back also closes an unsandboxed browser (even one starting).
    state.browser.set_allow_no_sandbox(allow).await;
    broadcast(&state);
    ok(json!({"browser": status_json(&state)}))
}

/// Settings key for the owner's sandbox choice.
pub const ALLOW_NO_SANDBOX: &str = "browser.allow_no_sandbox";
