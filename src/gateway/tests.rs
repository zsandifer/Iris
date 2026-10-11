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
      case "$line" in *'"fail":true'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"isError":true,"content":[{"type":"text","text":"failed with key %s"}]}}\n' "$id" "$FAKE_SECRET"
        continue ;;
      esac
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
async fn security_regression_discovery_is_scrubbed_before_caching() {
    for owner_first in [false, true] {
        let t = setup(|_| {});
        let mut con = t.connector();
        con.config.args[1] = FAKE_SERVER.replace(
            "*'\"method\":\"tools/list\"'*)",
            r#"*'"method":"tools/list"'*)
              printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"search_%s","description":"secret %s","inputSchema":{"type":"object","properties":{"key":{"enum":["%s"]}}},"annotations":{"readOnlyHint":true,"title":"%s"}}]}}\n' "$id" "$FAKE_SECRET" "$FAKE_SECRET" "$FAKE_SECRET" "$FAKE_SECRET"
              continue;"#,
        );
        let cli_tools;
        let owner_tools;
        if owner_first {
            owner_tools = describe_tools(&t.state, &con).await.unwrap();
            cli_tools = list_tools(&t.state, &con).await.unwrap();
        } else {
            cli_tools = list_tools(&t.state, &con).await.unwrap();
            owner_tools = describe_tools(&t.state, &con).await.unwrap();
        }
        let cached = cached_tools(&t.state, &con).unwrap();
        assert_eq!(cached[0]["name"], "search_[hidden]");
        assert_eq!(cached[0]["description"], "secret [hidden]");
        assert_eq!(
            cached[0]["inputSchema"]["properties"]["key"]["enum"][0],
            "[hidden]"
        );
        assert_eq!(cached[0]["annotations"]["title"], "[hidden]");
        for tools in [cli_tools, owner_tools, cached] {
            let encoded = serde_json::to_string(&tools).unwrap();
            assert!(!encoded.contains("vaultsecret42"), "{encoded}");
            assert!(encoded.contains("[hidden]"), "{encoded}");
        }
        stop(&t.state, &con.id).await;
    }
}

#[test]
fn security_regression_header_credentials_are_redacted_separately() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    let t = setup(|_| {});
    for (header, value, fragments) in [
        (
            "Authorization",
            "Bearer github_pat_synthetic42".to_string(),
            vec!["github_pat_synthetic42"],
        ),
        (
            "authorization",
            "bEaReR github_pat_synthetic42".to_string(),
            vec!["github_pat_synthetic42"],
        ),
        (
            "AUTHORIZATION",
            "tOkEn synthetic_token42".to_string(),
            vec!["synthetic_token42"],
        ),
        (
            "Authorization",
            format!("bAsIc {}", STANDARD.encode("user:synthetic:password42")),
            vec!["synthetic:password42"],
        ),
        (
            "Authorization",
            "Basic invalid_base64!".to_string(),
            vec!["invalid_base64!"],
        ),
    ] {
        let mut con = t.connector();
        con.transport = "http".into();
        con.config.command = None;
        con.config.url = Some("https://connector.example.com/mcp".into());
        con.config.headers = crate::server::seal_connector_secrets(
            &t.state,
            &con.id,
            "headers",
            &BTreeMap::from([(header.into(), value.clone())]),
        )
        .unwrap();
        let (launch, secrets) = launch_for(&t.state, &con, None).unwrap();
        let Launch::Http { headers, .. } = launch else {
            panic!("expected HTTP launch")
        };
        assert_eq!(headers[header], value);
        assert_eq!(redact(&value, &secrets), "[hidden]");
        let credential = value.split_once(' ').unwrap().1;
        assert_eq!(redact(credential, &secrets), "[hidden]");
        for fragment in fragments {
            assert_eq!(redact(fragment, &secrets), "[hidden]", "{header}");
        }
    }
    // Short pieces must not redact unrelated text, but full headers stay hidden.
    for value in [
        "Bearer short42".to_string(),
        format!("Basic {}", STANDARD.encode("user:short42")),
    ] {
        let mut con = t.connector();
        con.config.headers = crate::server::seal_connector_secrets(
            &t.state,
            &con.id,
            "headers",
            &BTreeMap::from([("Authorization".into(), value.clone())]),
        )
        .unwrap();
        let (_, secrets) = launch_for(&t.state, &con, None).unwrap();
        assert_eq!(redact("short42", &secrets), "short42");
        assert_eq!(redact(&value, &secrets), "[hidden]");
    }
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
    // The connector got its secret from the vault; even successful
    // results are scrubbed before the agent sees them.
    assert_eq!(text(&r), "ok search_messages secret=[hidden]");
    let calls = t.calls();
    assert_eq!(calls[0].decision, "auto");
    assert_eq!(calls[0].outcome.as_deref(), Some("ok"));
    // A tool error that echoes the connector's configuration does not
    // hand its secret to the agent.
    let r = t
        .call(
            &token,
            "search_messages",
            json!({"query": "x", "fail": true}),
        )
        .await;
    assert_eq!(r["isError"], true, "{r}");
    assert_eq!(text(&r), "failed with key [hidden]");

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
    assert_eq!(text(&r), "ok send_message secret=[hidden]");
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
        assert_eq!(text(&r), "ok send_message secret=[hidden]");
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
    // Reads are refused too while paused, and the refusal is in the
    // activity log.
    let r = t.call(&token, "search_messages", json!({})).await;
    assert!(text(&r).contains("paused"));
    let last = t
        .calls()
        .into_iter()
        .find(|c| c.tool == "search_messages")
        .unwrap();
    assert_eq!(last.decision, "blocked");
    assert!(last.reason.contains("paused"), "{}", last.reason);
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

/// Hold a send for approval; returns the call and its card.
async fn held_send(t: &T, token: &str) -> (tokio::task::JoinHandle<Value>, db::Message) {
    let (state, tok) = (t.state.clone(), token.to_string());
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
    (call, card)
}

#[tokio::test]
async fn stopping_the_turn_cancels_its_held_calls_right_away() {
    let t = setup(|c| c.safety.approval_timeout_secs = 60);
    // The grant outlives Stop while the CLI exits; Stop must still end it.
    let (token, _guard, _) = t.grant().await;
    let (call, card) = held_send(&t, &token).await;
    crate::turn::cancel(&t.state, &t.main);
    let r = call.await.unwrap()["result"].clone();
    assert!(text(&r).contains("cancelled"), "{r}");
    t.decide(&card.id, true);
    assert!(!t.calls_log().contains("send_message"));
    let r = t.call(&token, "search_messages", json!({})).await;
    assert!(text(&r).contains("cancelled"), "{r}");
}

#[tokio::test]
async fn a_chat_that_stops_being_private_loses_connectors() {
    let t = setup(|c| c.safety.approval_timeout_secs = 60);
    let (token, _guard, _) = t.grant().await;
    let (call, card) = held_send(&t, &token).await;
    // Someone joins (written directly, so only the dispatch check can
    // notice), then the owner approves.
    let main = t.main.clone();
    t.state
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO users (id, display_name, created_at) VALUES ('guest', 'Guest', 0)",
                [],
            )?;
            c.execute(
                "INSERT INTO conversation_members (conversation_id, user_id, role, joined_at)
                 VALUES (?1, 'guest', 'member', 0)",
                [&main],
            )?;
            Ok(())
        })
        .unwrap();
    t.decide(&card.id, true);
    let r = call.await.unwrap()["result"].clone();
    assert!(text(&r).contains("joined"), "{r}");
    assert!(!t.calls_log().contains("send_message"));
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
    assert_eq!(text(&r), "ok create_event secret=[hidden]");
    // Known now: the next invite to them runs without asking.
    let r = t
        .call(
            &token,
            "create_event",
            json!({"title": "Coffee", "attendees": [{"email": "new@example.com"}]}),
        )
        .await;
    assert_eq!(text(&r), "ok create_event secret=[hidden]");
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
async fn auto_and_trusted_calls_cannot_dispatch_to_a_changed_target() {
    for tool in ["search_messages", "send_message"] {
        let t = setup(|_| {});
        t.state
            .db
            .with(|c| db::set_setting(c, "safety.trusted_recipients", r#"["friend@example.com"]"#))
            .unwrap();
        let (token, _guard, _) = t.grant().await;
        let grant = t.state.gateway.grant(&token).unwrap();
        let con = t.connector();
        // Discovery describes the old server. Retarget before dispatch;
        // the new server exports the same tool names.
        list_tools(&t.state, &con).await.unwrap();
        let mut changed = con.config.clone();
        changed.args[1].push_str("\n# different target");
        t.state
            .db
            .with(|c| entities::update_connector(c, &con.id, None, Some(&changed), None))
            .unwrap();
        let result = call_tool(
            &t.state,
            &grant,
            &con,
            tool,
            &json!({"to": "friend@example.com"}),
        )
        .await;
        assert_eq!(result["isError"], true, "{result}");
        assert!(text(&result).contains("changed"), "{result}");
        assert!(t.calls_log().is_empty());
        assert_eq!(t.calls()[0].decision, "cancelled");
        stop_all(&t.state).await;
    }
}

#[tokio::test]
async fn dispatch_checks_the_target_again_after_startup() {
    let t = setup(|_| {});
    let (token, _guard, _) = t.grant().await;
    let grant = t.state.gateway.grant(&token).unwrap();
    let con = t.connector();
    let gate = Gate {
        tool: "search_messages",
        category: Category::Read,
        recipients: &[],
        decided_by: "auto",
        target: target(&con),
    };
    assert!(
        gate_refusal(&t.state, &grant, &con.id, &gate, true)
            .await
            .is_none()
    );
    let mut changed = con.config.clone();
    changed.args[1].push_str("\n# target changed before startup");
    t.state
        .db
        .with(|c| entities::update_connector(c, &con.id, None, Some(&changed), None))
        .unwrap();
    assert!(
        forward(&t.state, &grant, &con.id, &gate, &json!({}))
            .await
            .is_err()
    );
    assert!(t.calls_log().is_empty());
}

#[test]
fn call_targets_bind_oauth_settings_but_survive_token_refresh() {
    let t = setup(|_| {});
    let mut con = t.connector();
    con.config.oauth = Some(entities::OAuthConfig {
        client_id: Some("client".into()),
        token_endpoint: Some("https://auth.example/token".into()),
        ..Default::default()
    });
    let before = target(&con);
    let o = con.config.oauth.as_mut().unwrap();
    o.access_token = Some("rotated-access-ciphertext".into());
    o.refresh_token = Some("rotated-refresh-ciphertext".into());
    o.expires_at = Some(1000);
    assert_eq!(target(&con), before);
    con.config.oauth.as_mut().unwrap().token_endpoint = Some("https://other.example/token".into());
    assert_ne!(target(&con), before);
}

#[tokio::test]
async fn credentials_are_never_attached_to_nonlocal_http() {
    let t = setup(|_| {});
    let mut con = t.connector();
    con.transport = "http".into();
    con.config.command = None;
    con.config.url = Some("http://connector.example.com/mcp".into());
    assert!(launch_for(&t.state, &con, Some("synthetic-token")).is_err());
    con.config.headers = crate::server::seal_connector_secrets(
        &t.state,
        &con.id,
        "headers",
        &BTreeMap::from([("X-Api-Key".into(), "synthetic-key".into())]),
    )
    .unwrap();
    assert!(launch_for(&t.state, &con, None).is_err());
    con.config.oauth = Some(entities::OAuthConfig::default());
    assert!(crate::oauth::bearer(&t.state, &con).await.is_err());
    for url in [
        "https://connector.example.com/mcp",
        "http://127.0.0.1:7999/mcp",
        "http://[::1]:7999/mcp",
        "http://localhost:7999/mcp",
    ] {
        con.config.url = Some(url.into());
        assert!(
            launch_for(&t.state, &con, Some("synthetic-token")).is_ok(),
            "{url}"
        );
    }
    con.config.command = Some("local-server".into());
    con.config.url = Some("http://127.0.0.1:{port}/mcp".into());
    assert!(launch_for(&t.state, &con, Some("synthetic-token")).is_ok());
    con.config.url = Some("http://remote.example:{port}/mcp".into());
    assert!(launch_for(&t.state, &con, Some("synthetic-token")).is_err());
    con.config.url = Some("http://127.0.0.{port}/mcp".into());
    assert!(launch_for(&t.state, &con, Some("synthetic-token")).is_err());
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
        assert_eq!(text(&r), "ok search_messages secret=[hidden]");
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

#[tokio::test]
async fn a_card_is_never_posted_once_someone_joined_the_chat() {
    let t = setup(|_| {});
    let (token, _guard, _) = t.grant().await;
    let grant = t
        .state
        .gateway
        .grants
        .lock()
        .unwrap()
        .get(&token)
        .unwrap()
        .clone();
    // A member joins after the call passed its checks, before the card.
    let main = t.main.clone();
    t.state
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO users (id, display_name, created_at) VALUES ('guest', 'Guest', 0)",
                [],
            )?;
            c.execute(
                "INSERT INTO conversation_members (conversation_id, user_id, role, joined_at)
                 VALUES (?1, 'guest', 'member', 0)",
                [&main],
            )?;
            Ok(())
        })
        .unwrap();
    let con = t.connector();
    let args = json!({"to": "sam@example.com", "text": "secret plans"});
    let p = policy::preview("Chat", "send_message", Category::Send, &args);
    let call = ConnectorCall::default();
    let mut events = t.state.hub.subscribe();
    let answer = ask(&t.state, &grant, &con, &call, &p, Category::Send).await;
    assert!(events.try_recv().is_err(), "no card was broadcast");
    assert!(matches!(answer, Answer::Cancelled));
    let msgs = t
        .state
        .db
        .with(|c| db::list_messages(c, &t.main, None, 50))
        .unwrap();
    assert!(msgs.iter().all(|m| m.card.is_none()), "no card was posted");
}

#[tokio::test]
async fn unverified_tools_require_approval_even_with_an_allow_override() {
    for discovery_fails in [false, true] {
        for allow_override in [false, true] {
            let t = setup(|_| {});
            let tool = if discovery_fails {
                "search_messages"
            } else {
                "search_hidden"
            };
            let mut con = t.connector();
            if discovery_fails {
                con.config.args[1] = FAKE_SERVER.replace(
                    "*'\"method\":\"tools/list\"'*)",
                    "*'\"method\":\"tools/list\"'*) printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"error\":{\"code\":-32603,\"message\":\"discovery failed\"}}\n' \"$id\"; continue;",
                );
            }
            if allow_override {
                con.policy
                    .tools
                    .insert(tool.into(), policy::ToolRule::Allow);
            }
            t.state
                .db
                .with(|c| {
                    c.execute(
                        "UPDATE connectors SET config = ?2, policy = ?3 WHERE id = ?1",
                        rusqlite::params![
                            con.id,
                            serde_json::to_string(&con.config).unwrap(),
                            serde_json::to_string(&con.policy).unwrap()
                        ],
                    )?;
                    Ok(())
                })
                .unwrap();
            let (token, _guard, _) = t.grant().await;
            let state = t.state.clone();
            let call = tokio::spawn(async move {
                handle(
                    &state,
                    &json!({"token": token, "connector": "chat", "request": {
                    "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": {"name": tool, "arguments": {"query": "x"}}}}),
                )
                .await
            });
            let card = t.pending_card().await;
            assert!(t.calls_log().is_empty());
            t.decide(&card.id, false);
            let r = call.await.unwrap()["result"].clone();
            assert_eq!(r["isError"], true);
            assert!(t.calls_log().is_empty());
        }
    }
}

#[tokio::test]
async fn a_cancelled_grant_posts_no_approval_card() {
    let t = setup(|_| {});
    let (token, _guard, _) = t.grant().await;
    let grant = t.state.gateway.grant(&token).unwrap();
    grant.cancel.send_replace(true);
    let mut events = t.state.hub.subscribe();
    let args = json!({"to": "sam@example.com", "text": "private request"});
    let p = policy::preview("Chat", "send_message", Category::Send, &args);
    let answer = ask(
        &t.state,
        &grant,
        &t.connector(),
        &ConnectorCall::default(),
        &p,
        Category::Send,
    )
    .await;
    assert!(matches!(answer, Answer::Cancelled));
    assert!(events.try_recv().is_err());
    assert!(
        t.state
            .db
            .with(|c| db::list_messages(c, &t.main, None, 50))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn joining_during_discovery_posts_no_private_request() {
    let t = setup(|_| {});
    let mut con = t.connector();
    con.config.args[1] = FAKE_SERVER.replace(
        "*'\"method\":\"tools/list\"'*)",
        "*'\"method\":\"tools/list\"'*) touch discovering; while [ ! -e discovered ]; do sleep 0.01; done;",
    );
    t.state
        .db
        .with(|c| {
            c.execute(
                "UPDATE connectors SET config = ?2 WHERE id = ?1",
                rusqlite::params![con.id, serde_json::to_string(&con.config).unwrap()],
            )?;
            Ok(())
        })
        .unwrap();
    let (token, _guard, _) = t.grant().await;
    let state = t.state.clone();
    let call = tokio::spawn(async move {
        handle(&state, &json!({"token": token, "connector": "chat", "request": {
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "send_message", "arguments": {"to": "sam@example.com", "text": "private request"}}}})).await
    });
    let dir = connector_dir(&t.state, &t.connector_id);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !dir.join("discovering").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the call reached discovery");
    let mut events = t.state.hub.subscribe();
    t.state
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO users (id, display_name, created_at) VALUES ('guest', 'Guest', 0)",
                [],
            )?;
            c.execute(
                "INSERT INTO conversation_members (conversation_id, user_id, role, joined_at)
            VALUES (?1, 'guest', 'member', 0)",
                [&t.main],
            )?;
            Ok(())
        })
        .unwrap();
    cancel_conversation_grants(&t.state, &t.main);
    std::fs::write(dir.join("discovered"), "").unwrap();
    let r = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .unwrap()
        .unwrap()["result"]
        .clone();
    assert_eq!(r["isError"], true);
    assert!(text(&r).contains("cancelled"));
    assert!(
        events.try_recv().is_err(),
        "no private request was broadcast"
    );
    assert!(
        t.state
            .db
            .with(|c| db::list_messages(c, &t.main, None, 50))
            .unwrap()
            .is_empty()
    );
    assert!(t.calls_log().is_empty());
}

#[tokio::test]
async fn an_unverified_tool_is_denied_on_a_read_only_connection() {
    let t = setup(|_| {});
    let policy = policy::Policy {
        mode: policy::Mode::ReadOnly,
        ..Default::default()
    };
    t.state
        .db
        .with(|c| {
            c.execute(
                "UPDATE connectors SET policy = ?2 WHERE id = ?1",
                rusqlite::params![t.connector_id, serde_json::to_string(&policy).unwrap()],
            )?;
            Ok(())
        })
        .unwrap();
    let (token, _guard, _) = t.grant().await;
    let mut events = t.state.hub.subscribe();
    let r = t.call(&token, "search_hidden", json!({"query": "x"})).await;
    assert_eq!(r["isError"], true);
    assert!(text(&r).contains("read only"), "{r}");
    assert!(events.try_recv().is_err());
    assert!(t.calls_log().is_empty());
    assert_eq!(t.calls()[0].decision, "blocked");
}
