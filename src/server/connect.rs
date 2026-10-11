//! Connections and safety: the connector catalog and setup, connector
//! sign-in (OAuth) with Iris's callback, approval rules, the connector
//! activity log, Pause Iris, Disconnect everything, trusted recipients, and
//! engine API keys.

use super::{ApiError, ApiResult, AuthUser, JsonBody, ok};
use crate::entities::{self, ConnectorState};
use crate::gateway::policy::{Mode, Policy, ToolRule};
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connectors/catalog", get(catalog))
        .route("/connectors/catalog/{service}", post(setup_service))
        .route("/connectors/activity", get(activity))
        .route("/connectors/{id}/oauth/start", post(oauth_start))
        .route("/connectors/{id}/oauth/client", post(oauth_client))
        .route("/connectors/{id}/resume", post(resume))
        .route("/connectors/{id}/policy", axum::routing::put(put_policy))
        .route("/connectors/{id}/tools", get(tools))
        .route("/oauth/complete", post(oauth_complete))
        .route("/oauth/callback", get(oauth_callback))
        .route("/safety", get(get_safety).put(put_safety))
        .route("/safety/disconnect-all", post(disconnect_all))
        .route("/harnesses/{name}/keys", get(list_keys))
        .route(
            "/harnesses/{name}/keys/{env}",
            axum::routing::put(put_key).delete(delete_key),
        )
}

async fn connector(state: &AppState, id: &str) -> Result<entities::Connector, ApiError> {
    let id = id.to_string();
    state
        .db
        .call(move |c| entities::get_connector(c, &id))
        .await?
        .ok_or_else(|| ApiError::not_found("connector"))
}

// ----------------------------------------------------------------- catalog

#[derive(Deserialize)]
struct OriginQuery {
    origin: Option<String>,
}

async fn catalog(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<OriginQuery>,
) -> ApiResult {
    user.require_owner()?;
    ok(json!({
        "services": crate::connectors::catalog_json(&state, q.origin.as_deref()).await,
        "modes": Mode::ALL.iter().map(|m| json!({"mode": m, "label": m.describe()})).collect::<Vec<_>>(),
    }))
}

async fn setup_service(
    State(state): State<AppState>,
    user: AuthUser,
    Path(service): Path<String>,
    JsonBody(body): JsonBody<crate::connectors::SetupBody>,
) -> ApiResult {
    user.require_owner()?;
    let (id, needs_sign_in) = crate::connectors::setup(&state, &service, &body)
        .await
        .map_err(ApiError::bad_request)?;
    let con = connector(&state, &id).await?;
    ok(json!({"connector": con.public_json(), "needs_sign_in": needs_sign_in}))
}

// ------------------------------------------------------------------- oauth

#[derive(Deserialize, Default)]
struct StartBody {
    origin: Option<String>,
}

async fn oauth_start(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<StartBody>,
) -> ApiResult {
    user.require_owner()?;
    let origin = body.origin;
    match crate::oauth::start(&state, &id, origin.as_deref()).await {
        Ok(s) => ok(json!(s)),
        Err(crate::oauth::StartError::NeedsClient { redirect_uri }) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "needs_client",
            format!(
                "This service doesn't let apps register themselves. Create an OAuth app on the \
                 service with the callback address {redirect_uri}, then paste its Client ID \
                 (and secret) here."
            ),
        )),
        Err(crate::oauth::StartError::Other(e)) => Err(ApiError::bad_request(e)),
    }
}

#[derive(Deserialize)]
struct ClientBody {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

/// The owner's own OAuth client for a connector whose service has no
/// dynamic registration.
async fn oauth_client(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ClientBody>,
) -> ApiResult {
    user.require_owner()?;
    let con = connector(&state, &id).await?;
    let client_id = body.client_id.trim().to_string();
    if client_id.is_empty() || client_id.len() > 500 {
        return Err(ApiError::bad_request("paste the Client ID"));
    }
    let mut o = con
        .config
        .oauth
        .clone()
        .unwrap_or_else(|| entities::OAuthConfig {
            mode: "discover".into(),
            ..Default::default()
        });
    o.client_id = Some(client_id);
    o.client_secret = match body.client_secret.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => Some(
            crate::oauth::seal_client_secret(&state, &con.id, s).map_err(ApiError::bad_request)?,
        ),
        _ => None,
    };
    o.client_source = "owner".into();
    o.access_token = None;
    o.refresh_token = None;
    o.expires_at = None;
    let mut cfg = con.config.clone();
    cfg.oauth = Some(o);
    let cid = con.id.clone();
    let updated = state
        .db
        .call(move |c| entities::update_connector(c, &cid, None, Some(&cfg), None))
        .await?
        .ok_or_else(|| ApiError::not_found("connector"))?;
    ok(json!({"connector": updated.public_json()}))
}

#[derive(Deserialize)]
struct CompleteBody {
    url: String,
}

/// Finish a sign-in with the address the owner landed on (when the
/// provider could not send the browser straight back to Iris).
async fn oauth_complete(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<CompleteBody>,
) -> ApiResult {
    user.require_owner()?;
    if body.url.len() > 8000 {
        return Err(ApiError::bad_request("that address is too long"));
    }
    let label = crate::oauth::finish_from_url(&state, &body.url)
        .await
        .map_err(ApiError::bad_request)?;
    ok(json!({"connected": label}))
}

#[derive(Deserialize)]
struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Where the provider sends the browser after the owner approves. No
/// device token (it is a plain navigation); the one-time `state` ties it to
/// the sign-in the owner started from the app.
async fn oauth_callback(State(state): State<AppState>, Query(q): Query<CallbackQuery>) -> Response {
    let result = match q.state.as_deref() {
        Some(s) => crate::oauth::finish(&state, s, q.code.as_deref(), q.error.as_deref()).await,
        None => Err("This page is only used while connecting a service.".to_string()),
    };
    let (title, body) = match result {
        Ok(label) => (
            format!("{} is connected", escape_html(&label)),
            "You can close this page and go back to Iris.".to_string(),
        ),
        Err(e) => ("That didn't work".to_string(), escape_html(&e)),
    };
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>body{{font:17px system-ui,sans-serif;background:#fbf8f3;\
         color:#1d1a24;display:grid;place-items:center;min-height:90vh;margin:0;padding:24px;\
         text-align:center}}@media(prefers-color-scheme:dark){{body{{background:#120f18;\
         color:#eee}}}}a{{color:#7c5cf6}}</style></head><body><main><h1>{title}</h1>\
         <p>{body}</p><p><a href=\"/\">Open Iris</a></p></main></body></html>"
    );
    let mut res = Html(page).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

// ------------------------------------------------------------- connectors

async fn resume(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    let cid = id.clone();
    let con = state
        .db
        .call(move |c| {
            entities::update_connector(c, &cid, None, None, Some(true))?;
            entities::set_connector_state(
                c,
                &cid,
                &ConnectorState {
                    paused: Some(None),
                    ..Default::default()
                },
            )
        })
        .await?
        .ok_or_else(|| ApiError::not_found("connector"))?;
    crate::info!("connector {}: resumed by the owner", con.name);
    crate::connectors::broadcast_changed(&state).await;
    ok(json!({"connector": con.public_json()}))
}

#[derive(Deserialize)]
struct PolicyBody {
    mode: Option<Mode>,
    /// Tool name -> "allow" | "ask" | "block" | null (back to the rules).
    tools: Option<BTreeMap<String, Option<ToolRule>>>,
}

async fn put_policy(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<PolicyBody>,
) -> ApiResult {
    user.require_owner()?;
    let con = connector(&state, &id).await?;
    let mut p: Policy = con.policy.clone();
    if let Some(m) = body.mode {
        p.mode = m;
    }
    for (tool, rule) in body.tools.unwrap_or_default() {
        if tool.is_empty() || tool.len() > 200 {
            return Err(ApiError::bad_request("invalid tool name"));
        }
        match rule {
            Some(r) => {
                p.tools.insert(tool, r);
            }
            None => {
                p.tools.remove(&tool);
            }
        }
    }
    if p.tools.len() > 500 {
        return Err(ApiError::bad_request("too many tool rules"));
    }
    let cid = con.id.clone();
    let updated = state
        .db
        .call(move |c| {
            entities::set_connector_state(
                c,
                &cid,
                &ConnectorState {
                    policy: Some(&p),
                    ..Default::default()
                },
            )
        })
        .await?
        .ok_or_else(|| ApiError::not_found("connector"))?;
    crate::connectors::broadcast_changed(&state).await;
    ok(json!({"connector": updated.public_json()}))
}

/// The connector's tools with what the rules do with each (starts the
/// connector if needed).
async fn tools(State(state): State<AppState>, user: AuthUser, Path(id): Path<String>) -> ApiResult {
    user.require_owner()?;
    let con = connector(&state, &id).await?;
    if !con.enabled {
        return ok(json!({"tools": [], "error": "Turn the connection on to see its tools."}));
    }
    match crate::gateway::describe_tools(&state, &con).await {
        Ok(tools) => ok(json!({"tools": tools, "error": null})),
        Err(e) => ok(json!({
            "tools": [],
            "error": e,
            "detail": state.gateway.last_error(&con.id),
        })),
    }
}

#[derive(Deserialize)]
struct ActivityQuery {
    connector_id: Option<String>,
    before: Option<i64>,
    limit: Option<usize>,
}

async fn activity(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ActivityQuery>,
) -> ApiResult {
    user.require_owner()?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let mut items = state
        .db
        .call(move |c| {
            entities::list_connector_calls(c, q.connector_id.as_deref(), q.before, limit + 1)
        })
        .await?;
    let has_more = items.len() > limit;
    items.truncate(limit);
    ok(json!({"items": items, "has_more": has_more}))
}

// ------------------------------------------------------------------ safety

async fn safety_json(state: &AppState) -> Value {
    let s = &state.cfg.safety;
    json!({
        "paused": state.is_paused().await,
        "paused_at": state.setting("paused").await.and_then(|v| v.parse::<i64>().ok()),
        "trusted_recipients": state.trusted_recipients().await,
        "pending_approvals": state.gateway.pending_count(),
        "limits": {
            "approval_timeout_secs": s.approval_timeout_secs,
            "max_sends_per_hour": s.max_sends_per_hour,
            "max_calls_per_hour": s.max_calls_per_hour,
            "max_read_mb_per_hour": s.max_read_mb_per_hour,
        },
    })
}

async fn get_safety(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    ok(safety_json(&state).await)
}

#[derive(Deserialize)]
struct SafetyBody {
    paused: Option<bool>,
    trusted_recipients: Option<Vec<String>>,
}

async fn put_safety(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<SafetyBody>,
) -> ApiResult {
    user.require_owner()?;
    if let Some(list) = &body.trusted_recipients {
        if list.len() > 200 {
            return Err(ApiError::bad_request("at most 200 trusted recipients"));
        }
        let mut clean: Vec<String> = Vec::new();
        for r in list {
            let n = crate::gateway::policy::normalize_recipient(r)
                .ok_or_else(|| ApiError::bad_request(format!("'{r}' is not an address")))?;
            if !clean.contains(&n) {
                clean.push(n);
            }
        }
        let raw = serde_json::to_string(&clean).unwrap_or_else(|_| "[]".into());
        state
            .db
            .call(move |c| crate::db::set_setting(c, "safety.trusted_recipients", &raw))
            .await?;
    }
    if let Some(paused) = body.paused {
        set_paused(&state, paused).await?;
    }
    ok(safety_json(&state).await)
}

/// Pause or resume Iris. Pausing cancels running turns (their waiting
/// approvals with them) and stops new turns, jobs, and connector calls.
pub async fn set_paused(state: &AppState, paused: bool) -> Result<(), ApiError> {
    let was = state.is_paused().await;
    state
        .db
        .call(move |c| {
            if paused {
                crate::db::set_setting(c, "paused", &crate::util::now_ms().to_string())
            } else {
                crate::db::delete_setting(c, "paused")
            }
        })
        .await?;
    if paused {
        let running: Vec<String> = state
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        for conv in &running {
            crate::turn::cancel(state, conv);
        }
        crate::gateway::cancel_all_grants(state);
    }
    if was != paused {
        crate::warn!(
            "Iris {} by the owner",
            if paused { "paused" } else { "resumed" }
        );
    }
    let settings = state.settings_json().await;
    state.hub.send(
        crate::events::Audience::All,
        json!({"type": "settings.changed", "settings": settings}),
    );
    Ok(())
}

async fn disconnect_all(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    let n = crate::gateway::disconnect_all(&state).await;
    crate::connectors::broadcast_changed(&state).await;
    let list = state
        .db
        .call(|c| entities::list_connectors(c, false))
        .await?;
    ok(json!({
        "disconnected": n,
        "connectors": list.iter().map(|c| c.public_json()).collect::<Vec<_>>(),
    }))
}

// -------------------------------------------------------------- engine keys

fn known_engine(state: &AppState, name: &str) -> Result<(), ApiError> {
    state
        .harness(name)
        .map(|_| ())
        .ok_or_else(|| ApiError::not_found("engine"))
}

async fn list_keys(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    known_engine(&state, &name)?;
    ok(crate::keys::list_json(&state, &name).await)
}

#[derive(Deserialize)]
struct KeyBody {
    value: String,
}

/// Save an engine API key and check it with one tiny real request.
async fn put_key(
    State(state): State<AppState>,
    user: AuthUser,
    Path((name, env)): Path<(String, String)>,
    JsonBody(body): JsonBody<KeyBody>,
) -> ApiResult {
    user.require_owner()?;
    known_engine(&state, &name)?;
    crate::keys::set(&state, &name, &env, &body.value)
        .await
        .map_err(ApiError::bad_request)?;
    let auth = crate::keys::verify(&state, &name).await;
    let mut v = crate::keys::list_json(&state, &name).await;
    v["auth"] = serde_json::to_value(auth).unwrap_or(Value::Null);
    ok(v)
}

async fn delete_key(
    State(state): State<AppState>,
    user: AuthUser,
    Path((name, env)): Path<(String, String)>,
) -> ApiResult {
    user.require_owner()?;
    known_engine(&state, &name)?;
    let removed = crate::keys::delete(&state, &name, &env).await;
    state.recheck_harnesses().await;
    let mut v = crate::keys::list_json(&state, &name).await;
    v["removed"] = removed.into();
    ok(v)
}
