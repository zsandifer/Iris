//! Settings, harness status, models, connectors, credentials, push, jobs,
//! and the voice placeholder.

use super::chat::{file_response, store_upload};
use super::{
    ApiError, ApiResult, AuthUser, JsonBody, RESERVED_CONNECTOR_NAMES, ok, seal_connector_secrets,
};
use crate::db;
use crate::entities::{self, ConnectorConfig, check_text};
use crate::events::Audience;
use crate::harness::valid_server_name;
use crate::ranking::{Pin, Slot};
use crate::state::{AppState, slot_pins_json};
use crate::util::{new_id, now_ms};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings", get(get_settings).put(put_settings))
        .route(
            "/settings/avatar",
            axum::routing::put(put_avatar).delete(delete_avatar),
        )
        .route("/avatar", get(get_avatar))
        .route("/harnesses", get(list_harnesses))
        .route("/harnesses/refresh", post(refresh_harnesses))
        .route(
            "/harnesses/{name}/login",
            get(get_login).post(start_login).delete(cancel_login),
        )
        .route("/harnesses/{name}/login/code", post(login_code))
        .route("/engines", get(list_engines))
        .route("/engines/{name}", axum::routing::delete(uninstall_engine))
        .route("/engines/{name}/install", post(install_engine))
        .route("/engines/{name}/update", post(update_engine))
        .route("/models", get(list_models))
        .route("/models/slots", get(list_slots))
        .route("/models/slots/{slot}", axum::routing::put(put_slot))
        .route("/connectors", get(list_connectors).post(create_connector))
        .route(
            "/connectors/{id}",
            axum::routing::patch(update_connector).delete(delete_connector),
        )
        .route("/connectors/{id}/connect", post(connect))
        .route("/connectors/{id}/disconnect", post(disconnect))
        .route(
            "/credentials",
            get(list_credentials).post(create_credential),
        )
        .route(
            "/credentials/{id}",
            axum::routing::patch(update_credential).delete(delete_credential),
        )
        .route("/push/vapid-public-key", get(vapid_key))
        .route("/push/subscriptions", get(list_push).post(subscribe_push))
        .route(
            "/push/subscriptions/{id}",
            axum::routing::delete(unsubscribe_push),
        )
        .route("/push/test", post(test_push))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{name}/run", post(run_job))
        .route("/voice/speak", post(voice_speak))
}

// ----------------------------------------------------------------- settings

async fn get_settings(State(state): State<AppState>, _user: AuthUser) -> ApiResult {
    ok(state.settings_json().await)
}

#[derive(Deserialize)]
struct SettingsBody {
    agent_name: Option<String>,
    feed_prompt: Option<String>,
    allow_claude_for_shared: Option<bool>,
}

async fn put_settings(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<SettingsBody>,
) -> ApiResult {
    user.require_owner()?;
    let mut writes: Vec<(&'static str, Option<String>)> = Vec::new();
    if let Some(name) = &body.agent_name {
        check_text("agent_name", name, 40, true)?;
        if name.contains(['\n', '\r', '<', '>', '`']) {
            return Err(ApiError::bad_request(
                "agent_name contains invalid characters",
            ));
        }
        writes.push(("agent_name", Some(name.trim().to_string())));
    }
    if let Some(allow) = body.allow_claude_for_shared {
        writes.push(("allow_claude_for_shared", allow.then(|| "true".to_string())));
    }
    if let Some(p) = &body.feed_prompt {
        check_text("feed_prompt", p, 4000, false)?;
        writes.push((
            "feed_prompt",
            (!p.trim().is_empty()).then(|| p.trim().to_string()),
        ));
    }
    state
        .db
        .call(move |c| {
            for (k, v) in &writes {
                match v {
                    Some(v) => db::set_setting(c, k, v)?,
                    None => db::delete_setting(c, k)?,
                }
            }
            Ok(())
        })
        .await?;
    let settings = state.settings_json().await;
    state.hub.send(
        Audience::All,
        json!({"type": "settings.changed", "settings": settings}),
    );
    ok(settings)
}

async fn put_avatar(State(state): State<AppState>, user: AuthUser, body: Bytes) -> ApiResult {
    user.require_owner()?;
    let att = store_upload(&state, user.id(), &body).await?;
    let id = att.id.clone();
    state
        .db
        .call(move |c| {
            db::set_setting(c, "agent_avatar", &id)?;
            db::set_setting(c, "agent_avatar_updated", &now_ms().to_string())
        })
        .await?;
    let settings = state.settings_json().await;
    state.hub.send(
        Audience::All,
        json!({"type": "settings.changed", "settings": settings}),
    );
    ok(settings)
}

async fn delete_avatar(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    state
        .db
        .call(|c| db::delete_setting(c, "agent_avatar"))
        .await?;
    let settings = state.settings_json().await;
    state.hub.send(
        Audience::All,
        json!({"type": "settings.changed", "settings": settings}),
    );
    ok(settings)
}

const DEFAULT_AVATAR: &str = include_str!("avatar.svg");

async fn get_avatar(State(state): State<AppState>, _user: AuthUser) -> Result<Response, ApiError> {
    if let Some(id) = state.setting("agent_avatar").await {
        let att = state.db.call(move |c| db::get_attachment(c, &id)).await?;
        if let Some(att) = att
            && let Ok(bytes) = tokio::fs::read(state.paths.uploads_dir().join(&att.file_name)).await
        {
            return Ok(file_response(&att.mime, bytes));
        }
    }
    Ok(file_response(
        "image/svg+xml",
        DEFAULT_AVATAR.as_bytes().to_vec(),
    ))
}

// ---------------------------------------------------------- harness/models

/// Harness status as visible to `user`: server paths, sign-in sessions,
/// and sign-in hints only for the owner.
pub fn harnesses_for(state: &AppState, user: &AuthUser) -> Vec<Value> {
    state
        .harnesses
        .values()
        .map(|e| {
            let mut st = e.status();
            if !user.user.is_owner {
                st.path = None;
            }
            let mut v = serde_json::to_value(&st).unwrap_or(Value::Null);
            let caps = e.harness.capabilities();
            v["capabilities"] = serde_json::to_value(caps).unwrap_or(Value::Null);
            v["label"] = crate::login::engine_label(e.harness.name()).into();
            if user.user.is_owner {
                v["login_flow"] = match e.harness.login().map(|l| l.flow) {
                    Some(f) => serde_json::to_value(f).unwrap_or(Value::Null),
                    None => "manual".into(),
                };
                v["login_hint"] = e.harness.manual_login_hint().into();
                v["key_options"] = serde_json::to_value(crate::keys::options(e.harness.name()))
                    .unwrap_or_default();
                v["login"] = serde_json::to_value(state.logins.status(e.harness.name()))
                    .unwrap_or(Value::Null);
            }
            v
        })
        .collect()
}

// ------------------------------------------------------------------ engines

async fn list_engines(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    ok(json!({
        "engines": crate::setup::engines_json(&state),
        "dir": crate::setup::engines_root(&state),
        "privileged_helper": !state.cfg.engines.helper.is_empty(),
        "auto_update": state.cfg.engines.auto_update,
    }))
}

async fn engine_op(state: &AppState, name: &str, op: &'static str) -> ApiResult {
    match crate::setup::start_op(state, name, op) {
        Ok(started) => ok(json!({
            "started": started,
            "engines": crate::setup::engines_json(state),
        })),
        Err(e) if e.to_string().starts_with("unknown engine") => Err(ApiError::not_found("engine")),
        Err(e) => Err(ApiError::bad_request(e.to_string())),
    }
}

async fn install_engine(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    engine_op(&state, &name, "install").await
}

async fn update_engine(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    engine_op(&state, &name, "update").await
}

async fn uninstall_engine(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    engine_op(&state, &name, "uninstall").await
}

fn known_harness(state: &AppState, name: &str) -> Result<(), ApiError> {
    state
        .harness(name)
        .map(|_| ())
        .ok_or_else(|| ApiError::not_found("engine"))
}

async fn get_login(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    known_harness(&state, &name)?;
    ok(json!({"login": state.logins.status(&name)}))
}

/// Start the CLI's own login flow (or report the manual step).
async fn start_login(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    known_harness(&state, &name)?;
    let entry = state.harness(&name).expect("known");
    if entry.harness.login().is_none() {
        // No headless login: re-check (the owner may have signed in on the
        // server) and return the manual step.
        let auth = state.recheck_auth(&name).await;
        return ok(json!({
            "login": null,
            "manual": entry.harness.manual_login_hint(),
            "auth": auth,
        }));
    }
    let status = crate::login::start(&state, &name)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    ok(json!({"login": status}))
}

#[derive(Deserialize)]
struct LoginCodeBody {
    code: String,
}

/// Hand the code the user copied to the waiting login process. Never
/// stored or logged.
async fn login_code(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
    JsonBody(body): JsonBody<LoginCodeBody>,
) -> ApiResult {
    user.require_owner()?;
    known_harness(&state, &name)?;
    let status = crate::login::submit_code(&state, &name, &body.code)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    ok(json!({"login": status}))
}

async fn cancel_login(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    known_harness(&state, &name)?;
    ok(json!({"cancelled": crate::login::cancel(&state, &name).await}))
}

/// Harness list plus, for the owner, which API keys are saved (names
/// only).
async fn harnesses_with_keys(state: &AppState, user: &AuthUser) -> Vec<Value> {
    let mut list = harnesses_for(state, user);
    if user.user.is_owner {
        for h in &mut list {
            let name = h["name"].as_str().unwrap_or("").to_string();
            let saved = state
                .db
                .call(move |c| entities::engine_keys(c, &name))
                .await
                .unwrap_or_default();
            h["keys"] = saved.into_iter().map(|(env, _, _)| env).collect();
        }
    }
    list
}

async fn list_harnesses(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    ok(json!({"harnesses": harnesses_with_keys(&state, &user).await}))
}

async fn refresh_harnesses(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    state.recheck_harnesses().await;
    ok(json!({"harnesses": harnesses_with_keys(&state, &user).await}))
}

/// Advanced "AI engine" view: every model the CLIs offer, plus slots.
async fn list_models(State(state): State<AppState>, _user: AuthUser) -> ApiResult {
    let catalog = state.catalog();
    let models: Vec<Value> = catalog
        .models
        .iter()
        .map(|m| {
            let mut v = serde_json::to_value(m).unwrap_or(Value::Null);
            v["available"] = state.model_available(m).into();
            v
        })
        .collect();
    ok(json!({
        "models": models,
        "slots": state.slots_json().await,
        "allow_claude_for_shared": state.allow_claude_for_shared().await,
    }))
}

async fn list_slots(State(state): State<AppState>, _user: AuthUser) -> ApiResult {
    ok(json!({"slots": state.slots_json().await}))
}

#[derive(Deserialize)]
struct SlotBody {
    mode: String,
    model: Option<String>,
    effort: Option<String>,
}

/// Pin a slot to a model (and optional effort), or return it to automatic.
async fn put_slot(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slot): Path<String>,
    JsonBody(body): JsonBody<SlotBody>,
) -> ApiResult {
    user.require_owner()?;
    let slot = Slot::parse(&slot).ok_or_else(|| ApiError::not_found("slot"))?;
    let mut pins = state.slot_pins().await;
    match body.mode.as_str() {
        "automatic" => {
            pins.remove(&slot);
        }
        "pinned" => {
            let model = body
                .model
                .clone()
                .ok_or_else(|| ApiError::bad_request("model is required when pinned"))?;
            let catalog = state.catalog();
            let Some(entry) = catalog.get(&model) else {
                return Err(ApiError::bad_request(format!("unknown model {model}")));
            };
            if !state.model_available(entry) {
                return Err(ApiError::bad_request(format!(
                    "model {model} is not available on this server right now"
                )));
            }
            catalog.check_effort(&model, body.effort.as_deref())?;
            pins.insert(
                slot,
                Pin {
                    model,
                    effort: body.effort.clone(),
                },
            );
        }
        _ => return Err(ApiError::bad_request("mode must be automatic or pinned")),
    }
    let raw = slot_pins_json(&pins);
    state
        .db
        .call(move |c| db::set_setting(c, "slots", &raw))
        .await?;
    let slots = state.slots_json().await;
    state.hub.send(
        Audience::All,
        json!({"type": "models.changed", "slots": slots}),
    );
    ok(json!({"slots": slots}))
}

// --------------------------------------------------------------- connectors

#[derive(Deserialize)]
struct ConnectorBody {
    name: Option<String>,
    description: Option<String>,
    transport: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    url: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    enabled: Option<bool>,
    /// http connectors: `"oauth"` signs in with the MCP authorization spec
    /// (`POST /api/connectors/{id}/oauth/start`).
    auth: Option<String>,
}

fn check_keys(field: &str, m: &BTreeMap<String, String>) -> Result<(), ApiError> {
    for k in m.keys() {
        if k.is_empty()
            || k.len() > 100
            || !k
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ApiError::bad_request(format!("invalid {field} name '{k}'")));
        }
    }
    Ok(())
}

fn build_config(
    state: &AppState,
    id: &str,
    transport: &str,
    body: &ConnectorBody,
    base: Option<&ConnectorConfig>,
) -> Result<ConnectorConfig, ApiError> {
    let mut cfg = base.cloned().unwrap_or_default();
    if let Some(c) = &body.command {
        check_text("command", c, 500, false)?;
        cfg.command = Some(c.clone()).filter(|c| !c.trim().is_empty());
    }
    if let Some(a) = &body.args {
        cfg.args = a.clone();
    }
    if let Some(u) = &body.url {
        entities::check_url("url", u)?;
        cfg.url = Some(u.clone()).filter(|u| !u.is_empty());
    }
    if let Some(env) = &body.env {
        check_keys("env", env)?;
        cfg.env = seal_connector_secrets(state, id, "env", env)?;
    }
    if let Some(h) = &body.headers {
        check_keys("header", h)?;
        cfg.headers = seal_connector_secrets(state, id, "headers", h)?;
    }
    match transport {
        "stdio" if cfg.command.is_none() => {
            Err(ApiError::bad_request("stdio connectors need a command"))
        }
        "http" if cfg.url.is_none() => Err(ApiError::bad_request("http connectors need a url")),
        "stdio" | "http" => Ok(cfg),
        _ => Err(ApiError::bad_request("transport must be stdio or http")),
    }
}

async fn list_connectors(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    let list = state
        .db
        .call(|c| entities::list_connectors(c, false))
        .await?;
    let items: Vec<Value> = list.iter().map(|c| c.public_json()).collect();
    ok(json!({"connectors": items}))
}

async fn create_connector(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<ConnectorBody>,
) -> ApiResult {
    user.require_owner()?;
    let name = body.name.clone().unwrap_or_default();
    if !valid_server_name(&name) || RESERVED_CONNECTOR_NAMES.contains(&name.as_str()) {
        return Err(ApiError::bad_request(
            "name must be 1-48 characters of A-Z, a-z, 0-9, _ or - (and not 'iris')",
        ));
    }
    let description = body.description.clone().unwrap_or_default();
    check_text("description", &description, 500, false)?;
    let transport = body.transport.clone().unwrap_or_else(|| "stdio".into());
    let id = new_id();
    let mut cfg = build_config(&state, &id, &transport, &body, None)?;
    let oauth = match body.auth.as_deref() {
        None | Some("") | Some("none") | Some("secrets") => false,
        Some("oauth") if transport == "http" => true,
        Some("oauth") => {
            return Err(ApiError::bad_request(
                "sign-in (OAuth) is for connections with a web address",
            ));
        }
        Some(_) => return Err(ApiError::bad_request("auth must be oauth or none")),
    };
    if oauth {
        cfg.oauth = Some(entities::OAuthConfig {
            mode: "discover".into(),
            ..Default::default()
        });
    }
    let enabled = body.enabled.unwrap_or(true);
    let res = state
        .db
        .call(move |c| {
            let con =
                entities::insert_connector(c, &id, &name, &description, &transport, &cfg, enabled)?;
            if oauth {
                entities::set_connector_state(
                    c,
                    &con.id,
                    &entities::ConnectorState {
                        auth_state: Some("needs_sign_in"),
                        ..Default::default()
                    },
                )?;
            }
            entities::get_connector(c, &con.id).map(|c| c.expect("inserted"))
        })
        .await;
    match res {
        Ok(con) => {
            crate::connectors::broadcast_changed(&state).await;
            ok(json!({"connector": con.public_json()}))
        }
        Err(e) if e.to_string().contains("UNIQUE") => Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            "a connector with that name already exists",
        )),
        Err(e) => Err(e.into()),
    }
}

async fn update_connector(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ConnectorBody>,
) -> ApiResult {
    user.require_owner()?;
    let cid = id.clone();
    let existing = state
        .db
        .call(move |c| entities::get_connector(c, &cid))
        .await?
        .ok_or_else(|| ApiError::not_found("connector"))?;
    if let Some(d) = &body.description {
        check_text("description", d, 500, false)?;
    }
    let touches_config = body.command.is_some()
        || body.args.is_some()
        || body.env.is_some()
        || body.url.is_some()
        || body.headers.is_some();
    let cfg = if touches_config {
        Some(build_config(
            &state,
            &id,
            &existing.transport,
            &body,
            Some(&existing.config),
        )?)
    } else {
        None
    };
    let (desc, enabled) = (body.description.clone(), body.enabled);
    let cid = id.clone();
    let con = state
        .db
        .call(move |c| entities::update_connector(c, &id, desc.as_deref(), cfg.as_ref(), enabled))
        .await?
        .ok_or_else(|| ApiError::not_found("connector"))?;
    if enabled == Some(false) {
        crate::gateway::cancel_approvals(&state, Some(&cid));
        crate::oauth::forget_flows(&state, Some(&cid));
    }
    if touches_config || enabled == Some(false) {
        crate::gateway::stop(&state, &cid).await;
    }
    if touches_config {
        crate::login::on_connector_updated(&state, &cid).await;
    }
    crate::connectors::broadcast_changed(&state).await;
    ok(json!({"connector": con.public_json()}))
}

async fn set_enabled(state: &AppState, id: String, enabled: bool) -> ApiResult {
    let cid = id.clone();
    let con = state
        .db
        .call(move |c| entities::update_connector(c, &id, None, None, Some(enabled)))
        .await?
        .ok_or_else(|| ApiError::not_found("connector"))?;
    if !enabled {
        crate::gateway::cancel_approvals(state, Some(&cid));
        crate::oauth::forget_flows(state, Some(&cid));
        crate::gateway::stop(state, &cid).await;
    }
    crate::connectors::broadcast_changed(state).await;
    ok(json!({"connector": con.public_json()}))
}

async fn connect(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    set_enabled(&state, id, true).await
}

async fn disconnect(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    set_enabled(&state, id, false).await
}

async fn delete_connector(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    let cid = id.clone();
    if !state
        .db
        .call(move |c| entities::delete_connector(c, &id))
        .await?
    {
        return Err(ApiError::not_found("connector"));
    }
    crate::gateway::cancel_approvals(&state, Some(&cid));
    crate::oauth::forget_flows(&state, Some(&cid));
    crate::gateway::stop(&state, &cid).await;
    let _ = std::fs::remove_dir_all(crate::gateway::connector_dir(&state, &cid));
    crate::connectors::broadcast_changed(&state).await;
    ok(json!({"ok": true}))
}

// -------------------------------------------------------------- credentials

#[derive(Deserialize, Default)]
struct CredentialBody {
    kind: Option<String>,
    label: Option<String>,
    // login
    site: Option<String>,
    username: Option<String>,
    password: Option<String>,
    notes: Option<String>,
    // card
    cardholder: Option<String>,
    number: Option<String>,
    exp_month: Option<u8>,
    exp_year: Option<u16>,
    cvc: Option<String>,
    brand: Option<String>,
}

fn card_brand(number: &str) -> &'static str {
    match number.as_bytes().first() {
        Some(b'4') => "visa",
        Some(b'5') | Some(b'2') => "mastercard",
        Some(b'3') => "amex",
        Some(b'6') => "discover",
        _ => "card",
    }
}

/// Luhn checksum for card numbers.
pub fn luhn_ok(digits: &str) -> bool {
    if digits.len() < 12 || digits.len() > 19 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let sum: u32 = digits
        .bytes()
        .rev()
        .enumerate()
        .map(|(i, b)| {
            let d = (b - b'0') as u32;
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// Secret fields supplied in a request (only those present).
type SecretFields = serde_json::Map<String, Value>;

/// Split a request into (metadata, supplied secret fields). Metadata is
/// display-only; secret fields are encrypted and never returned.
fn credential_parts(kind: &str, b: &CredentialBody) -> Result<(Value, SecretFields), ApiError> {
    match kind {
        "login" => {
            check_text("site", b.site.as_deref().unwrap_or(""), 300, false)?;
            check_text("username", b.username.as_deref().unwrap_or(""), 300, false)?;
            check_text("password", b.password.as_deref().unwrap_or(""), 1000, false)?;
            check_text("notes", b.notes.as_deref().unwrap_or(""), 4000, false)?;
            let meta = json!({"site": b.site, "username": b.username});
            let mut secret = SecretFields::new();
            if let Some(p) = &b.password {
                secret.insert("password".into(), p.clone().into());
            }
            if let Some(n) = &b.notes {
                secret.insert("notes".into(), n.clone().into());
            }
            Ok((meta, secret))
        }
        "card" => {
            let number: Option<String> = b
                .number
                .as_ref()
                .map(|n| n.chars().filter(|c| c.is_ascii_digit()).collect());
            if let Some(n) = &number
                && !luhn_ok(n)
            {
                return Err(ApiError::bad_request("card number is not valid"));
            }
            if b.exp_month.is_some_and(|m| !(1..=12).contains(&m)) {
                return Err(ApiError::bad_request("exp_month must be 1-12"));
            }
            if b.cvc.as_ref().is_some_and(|c| {
                !(3..=4).contains(&c.len()) || !c.bytes().all(|x| x.is_ascii_digit())
            }) {
                return Err(ApiError::bad_request("cvc must be 3-4 digits"));
            }
            check_text(
                "cardholder",
                b.cardholder.as_deref().unwrap_or(""),
                200,
                false,
            )?;
            let last4 = number.as_ref().map(|n| n[n.len() - 4..].to_string());
            let brand = b
                .brand
                .clone()
                .or_else(|| number.as_deref().map(|n| card_brand(n).to_string()));
            let meta = json!({
                "brand": brand, "last4": last4, "exp_month": b.exp_month,
                "exp_year": b.exp_year, "cardholder": b.cardholder,
            });
            let mut secret = SecretFields::new();
            if let Some(n) = number {
                secret.insert("number".into(), n.into());
            }
            if let Some(c) = &b.cvc {
                secret.insert("cvc".into(), c.clone().into());
            }
            Ok((meta, secret))
        }
        _ => Err(ApiError::bad_request("kind must be login or card")),
    }
}

async fn list_credentials(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    let uid = user.id().to_string();
    let list = state
        .db
        .call(move |c| entities::list_credentials(c, &uid))
        .await?;
    ok(json!({"credentials": list, "injection": "not_implemented"}))
}

async fn create_credential(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<CredentialBody>,
) -> ApiResult {
    user.require_owner()?;
    let kind = body.kind.clone().unwrap_or_default();
    let label = body.label.clone().unwrap_or_default();
    check_text("label", &label, 200, true)?;
    let (meta, secret) = credential_parts(&kind, &body)?;
    let has_secret = if kind == "card" {
        secret.contains_key("number")
    } else {
        secret.contains_key("password") || secret.contains_key("notes")
    };
    if !has_secret {
        return Err(ApiError::bad_request(if kind == "card" {
            "number is required"
        } else {
            "password or notes is required"
        }));
    }
    let id = new_id();
    let ct = state
        .vault
        .encrypt(Value::Object(secret).to_string().as_bytes(), &id)?;
    let uid = user.id().to_string();
    let cred = state
        .db
        .call(move |c| entities::insert_credential(c, &id, &uid, &kind, label.trim(), &meta, &ct))
        .await?;
    ok(json!({"credential": cred}))
}

async fn update_credential(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<CredentialBody>,
) -> ApiResult {
    user.require_owner()?;
    if let Some(l) = &body.label {
        check_text("label", l, 200, true)?;
    }
    let (uid, st) = (user.id().to_string(), state.clone());
    // Read, merge, and write in one database call (one connection, one
    // lock), so concurrent partial updates cannot overwrite each other.
    let cred = state
        .db
        .call(move |c| {
            let Some(existing) = entities::get_credential(c, &uid, &id)? else {
                return Ok(Err(ApiError::not_found("credential")));
            };
            let (mut meta, secret) = match credential_parts(&existing.kind, &body) {
                Ok(parts) => parts,
                Err(e) => return Ok(Err(e)),
            };
            // Keep metadata fields that were not supplied.
            if let (Some(new), Some(old)) = (meta.as_object_mut(), existing.metadata.as_object()) {
                for (k, v) in old {
                    if new.get(k).is_none_or(Value::is_null) {
                        new.insert(k.clone(), v.clone());
                    }
                }
            }
            // Merge supplied secret fields into the stored secret so a
            // partial update (e.g. only notes) keeps the rest. Decrypted
            // only in memory.
            let ct = if secret.is_empty() {
                None
            } else {
                let Some(stored) = entities::get_credential_ciphertext(c, &uid, &id)? else {
                    return Ok(Err(ApiError::not_found("credential")));
                };
                let sealed = st.vault.decrypt(&stored, &id).and_then(|plain| {
                    let mut merged: SecretFields =
                        serde_json::from_slice(&plain).unwrap_or_default();
                    merged.extend(secret);
                    st.vault
                        .encrypt(Value::Object(merged).to_string().as_bytes(), &id)
                });
                match sealed {
                    Ok(ct) => Some(ct),
                    Err(e) => return Ok(Err(e.into())),
                }
            };
            Ok(entities::update_credential(
                c,
                &uid,
                &id,
                body.label.as_deref(),
                Some(&meta),
                ct.as_deref(),
            )?
            .ok_or_else(|| ApiError::not_found("credential")))
        })
        .await??;
    ok(json!({"credential": cred}))
}

async fn delete_credential(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    let uid = user.id().to_string();
    if !state
        .db
        .call(move |c| entities::delete_credential(c, &uid, &id))
        .await?
    {
        return Err(ApiError::not_found("credential"));
    }
    ok(json!({"ok": true}))
}

// --------------------------------------------------------------------- push

async fn vapid_key(State(state): State<AppState>, _user: AuthUser) -> ApiResult {
    ok(json!({
        "enabled": state.cfg.push.enabled,
        "public_key": state.vapid.as_ref().map(|k| k.public_key_b64.clone()),
    }))
}

async fn list_push(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    let uid = user.id().to_string();
    let subs = state
        .db
        .call(move |c| entities::list_push_subscriptions(c, &uid))
        .await?;
    ok(json!({"subscriptions": subs}))
}

#[derive(Deserialize)]
struct PushKeys {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
struct SubscribeBody {
    endpoint: String,
    keys: PushKeys,
}

async fn subscribe_push(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<SubscribeBody>,
) -> ApiResult {
    if !body.endpoint.starts_with("https://") || body.endpoint.len() > 2000 {
        return Err(ApiError::bad_request("endpoint must be an https URL"));
    }
    // Validate the keys now rather than failing at send time.
    crate::push::encrypt(b"{}", &body.keys.p256dh, &body.keys.auth)
        .map_err(|e| ApiError::bad_request(format!("invalid subscription keys: {e}")))?;
    let (uid, did) = (user.id().to_string(), user.device.id.clone());
    let sub = state
        .db
        .call(move |c| {
            entities::upsert_push_subscription(
                c,
                &uid,
                &did,
                &body.endpoint,
                &body.keys.p256dh,
                &body.keys.auth,
            )
        })
        .await?;
    ok(json!({"subscription": sub}))
}

async fn unsubscribe_push(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let uid = user.id().to_string();
    if !state
        .db
        .call(move |c| entities::delete_push_subscription(c, &uid, &id))
        .await?
    {
        return Err(ApiError::not_found("subscription"));
    }
    ok(json!({"ok": true}))
}

async fn test_push(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    let name = state.agent_name().await;
    let (delivered, failed) = crate::notify::push_users_now(
        &state,
        &[user.id().to_string()],
        &json!({"title": name, "body": "Notifications are working.", "kind": "test"}),
    )
    .await;
    ok(json!({"delivered": delivered, "failed": failed}))
}

// --------------------------------------------------------------------- jobs

async fn list_jobs(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    ok(json!({"jobs": crate::jobs::status(&state).await}))
}

async fn run_job(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    user.require_owner()?;
    let job = crate::jobs::Job::parse(&name).ok_or_else(|| ApiError::not_found("job"))?;
    if state.is_paused().await {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "paused",
            "Iris is paused; resume it first",
        ));
    }
    let started = crate::jobs::trigger(&state, job);
    Ok((
        StatusCode::ACCEPTED,
        axum::Json(json!({"started": started})),
    )
        .into_response())
}

// -------------------------------------------------------------------- voice

/// TODO(voice): text-to-speech for agent replies. Planned shape:
/// `POST /api/voice/speak {"message_id"}` -> `audio/mpeg`, using a local
/// TTS engine so audio never leaves the machine. Not implemented yet.
async fn voice_speak(_user: AuthUser) -> Result<Response, ApiError> {
    Err(ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "not_implemented",
        "voice replies are not implemented yet",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn_and_brand() {
        assert!(luhn_ok("4242424242424242"));
        assert!(!luhn_ok("4242424242424241"));
        assert!(!luhn_ok("12"));
        assert_eq!(card_brand("4242"), "visa");
        assert_eq!(card_brand("5555"), "mastercard");
    }

    #[test]
    fn credential_split_keeps_secrets_out_of_metadata() {
        let b = CredentialBody {
            site: Some("bank.example".into()),
            username: Some("someone".into()),
            password: Some("hunter2".into()),
            ..Default::default()
        };
        let (meta, secret) = credential_parts("login", &b).unwrap();
        assert!(!meta.to_string().contains("hunter2"));
        assert_eq!(secret["password"], "hunter2");
        assert!(
            !secret.contains_key("notes"),
            "absent fields are not cleared"
        );
        let c = CredentialBody {
            number: Some("4242 4242 4242 4242".into()),
            exp_month: Some(12),
            exp_year: Some(2030),
            cvc: Some("123".into()),
            ..Default::default()
        };
        let (meta, secret) = credential_parts("card", &c).unwrap();
        assert_eq!(meta["last4"], "4242");
        assert_eq!(meta["brand"], "visa");
        assert!(!meta.to_string().contains("123"));
        assert_eq!(secret["number"], "4242424242424242");
        let cvc_only = CredentialBody {
            cvc: Some("999".into()),
            ..Default::default()
        };
        let (_, s) = credential_parts("card", &cvc_only).unwrap();
        assert_eq!(Value::Object(s), json!({"cvc": "999"}));
        let bad = CredentialBody {
            number: Some("1234".into()),
            ..Default::default()
        };
        assert!(credential_parts("card", &bad).is_err());
    }
}
