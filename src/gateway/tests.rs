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
async fn a_one_tap_service_reads_freely_and_asks_before_changes() {
    let t = setup(|_| {});
    // A hosted server that names its tools after itself, set up from the
    // catalog with its default rules.
    let script = FAKE_SERVER
        .replace(r#""name":"search_messages""#, r#""name":"notion-search""#)
        .replace(
            r#""name":"create_event""#,
            r#""name":"notion-create-pages""#,
        );
    let id = t.connector_id.clone();
    let mode = crate::connectors::catalog_entry("notion").unwrap().mode;
    t.state
        .db
        .with(move |c| {
            let mut cfg = entities::get_connector(c, &id)?.unwrap().config;
            cfg.args = vec!["-c".into(), script];
            entities::update_connector(c, &id, None, Some(&cfg), None)?;
            entities::set_connector_state(
                c,
                &id,
                &ConnectorState {
                    service: Some("notion"),
                    policy: Some(&policy::Policy {
                        mode,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )?;
            Ok(())
        })
        .unwrap();
    let (token, _guard, _) = t.grant().await;
    let r = t
        .call(&token, "notion-search", json!({"query": "plans"}))
        .await;
    assert_eq!(text(&r), "ok notion-search secret=[hidden]", "{r}");
    let calls = t.calls();
    assert_eq!(calls[0].decision, "auto");
    // The log keeps the real tool name.
    assert_eq!(calls[0].tool, "notion-search");

    // An ordinary change still waits for the owner on this service.
    let (state, tok) = (t.state.clone(), token.clone());
    let call = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "notion-create-pages", "arguments": {"title": "Plans"}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    assert!(
        card.card.as_ref().unwrap()["title"]
            .as_str()
            .unwrap()
            .contains("Notion")
    );
    t.decide(&card.id, false);
    let r = call.await.unwrap()["result"].clone();
    assert_eq!(r["isError"], true);
    assert!(!t.calls_log().contains("notion-create-pages"));
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
    // The next turn can check Iris status, but has no permission for the paused connector.
    drop(_guard);
    let (servers, _guard) = grant_for_turn(&t.state, &t.owner, &t.main, "main", "m2")
        .await
        .unwrap();
    assert!(servers.is_empty());
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
    assert_eq!(disconnect_all(&t.state).await.unwrap(), 1);
    assert!(!t.connector().enabled);
    let r = t.call(&token, "search_messages", json!({})).await;
    assert!(text(&r).contains("cancelled"), "{r}");
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
    let read = policy::Subject {
        part: None,
        keys: vec![("search_messages".into(), Category::Read)],
        every: &[],
    };
    let gate = Gate {
        tool: "search_messages",
        subject: &read,
        recipients: &[],
        decided_by: "auto",
        target: target(&con),
    };
    assert!(
        gate_refusal(&t.state, &grant, &con.id, &gate, &gate.target, true)
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
    assert!(
        crate::oauth::bearer(
            &t.state,
            &con,
            &crate::oauth::credentials_lock(&t.state).await
        )
        .await
        .is_err()
    );
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

/// HTTP connector that echoes credentials in RPC errors, status bodies, or
/// auth challenges, and otherwise completes a normal MCP handshake.
async fn fake_http_connector(t: &T, token: &str) -> (Connector, tokio::task::JoinHandle<()>) {
    use axum::response::IntoResponse;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(
            |headers: axum::http::HeaderMap, axum::Json(req): axum::Json<Value>| async move {
                let auth = headers["authorization"].to_str().unwrap();
                let token = auth.strip_prefix("Bearer ").unwrap();
                let result = match req["method"].as_str().unwrap() {
                    "initialize" => json!({"protocolVersion": upstream::PROTOCOL_VERSION}),
                    "notifications/initialized" => {
                        return axum::http::StatusCode::ACCEPTED.into_response();
                    }
                    "tools/list" => json!({"tools": [{"name": "search_messages"}]}),
                    "tools/call" => match req["params"]["arguments"]["error"].as_str() {
                        Some("rpc") => {
                            return axum::Json(json!({"jsonrpc": "2.0", "id": req["id"],
                                "error": {"code": -32000, "message": format!("failed: {token}")}}))
                            .into_response();
                        }
                        Some("status") => {
                            return (
                                axum::http::StatusCode::BAD_GATEWAY,
                                format!("failed: {token}"),
                            )
                                .into_response();
                        }
                        Some("challenge") => {
                            return (
                                axum::http::StatusCode::UNAUTHORIZED,
                                [("www-authenticate", format!("Bearer error=\"{token}\""))],
                            )
                                .into_response();
                        }
                        _ => json!({"content": [{"type": "text", "text": auth}]}),
                    },
                    _ => unreachable!(),
                };
                axum::Json(json!({"jsonrpc": "2.0", "id": req["id"], "result": result}))
                    .into_response()
            },
        ),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut con = t.connector();
    con.transport = "http".into();
    con.config = entities::ConnectorConfig {
        url: Some(url),
        oauth: Some(entities::OAuthConfig {
            connected_at: Some(1),
            access_token: Some(
                t.state
                    .vault
                    .seal_str(token, &format!("connector:{}:oauth:access_token", con.id))
                    .unwrap(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    };
    t.state
        .db
        .with(|c| {
            c.execute(
                "UPDATE connectors SET transport = 'http', config = ?2 WHERE id = ?1",
                rusqlite::params![con.id, serde_json::to_string(&con.config).unwrap()],
            )?;
            Ok(())
        })
        .unwrap();
    (con, server)
}

#[tokio::test]
async fn security_regression_old_account_cannot_replace_a_new_session_token() {
    for expiry in [None, Some(now_ms() + 3_600_000)] {
        let t = setup(|_| {});
        let (mut old, server) = fake_http_connector(&t, "old-account-token").await;
        old.config.oauth.as_mut().unwrap().expires_at = expiry;
        t.state
            .db
            .with(|c| entities::update_connector(c, &old.id, None, Some(&old.config), None))
            .unwrap();
        let old_session = session(&t.state, &old).await.unwrap();
        let mut new = old.clone();
        let o = new.config.oauth.as_mut().unwrap();
        o.connected_at = Some(2);
        o.access_token = Some(
            t.state
                .vault
                .seal_str(
                    "new-account-token",
                    &format!("connector:{}:oauth:access_token", new.id),
                )
                .unwrap(),
        );
        let waiting;
        {
            let _credentials = crate::oauth::credentials_lock(&t.state).await;
            let state = t.state.clone();
            let snapshot = old.clone();
            let (started, ready) = oneshot::channel();
            waiting = tokio::spawn(async move {
                started.send(()).unwrap();
                session(&state, &snapshot).await
            });
            ready.await.unwrap();
            // This sign-in wins while the old request waits for credentials.
            t.state
                .db
                .with(|c| entities::update_connector(c, &new.id, None, Some(&new.config), None))
                .unwrap();
        }
        assert!(waiting.await.unwrap().is_err());
        let new_session = session(&t.state, &new).await.unwrap();
        assert!(
            !Arc::ptr_eq(&old_session, &new_session),
            "new account needs a new MCP session"
        );
        // A delayed request still holding the old connector must not put
        // that account's token back into the current HTTP session.
        assert!(session(&t.state, &old).await.is_err());
        assert!(
            crate::oauth::bearer(
                &t.state,
                &old,
                &crate::oauth::credentials_lock(&t.state).await
            )
            .await
            .is_err()
        );
        assert!(Arc::ptr_eq(
            &new_session,
            &session(&t.state, &new).await.unwrap()
        ));
        let reply = new_session
            .upstream
            .call_tool("search_messages", &json!({}), CALL_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(text(&reply), "Bearer new-account-token");
        // Refreshing within the same account keeps the session, but a
        // snapshot with the pre-refresh token must use the current token.
        let mut refreshed = new.config.clone();
        refreshed.oauth.as_mut().unwrap().access_token = Some(
            t.state
                .vault
                .seal_str(
                    "refreshed-account-token",
                    &format!("connector:{}:oauth:access_token", new.id),
                )
                .unwrap(),
        );
        {
            let _credentials = crate::oauth::credentials_lock(&t.state).await;
            t.state
                .db
                .with(|c| entities::update_connector(c, &new.id, None, Some(&refreshed), None))
                .unwrap();
        }
        assert!(Arc::ptr_eq(
            &new_session,
            &session(&t.state, &new).await.unwrap()
        ));
        let reply = new_session
            .upstream
            .call_tool("search_messages", &json!({}), CALL_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(text(&reply), "Bearer refreshed-account-token");
        let (token, _guard, _) = t.grant().await;
        let grant = t.state.gateway.grant(&token).unwrap();
        let read = policy::Subject {
            part: None,
            keys: vec![("search_messages".into(), Category::Read)],
            every: &[],
        };
        let gate = Gate {
            tool: "search_messages",
            subject: &read,
            recipients: &[],
            decided_by: "auto",
            target: target(&new),
        };
        assert!(
            gate_refusal(&t.state, &grant, &new.id, &gate, &old_session.target, false)
                .await
                .is_some(),
            "the gate must check the actual session's generation"
        );
        stop(&t.state, &new.id).await;
        server.abort();
    }
}

#[tokio::test]
async fn security_regression_cancelled_card_has_no_private_details_for_new_members() {
    let t = setup(|c| c.safety.approval_timeout_secs = 60);
    let (token, _guard, _) = t.grant().await;
    let mut events = t.state.hub.subscribe();
    let (call, pending) = held_send(&t, &token).await;
    let private_card = pending.card.as_ref().unwrap();
    assert!(private_card["details"].is_string());
    // The original card was sent only to its original audience.
    let created = events.recv().await.unwrap();
    assert!(created.audience.includes(&t.owner));
    assert!(!created.audience.includes("guest"));
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
    call.await.unwrap();
    let closed = t
        .state
        .db
        .with(|c| db::get_message(c, &pending.id))
        .unwrap()
        .unwrap();
    assert_eq!(closed.content, "Request cancelled");
    let card = closed.card.as_ref().unwrap();
    assert_eq!(card["state"], "cancelled");
    assert_eq!(
        card.as_object().unwrap().len(),
        4,
        "only generic cancellation fields persist"
    );
    while let Ok(event) = events.try_recv() {
        if event.audience.includes("guest") {
            let v: Value = serde_json::from_str(&event.json).unwrap();
            if v["type"] == "message.updated" {
                assert_eq!(v["message"]["card"], *card);
                assert_eq!(v["message"]["content"], "Request cancelled");
            }
            assert!(
                !event
                    .json
                    .contains(private_card["details"].as_str().unwrap())
            );
        }
    }
    assert!(t.calls_log().is_empty());
}

#[tokio::test]
async fn security_regression_long_upstream_secrets_are_scrubbed_before_truncation() {
    // Longer than the RPC (500), HTTP (300), and old body-read (2000) caps.
    let secret = "synthetic-secret-".repeat(200);
    let t = setup(|_| {});
    let (con, server) = fake_http_connector(&t, &secret).await;
    let s = session(&t.state, &con).await.unwrap();
    let (token, _guard, _) = t.grant().await;
    let grant = t.state.gateway.grant(&token).unwrap();
    let read = policy::Subject {
        part: None,
        keys: vec![("search_messages".into(), Category::Read)],
        every: &[],
    };
    let gate = Gate {
        tool: "search_messages",
        subject: &read,
        recipients: &[],
        decided_by: "auto",
        target: target(&con),
    };
    for kind in ["rpc", "status", "challenge"] {
        let error = s
            .upstream
            .call_tool("search_messages", &json!({"error": kind}), CALL_TIMEOUT)
            .await
            .unwrap_err();
        let diagnostic = truncate_chars(&s.scrub(&error.to_string()), 300);
        assert!(diagnostic.contains("[hidden]"), "{kind}: {diagnostic}");
        assert!(
            !diagnostic.contains("synthetic-secret"),
            "{kind}: {diagnostic}"
        );
        if kind != "challenge" {
            let result = forward(&t.state, &grant, &con.id, &gate, &json!({"error": kind})).await;
            let Err(CallError::Other(message)) = result else {
                panic!("expected scrubbed error")
            };
            assert!(message.contains("[hidden]"), "{kind}: {message}");
            assert!(!message.contains("synthetic-secret"), "{kind}: {message}");
        }
    }
    stop(&t.state, &con.id).await;
    server.abort();
}

/// Browser wallet calls use the same pending map, decision route, expiry,
/// and cancellation handling as connector approvals.
#[tokio::test]
async fn wallet_approvals_use_the_gateway_and_never_resume_ended_turns() {
    for action in [
        "approve",
        "deny",
        "expire",
        "stop",
        "pause",
        "settings_pause",
        "disconnect",
        "join",
        "abort",
    ] {
        let t = setup(|c| c.safety.approval_timeout_secs = 1);
        let guard = t.state.browser.begin_turn(
            &t.owner,
            &t.main,
            "wallet-turn",
            std::slice::from_ref(&t.owner),
        );
        let ctx = t.state.browser.turn(guard.token()).unwrap();
        let st = t.state.clone();
        let card = entities::approval_card(&entities::ApprovalRequest {
            title: "Fill card at shop.example",
            service: "wallet",
            approver_user_id: &t.owner,
            ..Default::default()
        });
        let held = tokio::spawn(async move { approve_browser_card(&st, &ctx, card).await });
        let msg = t.pending_card().await;
        assert_eq!(msg.card.as_ref().unwrap()["turn_message_id"], "wallet-turn");
        assert!(msg.card.as_ref().unwrap()["expires_at"].as_i64().unwrap() > msg.created_at);
        // The socket receives the same release as a connector call.
        match action {
            "approve" | "deny" => {
                owner_request(
                    &t,
                    "POST",
                    &format!("/api/messages/{}/decision", msg.id),
                    json!({"approve": action == "approve"}),
                )
                .await;
            }
            "stop" => {
                crate::turn::cancel(&t.state, &t.main);
            }
            "pause" => {
                owner_request(&t, "PUT", "/api/safety", json!({"paused": true})).await;
            }
            "settings_pause" => {
                owner_request(&t, "PUT", "/api/settings", json!({"paused": true})).await;
            }
            "disconnect" => {
                owner_request(&t, "POST", "/api/safety/disconnect-all", Value::Null).await;
            }
            "join" => {
                t.state.db.with(|c| {
                    let u = add_guest(c)?;
                    c.execute("INSERT INTO conversation_members (conversation_id, user_id, role, joined_at)
                        VALUES (?1, ?2, 'member', 1)", rusqlite::params![t.main, u.id])?;
                    Ok(())
                }).unwrap();
                cancel_conversation_grants(&t.state, &t.main);
            }
            "abort" => {
                held.abort();
                t.state.browser.end_turns(Some(&t.main));
            }
            _ => {}
        }
        if action != "abort" {
            let (answer, _) = tokio::time::timeout(Duration::from_secs(3), held)
                .await
                .unwrap()
                .unwrap();
            assert!(
                match action {
                    "approve" => matches!(answer, Answer::Approved),
                    "deny" => matches!(answer, Answer::Denied),
                    "expire" => matches!(answer, Answer::Expired),
                    _ => matches!(answer, Answer::Cancelled),
                },
                "{action}"
            );
        }
        // Detached wallet waiter closes its card even if the MCP request
        // was dropped while waiting (Stop, disconnect, or CLI exit).
        for _ in 0..100 {
            let closed = t
                .state
                .db
                .with(|c| db::get_message(c, &msg.id))
                .unwrap()
                .unwrap()
                .card
                .as_ref()
                .is_some_and(|c| c["state"] != "pending");
            if closed && t.state.gateway.pending_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let card = t
            .state
            .db
            .with(|c| db::get_message(c, &msg.id))
            .unwrap()
            .unwrap()
            .card
            .unwrap();
        let expected = match action {
            "approve" => "approved",
            "deny" => "denied",
            "expire" => "expired",
            _ => "cancelled",
        };
        assert_eq!(card["state"], expected, "{action}");
        assert_eq!(t.state.gateway.pending_count(), 0);
        if action == "disconnect" {
            assert!(!t.state.browser.enabled());
            assert!(t.state.browser.running().is_none());
            assert!(t.state.setting("browser.disconnected").await.is_some());
            assert!(t.state.browser.ensure().await.is_err());
        }
        let messages = t
            .state
            .db
            .with(|c| db::list_messages(c, &t.main, None, 50))
            .unwrap();
        assert_eq!(messages.iter().filter(|m| m.kind == "card").count(), 1);
        assert_eq!(
            messages.iter().filter(|m| m.kind == "system_note").count(),
            usize::from(matches!(action, "approve" | "deny"))
        );
        drop(guard);
    }
}

#[tokio::test]
async fn browser_access_is_owner_only_private_and_revoked_on_membership_change() {
    let t = setup(|_| {});
    let owner_turn = t.state.browser.begin_turn(&t.owner, &t.main, "m", &[]);
    let ctx = t.state.browser.turn(owner_turn.token()).unwrap();
    assert!(crate::browser::tools::access_allowed(&t.state, &ctx).await);
    let guest = t.state.db.with(add_guest).unwrap();
    let guest_turn = t.state.browser.begin_turn(&guest.id, &t.main, "g", &[]);
    let guest_ctx = t.state.browser.turn(guest_turn.token()).unwrap();
    assert!(!crate::browser::tools::access_allowed(&t.state, &guest_ctx).await);
    t.state
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO conversation_members (conversation_id, user_id, role, joined_at)
            VALUES (?1, ?2, 'member', 1)",
                rusqlite::params![t.main, guest.id],
            )?;
            Ok(())
        })
        .unwrap();
    // Even before a membership notification, the next socket call fails closed.
    assert!(!crate::browser::tools::access_allowed(&t.state, &ctx).await);
    cancel_conversation_grants(&t.state, &t.main);
    assert!(ctx.is_ended());
    assert!(guest_ctx.is_ended());
}

fn add_guest(c: &rusqlite::Connection) -> rusqlite::Result<db::User> {
    c.execute("INSERT INTO users (id, display_name, is_owner, created_at) VALUES ('guest', 'Guest', 0, 1)", [])?;
    Ok(db::get_user(c, "guest")?.unwrap())
}

async fn owner_request(t: &T, method: &str, path: &str, body: Value) {
    use axum::body::Body;
    use tower::ServiceExt;
    let app = crate::server::router(t.state.clone());
    let code = t
        .state
        .db
        .with(|c| Ok(crate::auth::create_pairing_code(c, &t.owner)))
        .unwrap()
        .unwrap()
        .0;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::post("/api/pair")
                .header("content-type", "application/json")
                .body(Body::from(json!({"code": code}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let paired: Value = serde_json::from_slice(&bytes).unwrap();
    let token = paired["token"].as_str().unwrap();
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{method} {path}: {}",
        response.status()
    );
}

#[test]
fn encoded_connector_secrets_are_hidden() {
    use base64::{
        Engine as _,
        engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
    };
    let secret = "Canary+/ secret?";
    let mut forms = vec![
        secret.to_uppercase(),
        secret.chars().rev().collect::<String>(),
        crate::util::hex(secret.as_bytes()),
        "Canary%2B%2F%20secret%3F".into(),
        "Canary%2B%2F+secret%3F".into(),
        secret.bytes().map(|b| format!("%{b:02X}")).collect(),
    ];
    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        forms.push(engine.encode(secret));
    }
    assert_eq!(
        redact("🔒 PÄssäÖ12345 end", &["pässÄö12345".into()]),
        "🔒 [hidden] end"
    );
    assert_eq!(
        redact("prefix i̇-CANARY8 suffix", &["İ-canary8".into()]),
        "prefix [hidden] suffix"
    );
    let unicode_secret = "canaryÿÿÿ!";
    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        assert_eq!(
            redact(&engine.encode(unicode_secret), &[unicode_secret.into()]),
            "[hidden]"
        );
    }
    assert_eq!(redact("ABCD123", &["Abcd123".into()]), "ABCD123");
    for form in forms {
        for echo in [
            form.clone(),
            form.to_ascii_uppercase(),
            form.to_ascii_lowercase(),
        ] {
            assert_eq!(
                redact(&format!("before {echo} after"), &[secret.into()]),
                "before [hidden] after",
                "{echo}"
            );
        }
    }
}

#[tokio::test]
async fn concatenated_sends_count_against_the_send_tripwire() {
    let t = setup(|cfg| cfg.safety.max_sends_per_hour = 1);
    let mut connector = t.connector();
    connector.config.args[1] = FAKE_SERVER.replace("send_message", "sendemail");
    t.state
        .db
        .with(|c| {
            entities::update_connector(c, &t.connector_id, None, Some(&connector.config), None)?;
            db::set_setting(c, "safety.trusted_recipients", r#"["friend@example.test"]"#)
        })
        .unwrap();
    let (token, _grant, _) = t.grant().await;
    let first = t
        .call(&token, "sendemail", json!({"to":"friend@example.test"}))
        .await;
    assert!(!first["isError"].as_bool().unwrap_or(false), "{first}");
    assert_eq!(t.calls()[0].category, "send");
    let second = t
        .call(&token, "sendemail", json!({"to":"friend@example.test"}))
        .await;
    assert!(text(&second).contains("paused"), "{second}");
    assert_eq!(t.calls_log().matches("sendemail").count(), 1);
}

/// The fake server with Google Workspace MCP tool names, set up as the
/// Google connection with the given rules.
fn as_google(t: &T, p: &policy::Policy) {
    let script = FAKE_SERVER
        .replace(
            r#""name":"search_messages""#,
            r#""name":"search_gmail_messages""#,
        )
        .replace(r#""name":"send_message""#, r#""name":"send_gmail_message""#)
        .replace(r#""name":"delete_message""#, r#""name":"manage_event""#)
        .replace(r#""name":"create_event""#, r#""name":"list_calendars""#)
        .replace(r#""name":"export_everything""#, r#""name":"get_events""#);
    let id = t.connector_id.clone();
    let p = p.clone();
    t.state
        .db
        .with(move |c| {
            let mut cfg = entities::get_connector(c, &id)?.unwrap().config;
            cfg.args = vec!["-c".into(), script];
            entities::update_connector(c, &id, None, Some(&cfg), None)?;
            entities::set_connector_state(
                c,
                &id,
                &ConnectorState {
                    service: Some("google"),
                    policy: Some(&p),
                    ..Default::default()
                },
            )?;
            Ok(())
        })
        .unwrap();
}

fn set_policy(t: &T, p: &policy::Policy) {
    let (id, p) = (t.connector_id.clone(), p.clone());
    t.state
        .db
        .with(move |c| {
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
}

async fn listed(t: &T, token: &str) -> Vec<String> {
    let list = handle(
        &t.state,
        &json!({"token": token, "connector": "chat",
                "request": {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}}),
    )
    .await;
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn google_actions_allow_ask_and_off_are_enforced_in_the_gateway() {
    let t = setup(|_| {});
    let mode = crate::connectors::catalog_entry("google").unwrap().mode;
    let mut p = policy::Policy::new(mode);
    as_google(&t, &p);
    let (token, _guard, _) = t.grant().await;

    // Defaults: reads run.
    let r = t
        .call(&token, "search_gmail_messages", json!({"query": "x"}))
        .await;
    assert_eq!(text(&r), "ok search_gmail_messages secret=[hidden]", "{r}");
    assert_eq!(t.calls()[0].decision, "auto");
    assert_eq!(listed(&t, &token).await.len(), 6);

    // Defaults: a plain new event asks.
    let (state, tok) = (t.state.clone(), token.clone());
    let held = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "manage_event", "arguments": {
                    "action": "create", "summary": "Lunch"}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    t.decide(&card.id, false);
    assert_eq!(held.await.unwrap()["result"]["isError"], true);
    assert!(!t.calls_log().contains("manage_event"));

    // Allow: creating events runs; an invitation is its own action and
    // still asks; deleting is off and refused while the tool stays listed.
    p.tools
        .insert("calendar.events.create".into(), policy::ToolRule::Allow);
    p.tools
        .insert("calendar.events.delete".into(), policy::ToolRule::Block);
    set_policy(&t, &p);
    let r = t
        .call(
            &token,
            "manage_event",
            json!({"action": "create", "summary": "Lunch"}),
        )
        .await;
    assert_eq!(text(&r), "ok manage_event secret=[hidden]", "{r}");
    let r = t
        .call(
            &token,
            "manage_event",
            json!({"action": "delete", "event_id": "e1"}),
        )
        .await;
    assert!(text(&r).contains("turned this action off"), "{r}");
    assert_eq!(t.calls_log().matches("manage_event").count(), 1);
    let tools = listed(&t, &token).await;
    assert!(tools.contains(&"manage_event".to_string()));
    let (state, tok) = (t.state.clone(), token.clone());
    let held = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {"name": "manage_event", "arguments": {
                    "action": "create", "summary": "Lunch",
                    "attendees": ["sam@example.com"]}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    t.decide(&card.id, false);
    assert_eq!(held.await.unwrap()["result"]["isError"], true);
    // An argument value the table does not know falls back to the general
    // classification and asks.
    let (state, tok) = (t.state.clone(), token.clone());
    let held = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": {"name": "manage_event", "arguments": {"action": "purge_all"}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    t.decide(&card.id, false);
    assert_eq!(held.await.unwrap()["result"]["isError"], true);
    assert_eq!(t.calls_log().matches("manage_event").count(), 1);

    // Off for every action a tool performs: hidden and refused, whatever
    // it is asked to do.
    p.tools.insert("gmail.send".into(), policy::ToolRule::Block);
    for id in google::tool_actions("manage_event") {
        p.tools.insert((*id).into(), policy::ToolRule::Block);
    }
    set_policy(&t, &p);
    let tools = listed(&t, &token).await;
    assert!(!tools.contains(&"send_gmail_message".to_string()));
    assert!(!tools.contains(&"manage_event".to_string()));
    let r = t
        .call(
            &token,
            "send_gmail_message",
            json!({"to": "sam@example.com"}),
        )
        .await;
    assert!(text(&r).contains("turned this action off"), "{r}");
    let r = t
        .call(&token, "manage_event", json!({"action": "purge_all"}))
        .await;
    assert!(text(&r).contains("turned this action off"), "{r}");

    // A service turned off as a whole: its tools are hidden and refused.
    p.off.insert("calendar".into());
    set_policy(&t, &p);
    let tools = listed(&t, &token).await;
    assert_eq!(tools, ["search_gmail_messages", "read_archive"]);
    let r = t.call(&token, "list_calendars", json!({})).await;
    assert!(text(&r).contains("turned Google Calendar off"), "{r}");
    assert!(!t.calls_log().contains("list_calendars"));
    // Allowing an action does not bring back a service that is off.
    p.tools
        .insert("calendar.calendars.read".into(), policy::ToolRule::Allow);
    set_policy(&t, &p);
    let r = t.call(&token, "list_calendars", json!({})).await;
    assert!(text(&r).contains("turned Google Calendar off"), "{r}");
    p.off.clear();
    set_policy(&t, &p);
    let r = t.call(&token, "list_calendars", json!({})).await;
    assert_eq!(text(&r), "ok list_calendars secret=[hidden]", "{r}");
    // The activity log keeps the real tool names.
    assert!(t.calls().iter().any(|c| c.tool == "manage_event"));
}

#[tokio::test]
async fn a_google_action_turned_off_while_its_call_waits_is_not_sent() {
    let t = setup(|_| {});
    let mut p = policy::Policy::new(policy::Mode::Careful);
    as_google(&t, &p);
    let (token, _guard, _) = t.grant().await;
    let (state, tok) = (t.state.clone(), token.clone());
    let held = tokio::spawn(async move {
        handle(
            &state,
            &json!({"token": tok, "connector": "chat", "request": {
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "send_gmail_message", "arguments": {
                    "to": "sam@example.com", "subject": "Hi", "body": "Hello"}}}}),
        )
        .await
    });
    let card = t.pending_card().await;
    p.off.insert("gmail".into());
    set_policy(&t, &p);
    t.decide(&card.id, true);
    let r = held.await.unwrap()["result"].clone();
    assert_eq!(r["isError"], true);
    assert!(text(&r).contains("turned Gmail off"), "{r}");
    assert!(!t.calls_log().contains("send_gmail_message"));
}

#[tokio::test]
async fn an_older_google_connection_keeps_its_rules_as_actions() {
    let t = setup(|_| {});
    // Saved by an older Iris: the old default and rules on tools.
    let mut old = policy::Policy::default();
    old.tools
        .insert("send_gmail_message".into(), policy::ToolRule::Allow);
    old.tools
        .insert("manage_event".into(), policy::ToolRule::Block);
    old.tools
        .insert("some_unknown_tool".into(), policy::ToolRule::Ask);
    as_google(&t, &old);
    let con = t.connector();
    assert_eq!(con.policy.mode, policy::Mode::Careful);
    assert_eq!(con.policy.version, policy::POLICY_VERSION);
    assert_eq!(
        con.policy.tools.get("gmail.send"),
        Some(&policy::ToolRule::Allow)
    );
    for id in google::tool_actions("manage_event") {
        assert_eq!(
            con.policy.tools.get(*id),
            Some(&policy::ToolRule::Block),
            "{id}"
        );
    }
    assert!(!con.policy.tools.contains_key("send_gmail_message"));
    assert_eq!(
        con.policy.tools.get("some_unknown_tool"),
        Some(&policy::ToolRule::Ask)
    );
    let (token, _guard, _) = t.grant().await;
    assert!(
        !listed(&t, &token)
            .await
            .contains(&"manage_event".to_string())
    );
    let r = t
        .call(
            &token,
            "send_gmail_message",
            json!({"to": "sam@example.com"}),
        )
        .await;
    assert_eq!(text(&r), "ok send_gmail_message secret=[hidden]", "{r}");
    // A connection that is not Google keeps the old default.
    let mut other = policy::Policy::default();
    other
        .tools
        .insert("manage_event".into(), policy::ToolRule::Block);
    let up = other.clone().upgraded("notion");
    assert_eq!(up.mode, policy::Mode::Standard);
    assert_eq!(up.tools, other.tools);
}
