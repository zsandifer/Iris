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
    let _creds = crate::oauth::credentials_lock(&state).await;
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
    drop(_creds);
    crate::gateway::cancel_approvals(&state, Some(&updated.id));
    crate::gateway::stop(&state, &updated.id).await;
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
            "approval_timeout_secs": state.cfg.approval_timeout_secs(),
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
        crate::gateway::cancel_approvals(state, None);
        // Stop connector programs too, so nothing already sent to them
        // keeps running.
        crate::gateway::stop_all(state).await;
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

/// Save an engine API key and check it with one tiny real request (a
/// refused key is not kept; `auth` says what happened).
async fn put_key(
    State(state): State<AppState>,
    user: AuthUser,
    Path((name, env)): Path<(String, String)>,
    JsonBody(body): JsonBody<KeyBody>,
) -> ApiResult {
    user.require_owner()?;
    known_engine(&state, &name)?;
    let value = crate::keys::check(&name, &env, &body.value).map_err(ApiError::bad_request)?;
    // Check the candidate before it replaces anything; engines turned off
    // in config are never run, not even to check.
    let auth = if state.harness(&name).is_some_and(|e| e.cfg.enabled) {
        crate::keys::verify(&state, &name, &env, &value).await
    } else {
        crate::harness::AuthState::Unknown
    };
    // A refused key is not kept (a working saved key stays as it was).
    if auth != crate::harness::AuthState::AuthRequired {
        crate::keys::set(&state, &name, &env, &value)
            .await
            .map_err(ApiError::bad_request)?;
        if auth == crate::harness::AuthState::Ok {
            state.note_auth(&name, auth, None).await;
        }
        state.probe_auth(&name).await;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    /// A fake remote MCP service with its own OAuth authorization server
    /// (metadata, dynamic registration, PKCE code exchange, refresh).
    #[derive(Default)]
    struct Provider {
        base: String,
        /// Issue a long-lived token, then refuse it on tool calls (a token
        /// revoked early): Iris must refresh and retry.
        strict: bool,
        /// How the token endpoint answers refreshes: "" (works), "refuse"
        /// (invalid_grant), or "down" (503).
        refresh_mode: &'static str,
        /// Refuse the first token on every request, initialize included.
        refuse_at_start: bool,
        challenge: Option<String>,
        refreshed: bool,
        last_auth: Option<String>,
        token_requests: Vec<std::collections::HashMap<String, String>>,
    }

    async fn fake_provider() -> Arc<Mutex<Provider>> {
        use axum::extract::State as S;
        use axum::http::HeaderMap;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let p = Arc::new(Mutex::new(Provider {
            base: base.clone(),
            ..Default::default()
        }));
        type P = Arc<Mutex<Provider>>;
        async fn mcp(S(p): S<P>, headers: HeaderMap, body: String) -> Response {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let base = p.lock().unwrap().base.clone();
            p.lock().unwrap().last_auth = auth.clone();
            let strict = p.lock().unwrap().strict;
            let refuse_all = p.lock().unwrap().refuse_at_start;
            let revoked = (refuse_all || (strict && body.contains("tools/call")))
                && auth.as_deref() == Some("Bearer at-1");
            if revoked || !matches!(auth.as_deref(), Some("Bearer at-1" | "Bearer at-2")) {
                let mut r = (StatusCode::UNAUTHORIZED, "").into_response();
                r.headers_mut().insert(
                    "www-authenticate",
                    HeaderValue::from_str(&format!(
                        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""
                    ))
                    .unwrap(),
                );
                return r;
            }
            let req: Value = serde_json::from_str(&body).unwrap();
            let Some(id) = req.get("id").cloned() else {
                return StatusCode::ACCEPTED.into_response();
            };
            let result = match req["method"].as_str().unwrap() {
                "initialize" => {
                    json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                                       "serverInfo": {"name": "fake-mail", "version": "1"}})
                }
                "tools/list" => {
                    json!({"tools": [{"name": "list_inbox", "inputSchema": {"type": "object"}}]})
                }
                _ => json!({"content": [{"type": "text", "text": "3 unread"}]}),
            };
            axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
        }
        let app = Router::new()
            .route("/mcp", axum::routing::post(mcp))
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(|S(p): S<P>| async move {
                    let b = p.lock().unwrap().base.clone();
                    axum::Json(json!({"resource": format!("{b}/mcp"),
                        "authorization_servers": [format!("{b}/auth")],
                        "scopes_supported": ["mail.read"]}))
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server/auth",
                get(|S(p): S<P>| async move {
                    let b = p.lock().unwrap().base.clone();
                    axum::Json(json!({"issuer": format!("{b}/auth"),
                        "authorization_endpoint": format!("{b}/auth/authorize"),
                        "token_endpoint": format!("{b}/auth/token"),
                        "registration_endpoint": format!("{b}/auth/register"),
                        "code_challenge_methods_supported": ["S256"]}))
                }),
            )
            .route(
                "/auth/register",
                post(|| async { axum::Json(json!({"client_id": "dyn-client"})) }),
            )
            .route(
                "/auth/token",
                post(|S(p): S<P>, body: String| async move {
                    let form = crate::oauth::parse_query(&body);
                    let mut g = p.lock().unwrap();
                    g.token_requests.push(form.clone());
                    match form.get("grant_type").map(String::as_str) {
                        Some("authorization_code") => {
                            let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                            let ok = form.get("code").map(String::as_str) == Some("the-code")
                                && g.challenge.as_deref()
                                    == Some(crate::oauth::pkce_challenge(&verifier).as_str());
                            if ok {
                                let life = if g.strict { 3600 } else { 1 };
                                axum::Json(json!({"access_token": "at-1", "refresh_token": "rt-1",
                                                  "expires_in": life, "token_type": "Bearer"}))
                                .into_response()
                            } else {
                                (
                                    StatusCode::BAD_REQUEST,
                                    axum::Json(json!({"error": "invalid_grant"})),
                                )
                                    .into_response()
                            }
                        }
                        Some("refresh_token") if g.refresh_mode == "down" => {
                            (StatusCode::SERVICE_UNAVAILABLE, "busy").into_response()
                        }
                        Some("refresh_token")
                            if form.get("refresh_token").map(String::as_str) == Some("rt-1")
                                && g.refresh_mode != "refuse" =>
                        {
                            g.refreshed = true;
                            axum::Json(json!({"access_token": "at-2", "expires_in": 3600}))
                                .into_response()
                        }
                        _ => (
                            StatusCode::BAD_REQUEST,
                            axum::Json(json!({"error": "invalid_grant"})),
                        )
                            .into_response(),
                    }
                }),
            )
            .with_state(p.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        p
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let mut req = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
        let req = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                req.body(Body::from(b.to_string()))
            }
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_connector_signs_in_with_discovery_dcr_pkce_and_refresh() {
        oauth_flow(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_token_refused_early_is_refreshed_and_the_call_retried() {
        oauth_flow(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_refresh_asks_to_sign_in_again() {
        oauth_flow_with(false, "refuse").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreachable_provider_fails_only_the_call() {
        oauth_flow_with(false, "down").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_token_refused_at_startup_is_refreshed() {
        oauth_flow_with(true, "start").await;
    }

    async fn oauth_flow(strict: bool) {
        oauth_flow_with(strict, "").await;
    }

    async fn oauth_flow_with(strict: bool, refresh_mode: &'static str) {
        let provider = fake_provider().await;
        provider.lock().unwrap().strict = strict;
        provider.lock().unwrap().refresh_mode = if refresh_mode == "start" {
            ""
        } else {
            refresh_mode
        };
        provider.lock().unwrap().refuse_at_start = refresh_mode == "start";
        let base = provider.lock().unwrap().base.clone();
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
        cfg.push.enabled = false;
        let state = AppState::new(cfg, paths, db).unwrap();
        let app = super::super::router(state.clone());
        let owner = state.db.with(crate::db::owner_user_id).unwrap().unwrap();
        let code = state
            .db
            .with(|c| Ok(crate::auth::create_pairing_code(c, &owner)))
            .unwrap()
            .unwrap()
            .0;
        let res = app
            .clone()
            .oneshot(
                HttpRequest::post("/api/pair")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"code": code}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let token = serde_json::from_slice::<Value>(&bytes).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();

        let (s, b) = send(
            &app,
            "POST",
            "/api/connectors",
            &token,
            Some(json!({"name": "mail", "transport": "http", "url": format!("{base}/mcp"), "auth": "oauth"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let con: Value = serde_json::from_str(&b).unwrap();
        let id = con["connector"]["id"].as_str().unwrap().to_string();
        assert_eq!(con["connector"]["auth_state"], "needs_sign_in");

        let (s, b) = send(
            &app,
            "POST",
            &format!("/api/connectors/{id}/oauth/start"),
            &token,
            Some(json!({"origin": "http://127.0.0.1:7999"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let started: Value = serde_json::from_str(&b).unwrap();
        assert_eq!(started["paste"], false);
        assert_eq!(
            started["redirect_uri"],
            "http://127.0.0.1:7999/api/oauth/callback"
        );
        let url = started["authorize_url"].as_str().unwrap();
        assert!(url.starts_with(&format!("{base}/auth/authorize?")), "{url}");
        let q = crate::oauth::parse_query(url);
        assert_eq!(q["client_id"], "dyn-client");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["resource"], format!("{base}/mcp"));
        assert_eq!(q["scope"], "mail.read");
        provider.lock().unwrap().challenge = Some(q["code_challenge"].clone());

        // A forged state does nothing.
        let (s, b) = send(
            &app,
            "GET",
            "/api/oauth/callback?state=forged&code=the-code",
            "",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(b.contains("didn"), "{b}");
        // The provider sends the phone's browser back to Iris.
        let (s, b) = send(
            &app,
            "GET",
            &format!("/api/oauth/callback?state={}&code=the-code", q["state"]),
            "",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(b.contains("is connected"), "{b}");
        // Used once.
        let (_, b) = send(
            &app,
            "GET",
            &format!("/api/oauth/callback?state={}&code=the-code", q["state"]),
            "",
            None,
        )
        .await;
        assert!(b.contains("expired or was already used"));
        {
            let p = provider.lock().unwrap();
            let exchange = &p.token_requests[0];
            assert_eq!(exchange["resource"], format!("{base}/mcp"));
            assert_eq!(
                exchange["redirect_uri"],
                "http://127.0.0.1:7999/api/oauth/callback"
            );
        }
        let (_, b) = send(&app, "GET", "/api/connectors", &token, None).await;
        let list: Value = serde_json::from_str(&b).unwrap();
        let c = &list["connectors"][0];
        assert_eq!(c["auth_state"], "ok");
        assert_eq!(c["oauth"]["connected"], true);
        assert!(
            !b.contains("at-1") && !b.contains("rt-1"),
            "tokens never leave the vault"
        );

        // A turn uses it through the gateway; the short-lived token is
        // refreshed first and sent as a bearer header.
        let main = state
            .db
            .with(|c| crate::db::ensure_main_conversation(c, &owner))
            .unwrap();
        let (servers, _guard) = crate::gateway::grant_for_turn(&state, &owner, &main, "main", "m1")
            .await
            .unwrap();
        let crate::harness::McpTransport::Stdio { env, .. } = &servers[0].transport;
        let r = crate::gateway::handle(
            &state,
            &json!({"token": env[crate::gateway::TOKEN_ENV], "connector": "mail", "request": {
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "list_inbox", "arguments": {}}}}),
        )
        .await;
        let text = r["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let reconnect_cards = || {
            state
                .db
                .with(|c| crate::db::list_messages(c, &main, None, 50))
                .unwrap()
                .into_iter()
                .filter(|m| {
                    m.card
                        .as_ref()
                        .is_some_and(|c| c["type"] == "connector_reconnect")
                })
                .count()
        };
        let auth_state = || {
            state
                .db
                .with(|c| crate::entities::get_connector(c, &id))
                .unwrap()
                .unwrap()
                .auth_state
        };
        if refresh_mode == "refuse" {
            assert!(text.contains("refused the sign-in"), "{r}");
            assert_eq!(auth_state(), "needs_reconnect");
            assert_eq!(reconnect_cards(), 1);
            return;
        }
        if refresh_mode == "down" {
            assert!(text.contains("could not be reached"), "{r}");
            assert_eq!(auth_state(), "ok", "a provider outage is not a sign-out");
            assert_eq!(reconnect_cards(), 0);
            return;
        }
        assert_eq!(text, "3 unread", "{r}");
        {
            let p = provider.lock().unwrap();
            assert!(p.refreshed);
            assert_eq!(p.last_auth.as_deref(), Some("Bearer at-2"));
        }
        // Recovered without asking the owner to sign in again.
        let (_, b) = send(&app, "GET", "/api/connectors", &token, None).await;
        let list: Value = serde_json::from_str(&b).unwrap();
        assert_eq!(list["connectors"][0]["auth_state"], "ok");
        let reconnect = state
            .db
            .with(|c| crate::db::list_messages(c, &main, None, 50))
            .unwrap()
            .into_iter()
            .any(|m| m.card.is_some_and(|c| c["type"] == "connector_reconnect"));
        assert!(!reconnect);
    }

    struct App {
        state: AppState,
        app: Router,
        token: String,
        _tmp: tempfile::TempDir,
    }

    async fn app() -> App {
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
        cfg.push.enabled = false;
        cfg.bind = "100.64.0.1:7420".into();
        let state = AppState::new(cfg, paths, db).unwrap();
        let app = super::super::router(state.clone());
        let owner = state.db.with(crate::db::owner_user_id).unwrap().unwrap();
        let code = state
            .db
            .with(|c| Ok(crate::auth::create_pairing_code(c, &owner)))
            .unwrap()
            .unwrap()
            .0;
        let (_, b) = send(&app, "POST", "/api/pair", "", Some(json!({"code": code}))).await;
        let token = serde_json::from_str::<Value>(&b).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        App {
            state,
            app,
            token,
            _tmp: tmp,
        }
    }

    async fn call(a: &App, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (s, b) = send(&a.app, method, uri, &a.token, body).await;
        (s, serde_json::from_str(&b).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn pause_trusted_people_and_disconnect_everything() {
        let a = app().await;
        let (s, v) = call(&a, "PUT", "/api/safety", Some(json!({"paused": true}))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["paused"], true);
        let (_, settings) = call(&a, "GET", "/api/settings", None).await;
        assert_eq!(settings["paused"], true);
        let (s, _) = call(&a, "POST", "/api/jobs/heartbeat/run", None).await;
        assert_eq!(s, StatusCode::CONFLICT);
        // A message while paused gets a plain "I'm paused" reply, no turn.
        let (_, main) = call(&a, "GET", "/api/conversations/main", None).await;
        let cid = main["conversation"]["id"].as_str().unwrap().to_string();
        let (s, sent) = call(
            &a,
            "POST",
            &format!("/api/conversations/{cid}/messages"),
            Some(json!({"text": "hi"})),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let mid = sent["assistant_message"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut reply = Value::Null;
        for _ in 0..100 {
            let m = a
                .state
                .db
                .with(|c| crate::db::get_message(c, &mid))
                .unwrap()
                .unwrap();
            if m.status != "streaming" {
                reply = serde_json::to_value(m).unwrap();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(reply["meta"]["error"]["kind"], "paused", "{reply}");
        assert!(reply["content"].as_str().unwrap().contains("paused"));
        let (_, v) = call(&a, "PUT", "/api/safety", Some(json!({"paused": false}))).await;
        assert_eq!(v["paused"], false);

        let (s, v) = call(
            &a,
            "PUT",
            "/api/safety",
            Some(json!({"trusted_recipients": ["Sam <SAM@Example.com>", "+1 (555) 010-2000", "sam@example.com"]})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            v["trusted_recipients"],
            json!(["sam@example.com", "+15550102000"])
        );

        for name in ["one", "two"] {
            let (s, _) = call(
                &a,
                "POST",
                "/api/connectors",
                Some(json!({"name": name, "command": "/bin/true"})),
            )
            .await;
            assert_eq!(s, StatusCode::OK);
        }
        let (s, v) = call(&a, "POST", "/api/safety/disconnect-all", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["disconnected"], 2);
        assert!(
            v["connectors"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["enabled"] == false)
        );
    }

    #[tokio::test]
    async fn google_and_github_setup_from_the_catalog() {
        let a = app().await;
        let (_, cat) = call(
            &a,
            "GET",
            "/api/connectors/catalog?origin=https%3A%2F%2Firis.example.ts.net",
            None,
        )
        .await;
        let ids: Vec<&str> = cat["services"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["google", "github", "custom"]);
        assert_eq!(
            cat["services"][0]["redirect_uri"],
            "https://iris.example.ts.net/api/oauth/callback"
        );
        assert_eq!(cat["services"][0]["redirect_direct"], true);

        let (s, _) = call(
            &a,
            "POST",
            "/api/connectors/catalog/google",
            Some(json!({"client_id": "not-a-google-id", "client_secret": "x"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, v) = call(
            &a,
            "POST",
            "/api/connectors/catalog/google",
            Some(json!({"client_id": "123-abc.apps.googleusercontent.com", "client_secret": "GOCSPX-test-secret"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["needs_sign_in"], true);
        let con = &v["connector"];
        assert_eq!(con["service"], "google");
        assert_eq!(con["auth_state"], "needs_sign_in");
        assert_eq!(con["command"], "uvx");
        assert!(
            !v.to_string().contains("GOCSPX-test-secret"),
            "secrets stay hidden"
        );
        let id = con["id"].as_str().unwrap().to_string();

        let (s, v) = call(
            &a,
            "POST",
            &format!("/api/connectors/{id}/oauth/start"),
            Some(json!({"origin": "https://iris.example.ts.net"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let url = v["authorize_url"].as_str().unwrap();
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        let q = crate::oauth::parse_query(url);
        assert_eq!(q["client_id"], "123-abc.apps.googleusercontent.com");
        assert_eq!(q["access_type"], "offline");
        assert_eq!(
            q["redirect_uri"],
            "https://iris.example.ts.net/api/oauth/callback"
        );
        assert!(q["scope"].contains("gmail.modify"));
        assert!(!q.contains_key("resource"));
        assert_eq!(v["paste"], false);
        // Not on https: the owner pastes the address they land on.
        let (_, v) = call(
            &a,
            "POST",
            &format!("/api/connectors/{id}/oauth/start"),
            Some(json!({"origin": "http://100.64.0.1:7420"})),
        )
        .await;
        assert_eq!(v["paste"], true);
        assert_eq!(
            v["redirect_uri"],
            "http://127.0.0.1:7420/api/oauth/callback"
        );
        let (s, v) = call(
            &a,
            "POST",
            "/api/oauth/complete",
            Some(json!({"url": "http://127.0.0.1:7420/api/oauth/callback?state=unknown&code=x"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"].as_str().unwrap().contains("expired"));

        let (s, v) = call(
            &a,
            "POST",
            "/api/connectors/catalog/github",
            Some(json!({"token": "github_pat_example"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["needs_sign_in"], false);
        assert_eq!(v["connector"]["header_keys"], json!(["Authorization"]));
        assert_eq!(v["connector"]["auth_state"], "ok");
        assert!(!v.to_string().contains("github_pat_example"));
    }

    #[tokio::test]
    async fn rules_and_engine_keys() {
        let a = app().await;
        let (_, v) = call(
            &a,
            "POST",
            "/api/connectors",
            Some(json!({"name": "mail", "command": "/bin/true"})),
        )
        .await;
        let id = v["connector"]["id"].as_str().unwrap().to_string();
        let (s, v) = call(
            &a,
            "PUT",
            &format!("/api/connectors/{id}/policy"),
            Some(json!({"mode": "read_only", "tools": {"send": "ask", "list": "allow"}})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["connector"]["policy"]["mode"], "read_only");
        assert_eq!(
            v["connector"]["rules"],
            "Only reads · never changes anything"
        );
        let (_, v) = call(
            &a,
            "PUT",
            &format!("/api/connectors/{id}/policy"),
            Some(json!({"tools": {"send": null}})),
        )
        .await;
        assert_eq!(v["connector"]["policy"]["tools"], json!({"list": "allow"}));
        let (s, _) = call(
            &a,
            "PUT",
            &format!("/api/connectors/{id}/policy"),
            Some(json!({"mode": "yolo"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        let (s, _) = call(
            &a,
            "PUT",
            "/api/harnesses/gemini/keys/OPENAI_API_KEY",
            Some(json!({"value": "x"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, v) = call(
            &a,
            "PUT",
            "/api/harnesses/gemini/keys/GEMINI_API_KEY",
            Some(json!({"value": "AIza-test-key"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["keys"][0]["env"], "GEMINI_API_KEY");
        assert_eq!(v["auth"], "unknown", "disabled engines are not run");
        let (_, h) = call(&a, "GET", "/api/harnesses", None).await;
        let gemini = h["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "gemini")
            .unwrap()
            .clone();
        assert_eq!(gemini["keys"], json!(["GEMINI_API_KEY"]));
        assert!(!h.to_string().contains("AIza-test-key"));
        assert_eq!(
            crate::keys::env_for(&a.state, "gemini").await,
            [("GEMINI_API_KEY".to_string(), "AIza-test-key".to_string())]
        );
        let (_, v) = call(
            &a,
            "DELETE",
            "/api/harnesses/gemini/keys/GEMINI_API_KEY",
            None,
        )
        .await;
        assert_eq!(v["removed"], true);
        assert!(crate::keys::env_for(&a.state, "gemini").await.is_empty());
    }

    #[tokio::test]
    async fn engine_keys_are_checked_before_they_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        // A stand-in Gemini CLI that accepts only "good-key".
        let cli = tmp.path().join("fake-gemini");
        let seen = tmp.path().join("seen");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 0.0.0; exit 0; fi\ncat >/dev/null\n\
                 echo \"$GEMINI_API_KEY\" >> {}\n\
                 if [ \"$GEMINI_API_KEY\" = good-key ]; then echo '{{\"type\":\"result\",\"status\":\"success\"}}'; \
                 else echo '{{\"type\":\"result\",\"status\":\"error\",\"error\":{{\"message\":\"API key not valid. Please pass a valid API key.\"}}}}'; exit 1; fi\n",
                seen.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = Config::default();
        for (name, h) in cfg.harness.iter_mut() {
            h.enabled = name == "gemini";
        }
        cfg.harness.get_mut("gemini").unwrap().command = cli.to_string_lossy().into_owned();
        cfg.push.enabled = false;
        let state = AppState::new(cfg, paths, db).unwrap();
        state.refresh_harnesses().await;
        let a = App {
            app: super::super::router(state.clone()),
            token: {
                let owner = state.db.with(crate::db::owner_user_id).unwrap().unwrap();
                let code = state
                    .db
                    .with(|c| Ok(crate::auth::create_pairing_code(c, &owner)))
                    .unwrap()
                    .unwrap()
                    .0;
                let app = super::super::router(state.clone());
                let (_, b) = send(&app, "POST", "/api/pair", "", Some(json!({"code": code}))).await;
                serde_json::from_str::<Value>(&b).unwrap()["token"]
                    .as_str()
                    .unwrap()
                    .to_string()
            },
            state,
            _tmp: tmp,
        };
        let put = |v: &str| json!({"value": v});
        let (_, v) = call(
            &a,
            "PUT",
            "/api/harnesses/gemini/keys/GEMINI_API_KEY",
            Some(put("bad-key")),
        )
        .await;
        assert_eq!(v["auth"], "auth_required", "{v}");
        assert_eq!(v["keys"], json!([]), "a refused key is not kept");
        let (_, v) = call(
            &a,
            "PUT",
            "/api/harnesses/gemini/keys/GEMINI_API_KEY",
            Some(put("good-key")),
        )
        .await;
        assert_eq!(v["auth"], "ok", "{v}");
        assert_eq!(v["keys"][0]["env"], "GEMINI_API_KEY");
        assert_eq!(
            a.state.harness("gemini").unwrap().status().auth,
            crate::harness::AuthState::Ok
        );
        // A bad replacement leaves the working key in place.
        let (_, v) = call(
            &a,
            "PUT",
            "/api/harnesses/gemini/keys/GEMINI_API_KEY",
            Some(put("typo-key")),
        )
        .await;
        assert_eq!(v["auth"], "auth_required");
        assert_eq!(
            crate::keys::env_for(&a.state, "gemini").await,
            [("GEMINI_API_KEY".to_string(), "good-key".to_string())]
        );
        let seen = std::fs::read_to_string(a._tmp.path().join("seen")).unwrap();
        assert_eq!(
            seen.lines().collect::<Vec<_>>(),
            ["bad-key", "good-key", "typo-key"]
        );
    }
}
