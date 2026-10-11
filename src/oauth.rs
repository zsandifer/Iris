//! Connector sign-in with OAuth 2.1 (authorization code + PKCE), completed
//! from the phone through Iris's callback URL.
//!
//! Two kinds of connectors use it:
//! - `discover`: remote MCP servers following the MCP authorization spec.
//!   Iris finds the authorization server from the server's protected
//!   resource metadata (RFC 9728) and the server's metadata (RFC 8414 /
//!   OpenID discovery), registers itself with dynamic client registration
//!   (RFC 7591) when offered, or uses a client the owner pasted, and sends
//!   the RFC 8707 `resource` parameter.
//! - `preset`: a catalog service (Google) with fixed endpoints and the
//!   owner's own OAuth client.
//!
//! Tokens are sealed with the vault, refreshed automatically before they
//! expire, and only ever handed to the connector (as a bearer header or the
//! connector program's environment), never to the agent. These are tokens
//! for the owner's connected services, not the CLIs' subscription
//! credentials, which Iris never touches.

use crate::entities::{self, ConnectorState, OAuthConfig};
use crate::state::AppState;
use crate::util::{now_ms, truncate_chars};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// A sign-in started from the app must finish within this time.
const FLOW_TTL_MS: i64 = 15 * 60 * 1000;
/// Refresh tokens this long before they expire.
const REFRESH_EARLY_MS: i64 = 60 * 1000;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum TokenError {
    /// The connector has no usable token; the owner has to sign in.
    NeedsSignIn,
    /// The sign-in service could not be reached to refresh the token.
    Unavailable(String),
}

struct Flow {
    connector_id: String,
    verifier: String,
    redirect_uri: String,
    created_at: i64,
}

/// Sign-ins in progress, by `state` parameter.
#[derive(Default)]
pub struct Flows {
    flows: Mutex<HashMap<String, Flow>>,
    /// Serializes token refreshes.
    refresh: tokio::sync::Mutex<()>,
}

// ------------------------------------------------------------------ helpers

pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `a=1&b=2` (also the query part of a full URL).
pub fn parse_query(s: &str) -> HashMap<String, String> {
    let q = s.split_once('?').map_or(s, |(_, q)| q);
    let q = q.split('#').next().unwrap_or("");
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (pct_decode(k), pct_decode(v)),
            None => (pct_decode(p), String::new()),
        })
        .collect()
}

fn with_query(base: &str, params: &[(&str, &str)]) -> String {
    let q: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(v)))
        .collect();
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}{}", q.join("&"))
}

pub fn pkce_challenge(verifier: &str) -> String {
    B64URL.encode(Sha256::digest(verifier.as_bytes()))
}

fn host_of(url: &str) -> &str {
    let rest = url.split("://").nth(1).unwrap_or("");
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    if host.starts_with('[') {
        host.split(']').next().unwrap_or("").trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or("")
    }
}

pub fn is_loopback_url(url: &str) -> bool {
    let h = host_of(url);
    h == "localhost" || h == "::1" || h.starts_with("127.")
}

/// Endpoints from metadata must be https (or loopback, for testing).
fn check_endpoint(url: &str) -> Result<(), String> {
    if url.starts_with("https://") || (url.starts_with("http://") && is_loopback_url(url)) {
        Ok(())
    } else {
        Err(format!(
            "the sign-in server uses an insecure address ({url})"
        ))
    }
}

fn origin_of(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    format!("{scheme}://{host}")
}

fn path_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let path = rest
        .find('/')
        .map(|i| &rest[i..])
        .unwrap_or("")
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    path.trim_end_matches('/').to_string()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(HTTP_TIMEOUT))
        .user_agent(format!("iris/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn get_json(url: &str) -> Option<Value> {
    let mut resp = agent()
        .get(url)
        .header("Accept", "application/json")
        .call()
        .ok()?;
    if resp.status().as_u16() != 200 {
        return None;
    }
    let text = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .ok()?;
    serde_json::from_str(&text).ok()
}

/// POST a form and parse the JSON reply (status, body).
fn post_form(url: &str, form: &[(&str, &str)]) -> Result<(u16, Value), String> {
    let body: Vec<String> = form
        .iter()
        .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(v)))
        .collect();
    let mut resp = agent()
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body.join("&").as_bytes())
        .map_err(|e| format!("could not reach the sign-in server: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .unwrap_or_default();
    let v = serde_json::from_str(&text).unwrap_or_else(|_| {
        // Some servers answer form-encoded despite Accept.
        let q = parse_query(&text);
        json!(q)
    });
    Ok((status, v))
}

// ------------------------------------------------------------- discovery

/// What the MCP server's metadata says about signing in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Discovered {
    pub issuer: Option<String>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes: Vec<String>,
    pub resource: String,
}

/// `resource_metadata` and `scope` from a `WWW-Authenticate: Bearer` header.
pub fn parse_www_authenticate(h: &str) -> (Option<String>, Option<String>) {
    let find = |key: &str| {
        let i = h.find(&format!("{key}=\""))?;
        let rest = &h[i + key.len() + 2..];
        rest.find('"').map(|j| rest[..j].to_string())
    };
    (find("resource_metadata"), find("scope"))
}

/// Candidate URLs for the authorization server's metadata (MCP spec
/// order).
pub fn as_metadata_urls(issuer: &str) -> Vec<String> {
    let origin = origin_of(issuer);
    let path = path_of(issuer);
    if path.is_empty() {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            format!("{origin}/.well-known/openid-configuration{path}"),
            format!("{origin}{path}/.well-known/openid-configuration"),
        ]
    }
}

/// Run discovery against a remote MCP server (blocking).
pub fn discover(mcp_url: &str) -> Result<Discovered, String> {
    // An unauthenticated request tells us where the metadata lives.
    let probe = agent()
        .post(mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .send(
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
                "protocolVersion": crate::gateway::upstream::PROTOCOL_VERSION,
                "capabilities": {}, "clientInfo": {"name": "iris", "version": env!("CARGO_PKG_VERSION")}}})
            .to_string()
            .as_bytes(),
        )
        .map_err(|e| format!("could not reach {mcp_url}: {e}"))?;
    let www = probe
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let (rm, scope) = parse_www_authenticate(&www);
    let origin = origin_of(mcp_url);
    let path = path_of(mcp_url);
    let mut prm_urls: Vec<String> = rm.into_iter().collect();
    if !path.is_empty() {
        prm_urls.push(format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        ));
    }
    prm_urls.push(format!("{origin}/.well-known/oauth-protected-resource"));
    let prm = prm_urls.iter().find_map(|u| get_json(u));
    let (issuer, resource, supported) = match &prm {
        Some(p) => (
            p["authorization_servers"][0]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| origin.clone()),
            p["resource"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| mcp_url.to_string()),
            p["scopes_supported"].clone(),
        ),
        // Older servers: the MCP server is its own authorization server.
        None => (origin.clone(), mcp_url.to_string(), Value::Null),
    };
    let meta = as_metadata_urls(&issuer).iter().find_map(|u| get_json(u));
    let (auth, token, reg, scopes_meta) = match &meta {
        Some(m) => {
            if let Some(methods) = m["code_challenge_methods_supported"].as_array()
                && !methods.iter().any(|x| x == "S256")
            {
                return Err("the sign-in server does not support PKCE".into());
            }
            (
                m["authorization_endpoint"].as_str().map(String::from),
                m["token_endpoint"].as_str().map(String::from),
                m["registration_endpoint"].as_str().map(String::from),
                m["scopes_supported"].clone(),
            )
        }
        None if prm.is_none() => (
            Some(format!("{origin}/authorize")),
            Some(format!("{origin}/token")),
            Some(format!("{origin}/register")),
            Value::Null,
        ),
        None => return Err("could not find the service's sign-in server".into()),
    };
    let (Some(auth), Some(token)) = (auth, token) else {
        return Err("the sign-in server's details are incomplete".into());
    };
    check_endpoint(&auth)?;
    check_endpoint(&token)?;
    if let Some(r) = &reg {
        check_endpoint(r)?;
    }
    let list = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let scopes = match scope {
        Some(s) => s.split_whitespace().map(String::from).collect(),
        None if !list(&supported).is_empty() => list(&supported),
        None => list(&scopes_meta)
            .into_iter()
            .filter(|s| s != "openid" && !s.contains("offline"))
            .collect::<Vec<_>>(),
    };
    Ok(Discovered {
        issuer: meta
            .as_ref()
            .and_then(|m| m["issuer"].as_str().map(String::from)),
        authorization_endpoint: auth,
        token_endpoint: token,
        registration_endpoint: reg,
        scopes,
        resource: resource.trim_end_matches('/').to_string(),
    })
}

/// Dynamic client registration (blocking). Returns (client_id, secret).
fn register(endpoint: &str, redirect_uri: &str) -> Result<(String, Option<String>), String> {
    let body = json!({
        "client_name": "Iris",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let mut resp = agent()
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body.to_string().as_bytes())
        .map_err(|e| format!("could not register with the sign-in server: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match v["client_id"].as_str() {
        Some(id) if (200..300).contains(&status) => Ok((
            id.to_string(),
            v["client_secret"].as_str().map(String::from),
        )),
        _ => Err(format!(
            "the sign-in server refused to register Iris (HTTP {status})"
        )),
    }
}

// ---------------------------------------------------------------- sealing

fn aad(id: &str, field: &str) -> String {
    format!("connector:{id}:oauth:{field}")
}

fn seal(state: &AppState, id: &str, field: &str, v: &str) -> Result<String, String> {
    state
        .vault
        .seal_str(v, &aad(id, field))
        .map_err(|e| e.to_string())
}

fn open(state: &AppState, id: &str, field: &str, v: &Option<String>) -> Option<String> {
    v.as_ref()
        .and_then(|s| state.vault.open_str(s, &aad(id, field)).ok())
}

async fn save(state: &AppState, id: &str, oauth: OAuthConfig) -> Result<(), String> {
    let id = id.to_string();
    state
        .db
        .call(move |c| {
            let Some(con) = entities::get_connector(c, &id)? else {
                return Ok(());
            };
            let mut cfg = con.config.clone();
            cfg.oauth = Some(oauth);
            entities::update_connector(c, &id, None, Some(&cfg), None)?;
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())
}

/// Store an OAuth client the owner created (Google, GitHub OAuth app...).
pub fn seal_client_secret(state: &AppState, id: &str, secret: &str) -> Result<String, String> {
    seal(state, id, "client_secret", secret)
}

// ------------------------------------------------------------------ tokens

/// A fresh access token for the connector (refreshing it if needed), None
/// for connectors without OAuth.
pub async fn bearer(
    state: &AppState,
    con: &entities::Connector,
) -> Result<Option<String>, TokenError> {
    let Some(o) = &con.config.oauth else {
        return Ok(None);
    };
    if let (Some(_), Some(exp)) = (&o.access_token, o.expires_at)
        && exp > now_ms() + REFRESH_EARLY_MS
    {
        return open(state, &con.id, "access_token", &o.access_token)
            .map(Some)
            .ok_or(TokenError::NeedsSignIn);
    }
    if o.access_token.is_some() && o.expires_at.is_none() && o.refresh_token.is_none() {
        // A token without expiry (some servers): use it until refused.
        return open(state, &con.id, "access_token", &o.access_token)
            .map(Some)
            .ok_or(TokenError::NeedsSignIn);
    }
    let _guard = state.oauth.refresh.lock().await;
    // Another call may have refreshed while we waited.
    let id = con.id.clone();
    let fresh = state
        .db
        .call(move |c| entities::get_connector(c, &id))
        .await
        .ok()
        .flatten()
        .and_then(|c| c.config.oauth);
    let Some(o) = fresh else {
        return Err(TokenError::NeedsSignIn);
    };
    if let (Some(_), Some(exp)) = (&o.access_token, o.expires_at)
        && exp > now_ms() + REFRESH_EARLY_MS
    {
        return open(state, &con.id, "access_token", &o.access_token)
            .map(Some)
            .ok_or(TokenError::NeedsSignIn);
    }
    let refresh =
        open(state, &con.id, "refresh_token", &o.refresh_token).ok_or(TokenError::NeedsSignIn)?;
    let (Some(token_endpoint), Some(client_id)) = (o.token_endpoint.clone(), o.client_id.clone())
    else {
        return Err(TokenError::NeedsSignIn);
    };
    let secret = open(state, &con.id, "client_secret", &o.client_secret);
    let resource = (o.mode == "discover").then(|| o.resource.clone()).flatten();
    let reply = tokio::task::spawn_blocking(move || {
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", client_id.as_str()),
        ];
        if let Some(s) = &secret {
            form.push(("client_secret", s.as_str()));
        }
        if let Some(r) = &resource {
            form.push(("resource", r.as_str()));
        }
        post_form(&token_endpoint, &form)
    })
    .await
    .map_err(|_| TokenError::NeedsSignIn)?;
    match reply {
        Ok((200, v)) if v["access_token"].is_string() => {
            let token = v["access_token"].as_str().unwrap_or_default().to_string();
            let updated =
                apply_tokens(state, &con.id, o, &v).map_err(|_| TokenError::NeedsSignIn)?;
            save(state, &con.id, updated)
                .await
                .map_err(|_| TokenError::NeedsSignIn)?;
            crate::info!("connector {}: token refreshed", con.name);
            Ok(Some(token))
        }
        Ok((status, v)) => {
            crate::warn!(
                "connector {}: token refresh refused (HTTP {status}, {})",
                con.name,
                truncate_chars(v["error"].as_str().unwrap_or("unknown"), 60)
            );
            Err(TokenError::NeedsSignIn)
        }
        Err(e) => {
            // The sign-in server is unreachable: not the owner's problem
            // yet; fail this call without asking them to sign in again.
            crate::warn!("connector {}: token refresh failed: {e}", con.name);
            Err(TokenError::Unavailable(e))
        }
    }
}

/// Merge a token response into the stored config (sealed).
fn apply_tokens(
    state: &AppState,
    id: &str,
    mut o: OAuthConfig,
    v: &Value,
) -> Result<OAuthConfig, String> {
    let access = v["access_token"].as_str().ok_or("no access token")?;
    o.access_token = Some(seal(state, id, "access_token", access)?);
    if let Some(r) = v["refresh_token"].as_str() {
        o.refresh_token = Some(seal(state, id, "refresh_token", r)?);
    }
    o.expires_at = v["expires_in"]
        .as_i64()
        .or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok()))
        .map(|s| now_ms() + s * 1000);
    Ok(o)
}

/// The `email` claim of an ID token received straight from the token
/// endpoint over TLS (display only; not used for any decision).
pub fn id_token_email(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = B64URL.decode(payload.trim_end_matches('=')).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v["email"].as_str().map(|e| truncate_chars(e, 200))
}

/// Force a refresh on next use (the server refused the current token).
pub async fn invalidate(state: &AppState, con: &entities::Connector) {
    if let Some(mut o) = con.config.oauth.clone() {
        o.expires_at = Some(0);
        let _ = save(state, &con.id, o).await;
    }
}

// ------------------------------------------------------------------- flows

/// Where the provider sends the browser back to. Returns (redirect URI,
/// direct): direct when the phone can reach it (https, or this machine);
/// otherwise the owner copies the address they land on back into the app.
pub fn redirect_uri(state: &AppState, origin: Option<&str>) -> (String, bool) {
    let base = if !state.cfg.public_url.trim().is_empty() {
        state
            .cfg
            .public_url
            .trim()
            .trim_end_matches('/')
            .to_string()
    } else {
        origin
            .filter(|o| o.starts_with("https://") || o.starts_with("http://"))
            .map(origin_of)
            .unwrap_or_else(|| state.cfg.base_url())
    };
    if base.starts_with("https://") || is_loopback_url(&base) {
        (format!("{base}/api/oauth/callback"), true)
    } else {
        let port = state
            .cfg
            .bind
            .rsplit_once(':')
            .map(|(_, p)| p.to_string())
            .unwrap_or_else(|| "7420".into());
        (format!("http://127.0.0.1:{port}/api/oauth/callback"), false)
    }
}

/// What the app needs to send the owner to the provider.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Started {
    pub authorize_url: String,
    pub redirect_uri: String,
    /// The owner has to paste the address they end up on.
    pub paste: bool,
}

#[derive(Debug)]
pub enum StartError {
    /// No client: the provider has no dynamic registration, so the owner
    /// pastes the client ID (and secret) of an OAuth app they created.
    NeedsClient {
        redirect_uri: String,
    },
    Other(String),
}

/// Start signing a connector in. Discovers and registers as needed.
pub async fn start(
    state: &AppState,
    connector_id: &str,
    origin: Option<&str>,
) -> Result<Started, StartError> {
    let id = connector_id.to_string();
    let con = state
        .db
        .call(move |c| entities::get_connector(c, &id))
        .await
        .ok()
        .flatten()
        .ok_or_else(|| StartError::Other("connection not found".into()))?;
    let mut o = con.config.oauth.clone().unwrap_or_else(|| OAuthConfig {
        mode: "discover".into(),
        ..Default::default()
    });
    let (redirect, direct) = redirect_uri(state, origin);
    if o.mode == "discover" && (o.authorization_endpoint.is_none() || o.token_endpoint.is_none()) {
        let url = con
            .config
            .url
            .clone()
            .ok_or_else(|| StartError::Other("this connection has no web address".into()))?;
        let d = tokio::task::spawn_blocking(move || discover(&url))
            .await
            .map_err(|e| StartError::Other(e.to_string()))?
            .map_err(StartError::Other)?;
        o.issuer = d.issuer;
        o.authorization_endpoint = Some(d.authorization_endpoint);
        o.token_endpoint = Some(d.token_endpoint);
        o.registration_endpoint = d.registration_endpoint;
        if o.scopes.is_empty() {
            o.scopes = d.scopes;
        }
        o.resource = Some(d.resource);
    }
    let reuse_client = o.client_id.is_some()
        && (o.client_source == "owner" || o.redirect_uri.as_deref() == Some(redirect.as_str()));
    if !reuse_client {
        match o.registration_endpoint.clone() {
            Some(reg) if o.client_source != "owner" => {
                let r = redirect.clone();
                let (cid, secret) = tokio::task::spawn_blocking(move || register(&reg, &r))
                    .await
                    .map_err(|e| StartError::Other(e.to_string()))?
                    .map_err(StartError::Other)?;
                o.client_id = Some(cid);
                o.client_secret = match secret {
                    Some(s) => {
                        Some(seal(state, &con.id, "client_secret", &s).map_err(StartError::Other)?)
                    }
                    None => None,
                };
                o.client_source = "dcr".into();
            }
            _ if o.client_id.is_some() => {}
            _ => {
                save(state, &con.id, o).await.map_err(StartError::Other)?;
                return Err(StartError::NeedsClient {
                    redirect_uri: redirect,
                });
            }
        }
    }
    o.redirect_uri = Some(redirect.clone());
    let auth_endpoint = o
        .authorization_endpoint
        .clone()
        .ok_or_else(|| StartError::Other("no sign-in page known".into()))?;
    let client_id = o.client_id.clone().unwrap_or_default();
    let verifier = B64URL.encode(crate::util::random_bytes::<32>());
    let flow_state = crate::util::hex(&crate::util::random_bytes::<32>());
    let challenge = pkce_challenge(&verifier);
    let scope = o.scopes.join(" ");
    let mut params: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", &redirect),
        ("state", &flow_state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ];
    if !scope.is_empty() {
        params.push(("scope", &scope));
    }
    let resource = o.resource.clone().unwrap_or_default();
    if o.mode == "discover" && !resource.is_empty() {
        params.push(("resource", &resource));
    }
    for (k, v) in &o.extra_params {
        params.push((k, v));
    }
    let authorize_url = with_query(&auth_endpoint, &params);
    save(state, &con.id, o.clone())
        .await
        .map_err(StartError::Other)?;
    {
        let mut flows = state.oauth.flows.lock().unwrap_or_else(|p| p.into_inner());
        let now = now_ms();
        flows.retain(|_, f| now - f.created_at < FLOW_TTL_MS);
        // One sign-in per connector at a time.
        flows.retain(|_, f| f.connector_id != con.id);
        flows.insert(
            flow_state,
            Flow {
                connector_id: con.id.clone(),
                verifier,
                redirect_uri: redirect.clone(),
                created_at: now,
            },
        );
    }
    crate::info!("connector {}: sign-in started", con.name);
    Ok(Started {
        authorize_url,
        redirect_uri: redirect,
        paste: !direct,
    })
}

/// Finish a sign-in with the provider's answer (callback or pasted URL).
/// Returns the connector's display label.
pub async fn finish(
    state: &AppState,
    flow_state: &str,
    code: Option<&str>,
    error: Option<&str>,
) -> Result<String, String> {
    let flow = state
        .oauth
        .flows
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(flow_state)
        .filter(|f| now_ms() - f.created_at < FLOW_TTL_MS)
        .ok_or("This sign-in link expired or was already used. Start again from the app.")?;
    if let Some(e) = error {
        return Err(match e {
            "access_denied" => "You declined, so nothing was connected.".to_string(),
            other => format!(
                "The service reported a problem ({}).",
                truncate_chars(other, 80)
            ),
        });
    }
    let code = code
        .filter(|c| !c.is_empty())
        .ok_or("The service did not send a sign-in code.")?;
    let id = flow.connector_id.clone();
    let con = state
        .db
        .call(move |c| entities::get_connector(c, &id))
        .await
        .ok()
        .flatten()
        .ok_or("That connection was removed.")?;
    let o = con
        .config
        .oauth
        .clone()
        .ok_or("That connection does not use sign-in.")?;
    let token_endpoint = o.token_endpoint.clone().ok_or("No token address known.")?;
    let client_id = o.client_id.clone().unwrap_or_default();
    let secret = open(state, &con.id, "client_secret", &o.client_secret);
    let resource = (o.mode == "discover").then(|| o.resource.clone()).flatten();
    let code = code.to_string();
    let reply = tokio::task::spawn_blocking(move || {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", flow.redirect_uri.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", flow.verifier.as_str()),
        ];
        if let Some(s) = &secret {
            form.push(("client_secret", s.as_str()));
        }
        if let Some(r) = &resource {
            form.push(("resource", r.as_str()));
        }
        post_form(&token_endpoint, &form)
    })
    .await
    .map_err(|e| e.to_string())??;
    let (status, v) = reply;
    if status != 200 || !v["access_token"].is_string() {
        crate::warn!(
            "connector {}: code exchange refused (HTTP {status}, {})",
            con.name,
            truncate_chars(v["error"].as_str().unwrap_or("unknown"), 60)
        );
        return Err(format!(
            "The service refused the sign-in ({}). Try again.",
            truncate_chars(
                v["error_description"]
                    .as_str()
                    .or(v["error"].as_str())
                    .unwrap_or("unknown error"),
                120
            )
        ));
    }
    let mut updated = apply_tokens(state, &con.id, o, &v)?;
    updated.connected_at = Some(now_ms());
    if let Some(email) = v["id_token"].as_str().and_then(id_token_email) {
        updated.account = Some(email);
    }
    save(state, &con.id, updated).await?;
    let id = con.id.clone();
    let _ = state
        .db
        .call(move |c| {
            entities::update_connector(c, &id, None, None, Some(true))?;
            entities::set_connector_state(
                c,
                &id,
                &ConnectorState {
                    auth_state: Some("ok"),
                    ..Default::default()
                },
            )
        })
        .await;
    crate::gateway::stop(state, &con.id).await;
    crate::login::on_connector_updated(state, &con.id).await;
    crate::connectors::broadcast_changed(state).await;
    crate::info!("connector {}: signed in", con.name);
    Ok(crate::gateway::service_label(&con))
}

/// Finish from the address the owner copied after approving (when the
/// callback could not reach Iris directly).
pub async fn finish_from_url(state: &AppState, url: &str) -> Result<String, String> {
    let q = parse_query(url.trim());
    let flow_state = q
        .get("state")
        .ok_or("That address has no sign-in details. Copy the whole address.")?;
    finish(
        state,
        flow_state,
        q.get("code").map(String::as_str),
        q.get("error").map(String::as_str),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_round_trip() {
        let u = with_query(
            "https://auth.example.com/authorize",
            &[
                ("redirect_uri", "https://iris.example/api/oauth/callback"),
                ("scope", "a b"),
            ],
        );
        assert_eq!(
            u,
            "https://auth.example.com/authorize?redirect_uri=https%3A%2F%2Firis.example%2Fapi%2Foauth%2Fcallback&scope=a%20b"
        );
        let q = parse_query("http://127.0.0.1:7420/api/oauth/callback?code=4%2F0Ab&state=xyz#frag");
        assert_eq!(q["code"], "4/0Ab");
        assert_eq!(q["state"], "xyz");
        assert_eq!(parse_query("a=1+2&b")["a"], "1 2");
    }

    #[test]
    fn reads_email_from_id_token() {
        let payload = B64URL.encode(br#"{"email":"someone@example.com","sub":"1"}"#);
        assert_eq!(
            id_token_email(&format!("e30.{payload}.sig")).as_deref(),
            Some("someone@example.com")
        );
        assert_eq!(id_token_email("garbage"), None);
    }

    #[test]
    fn pkce_matches_rfc7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn www_authenticate_and_metadata_urls() {
        let (rm, scope) = parse_www_authenticate(
            r#"Bearer error="invalid_request", resource_metadata="https://api.example.com/.well-known/oauth-protected-resource/mcp/", scope="repo read:org""#,
        );
        assert_eq!(
            rm.as_deref(),
            Some("https://api.example.com/.well-known/oauth-protected-resource/mcp/")
        );
        assert_eq!(scope.as_deref(), Some("repo read:org"));
        assert_eq!(
            as_metadata_urls("https://example.com/login/oauth"),
            [
                "https://example.com/.well-known/oauth-authorization-server/login/oauth",
                "https://example.com/.well-known/openid-configuration/login/oauth",
                "https://example.com/login/oauth/.well-known/openid-configuration",
            ]
        );
        assert_eq!(
            as_metadata_urls("https://auth.example.com/")[0],
            "https://auth.example.com/.well-known/oauth-authorization-server"
        );
        assert!(check_endpoint("http://evil.example/token").is_err());
        assert!(check_endpoint("http://127.0.0.1:9/token").is_ok());
    }
}
