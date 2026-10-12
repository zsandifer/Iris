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
    /// Sign-in generation when it started (see `Flows::epochs`).
    epoch: (u64, u64),
}

/// Sign-ins in progress, by `state` parameter.
#[derive(Default)]
pub struct Flows {
    flows: Mutex<HashMap<String, Flow>>,
    /// Serializes token refreshes and every change to a connector's
    /// sign-in state (see `credentials_lock`).
    refresh: tokio::sync::Mutex<()>,
    /// Sign-in generations (all connectors, and per connector id). Bumped
    /// under `credentials_lock` whenever a connector's address, client, or
    /// grant changes, so a code exchange that was already under way when
    /// that happened is refused instead of saving stale tokens.
    epochs: Mutex<(u64, HashMap<String, u64>)>,
}

impl Flows {
    fn epoch(&self, connector_id: &str) -> (u64, u64) {
        let e = self.epochs.lock().unwrap_or_else(|p| p.into_inner());
        (e.0, e.1.get(connector_id).copied().unwrap_or(0))
    }

    fn bump(&self, connector_id: Option<&str>) {
        let mut e = self.epochs.lock().unwrap_or_else(|p| p.into_inner());
        match connector_id {
            None => e.0 += 1,
            Some(id) => *e.1.entry(id.to_string()).or_default() += 1,
        }
    }
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

/// The URL's host is this machine: `localhost` or a loopback IP address
/// (parsed, so `127.example.com` does not count).
pub fn is_loopback_url(url: &str) -> bool {
    let h = host_of(url);
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Credentials may cross the network only over TLS. HTTP is reserved
/// for a server on this machine, with a parsed authority (no userinfo).
pub fn check_secure_url(url: &str) -> Result<(), String> {
    let safe = url.parse::<ureq::http::Uri>().ok().is_some_and(|uri| {
        let Some(authority) = uri.authority() else {
            return false;
        };
        if authority.as_str().contains('@') || url.contains('\\') {
            return false;
        }
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']');
        match uri.scheme_str() {
            Some("https") => !host.is_empty(),
            Some("http") => {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }
            _ => false,
        }
    });
    if safe {
        Ok(())
    } else {
        Err("I need an HTTPS address for a connection with credentials (HTTP is allowed only on localhost or a loopback IP).".into())
    }
}

fn private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => {
            v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.octets()[0] == 100 && (64..128).contains(&v.octets()[1])
        }
        std::net::IpAddr::V6(v) => {
            v.is_loopback()
                || v.is_unspecified()
                || (v.segments()[0] & 0xfe00) == 0xfc00
                || (v.segments()[0] & 0xffc0) == 0xfe80
                || v.to_ipv4_mapped().is_some_and(|m| private_ip(m.into()))
        }
    }
}

/// An address Iris may fetch while signing in: https, and (unless the
/// connector itself is on this machine) never a loopback, private, or
/// link-local destination, so a remote server cannot point Iris at the
/// owner's local network.
fn check_endpoint(url: &str, allow_local: bool) -> Result<(), String> {
    check_secure_url(url)?;
    let local = is_loopback_url(url);
    let scheme_ok = url.starts_with("https://") || (url.starts_with("http://") && local);
    if !scheme_ok || (local && !allow_local) {
        return Err("I can't use that sign-in server address.".into());
    }
    if !allow_local {
        use std::net::ToSocketAddrs;
        let port = if url.starts_with("https://") { 443 } else { 80 };
        let addrs = (host_of(url), port)
            .to_socket_addrs()
            .map_err(|_| "I could not find the sign-in server.".to_string())?;
        if addrs.into_iter().any(|a| private_ip(a.ip())) {
            return Err(
                "I can't use a sign-in server at a private address for this connection.".into(),
            );
        }
    }
    Ok(())
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

/// Resolves names but only connects to public addresses, so a name that
/// passed a check cannot later resolve to the owner's own network.
#[derive(Debug)]
struct PublicOnly;

impl ureq::unversioned::resolver::Resolver for PublicOnly {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let all = ureq::unversioned::resolver::DefaultResolver::default()
            .resolve(uri, config, timeout)?;
        let mut out = self.empty();
        for a in all.iter().filter(|a| !private_ip(a.ip())) {
            out.push(*a);
        }
        if out.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(out)
    }
}

/// HTTP client for signing in. `local`: the connector itself is on this
/// machine or the owner's network, so private addresses are allowed.
fn agent(local: bool) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        // Redirects could lead anywhere; metadata and token endpoints are
        // used exactly as published.
        .max_redirects(0)
        .timeout_global(Some(HTTP_TIMEOUT))
        .user_agent(format!("iris/{}", env!("CARGO_PKG_VERSION")))
        .build();
    if local {
        config.into()
    } else {
        ureq::Agent::with_parts(
            config,
            ureq::unversioned::transport::DefaultConnector::default(),
            PublicOnly,
        )
    }
}

/// The URL's host is on this machine or a private network: every address
/// it resolves to is private. A name with any public address is treated as
/// public, so a remote service cannot unlock private addresses by also
/// publishing one.
fn resolves_private(url: &str) -> bool {
    use std::net::ToSocketAddrs;
    if is_loopback_url(url) {
        return true;
    }
    let port = if url.starts_with("https://") { 443 } else { 80 };
    (host_of(url), port).to_socket_addrs().is_ok_and(|a| {
        let ips: Vec<std::net::IpAddr> = a.map(|a| a.ip()).collect();
        !ips.is_empty() && ips.iter().all(|ip| private_ip(*ip))
    })
}

fn get_json(url: &str, local: bool) -> Option<Value> {
    let mut resp = agent(local)
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
fn post_form(
    url: &str,
    form: &[(&str, &str)],
    local: bool,
    secrets: &[String],
) -> Result<(u16, Value), String> {
    check_endpoint(url, local)?;
    let body: Vec<String> = form
        .iter()
        .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(v)))
        .collect();
    let mut resp = agent(local)
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body.join("&").as_bytes())
        .map_err(|_| "I could not reach the sign-in server.".to_string())?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .unwrap_or_default();
    let mut v = serde_json::from_str(&text).unwrap_or_else(|_| {
        // Some servers answer form-encoded despite Accept.
        let q = parse_query(&text);
        json!(q)
    });
    let mut known = secrets.to_vec();
    known.extend(
        form.iter()
            .filter(|(k, _)| {
                matches!(
                    *k,
                    "client_secret" | "code" | "code_verifier" | "refresh_token" | "access_token"
                )
            })
            .map(|(_, v)| v.to_string()),
    );
    known.extend(
        ["access_token", "refresh_token", "client_secret", "id_token"]
            .iter()
            .filter_map(|k| v[*k].as_str().map(String::from)),
    );
    for key in ["error", "error_description"] {
        if let Some(text) = v[key].as_str() {
            v[key] = sanitize_provider_error(text, &known).into();
        }
    }
    Ok((status, v))
}

fn sanitize_provider_error(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    // Redact before truncation, including short secrets and form encoding.
    let mut patterns: Vec<String> = secrets
        .iter()
        .filter(|s| !s.is_empty())
        .flat_map(|s| [s.clone(), pct_encode(s)])
        .collect();
    // A token can be a prefix of another token. Hide the longest first.
    patterns.sort_unstable_by_key(|s| std::cmp::Reverse(s.len()));
    patterns.dedup();
    for secret in patterns {
        out = out.replace(&secret, "[hidden]");
    }
    let out: String = out.chars().filter(|c| !c.is_control()).collect();
    truncate_chars(&out, 120)
}

/// Only fixed protocol codes go to logs, never provider-supplied prose.
fn provider_error_code(v: &Value) -> &'static str {
    match v["error"].as_str() {
        Some("invalid_request") => "invalid_request",
        Some("invalid_client") => "invalid_client",
        Some("invalid_grant") => "invalid_grant",
        Some("unauthorized_client") => "unauthorized_client",
        Some("unsupported_grant_type") => "unsupported_grant_type",
        Some("invalid_scope") => "invalid_scope",
        Some("temporarily_unavailable") => "temporarily_unavailable",
        Some("server_error") => "server_error",
        _ => "unknown_error",
    }
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
    /// The service is on this machine or the owner's network.
    pub local: bool,
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
    check_secure_url(mcp_url)?;
    // An unauthenticated request tells us where the metadata lives.
    let probe = agent(true)
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
    // Metadata addresses a server points us to must be https (or this
    // machine, for testing), like the endpoints themselves.
    // Private addresses only for a connector the owner put on their own
    // machine or network.
    let allow_local = resolves_private(mcp_url);
    let mut prm_urls: Vec<String> = rm
        .into_iter()
        .filter(|u| check_endpoint(u, allow_local).is_ok())
        .collect();
    if !path.is_empty() {
        prm_urls.push(format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        ));
    }
    prm_urls.push(format!("{origin}/.well-known/oauth-protected-resource"));
    let prm = prm_urls.iter().find_map(|u| get_json(u, allow_local));
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
    check_endpoint(&issuer, allow_local)?;
    let meta = as_metadata_urls(&issuer)
        .iter()
        .find_map(|u| get_json(u, allow_local));
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
    check_endpoint(&auth, allow_local)?;
    check_endpoint(&token, allow_local)?;
    if let Some(r) = &reg {
        check_endpoint(r, allow_local)?;
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
        // Exactly as published: a trailing slash can matter.
        resource,
        local: allow_local,
    })
}

/// Dynamic client registration (blocking). Returns (client_id, secret).
fn register(
    endpoint: &str,
    redirect_uri: &str,
    local: bool,
) -> Result<(String, Option<String>), String> {
    let body = json!({
        "client_name": "Iris",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let mut resp = agent(local)
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

fn known_secrets(state: &AppState, id: &str, o: &OAuthConfig) -> Vec<String> {
    [
        ("client_secret", &o.client_secret),
        ("access_token", &o.access_token),
        ("refresh_token", &o.refresh_token),
    ]
    .into_iter()
    .filter_map(|(field, value)| open(state, id, field, value))
    .collect()
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

/// Sign a connection out: forget its tokens and account (the client Iris
/// registered or the owner created, the endpoints, and the rules stay, so
/// connecting again is one sign-in). Calls waiting for approval and
/// sign-ins under way are cancelled, and its server stops. Iris does not
/// revoke the grant at the provider.
pub async fn sign_out(state: &AppState, id: &str) -> Result<Option<entities::Connector>, String> {
    let creds = credentials_lock(state).await;
    let cid = id.to_string();
    let con = state
        .db
        .call(move |c| {
            let Some(con) = entities::get_connector(c, &cid)? else {
                return Ok(None);
            };
            let Some(mut o) = con.config.oauth.clone() else {
                return Ok(Some(Err(
                    "This connection doesn't sign in with an account.",
                )));
            };
            o.access_token = None;
            o.refresh_token = None;
            o.expires_at = None;
            o.account = None;
            o.connected_at = None;
            let mut cfg = con.config.clone();
            cfg.oauth = Some(o);
            entities::update_connector(c, &cid, None, Some(&cfg), None)?;
            Ok(entities::set_connector_state(
                c,
                &cid,
                &ConnectorState {
                    auth_state: Some("needs_sign_in"),
                    ..Default::default()
                },
            )?
            .map(Ok))
        })
        .await
        .map_err(|e| e.to_string())?;
    let con = match con {
        None => return Ok(None),
        Some(Err(e)) => return Err(e.into()),
        Some(Ok(c)) => c,
    };
    forget_flows(state, Some(id));
    drop(creds);
    crate::gateway::cancel_approvals(state, Some(id));
    crate::gateway::stop(state, id).await;
    crate::info!("connector {}: signed out by the owner", con.name);
    crate::connectors::broadcast_changed(state).await;
    Ok(Some(con))
}

/// Store an OAuth client the owner created (Google, GitHub OAuth app...).
pub fn seal_client_secret(state: &AppState, id: &str, secret: &str) -> Result<String, String> {
    seal(state, id, "client_secret", secret)
}

// ------------------------------------------------------------------ tokens

/// A fresh access token for the connector (refreshing it if needed), None
/// for connectors without OAuth.
/// Hold while changing a connector's sign-in (tokens, client, endpoints)
/// so a refresh in flight cannot write back older credentials.
pub async fn credentials_lock(state: &AppState) -> tokio::sync::MutexGuard<'_, ()> {
    state.oauth.refresh.lock().await
}

/// Read/refresh credentials and update a session under the same lock, so
/// a superseded snapshot can never install its token in a newer session.
pub(crate) async fn bearer(
    state: &AppState,
    con: &entities::Connector,
    _credentials: &tokio::sync::MutexGuard<'_, ()>,
) -> Result<Option<String>, TokenError> {
    crate::connectors::check_transport_security(&con.transport, &con.config)
        .map_err(TokenError::Unavailable)?;
    // Another call may have refreshed or signed in while we waited.
    let id = con.id.clone();
    let fresh = state
        .db
        .call(move |c| entities::get_connector(c, &id))
        .await
        .ok()
        .flatten();
    let Some(fresh) = fresh else {
        return Err(TokenError::NeedsSignIn);
    };
    crate::connectors::check_transport_security(&fresh.transport, &fresh.config)
        .map_err(TokenError::Unavailable)?;
    if con.config.oauth.is_none() && fresh.config.oauth.is_none() {
        return Ok(None);
    }
    if crate::gateway::target(&fresh) != crate::gateway::target(con) {
        return Err(TokenError::Unavailable(
            "This connection changed while I was checking its sign-in. Try again.".into(),
        ));
    }
    let Some(o) = fresh.config.oauth else {
        return Err(TokenError::NeedsSignIn);
    };
    if let (Some(_), Some(exp)) = (&o.access_token, o.expires_at)
        && exp > now_ms() + REFRESH_EARLY_MS
    {
        return open(state, &con.id, "access_token", &o.access_token)
            .map(Some)
            .ok_or(TokenError::NeedsSignIn);
    }
    if o.access_token.is_some() && o.expires_at.is_none() && o.refresh_token.is_none() {
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
    let local = o.local;
    let secrets = known_secrets(state, &con.id, &o);
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
        post_form(&token_endpoint, &form, local, &secrets)
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
        Ok((status, v))
            if status == 429
                || status >= 500
                || v["error"] == "temporarily_unavailable"
                || v["error"] == "server_error" =>
        {
            // The provider is having trouble: fail this call, keep the
            // connection signed in.
            crate::warn!(
                "connector {}: token refresh unavailable (HTTP {status})",
                con.name
            );
            Err(TokenError::Unavailable(format!("HTTP {status}")))
        }
        Ok((status, v)) => {
            crate::warn!(
                "connector {}: token refresh refused (HTTP {status}, {})",
                con.name,
                provider_error_code(&v)
            );
            Err(TokenError::NeedsSignIn)
        }
        Err(e) => {
            // The sign-in server is unreachable: not the owner's problem
            // yet; fail this call without asking them to sign in again.
            crate::warn!("connector {}: token refresh failed", con.id);
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
pub async fn invalidate(state: &AppState, connector_id: &str, rejected: Option<&str>) {
    // Serialized with refreshes, on the latest stored state: only expire
    // the token that was refused, never restore older tokens.
    let _guard = state.oauth.refresh.lock().await;
    let id = connector_id.to_string();
    let Some(o) = state
        .db
        .call(move |c| entities::get_connector(c, &id))
        .await
        .ok()
        .flatten()
        .and_then(|c| c.config.oauth)
    else {
        return;
    };
    let current = open(state, connector_id, "access_token", &o.access_token);
    if rejected.is_some() && current.as_deref() != rejected {
        return; // Already replaced by a refresh.
    }
    let mut o = o;
    o.expires_at = Some(0);
    let _ = save(state, connector_id, o).await;
}

/// Sign-ins in progress for a connector (tests).
#[cfg(test)]
pub fn pending_flows(state: &AppState, connector_id: &str) -> usize {
    state
        .oauth
        .flows
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .filter(|f| f.connector_id == connector_id)
        .count()
}

/// Drop sign-ins in progress for a connector (or all): a connection that
/// was turned off or removed is not signed in by an old browser tab.
pub fn forget_flows(state: &AppState, connector_id: Option<&str>) {
    state.oauth.bump(connector_id);
    state
        .oauth
        .flows
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|_, f| connector_id.is_some_and(|c| c != f.connector_id));
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
    let _creds = credentials_lock(state).await;
    // The latest stored sign-in state (a refresh may just have run).
    let fresh_id = con.id.clone();
    let con = state
        .db
        .call(move |c| entities::get_connector(c, &fresh_id))
        .await
        .ok()
        .flatten()
        .ok_or_else(|| StartError::Other("connection not found".into()))?;
    let mut o = con
        .config
        .oauth
        .clone()
        .unwrap_or_else(OAuthConfig::discover);
    // Reconnecting an older Google grant must request today's scopes too.
    // `o.scopes` keeps recording the current grant until this one lands.
    let mut scopes = o.scopes.clone();
    if con.service == "google" && o.mode == "preset" {
        for scope in crate::connectors::GOOGLE_SCOPES {
            if !scopes.iter().any(|s| s == scope) {
                scopes.push((*scope).to_string());
            }
        }
    }
    if con.transport == "http" {
        crate::connectors::check_credential_url(&con.config).map_err(StartError::Other)?;
    }
    let (redirect, direct) = if con.service == "google" {
        (crate::google_setup::callback(&state.cfg.public_url).ok_or_else(|| StartError::Other(
            "I need public_url set to Iris's HTTPS address first. Open Google setup, step 4, for help.".into()
        ))?, true)
    } else {
        redirect_uri(state, origin)
    };
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
            o.scopes = d.scopes.clone();
            scopes = d.scopes;
        }
        o.resource = Some(d.resource);
        o.local = d.local;
    }
    let reuse_client = o.client_id.is_some()
        && (o.client_source == "owner" || o.redirect_uri.as_deref() == Some(redirect.as_str()));
    if !reuse_client {
        match o.registration_endpoint.clone() {
            Some(reg) if o.client_source != "owner" => {
                let (r, local) = (redirect.clone(), o.local);
                let (cid, secret) = tokio::task::spawn_blocking(move || register(&reg, &r, local))
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
    let scope = scopes.join(" ");
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
    if con.service == "google" {
        state
            .db
            .call(|c| {
                crate::google_setup::save(
                    c,
                    &crate::google_setup::Progress {
                        step: 6,
                        sign_in: true,
                        completed: false,
                        issue: None,
                        ..crate::google_setup::read(c)?
                    },
                )
            })
            .await
            .map_err(|_| {
                StartError::Other("I couldn't save Google setup progress. Please try again.".into())
            })?;
        crate::connectors::broadcast_changed(state).await;
    }
    // Exchanges of earlier flows were for the previous client/endpoints.
    state.oauth.bump(Some(&con.id));
    let epoch = state.oauth.epoch(&con.id);
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
                epoch,
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
    let id = flow.connector_id.clone();
    let con = state
        .db
        .call(move |c| entities::get_connector(c, &id))
        .await
        .ok()
        .flatten()
        .ok_or("That connection was removed.")?;
    if state.oauth.epoch(&con.id) != flow.epoch {
        return Err(
            "That sign-in was replaced by a newer change. Start again from the app.".into(),
        );
    }
    if let Some(e) = error {
        let _creds = credentials_lock(state).await;
        if state.oauth.epoch(&con.id) != flow.epoch {
            return Err(
                "That sign-in was replaced by a newer change. Start again from the app.".into(),
            );
        }
        if con.service == "google" {
            return Err(crate::google_setup::report_issue(state, e)
                .await
                .map_err(|_| "I couldn't save the Google setup fix. Please try again.")?
                .unwrap_or("I couldn't finish Google sign-in. Please try again.")
                .into());
        }
        if e == "access_denied" {
            crate::awareness::finish(
                state,
                "connector",
                &flow.connector_id,
                "declined",
                "You declined the connection. Nothing was connected.",
            )
            .await;
        }
        return Err(match e {
            "access_denied" => "You declined, so nothing was connected.".to_string(),
            _ => "The service reported a sign-in problem. Try again.".to_string(),
        });
    }
    let code = code
        .filter(|c| !c.is_empty())
        .ok_or("The service did not send a sign-in code.")?;
    let flow_epoch = flow.epoch;
    let o = con
        .config
        .oauth
        .clone()
        .ok_or("That connection does not use sign-in.")?;
    crate::connectors::check_transport_security(&con.transport, &con.config)?;
    let token_endpoint = o.token_endpoint.clone().ok_or("No token address known.")?;
    let client_id = o.client_id.clone().unwrap_or_default();
    let secret = open(state, &con.id, "client_secret", &o.client_secret);
    let resource = (o.mode == "discover").then(|| o.resource.clone()).flatten();
    let local = o.local;
    let secrets = known_secrets(state, &con.id, &o);
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
        post_form(&token_endpoint, &form, local, &secrets)
    })
    .await
    .map_err(|e| e.to_string())??;
    let (status, v) = reply;
    if status != 200 || !v["access_token"].is_string() {
        crate::warn!(
            "connector {}: code exchange refused (HTTP {status}, {})",
            con.name,
            provider_error_code(&v)
        );
        if con.service == "google" {
            let _creds = credentials_lock(state).await;
            if state.oauth.epoch(&con.id) != flow_epoch {
                return Err(
                    "That sign-in was replaced by a newer change. Start again from the app.".into(),
                );
            }
            return Err(crate::google_setup::report_issue(
                state,
                v["error"].as_str().unwrap_or(""),
            )
            .await
            .map_err(|_| "I couldn't save the Google setup fix. Please try again.")?
            .unwrap_or("I couldn't finish Google sign-in. Please try again.")
            .into());
        }
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
    let _creds = credentials_lock(state).await;
    // Refused if the connector's address, client, or grant changed while
    // the code was being exchanged (a newer sign-in included).
    if state.oauth.epoch(&con.id) != flow_epoch {
        crate::warn!("connector {}: superseded sign-in ignored", con.name);
        return Err(
            "That sign-in was replaced by a newer change. Start again from the app.".into(),
        );
    }
    // Google allows people to deselect individual services on consent. A
    // partial grant must not light up every service in Connections.
    if con.service == "google"
        && let Some(granted) = v["scope"].as_str()
    {
        let missing = crate::connectors::GOOGLE_SCOPES
            .iter()
            .filter(|s| {
                s.starts_with("https://www.googleapis.com/auth/") && !s.contains("/userinfo.")
            })
            .any(|required| !granted.split_whitespace().any(|scope| scope == *required));
        if missing {
            return Err(
                crate::google_setup::report_issue(state, "missing_permissions")
                    .await
                    .map_err(|_| "I couldn't save the Google setup fix. Please try again.")?
                    .unwrap_or(
                        "I need permission for all of the Google services. Please sign in again.",
                    )
                    .into(),
            );
        }
    }
    // Built on the latest stored state; a new sign-in replaces the old
    // grant entirely (never keeps the previous account's refresh token).
    let latest_id = con.id.clone();
    let mut o = state
        .db
        .call(move |c| entities::get_connector(c, &latest_id))
        .await
        .ok()
        .flatten()
        .and_then(|c| c.config.oauth)
        .ok_or("That connection was removed.")?;
    o.refresh_token = None;
    o.account = None;
    let mut updated = apply_tokens(state, &con.id, o, &v)?;
    updated.connected_at = Some(now_ms().max(updated.connected_at.unwrap_or(0).saturating_add(1)));
    if let Some(email) = v["id_token"].as_str().and_then(id_token_email) {
        updated.account = Some(email);
    }
    if con.service == "google" {
        // Every Google service was granted (checked above): record it, and
        // turn on the services an older default left out.
        updated.scopes = crate::connectors::GOOGLE_SCOPES
            .iter()
            .map(|s| s.to_string())
            .collect();
        let id = con.id.clone();
        state
            .db
            .call(move |c| {
                if let Some(mut cfg) = entities::get_connector(c, &id)?.map(|c| c.config)
                    && let Some(args) = crate::connectors::upgraded_google_args(&cfg.args)
                {
                    cfg.args = args;
                    entities::update_connector(c, &id, None, Some(&cfg), None)?;
                }
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?;
    }
    save(state, &con.id, updated).await?;
    let id = con.id.clone();
    let _ = state
        .db
        .call(move |c| {
            // Signing in never turns a connection on by itself.
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
    if con.service == "google" {
        state
            .db
            .call(|c| {
                crate::google_setup::save(
                    c,
                    &crate::google_setup::Progress {
                        step: 6,
                        sign_in: true,
                        completed: true,
                        issue: None,
                        ..crate::google_setup::read(c)?
                    },
                )
            })
            .await
            .map_err(|e| e.to_string())?;
    }
    // Any other exchange still under way is now stale.
    state.oauth.bump(Some(&con.id));
    drop(_creds);
    // Approvals were for the previous sign-in.
    crate::gateway::cancel_approvals(state, Some(&con.id));
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
    fn provider_errors_hide_every_known_secret_and_only_log_protocol_codes() {
        let secrets = [
            "client/secret",
            "code42",
            "verifier42",
            "refresh42",
            "access42",
            "x",
        ]
        .map(String::from);
        let text = format!(
            "<script>client/secret client%2Fsecret code42 verifier42 refresh42 access42 x</script>\n{}",
            "界".repeat(200)
        );
        let clean = sanitize_provider_error(&text, &secrets);
        for secret in &secrets {
            assert!(!clean.contains(secret), "{clean}");
        }
        assert!(!clean.contains("client%2Fsecret"));
        assert_eq!(
            sanitize_provider_error(
                "access42-longer",
                &["access42".into(), "access42-longer".into()]
            ),
            "[hidden]"
        );
        assert!(clean.chars().count() <= 121);
        assert_eq!(
            provider_error_code(&json!({"error": "invalid_grant"})),
            "invalid_grant"
        );
        assert_eq!(
            provider_error_code(&json!({"error": "invalid_grant secret\n<script>"})),
            "unknown_error"
        );
    }

    #[test]
    fn credential_urls_require_tls_or_parsed_loopback_authorities() {
        for url in [
            "http://connector.example/mcp",
            "http://192.168.1.2/mcp",
            "http://localhost.example/mcp",
            "http://127.0.0.1.example/mcp",
            "http://localhost@evil.example/mcp",
            "http://evil.example@localhost/mcp",
            "http://localhost\\@evil.example/mcp",
            "ftp://localhost/mcp",
        ] {
            assert!(check_secure_url(url).is_err(), "{url}");
            assert!(
                discover(url).is_err(),
                "discovery must refuse before connecting: {url}"
            );
        }
        for url in [
            "https://connector.example/mcp",
            "http://localhost:7999/mcp",
            "http://127.0.0.2:7999/mcp",
            "http://[::1]:7999/mcp",
        ] {
            assert!(check_secure_url(url).is_ok(), "{url}");
        }
    }

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
        assert!(check_endpoint("http://evil.example/token", false).is_err());
        assert!(check_endpoint("http://127.0.0.1:9/token", true).is_ok());
        // Remote servers cannot point Iris at this machine or the LAN.
        assert!(check_endpoint("http://127.0.0.1:9/token", false).is_err());
        assert!(check_endpoint("https://127.0.0.1/token", false).is_err());
        assert!(check_endpoint("https://10.0.0.5/token", false).is_err());
        assert!(check_endpoint("https://[::1]/token", false).is_err());
        // Names that merely start with 127 are not loopback.
        assert!(!is_loopback_url("http://127.attacker.example/token"));
        assert!(check_endpoint("http://127.attacker.example/token", true).is_err());
        assert!(is_loopback_url("http://[::1]:8/x"));
        assert!(is_loopback_url("http://LOCALHOST/x"));
    }
}
