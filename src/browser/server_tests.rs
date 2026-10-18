//! Iris's browser against a stand-in Playwright MCP server (a tiny MCP
//! server in shell, answering like Playwright MCP does): starting it, the
//! agent's calls, take over, the private-page rule, saved-login fills, the
//! sandbox consent, and the live view.

use super::*;
use crate::state::AppState;

/// Answers like Playwright MCP (synthetic results modeled on its markdown).
/// Works in its working directory: `url` holds the current address,
/// `marked` exists while the current document is marked private (a new
/// page from `browser_navigate` drops it), `field` whether the focused
/// field is private (default true), `frame` whether it is in another
/// site's frame, `tabs` how many tabs are open, `modal` a pending dialog
/// or file picker line, `nosandbox` makes a sandboxed start fail,
/// `pagetext` the page's text, `fields` what its fields hold (snapshots
/// show it), `typed` what the owner typed into a private field (Iris reads
/// it back; `capturefail` makes that fail, `captured` exists once it
/// worked for the current document), `calls.log` records calls.
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
      if [ -f modal ] && [ "$tool" != browser_tabs ]; then
        m=$(cat modal)
        case "$tool:$m" in
          browser_handle_dialog:*dialog*|browser_file_upload:*file_upload*)
            rm -f modal
            reply "### Page\\n- Page URL: $url" '' '' ;;
          *)
            reply "### Error\\nError: Tool \\\"$tool\\\" does not handle the modal state.\\n### Modal state\\n$m" '' ',"isError":true' ;;
        esac
        continue
      fi
      case "$tool" in
        browser_tabs)
          n=$(cat tabs 2>/dev/null || echo 1)
          if [ -f nosandbox ] && grep -q '"chromiumSandbox":true' "$cfg"; then
            reply '### Error\nError: browserBackend.callTool: Target page, context or browser has been closed\nBrowser logs:\nChromium sandboxing failed!\n[pid=1][err] [1:1:FATAL:zygote_host_impl_linux.cc(130)] No usable sandbox!' '' ',"isError":true'
          else
            list="### Result"; i=0
            while [ "$i" -lt "$n" ]; do list="$list\\n- $i: [Sign in]($url)"; i=$((i+1)); done
            reply "$list" '' ''
          fi ;;
        browser_navigate)
          to=$(printf '%s' "$line" | sed -n 's/.*"url":"\([^"]*\)".*/\1/p')
          printf '%s' "$to" > url
          rm -f marked captured
          reply "### Page\\n- Page URL: $to\\n- Page Title: Sign in\\n### Snapshot\\n- [Snapshot](out/page-2.yml)" '' '' ;;
        browser_run_code_unsafe)
          case "$line" in
            *iris:capture*)
              m=false; [ -f marked ] && m=true
              c=true; [ -f capturefail ] && c=false
              v=$(cat typed 2>/dev/null)
              [ -n "$v" ] && [ "${#v}" -lt 4 ] && { c=false; v=''; }
              vals=''; [ -n "$v" ] && vals="\\\"$v\\\""
              [ "$c" = true ] && : > captured
              reply "### Result\n[{\\\"url\\\":\\\"$url\\\",\\\"captured\\\":$c,\\\"marked\\\":$m,\\\"values\\\":[$vals]}]" '' '' ;;
            *iris:shot*)
              reply '### Result\n{\"data\":\"TUFTSw==\",\"type\":\"png\"}' '' '' ;;
            *aria-ref=*|*hasFocus*)
              # Iris's fills: `fieldframe` is the field's frame address
              # (default: the page's), `noref` hides the field, `kinds`
              # lists input types by ref (`e4 password`; the owner's
              # focused field is `focused`; default text).
              ff=$(cat fieldframe 2>/dev/null || echo "$url")
              r=$(printf '%s' "$line" | sed -n 's/.*aria-ref=\([a-z0-9]*\).*/\1/p')
              [ -n "$r" ] || r=focused
              f2=$(grep "^$r " frames 2>/dev/null | cut -d' ' -f2)
              [ -n "$f2" ] && ff=$f2
              k=$(grep "^$r " kinds 2>/dev/null | cut -d' ' -f2)
              [ -n "$k" ] || k=text
              if [ -f noref ]; then
                reply "### Result\\n{\\\"found\\\":false}" '' ''
              else
                case "$line" in
                  *'h.click('*)
                    reply "### Result\\n{\\\"clicked\\\":true}" '' '' ;;
                  *'const values'*)
                    case "$line" in *'if (true) await page.evaluate'*) : > marked ;; esac
                    reply "### Result\\n{\\\"filled\\\":true}" '' '' ;;
                  *"h.press('Enter'"*)
                    reply "### Result\\n{\\\"clicked\\\":true}" '' '' ;;
                  *)
                    reply "### Result\\n{\\\"found\\\":true,\\\"top\\\":\\\"$url\\\",\\\"frame\\\":\\\"$ff\\\",\\\"tag\\\":\\\"input\\\",\\\"kind\\\":\\\"$k\\\"}" '' '' ;;
                esac
              fi ;;
            *'walk(document)'*)
              t="$(cat fields 2>/dev/null) $(cat pagetext 2>/dev/null)"
              if [ -f fieldsfail ]; then reply "### Result\\nnull" '' ''
              else reply "### Result\\n\\\"$t\\\"" '' ''; fi ;;
            *innerText*)
              t=$(cat pagetext 2>/dev/null)
              reply "### Result\\n\\\"$t\\\"" '' '' ;;
            *captureScreenshot*)
              # `hangcast`: the first picture is never answered.
              if [ -f hangcast ]; then rm -f hangcast
              else reply '### Result\n\"QUJD\"' '' ''; fi ;;
            *insertText*)
              reply "### Page\\n- Page URL: $url" '' '' ;;
            *activeElement*)
              p=$(cat field 2>/dev/null || echo true)
              fr=$(cat frame 2>/dev/null || echo false)
              case "$line" in *'priv = true ||'*) p=true ;; esac
              case "$line" in *'if (true && priv)'*) [ "$p" = true ] && : > marked ;; esac
              reply "### Result\\n{\\\"url\\\":\\\"$url\\\",\\\"private\\\":$p,\\\"frame\\\":$fr}" '' '' ;;
            *)
              m=false; [ -f marked ] && m=true
              c=false; [ -f captured ] && c=true
              reply "### Result\\n{\\\"url\\\":\\\"$url\\\",\\\"title\\\":\\\"Sign in\\\",\\\"private\\\":$m,\\\"captured\\\":$c}" '' '' ;;
          esac ;;
        browser_snapshot)
          t=$(cat fields 2>/dev/null)
          reply "### Page\\n- Page URL: $url\\n- Page Title: Sign in\\n### Snapshot\\n- textbox \\\"Password\\\" [ref=e4]: $t" '' '' ;;
        browser_take_screenshot)
          [ -f slowshot ] && sleep 1
          [ -f pagetext_after ] && mv pagetext_after pagetext
          mkdir -p out && echo x > out/page-1.jpeg && echo "page said hello" > out/console-1.log
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

    /// A new document at `url` (nothing marked on it).
    fn load(&self, url: &str) {
        self.set("url", url);
        let _ = std::fs::remove_file(self.file("marked"));
        let _ = std::fs::remove_file(self.file("captured"));
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

    // Only reviewed tools, without file options, then Iris's own fill
    // tools; listing starts no browser.
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
            "browser_tabs",
            "fill_saved_login",
            "fill_saved_card",
            "restart_browser",
            "ask_to_save_login"
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
    // A wrong argument name is refused with the real one, never forwarded.
    let before = f.calls().len();
    let r = f
        .agent(
            &token,
            "browser_navigate",
            json!({"file_path": "https://site.test/other"}),
        )
        .await;
    assert_eq!(r["isError"], true, "{r}");
    assert!(text(&r).contains("no argument `file_path`"), "{r}");
    assert!(text(&r).contains("`url` (required)"), "{r}");
    assert_eq!(f.calls().len(), before);

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
        .position(|(t, _)| t == "browser_mouse_click_xy")
        .unwrap();
    assert_eq!(calls[i].1, json!({"x": 250.0, "y": 250.0}));
    let keys: Vec<_> = calls
        .iter()
        .filter(|(t, _)| t == "browser_press_key")
        .map(|(_, a)| a["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["p", "Backspace"]);
    assert!(calls.iter().any(|(t, a)| {
        t == "browser_run_code_unsafe"
            && a["code"]
                .as_str()
                .unwrap()
                .contains("keyboard.insertText(\"ä\")")
    }));
    // The field was checked (and the page marked) before anything typed.
    let checked = calls
        .iter()
        .position(|(t, a)| {
            t == "browser_run_code_unsafe" && a["code"].to_string().contains("activeElement")
        })
        .unwrap();
    let typed = calls
        .iter()
        .position(|(t, _)| t == "browser_press_key")
        .unwrap();
    assert!(checked < typed);
    b.hand_back();

    // What was typed is read back from Iris's side before the agent's next
    // call, so the page stays usable: the value is hidden from what the
    // agent reads, and its screenshot is Iris's own, with private details
    // blacked out (never Playwright's plain one).
    f.set("typed", "pw-typed-77");
    f.set("fields", "sam pw-typed-77");
    let before = f.calls().len();
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(
        text(&r).contains("[hidden]") && !r.to_string().contains("pw-typed-77"),
        "{r}"
    );
    let captures = |f: &Fake| {
        f.calls()
            .iter()
            .filter(|(t, a)| {
                t == "browser_run_code_unsafe" && a["code"].to_string().contains("iris:capture")
            })
            .count()
    };
    assert_eq!(captures(&f), 1);
    assert!(
        f.calls()[before..]
            .iter()
            .all(|(t, a)| t == "browser_snapshot"
                || (t == "browser_run_code_unsafe"
                    && !a["code"].to_string().contains("pw-typed-77")))
    );
    let shots = f.called("browser_take_screenshot");
    let r = f.agent(&token, "browser_take_screenshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(text(&r).contains("blacked out"), "{r}");
    assert_eq!(r["content"][1]["data"], "TUFTSw==");
    assert_eq!(f.called("browser_take_screenshot"), shots);
    let shot = f
        .calls()
        .into_iter()
        .rev()
        .find(|(t, a)| {
            t == "browser_run_code_unsafe" && a["code"].to_string().contains("iris:shot")
        })
        .unwrap();
    // The masks cover what was typed (searched for by Playwright itself).
    assert!(shot.1["code"].to_string().contains("pw-typed-77"));
    // Read once: later calls do not read it again.
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert_eq!(captures(&f), 1);
    // A fragment, another address in the same document, the page moving
    // on, or coming back to its address: all open, with the value hidden.
    for url in [
        "https://site.test/login#password",
        "https://site.test/login/step-2",
    ] {
        f.set("url", url);
        let r = f.agent(&token, "browser_snapshot", json!({})).await;
        assert_ne!(r["isError"], true, "{url}: {r}");
    }
    f.load("https://site.test/home");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    f.load("https://site.test/login");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert!(!r.to_string().contains("pw-typed-77"), "{r}");
    assert_ne!(r["isError"], true, "{r}");

    // What cannot be read back keeps the page closed, as before: the
    // document, another address in it, and its address after it is gone
    // (history may restore what was typed).
    b.take_over(&f.owner, "dev").await.unwrap();
    f.set("capturefail", "");
    b.user_input("dev", &json!({"kind": "text", "text": "x"}))
        .await
        .unwrap();
    b.hand_back();
    for tool in ["browser_snapshot", "browser_take_screenshot"] {
        let r = f.agent(&token, tool, json!({})).await;
        assert_eq!(r["isError"], true);
        assert_eq!(text(&r), PRIVATE_PAGE, "{tool}");
    }
    f.set("url", "https://site.test/login/step-3");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_eq!(text(&r), PRIVATE_PAGE);
    f.load("https://site.test/home");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    f.load("https://site.test/login");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_eq!(text(&r), PRIVATE_PAGE);
    std::fs::remove_file(f.file("capturefail")).unwrap();
    // So does a value too short to look for.
    f.load("https://site.test/pin");
    b.take_over(&f.owner, "dev").await.unwrap();
    f.set("typed", "42");
    b.user_input("dev", &json!({"kind": "text", "text": "42"}))
        .await
        .unwrap();
    b.hand_back();
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_eq!(text(&r), PRIVATE_PAGE);
    f.load("https://site.test/home");
    let _ = std::fs::remove_file(f.file("typed"));

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

    // One check covers a burst of typing (each is a round trip with
    // Playwright's settle wait); a key, a tap, or time passing checks
    // again, so a page moving focus by itself is caught.
    let checks = |f: &Fake| {
        f.calls()
            .iter()
            .filter(|(t, a)| {
                t == "browser_run_code_unsafe" && a["code"].to_string().contains("activeElement")
            })
            .count()
    };
    b.take_over(&f.owner, "dev").await.unwrap();
    let n = checks(&f);
    for c in ["a", "b", "c"] {
        b.user_input("dev", &json!({"kind": "text", "text": c}))
            .await
            .unwrap();
    }
    assert_eq!(checks(&f), n + 1);
    b.user_input("dev", &json!({"kind": "key", "key": "Tab"}))
        .await
        .unwrap();
    b.user_input("dev", &json!({"kind": "text", "text": "d"}))
        .await
        .unwrap();
    assert_eq!(checks(&f), n + 2);
    f.set("field", "true");
    b.user_input("dev", &json!({"kind": "text", "text": "1"}))
        .await
        .unwrap();
    assert_eq!(checks(&f), n + 2);
    tokio::time::sleep(FIELD_CHECK_FOR).await;
    b.user_input("dev", &json!({"kind": "text", "text": "23456"}))
        .await
        .unwrap();
    assert_eq!(checks(&f), n + 3);
    b.hand_back();
    f.set("capturefail", "");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_eq!(text(&r), PRIVATE_PAGE);
    // Navigating away from a private page is always allowed and opens it;
    // reloading its address does not.
    let r = f
        .agent(
            &token,
            "browser_navigate",
            json!({"url": "https://site.test/home"}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_eq!(text(&r), PRIVATE_PAGE);
    let r = f
        .agent(
            &token,
            "browser_navigate",
            json!({"url": "https://site.test/next"}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    std::fs::remove_file(f.file("capturefail")).unwrap();

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
    // Its own site, but the focused field is in another site's frame.
    f.set("url", "https://site.test/login");
    f.set("fieldframe", "https://ads.test/frame");
    let err = fill::fill(&f.state, "dev", &id, "password")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("another site"), "{err}");
    assert!(fills(&f).is_empty());
    // Its own site: the focused element itself is filled (replacing what
    // it held), never typed key by key, and the page closes to the agent
    // while the password is in its field.
    std::fs::remove_file(f.file("fieldframe")).unwrap();
    fill::fill(&f.state, "dev", &id, "password").await.unwrap();
    f.set("fields", "sam Pw-1234");
    let done = fills(&f);
    assert_eq!(done.len(), 1);
    assert!(done[0].0.contains("hasFocus") && done[0].0.contains("Pw-1234"));
    assert!(done[0].0.contains("h.fill(v, "));
    assert_eq!(f.called("browser_press_key"), 0);
    // Iris knows the value, so the document is not marked for good.
    assert!(!f.file("marked").exists());
    b.hand_back();
    let g = b.begin_turn(&f.owner, &f.conv, "m", &[]);
    let token = g.token().to_string();
    // While the password is in its field the page stays usable: actions
    // go through, a screenshot is Iris's own with it blacked out, and only
    // searching the page's text waits.
    let clicks = f.called("browser_click");
    for (tool, args) in [
        ("browser_click", json!({"target": "e3"})),
        ("browser_type", json!({"target": "e4", "text": "x"})),
        ("browser_press_key", json!({"key": "Enter"})),
    ] {
        let r = f.agent(&token, tool, args).await;
        assert_ne!(r["isError"], true, "{tool}: {r}");
    }
    assert_eq!(f.called("browser_click"), clicks + 1);
    let r = f.agent(&token, "browser_take_screenshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(!r.to_string().contains("QUJD") && r.to_string().contains("TUFTSw=="));
    let r = f
        .agent(&token, "browser_find", json!({"text": "Pw-1"}))
        .await;
    assert!(text(&r).contains("searching it"), "{r}");
    // Reading it works, with the password hidden.
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(
        text(&r).contains("[hidden]") && !r.to_string().contains("Pw-1234"),
        "{r}"
    );
    // Once the field no longer holds it (the form was sent or cleared, on
    // the same address), the page is the agent's again.
    f.set("fields", "sam");
    let r = f
        .agent(&token, "browser_find", json!({"text": "Pw-1"}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    // A frame that could not be read counts as still holding it.
    f.set("fieldsfail", "");
    let r = f
        .agent(&token, "browser_find", json!({"text": "Pw-1"}))
        .await;
    assert!(text(&r).contains("searching it"), "{r}");
    std::fs::remove_file(f.file("fieldsfail")).unwrap();

    // A saved password too short to look for closes the page until it
    // moves on, as if the owner typed it.
    let short = "login-2".to_string();
    let secret = f
        .state
        .vault
        .encrypt(json!({"password": "a1!"}).to_string().as_bytes(), &short)
        .unwrap();
    f.state
        .db
        .with(|c| {
            crate::entities::insert_credential(
                c,
                &short,
                &f.owner,
                "login",
                "Short",
                &json!({"site": "site.test", "username": "sam"}),
                &secret,
            )
        })
        .unwrap();
    b.take_over(&f.owner, "dev").await.unwrap();
    fill::fill(&f.state, "dev", &short, "password")
        .await
        .unwrap();
    b.hand_back();
    assert!(f.file("marked").exists());
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_eq!(text(&r), PRIVATE_PAGE);
    drop(g);
    let used = f
        .state
        .db
        .with(|c| crate::entities::get_credential(c, &f.owner, &id))
        .unwrap()
        .unwrap();
    assert!(used.last_used_at.is_some());
    b.close().await;
}

/// The `run_code` calls that filled a field, and whether each pressed Enter.
fn fills(f: &Fake) -> Vec<(String, bool)> {
    f.calls()
        .into_iter()
        .filter(|(t, a)| {
            t == "browser_run_code_unsafe" && a["code"].as_str().unwrap().contains("const values")
        })
        .map(|(_, a)| {
            let code = a["code"].as_str().unwrap().to_string();
            let enter = code.contains("if (filled && true)");
            (code, enter)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_fills_saved_logins_without_seeing_them() {
    let f = Fake::new();
    let b = f.browser();
    let id = "login-1".to_string();
    let secret = f
        .state
        .vault
        .encrypt(json!({"password": "Pw-5678"}).to_string().as_bytes(), &id)
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
                &json!({"site": "site.test", "username": "sam"}),
                &secret,
            )
        })
        .unwrap();
    let g = b.begin_turn(&f.owner, &f.conv, "m", std::slice::from_ref(&f.owner));
    let token = g.token().to_string();
    let listed = tools::list(&f.state, &token).await;
    assert!(
        listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "fill_saved_login")
    );
    let login = json!({"username_ref": "e3", "password_ref": "e4", "submit": true});
    f.set("kinds", "e4 password\n");

    // Another site, a field in another site's frame, a plain-HTTP page, a
    // bad ref: nothing is typed.
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://evil.test/login"}),
    )
    .await;
    let r = f.agent(&token, "fill_saved_login", login.clone()).await;
    assert!(text(&r).starts_with("not found (no saved login"), "{r}");
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://site.test/login"}),
    )
    .await;
    // The login does not cover that frame's site: nothing is typed, and
    // the owner is asked (the card names both sites).
    f.set("fieldframe", "https://ads.test/frame");
    let r = f.agent(&token, "fill_saved_login", login.clone()).await;
    assert!(
        text(&r).starts_with("not found (no saved login for site.test and ads.test)"),
        "{r}"
    );
    std::fs::remove_file(f.file("fieldframe")).unwrap();
    let r = f
        .agent(
            &token,
            "fill_saved_login",
            json!({"password_ref": "x\"); evil(\""}),
        )
        .await;
    assert!(text(&r).contains("no field for ref"), "{r}");
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "http://site.test/login"}),
    )
    .await;
    let r = f.agent(&token, "fill_saved_login", login.clone()).await;
    assert!(text(&r).starts_with("not found"), "{r}");
    // A password only goes into a password field (a search box could put
    // it in the address).
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://site.test/login"}),
    )
    .await;
    let r = f
        .agent(&token, "fill_saved_login", json!({"password_ref": "e3"}))
        .await;
    assert!(text(&r).contains("is not a password field"), "{r}");
    assert!(fills(&f).is_empty());

    // Its site: both fields filled by Iris, Enter only in the last; the
    // answer never carries the values, and says the password left the page.
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://site.test/login"}),
    )
    .await;
    let r = f.agent(&token, "fill_saved_login", login.clone()).await;
    assert_eq!(text(&r), fill::SUBMITTED, "{r}");
    assert!(!r.to_string().contains("Pw-5678") && !r.to_string().contains("sam"));
    let done = fills(&f);
    assert_eq!(done.len(), 2);
    assert!(done[0].0.contains("aria-ref=e3") && done[0].0.contains(r#"["sam"]"#));
    assert!(!done[0].1);
    assert!(done[1].0.contains("aria-ref=e4") && done[1].0.contains("Pw-5678"));
    assert!(done[1].1);
    // A sign-in button instead of Enter: clicked by Iris, only on the site.
    let clicks = |f: &Fake| {
        f.calls()
            .iter()
            .filter(|(t, a)| {
                t == "browser_run_code_unsafe" && a["code"].to_string().contains("h.click(")
            })
            .count()
    };
    f.set("frames", "e9 https://evil.test/x\n");
    let r = f
        .agent(
            &token,
            "fill_saved_login",
            json!({"password_ref": "e4", "submit_ref": "e9"}),
        )
        .await;
    assert!(text(&r).contains("not on the same site"), "{r}");
    assert_eq!(clicks(&f), 0);
    f.set("frames", "");
    // This time the sign-in does not take the password off the page (the
    // address does not change either): the answer says so after a while.
    f.set("fields", "sam Pw-5678");
    let started = std::time::Instant::now();
    let r = f
        .agent(
            &token,
            "fill_saved_login",
            json!({"password_ref": "e4", "submit_ref": "e9"}),
        )
        .await;
    assert_eq!(text(&r), fill::STILL_THERE, "{r}");
    assert!(started.elapsed() >= Duration::from_secs(4));
    assert_eq!(clicks(&f), 1);
    assert!(!fills(&f).last().unwrap().1, "no Enter with a button");
    // The agent can read the page and see it to find out what happened
    // (an error, a check box to tick), without the password, and act on it.
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(
        text(&r).contains("[hidden]") && !r.to_string().contains("Pw-5678"),
        "{r}"
    );
    let r = f.agent(&token, "browser_take_screenshot", json!({})).await;
    assert_eq!(r["content"][1]["data"], "TUFTSw==", "{r}");
    let r = f
        .agent(&token, "browser_click", json!({"target": "e3"}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    // Filling still works there (the password step of a two-step form).
    let r = f
        .agent(&token, "fill_saved_login", json!({"password_ref": "e4"}))
        .await;
    assert_eq!(text(&r), "filled", "{r}");
    // The form takes it: the page is open again.
    f.set("fields", "sam");
    let r = f
        .agent(&token, "browser_click", json!({"target": "e3"}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    // The password echoed anywhere later (an address, a title, a dialog)
    // is hidden from the agent; a page showing it is pictured with it
    // blacked out (Playwright's own text search finds it for the mask).
    f.load("https://site.test/search?q=Pw-5678");
    let r = f
        .agent(&token, "browser_tabs", json!({"action": "list"}))
        .await;
    assert!(
        text(&r).contains("q=[hidden]") && !r.to_string().contains("Pw-5678"),
        "{r}"
    );
    f.set("pagetext", "You searched for Pw-5678");
    let shots = f.called("browser_take_screenshot");
    let r = f.agent(&token, "browser_take_screenshot", json!({})).await;
    assert_eq!(r["content"][1]["data"], "TUFTSw==", "{r}");
    assert_eq!(f.called("browser_take_screenshot"), shots);
    // No searching it (a match or not would tell what is hidden), or
    // naming elements by text; other actions go on.
    for (tool, args) in [
        ("browser_find", json!({"text": "p=P"})),
        ("browser_wait_for", json!({"text": "Pw-5"})),
        ("browser_click", json!({"target": "text=/Pw-5/"})),
    ] {
        let r = f.agent(&token, tool, args).await;
        assert!(text(&r).contains("searching it"), "{tool}: {r}");
    }
    let click = json!({"target": "e3", "element": "Search"});
    let r = f.agent(&token, "browser_click", click.clone()).await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    // Once it is gone, searching works again.
    f.set("pagetext", "Welcome back");
    let r = f
        .agent(&token, "browser_find", json!({"text": "Welcome"}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    // The mask code names the secrets only to Playwright's own text
    // search and field check, never in what the page's scripts run.
    let shot = f
        .calls()
        .into_iter()
        .rev()
        .find(|(t, a)| {
            t == "browser_run_code_unsafe" && a["code"].to_string().contains("iris:shot")
        })
        .unwrap();
    let code = shot.1["code"].as_str().unwrap().to_string();
    let in_page: Vec<&str> = code
        .match_indices("f.evaluate(")
        .map(|(i, _)| &code[i..])
        .collect();
    assert!(!in_page.is_empty());
    for part in in_page {
        let body = &part[..part.find("}, attr)").unwrap_or(part.len())];
        assert!(!body.contains("Pw-5678"), "a secret went into the page");
    }
    b.hand_back();
    // A dialog can still be answered after a fill (even on a closed page).
    f.set(
        "modal",
        r#"- [\"alert\" dialog with message \"Hi\"]: can be handled by browser_handle_dialog"#,
    );
    let r = f
        .agent(&token, "browser_handle_dialog", json!({"accept": true}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    drop(g);
    b.close().await;

    // After the browser closes and starts again, nothing was filled in this
    // run, but saved secrets stay hidden from the first result on (a site
    // may keep a value from an earlier visit).
    let g = b.begin_turn(&f.owner, &f.conv, "m", std::slice::from_ref(&f.owner));
    let token = g.token().to_string();
    assert!(b.filled_secrets().is_empty() && b.running().is_none());
    // The very first call starts the browser and lands on a page whose
    // address carries the saved password.
    let r = f
        .agent(
            &token,
            "browser_navigate",
            json!({"url": "https://site.test/kept?remembered=Pw-5678"}),
        )
        .await;
    assert!(
        text(&r).contains("remembered=[hidden]") && !r.to_string().contains("Pw-5678"),
        "{r}"
    );
    assert!(!b.state_json().to_string().contains("Pw-5678"));
    let r = f
        .agent(&token, "browser_tabs", json!({"action": "list"}))
        .await;
    assert!(!r.to_string().contains("Pw-5678"), "{r}");
    f.set("pagetext", "Your password is Pw-5678");
    let r = f.agent(&token, "browser_take_screenshot", json!({})).await;
    assert_eq!(r["content"][1]["data"], "TUFTSw==", "{r}");
    drop(g);
    b.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playwrights_own_screenshots_leave_no_files() {
    // No secret known: Playwright's own screenshot, passed through.
    let f = Fake::new();
    let b = f.browser();
    let g = b.begin_turn(&f.owner, &f.conv, "m", std::slice::from_ref(&f.owner));
    let token = g.token().to_string();
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://site.test/home"}),
    )
    .await;
    let r = f.agent(&token, "browser_take_screenshot", json!({})).await;
    assert_eq!(r["content"][1]["data"], "QUJD", "{r}");
    // Nothing Playwright wrote stays (the picture came back inline; its
    // console log may hold what the page printed).
    let files = || {
        std::fs::read_dir(&b.out_dir)
            .map(|d| d.count())
            .unwrap_or(0)
    };
    assert_eq!(files(), 0);
    // Also when the call was abandoned (its turn stopped) and the file
    // came after, with no other browser call to tidy up meanwhile.
    f.set("slowshot", "");
    let stopped = b.begin_turn(&f.owner, &f.conv, "m2", std::slice::from_ref(&f.owner));
    let shots = f.called("browser_take_screenshot");
    let call = {
        let (st, tk) = (f.state.clone(), stopped.token().to_string());
        tokio::spawn(
            async move { tools::call(&st, &tk, "browser_take_screenshot", &json!({})).await },
        )
    };
    for _ in 0..100 {
        if f.called("browser_take_screenshot") > shots {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        f.called("browser_take_screenshot") > shots,
        "the picture was not asked for"
    );
    drop(stopped);
    let r = call.await.unwrap();
    assert_eq!(text(&r), TURN_ENDED);
    let mut written = false;
    for _ in 0..60 {
        if files() > 0 {
            written = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(written, "the late picture was never written");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(files(), 0, "the late picture stayed");
    std::fs::remove_file(f.file("slowshot")).unwrap();
    drop(g);
    b.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_fills_a_card_only_after_the_owner_approves() {
    let f = Fake::new();
    let b = f.browser();
    let id = "card-1".to_string();
    let secret = f
        .state
        .vault
        .encrypt(
            json!({"number": "4111111111111111", "cvc": "737"})
                .to_string()
                .as_bytes(),
            &id,
        )
        .unwrap();
    f.state
        .db
        .with(|c| {
            crate::entities::insert_credential(
                c,
                &id,
                &f.owner,
                "card",
                "Everyday",
                &json!({"last4": "1111", "brand": "visa", "exp_month": 4, "exp_year": 2031,
                        "cardholder": "S Doe"}),
                &secret,
            )
        })
        .unwrap();
    let g = b.begin_turn(&f.owner, &f.conv, "m", std::slice::from_ref(&f.owner));
    let token = g.token().to_string();
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://shop.test/pay"}),
    )
    .await;
    // The fields are in a payment provider's frame: the owner sees it.
    f.set("fieldframe", "https://pay.test/card");
    let args = json!({"number_ref": "e5", "exp_month_ref": "e6", "exp_year_ref": "e7",
                      "cvc_ref": "e8", "name_ref": "e9"});
    let pending = || async {
        for _ in 0..200 {
            let msgs = f
                .state
                .db
                .with(|c| crate::db::list_messages(c, &f.conv, None, 50))
                .unwrap();
            if let Some(m) = msgs.into_iter().find(|m| {
                m.card
                    .as_ref()
                    .is_some_and(|c| c["service"] == "wallet" && c["state"] == "pending")
            }) {
                return m;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no approval card");
    };
    let decide = |m: crate::db::Message, approve: bool| {
        let mut card = m.card.clone().unwrap();
        card["state"] = if approve { "approved" } else { "denied" }.into();
        card["decided_at"] = crate::util::now_ms().into();
        f.state
            .db
            .with(|c| crate::db::update_message_card(c, &m.id, &card))
            .unwrap();
        crate::gateway::decided(&f.state, &m.id, approve);
    };

    // The owner says no: nothing is typed.
    let call = {
        let (st, tk, a) = (f.state.clone(), token.clone(), args.clone());
        tokio::spawn(async move { tools::call(&st, &tk, "fill_saved_card", &a).await })
    };
    let m = pending().await;
    let card = m.card.as_ref().unwrap();
    assert!(
        card["title"]
            .as_str()
            .unwrap()
            .contains("Visa ending 1111 on shop.test")
    );
    assert!(
        card["preview"]["body"]
            .as_str()
            .unwrap()
            .contains("The card fields are on: pay.test:443"),
        "{card}"
    );
    assert_eq!(card["wallet"]["frames"], json!(["https://pay.test:443"]));
    assert!(!card.to_string().contains("4111111111111111"));
    decide(m, false);
    let r = call.await.unwrap();
    assert!(text(&r).starts_with("denied"), "{r}");
    assert!(fills(&f).is_empty());

    // Approved, but meanwhile a field's frame became another site, or the
    // page another origin (a port is enough): nothing is typed.
    for (file, moved, back) in [
        (
            "fieldframe",
            "https://evil.test/card",
            "https://pay.test/card",
        ),
        ("url", "https://shop.test:8443/pay", "https://shop.test/pay"),
    ] {
        let call = {
            let (st, tk, a) = (f.state.clone(), token.clone(), args.clone());
            tokio::spawn(async move { tools::call(&st, &tk, "fill_saved_card", &a).await })
        };
        let m = pending().await;
        f.set(file, moved);
        decide(m, true);
        let r = call.await.unwrap();
        assert!(text(&r).contains("not the one the user approved"), "{r}");
        assert!(fills(&f).is_empty());
        f.set(file, back);
    }

    // Approved: every field filled by Iris, and the page closes.
    let call = {
        let (st, tk, a) = (f.state.clone(), token.clone(), args.clone());
        tokio::spawn(async move { tools::call(&st, &tk, "fill_saved_card", &a).await })
    };
    decide(pending().await, true);
    let r = call.await.unwrap();
    assert_eq!(text(&r), "filled", "{r}");
    assert!(!r.to_string().contains("4111"));
    let done = fills(&f);
    assert_eq!(done.len(), 5);
    assert!(done[0].0.contains("4111111111111111"));
    assert!(
        done[1].0.contains(r#"["04","4","April","Apr"]"#),
        "{}",
        done[1].0
    );
    assert!(done[2].0.contains(r#"["2031","31"]"#));
    assert!(done[3].0.contains("737"));
    assert!(done[4].0.contains("S Doe"));
    // The expiry did not lock the document: while the number and code are
    // in their fields the page stays usable, with them hidden from text
    // and blacked out of pictures; only text searches wait.
    assert!(!f.file("marked").exists());
    f.set("fields", "4111111111111111 04 2031 737 S Doe");
    let r = f.agent(&token, "browser_take_screenshot", json!({})).await;
    assert_eq!(r["content"][1]["data"], "TUFTSw==", "{r}");
    let r = f
        .agent(&token, "browser_click", json!({"target": "e9"}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f
        .agent(&token, "browser_find", json!({"text": "411"}))
        .await;
    assert!(text(&r).contains("searching it"), "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(
        !r.to_string().contains("4111") && !text(&r).contains(" 737"),
        "{r}"
    );
    f.set("fields", "S Doe");
    let r = f
        .agent(&token, "browser_click", json!({"target": "e9"}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    f.set("fields", "");
    // The number and code are hidden from now on; the expiry and name are
    // not (ordinary pages show years and months).
    let hidden = b.filled_secrets();
    assert!(
        hidden.contains(&"4111111111111111".to_string()) && hidden.contains(&"737".to_string())
    );
    for shown in ["2031", "31", "04", "April", "S Doe"] {
        assert!(!hidden.contains(&shown.to_string()), "{shown}");
    }
    // That OK was for one fill; the ones refused above were never used.
    let approved: Vec<Value> = f
        .state
        .db
        .with(|c| crate::db::list_messages(c, &f.conv, None, 50))
        .unwrap()
        .into_iter()
        .filter_map(|m| m.card.filter(|c| c["state"] == "approved"))
        .collect();
    assert_eq!(approved.len(), 3);
    assert_eq!(
        approved
            .iter()
            .filter(|c| c["wallet"]["used_at"].is_i64())
            .count(),
        1
    );

    // Sending: only when every field asked for was filled. A card saved
    // without its code leaves the form unsent for the user.
    let sends = |f: &Fake| {
        f.calls()
            .iter()
            .filter(|(t, a)| {
                let c = a["code"].to_string();
                t == "browser_run_code_unsafe"
                    && !c.contains("const values")
                    && (c.contains("h.click(") || c.contains("h.press('Enter'"))
            })
            .count()
    };
    let nocode = "card-2".to_string();
    let secret = f
        .state
        .vault
        .encrypt(
            json!({"number": "5555555555554444"}).to_string().as_bytes(),
            &nocode,
        )
        .unwrap();
    f.state
        .db
        .with(|c| {
            crate::entities::insert_credential(
                c,
                &nocode,
                &f.owner,
                "card",
                "Spare",
                &json!({"last4": "4444", "brand": "mastercard", "exp_month": 1, "exp_year": 2030}),
                &secret,
            )
        })
        .unwrap();
    for (card, extra, sent) in [
        ("Spare", json!({"submit_ref": "e10"}), false),
        ("Everyday", json!({"submit": true}), true),
    ] {
        let mut a = json!({"number_ref": "e5", "cvc_ref": "e8", "card": card});
        a.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let before = sends(&f);
        let call = {
            let (st, tk) = (f.state.clone(), token.clone());
            tokio::spawn(async move { tools::call(&st, &tk, "fill_saved_card", &a).await })
        };
        decide(pending().await, true);
        let r = call.await.unwrap();
        if sent {
            assert_eq!(text(&r), "filled and submitted", "{r}");
            assert_eq!(sends(&f), before + 1);
        } else {
            assert!(
                text(&r).contains("cvc could not be filled, so nothing was sent"),
                "{r}"
            );
            assert_eq!(sends(&f), before);
        }
    }
    drop(g);
    b.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_restarts_the_browser_keeping_its_profile() {
    let f = Fake::new();
    let b = f.browser();
    let g = b.begin_turn(&f.owner, &f.conv, "m", std::slice::from_ref(&f.owner));
    let token = g.token().to_string();
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://site.test/login"}),
    )
    .await;
    let first = b.running().unwrap();
    std::fs::write(b.profile_dir.join("Cookies"), "kept").unwrap();
    // A page the owner typed a password into, before the restart.
    b.remember_private("https://site.test/login");
    let r = f.agent(&token, "restart_browser", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(text(&r).starts_with("restarted"), "{r}");
    // Closed gracefully (so the profile is saved), then a new browser.
    assert_eq!(f.called("browser_close"), 1);
    let second = b.running().unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(
        std::fs::read_to_string(b.profile_dir.join("Cookies")).unwrap(),
        "kept"
    );
    // Its tabs are gone, and with them what was typed.
    assert!(lock(&b.private).pages.is_empty());
    assert_eq!(b.state_json()["url"], "about:blank");
    // Without a running browser it just opens one.
    b.close().await;
    let r = f.agent(&token, "restart_browser", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(b.running().is_some());
    // Playwright's own close is still never offered.
    let r = f.agent(&token, "browser_close", json!({})).await;
    assert_eq!(text(&r), "unknown tool browser_close");
    drop(g);
    b.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_asks_the_owner_to_save_a_login_for_the_open_site() {
    let f = Fake::new();
    let b = f.browser();
    let mut rx = f.state.hub.subscribe();
    let g = b.begin_turn(&f.owner, &f.conv, "m", std::slice::from_ref(&f.owner));
    let token = g.token().to_string();
    let cards = |f: &Fake| {
        f.state
            .db
            .with(|c| crate::db::list_messages(c, &f.conv, None, 100))
            .unwrap()
            .into_iter()
            .filter(|m| m.kind == "card")
            .collect::<Vec<_>>()
    };
    // No page yet, or one that is not secure: nothing is posted.
    let r = f.agent(&token, "ask_to_save_login", json!({})).await;
    assert!(text(&r).starts_with("not posted (open the site"), "{r}");
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "http://shop.test/login"}),
    )
    .await;
    let r = f.agent(&token, "ask_to_save_login", json!({})).await;
    assert!(
        text(&r).starts_with("not posted (this page is not secure"),
        "{r}"
    );
    assert!(cards(&f).is_empty());
    // The site comes from the page itself (its port kept, its path not);
    // the agent may only suggest a name and username.
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://Shop.test:8443/account/login?next=/cart"}),
    )
    .await;
    let r = f
        .agent(
            &token,
            "ask_to_save_login",
            json!({"label": "Shop", "username": "sam@example.test", "site": "https://evil.test"}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(text(&r).starts_with("posted"), "{r}");
    let posted = cards(&f);
    assert_eq!(posted.len(), 1);
    let card = posted[0].card.clone().unwrap();
    assert_eq!(card["type"], "save_login");
    assert_eq!(card["site"], "https://shop.test:8443");
    assert_eq!(card["host"], "shop.test");
    assert_eq!(card["label"], "Shop");
    assert_eq!(card["username"], "sam@example.test");
    assert_eq!(card["state"], "pending");
    assert_eq!(posted[0].status, "complete");
    // The chat gets it right away.
    let mut created = false;
    while let Ok(e) = rx.try_recv() {
        let v: Value = serde_json::from_str(&e.json).unwrap();
        created |= v["type"] == "message.created" && v["message"]["id"] == posted[0].id;
    }
    assert!(created);
    // Bad suggestions are refused.
    let r = f
        .agent(&token, "ask_to_save_login", json!({"username": "a\nb"}))
        .await;
    assert_eq!(r["isError"], true, "{r}");
    // Not in a job (nobody would see the card).
    let job = b.begin_turn(&f.owner, &f.conv, "j", &[]);
    let r = f.agent(job.token(), "ask_to_save_login", json!({})).await;
    assert!(text(&r).starts_with("not posted (only in a chat"), "{r}");
    assert_eq!(cards(&f).len(), 1);
    drop(job);
    drop(g);
    b.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_saved_login_posts_a_card_for_the_sign_in_page() {
    let f = Fake::new();
    let b = f.browser();
    let g = b.begin_turn(&f.owner, &f.conv, "m", std::slice::from_ref(&f.owner));
    let token = g.token().to_string();
    let cards = |f: &Fake| {
        f.state
            .db
            .with(|c| crate::db::list_messages(c, &f.conv, None, 100))
            .unwrap()
            .into_iter()
            .filter_map(|m| m.card)
            .collect::<Vec<_>>()
    };
    let login = json!({"username_ref": "e3", "password_ref": "e4", "submit": true});
    f.set("kinds", "e4 password\n");
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://shop.test/account/login"}),
    )
    .await;
    let r = f.agent(&token, "fill_saved_login", login.clone()).await;
    assert!(
        text(&r).starts_with("not found (no saved login for shop.test). I posted a card"),
        "{r}"
    );
    let posted = cards(&f);
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0]["type"], "save_login");
    assert_eq!(posted[0]["site"], "https://shop.test");
    assert_eq!(posted[0]["state"], "pending");
    assert_eq!(posted[0]["turn_message_id"], "m");
    assert!(posted[0].get("also_sites").is_none());
    // Asked again while it waits: no second card.
    let r = f.agent(&token, "fill_saved_login", login.clone()).await;
    assert!(text(&r).contains("already in the chat"), "{r}");
    assert_eq!(cards(&f).len(), 1);
    // The sign-in box is another site's frame: the card names both sites.
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://order.shop.test/"}),
    )
    .await;
    f.set("fieldframe", "https://auth.test/sign_in.iframe");
    let r = f.agent(&token, "fill_saved_login", login.clone()).await;
    assert!(text(&r).contains("order.shop.test"), "{r}");
    let posted = cards(&f);
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[1]["site"], "https://order.shop.test");
    assert_eq!(posted[1]["also_sites"], json!(["https://auth.test"]));
    std::fs::remove_file(f.file("fieldframe")).unwrap();
    // A login for the page that does not cover the sign-in frame's site:
    // nothing is typed, and a card asks to extend it.
    let save = |id: &str, meta: Value| {
        let secret = f
            .state
            .vault
            .encrypt(json!({"password": "Pw-5678"}).to_string().as_bytes(), id)
            .unwrap();
        f.state
            .db
            .with(|c| {
                crate::entities::insert_credential(c, id, &f.owner, "login", id, &meta, &secret)
            })
            .unwrap();
    };
    save("covers-page", json!({"site": "https://page.test"}));
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "https://page.test/"}),
    )
    .await;
    f.set("fieldframe", "https://signin.test/box");
    let r = f.agent(&token, "fill_saved_login", login.clone()).await;
    assert!(
        text(&r).starts_with("not found (no saved login for page.test and signin.test)"),
        "{r}"
    );
    let posted = cards(&f);
    assert_eq!(posted.len(), 3);
    assert_eq!(posted[2]["site"], "https://page.test");
    assert_eq!(posted[2]["also_sites"], json!(["https://signin.test"]));
    // A login that names an insecure page and a secure sign-in frame: the
    // page itself must be secure too.
    save(
        "plain-page",
        json!({"site": "http://plain.test", "also_sites": ["https://signin2.test"]}),
    );
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": "http://plain.test/"}),
    )
    .await;
    f.set("fieldframe", "https://signin2.test/box");
    let r = f.agent(&token, "fill_saved_login", login.clone()).await;
    assert!(text(&r).contains("over a secure connection"), "{r}");
    assert_eq!(cards(&f).len(), 3);
    std::fs::remove_file(f.file("fieldframe")).unwrap();
    // A job: nobody would see a card.
    let job = b.begin_turn(&f.owner, &f.conv, "j", &[]);
    f.agent(
        job.token(),
        "browser_navigate",
        json!({"url": "https://other.test/login"}),
    )
    .await;
    let r = f.agent(job.token(), "fill_saved_login", login).await;
    assert!(text(&r).contains("ask the user to add it"), "{r}");
    assert_eq!(cards(&f).len(), 3);
    drop(job);
    drop(g);
    b.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_turns_peek_stays_until_the_owner_takes_over() {
    let f = Fake::new();
    let b = f.browser();
    let mut rx = f.state.hub.subscribe();
    let active = |b: &Browser| b.state_json()["active"].as_array().unwrap().clone();
    let g = b.begin_turn(&f.owner, &f.conv, "m1", std::slice::from_ref(&f.owner));
    f.agent(
        g.token(),
        "browser_navigate",
        json!({"url": "https://site.test/"}),
    )
    .await;
    assert_eq!(
        active(b),
        [json!({"conversation_id": f.conv, "message_id": "m1"})]
    );
    drop(g);
    // The turn ended (asking the owner to take over): its peek stays.
    let done = loop {
        let v: Value = serde_json::from_str(&rx.recv().await.unwrap().json).unwrap();
        if v["type"] == "browser.done" {
            break v;
        }
    };
    assert_eq!(done["message_id"], "m1");
    assert_eq!(done["linger_ms"], LINGER_MS);
    let a = active(b);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["message_id"], "m1");
    assert!(a[0]["ended_at"].as_i64().unwrap() > 0);
    assert!(a[0]["linger_ms"].as_i64().unwrap() > LINGER_MS - 5000);
    // A later browsing turn in the chat takes its place.
    let g = b.begin_turn(&f.owner, &f.conv, "m2", std::slice::from_ref(&f.owner));
    f.agent(g.token(), "browser_snapshot", json!({})).await;
    assert_eq!(
        active(b),
        [json!({"conversation_id": f.conv, "message_id": "m2"})]
    );
    drop(g);
    assert_eq!(active(b)[0]["message_id"], "m2");
    // The owner takes over: it has done its job.
    b.take_over(&f.owner, "dev").await.unwrap();
    assert!(active(b).is_empty());
    // A turn that ends while the owner is in control leaves none.
    let g = b.begin_turn(&f.owner, &f.conv, "m3", std::slice::from_ref(&f.owner));
    let ctx = b.turn(g.token()).unwrap();
    b.mark_used(&ctx);
    drop(g);
    assert!(active(b).is_empty());
    b.hand_back();
    // Nor does one whose browser closed, and closing drops what is left.
    let g = b.begin_turn(&f.owner, &f.conv, "m4", std::slice::from_ref(&f.owner));
    f.agent(g.token(), "browser_snapshot", json!({})).await;
    drop(g);
    assert_eq!(active(b).len(), 1);
    b.close().await;
    assert!(active(b).is_empty());
    let g = b.begin_turn(&f.owner, &f.conv, "m5", std::slice::from_ref(&f.owner));
    let ctx = b.turn(g.token()).unwrap();
    b.mark_used(&ctx);
    drop(g);
    assert!(active(b).is_empty());
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

/// Live-view pictures taken so far.
fn frames(f: &Fake) -> usize {
    f.calls()
        .iter()
        .filter(|(t, a)| {
            t == "browser_run_code_unsafe" && a["code"].to_string().contains("captureScreenshot")
        })
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_picture_that_never_comes_holds_the_live_view_only_briefly() {
    let f = Fake::new();
    let b = f.browser();
    f.set("hangcast", "");
    b.ensure().await.unwrap();
    let (_guard, mut rx) = b.watch(Mode::Full, None);
    // The stuck picture is given up after FRAME_TIMEOUT (not the 20 s of
    // other quick calls) and the next one shows.
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(12), rx.wait_for(Option::is_some))
        .await
        .expect("a picture after the stuck one")
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(7),
        "{:?}",
        started.elapsed()
    );
    assert!(frames(&f) >= 2);
    // Each step of a picture has its own limit inside the page's code, and
    // a DevTools session that stops answering is let go.
    let code = frame_code(500, 55);
    assert!(code.contains("did not answer in time") && code.contains("s.detach()"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_view_sends_new_pictures_and_removes_its_files() {
    let f = Fake::new();
    let b = f.browser();
    b.ensure().await.unwrap();
    let (guard, mut rx) = b.watch(Mode::Peek, Some(216));
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
    assert!(frames(&f) >= 2);
    // Pictures come straight from Chrome at the asked size and quality;
    // nothing is written to disk.
    let code = f
        .calls()
        .into_iter()
        .rev()
        .find_map(|(t, a)| (t == "browser_run_code_unsafe").then(|| a["code"].to_string()))
        .unwrap();
    assert!(
        code.contains("quality: 50") && code.contains("scale: 216 /"),
        "{code}"
    );
    assert_eq!(f.called("browser_take_screenshot"), 0);
    assert!(!b.out_dir.exists() || std::fs::read_dir(&b.out_dir).unwrap().next().is_none());
    assert_eq!(b.state_json()["streaming"], true);
    drop(guard);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let n = frames(&f);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(frames(&f), n, "stops when nobody watches");
    b.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dialogs_file_pickers_and_new_windows_during_take_over() {
    let f = Fake::new();
    let b = f.browser();
    b.ensure().await.unwrap();
    b.take_over(&f.owner, "dev").await.unwrap();
    let tap = json!({"kind": "tap", "x": 0.5, "y": 0.5});

    // A confirmation the owner cannot see is accepted, and the tap goes
    // through after it.
    f.set(
        "modal",
        r#"- [\"confirm\" dialog with message \"Leave site?\"]: can be handled by browser_handle_dialog"#,
    );
    b.user_input("dev", &tap).await.unwrap();
    let dialogs: Vec<_> = f
        .calls()
        .into_iter()
        .filter(|(t, _)| t == "browser_handle_dialog")
        .collect();
    assert_eq!(
        dialogs,
        [("browser_handle_dialog".into(), json!({"accept": true}))]
    );
    // Refused once because of it, then tried again.
    assert_eq!(f.called("browser_mouse_click_xy"), 2);
    // A prompt would need text: dismissed.
    f.set(
        "modal",
        r#"- [\"prompt\" dialog with message \"Name?\"]: can be handled by browser_handle_dialog"#,
    );
    b.user_input("dev", &tap).await.unwrap();
    assert_eq!(
        f.calls()
            .iter()
            .rfind(|(t, _)| t == "browser_handle_dialog")
            .unwrap()
            .1,
        json!({"accept": false})
    );
    // A file picker is cancelled, never given files.
    f.set(
        "modal",
        r#"- [File chooser]: can be handled by browser_file_upload"#,
    );
    b.user_input("dev", &tap).await.unwrap();
    assert_eq!(
        f.calls()
            .iter()
            .rfind(|(t, _)| t == "browser_file_upload")
            .unwrap()
            .1,
        json!({})
    );

    // A window the tap opened (a sign-in popup) is what the owner sees
    // and uses next.
    f.set("tabs", "2");
    b.user_input("dev", &tap).await.unwrap();
    let selects: Vec<_> = f
        .calls()
        .into_iter()
        .filter(|(t, a)| t == "browser_tabs" && a["action"] == "select")
        .collect();
    assert_eq!(selects.len(), 1);
    assert_eq!(selects[0].1["index"], 1);
    // Only once.
    b.user_input("dev", &tap).await.unwrap();
    assert_eq!(
        f.calls()
            .iter()
            .filter(|(t, a)| t == "browser_tabs" && a["action"] == "select")
            .count(),
        1
    );
    b.hand_back();
    // Taking over keeps the tab the agent was on, even with newer tabs
    // open (the agent selects a sign-in window before asking); a window
    // opened while the owner is in control is still followed.
    let selects = |f: &Fake| {
        f.calls()
            .iter()
            .filter(|(t, a)| t == "browser_tabs" && a["action"] == "select")
            .count()
    };
    let before = selects(&f);
    f.set("tabs", "3");
    b.take_over(&f.owner, "dev").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    b.user_input("dev", &tap).await.unwrap();
    assert_eq!(selects(&f), before);
    f.set("tabs", "4");
    b.user_input("dev", &tap).await.unwrap();
    assert_eq!(selects(&f), before + 1);
    assert_eq!(
        f.calls()
            .iter()
            .rfind(|(t, _)| t == "browser_tabs")
            .unwrap()
            .1["index"],
        3
    );
    b.hand_back();

    // The agent answers its own dialogs, but a file picker (it may not
    // upload) is cancelled for it so the browser is not stuck.
    let g = b.begin_turn(&f.owner, &f.conv, "m", &[]);
    f.set(
        "modal",
        r#"- [File chooser]: can be handled by browser_file_upload"#,
    );
    let r = f
        .agent(g.token(), "browser_click", json!({"ref": "e1"}))
        .await;
    assert!(text_of(&r).contains("closed the file picker"), "{r}");
    assert!(!f.file("modal").exists());
    f.set(
        "modal",
        r#"- [\"alert\" dialog with message \"Hi\"]: can be handled by browser_handle_dialog"#,
    );
    let before = f.called("browser_handle_dialog");
    let r = f
        .agent(g.token(), "browser_click", json!({"ref": "e1"}))
        .await;
    assert_eq!(r["isError"], true);
    assert_eq!(f.called("browser_handle_dialog"), before);
    let r = f
        .agent(g.token(), "browser_handle_dialog", json!({"accept": true}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    drop(g);
    b.close().await;
}

/// A JPEG's width and height (its start-of-frame header).
fn jpeg_size(b64: &str) -> (u32, u32) {
    use base64::Engine as _;
    let b = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    let mut i = 2;
    while i + 9 < b.len() {
        assert_eq!(b[i], 0xFF, "JPEG marker");
        let marker = b[i + 1];
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if matches!(marker, 0xC0..=0xC2) {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return (w, h);
        }
        i += 2 + len;
    }
    panic!("no JPEG frame header");
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
    // Two origins: the site, and another one (its own port) for a frame.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_port = other.local_addr().unwrap().port();
    let serve = move |listener: std::net::TcpListener| {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let big = "position:absolute;left:0;width:500px;height:200px";
            // A sign-in kept in a cookie that outlives the browser.
            let mut extra = String::new();
            let body = if req.starts_with("GET /remember") {
                extra = "Set-Cookie: iris_smoke=1; Max-Age=86400; Path=/\r\n".into();
                "<html><body><p>Remembered</p></body></html>".to_string()
            } else if req.starts_with("GET /me") {
                let signed = req.lines().any(|l| {
                    l.to_ascii_lowercase().starts_with("cookie:") && l.contains("iris_smoke=1")
                });
                format!(
                    "<html><body><p>{}</p></body></html>",
                    if signed { "Signed in" } else { "Signed out" }
                )
            } else if req.starts_with("GET /textonly") {
                // A sign-in form whose page later keeps the password only
                // in its text (Iris moves it there below).
                "<html><body><input type=password aria-label=Password>\
                 <button>Go</button><p id=out></p></body></html>"
                    .to_string()
            } else if req.starts_with("GET /login") {
                "<html><head><title>Iris smoke sign in</title></head><body style='margin:0'>\
                 <input type=password aria-label=Password style='position:absolute;top:100px;left:0;width:500px;height:100px'>\
                 <p style='margin-top:250px'>Sign in to continue</p></body></html>"
                    .to_string()
            } else if req.starts_with("GET /code") {
                // A one-time code field named only `code`, labelled apart,
                // and a field in another site's frame.
                format!(
                    "<html><body style='margin:0'><label for=c>Authentication code</label>\
                     <input id=c name=code style='{big};top:100px'>\
                     <iframe src='http://127.0.0.1:{other_port}/field' style='{big};top:400px;border:0'></iframe></body></html>"
                )
            } else if req.starts_with("GET /signin") {
                "<html><head><title>Iris smoke sign in form</title></head><body>\
                 <form method=post action=/home><input name=user aria-label=Username>\
                 <input name=pw type=password aria-label=Password><button>Sign in</button></form></body></html>"
                    .to_string()
            } else if req.starts_with("GET /pay") {
                // A checkout: number, month list (valued 0 to 11, labelled
                // by name), a two-digit year known only by its placeholder,
                // and a CVC.
                let months: String = [
                    "January",
                    "February",
                    "March",
                    "April",
                    "May",
                    "June",
                    "July",
                    "August",
                    "September",
                    "October",
                    "November",
                    "December",
                ]
                .iter()
                .enumerate()
                .map(|(i, m)| format!("<option value={i}>{m}</option>"))
                .collect();
                format!(
                    "<html><head><title>Iris smoke checkout</title></head><body><form>\
                     <input name=n aria-label='Card number' autocomplete=cc-number>\
                     <select name=m aria-label='Expiry month'><option value=''>MM</option>{months}</select>\
                     <input name=y aria-label='Expiry year' placeholder=YY>\
                     <input name=c aria-label='Security code' autocomplete=cc-csc></form></body></html>"
                )
            } else if req.starts_with("GET /reflect") {
                // A sign-in form that sends with GET: the password lands in
                // the next page's address, text, and a frame's field.
                "<html><body><form method=get action=/echo>\
                 <input name=u aria-label=Username><input name=p type=password aria-label=Password>\
                 <button>Go</button></form></body></html>"
                    .to_string()
            } else if let Some(rest) = req.strip_prefix("GET /echo?") {
                let q = rest.split_whitespace().next().unwrap_or("");
                format!(
                    "<html><head><title>Sent {q}</title></head><body><p>You sent {q}</p>\
                     <script>console.log('loaded ' + location.href)</script>\
                     <iframe src='http://127.0.0.1:{other_port}/show?{q}'></iframe></body></html>"
                )
            } else if let Some(rest) = req.strip_prefix("GET /frameonly?") {
                // Nothing on the page itself; the value is only in another
                // site's frame.
                let q = rest.split_whitespace().next().unwrap_or("");
                format!(
                    "<html><body><p>Nothing to see here</p>\
                     <iframe src='http://127.0.0.1:{other_port}/show?{q}'></iframe></body></html>"
                )
            } else if let Some(rest) = req.strip_prefix("GET /show?") {
                let q = rest.split_whitespace().next().unwrap_or("");
                format!("<html><body><input aria-label=Shown value='{q}'></body></html>")
            } else if req.starts_with("GET /hosted") {
                // A checkout whose card fields live in a payment provider's
                // frame (another origin).
                format!(
                    "<html><head><title>Iris smoke pay box</title></head><body><h1>Pay</h1>\
                     <iframe src='http://127.0.0.1:{other_port}/cardframe' \
                     style='width:480px;height:300px;border:0'></iframe></body></html>"
                )
            } else if req.starts_with("GET /cardframe") {
                "<html><body><input aria-label='Provider card number' autocomplete=cc-number>\
                 <input aria-label='Provider code' autocomplete=cc-csc></body></html>"
                    .to_string()
            } else if req.starts_with("GET /field") {
                "<html><body style='margin:0'><input name=q style='width:500px;height:200px'></body></html>"
                    .to_string()
            } else if req.starts_with("GET /dialog") {
                format!(
                    "<html><body style='margin:0'><button onclick=\"window.answer = confirm('Sure?')\" \
                     style='{big};top:0'>Go</button>\
                     <button onclick=\"window.open('/home')\" style='{big};top:400px'>Open</button></body></html>"
                )
            } else {
                "<html><head><title>Iris smoke home</title></head><body><h1>Welcome home</h1></body></html>"
                    .to_string()
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    };
    std::thread::spawn(move || serve(listener));
    std::thread::spawn(move || serve(other));
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

    // Script addresses never reach the browser.
    let r = f
        .agent(
            &token,
            "browser_navigate",
            json!({"url": "javascript:0;alert(1)"}),
        )
        .await;
    assert!(text(&r).contains("Only web pages"), "{r}");

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

    // Live view: pictures as wide as the viewer asked (a thumbnail), up to
    // the page's own device pixels (full screen).
    let next_frame = |rx: &mut tokio::sync::watch::Receiver<Option<Arc<Frame>>>| {
        let mut rx = rx.clone();
        async move {
            rx.mark_unchanged();
            tokio::time::timeout(Duration::from_secs(10), rx.wait_for(Option::is_some))
                .await
                .unwrap()
                .unwrap()
                .clone()
                .unwrap()
        }
    };
    let (peek, mut rx) = b.watch(Mode::Peek, Some(216));
    let frame = next_frame(&mut rx).await;
    let v: Value = serde_json::from_str(&frame.json).unwrap();
    assert_eq!(jpeg_size(v["data"].as_str().unwrap()), (216, 432));
    drop(peek);
    b.frames.send_replace(None);
    let (watching, mut rx) = b.watch(Mode::Full, None);
    let frame = loop {
        let frame = next_frame(&mut rx).await;
        let v: Value = serde_json::from_str(&frame.json).unwrap();
        if v["mode"] == "full" {
            break v;
        }
    };
    assert_eq!(jpeg_size(frame["data"].as_str().unwrap()), (1000, 2000));
    eprintln!(
        "full frame: {} base64 bytes",
        frame["data"].as_str().unwrap().len()
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
    assert!(
        text_of(&typed).contains("hunter2-sëcret"),
        "the typed text is not in the field"
    );
    b.hand_back();
    // What was typed is read back, so the page stays usable: the snapshot
    // shows the field without it, and the screenshot has the field
    // blacked out (and only the field).
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(
        text(&r).contains("[hidden]") && !r.to_string().contains("hunter2"),
        "the typed password reached the agent"
    );
    let pixel = |shot: &Value, x: u32, y: u32| {
        let data = shot["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["type"] == "image")
            .and_then(|p| p["data"].as_str())
            .expect("a picture")
            .to_string();
        let b = b.clone();
        let run = run.clone();
        async move {
            let code = format!(
                "async () => {{ const img = new Image(); img.src = 'data:image/png;base64,{data}'; \
                 await img.decode(); const c = document.createElement('canvas'); \
                 c.width = img.width; c.height = img.height; const g = c.getContext('2d'); \
                 g.drawImage(img, 0, 0); return Array.from(g.getImageData({x}, {y}, 1, 1).data.slice(0, 3)).join(','); }}"
            );
            text_of(
                &b.tool(&run, "browser_evaluate", &json!({"function": code}), QUICK)
                    .await
                    .unwrap(),
            )
        }
    };
    let shot = f.agent(&token, "browser_take_screenshot", json!({})).await;
    assert_ne!(shot["isError"], true, "{shot}");
    assert!(text(&shot).contains("blacked out"), "{shot}");
    assert!(
        pixel(&shot, 250, 150).await.contains("\"0,0,0\""),
        "the field is not masked"
    );
    assert!(
        pixel(&shot, 250, 50).await.contains("\"255,255,255\""),
        "the page is masked"
    );
    // The page changes its address without a new document (a single-page
    // app's route): still usable, what was typed still hidden.
    let js = |f: &str| json!({"function": f});
    b.tool(
        &run,
        "browser_evaluate",
        &js("() => history.pushState({}, '', '/account/next')"),
        QUICK,
    )
    .await
    .unwrap();
    let still = b
        .tool(
            &run,
            "browser_evaluate",
            &js("() => [location.pathname, document.querySelector('input').value.length]"),
            QUICK,
        )
        .await
        .unwrap();
    assert!(
        text_of(&still).contains("/account/next") && text_of(&still).contains("14"),
        "the field was not kept"
    );
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(
        !r.to_string().contains("hunter2"),
        "the typed password reached the agent"
    );
    b.tool(
        &run,
        "browser_evaluate",
        &js("() => history.replaceState({}, '', '/login')"),
        QUICK,
    )
    .await
    .unwrap();
    // Another tab, coming back to the first, moving on and going back:
    // all usable, never with what was typed.
    let home = format!("http://127.0.0.1:{port}/home");
    let r = f
        .agent(
            &token,
            "browser_tabs",
            json!({"action": "new", "url": home}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert!(text(&r).contains("Welcome home"), "{r}");
    let r = f
        .agent(
            &token,
            "browser_tabs",
            json!({"action": "select", "index": 0}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(
        !r.to_string().contains("hunter2"),
        "the typed password reached the agent"
    );
    let r = f
        .agent(
            &token,
            "browser_tabs",
            json!({"action": "close", "index": 1}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f
        .agent(&token, "browser_navigate", json!({"url": home}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_navigate_back", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert!(
        !r.to_string().contains("hunter2"),
        "the typed password reached the agent"
    );
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

    // The agent signs in with a saved login it never sees.
    let id = "real-login".to_string();
    let secret = f
        .state
        .vault
        .encrypt(
            json!({"password": "Real-Pw-42"}).to_string().as_bytes(),
            &id,
        )
        .unwrap();
    f.state
        .db
        .with(|c| {
            crate::entities::insert_credential(
                c,
                &id,
                &f.owner,
                "login",
                "Smoke",
                &json!({"site": format!("http://127.0.0.1:{port}"), "username": "sam"}),
                &secret,
            )
        })
        .unwrap();
    let signin = format!("http://127.0.0.1:{port}/signin");
    f.agent(&token, "browser_navigate", json!({"url": signin}))
        .await;
    let snap = text(&f.agent(&token, "browser_snapshot", json!({})).await);
    let ref_of = |label: &str| {
        let line = snap
            .lines()
            .find(|l| l.contains(&format!("textbox \"{label}\"")))
            .unwrap_or_else(|| panic!("{label} in {snap}"));
        let at = line.find("[ref=").unwrap() + 5;
        line[at..line[at..].find(']').unwrap() + at].to_string()
    };
    let (user_ref, pw_ref) = (ref_of("Username"), ref_of("Password"));
    let button_ref = {
        let line = snap
            .lines()
            .find(|l| l.contains("button \"Sign in\""))
            .unwrap_or_else(|| panic!("button in {snap}"));
        let at = line.find("[ref=").unwrap() + 5;
        line[at..line[at..].find(']').unwrap() + at].to_string()
    };
    let r = f
        .agent(
            &token,
            "fill_saved_login",
            json!({"username_ref": user_ref, "password_ref": pw_ref}),
        )
        .await;
    assert_eq!(text(&r), "filled", "{r}");
    let typed = b
        .tool(
            &run,
            "browser_evaluate",
            &json!({"function": "() => [...document.querySelectorAll('input')].map((i) => i.value)"}),
            QUICK,
        )
        .await
        .unwrap();
    assert!(
        text_of(&typed).contains("Real-Pw-42") && text_of(&typed).contains("sam"),
        "{typed}"
    );
    // While the password is in its field the agent reads and uses the
    // page without it.
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(
        !r.to_string().contains("Real-Pw-42"),
        "the password reached the agent"
    );
    let r = f
        .agent(&token, "browser_click", json!({"target": user_ref}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f
        .agent(
            &token,
            "fill_saved_login",
            json!({"password_ref": pw_ref, "submit_ref": button_ref}),
        )
        .await;
    assert_eq!(text(&r), fill::SUBMITTED, "{r}");
    let mut home_now = String::new();
    for _ in 0..20 {
        home_now = text(&f.agent(&token, "browser_snapshot", json!({})).await);
        if home_now.contains("Welcome home") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // (Never printed: it would be the secret if the check failed.)
    assert!(
        home_now.contains("Welcome home"),
        "the sign-in did not move on"
    );
    assert!(
        !home_now.contains("Real-Pw-42"),
        "the password reached the agent"
    );

    // A page that echoes the password (a sign-in form sending with GET):
    // the agent's results have it hidden, and no picture is taken.
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": format!("http://127.0.0.1:{port}/reflect")}),
    )
    .await;
    let snap = text(&f.agent(&token, "browser_snapshot", json!({})).await);
    let ref_in = |label: &str| {
        let line = snap
            .lines()
            .find(|l| l.contains(&format!("textbox \"{label}\"")))
            .unwrap_or_else(|| panic!("{label} in {snap}"));
        let at = line.find("[ref=").unwrap() + 5;
        line[at..line[at..].find(']').unwrap() + at].to_string()
    };
    let r = f
        .agent(
            &token,
            "fill_saved_login",
            json!({"username_ref": ref_in("Username"), "password_ref": ref_in("Password"), "submit": true}),
        )
        .await;
    // The next page holds it in a frame's field: the answer says so.
    assert_eq!(text(&r), fill::STILL_THERE, "{r}");
    let mut on_echo = false;
    for _ in 0..40 {
        let url = b.current_url(&run).await.unwrap();
        if url.contains("/echo?") {
            // (The page really holds it.)
            assert!(url.contains("Real-Pw-42"), "the page did not get the value");
            on_echo = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(on_echo, "the form did not send");
    tokio::time::sleep(Duration::from_millis(500)).await;
    for (tool, args) in [
        ("browser_tabs", json!({"action": "list"})),
        ("browser_snapshot", json!({})),
        ("browser_navigate_back", json!({})),
        ("browser_navigate_back", json!({})),
    ] {
        let r = f.agent(&token, tool, args).await;
        assert!(
            !r.to_string().contains("Real-Pw-42"),
            "{tool} showed the password"
        );
        if tool == "browser_snapshot" {
            assert!(
                text(&r).contains("You sent") && text(&r).contains("[hidden]"),
                "{tool}"
            );
            // A picture, with what shows the password blacked out.
            let shot = f.agent(&token, "browser_take_screenshot", json!({})).await;
            assert!(text(&shot).contains("blacked out"), "{shot}");
        }
    }

    // Nothing of it is left on disk either (Playwright logs the page's
    // console, which printed its address).
    for e in std::fs::read_dir(&b.out_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        let body = std::fs::read(e.path()).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&body).contains("Real-Pw-42"),
            "a secret was written to a file"
        );
    }

    // A saved secret only inside another site's frame: blacked out there.
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": format!("http://127.0.0.1:{port}/frameonly?v=Real-Pw-42")}),
    )
    .await;
    let mut in_frame = false;
    for _ in 0..40 {
        let v = b
            .run_code(
                &run,
                "async (page) => { const f = page.frames()[1]; return f ? await f.locator('input').inputValue().catch(() => '') : ''; }",
            )
            .await
            .unwrap_or_default();
        if v.as_str().is_some_and(|v| v.contains("Real-Pw-42")) {
            in_frame = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(in_frame, "the frame did not load the value");
    let top = b
        .run_code(
            &run,
            "async (page) => await page.locator('body').innerText()",
        )
        .await
        .unwrap();
    assert!(
        !top.to_string().contains("Real-Pw"),
        "the page itself shows it"
    );
    let shot = f.agent(&token, "browser_take_screenshot", json!({})).await;
    assert!(text(&shot).contains("blacked out"), "{shot}");
    let bx = b
        .run_code(
            &run,
            "async (page) => await page.frames()[1].locator('input').boundingBox()",
        )
        .await
        .unwrap();
    let (x, y) = (
        (bx["x"].as_f64().unwrap() + bx["width"].as_f64().unwrap() / 2.0) as u32,
        (bx["y"].as_f64().unwrap() + bx["height"].as_f64().unwrap() / 2.0) as u32,
    );
    assert!(
        pixel(&shot, x, y).await.contains("\"0,0,0\""),
        "the frame's field is not masked"
    );

    // A page that takes the password out of its field but keeps it in its
    // text, on the same document: usable, with the text hidden from what
    // the agent reads.
    f.agent(
        &token,
        "browser_navigate",
        json!({"url": format!("http://127.0.0.1:{port}/textonly")}),
    )
    .await;
    let snap = text(&f.agent(&token, "browser_snapshot", json!({})).await);
    let ref_named = |what: &str| {
        let line = snap
            .lines()
            .find(|l| l.contains(what))
            .unwrap_or_else(|| panic!("{what} in {snap}"));
        let at = line.find("[ref=").unwrap() + 5;
        line[at..line[at..].find(']').unwrap() + at].to_string()
    };
    let (pw, go) = (
        ref_named("textbox \"Password\""),
        ref_named("button \"Go\""),
    );
    let r = f
        .agent(&token, "fill_saved_login", json!({"password_ref": pw}))
        .await;
    assert_eq!(text(&r), "filled", "{r}");
    b.run_code(
        &run,
        "async (page) => { await page.evaluate(() => { const i = document.querySelector('input'); \
         document.getElementById('out').textContent = 'You typed ' + i.value; i.value = ''; }); return true; }",
    )
    .await
    .unwrap();
    let r = f
        .agent(&token, "browser_click", json!({"target": go}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f.agent(&token, "browser_snapshot", json!({})).await;
    assert!(
        text(&r).contains("You typed") && !r.to_string().contains("Real-Pw-42"),
        "the password reached the agent"
    );
    let r = f
        .agent(&token, "browser_find", json!({"text": "You typed"}))
        .await;
    assert!(text(&r).contains("searching it"), "{r}");
    b.run_code(
        &run,
        "async (page) => { await page.evaluate(() => { document.getElementById('out').textContent = 'Done'; }); return true; }",
    )
    .await
    .unwrap();
    let r = f
        .agent(&token, "browser_click", json!({"target": go}))
        .await;
    assert_ne!(r["isError"], true, "{r}");

    // The agent fills a saved card after the owner approves, never seeing
    // it.
    let card_id = "real-card".to_string();
    let secret = f
        .state
        .vault
        .encrypt(
            json!({"number": "4111111111111111", "cvc": "737"})
                .to_string()
                .as_bytes(),
            &card_id,
        )
        .unwrap();
    f.state
        .db
        .with(|c| {
            crate::entities::insert_credential(
                c,
                &card_id,
                &f.owner,
                "card",
                "Everyday",
                &json!({"last4": "1111", "brand": "visa", "exp_month": 4, "exp_year": 2031}),
                &secret,
            )
        })
        .unwrap();
    let chat = b.begin_turn(&f.owner, &f.conv, "m2", std::slice::from_ref(&f.owner));
    let chat_token = chat.token().to_string();
    f.agent(
        &chat_token,
        "browser_navigate",
        json!({"url": format!("http://127.0.0.1:{port}/pay")}),
    )
    .await;
    let snap = text(&f.agent(&chat_token, "browser_snapshot", json!({})).await);
    let ref_named = |label: &str| {
        let line = snap
            .lines()
            .find(|l| l.contains(&format!("\"{label}\"")))
            .unwrap_or_else(|| panic!("{label} in {snap}"));
        let at = line.find("[ref=").unwrap() + 5;
        line[at..line[at..].find(']').unwrap() + at].to_string()
    };
    let args = json!({"number_ref": ref_named("Card number"), "exp_month_ref": ref_named("Expiry month"),
        "exp_year_ref": ref_named("Expiry year"), "cvc_ref": ref_named("Security code")});
    let call = {
        let (st, tk) = (f.state.clone(), chat_token.clone());
        tokio::spawn(async move { tools::call(&st, &tk, "fill_saved_card", &args).await })
    };
    let mut waited = 0;
    let approval = loop {
        assert!(!call.is_finished(), "the fill ended before asking");
        waited += 1;
        assert!(waited < 400, "no approval card");
        let msgs = f
            .state
            .db
            .with(|c| crate::db::list_messages(c, &f.conv, None, 50))
            .unwrap();
        if let Some(m) = msgs.into_iter().find(|m| {
            m.card
                .as_ref()
                .is_some_and(|c| c["service"] == "wallet" && c["state"] == "pending")
        }) {
            break m;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let mut card = approval.card.clone().unwrap();
    card["state"] = "approved".into();
    card["decided_at"] = crate::util::now_ms().into();
    f.state
        .db
        .with(|c| crate::db::update_message_card(c, &approval.id, &card))
        .unwrap();
    crate::gateway::decided(&f.state, &approval.id, true);
    let r = call.await.unwrap();
    assert_eq!(text(&r), "filled", "{r}");
    let values = b
        .tool(
            &run,
            "browser_evaluate",
            &json!({"function": "() => [...document.querySelectorAll('input, select')].map((i) => i.value)"}),
            QUICK,
        )
        .await
        .unwrap();
    let values = text_of(&values).replace([' ', '\n'], "");
    assert!(
        values.contains(r#"["4111111111111111","3","31","737"]"#),
        "the card fields hold something else"
    );
    // The expiry does not lock the page: it is usable without the number
    // and code while they are in their fields.
    let r = f.agent(&chat_token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(
        !r.to_string().contains("4111"),
        "the card number reached the agent"
    );
    let r = f
        .agent(&chat_token, "browser_press_key", json!({"key": "Tab"}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    // Going away and back to the filled form writes nothing of it to disk.
    let r = f
        .agent(
            &chat_token,
            "browser_navigate",
            json!({"url": format!("http://127.0.0.1:{port}/home")}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let r = f
        .agent(&chat_token, "browser_navigate_back", json!({}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    for e in std::fs::read_dir(&b.out_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        let body = std::fs::read(e.path()).unwrap_or_default();
        let body = String::from_utf8_lossy(&body);
        assert!(
            !body.contains("4111111111111111") && !body.contains("Real-Pw-42"),
            "a secret was written to a file"
        );
    }
    // A payment provider's frame: its fields' refs resolve, the approval
    // names that site, and the values land in the frame.
    f.agent(
        &chat_token,
        "browser_navigate",
        json!({"url": format!("http://127.0.0.1:{port}/hosted")}),
    )
    .await;
    let mut boxed = String::new();
    for _ in 0..40 {
        boxed = text(&f.agent(&chat_token, "browser_snapshot", json!({})).await);
        if boxed.contains("Provider code") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ref_in_box = |label: &str| {
        let line = boxed
            .lines()
            .find(|l| l.contains(&format!("\"{label}\"")))
            .unwrap_or_else(|| panic!("{label} in {boxed}"));
        let at = line.find("[ref=").unwrap() + 5;
        line[at..line[at..].find(']').unwrap() + at].to_string()
    };
    let args = json!({"number_ref": ref_in_box("Provider card number"), "cvc_ref": ref_in_box("Provider code")});
    assert!(
        args["number_ref"].as_str().unwrap().starts_with('f'),
        "{args}"
    );
    let call = {
        let (st, tk) = (f.state.clone(), chat_token.clone());
        tokio::spawn(async move { tools::call(&st, &tk, "fill_saved_card", &args).await })
    };
    let mut waited = 0;
    let approval = loop {
        assert!(!call.is_finished(), "the fill ended before asking");
        waited += 1;
        assert!(waited < 400, "no approval card");
        let msgs = f
            .state
            .db
            .with(|c| crate::db::list_messages(c, &f.conv, None, 50))
            .unwrap();
        if let Some(m) = msgs.into_iter().find(|m| {
            m.card
                .as_ref()
                .is_some_and(|c| c["service"] == "wallet" && c["state"] == "pending")
        }) {
            break m;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let mut card = approval.card.clone().unwrap();
    assert!(
        card["preview"]["body"]
            .as_str()
            .unwrap()
            .contains(&format!("The card fields are on: 127.0.0.1:{other_port}")),
        "{card}"
    );
    card["state"] = "approved".into();
    card["decided_at"] = crate::util::now_ms().into();
    f.state
        .db
        .with(|c| crate::db::update_message_card(c, &approval.id, &card))
        .unwrap();
    crate::gateway::decided(&f.state, &approval.id, true);
    let r = call.await.unwrap();
    assert_eq!(text(&r), "filled", "{r}");
    let in_frame = b
        .run_code(
            &run,
            "async (page) => await page.frames()[1].locator('input').evaluateAll((els) => els.map((e) => e.value))",
        )
        .await
        .unwrap();
    assert_eq!(
        in_frame,
        json!(["4111111111111111", "737"]),
        "the frame's fields hold something else"
    );
    // The number and code are found in the other site's frame: the page
    // is usable without them, and its picture has them blacked out.
    let r = f.agent(&chat_token, "browser_snapshot", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(
        !r.to_string().contains("4111"),
        "the card number reached the agent"
    );
    let r = f
        .agent(
            &chat_token,
            "browser_click",
            json!({"target": ref_in_box("Provider code")}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let shot = f
        .agent(&chat_token, "browser_take_screenshot", json!({}))
        .await;
    let bx = b
        .run_code(
            &run,
            "async (page) => await page.frames()[1].locator('input').first().boundingBox()",
        )
        .await
        .unwrap();
    let (x, y) = (
        (bx["x"].as_f64().unwrap() + bx["width"].as_f64().unwrap() / 2.0) as u32,
        (bx["y"].as_f64().unwrap() + bx["height"].as_f64().unwrap() / 2.0) as u32,
    );
    assert!(
        pixel(&shot, x, y).await.contains("\"0,0,0\""),
        "the provider's card field is not masked"
    );

    drop(chat);

    // The owner again: a code field known only by its label is private,
    // and a field in another site's frame says so.
    b.take_over(&f.owner, "dev").await.unwrap();
    let input = |ev: Value| {
        let b = b.clone();
        async move { b.user_input("dev", &ev).await.unwrap() }
    };
    input(json!({"kind": "navigate", "url": format!("http://127.0.0.1:{port}/code")})).await;
    input(json!({"kind": "tap", "x": 0.5, "y": 0.2})).await;
    let field = b.focused_field(&run, false, false).await.unwrap();
    assert!(field.private && !field.frame, "{field:?}");
    // (The frame loads on its own.)
    tokio::time::sleep(Duration::from_secs(1)).await;
    input(json!({"kind": "tap", "x": 0.5, "y": 0.5})).await;
    let field = b.focused_field(&run, false, false).await.unwrap();
    assert!(field.private && field.frame, "{field:?}");
    assert_eq!(field.url, format!("http://127.0.0.1:{port}/code"));

    // A confirmation the owner cannot see is answered; a window the page
    // opens becomes the one they use.
    input(json!({"kind": "navigate", "url": format!("http://127.0.0.1:{port}/dialog")})).await;
    input(json!({"kind": "tap", "x": 0.5, "y": 0.1})).await;
    let answer = b
        .tool(
            &run,
            "browser_evaluate",
            &json!({"function": "() => window.answer"}),
            QUICK,
        )
        .await
        .unwrap();
    assert!(text_of(&answer).contains("true"), "{answer}");
    input(json!({"kind": "tap", "x": 0.5, "y": 0.5})).await;
    let mut url = String::new();
    for _ in 0..20 {
        let _op = b.op_lock.lock().await;
        let _ = b.follow_new_tab(&run).await;
        url = b.current_url(&run).await.unwrap();
        if url.ends_with("/home") {
            break;
        }
        drop(_op);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(url.ends_with("/home"), "{url}");
    b.hand_back();

    // The agent asks the owner to save a login for the open site, and
    // restarts the browser: a new one, its sign-ins kept.
    let chat = b.begin_turn(&f.owner, &f.conv, "m3", std::slice::from_ref(&f.owner));
    let token = chat.token().to_string();
    let page = |p: &str| json!({"url": format!("http://127.0.0.1:{port}/{p}")});
    let me = async |f: &Fake, token: &str| {
        f.agent(token, "browser_navigate", page("me")).await;
        text(&f.agent(token, "browser_snapshot", json!({})).await)
    };
    assert!(me(&f, &token).await.contains("Signed out"));
    f.agent(&token, "browser_navigate", page("remember")).await;
    assert!(me(&f, &token).await.contains("Signed in"));
    let r = f.agent(&token, "ask_to_save_login", json!({})).await;
    assert!(text(&r).starts_with("posted"), "{r}");
    let r = f.agent(&token, "restart_browser", json!({})).await;
    assert!(text(&r).starts_with("restarted"), "{r}");
    let again = b.running().unwrap();
    assert!(!Arc::ptr_eq(&run, &again));
    assert!(
        me(&f, &token).await.contains("Signed in"),
        "the sign-in did not survive the restart"
    );
    let r = f
        .agent(&token, "browser_navigate", json!({"url": home.clone()}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(text(&f.agent(&token, "browser_snapshot", json!({})).await).contains("Welcome home"));
    drop(chat);

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
