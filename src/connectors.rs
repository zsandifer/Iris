//! The small built-in catalog of services to connect: Google (Gmail,
//! Calendar, Drive), GitHub, services with a hosted MCP server the owner just
//! signs in to (Notion, Todoist, Linear, Atlassian, Canva), and a custom MCP
//! server. Each entry says how the connector is set up; the app walks the
//! owner through it.
//!
//! - Google runs the open-source Google Workspace MCP server
//!   (`workspace-mcp`, started with `uvx`) on this machine in its external
//!   OAuth mode: Iris signs the owner in with their own Google OAuth client
//!   and hands the server a fresh access token on every request. Nothing
//!   Google-specific is implemented in Iris itself, and no client secret
//!   ships with Iris: the owner creates one in their Google Cloud project.
//! - GitHub is GitHub's remote MCP server, reached with a personal access
//!   token or the owner's own GitHub OAuth app.
//! - One-tap services (`oauth_sign_in`) are the vendor's own hosted MCP
//!   server at a fixed address. Signing in is the same MCP authorization
//!   flow as a custom connection (discovery, dynamic client registration,
//!   PKCE): Iris registers itself, so the owner only approves access on the
//!   service's page. Each address was checked to publish a registration
//!   endpoint that accepts Iris's callback; services that only allow
//!   pre-registered apps are left to the custom path.

use crate::entities::{self, ConnectorConfig, ConnectorState, OAuthConfig};
use crate::events::Audience;
use crate::gateway::policy::{Mode, Policy};
use crate::state::AppState;
use crate::util::new_id;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub struct CatalogEntry {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Programs the server needs (checked on PATH).
    pub requires: &'static [&'static str],
    /// How the owner signs in: `google_client`, `token_or_oauth`,
    /// `oauth_sign_in`, `custom`.
    pub setup: &'static str,
    /// `oauth_sign_in`: the service's hosted MCP server.
    pub url: Option<&'static str>,
    /// The approval rules a new connection starts with.
    pub mode: Mode,
}

/// A service whose hosted MCP server the owner only signs in to. Its tools
/// change shared work (pages, issues, tasks, designs), so every change asks
/// first until the owner relaxes the rules.
const fn sign_in(
    id: &'static str,
    label: &'static str,
    description: &'static str,
    url: &'static str,
) -> CatalogEntry {
    CatalogEntry {
        id,
        label,
        description,
        requires: &[],
        setup: "oauth_sign_in",
        url: Some(url),
        mode: Mode::Careful,
    }
}

pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "google",
        label: "Google",
        description: "Gmail, Calendar, Drive, and YouTube read-only",
        requires: &["uvx"],
        setup: "google_client",
        url: None,
        mode: Mode::Standard,
    },
    CatalogEntry {
        id: "github",
        label: "GitHub",
        description: "Code, issues, and pull requests",
        requires: &[],
        setup: "token_or_oauth",
        url: None,
        mode: Mode::Standard,
    },
    sign_in(
        "notion",
        "Notion",
        "Pages, notes, and databases",
        "https://mcp.notion.com/mcp",
    ),
    sign_in(
        "todoist",
        "Todoist",
        "Tasks, projects, and due dates",
        "https://ai.todoist.net/mcp",
    ),
    sign_in(
        "linear",
        "Linear",
        "Issues, projects, and cycles",
        "https://mcp.linear.app/mcp",
    ),
    sign_in(
        "atlassian",
        "Atlassian",
        "Jira issues and Confluence pages",
        "https://mcp.atlassian.com/v1/mcp",
    ),
    sign_in(
        "canva",
        "Canva",
        "Designs, folders, and brand templates",
        "https://mcp.canva.com/mcp",
    ),
    CatalogEntry {
        id: "custom",
        label: "Custom",
        description: "Any MCP server: a web address or a program on the server",
        requires: &[],
        setup: "custom",
        url: None,
        mode: Mode::Standard,
    },
];

pub fn catalog_entry(id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.id == id)
}

/// The Google Workspace MCP server release Iris is set up for (the owner
/// can change it under the connection's advanced settings).
pub const GOOGLE_MCP_PACKAGE: &str = "workspace-mcp@2.1.0";
pub const GOOGLE_SCOPES: &[&str] = &[
    "openid",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/gmail.modify",
    "https://www.googleapis.com/auth/calendar",
    "https://www.googleapis.com/auth/drive",
    "https://www.googleapis.com/auth/youtube.readonly",
];
pub const GITHUB_MCP_URL: &str = "https://api.githubcopilot.com/mcp/";

/// Check both newly saved settings and old settings at dispatch.
pub fn check_transport_security(transport: &str, cfg: &ConnectorConfig) -> Result<(), String> {
    if transport == "http" && (cfg.oauth.is_some() || !cfg.headers.is_empty()) {
        check_credential_url(cfg)?;
    }
    Ok(())
}

pub fn check_credential_url(cfg: &ConnectorConfig) -> Result<(), String> {
    let url = cfg.url.as_deref().unwrap_or_default();
    // Iris chooses a numeric port for an HTTP server it starts locally.
    // Validate the same authority before the server has been started.
    let url = if cfg.command.is_some() {
        let authority = url
            .split_once("://")
            .map_or("", |(_, rest)| rest)
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("");
        if authority.contains("{port}")
            && !authority
                .strip_suffix(":{port}")
                .is_some_and(|host| !host.contains("{port}"))
        {
            return Err(
                "I can use a port placeholder only in the address's port, not its host.".into(),
            );
        }
        url.replace("{port}", "1")
    } else {
        url.to_string()
    };
    crate::oauth::check_secure_url(&url)
}

/// Tell the owner's apps the connector list changed.
pub async fn broadcast_changed(state: &AppState) {
    if let Ok(Some(owner)) = state.db.call(crate::db::owner_user_id).await {
        state.hub.send(
            Audience::Users(vec![owner]),
            json!({"type": "connectors.changed"}),
        );
    }
}

/// Catalog for the app, with what is already connected.
pub async fn catalog_json(state: &AppState, origin: Option<&str>) -> Vec<Value> {
    let existing = state
        .db
        .call(|c| entities::list_connectors(c, false))
        .await
        .unwrap_or_default();
    let (redirect_uri, direct) = crate::oauth::redirect_uri(state, origin);
    CATALOG
        .iter()
        .map(|e| {
            let con = existing
                .iter()
                .find(|c| c.service == e.id && e.id != "custom");
            let missing: Vec<&str> = e
                .requires
                .iter()
                .copied()
                .filter(|p| crate::harness::resolve_command(p).is_none())
                .collect();
            json!({
                "id": e.id,
                "label": e.label,
                "description": e.description,
                "setup": e.setup,
                "missing": missing,
                "connector_id": con.map(|c| c.id.clone()),
                "connected": con.is_some_and(|c| c.active()),
                "redirect_uri": if e.id == "google" { crate::google_setup::callback(&state.cfg.public_url) } else { Some(redirect_uri.clone()) },
                "redirect_direct": if e.id == "google" { crate::google_setup::callback(&state.cfg.public_url).is_some() } else { direct },
            })
        })
        .collect()
}

/// What the owner sends to set up a catalog service.
#[derive(Debug, Default, serde::Deserialize)]
pub struct SetupBody {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// GitHub: a personal access token instead of signing in.
    pub token: Option<String>,
}

fn clean(v: &Option<String>) -> Option<String> {
    v.as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Create (or update) the connector for a catalog service. Returns the
/// connector id and whether the owner still has to sign in.
pub async fn setup(
    state: &AppState,
    service: &str,
    body: &SetupBody,
) -> Result<(String, bool), String> {
    let entry = catalog_entry(service)
        .filter(|e| e.id != "custom")
        .ok_or("unknown service")?;
    let _creds = crate::oauth::credentials_lock(state).await;
    let existing = {
        let name = entry.id.to_string();
        state
            .db
            .call(move |c| entities::get_connector_by_name(c, &name))
            .await
            .map_err(|e| e.to_string())?
    };
    if let Some(c) = &existing
        && c.service != entry.id
    {
        return Err(format!(
            "a custom connection is already named {}; rename or remove it first",
            entry.id
        ));
    }
    // Setting up a one-tap service again keeps its sign-in (and any sign-in
    // under way) as long as it still points at the service's server.
    if let (Some(url), Some(c)) = (entry.url, &existing)
        && c.config.url.as_deref() == Some(url)
        && c.config.oauth.is_some()
    {
        return Ok((c.id.clone(), c.auth_state != "ok"));
    }
    let id = existing
        .as_ref()
        .map(|c| c.id.clone())
        .unwrap_or_else(new_id);
    let seal_env =
        |m: BTreeMap<String, String>| crate::server::seal_connector_secrets(state, &id, "env", &m);
    let (transport, cfg, description, needs_sign_in) = match entry.id {
        _ if entry.setup == "oauth_sign_in" => {
            let url = entry.url.ok_or("unknown service")?;
            (
                "http",
                ConnectorConfig {
                    url: Some(url.into()),
                    oauth: Some(OAuthConfig::discover()),
                    ..Default::default()
                },
                entry.description,
                true,
            )
        }
        "google" => {
            let old = existing.as_ref().and_then(|c| c.config.oauth.clone());
            let client_id = clean(&body.client_id)
                .or_else(|| old.as_ref().and_then(|o| o.client_id.clone()))
                .ok_or("paste the Client ID from Google Cloud")?;
            if !client_id.ends_with(".apps.googleusercontent.com") || client_id.len() > 500 {
                return Err(
                    "that doesn't look like a Google Client ID (it ends in .apps.googleusercontent.com)"
                        .into(),
                );
            }
            let new_secret = clean(&body.client_secret);
            if new_secret.as_ref().is_some_and(|s| s.len() > 500) {
                return Err("I couldn't use that login key. Paste the client ID and secret from Google Cloud.".into());
            }
            let sealed_secret = match &new_secret {
                Some(s) => Some(crate::oauth::seal_client_secret(state, &id, s)?),
                None => old.as_ref().and_then(|o| o.client_secret.clone()),
            }
            .ok_or("paste the Client secret from Google Cloud")?;
            // The local server also needs the client (it only validates
            // tokens with it); it stays on Iris's side.
            let secret_plain = match new_secret {
                Some(s) => s,
                None => {
                    let open = |v: &str| {
                        state
                            .vault
                            .open_str(v, &format!("connector:{id}:oauth:client_secret"))
                            .map_err(|e| e.to_string())
                    };
                    open(&sealed_secret)?
                }
            };
            let same_client = old.as_ref().and_then(|o| o.client_id.as_deref()) == Some(&client_id)
                && old
                    .as_ref()
                    .and_then(|o| o.client_secret.as_ref())
                    .and_then(|s| {
                        state
                            .vault
                            .open_str(s, &format!("connector:{id}:oauth:client_secret"))
                            .ok()
                    })
                    .as_deref()
                    == Some(secret_plain.as_str());
            let env = seal_env(BTreeMap::from([
                ("MCP_ENABLE_OAUTH21".into(), "true".into()),
                ("EXTERNAL_OAUTH21_PROVIDER".into(), "true".into()),
                ("WORKSPACE_MCP_STATELESS_MODE".into(), "true".into()),
                ("WORKSPACE_MCP_HOST".into(), "127.0.0.1".into()),
                ("WORKSPACE_MCP_PORT".into(), "{port}".into()),
                ("PORT".into(), "{port}".into()),
                ("GOOGLE_OAUTH_CLIENT_ID".into(), client_id.clone()),
                ("GOOGLE_OAUTH_CLIENT_SECRET".into(), secret_plain),
            ]))
            .map_err(|e| e.to_string())?;
            let mut o = if same_client {
                old.clone().unwrap_or_default()
            } else {
                OAuthConfig::default()
            };
            o.mode = "preset".into();
            o.authorization_endpoint = Some("https://accounts.google.com/o/oauth2/v2/auth".into());
            o.token_endpoint = Some("https://oauth2.googleapis.com/token".into());
            o.scopes = GOOGLE_SCOPES.iter().map(|s| s.to_string()).collect();
            o.extra_params = BTreeMap::from([
                ("access_type".into(), "offline".into()),
                ("prompt".into(), "consent".into()),
                ("include_granted_scopes".into(), "true".into()),
            ]);
            o.client_id = Some(client_id);
            o.client_secret = Some(sealed_secret);
            o.client_source = "owner".into();
            let signed_in = o.refresh_token.is_some();
            let args = existing
                .as_ref()
                .filter(|c| !c.config.args.is_empty())
                .map(|c| c.config.args.clone())
                .unwrap_or_else(|| {
                    [
                        GOOGLE_MCP_PACKAGE,
                        "--transport",
                        "streamable-http",
                        "--tools",
                        "gmail",
                        "calendar",
                        "drive",
                    ]
                    .map(String::from)
                    .to_vec()
                });
            (
                "http",
                ConnectorConfig {
                    command: Some("uvx".into()),
                    args,
                    env,
                    url: Some("http://127.0.0.1:{port}/mcp".into()),
                    headers: BTreeMap::new(),
                    oauth: Some(o),
                },
                entry.description,
                !signed_in,
            )
        }
        "github" => {
            if let Some(token) = clean(&body.token) {
                let headers = crate::server::seal_connector_secrets(
                    state,
                    &id,
                    "headers",
                    &BTreeMap::from([("Authorization".into(), format!("Bearer {token}"))]),
                )
                .map_err(|e| e.to_string())?;
                (
                    "http",
                    ConnectorConfig {
                        url: Some(GITHUB_MCP_URL.into()),
                        headers,
                        ..Default::default()
                    },
                    entry.description,
                    false,
                )
            } else {
                let client_id = clean(&body.client_id)
                    .ok_or("paste a personal access token, or your GitHub OAuth app's Client ID and secret")?;
                let secret = clean(&body.client_secret)
                    .ok_or("paste your GitHub OAuth app's Client secret too")?;
                let o = OAuthConfig {
                    client_id: Some(client_id),
                    client_secret: Some(crate::oauth::seal_client_secret(state, &id, &secret)?),
                    client_source: "owner".into(),
                    ..OAuthConfig::discover()
                };
                (
                    "http",
                    ConnectorConfig {
                        url: Some(GITHUB_MCP_URL.into()),
                        oauth: Some(o),
                        ..Default::default()
                    },
                    entry.description,
                    true,
                )
            }
        }
        _ => return Err("unknown service".into()),
    };
    check_transport_security(transport, &cfg)?;
    let (cid, svc, name) = (id.clone(), entry.id.to_string(), entry.id.to_string());
    let is_new = existing.is_none();
    // A new connection starts with the service's rules; the owner's own
    // choice is kept when it is set up again.
    let policy = is_new.then(|| Policy {
        mode: entry.mode,
        ..Default::default()
    });
    state
        .db
        .call(move |c| {
            if is_new {
                entities::insert_connector(c, &cid, &name, description, transport, &cfg, true)?;
            } else {
                entities::update_connector(c, &cid, Some(description), Some(&cfg), Some(true))?;
            }
            entities::set_connector_state(
                c,
                &cid,
                &ConnectorState {
                    service: Some(&svc),
                    auth_state: Some(if needs_sign_in { "needs_sign_in" } else { "ok" }),
                    policy: policy.as_ref(),
                    ..Default::default()
                },
            )?;
            if svc == "google" {
                crate::google_setup::save(
                    c,
                    &crate::google_setup::Progress {
                        step: 6,
                        sign_in: true,
                        completed: !needs_sign_in,
                        issue: None,
                        ..crate::google_setup::read(c)?
                    },
                )?;
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?;
    // Sign-ins under way were for the previous setup.
    crate::oauth::forget_flows(state, Some(&id));
    drop(_creds);
    crate::gateway::cancel_approvals(state, Some(&id));
    crate::gateway::stop(state, &id).await;
    if !needs_sign_in {
        crate::login::on_connector_updated(state, &id).await;
    }
    broadcast_changed(state).await;
    Ok((id, needs_sign_in))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_small_and_known() {
        let ids: Vec<&str> = CATALOG.iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            [
                "google",
                "github",
                "notion",
                "todoist",
                "linear",
                "atlassian",
                "canva",
                "custom"
            ]
        );
        assert!(catalog_entry("google").is_some());
        assert!(catalog_entry("spotify").is_none());
        // No client secret ships with Iris.
        assert!(GOOGLE_SCOPES.iter().all(|s| !s.contains("secret")));
    }

    #[test]
    fn one_tap_services_are_hosted_https_servers_that_ask_before_changes() {
        for e in CATALOG {
            if e.setup == "oauth_sign_in" {
                let url = e.url.expect("a one-tap service has its server's address");
                assert!(url.starts_with("https://"), "{url}");
                assert!(
                    crate::oauth::check_secure_url(url).is_ok(),
                    "{url} is a public https address"
                );
                assert_eq!(e.mode, Mode::Careful, "{}", e.id);
                assert!(e.requires.is_empty());
                // The connector is named after the service: a valid MCP
                // server name that is not reserved.
                assert!(e.id.bytes().all(|b| b.is_ascii_lowercase()), "{}", e.id);
            } else {
                assert!(e.url.is_none(), "{}", e.id);
                assert_eq!(e.mode, Mode::Standard, "{}", e.id);
            }
        }
    }
}
