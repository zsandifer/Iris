//! Gateway tests against a fake connector (a tiny MCP server in shell with
//! harmless tools; nothing is ever really sent).

use super::*;
use crate::config::{Config, Paths};
use std::time::Duration;

/// A fake MCP server: lists six tools, records every call it really runs
/// in `calls.log` and the exact request in `requests.log` (in its private
/// working directory), and echoes the secret it was given through its
/// environment. `read_archive` returns about 1.1 MB.
const FAKE_SERVER: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\)[,}].*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"search_messages","description":"Search messages","inputSchema":{"type":"object"}},{"name":"send_message","description":"Send a message","inputSchema":{"type":"object"}},{"name":"delete_message","inputSchema":{"type":"object"}},{"name":"create_event","inputSchema":{"type":"object"}},{"name":"export_everything","inputSchema":{"type":"object"}},{"name":"read_archive","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      tool=$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
      echo "$tool" >> calls.log
      printf '%s\n' "$line" >> requests.log
      if [ "$tool" = read_archive ]; then
        big=$(head -c 1100000 /dev/zero | tr '\0' a)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$big"
        continue
      fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ok %s secret=%s"}]}}\n' "$id" "$tool" "$FAKE_SECRET" ;;
  esac
done"#;

struct T {
    state: AppState,
    owner: String,
    main: String,
    connector_id: String,
    _tmp: tempfile::TempDir,
}

fn setup(tune: impl FnOnce(&mut Config)) -> T {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_file: tmp.path().join("config.toml"),
        data_dir: tmp.path().join("data"),
    };
    crate::persona::seed(&paths).unwrap();
    let db = db::Db::open(&paths.db_file()).unwrap();
    let mut cfg = Config::default();
    for h in cfg.harness.values_mut() {
        h.enabled = false;
    }
    cfg.push.enabled = false;
    cfg.safety.approval_timeout_secs = 5;
    tune(&mut cfg);
    let state = AppState::new(cfg, paths, db).unwrap();
    let (owner, main) = state
        .db
        .with(|c| {
            let o = db::owner_user_id(c)?.unwrap();
            let m = db::ensure_main_conversation(c, &o)?;
            Ok((o, m))
        })
        .unwrap();
    let id = crate::util::new_id();
    let env = crate::server::seal_connector_secrets(
        &state,
        &id,
        "env",
        &BTreeMap::from([("FAKE_SECRET".to_string(), "vaultsecret42".to_string())]),
    )
    .unwrap();
    let cfg = entities::ConnectorConfig {
        command: Some("/bin/sh".into()),
        args: vec!["-c".into(), FAKE_SERVER.into()],
        env,
        ..Default::default()
    };
    state
        .db
        .with(|c| entities::insert_connector(c, &id, "chat", "Test chat", "stdio", &cfg, true))
        .unwrap();
    T {
        state,
        owner,
        main,
        connector_id: id,
        _tmp: tmp,
    }
}

impl T {
    async fn grant(&self) -> (String, GrantGuard, Vec<McpServer>) {
        let (servers, guard) = grant_for_turn(&self.state, &self.owner, &self.main, "main", "m1")
            .await
            .expect("owner turn gets connectors");
        let McpTransport::Stdio { env, .. } = &servers[0].transport;
        (env[TOKEN_ENV].clone(), guard, servers)
    }

    async fn call(&self, token: &str, tool: &str, args: Value) -> Value {
        handle(
            &self.state,
            &json!({"token": token, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": {"name": tool, "arguments": args}}}),
        )
        .await["result"]
            .clone()
    }

    fn calls_log(&self) -> String {
        std::fs::read_to_string(connector_dir(&self.state, &self.connector_id).join("calls.log"))
            .unwrap_or_default()
    }

    fn last_request(&self) -> Value {
        let log = std::fs::read_to_string(
            connector_dir(&self.state, &self.connector_id).join("requests.log"),
        )
        .unwrap_or_default();
        serde_json::from_str(log.lines().last().unwrap_or("null")).unwrap()
    }

    fn calls(&self) -> Vec<ConnectorCall> {
        self.state
            .db
            .with(|c| entities::list_connector_calls(c, None, None, 100))
            .unwrap()
    }

    /// The approval card the gateway posted (waits for it).
    async fn pending_card(&self) -> db::Message {
        for _ in 0..200 {
            let msgs = self
                .state
                .db
                .with(|c| db::list_messages(c, &self.main, None, 50))
                .unwrap();
            if let Some(m) = msgs.into_iter().find(|m| {
                m.card
                    .as_ref()
                    .is_some_and(|c| c["type"] == "approval" && c["state"] == "pending")
            }) {
                return m;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no approval card was posted");
    }

    /// What `POST /api/messages/{id}/decision` does.
    fn decide(&self, card_id: &str, approve: bool) {
        self.state
            .db
            .with(|c| {
                let m = db::get_message(c, card_id)?.unwrap();
                let mut card = m.card.unwrap();
                card["state"] = if approve { "approved" } else { "denied" }.into();
                db::update_message_card(c, card_id, &card)?;
                Ok(())
            })
            .unwrap();
        decided(&self.state, card_id, approve);
    }

    fn connector(&self) -> Connector {
        self.state
            .db
            .with(|c| entities::get_connector(c, &self.connector_id))
            .unwrap()
            .unwrap()
    }
}

fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn secrets_stay_on_irises_side_and_reads_run_freely() {
    let t = setup(|_| {});
    let (token, _guard, servers) = t.grant().await;
    // The CLI only gets the proxy command and a per-turn token.
    let cli_config = serde_json::to_string(&servers).unwrap();
    assert!(!cli_config.contains("vaultsecret42"));
    assert!(cli_config.contains("mcp-gateway"));
    assert_eq!(servers[0].name, "chat");

    let list = handle(
        &t.state,
        &json!({"token": token, "connector": "chat",
                "request": {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}}),
    )
    .await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 6);
    let send = tools.iter().find(|t| t["name"] == "send_message").unwrap();
    assert!(send["description"].as_str().unwrap().contains("approve"));
    let search = tools
        .iter()
        .find(|t| t["name"] == "search_messages")
        .unwrap();
    assert_eq!(search["description"], "Search messages");

    let r = t
        .call(&token, "search_messages", json!({"query": "dinner"}))
        .await;
    assert_eq!(r["isError"], Value::Null, "{r}");
    // The connector got its secret from the vault; the agent sees only
    // what the tool returns.
    assert_eq!(text(&r), "ok search_messages secret=vaultsecret42");
    let calls = t.calls();
    assert_eq!(calls[0].decision, "auto");
    assert_eq!(calls[0].outcome.as_deref(), Some("ok"));

    // Wrong token or connector: refused.
    let bad = handle(
        &t.state,
        &json!({"token": "nope", "connector": "chat",
                "request": {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}}),
    )
    .await;
    assert_eq!(bad["error"]["code"], -32001);
}

#[tokio::test]
async fn a_send_waits_for_approval_then_runs() {
    let t = setup(|_| {});
    let (token, _guard, _) = t.grant().await;
    let state = t.state.clone();
    let tok = token.clone();
    let call = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "send_message", "arguments": {
                    "to": "Sam <sam@example.com>", "text": "Running late, 10 min"}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    let c = card.card.clone().unwrap();
    assert_eq!(c["preview"]["to"], "sam@example.com");
    assert_eq!(c["preview"]["body"], "Running late, 10 min");
    assert_eq!(c["action_label"], "Send");
    assert_eq!(c["approver_user_id"], t.owner.as_str());
    assert!(c["details"].as_str().unwrap().contains("Running late"));
    // Nothing went out yet.
    assert!(!t.calls_log().contains("send_message"));
    // The call waits on the card (registered right after it is posted).
    for _ in 0..200 {
        if t.state.gateway.pending_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(t.state.gateway.pending_count(), 1);
    t.decide(&card.id, true);
    let r = call.await.unwrap()["result"].clone();
    assert_eq!(text(&r), "ok send_message secret=vaultsecret42");
    assert!(t.calls_log().contains("send_message"));
    // Exactly what was approved reached the connector, unchanged.
    assert_eq!(
        t.last_request()["params"]["arguments"],
        json!({"to": "Sam <sam@example.com>", "text": "Running late, 10 min"})
    );
    let calls = t.calls();
    assert_eq!(calls[0].decision, "approved");
    assert_eq!(calls[0].card_message_id.as_deref(), Some(card.id.as_str()));
    // The recipient is now known (but not trusted).
    let known = t.state.db.with(entities::known_recipients).unwrap();
    assert_eq!(known, ["sam@example.com"]);
}

#[tokio::test]
async fn denial_and_timeout_never_run_the_call() {
    let t = setup(|c| c.safety.approval_timeout_secs = 1);
    let (token, _guard, _) = t.grant().await;
    let (state, tok) = (t.state.clone(), token.clone());
    let call = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "delete_message", "arguments": {"id": "m-1"}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    assert_eq!(card.card.as_ref().unwrap()["action_label"], "Delete");
    t.decide(&card.id, false);
    let r = call.await.unwrap()["result"].clone();
    assert_eq!(r["isError"], true);
    assert!(text(&r).contains("denied"));
    assert!(!t.calls_log().contains("delete_message"));
    assert_eq!(t.calls()[0].decision, "denied");

    // No answer: expired, and the agent is told.
    let r = t
        .call(
            &token,
            "send_message",
            json!({"to": "kim@example.com", "text": "hi"}),
        )
        .await;
    assert!(text(&r).contains("did not answer"), "{r}");
    assert!(!t.calls_log().contains("send_message"));
    let calls = t.calls();
    assert_eq!(calls[0].decision, "expired");
    let msgs = t
        .state
        .db
        .with(|c| db::list_messages(c, &t.main, None, 50))
        .unwrap();
    assert!(msgs.iter().any(|m| {
        m.card
            .as_ref()
            .is_some_and(|c| c["type"] == "approval" && c["state"] == "expired")
    }));
}

#[tokio::test]
async fn trusted_recipients_skip_approval_and_the_send_tripwire_pauses() {
    let t = setup(|c| c.safety.max_sends_per_hour = 2);
    t.state
        .db
        .with(|c| db::set_setting(c, "safety.trusted_recipients", r#"["sam@example.com"]"#))
        .unwrap();
    let (token, _guard, _) = t.grant().await;
    for _ in 0..2 {
        let r = t
            .call(
                &token,
                "send_message",
                json!({"to": "SAM@example.com", "text": "hi"}),
            )
            .await;
        assert_eq!(text(&r), "ok send_message secret=vaultsecret42");
    }
    assert_eq!(t.calls()[0].decision, "trusted");
    // The third send in an hour trips the wire: paused, owner told.
    let r = t
        .call(
            &token,
            "send_message",
            json!({"to": "sam@example.com", "text": "hi"}),
        )
        .await;
    assert!(text(&r).contains("paused"), "{r}");
    assert_eq!(t.calls_log().matches("send_message").count(), 2);
    let con = t.connector();
    assert!(con.paused_reason.unwrap().contains("more than 2 messages"));
    let msgs = t
        .state
        .db
        .with(|c| db::list_messages(c, &t.main, None, 50))
        .unwrap();
    let notice = msgs
        .iter()
        .find(|m| m.card.as_ref().is_some_and(|c| c["type"] == "action"))
        .expect("pause card");
    assert_eq!(
        notice.card.as_ref().unwrap()["action"]["path"],
        format!("/api/connectors/{}/resume", t.connector_id)
    );
    // Reads are refused too while paused.
    let r = t.call(&token, "search_messages", json!({})).await;
    assert!(text(&r).contains("paused"));
    // And the next turn does not get it at all.
    drop(_guard);
    assert!(
        grant_for_turn(&t.state, &t.owner, &t.main, "main", "m2")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn ending_the_turn_cancels_a_waiting_approval() {
    let t = setup(|c| c.safety.approval_timeout_secs = 60);
    let (token, guard, _) = t.grant().await;
    let (state, tok) = (t.state.clone(), token.clone());
    let call = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "export_everything", "arguments": {}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    drop(guard);
    let r = call.await.unwrap()["result"].clone();
    assert!(text(&r).contains("cancelled"), "{r}");
    let m = t
        .state
        .db
        .with(|c| db::get_message(c, &card.id))
        .unwrap()
        .unwrap();
    assert_eq!(m.card.unwrap()["state"], "cancelled");
    // The token died with the turn.
    let r = t.call(&token, "search_messages", json!({})).await;
    assert_eq!(r, Value::Null);
}

#[tokio::test]
async fn pause_and_disconnect_everything() {
    let t = setup(|_| {});
    let (token, _guard, _) = t.grant().await;
    t.state
        .db
        .with(|c| db::set_setting(c, "paused", "1"))
        .unwrap();
    let r = t.call(&token, "search_messages", json!({})).await;
    assert!(text(&r).contains("paused"));
    assert!(
        grant_for_turn(&t.state, &t.owner, &t.main, "main", "m2")
            .await
            .is_none()
    );
    t.state
        .db
        .with(|c| db::delete_setting(c, "paused"))
        .unwrap();
    assert_eq!(disconnect_all(&t.state).await, 1);
    assert!(!t.connector().enabled);
    let r = t.call(&token, "search_messages", json!({})).await;
    assert!(text(&r).contains("turned off"), "{r}");
    assert!(t.calls_log().is_empty());
}

#[tokio::test]
async fn rules_block_and_new_recipients_ask() {
    let t = setup(|_| {});
    let mut p = policy::Policy::default();
    p.tools
        .insert("search_messages".into(), policy::ToolRule::Block);
    let id = t.connector_id.clone();
    t.state
        .db
        .with(|c| {
            entities::set_connector_state(
                c,
                &id,
                &ConnectorState {
                    policy: Some(&p),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let (token, _guard, _) = t.grant().await;
    let con = t.connector();
    let listed = list_tools(&t.state, &con).await.unwrap();
    assert!(!listed.iter().any(|x| x["name"] == "search_messages"));
    let r = t.call(&token, "search_messages", json!({})).await;
    assert!(text(&r).contains("turned this action off"));
    // A calendar invite to someone new asks first.
    let (state, tok) = (t.state.clone(), token.clone());
    let call = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "create_event", "arguments": {
                    "title": "Lunch", "attendees": [{"email": "new@example.com"}]}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    assert!(
        card.card.as_ref().unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("new@example.com")
    );
    t.decide(&card.id, true);
    let r = call.await.unwrap()["result"].clone();
    assert_eq!(text(&r), "ok create_event secret=vaultsecret42");
    // Known now: the next invite to them runs without asking.
    let r = t
        .call(
            &token,
            "create_event",
            json!({"title": "Coffee", "attendees": [{"email": "new@example.com"}]}),
        )
        .await;
    assert_eq!(text(&r), "ok create_event secret=vaultsecret42");
}

#[tokio::test]
async fn restart_expires_old_approval_cards() {
    let t = setup(|_| {});
    let card = entities::approval_card(&entities::ApprovalRequest {
        title: "Send",
        approver_user_id: &t.owner,
        ..Default::default()
    });
    let main = t.main.clone();
    let m = t
        .state
        .db
        .with(|c| entities::insert_card(c, &main, &card, "assistant"))
        .unwrap();
    expire_stale_approvals(&t.state).await;
    let m = t
        .state
        .db
        .with(|c| db::get_message(c, &m.id))
        .unwrap()
        .unwrap();
    assert_eq!(m.card.unwrap()["state"], "expired");
}

#[tokio::test]
async fn turning_a_connector_off_cancels_its_held_calls() {
    let t = setup(|c| c.safety.approval_timeout_secs = 60);
    let (token, _guard, _) = t.grant().await;
    let (state, tok) = (t.state.clone(), token.clone());
    let call = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "send_message", "arguments": {"to": "kim@example.com", "text": "hi"}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    // What `POST /api/connectors/{id}/disconnect` does.
    let id = t.connector_id.clone();
    t.state
        .db
        .with(|c| entities::update_connector(c, &id, None, None, Some(false)))
        .unwrap();
    cancel_approvals(&t.state, Some(&t.connector_id));
    let r = call.await.unwrap()["result"].clone();
    assert!(text(&r).contains("cancelled"), "{r}");
    let m = t
        .state
        .db
        .with(|c| db::get_message(c, &card.id))
        .unwrap()
        .unwrap();
    assert_eq!(m.card.unwrap()["state"], "cancelled");
    assert!(!t.calls_log().contains("send_message"));
}

#[tokio::test]
async fn an_approval_released_after_the_connector_changed_does_not_run() {
    let t = setup(|c| c.safety.approval_timeout_secs = 60);
    let (token, _guard, _) = t.grant().await;
    let (state, tok) = (t.state.clone(), token.clone());
    let call = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "delete_message", "arguments": {"id": "m-1"}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    // Turned off behind the gateway's back; the card is still approvable.
    let id = t.connector_id.clone();
    t.state
        .db
        .with(|c| entities::update_connector(c, &id, None, None, Some(false)))
        .unwrap();
    t.decide(&card.id, true);
    let r = call.await.unwrap()["result"].clone();
    assert!(text(&r).contains("turned off"), "{r}");
    assert!(!t.calls_log().contains("delete_message"));
    assert_eq!(t.calls()[0].decision, "cancelled");
}

#[tokio::test]
async fn a_turn_that_already_ended_cannot_start_a_call() {
    let t = setup(|_| {});
    let (token, guard, _) = t.grant().await;
    let grant = t.state.gateway.grant(&token).unwrap();
    drop(guard);
    let con = t.connector();
    let r = call_tool(&t.state, &grant, &con, "search_messages", &json!({})).await;
    assert!(text(&r).contains("cancelled"), "{r}");
    assert!(t.calls_log().is_empty());
}

#[tokio::test]
async fn concurrent_sends_cannot_exceed_the_tripwire() {
    let t = setup(|c| c.safety.max_sends_per_hour = 2);
    t.state
        .db
        .with(|c| db::set_setting(c, "safety.trusted_recipients", r#"["sam@example.com"]"#))
        .unwrap();
    let (token, _guard, _) = t.grant().await;
    let mut calls = Vec::new();
    for i in 0..5 {
        let (state, tok) = (t.state.clone(), token.clone());
        calls.push(tokio::spawn(async move {
            handle(
                &state,
                &json!({"token": tok, "connector": "chat", "request": {
                    "jsonrpc": "2.0", "id": i, "method": "tools/call",
                    "params": {"name": "send_message", "arguments": {"to": "sam@example.com", "text": "hi"}}}}),
            )
            .await
        }));
    }
    for c in calls {
        c.await.unwrap();
    }
    assert!(
        t.calls_log().matches("send_message").count() <= 2,
        "{}",
        t.calls_log()
    );
    assert!(t.connector().paused_reason.is_some());
}

#[tokio::test]
async fn approvals_released_together_respect_the_tripwire() {
    let t = setup(|c| {
        c.safety.max_sends_per_hour = 1;
        c.safety.approval_timeout_secs = 60;
    });
    let (token, _guard, _) = t.grant().await;
    let mut calls = Vec::new();
    for (i, who) in ["a@example.com", "b@example.com"].into_iter().enumerate() {
        let (state, tok) = (t.state.clone(), token.clone());
        calls.push(tokio::spawn(async move {
            handle(
                &state,
                &json!({"token": tok, "connector": "chat", "request": {
                    "jsonrpc": "2.0", "id": i, "method": "tools/call",
                    "params": {"name": "send_message", "arguments": {"to": who, "text": "hi"}}}}),
            )
            .await
        }));
    }
    // Both cards wait; approve both.
    let mut cards = Vec::new();
    for _ in 0..200 {
        let msgs = t
            .state
            .db
            .with(|c| db::list_messages(c, &t.main, None, 50))
            .unwrap();
        cards = msgs
            .into_iter()
            .filter(|m| m.card.as_ref().is_some_and(|c| c["state"] == "pending"))
            .collect();
        if cards.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(cards.len(), 2);
    for c in &cards {
        t.decide(&c.id, true);
    }
    for c in calls {
        c.await.unwrap();
    }
    assert_eq!(t.calls_log().matches("send_message").count(), 1);
}

#[tokio::test]
async fn connector_diagnostics_never_show_its_secrets() {
    let t = setup(|_| {});
    // A program that prints its secret and dies.
    let env = crate::server::seal_connector_secrets(
        &t.state,
        "leaky",
        "env",
        &BTreeMap::from([("LEAK".to_string(), "hunter2-secret".to_string())]),
    )
    .unwrap();
    let cfg = entities::ConnectorConfig {
        command: Some("/bin/sh".into()),
        args: vec!["-c".into(), "echo \"oops $LEAK\" >&2; exit 3".into()],
        env,
        ..Default::default()
    };
    t.state
        .db
        .with(|c| entities::insert_connector(c, "leaky", "leaky", "", "stdio", &cfg, true))
        .unwrap();
    let con = t
        .state
        .db
        .with(|c| entities::get_connector(c, "leaky"))
        .unwrap()
        .unwrap();
    let err = describe_tools(&t.state, &con).await.unwrap_err();
    assert!(!err.contains("hunter2"), "{err}");
    let detail = t.state.gateway.last_error("leaky").unwrap_or_default();
    assert!(!detail.contains("hunter2"), "{detail}");
    let listed = list_tools(&t.state, &con).await.unwrap_err();
    assert!(!listed.contains("hunter2"), "{listed}");
}

#[test]
fn redacts_secret_values() {
    assert_eq!(
        redact("token=abcd1234 ok", &["abcd1234".into(), "ab".into()]),
        "token=[hidden] ok"
    );
}

#[tokio::test]
async fn the_call_count_tripwire_pauses() {
    let t = setup(|c| c.safety.max_calls_per_hour = 2);
    let (token, _guard, _) = t.grant().await;
    for _ in 0..2 {
        let r = t
            .call(&token, "search_messages", json!({"query": "x"}))
            .await;
        assert_eq!(text(&r), "ok search_messages secret=vaultsecret42");
    }
    let r = t
        .call(&token, "search_messages", json!({"query": "x"}))
        .await;
    assert!(text(&r).contains("used more than 2 times"), "{r}");
    assert_eq!(t.calls_log().matches("search_messages").count(), 2);
    assert!(t.connector().paused_reason.is_some());
}

#[tokio::test]
async fn a_result_crossing_the_data_limit_pauses_right_away() {
    let t = setup(|c| c.safety.max_read_mb_per_hour = 1);
    let (token, _guard, _) = t.grant().await;
    let r = t.call(&token, "read_archive", json!({"page": 1})).await;
    // The result itself is delivered (it was within the limit when asked)...
    assert!(text(&r).len() > 1_000_000);
    let calls = t.calls();
    assert!(calls[0].result_bytes > 1_048_576);
    // ...and the connection is paused at once, with the owner told.
    let con = t.connector();
    assert!(con.paused_reason.unwrap().contains("bulk export"));
    let notice = t
        .state
        .db
        .with(|c| db::list_messages(c, &t.main, None, 50))
        .unwrap()
        .into_iter()
        .any(|m| m.card.is_some_and(|c| c["type"] == "action"));
    assert!(notice);
    let r = t.call(&token, "search_messages", json!({})).await;
    assert!(text(&r).contains("paused"));
    assert_eq!(t.calls_log().matches("search_messages").count(), 0);
}

/// Owner-facing controls, through the real routes, cancel a held call.
#[tokio::test]
async fn safety_routes_cancel_held_calls() {
    use axum::body::Body;
    use tower::ServiceExt;
    for (method, path) in [
        ("POST", "/api/connectors/{id}/disconnect"),
        ("DELETE", "/api/connectors/{id}"),
        ("POST", "/api/safety/disconnect-all"),
        ("PUT", "/api/safety"),
    ] {
        let t = setup(|c| c.safety.approval_timeout_secs = 60);
        let app = crate::server::router(t.state.clone());
        let code = t
            .state
            .db
            .with(|c| Ok(crate::auth::create_pairing_code(c, &t.owner)))
            .unwrap()
            .unwrap()
            .0;
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::post("/api/pair")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"code": code}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let device = serde_json::from_slice::<Value>(&bytes).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        let (token, _guard, _) = t.grant().await;
        let (state, tok) = (t.state.clone(), token.clone());
        let call = tokio::spawn(async move {
            handle(
                &state,
                &json!({"token": tok, "connector": "chat", "request": {
                    "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": "send_message", "arguments": {"to": "kim@example.com", "text": "hi"}}}}),
            )
            .await
        });
        let card = t.pending_card().await;
        let body = if path == "/api/safety" {
            Body::from(json!({"paused": true}).to_string())
        } else {
            Body::empty()
        };
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(path.replace("{id}", &t.connector_id))
                    .header("authorization", format!("Bearer {device}"))
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            res.status().is_success(),
            "{method} {path}: {}",
            res.status()
        );
        let r = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("the held call ends promptly")
            .unwrap()["result"]
            .clone();
        assert!(
            text(&r).contains("cancelled") || text(&r).contains("paused"),
            "{path}: {r}"
        );
        let m = t
            .state
            .db
            .with(|c| db::get_message(c, &card.id))
            .unwrap()
            .unwrap();
        assert_eq!(m.card.unwrap()["state"], "cancelled", "{path}");
        assert!(!t.calls_log().contains("send_message"), "{path}");
    }
}
