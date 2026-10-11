//! End-to-end tests against a real Chromium, when one is installed (they
//! pass trivially otherwise). A tiny local web server plays a sign-in page
//! and a checkout page; the tests drive them through the same tool entry
//! point the MCP bridge uses and check that secrets reach the page but
//! never a tool result.

use super::tools;
use crate::config::{Config, Paths};
use crate::state::AppState;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "S3cret-Pa55-xyz!";
/// Quotes, a backslash, doubled spaces, and longer than snapshot cuts.
const TRICKY: &str =
    "q\"uo  te\\back-slash-0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-end";
const CARD: &str = "4242424242424242";
const CVC: &str = "987";

const LOGIN_PAGE: &str = r#"<!doctype html><html><head><title>Test Shop sign in</title>
<meta name="viewport" content="width=device-width"></head><body>
<h1>Sign in</h1>
<form method="post" action="/login">
<label>Email <input name="email" type="email" autocomplete="username"></label>
<label>Password <input name="password" type="password" autocomplete="current-password"></label>
<label><input type="checkbox" name="remember"> Remember me</label>
<select name="lang" aria-label="Language"><option>English</option><option>Deutsch</option></select>
<button type="submit">Sign in</button>
</form>
<p>New here? <a href="/about">About the shop</a></p>
</body></html>"#;

const CHECKOUT_PAGE: &str = r#"<!doctype html><html><head><title>Checkout</title></head><body>
<h1>Pay</h1>
<form method="post" action="/pay">
<input name="cardnumber" autocomplete="cc-number" placeholder="Card number">
<input name="exp" autocomplete="cc-exp" placeholder="MM / YY">
<input name="cvc" autocomplete="cc-csc" placeholder="CVC">
<input name="holder" autocomplete="cc-name" placeholder="Name on card">
<button>Pay now</button>
</form></body></html>"#;

/// A sign-in page that echoes what is typed into its password field: in a
/// dialog, a paragraph, and another field (all must be redacted).
const REFLECT_PAGE: &str = r#"<!doctype html><html><head><title>Echo sign in</title></head><body>
<h1>Echo</h1><form>
<input name="email" type="email" autocomplete="username" aria-label="Email">
<input id="pw" name="password" type="password" aria-label="Password"
  oninput="document.getElementById('echo').textContent = 'You typed ' + this.value;
           document.getElementById('copy').value = this.value; alert('Got ' + this.value)">
<input id="copy" aria-label="Copy">
<button type="button" onclick="pw.type = 'text'">Show password</button>
</form><p id="echo"></p></body></html>"#;

/// A password field the page later reveals and unmasks, with no other copy;
/// a focus handler that tries to move focus elsewhere; the password
/// echoed into the title; and a card form on the same page.
const TRICKY_PAGE: &str = r#"<!doctype html><html><head><title>Tricky</title></head><body>
<h1>Tricky</h1><form>
<input name="email" type="email" autocomplete="username" aria-label="Email">
<input id="pw" type="password" aria-label="Password"
  onfocus="queueMicrotask(() => document.getElementById('other').focus())"
  oninput="document.title = 'Hi ' + this.value">
<input id="other" aria-label="Other">
<button type="button" onclick="pw.type = 'text'; pw.style.removeProperty('-webkit-text-security')">Reveal</button>
<input name="cardnumber" autocomplete="cc-number" aria-label="Card number">
</form></body></html>"#;

/// Card fields recognized by name only (no autocomplete hints).
const PLAIN_CHECKOUT: &str = r#"<!doctype html><html><head><title>Plain checkout</title></head><body>
<h1>Pay</h1><form method="post" action="/pay">
<label>Card number <input name="card_number"></label>
<label>Expiry <input name="expiry" placeholder="MM/YY"></label>
<label>Security code <input name="security_code"></label>
<label>Name on card <input name="name_on_card"></label>
<button>Pay</button></form></body></html>"#;

/// Serve the test pages; returns the port and the request bodies posted.
async fn test_site() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let posted: Arc<Mutex<Vec<String>>> = Arc::default();
    let p = posted.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                break;
            };
            let p = p.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let Ok(n) = s.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            let body = text[end + 4..].to_string();
                            let line = text.lines().next().unwrap_or("").to_string();
                            let page = if line.starts_with("POST /login") {
                                p.lock().unwrap().push(body);
                                "<!doctype html><title>Welcome</title><h1>Welcome back</h1>".into()
                            } else if line.starts_with("POST /pay") {
                                p.lock().unwrap().push(body);
                                "<!doctype html><title>Paid</title><h1>Thanks</h1>".into()
                            } else if line.starts_with("GET /checkout") {
                                CHECKOUT_PAGE.to_string()
                            } else if line.starts_with("GET /plain-checkout") {
                                PLAIN_CHECKOUT.to_string()
                            } else if line.starts_with("GET /tricky") {
                                TRICKY_PAGE.to_string()
                            } else if line.starts_with("GET /reflect") {
                                REFLECT_PAGE.to_string()
                            } else if line.starts_with("GET / ") {
                                LOGIN_PAGE.to_string()
                            } else {
                                "<!doctype html><title>About</title><p>A test shop.</p>".into()
                            };
                            let res = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                                page.len()
                            );
                            let _ = s.write_all(res.as_bytes()).await;
                            return;
                        }
                    }
                }
            });
        }
    });
    (port, posted)
}

fn ref_of(snapshot: &str, needle: &str) -> String {
    let line = snapshot
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no {needle} in:\n{snapshot}"));
    let start = line.find("[ref=").unwrap() + 5;
    line[start..].split(']').next().unwrap().to_string()
}

/// A server state with a saved login (for 127.0.0.1 and localhost) and a
/// wallet card. None when no Chromium is installed.
fn setup() -> Option<(tempfile::TempDir, AppState, String, String)> {
    if super::launch::find("").is_none() {
        eprintln!("no Chromium found; skipping the browser end-to-end test");
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_file: tmp.path().join("config.toml"),
        data_dir: tmp.path().join("data"),
    };
    crate::persona::seed(&paths).unwrap();
    let db = crate::db::Db::open(&paths.db_file()).unwrap();
    let mut cfg = Config::default();
    for h in cfg.harness.values_mut() {
        h.enabled = false;
    }
    let state = AppState::new(cfg, paths, db).unwrap();
    // Test machines often cannot start Chrome's sandbox.
    state.browser.init_allow_no_sandbox(true);
    let (owner, conv) = state
        .db
        .with(|c| {
            let u = crate::db::owner_user_id(c)?.unwrap();
            let m = crate::db::ensure_main_conversation(c, &u)?;
            Ok((u, m))
        })
        .unwrap();
    for (id, kind, meta, secret) in [
        (
            "cred-login",
            "login",
            json!({"site": "127.0.0.1", "username": "pat@example.com"}),
            json!({"password": PASSWORD}),
        ),
        (
            "cred-login-local",
            "login",
            json!({"site": "localhost", "username": "pat@example.com"}),
            json!({"password": TRICKY}),
        ),
        (
            "cred-card",
            "card",
            json!({"brand": "visa", "last4": "4242", "exp_month": 8, "exp_year": 2031,
                   "cardholder": "Pat Example"}),
            json!({"number": CARD, "cvc": CVC}),
        ),
    ] {
        let ct = state
            .vault
            .encrypt(secret.to_string().as_bytes(), id)
            .unwrap();
        state
            .db
            .with(|c| crate::entities::insert_credential(c, id, &owner, kind, "Test", &meta, &ct))
            .unwrap();
    }
    Some((tmp, state, owner, conv))
}

/// The wallet approval cards in a conversation, newest last.
async fn approvals(state: &AppState, conv: &str) -> Vec<crate::db::Message> {
    let c = conv.to_string();
    state
        .db
        .call(move |db| crate::db::list_messages(db, &c, None, 50))
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.card.as_ref().is_some_and(|c| c["type"] == "approval"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browses_fills_logins_and_cards_without_leaking_them() {
    let Some((_tmp, state, owner, conv)) = setup() else {
        return;
    };
    let (port, posted) = test_site().await;
    let guard = state
        .browser
        .begin_turn(&owner, &conv, "msg-1", std::slice::from_ref(&owner));
    let token = guard.token().to_string();
    let mut outputs: Vec<String> = Vec::new();
    let call = |name: &'static str, args: Value| {
        let (st, tk) = (state.clone(), token.clone());
        async move { tools::call(&st, &tk, name, &args).await }
    };

    // Live view: a watcher gets frames once the browser runs.
    let (watch_guard, mut frames) = state.browser.watch(super::Mode::Full);

    let out = call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/")}),
    )
    .await;
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("heading \"Sign in\""), "{}", out.text);
    assert!(out.text.contains("Test Shop sign in"));
    {
        let run = state.browser.running().unwrap();
        let v = run
            .cdp
            .call(
                "Runtime.evaluate",
                json!({"expression": "[innerWidth, innerHeight, devicePixelRatio].join(' ')", "returnByValue": true}),
                Some(&run.page().session_id),
            )
            .await
            .unwrap();
        // A phone-shaped window at 2x, kept across navigations.
        let dims = v["result"]["value"].as_str().unwrap().to_string();
        assert!(dims.starts_with("500 ") && dims.ends_with(" 2"), "{dims}");
    }
    outputs.push(out.text.clone());
    let snap = out.text;

    let remember = ref_of(&snap, "checkbox \"Remember me\"");
    let out = call("click", json!({"ref": remember})).await;
    assert!(
        out.text.contains("Remember me") && out.text.contains("[checked]"),
        "{}",
        out.text
    );
    outputs.push(out.text);

    let lang = ref_of(&snap, "combobox \"Language\"");
    let out = call("select_option", json!({"ref": lang, "value": "Deutsch"})).await;
    assert_eq!(out.text, "Selected \"Deutsch\".");

    let out = call("fill_login", json!({"site": "example.com"})).await;
    assert!(out.text.starts_with("not found"), "{}", out.text);
    let out = call("fill_login", json!({})).await;
    assert_eq!(out.text, "filled");

    let out = call("snapshot", json!({})).await;
    assert!(out.text.contains("pat@example.com"), "{}", out.text);
    assert!(out.text.contains("value=\"••••\""), "{}", out.text);
    outputs.push(out.text.clone());

    let shot = call("screenshot", json!({})).await;
    assert!(shot.image.as_ref().is_some_and(|i| i.len() > 1000));
    outputs.push(shot.text);

    let sign_in = ref_of(&out.text, "button \"Sign in\"");
    let out = call("click", json!({"ref": sign_in})).await;
    assert!(out.text.contains("Welcome back"), "{}", out.text);
    outputs.push(out.text);
    {
        let p = posted.lock().unwrap();
        let body = p.first().expect("the form was posted");
        assert!(body.contains("email=pat%40example.com"), "{body}");
        assert!(body.contains("password=S3cret-Pa55-xyz%21"), "{body}");
        assert!(
            body.contains("remember=on") && body.contains("lang=Deutsch"),
            "{body}"
        );
    }

    // The live view streams: a newer frame follows a visible change.
    tokio::time::timeout(Duration::from_secs(10), frames.wait_for(|f| f.is_some()))
        .await
        .expect("a frame")
        .unwrap();
    let before = frames.borrow_and_update().as_ref().unwrap().json.clone();
    call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/about")}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), frames.changed())
        .await
        .expect("a newer frame")
        .unwrap();
    let after = frames.borrow().as_ref().unwrap().json.clone();
    assert_ne!(before, after);
    // Nobody watching: the screencast stops.
    drop(watch_guard);
    for _ in 0..50 {
        if state.browser.running().unwrap().casting().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(state.browser.running().unwrap().casting().is_none());

    // Take over: the agent waits until the owner hands back.
    state.browser.take_over(&owner, "dev-1").await.unwrap();
    state
        .browser
        .user_input(
            "dev-1",
            &json!({"kind": "navigate", "url": format!("http://127.0.0.1:{port}/about")}),
        )
        .await
        .unwrap();
    let waiting = tokio::spawn(call("snapshot", json!({})));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!waiting.is_finished());
    state.browser.hand_back();
    let out = waiting.await.unwrap();
    assert!(out.text.contains("A test shop"), "{}", out.text);

    // Cards: the owner approves on a card in the chat, then Iris fills.
    let out = call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/checkout")}),
    )
    .await;
    assert!(out.text.contains("Card number"), "{}", out.text);
    let approver = {
        let st = state.clone();
        let conv = conv.clone();
        tokio::spawn(async move {
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let c = conv.clone();
                let pending = st
                    .db
                    .call(move |db| crate::db::list_messages(db, &c, None, 20))
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|m| m.card.as_ref().is_some_and(|c| c["type"] == "approval"));
                if let Some(m) = pending {
                    let mut card = m.card.unwrap();
                    assert_eq!(card["preview"]["to"], "127.0.0.1");
                    assert!(!card.to_string().contains(CARD));
                    card["state"] = "approved".into();
                    st.db
                        .call(move |db| crate::db::update_message_card(db, &m.id, &card))
                        .await
                        .unwrap();
                    return true;
                }
            }
            false
        })
    };
    let out = call("fill_card", json!({})).await;
    assert!(approver.await.unwrap());
    assert_eq!(out.text, "filled");
    // One approval fills once.
    let again = call("fill_card", json!({"card": "4242"})).await;
    assert!(
        again.text.starts_with("waiting for approval"),
        "{}",
        again.text
    );
    let snap = call("snapshot", json!({})).await;
    assert!(!snap.text.contains("4242424242424242"), "{}", snap.text);
    outputs.push(snap.text.clone());
    let pay = ref_of(&snap.text, "button \"Pay now\"");
    call("click", json!({"ref": pay})).await;
    {
        let p = posted.lock().unwrap();
        let body = p.last().unwrap();
        assert!(body.contains("cardnumber=4242424242424242"), "{body}");
        assert!(
            body.contains("cvc=987") && body.contains("exp=08%2F31"),
            "{body}"
        );
        assert!(body.contains("holder=Pat+Example"), "{body}");
    }

    // No tool result ever carried a secret.
    for o in &outputs {
        assert!(!o.contains(PASSWORD) && !o.contains(CARD), "{o}");
    }
    drop(guard);
    assert!(
        tools::call(&state, &token, "snapshot", &json!({}))
            .await
            .is_error
    );
    state.browser.close().await;
    assert!(state.browser.running().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflected_secrets_pending_and_denied_cards_and_stale_refs() {
    let Some((_tmp, state, owner, conv)) = setup() else {
        return;
    };
    let (port, _posted) = test_site().await;
    let guard = state
        .browser
        .begin_turn(&owner, &conv, "msg-2", std::slice::from_ref(&owner));
    let token = guard.token().to_string();
    let call = |name: &'static str, args: Value| {
        let (st, tk) = (state.clone(), token.clone());
        async move { tools::call(&st, &tk, name, &args).await }
    };
    let mut outputs: Vec<String> = Vec::new();

    // A page that echoes the password everywhere it can.
    let out = call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/reflect")}),
    )
    .await;
    let stale = ref_of(&out.text, "button \"Show password\"");
    let out = call("fill_login", json!({})).await;
    assert!(out.text.ends_with("filled"), "{}", out.text);
    assert!(out.text.contains("Dialog"), "{}", out.text);
    outputs.push(out.text);
    let snap = call("snapshot", json!({})).await;
    assert!(snap.text.contains("You typed ••••"), "{}", snap.text);
    outputs.push(snap.text.clone());
    // Revealed as plain text: still masked in what the agent reads.
    let show = ref_of(&snap.text, "button \"Show password\"");
    let out = call("click", json!({"ref": show})).await;
    outputs.push(out.text.clone());
    assert!(out.text.contains("textbox \"Password\""), "{}", out.text);
    // Pixels cannot be redacted: no screenshot while the page shows it.
    let shot = call("screenshot", json!({})).await;
    assert!(shot.is_error && shot.image.is_none(), "{}", shot.text);

    // Refs from an earlier page never hit an element of a later one.
    call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/about")}),
    )
    .await;
    let out = call("click", json!({"ref": stale})).await;
    assert!(
        out.is_error && out.text.contains("new snapshot"),
        "{}",
        out.text
    );

    // Cards: nothing is typed while the OK is pending, or after a no.
    let url = format!("http://localhost:{port}/plain-checkout");
    call("navigate", json!({"url": url})).await;
    let out = call("fill_card", json!({})).await;
    assert!(out.text.starts_with("waiting for approval"), "{}", out.text);
    let snap = call("snapshot", json!({})).await;
    assert!(!snap.text.contains("value="), "{}", snap.text);
    let pending = approvals(&state, &conv).await;
    assert_eq!(pending.len(), 1);
    let (id, mut card) = (pending[0].id.clone(), pending[0].card.clone().unwrap());
    assert_eq!(card["state"], "pending");
    card["state"] = "denied".into();
    card["decided_at"] = crate::util::now_ms().into();
    state
        .db
        .call(move |db| crate::db::update_message_card(db, &id, &card))
        .await
        .unwrap();
    let out = call("fill_card", json!({})).await;
    assert!(out.text.starts_with("denied"), "{}", out.text);
    let snap = call("snapshot", json!({})).await;
    assert!(!snap.text.contains("value="), "{}", snap.text);
    // Still just the one card: a fresh "no" is not asked again at once.
    assert_eq!(approvals(&state, &conv).await.len(), 1);

    // Card fields found by name alone are masked once filled.
    let url = format!("http://127.0.0.1:{port}/plain-checkout");
    call("navigate", json!({"url": url})).await;
    let approver = {
        let st = state.clone();
        let conv = conv.clone();
        tokio::spawn(async move {
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Some(m) = approvals(&st, &conv)
                    .await
                    .into_iter()
                    .find(|m| m.card.as_ref().is_some_and(|c| c["state"] == "pending"))
                {
                    let mut card = m.card.unwrap();
                    card["state"] = "approved".into();
                    st.db
                        .call(move |db| crate::db::update_message_card(db, &m.id, &card))
                        .await
                        .unwrap();
                    return;
                }
            }
        })
    };
    let out = call("fill_card", json!({})).await;
    approver.await.unwrap();
    assert_eq!(out.text, "filled");
    let snap = call("snapshot", json!({})).await;
    assert!(snap.text.contains("value=\"08/31\""), "{}", snap.text);
    outputs.push(snap.text);

    for o in &outputs {
        assert!(!o.contains(PASSWORD) && !o.contains(CARD), "{o}");
    }
    drop(guard);
    state.browser.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_pages_cannot_extract_or_redirect_secrets() {
    let Some((_tmp, state, owner, conv)) = setup() else {
        return;
    };
    let (port, _posted) = test_site().await;
    let guard = state
        .browser
        .begin_turn(&owner, &conv, "msg-3", std::slice::from_ref(&owner));
    let token = guard.token().to_string();
    let call = |name: &'static str, args: Value| {
        let (st, tk) = (state.clone(), token.clone());
        async move { tools::call(&st, &tk, name, &args).await }
    };
    let leaks = |text: &str| {
        let escaped = serde_json::to_string(TRICKY).unwrap();
        let folded = TRICKY.split_whitespace().collect::<Vec<_>>().join(" ");
        text.contains(TRICKY)
            || text.contains(&escaped[1..escaped.len() - 1])
            || text.contains(&folded)
            || text.contains(&TRICKY[..20])
            || text.contains("0123456789-0123456789")
    };
    let mut outputs = Vec::new();
    let url = format!("http://localhost:{port}/tricky");
    outputs.push(call("navigate", json!({"url": url})).await.text);
    let out = call("fill_login", json!({})).await;
    assert_eq!(out.text, "filled");
    // The focus handler could not redirect the secret into another field.
    let run = state.browser.running().unwrap();
    let probe = |expr: &'static str| {
        let run = run.clone();
        async move {
            run.cdp
                .call(
                    "Runtime.evaluate",
                    json!({"expression": expr, "returnByValue": true}),
                    Some(&run.page().session_id),
                )
                .await
                .unwrap()["result"]["value"]
                .clone()
        }
    };
    assert_eq!(probe("document.getElementById('other').value").await, "");
    assert_eq!(
        probe("document.getElementById('pw').value.length").await,
        TRICKY.len()
    );
    // Snapshot and title: the long, quoted secret never shows, not even cut.
    let snap = call("snapshot", json!({})).await;
    outputs.push(snap.text.clone());
    assert!(snap.text.starts_with("Page: Hi ••••"), "{}", snap.text);
    // Revealed and unmasked by the page, with no other copy: no screenshot.
    let reveal = ref_of(&snap.text, "button \"Reveal\"");
    outputs.push(call("click", json!({"ref": reveal})).await.text);
    let shot = call("screenshot", json!({})).await;
    assert!(shot.is_error && shot.image.is_none(), "{}", shot.text);
    // A card approval built from this page does not store the echoed secret.
    let out = call("fill_card", json!({})).await;
    assert!(out.text.starts_with("waiting for approval"), "{}", out.text);
    let card = approvals(&state, &conv).await.pop().unwrap().card.unwrap();
    assert!(!leaks(&card.to_string()), "{card}");
    assert!(
        card["preview"]["subject"]
            .as_str()
            .unwrap()
            .contains("••••")
    );
    for o in &outputs {
        assert!(!leaks(o), "{o}");
    }

    // A turn that ends while the browser is starting does nothing after.
    state.browser.close().await;
    let held = state.browser.launch_lock_for_test().await;
    let (st, tk) = (state.clone(), token.clone());
    let pending = tokio::spawn(async move {
        tools::call(&st, &tk, "navigate", &json!({"url": "https://example.com"})).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(guard);
    drop(held);
    let out = tokio::time::timeout(Duration::from_secs(20), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.is_error && out.text == super::TURN_ENDED,
        "{}",
        out.text
    );
    state.browser.close().await;
}
