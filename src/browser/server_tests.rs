//! Iris's browser against a stand-in Playwright MCP server (a tiny MCP
//! server in shell, answering like Playwright MCP does): starting it, the
//! agent's calls, take over, the private-page rule, saved-login fills, the
//! sandbox consent, and the live view.

use super::*;
use crate::state::AppState;

/// Answers like Playwright MCP (synthetic results modeled on its markdown).
/// Works in its working directory: `url` holds the current address,
/// `field` whether the focused field is private (default true),
/// `nosandbox` makes a sandboxed start fail, `calls.log` records calls.
const FAKE: &str = r####"#!/bin/sh
cfg="$2"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}%s]%s}}\n' "$id" "$1" "$2" "$3"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\)[,}].*/\1/p')
  [ -n "$id" ] || continue
  url=$(cat url 2>/dev/null || echo about:blank)
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"Playwright","version":"0.0.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"browser_navigate","inputSchema":{"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}},{"name":"browser_snapshot","inputSchema":{"type":"object","properties":{"filename":{"type":"string"}}}},{"name":"browser_take_screenshot","inputSchema":{"type":"object","properties":{"type":{"type":"string"},"filename":{"type":"string"}}}},{"name":"browser_run_code_unsafe","inputSchema":{"type":"object"}},{"name":"browser_evaluate","inputSchema":{"type":"object"}},{"name":"browser_file_upload","inputSchema":{"type":"object"}},{"name":"browser_tabs","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '%s\n' "$line" >> calls.log
      tool=$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
      case "$tool" in
        browser_tabs)
          if [ -f nosandbox ] && grep -q '"chromiumSandbox":true' "$cfg"; then
            reply '### Error\nError: browserBackend.callTool: Target page, context or browser has been closed\nBrowser logs:\nChromium sandboxing failed!\n[pid=1][err] [1:1:FATAL:zygote_host_impl_linux.cc(130)] No usable sandbox!' '' ',"isError":true'
          else
            reply "### Result\\n- 0: (current) [Sign in]($url)" '' ''
          fi ;;
        browser_navigate)
          to=$(printf '%s' "$line" | sed -n 's/.*"url":"\([^"]*\)".*/\1/p')
          printf '%s' "$to" > url
          reply "### Page\\n- Page URL: $to\\n- Page Title: Sign in\\n### Snapshot\\n- [Snapshot](out/page-2.yml)" '' '' ;;
        browser_evaluate)
          reply "### Result\\n$(cat field 2>/dev/null || echo true)" '' '' ;;
        browser_take_screenshot)
          mkdir -p out && echo x > out/page-1.jpeg
          reply '### Result\n- [Screenshot of viewport](out/page-1.jpeg)' ',{"type":"image","data":"QUJD","mimeType":"image/jpeg"}' '' ;;
        *)
          reply "### Page\\n- Page URL: $url\\n- Page Title: Sign in" '' '' ;;
      esac ;;
  esac
done
"####;

struct Fake {
    _tmp: tempfile::TempDir,
    state: AppState,
    owner: String,
    conv: String,
}

impl Fake {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let state = tools::tests::test_state(tmp.path());
        let bin = tmp.path().join("fake-playwright-mcp");
        std::fs::write(&bin, FAKE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        *lock(&state.browser.server_override) = Some(launch::Server {
            program: bin,
            chromium: None,
            user_agent: None,
        });
        std::fs::create_dir_all(&state.browser.work_dir).unwrap();
        let (owner, conv) = state
            .db
            .with(|c| {
                let owner = crate::db::owner_user_id(c)?.unwrap();
                let conv = crate::db::ensure_main_conversation(c, &owner)?;
                Ok((owner, conv))
            })
            .unwrap();
        Self {
            _tmp: tmp,
            state,
            owner,
            conv,
        }
    }

    fn browser(&self) -> &Browser {
        &self.state.browser
    }

    fn file(&self, name: &str) -> PathBuf {
        self.browser().work_dir.join(name)
    }

    fn set(&self, name: &str, value: &str) {
        std::fs::write(self.file(name), value).unwrap();
    }

    /// Every call the server got: (tool, arguments).
    fn calls(&self) -> Vec<(String, Value)> {
        std::fs::read_to_string(self.file("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                (
                    v["params"]["name"].as_str().unwrap().to_string(),
                    v["params"]["arguments"].clone(),
                )
            })
            .collect()
    }

    fn called(&self, tool: &str) -> usize {
        self.calls().iter().filter(|(t, _)| t == tool).count()
    }

    async fn agent(&self, token: &str, tool: &str, args: Value) -> Value {
        tools::call(&self.state, token, tool, &args).await
    }
}

fn text(v: &Value) -> String {
    v["content"][0]["text"].as_str().unwrap_or("").to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_calls_pass_through_and_private_pages_stay_closed() {
    let f = Fake::new();
    let b = f.browser();
    let g = b.begin_turn(&f.owner, &f.conv, "m", &[]);
    let token = g.token().to_string();

    // Only reviewed tools, without file options; listing starts no browser.
    let listed = tools::list(&f.state, &token).await;
    let names: Vec<_> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "browser_navigate",
            "browser_snapshot",
            "browser_take_screenshot",
            "browser_tabs"
        ]
    );
    assert!(
        listed["tools"][2]["inputSchema"]["properties"]
            .get("filename")
            .is_none()
    );
    assert!(b.running().is_none());

    // A call starts the browser and passes through unchanged.
    let r = f
        .agent(
            &token,
            "browser_navigate",
            json!({"url": "https://site.test/login"}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(text(&r).contains("- Page URL: https://site.test/login"));
    assert!(b.running().is_some());
    assert_eq!(b.state_json()["url"], "https://site.test/login");
    assert_eq!(
        f.calls().last().unwrap(),
        &(
            "browser_navigate".to_string(),
            json!({"url": "https://site.test/login"})
        )
    );

    // The owner takes over and types a password: taps become mouse
    // input at page coordinates, typing becomes key presses (and inserted
    // text beyond ASCII).
    b.take_over(&f.owner, "dev").await.unwrap();
    b.user_input("dev", &json!({"kind": "tap", "x": 0.5, "y": 0.25}))
        .await
        .unwrap();
    b.user_input("dev", &json!({"kind": "text", "text": "pä"}))
        .await
        .unwrap();
    b.user_input("dev", &json!({"kind": "key", "key": "Backspace"}))
        .await
        .unwrap();
    assert!(
        b.user_input("dev", &json!({"kind": "key", "key": "F12"}))
            .await
            .is_err()
    );
    let calls = f.calls();
    let i = calls
        .iter()
        .position(|(t, _)| t == "browser_mouse_move_xy")
        .unwrap();
    assert_eq!(calls[i].1, json!({"x": 250.0, "y": 250.0}));
    assert_eq!(calls[i + 1].0, "browser_mouse_down");
    assert_eq!(calls[i + 2].0, "browser_mouse_up");
    let keys: Vec<_> = calls
        .iter()
        .filter(|(t, _)| t == "browser_press_key")
        .map(|(_, a)| a["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["p", "Backspace"]);
    assert!(calls.iter().any(|(t, a)| {
        t == "browser_evaluate"
            && a["function"]
                .as_str()
                .unwrap()
                .contains("insertText', false, \"ä\"")
    }));
    b.hand_back();

    // That page is closed to the agent, also for screenshots; nothing
    // reaches the server.
    let before = f.calls().len();
    for tool in ["browser_snapshot", "browser_take_screenshot"] {
        let r = f.agent(&token, tool, json!({})).await;
        assert_eq!(r["isError"], true);
        assert_eq!(text(&r), PRIVATE_PAGE);
    }
    assert!(f.calls()[before..].iter().all(|(t, _)| t == "browser_tabs"));
    // A fragment is the same page; the page moving on opens it again.
    f.set("url", "https://site.test/login#password");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_eq!(text(&r), PRIVATE_PAGE);
    f.set("url", "https://site.test/home");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert_eq!(f.called("browser_snapshot"), 1);

    // Typing into an ordinary field keeps the page open.
    b.take_over(&f.owner, "dev").await.unwrap();
    f.set("field", "false");
    b.user_input("dev", &json!({"kind": "tap", "x": 0.1, "y": 0.1}))
        .await
        .unwrap();
    b.user_input("dev", &json!({"kind": "text", "text": "shoes"}))
        .await
        .unwrap();
    b.hand_back();
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");

    // Navigating away from a private page is always allowed and opens it.
    b.take_over(&f.owner, "dev").await.unwrap();
    f.set("field", "true");
    b.user_input("dev", &json!({"kind": "tap", "x": 0.1, "y": 0.1}))
        .await
        .unwrap();
    b.user_input("dev", &json!({"kind": "text", "text": "123456"}))
        .await
        .unwrap();
    b.hand_back();
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_eq!(text(&r), PRIVATE_PAGE);
    let r = f
        .agent(
            &token,
            "browser_navigate",
            json!({"url": "https://site.test/home"}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");

    // Closing: gracefully, and nothing of the session's output stays.
    std::fs::create_dir_all(&b.out_dir).unwrap();
    b.close().await;
    assert!(b.running().is_none());
    assert_eq!(f.calls().last().unwrap().0, "browser_close");
    assert!(!b.out_dir.exists());
    drop(g);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_fills_saved_logins_only_on_their_site() {
    let f = Fake::new();
    let b = f.browser();
    let id = "login-1".to_string();
    let secret = f
        .state
        .vault
        .encrypt(json!({"password": "Pw-1234"}).to_string().as_bytes(), &id)
        .unwrap();
    f.state
        .db
        .with(|c| {
            crate::entities::insert_credential(
                c,
                &id,
                &f.owner,
                "login",
                "Site",
                &json!({"site": "site.test", "username": "sam", "allow_subdomains": false}),
                &secret,
            )
        })
        .unwrap();
    // Not in control: nothing happens.
    let err = fill::fill(&f.state, "dev", &id, "password")
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "take over first");
    b.take_over(&f.owner, "dev").await.unwrap();
    // Another site, or a page without HTTPS: refused before typing.
    f.set("url", "https://evil.test/login");
    let err = fill::fill(&f.state, "dev", &id, "password")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("saved for site.test"), "{err}");
    f.set("url", "http://site.test/login");
    assert!(fill::fill(&f.state, "dev", &id, "password").await.is_err());
    assert_eq!(f.called("browser_press_key"), 0);
    // Its own site: typed key by key, and the page closes to the agent.
    f.set("url", "https://site.test/login");
    f.set("field", "false");
    fill::fill(&f.state, "dev", &id, "password").await.unwrap();
    let keys: String = f
        .calls()
        .iter()
        .filter(|(t, _)| t == "browser_press_key")
        .map(|(_, a)| a["key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys, "Pw-1234");
    b.hand_back();
    let g = b.begin_turn(&f.owner, &f.conv, "m", &[]);
    let r = f.agent(g.token(), "browser_snapshot", json!({})).await;
    assert_eq!(text(&r), PRIVATE_PAGE);
    let used = f
        .state
        .db
        .with(|c| crate::entities::get_credential(c, &f.owner, &id))
        .unwrap()
        .unwrap();
    assert!(used.last_used_at.is_some());
    b.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sandbox_stays_on_without_the_owners_consent() {
    let f = Fake::new();
    let b = f.browser();
    f.set("nosandbox", "");
    let err = b.ensure().await.err().unwrap();
    assert_eq!(err.to_string(), SANDBOX_UNAVAILABLE);
    assert_eq!(b.state_json()["sandbox_unavailable"], true);
    assert!(b.running().is_none());
    // With the owner's OK it runs without, and says so.
    b.set_allow_no_sandbox(true).await;
    let run = b.ensure().await.unwrap();
    assert!(run.no_sandbox);
    assert_eq!(b.state_json()["sandbox"], false);
    let cfg: Value = serde_json::from_slice(&std::fs::read(&b.config_file).unwrap()).unwrap();
    assert_eq!(cfg["browser"]["launchOptions"]["chromiumSandbox"], false);
    // Taking the OK back closes it.
    b.set_allow_no_sandbox(false).await;
    assert!(b.running().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_view_sends_new_pictures_and_removes_its_files() {
    let f = Fake::new();
    let b = f.browser();
    b.ensure().await.unwrap();
    let (guard, mut rx) = b.watch(Mode::Peek);
    let frame = tokio::time::timeout(Duration::from_secs(5), rx.wait_for(Option::is_some))
        .await
        .unwrap()
        .unwrap()
        .clone()
        .unwrap();
    let v: Value = serde_json::from_str(&frame.json).unwrap();
    assert_eq!(
        (v["type"].as_str(), v["data"].as_str()),
        (Some("browser.frame"), Some("QUJD"))
    );
    assert_eq!(
        (v["width"].as_u64(), v["mode"].as_str()),
        (Some(500), Some("peek"))
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // The same picture is not sent again; the file Playwright saved is gone.
    assert!(!rx.has_changed().unwrap());
    assert!(f.called("browser_take_screenshot") >= 2);
    assert!(!b.out_dir.join("page-1.jpeg").exists());
    assert_eq!(b.state_json()["streaming"], true);
    drop(guard);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let n = f.called("browser_take_screenshot");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        f.called("browser_take_screenshot"),
        n,
        "stops when nobody watches"
    );
    b.close().await;
}

/// End to end with the real Playwright MCP and Chromium, installed by
/// Iris: `iris engines install browser --dir DIR`, then
/// `IRIS_REAL_BROWSER=DIR cargo test real_playwright -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn real_playwright() {
    use std::io::{Read, Write};
    let root = PathBuf::from(std::env::var("IRIS_REAL_BROWSER").expect("IRIS_REAL_BROWSER"));
    // A local site: a sign-in page with a big password field at the top.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let body = if req.starts_with("GET /login") {
                "<html><head><title>Iris smoke sign in</title></head><body style='margin:0'>\
                 <input type=password aria-label=Password style='position:absolute;top:100px;left:0;width:500px;height:100px'>\
                 <p style='margin-top:250px'>Sign in to continue</p></body></html>"
            } else {
                "<html><head><title>Iris smoke home</title></head><body><h1>Welcome home</h1></body></html>"
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let f = Fake::new();
    let b = f.browser();
    let server = launch::find_in("", Some(&root), &crate::harness::resolve_command).unwrap();
    assert!(server.chromium.is_some() && server.user_agent.is_some());
    *lock(&b.server_override) = Some(server);
    let g = b.begin_turn(&f.owner, &f.conv, "m", &[]);
    let token = g.token().to_string();

    let listed = tools::list(&f.state, &token).await;
    let names: Vec<_> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    eprintln!("agent tools: {names:?}");
    assert!(names.contains(&"browser_navigate".to_string()));
    assert!(
        !names
            .iter()
            .any(|n| n.contains("run_code") || n == "browser_evaluate")
    );

    // Chrome's sandbox: where this system blocks it, the owner's OK runs
    // without.
    match b.ensure().await {
        Ok(_) => eprintln!("started with Chrome's sandbox"),
        Err(e) => {
            assert_eq!(e.to_string(), SANDBOX_UNAVAILABLE, "{e}");
            eprintln!("sandbox blocked here; running without it (owner's OK)");
            b.set_allow_no_sandbox(true).await;
        }
    }

    let login = format!("http://127.0.0.1:{port}/login");
    let r = f
        .agent(&token, "browser_navigate", json!({"url": login}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert!(text(&r).contains("Sign in to continue"), "{r}");
    assert_eq!(b.state_json()["title"], "Iris smoke sign in");

    let run = b.running().unwrap();
    let ua = b
        .tool(
            &run,
            "browser_evaluate",
            &json!({"function": "() => navigator.userAgent"}),
            QUICK,
        )
        .await
        .unwrap();
    assert!(!text_of(&ua).contains("HeadlessChrome"), "{ua}");

    // Live view.
    let (watching, mut rx) = b.watch(Mode::Full);
    let frame = tokio::time::timeout(Duration::from_secs(10), rx.wait_for(Option::is_some))
        .await
        .unwrap()
        .unwrap()
        .clone()
        .unwrap();
    let v: Value = serde_json::from_str(&frame.json).unwrap();
    assert!(v["data"].as_str().unwrap().starts_with("/9j/"), "a JPEG");
    eprintln!(
        "full frame: {} base64 bytes",
        v["data"].as_str().unwrap().len()
    );

    // The owner signs in: tap the password field, type.
    b.take_over(&f.owner, "dev").await.unwrap();
    b.user_input("dev", &json!({"kind": "tap", "x": 0.5, "y": 0.15}))
        .await
        .unwrap();
    b.user_input("dev", &json!({"kind": "text", "text": "hunter2-sëcret"}))
        .await
        .unwrap();
    let typed = b
        .tool(
            &run,
            "browser_evaluate",
            &json!({"function": "() => document.querySelector('input').value"}),
            QUICK,
        )
        .await
        .unwrap();
    assert!(text_of(&typed).contains("hunter2-sëcret"), "{typed}");
    b.hand_back();
    for tool in ["browser_snapshot", "browser_take_screenshot"] {
        let r = f.agent(&token, tool, json!({})).await;
        assert_eq!(text(&r), PRIVATE_PAGE, "{tool}");
    }
    // Moving on opens the browser to the agent again.
    let home = format!("http://127.0.0.1:{port}/home");
    let r = f
        .agent(&token, "browser_navigate", json!({"url": home}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert!(
        text(&r).contains("Welcome home") && !text(&r).contains("hunter2"),
        "{r}"
    );
    let r = f.agent(&token, "browser_take_screenshot", json!({})).await;
    assert!(
        r["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["type"] == "image"),
        "{r}"
    );

    drop(watching);
    drop(g);
    b.close().await;
    assert!(b.running().is_none());
    // Chromium went with the server.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let profile = format!("--user-data-dir={}", b.profile_dir.display());
    let left = std::process::Command::new("pgrep")
        .args(["-f", &profile])
        .output()
        .unwrap();
    assert!(
        left.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&left.stdout)
    );
}
