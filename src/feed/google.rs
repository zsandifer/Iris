//! Google credentials stay on Iris's side. The feed uses the owner's
//! connected Google connector and the same vault/refresh path as the gateway.

use crate::db::{self, NewMessage};
use crate::entities;
use crate::state::AppState;
use crate::util::Result;
use serde_json::json;

pub async fn access_token(state: &AppState, user_id: &str) -> Option<String> {
    // Connectors belong to the owner, never another feed user.
    let credentials = crate::oauth::credentials_lock(state).await;
    let uid = user_id.to_string();
    let con = state
        .db
        .call(move |c| {
            if db::owner_user_id(c)?.as_deref() != Some(uid.as_str()) {
                return Ok(None);
            }
            Ok(entities::list_connectors(c, true)?.into_iter().find(|c| {
                c.service == "google"
                        && c.active()
                        && c.config.oauth.is_some()
                        // The owner turned YouTube off for Iris.
                        && !c.policy.is_off("youtube")
            }))
        })
        .await
        .ok()
        .flatten()?;
    crate::oauth::bearer(state, &con, &credentials)
        .await
        .ok()
        .flatten()
}

/// An old Google grant may lack YouTube permission. Explain once, while
/// continuing the feed with public sources; never reflect provider prose.
pub async fn note_reconnect(state: &AppState, user_id: &str) -> Result<()> {
    let uid = user_id.to_string();
    let message = state
        .db
        .call(move |c| {
            db::write_tx(c, |c| {
                let st = super::store::get_state(c, &uid)?;
                if st.paused
                    || st.youtube_reconnect_noted
                    || db::get_setting(c, "paused")?.is_some()
                {
                    return Ok(None);
                }
                super::store::update_state(c, &uid, |s| s.youtube_reconnect_noted = true)?;
                let main = db::ensure_main_conversation(c, &uid)?;
                db::insert_message(c, &NewMessage {
                    conversation_id: &main,
                    role: "assistant",
                    content: "I can add your YouTube subscriptions and likes to For you once you \
                              reconnect Google in Settings → Connections to add YouTube access. \
                              I'll keep finding posts from your other sources meanwhile.",
                    meta: Some(&json!({"initiated_by": "feed"})),
                    ..Default::default()
                }).map(Some)
            })
        })
        .await?;
    if let Some(m) = message {
        state.hub.to_users(
            &[user_id.to_string()],
            json!({
                "type": "message.created", "conversation_id": m.conversation_id, "message": m
            }),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};
    use crate::entities::{ConnectorConfig, ConnectorState, OAuthConfig};
    use crate::util::now_ms;

    fn setup() -> (AppState, tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = Config {
            public_url: "https://iris.example".into(),
            ..Default::default()
        };
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        let state = AppState::new(cfg, paths, db).unwrap();
        let owner = state.db.with(db::owner_user_id).unwrap().unwrap();
        (state, tmp, owner)
    }

    fn connect(state: &AppState, expires_at: i64, endpoint: Option<String>) {
        let seal = |field| {
            state
                .vault
                .seal_str(
                    &format!("test-{field}"),
                    &format!("connector:google-test:oauth:{field}"),
                )
                .unwrap()
        };
        let config = ConnectorConfig {
            url: Some("https://google-mcp.example/mcp".into()),
            oauth: Some(OAuthConfig {
                mode: "preset".into(),
                authorization_endpoint: Some("https://accounts.google.com/o/oauth2/v2/auth".into()),
                client_id: Some("test-client".into()),
                client_source: "owner".into(),
                token_endpoint: endpoint,
                access_token: Some(seal("access_token")),
                refresh_token: Some(seal("refresh_token")),
                expires_at: Some(expires_at),
                scopes: vec!["openid".into()],
                local: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        state
            .db
            .with(|c| {
                entities::insert_connector(c, "google-test", "Google", "", "http", &config, true)?;
                entities::set_connector_state(
                    c,
                    "google-test",
                    &ConnectorState {
                        service: Some("google"),
                        ..Default::default()
                    },
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn feed_reads_only_the_owners_active_google_oauth_token() {
        let (state, _tmp, owner) = setup();
        assert!(access_token(&state, &owner).await.is_none());
        connect(&state, now_ms() + 3_600_000, None);
        assert_eq!(
            access_token(&state, &owner).await.as_deref(),
            Some("test-access_token")
        );
        assert!(access_token(&state, "other-user").await.is_none());
        state
            .db
            .with(|c| entities::update_connector(c, "google-test", None, None, Some(false)))
            .unwrap();
        assert!(access_token(&state, &owner).await.is_none());
        state
            .db
            .with(|c| {
                entities::update_connector(c, "google-test", None, None, Some(true))?;
                entities::set_connector_state(
                    c,
                    "google-test",
                    &ConnectorState {
                        paused: Some(Some("test")),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        assert!(access_token(&state, &owner).await.is_none());
        state
            .db
            .with(|c| {
                entities::set_connector_state(
                    c,
                    "google-test",
                    &ConnectorState {
                        paused: Some(None),
                        auth_state: Some("needs_sign_in"),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        assert!(access_token(&state, &owner).await.is_none());
        // Signed in, but the owner turned YouTube off for Iris.
        let mut policy = crate::gateway::policy::Policy::new(Default::default());
        policy.off.insert("youtube".into());
        state
            .db
            .with(|c| {
                entities::set_connector_state(
                    c,
                    "google-test",
                    &ConnectorState {
                        auth_state: Some("ok"),
                        policy: Some(&policy),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        assert!(access_token(&state, &owner).await.is_none());
        policy.off.clear();
        state
            .db
            .with(|c| {
                entities::set_connector_state(
                    c,
                    "google-test",
                    &ConnectorState {
                        policy: Some(&policy),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        assert!(access_token(&state, &owner).await.is_some());
    }

    #[tokio::test]
    async fn feed_refreshes_and_seals_expired_google_tokens() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let provider = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|s| s.trim().parse().unwrap())
                        })
                        .unwrap();
                    if body.len() >= length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.contains("grant_type=refresh_token"));
            assert!(request.contains("refresh_token=test-refresh_token"));
            let body = r#"{"access_token":"refreshed-test-token","expires_in":3600}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let (state, _tmp, owner) = setup();
        connect(&state, 0, Some(endpoint));
        assert_eq!(
            access_token(&state, &owner).await.as_deref(),
            Some("refreshed-test-token")
        );
        provider.join().unwrap();
        let con = state
            .db
            .with(|c| entities::get_connector(c, "google-test"))
            .unwrap()
            .unwrap();
        let oauth = con.config.oauth.as_ref().unwrap();
        let sealed = oauth.access_token.as_ref().unwrap();
        assert_ne!(sealed, "refreshed-test-token");
        assert_eq!(
            state
                .vault
                .open_str(sealed, "connector:google-test:oauth:access_token")
                .unwrap(),
            "refreshed-test-token"
        );
        assert!(
            !con.public_json()
                .to_string()
                .contains("refreshed-test-token")
        );
        // Cached refreshed credentials: no second request to the provider.
        assert_eq!(
            access_token(&state, &owner).await.as_deref(),
            Some("refreshed-test-token")
        );
    }

    #[tokio::test]
    async fn old_google_connections_request_youtube_when_reconnecting() {
        let (state, _tmp, _) = setup();
        connect(&state, now_ms() + 3_600_000, None);
        let started = crate::oauth::start(&state, "google-test", None)
            .await
            .unwrap();
        let query = crate::oauth::parse_query(&started.authorize_url);
        assert!(
            query["scope"]
                .split_whitespace()
                .any(|s| s == "https://www.googleapis.com/auth/youtube.readonly")
        );
        assert!(
            crate::connectors::GOOGLE_SCOPES
                .contains(&"https://www.googleapis.com/auth/youtube.readonly")
        );
    }

    #[tokio::test]
    async fn insufficient_scope_explains_reconnect_only_once_without_pausing_feed() {
        let (state, _tmp, owner) = setup();
        note_reconnect(&state, &owner).await.unwrap();
        note_reconnect(&state, &owner).await.unwrap();
        state
            .db
            .with(|c| {
                let main = db::ensure_main_conversation(c, &owner)?;
                let messages = db::list_messages(c, &main, None, 10)?;
                assert_eq!(messages.len(), 1);
                assert!(messages[0].content.contains("reconnect Google"));
                assert!(!super::super::store::get_state(c, &owner)?.paused);
                Ok(())
            })
            .unwrap();
    }
}
