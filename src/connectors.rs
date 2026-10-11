//! The small built-in catalog of services to connect: Google (Gmail,
//! Calendar, Drive), GitHub, and a custom MCP server. Each entry says how
//! the connector is set up; the app walks the owner through it.
//!
//! - Google runs the open-source Google Workspace MCP server
//!   (`workspace-mcp`, started with `uvx`) on this machine in its external
//!   OAuth mode: Iris signs the owner in with their own Google OAuth client
//!   and hands the server a fresh access token on every request. Nothing
//!   Google-specific is implemented in Iris itself, and no client secret
//!   ships with Iris: the owner creates one in their Google Cloud project.
//! - GitHub is GitHub's remote MCP server, reached with a personal access
//!   token or the owner's own GitHub OAuth app.

use crate::entities::{self, ConnectorConfig, ConnectorState, OAuthConfig};
use crate::events::Audience;
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
    /// How the owner signs in: `google_client`, `token_or_oauth`, `custom`.
    pub setup: &'static str,
}

pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "google",
        label: "Google",
        description: "Gmail, Calendar, and Drive",
        requires: &["uvx"],
        setup: "google_client",
    },
    CatalogEntry {
        id: "github",
        label: "GitHub",
        description: "Code, issues, and pull requests",
        requires: &[],
        setup: "token_or_oauth",
    },
    CatalogEntry {
        id: "custom",
        label: "Custom",
        description: "Any MCP server: a web address or a program on the server",
        requires: &[],
        setup: "custom",
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
];
pub const GITHUB_MCP_URL: &str = "https://api.githubcopilot.com/mcp/";

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
                "redirect_uri": redirect_uri,
                "redirect_direct": direct,
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
    let id = existing
        .as_ref()
        .map(|c| c.id.clone())
        .unwrap_or_else(new_id);
    let seal_env =
        |m: BTreeMap<String, String>| crate::server::seal_connector_secrets(state, &id, "env", &m);
    let (transport, cfg, description, needs_sign_in) = match entry.id {
        "google" => {
            let old = existing.as_ref().and_then(|c| c.config.oauth.clone());
            let client_id = clean(&body.client_id)
                .or_else(|| old.as_ref().and_then(|o| o.client_id.clone()))
                .ok_or("paste the Client ID from Google Cloud")?;
            if !client_id.ends_with(".apps.googleusercontent.com") {
                return Err(
                    "that doesn't look like a Google Client ID (it ends in .apps.googleusercontent.com)"
                        .into(),
                );
            }
            let new_secret = clean(&body.client_secret);
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
            let same_client = old.as_ref().and_then(|o| o.client_id.as_deref()) == Some(&client_id);
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
                "Gmail, Calendar, and Drive",
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
                    "Code, issues, and pull requests",
                    false,
                )
            } else {
                let client_id = clean(&body.client_id)
                    .ok_or("paste a personal access token, or your GitHub OAuth app's Client ID and secret")?;
                let secret = clean(&body.client_secret)
                    .ok_or("paste your GitHub OAuth app's Client secret too")?;
                let o = OAuthConfig {
                    mode: "discover".into(),
                    client_id: Some(client_id),
                    client_secret: Some(crate::oauth::seal_client_secret(state, &id, &secret)?),
                    client_source: "owner".into(),
                    ..Default::default()
                };
                (
                    "http",
                    ConnectorConfig {
                        url: Some(GITHUB_MCP_URL.into()),
                        oauth: Some(o),
                        ..Default::default()
                    },
                    "Code, issues, and pull requests",
                    true,
                )
            }
        }
        _ => return Err("unknown service".into()),
    };
    let (cid, svc, name) = (id.clone(), entry.id.to_string(), entry.id.to_string());
    let is_new = existing.is_none();
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
                    ..Default::default()
                },
            )?;
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?;
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
        assert_eq!(ids, ["google", "github", "custom"]);
        assert!(catalog_entry("google").is_some());
        assert!(catalog_entry("spotify").is_none());
        // No client secret ships with Iris.
        assert!(GOOGLE_SCOPES.iter().all(|s| !s.contains("secret")));
    }
}
