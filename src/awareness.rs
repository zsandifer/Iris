//! Service-owned, allowlisted status and card-only setup requests. No secrets or operations.
use crate::{db, entities, harness::AuthState, state::AppState, util::now_ms};
use serde_json::{Value, json};

pub const TOOLS: &[&str] = &[
    "system_status",
    "request_engine_signin",
    "request_engine_install",
    "request_connector_setup",
];

pub fn resolved(card: &Value) -> bool {
    matches!(
        card["state"].as_str(),
        Some(
            "done"
                | "connected"
                | "declined"
                | "approved"
                | "denied"
                | "expired"
                | "cancelled"
                | "saved"
                | "superseded"
        )
    )
}

/// Explicit projection: never serialize engine paths, connector configs, credential metadata,
/// account identities, card bodies, or vault records.
pub async fn status(
    state: &AppState,
    owner: &str,
    conversation: &str,
    engine: &str,
    model: &str,
) -> crate::util::Result<Value> {
    let engines: Vec<_> = state.harness_statuses().into_iter().map(|s| json!({
        "name": s.name, "installed": s.installed,
        "signed_in": match s.auth { AuthState::Ok => Some(true), AuthState::AuthRequired => Some(false), AuthState::Unknown => None },
        "version": s.version,
    })).collect();
    let (uid, conv) = (owner.to_owned(), conversation.to_owned());
    let Some((connectors, logins, wallet_cards, pending, notes, mut google_setup)) = state.db.call(move |c| {
        if db::owner_user_id(c)?.as_deref() != Some(&uid) || db::conversation_member_ids(c, &conv)? != [uid.clone()] {
            return Ok(None);
        }
        let connectors: Vec<_> = entities::list_connectors(c, false)?.into_iter().map(|con| {
            let status = if !con.enabled { "disabled" } else if con.paused_reason.is_some() { "paused" } else if con.auth_state != "ok" { "needs reconnect" } else { "connected" };
            let mut rules = con.policy.mode.describe().to_owned();
            if !con.policy.tools.is_empty() {
                use crate::gateway::policy::ToolRule;
                let counts = con.policy.tools.values().fold([0; 3], |mut counts, rule| {
                    counts[match rule { ToolRule::Allow => 0, ToolRule::Ask => 1, ToolRule::Block => 2 }] += 1;
                    counts
                });
                rules.push_str(&format!("; tool overrides: {} run, {} ask, {} blocked (purchases still ask)", counts[0], counts[1], counts[2]));
            }
            if !con.policy.off.is_empty() {
                let off: Vec<&str> = con.policy.off.iter().map(|s| crate::gateway::google::service(s).map_or(s.as_str(), |x| x.label)).collect();
                rules.push_str(&format!("; turned off by the owner: {}", off.join(", ")));
            }
            json!({"name": con.name, "status": status, "rules": rules})
        }).collect();
        let credentials = entities::list_credentials(c, &uid)?;
        let mut logins: Vec<_> = credentials.iter().filter(|x| x.kind == "login")
            .filter_map(|x| x.metadata["site"].as_str())
            .filter_map(|site| crate::browser::fill::origin_of(&if site.contains("://") { site.to_owned() } else { format!("https://{site}") }).map(|(_, host)| host)).collect();
        logins.sort(); logins.dedup();
        let wallet_cards = credentials.iter().filter(|x| x.kind == "card").count();
        let mut stmt = c.prepare("SELECT json_extract(card, '$.type'), json_extract(card, '$.title') FROM messages
            WHERE kind = 'card' AND status = 'complete'
              AND json_extract(card, '$.type') IN ('engine_signin', 'engine_setup', 'connect', 'connector_reconnect', 'approval', 'action', 'save_login')
              AND COALESCE(json_extract(card, '$.state'), '') NOT IN ('done', 'connected', 'declined', 'approved', 'denied', 'expired', 'cancelled', 'saved')
              AND conversation_id IN (SELECT conversation_id FROM conversation_members WHERE user_id = ?1
                AND (SELECT COUNT(*) FROM conversation_members m WHERE m.conversation_id = conversation_members.conversation_id) = 1)
            ORDER BY created_at DESC LIMIT 8")?;
        let pending = stmt.query_map([&uid], |r| {
            let kind: String = r.get(0)?;
            let title: String = r.get(1)?;
            Ok(json!({"type": kind, "title": crate::util::truncate_chars(&title, 80)}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut stmt = c.prepare("SELECT content FROM messages WHERE conversation_id = ?1 AND kind = 'system_note' ORDER BY created_at DESC LIMIT 8")?;
        let mut notes = stmt.query_map([&conv], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        notes.reverse();
        Ok(Some((connectors, logins, wallet_cards, pending, notes, crate::google_setup::status(c)?)))
    }).await? else { return Ok(json!({"outcome": "not allowed"})); };
    google_setup["authorized_domain"] = json!(crate::google_setup::authorized_domain(
        &state.cfg.public_url
    ));
    let quiet = crate::notify::quiet_hours(state, owner).await;
    Ok(json!({
        "iris_version": env!("CARGO_PKG_VERSION"), "iris_build_id": env!("IRIS_BUILD_ID"), "engines": engines,
        "current_turn": {"engine": engine, "model": model},
        "isolation": if state.cfg.agent.user.is_empty() { "single-user" } else { "split" },
        "connectors": connectors, "saved_logins": logins, "wallet_cards": wallet_cards,
        "settings": {"agent_name": state.agent_name().await, "feed": crate::feed::enabled(state).await,
            "paused": state.is_paused().await, "quiet_hours": quiet.map(|q| json!({"start": q["start"], "end": q["end"]})), "voice_replies": false},
        "pending_cards": pending, "recent_system_notes": notes,
        "google_setup": google_setup,
    }))
}

pub fn insert_note(
    c: &rusqlite::Connection,
    conv: &str,
    content: &str,
) -> rusqlite::Result<db::Message> {
    db::insert_message(
        c,
        &db::NewMessage {
            conversation_id: conv,
            role: "system",
            kind: "system_note",
            content,
            ..Default::default()
        },
    )
}

pub async fn broadcast_created(state: &AppState, message: &db::Message) {
    let conv = message.conversation_id.clone();
    let members = state
        .db
        .call(move |c| db::conversation_member_ids(c, &conv))
        .await
        .unwrap_or_default();
    state.hub.to_users(&members, json!({"type": "message.created", "conversation_id": message.conversation_id, "message": message}));
}

/// Resolve all matching outstanding owner cards, once per conversation, with an outcome note.
pub async fn finish(state: &AppState, kind: &str, target: &str, outcome: &str, note: &str) {
    let (kind, target, outcome, note) = (
        kind.to_owned(),
        target.to_owned(),
        outcome.to_owned(),
        note.to_owned(),
    );
    let result = state
        .db
        .call(move |c| {
            db::write_tx(c, |c| {
                let service = if kind == "connector" {
                    entities::get_connector(c, &target)?.map(|con| con.service)
                } else {
                    None
                };
                let Some(owner) = db::owner_user_id(c)? else {
                    return Ok((vec![], vec![]));
                };
                let mut stmt = c.prepare("SELECT id FROM messages WHERE kind = 'card'
                    AND json_extract(card, '$.type') IN ('engine_signin', 'engine_setup', 'connect', 'connector_reconnect')
                    AND COALESCE(json_extract(card, '$.state'), '') NOT IN ('done', 'connected', 'declined', 'approved', 'denied', 'expired', 'cancelled')")?;
                let ids = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut updates = vec![];
                let mut conversations = std::collections::BTreeSet::new();
                for id in ids {
                    let Some(m) = db::get_message(c, &id)? else {
                        continue;
                    };
                    let Some(mut card) = m.card else {
                        continue;
                    };
                    let matches = match kind.as_str() {
                        "signin" => card["type"] == "engine_signin" && card["harness"] == target,
                        "install" => card["type"] == "engine_setup" && card["harness"] == target,
                        "connector" => {
                            (card["type"] == "connector_reconnect"
                                && card["connector_id"] == target)
                                || (card["type"] == "connect"
                                    && card["service"].as_str() == service.as_deref()
                                    && service.is_some())
                        }
                        _ => false,
                    };
                    if !matches
                        || resolved(&card)
                        || db::conversation_member_ids(c, &m.conversation_id)? != [owner.clone()]
                    {
                        continue;
                    }
                    card["state"] = outcome.clone().into();
                    card["body"] = note.clone().into();
                    for field in ["url", "user_code", "error"] {
                        card[field] = Value::Null;
                    }
                    updates.push(db::update_message_card(c, &id, &card)?);
                    conversations.insert(m.conversation_id);
                }
                // A Google sign-in from Connections also informs the next
                // private turn. Its timestamp makes repeated updates idempotent.
                if kind == "connector" && service.as_deref() == Some("google") && outcome == "done"
                    && let Some(at) = entities::get_connector(c, &target)?.and_then(|c| c.config.oauth.and_then(|o| o.connected_at)) {
                    let key = format!("google.connected_note.{target}");
                    let marker = at.to_string();
                    if db::get_setting(c, &key)?.as_deref() != Some(&marker) {
                        if conversations.is_empty() {
                            let main = db::ensure_main_conversation(c, &owner)?;
                            if db::conversation_member_ids(c, &main)? == [owner.clone()] {
                                conversations.insert(main);
                            }
                        }
                        db::set_setting(c, &key, &marker)?;
                    }
                }
                let notes = conversations
                    .iter()
                    .map(|conv| insert_note(c, conv, &note))
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok((updates, notes))
            })
        })
        .await;
    if let Ok((updates, notes)) = result {
        for m in updates {
            crate::login::broadcast_updated(state, &m).await;
        }
        for m in notes {
            broadcast_created(state, &m).await;
        }
    }
}

pub async fn request(
    state: &AppState,
    owner: &str,
    conv: &str,
    tool: &str,
    args: &Value,
) -> crate::util::Result<Value> {
    let (uid, cid) = (owner.to_owned(), conv.to_owned());
    if !state
        .db
        .call(move |c| {
            Ok(db::owner_user_id(c)?.as_deref() == Some(&uid)
                && db::conversation_member_ids(c, &cid)? == [uid])
        })
        .await?
    {
        return Ok(json!({"outcome": "not allowed"}));
    }
    let (target, card, happened) = if tool == "request_connector_setup" {
        let name = args["name"].as_str().unwrap_or("").to_lowercase();
        let Some(catalog) = crate::connectors::catalog_entry(&name) else {
            return Ok(json!({"outcome": "not allowed"}));
        };
        let n = name.clone();
        let con = state
            .db
            .call(move |c| {
                Ok(entities::list_connectors(c, false)?
                    .into_iter()
                    .find(|x| (x.service == n && n != "custom") || x.name.eq_ignore_ascii_case(&n)))
            })
            .await?;
        let more_access = con
            .as_ref()
            .is_some_and(crate::connectors::google_needs_more_access);
        if con.as_ref().is_some_and(|c| c.active()) && !more_access {
            return Ok(json!({"outcome": "already connected"}));
        }
        if let Some(con) = con
            .as_ref()
            .filter(|c| !c.enabled || c.paused_reason.is_some())
        {
            let status = if !con.enabled { "disabled" } else { "paused" };
            (
                name,
                json!({"type": "action", "title": format!("{} is {status}", catalog.label),
                    "body": format!("I can't use {} because it is {status} by the owner. Review its settings and resume it when you're ready.", catalog.label),
                    "connector_id": con.id, "service": con.service, "state": "pending", "owner_only": true,
                    "action_label": "Resume", "action": {"method": "POST", "path": format!("/api/connectors/{}/resume", con.id)}}),
                if !con.enabled {
                    "connector disabled by owner"
                } else {
                    "connector paused by owner"
                },
            )
        } else {
            let card = if let Some(con) = con.as_ref().filter(|_| more_access) {
                json!({"type": "connector_reconnect", "title": "Add more Google services", "connector_id": con.id, "service": name,
                "body": "Google works as before. Turn on the new Google APIs in your project, then sign in again to add Docs, Sheets, Slides, Forms, Tasks, Contacts and Gmail filters.",
                "auth": "oauth", "state": "needs_reconnect", "owner_only": true})
            } else if let Some(con) = con.filter(|c| c.auth_state != "ok") {
                json!({"type": "connector_reconnect", "title": format!("Reconnect {}", catalog.label), "connector_id": con.id, "service": name,
                "auth": if con.config.oauth.is_some() { "oauth" } else { "secrets" }, "state": "needs_reconnect", "owner_only": true})
            } else {
                json!({"type": "connect", "title": format!("Connect {}", catalog.label), "service": name, "state": "pending", "owner_only": true})
            };
            (name, card, "posted a connect card")
        }
    } else {
        let name = args["engine"].as_str().unwrap_or("");
        let Some(entry) = state.harness(name) else {
            return Ok(json!({"outcome": "not allowed"}));
        };
        let status = entry.status();
        let label = crate::login::engine_label(name);
        if tool == "request_engine_signin" {
            if status.auth == AuthState::Ok {
                return Ok(json!({"outcome": "already signed in"}));
            }
            if !status.installed {
                return Ok(json!({"outcome": "not allowed", "reason": "request an install first"}));
            }
            let flow = entry.harness.login().map(|s| s.flow);
            let options = crate::keys::options(name);
            let api_key = flow.is_none() && !options.is_empty();
            (
                name.to_owned(),
                json!({"type": "engine_signin", "title": format!("Sign in to {label}"), "harness": name,
                    "state": "needs_reconnect", "owner_only": true,
                    "flow": match flow { Some(crate::harness::LoginFlow::CodePaste) => "code_paste", Some(crate::harness::LoginFlow::DeviceCode) => "device_code", None if api_key => "api_key", None => "manual" },
                    "key_options": options,
                "instructions": if flow.is_none() { Some(crate::login::manual_login_hint(state, entry.harness.as_ref())) } else { None },
                }),
                "posted a sign-in card",
            )
        } else if tool == "request_engine_install" {
            if status.installed {
                return Ok(json!({"outcome": "already installed"}));
            }
            (
                name.to_owned(),
                json!({"type": "engine_setup", "title": format!("Install {label}"), "harness": name, "state": "needs_setup", "owner_only": true,
                    "options": [{"harness": name, "label": label, "installed": false, "action": {"method": "POST", "path": format!("/api/engines/{name}/install")}}],
                }),
                "posted an install card",
            )
        } else {
            return Ok(json!({"outcome": "not allowed"}));
        }
    };
    let (uid, conv, tool) = (owner.to_owned(), conv.to_owned(), tool.to_owned());
    let login_state = state.clone();
    let (value, posted) = state
        .db
        .call(move |c| {
            db::write_tx(c, |c| {
                // Recheck membership under the write lock; a joining person revokes private requests.
                if db::owner_user_id(c)?.as_deref() != Some(&uid)
                    || db::conversation_member_ids(c, &conv)? != [uid.clone()]
                {
                    return Ok((json!({"outcome": "not allowed"}), None));
                }
                let mut stmt = c.prepare(
                    "SELECT id FROM messages WHERE conversation_id = ?1 AND kind = 'card'
                        AND json_extract(card, '$.type') = ?2
                        AND COALESCE(json_extract(card, '$.state'), '') NOT IN ('done', 'connected', 'declined', 'approved', 'denied', 'expired', 'cancelled')",
                )?;
                let ids = stmt
                    .query_map(rusqlite::params![conv, card["type"].as_str()], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let messages = ids
                    .iter()
                    .filter_map(|id| db::get_message(c, id).transpose())
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if let Some(m) = messages.iter().find(|m| {
                    m.card.as_ref().is_some_and(|k| {
                        !resolved(k)
                            && k["type"] == card["type"]
                            && (k["harness"] == target || k["service"] == target || (card["connector_id"].is_string() && k["connector_id"] == card["connector_id"]))
                    })
                }) {
                    return Ok((
                        json!({"outcome": if card["type"] == "action" { happened } else { "card already posted" }, "card_id": m.id}),
                        None,
                    ));
                }
                // Global owner budget survives new turns and CLI restarts: at most 6 cards/hour.
                let key = format!("awareness.requests.{uid}");
                let raw = db::get_setting(c, &key)?.unwrap_or_default();
                let mut times: Vec<i64> = serde_json::from_str(&raw).unwrap_or_default();
                times.retain(|at| *at > now_ms() - 3_600_000);
                if times.len() >= 6 {
                    return Ok((
                        json!({"outcome": "not allowed", "reason": "request limit reached"}),
                        None,
                    ));
                }
                times.push(now_ms());
                db::set_setting(c, &key, &json!(times).to_string())?;
                let mut card = card;
                if card["type"] == "engine_signin"
                    && let Some(login) = login_state.logins.status(&target).filter(|s| !s.finished()) {
                    login.apply_to_card(&mut card);
                }
                card["requested_by"] = "system_request".into();
                card["request_tool"] = tool.into();
                let message = entities::insert_card(c, &conv, &card, "assistant")?;
                if card["type"] == "engine_signin" {
                    db::set_setting(c, &format!("signin_card.{target}"), &message.id)?;
                } else if card["type"] == "connector_reconnect"
                    && let Some(id) = card["connector_id"].as_str() {
                    db::set_setting(c, &format!("connector_card.{id}"), &message.id)?;
                }
                Ok((
                    json!({"outcome": happened, "card_id": message.id}),
                    Some(message),
                ))
            })
        })
        .await?;
    if let Some(m) = posted {
        broadcast_created(state, &m).await;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::{ConnectorConfig, ConnectorState, OAuthConfig};

    fn fixture() -> (tempfile::TempDir, AppState, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let state = crate::browser::tools::tests::test_state(tmp.path());
        let (owner, conv) = state
            .db
            .with(|c| {
                let owner = db::owner_user_id(c)?.unwrap();
                let conv = db::ensure_main_conversation(c, &owner)?;
                Ok((owner, conv))
            })
            .unwrap();
        (tmp, state, owner, conv)
    }

    #[tokio::test]
    async fn status_projects_only_non_sensitive_fields() {
        let (_tmp, state, owner, conv) = fixture();
        let key = state.vault.seal_str("vault-key-canary", "key").unwrap();
        let token = state
            .vault
            .seal_str("connector-token-canary", "connector")
            .unwrap();
        let secret = state
            .vault
            .encrypt(b"credential-secret-canary", "credential")
            .unwrap();
        state.db.with(|c| {
            entities::set_engine_key(c, "codex", "CODEX_API_KEY", &key)?;
            entities::insert_connector(c, "mail", "mail", "description-canary", "http", &ConnectorConfig {
                url: Some("https://connector.example/path-canary?token=url-canary".into()),
                env: [("KEY".into(), token.clone())].into(),
                headers: [("Authorization".into(), token.clone())].into(),
                oauth: Some(OAuthConfig { access_token: Some(token.clone()), refresh_token: Some(token.clone()), client_secret: Some(token.clone()), account: Some("account-canary@example.com".into()), ..Default::default() }),
                ..Default::default()
            }, true)?;
            entities::insert_credential(c, "login", &owner, "login", "label-canary", &json!({"site": "https://shop.example/path-canary?token=url-canary", "username": "username-canary"}), &secret)?;
            entities::insert_credential(c, "wallet", &owner, "card", "wallet-label-canary", &json!({"last4": "4242", "cardholder": "holder-canary", "exp_year": 2099}), &secret)?;
            Ok(())
        }).unwrap();
        {
            let mut status = state.harness("codex").unwrap().status.write().unwrap();
            status.installed = true;
            status.auth = AuthState::Ok;
            status.version = Some("1.2.3".into());
            status.path = Some("/path-canary/bin/codex".into());
            status.auth_message = Some("auth-message-canary".into());
        }
        let value = status(&state, &owner, &conv, "codex", "test-model")
            .await
            .unwrap();
        assert_eq!(value["saved_logins"], json!(["shop.example"]));
        assert_eq!(value["iris_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["iris_build_id"], env!("IRIS_BUILD_ID"));
        assert_eq!(value["wallet_cards"], 1);
        assert_eq!(value["current_turn"]["engine"], "codex");
        let raw = value.to_string();
        for forbidden in ["canary", "4242", "2099", key.as_str(), token.as_str()] {
            assert!(!raw.contains(forbidden), "leaked {forbidden}: {raw}");
        }
        assert_eq!(
            status(&state, "not-owner", &conv, "codex", "test-model")
                .await
                .unwrap()["outcome"],
            "not allowed"
        );
    }

    #[tokio::test]
    async fn requests_only_post_cards_and_deduplicate_concurrently() {
        let (_tmp, state, owner, conv) = fixture();
        let args = json!({"engine": "codex"});
        let (a, b) = tokio::join!(
            request(&state, &owner, &conv, "request_engine_install", &args),
            request(&state, &owner, &conv, "request_engine_install", &args)
        );
        assert_eq!(a.unwrap()["card_id"], b.unwrap()["card_id"]);
        assert!(state.engine_ops.get("codex").is_none());
        assert!(state.logins.status("codex").is_none());
        assert_eq!(
            state
                .db
                .with(|c| entities::list_connectors(c, false))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            state
                .db
                .with(|c| db::list_messages(c, &conv, None, 100))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            request(
                &state,
                &owner,
                &conv,
                "request_engine_install",
                &json!({"engine": "shell"})
            )
            .await
            .unwrap()["outcome"],
            "not allowed"
        );
        {
            let mut s = state.harness("claude").unwrap().status.write().unwrap();
            s.installed = true;
            s.auth = AuthState::Ok;
        }
        assert_eq!(
            request(
                &state,
                &owner,
                &conv,
                "request_engine_signin",
                &json!({"engine": "claude"})
            )
            .await
            .unwrap()["outcome"],
            "already signed in"
        );
        let args = json!({"name": "google"});
        let a = request(&state, &owner, &conv, "request_connector_setup", &args)
            .await
            .unwrap();
        let b = request(&state, &owner, &conv, "request_connector_setup", &args)
            .await
            .unwrap();
        assert_eq!(a["card_id"], b["card_id"]);
        assert!(
            state
                .db
                .with(|c| entities::list_connectors(c, false))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn request_budget_survives_conversations_and_turns() {
        let (_tmp, state, owner, conv) = fixture();
        for _ in 0..6 {
            let response = request(
                &state,
                &owner,
                &conv,
                "request_engine_install",
                &json!({"engine": "codex"}),
            )
            .await
            .unwrap();
            assert_eq!(response["outcome"], "posted an install card");
            finish(
                &state,
                "install",
                "codex",
                "declined",
                "You declined installation.",
            )
            .await;
        }
        let other = state
            .db
            .with(|c| db::create_conversation(c, &owner, "side", "Other", None))
            .unwrap();
        assert_eq!(
            request(
                &state,
                &owner,
                &other.id,
                "request_engine_install",
                &json!({"engine": "codex"})
            )
            .await
            .unwrap()["reason"],
            "request limit reached"
        );
    }

    #[tokio::test]
    async fn completions_resolve_all_stale_cards_and_note_each_conversation_once() {
        let (_tmp, state, owner, conv) = fixture();
        state
            .harness("codex")
            .unwrap()
            .status
            .write()
            .unwrap()
            .installed = true;
        request(
            &state,
            &owner,
            &conv,
            "request_engine_signin",
            &json!({"engine": "codex"}),
        )
        .await
        .unwrap();
        let other = state.db.with(|c| {
            let other = db::create_conversation(c, &owner, "side", "Other", None)?;
            for _ in 0..2 { entities::insert_card(c, &other.id, &json!({"type": "engine_signin", "harness": "codex", "title": "Reconnect", "state": "needs_reconnect", "url": "https://signin.example", "user_code": "one-time-canary"}), "assistant")?; }
            Ok(other)
        }).unwrap();
        state.note_auth("codex", AuthState::Ok, None).await;
        crate::login::on_signed_in(&state, "codex").await;
        for cid in [&conv, &other.id] {
            let messages = state
                .db
                .with(|c| db::list_messages(c, cid, None, 100))
                .unwrap();
            assert_eq!(
                messages.iter().filter(|m| m.kind == "system_note").count(),
                1
            );
            for m in messages.iter().filter(|m| m.kind == "card") {
                let card = m.card.as_ref().unwrap();
                assert_eq!(card["state"], "done");
                assert!(card["url"].is_null());
                assert!(card["user_code"].is_null());
            }
        }
        let value = status(&state, &owner, &conv, "codex", "test-model")
            .await
            .unwrap();
        assert!(value["pending_cards"].as_array().unwrap().is_empty());
        assert!(
            value["recent_system_notes"][0]
                .as_str()
                .unwrap()
                .contains("signed in ✓")
        );
    }

    #[tokio::test]
    async fn completions_leave_shared_and_guest_cards_untouched() {
        let (_tmp, state, owner, conv) = fixture();
        let (shared, guest, cards) = state.db.with(|c| {
            c.execute("INSERT INTO users (id, display_name, is_owner, created_at) VALUES ('guest', 'Guest', 0, 1)", [])?;
            let shared = db::create_conversation(c, &owner, "side", "Shared", None)?;
            c.execute("INSERT INTO conversation_members (conversation_id, user_id, role, joined_at) VALUES (?1, 'guest', 'member', 1)", [&shared.id])?;
            let guest = db::create_conversation(c, "guest", "side", "Guest", None)?;
            entities::insert_connector(c, "connector-id", "google", "", "http", &ConnectorConfig::default(), true)?;
            entities::set_connector_state(c, "connector-id", &ConnectorState { service: Some("google"), ..Default::default() })?;
            let mut cards = vec![];
            for cid in [&conv, &shared.id, &guest.id] {
                for card in [
                    json!({"type": "engine_signin", "harness": "codex", "state": "needs_reconnect"}),
                    json!({"type": "engine_setup", "harness": "codex", "state": "needs_setup"}),
                    json!({"type": "connect", "service": "google", "state": "pending"}),
                    json!({"type": "connector_reconnect", "connector_id": "connector-id", "state": "needs_reconnect"}),
                ] {
                    cards.push(entities::insert_card(c, cid, &card, "assistant")?);
                }
            }
            Ok((shared, guest, cards))
        }).unwrap();
        let mut events = state.hub.subscribe();
        for (kind, target) in [
            ("signin", "codex"),
            ("install", "codex"),
            ("connector", "connector-id"),
        ] {
            finish(&state, kind, target, "done", "Connection outcome.").await;
            // Completion is idempotent.
            finish(&state, kind, target, "done", "Connection outcome.").await;
        }
        state
            .db
            .with(|c| {
                for before in &cards {
                    let after = db::get_message(c, &before.id)?.unwrap();
                    if before.conversation_id == conv {
                        assert_eq!(after.card.unwrap()["state"], "done");
                    } else {
                        assert_eq!(after.card, before.card);
                    }
                }
                for cid in [&shared.id, &guest.id] {
                    assert!(
                        db::list_messages(c, cid, None, 100)?
                            .iter()
                            .all(|m| m.kind != "system_note")
                    );
                }
                assert_eq!(
                    db::list_messages(c, &conv, None, 100)?
                        .iter()
                        .filter(|m| m.kind == "system_note")
                        .count(),
                    3
                );
                Ok(())
            })
            .unwrap();
        let mut count = 0;
        while let Ok(event) = events.try_recv() {
            assert!(!event.audience.includes("guest"));
            let value: Value = serde_json::from_str(&event.json).unwrap();
            assert_eq!(value["conversation_id"], conv);
            count += 1;
        }
        assert_eq!(count, 7);
    }

    #[tokio::test]
    async fn paused_and_disabled_connectors_offer_resume_without_claiming_connected() {
        for disabled in [false, true] {
            let (_tmp, state, owner, conv) = fixture();
            state
                .db
                .with(|c| {
                    entities::insert_connector(
                        c,
                        "connector-id",
                        "google",
                        "",
                        "http",
                        &ConnectorConfig::default(),
                        !disabled,
                    )?;
                    entities::set_connector_state(
                        c,
                        "connector-id",
                        &ConnectorState {
                            service: Some("google"),
                            auth_state: Some("ok"),
                            paused: (!disabled).then_some(Some("owner paused it")),
                            ..Default::default()
                        },
                    )?;
                    entities::insert_card(
                        c,
                        &conv,
                        &json!({"type": "connect", "service": "google", "state": "pending"}),
                        "assistant",
                    )?;
                    Ok(())
                })
                .unwrap();
            let args = json!({"name": "google"});
            let response = request(&state, &owner, &conv, "request_connector_setup", &args)
                .await
                .unwrap();
            assert_eq!(
                response["outcome"],
                if disabled {
                    "connector disabled by owner"
                } else {
                    "connector paused by owner"
                }
            );
            let repeated = request(&state, &owner, &conv, "request_connector_setup", &args)
                .await
                .unwrap();
            assert_eq!(response, repeated);
            let card = state
                .db
                .with(|c| db::get_message(c, response["card_id"].as_str().unwrap()))
                .unwrap()
                .unwrap()
                .card
                .unwrap();
            assert_eq!(card["type"], "action");
            assert_eq!(card["action_label"], "Resume");
            assert_eq!(card["action"]["method"], "POST");
            assert_eq!(
                card["action"]["path"],
                "/api/connectors/connector-id/resume"
            );
            assert_eq!(card["owner_only"], true);
            assert!(card["body"].as_str().unwrap().contains(if disabled {
                "disabled by the owner"
            } else {
                "paused by the owner"
            }));
            crate::login::on_connector_updated(&state, "connector-id").await;
            state
                .db
                .with(|c| {
                    let messages = db::list_messages(c, &conv, None, 100)?;
                    assert_eq!(messages.len(), 2);
                    assert!(
                        messages
                            .iter()
                            .all(|m| m.card.as_ref().unwrap()["state"] == "pending")
                    );
                    assert!(
                        !entities::get_connector(c, "connector-id")?
                            .unwrap()
                            .active()
                    );
                    Ok(())
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn an_installed_engine_resolves_its_install_request_with_a_note() {
        let (_tmp, state, owner, conv) = fixture();
        request(
            &state,
            &owner,
            &conv,
            "request_engine_install",
            &json!({"engine": "codex"}),
        )
        .await
        .unwrap();
        state
            .harness("codex")
            .unwrap()
            .status
            .write()
            .unwrap()
            .installed = true;
        state.note_auth("codex", AuthState::Ok, None).await;
        let messages = state
            .db
            .with(|c| db::list_messages(c, &conv, None, 100))
            .unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m.kind == "system_note" && m.content.contains("installed ✓"))
        );
        assert!(
            messages
                .iter()
                .filter(|m| m.kind == "card")
                .all(|m| m.card.as_ref().unwrap()["state"] == "done")
        );
    }

    #[tokio::test]
    async fn connector_success_resolves_connect_and_reconnect_cards() {
        let (_tmp, state, owner, conv) = fixture();
        request(
            &state,
            &owner,
            &conv,
            "request_connector_setup",
            &json!({"name": "google"}),
        )
        .await
        .unwrap();
        state
            .db
            .with(|c| {
                entities::insert_connector(
                    c,
                    "google-id",
                    "google",
                    "",
                    "http",
                    &ConnectorConfig::default(),
                    true,
                )?;
                entities::set_connector_state(
                    c,
                    "google-id",
                    &ConnectorState {
                        service: Some("google"),
                        auth_state: Some("needs_reconnect"),
                        ..Default::default()
                    },
                )?;
                Ok(())
            })
            .unwrap();
        let response = request(
            &state,
            &owner,
            &conv,
            "request_connector_setup",
            &json!({"name": "google"}),
        )
        .await
        .unwrap();
        assert_eq!(response["outcome"], "posted a connect card");
        crate::login::on_connector_updated(&state, "google-id").await;
        assert!(
            status(&state, &owner, &conv, "codex", "test-model")
                .await
                .unwrap()["recent_system_notes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        state
            .db
            .with(|c| {
                entities::set_connector_state(
                    c,
                    "google-id",
                    &ConnectorState {
                        auth_state: Some("ok"),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        crate::login::on_connector_updated(&state, "google-id").await;
        let messages = state
            .db
            .with(|c| db::list_messages(c, &conv, None, 100))
            .unwrap();
        assert!(
            messages
                .iter()
                .filter(|m| m.kind == "card")
                .all(|m| m.card.as_ref().unwrap()["state"] == "done")
        );
        assert!(
            messages
                .iter()
                .any(|m| m.kind == "system_note" && m.content.contains("connected ✓"))
        );
    }

    #[tokio::test]
    async fn turn_token_expires_and_a_join_revokes_status_and_requests() {
        let (_tmp, state, owner, conv) = fixture();
        let (_, guard) = crate::gateway::grant_for_turn(&state, &owner, &conv, "main", "turn")
            .await
            .unwrap();
        let token = guard.status_token("codex", "test-model");
        let envelope = |tool: &str| json!({"token": token, "awareness": true, "request": {"id": 1, "method": "tools/call", "params": {"name": tool, "arguments": {"engine": "codex"}}}});
        let value = crate::gateway::handle(&state, &envelope("system_status")).await;
        assert_eq!(value["result"]["current_turn"]["model"], "test-model");
        state
            .db
            .with(|c| {
                c.execute("INSERT INTO users (id, display_name, is_owner, created_at) VALUES ('guest', 'Guest', 0, 1)", [])?;
                c.execute("INSERT INTO conversation_members (conversation_id, user_id, role, joined_at) VALUES (?1, 'guest', 'member', ?2)", rusqlite::params![conv, db::join_timestamp(c, &conv)?])?;
                Ok(())
            })
            .unwrap();
        for tool in TOOLS {
            assert!(
                crate::gateway::handle(&state, &envelope(tool))
                    .await
                    .get("error")
                    .is_some()
            );
        }
        assert_eq!(
            request(
                &state,
                &owner,
                &conv,
                "request_engine_install",
                &json!({"engine": "codex"})
            )
            .await
            .unwrap()["outcome"],
            "not allowed"
        );
        drop(guard);
        assert!(
            crate::gateway::handle(&state, &envelope("system_status"))
                .await
                .get("error")
                .is_some()
        );
    }
}
