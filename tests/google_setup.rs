//! Exercise the actual HTTP server and its stderr, not just its JSON projection.
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn google_pastes_and_malformed_requests_never_leak_to_responses_or_logs() {
    const ID: &str = "123-http-canary.apps.googleusercontent.com";
    const SECRET: &str = "GOCSPX-http-canary_123";
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.toml");
    let data = tmp.path().join("data");
    let command = |action: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_iris"));
        cmd.arg(action)
            .arg("--config")
            .arg(&config)
            .arg("--data-dir")
            .arg(&data);
        cmd
    };
    assert!(command("init").output().unwrap().status.success());
    let mut cfg: toml::Value = toml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    cfg["public_url"] = "https://iris.example".into();
    for (_, harness) in cfg["harness"].as_table_mut().unwrap().iter_mut() {
        harness["enabled"] = false.into();
    }
    for section in ["engines", "heartbeat", "feed", "push"] {
        cfg[section][if section == "engines" {
            "auto_update"
        } else {
            "enabled"
        }] = false.into();
    }
    std::fs::write(&config, toml::to_string(&cfg).unwrap()).unwrap();
    let paired = command("pair").output().unwrap();
    assert!(paired.status.success());
    let output = String::from_utf8(paired.stdout).unwrap();
    let code = output
        .lines()
        .find(|s| s.starts_with("Pairing code for "))
        .unwrap()
        .rsplit_once(": ")
        .unwrap()
        .1;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let log_path = tmp.path().join("server.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let server = Server(
        command("serve")
            .arg("--bind")
            .arg(address.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let base = format!("http://{address}/api");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into();
    let mut ready = false;
    for _ in 0..100 {
        if agent.get(format!("{base}/health")).call().is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready, "test server did not start");
    let mut response = agent
        .post(format!("{base}/pair"))
        .header("Content-Type", "application/json")
        .send(json!({"code": code}).to_string().as_bytes())
        .unwrap();
    let paired: Value =
        serde_json::from_str(&response.body_mut().read_to_string().unwrap()).unwrap();
    let token = paired["token"].as_str().unwrap();
    for (path, body, expected_status) in [
        (
            "connectors/google/setup/client",
            json!({"paste": format!("{ID}\n{SECRET}"), "validate_only": true}),
            200,
        ),
        (
            "connectors/google/setup/client",
            json!({"paste": format!("{SECRET}\n{ID}")}),
            200,
        ),
        (
            "connectors/google/setup/client",
            json!({"paste": format!("junk {SECRET}")}),
            400,
        ),
        (
            "connectors/google/setup/error",
            json!({"code": format!("invalid_client {SECRET}")}),
            400,
        ),
        ("connectors/google/setup", json!({"step": SECRET}), 400),
    ] {
        let request = if path.ends_with("/setup") {
            agent.put(format!("{base}/{path}"))
        } else {
            agent.post(format!("{base}/{path}"))
        };
        let mut response = request
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())
            .unwrap();
        assert_eq!(response.status().as_u16(), expected_status);
        let text = response.body_mut().read_to_string().unwrap();
        assert!(!text.contains(SECRET) && !text.contains(ID));
    }
    drop(server);
    let logs = std::fs::read_to_string(log_path).unwrap();
    assert!(
        !logs.contains(SECRET) && !logs.contains(ID),
        "credential leaked to logs"
    );
}
