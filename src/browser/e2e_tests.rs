//! End-to-end tests against a real Chromium, when one is installed (they
//! pass trivially otherwise, except in CI, where `IRIS_REQUIRE_BROWSER`
//! makes a missing Chromium a failure). A tiny local web server plays a
//! sign-in page, a checkout page, and a page of web basics with CAPTCHA-
//! style widgets; the tests drive them through the same tool entry point
//! the MCP bridge uses and check that secrets reach the page but never a
//! tool result.

use super::tools;
use crate::config::{Config, Paths};
use crate::state::AppState;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "S3cret-Pa55-xyz!";
/// Quotes, a backslash, doubled spaces, and longer than snapshot cuts.
const TRICKY: &str = concat!(
    "q\"uo  te\\back-slash+plus/slash?query&amp-",
    "0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-",
    "0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-",
    "0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-",
    "end-päss-ü"
);
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

/// A one-page, two-step sign-in (like X): the username step swaps itself
/// for a password step without a new document or path.
const TWO_STEP_PAGE: &str = r#"<!doctype html><html><head><title>Two step</title></head><body>
<form id="f" method="post" action="/login"><div id="step">
<input name="email" autocomplete="username" aria-label="Phone, email, or username">
</div><button>Next</button></form>
<script>
f.onsubmit = (e) => {
  if (document.querySelector('input[type=password]')) return;
  e.preventDefault();
  const u = f.email.value;
  step.innerHTML = '<input type="hidden" name="email"><input type="password" name="password" aria-label="Password">';
  f.email.value = u;
};
</script></body></html>"#;

/// Everyday web basics on one page, plus three CAPTCHA-style widgets, each
/// in a frame from another origin (like reCAPTCHA, hCaptcha, Turnstile) so
/// the snapshot cannot reach into them. Results land in `#status`.
const BASICS_PAGE: &str = r##"<!doctype html><html><head><title>Basics</title>
<meta name="viewport" content="width=device-width">
<style>body{margin:8px;font:16px sans-serif} iframe{border:0;display:block;margin:4px 0}
#menu .items{display:none} #menu:hover .items{display:block}</style></head><body>
<h1>Basics</h1>
<p id="status">Status:</p>
<iframe title="Robot check" src="http://localhost:PORT/captcha-box" width="300" height="80"></iframe>
<iframe title="Picture check" src="http://localhost:PORT/captcha-grid" width="270" height="310"></iframe>
<iframe title="Slider check" src="http://localhost:PORT/captcha-slider" width="320" height="60"></iframe>
<input id="q" aria-label="Search" onkeydown="if (event.key === 'Enter') say('searched ' + this.value)">
<button ondblclick="say('doubled')">Double me</button>
<div id="menu"><button>Menu</button><div class="items"><a href="/about">Hidden item</a></div></div>
<button onclick="alert('Hello'); say('after-alert')">Alert me</button>
<button onclick="setTimeout(() => say('loaded-late'), 1500)">Load later</button>
<a href="/about" target="_blank">Open popup</a>
<a href="/about">About link</a>
<div id="host"></div>
<iframe title="Inner" srcdoc="<button onclick=&quot;this.textContent='Inner clicked'&quot;>Inner button</button>"></iframe>
<div style="height:3000px"></div><p>Bottom of page</p>
<script>
function say(t) { document.getElementById('status').textContent += ' ' + t; }
addEventListener('message', (e) => say(String(e.data)));
document.getElementById('host').attachShadow({ mode: 'open' }).innerHTML =
  '<button onclick="this.textContent = \'Shadow clicked\'">Shadow button</button>';
</script></body></html>"##;

/// "I'm not a robot": a box that only counts when the box itself is clicked.
const CAPTCHA_BOX: &str = r##"<!doctype html><body style="margin:0">
<div id="box" role="checkbox" aria-label="I'm not a robot"
  style="position:absolute;left:20px;top:20px;width:40px;height:40px;border:2px solid #555"></div>
<script>
box.onclick = () => { box.style.background = 'green'; parent.postMessage('robot-passed', '*'); };
parent.postMessage('ready-box', '*');
</script></body>"##;

/// Pick the right tiles (unlabeled colors only a screenshot shows), then Verify.
const CAPTCHA_GRID: &str = r##"<!doctype html><body style="margin:0">
<div id="g" style="display:grid;grid-template-columns:repeat(3,90px);grid-auto-rows:90px"></div>
<div id="v" style="position:absolute;left:0;top:270px;width:270px;height:40px;background:#36c"></div>
<script>
const want = [0, 4, 8], picked = new Set();
for (let i = 0; i < 9; i++) {
  const t = document.createElement('div');
  t.style.background = want.includes(i) ? 'red' : 'gray';
  t.style.outline = '1px solid white';
  t.onclick = () => { picked.has(i) ? picked.delete(i) : picked.add(i); t.style.opacity = picked.has(i) ? 0.5 : 1; };
  g.appendChild(t);
}
v.onclick = () => {
  const ok = picked.size === want.length && want.every((i) => picked.has(i));
  parent.postMessage(ok ? 'grid-passed' : 'grid-failed', '*');
  picked.clear();
};
parent.postMessage('ready-grid', '*');
</script></body>"##;

/// Slide to verify: drag the handle to the end of the track.
const CAPTCHA_SLIDER: &str = r##"<!doctype html><body style="margin:0">
<div style="position:absolute;left:10px;top:10px;width:300px;height:40px;background:#ddd"></div>
<div id="h" style="position:absolute;left:10px;top:10px;width:40px;height:40px;background:#36c"></div>
<script>
let start = null, moves = 0;
h.onpointerdown = (e) => { start = e.clientX - h.offsetLeft; h.setPointerCapture(e.pointerId); };
h.onpointermove = (e) => {
  if (start === null) return;
  moves++;
  h.style.left = Math.max(10, Math.min(270, e.clientX - start)) + 'px';
};
h.onpointerup = () => {
  if (start === null) return;
  start = null;
  // A jump straight to the end is a robot: a person moves on the way.
  parent.postMessage(h.offsetLeft >= 260 && moves > 3 ? 'slider-passed' : 'slider-failed', '*');
};
parent.postMessage('ready-slider', '*');
</script></body>"##;

const CHECKOUT_PAGE: &str = r#"<!doctype html><html><head><title>Checkout</title></head><body>
<h1>Pay</h1>
<form method="post" action="/pay">
<input name="cardnumber" autocomplete="cc-number" placeholder="Card number">
<input name="exp" autocomplete="cc-exp" placeholder="MM / YY">
<input name="cvc" autocomplete="cc-csc" placeholder="CVC" oninput="localStorage.setItem('cvc', this.value)">
<input name="holder" autocomplete="cc-name" placeholder="Name on card">
<button>Pay now</button>
</form></body></html>"#;

/// A sign-in page that echoes what is typed into its password field: in a
/// dialog, a paragraph, another field, and transformed metadata. Reads
/// must refuse until submission loads a new document.
const REFLECT_PAGE: &str = r#"<!doctype html><html><head><title>Echo sign in</title></head><body>
<h1>Echo</h1><form method="post" action="/login">
<input name="email" type="email" autocomplete="username" aria-label="Email">
<input id="pw" name="password" type="password" aria-label="Password"
  oninput="document.getElementById('echo').textContent = 'You typed ' + this.value;
           document.getElementById('copy').value = this.value; alert('Got ' + this.value);
           const v = this.value, a = v.slice(0, 6), b = v.slice(6);
           document.title = btoa([...v].reverse().join(''));
           history.replaceState(null, '', '/reflect#' + btoa(v));
           document.getElementById('split').innerHTML = 'Split: <span>' + a + '</span><span>' + b + '</span>';
           document.getElementById('blocks').innerHTML = '<p>' + a + '</p><p>' + b + '</p><span>' + a + '</span><button>' + b + '</button>'">
<input id="copy" aria-label="Copy">
<button type="button" onclick="pw.type = 'text'">Show password</button>
<button type="submit">Sign in</button>
</form><p id="echo"></p><p id="split"></p><div id="blocks"></div></body></html>"#;

/// A saved password shown in a masked field (never filled by Iris), one
/// in an open shadow root, and what the page kept from an earlier fill.
const SAVED_PAGE: &str = r#"<!doctype html><html><head><title>Saved</title></head><body>
<h1>Saved</h1>
<input id="m" type="password" value="S3cret-Pa55-xyz!" aria-label="Saved">
<button type="button" onclick="m.type = 'text'">Reveal</button>
<p id="kept"></p><div id="host"></div>
<script>
document.getElementById('kept').textContent = 'Kept: ' + (localStorage.getItem('cvc') || '');
if (location.hash === '#shadow') {
  document.getElementById('host').attachShadow({ mode: 'open' }).innerHTML = '<input value="S3cret-Pa55-xyz!">';
}
</script></body></html>"#;

/// A password field the page later reveals and unmasks, with no other copy;
/// a focus handler that tries to move focus elsewhere; the password
/// echoed into the title; and a card form on the same page.
const TRICKY_PAGE: &str = r#"<!doctype html><html><head><title>Tricky</title></head><body>
<h1>Tricky</h1><form>
<input name="email" type="email" autocomplete="username" aria-label="Email">
<input id="pw" name="password" type="password" aria-label="Password"
  onfocus="queueMicrotask(() => document.getElementById('other').focus())"
  oninput="document.title = 'Hi ' + this.value; document.getElementById('mirror').value = this.value;
           document.getElementById('esc').textContent = encodeURIComponent(this.value).replace(/%[0-9A-F]{2}/g, (m) => m.toLowerCase())">
<input id="other" aria-label="Other">
<label>Mirror <input id="mirror"></label>
<button type="submit">Go</button>
<p id="esc"></p>
<button type="button" onclick="pw.type = 'text'; pw.style.removeProperty('-webkit-text-security')">Reveal</button>
<input name="cardnumber" autocomplete="cc-number" aria-label="Card number">
</form></body></html>"#;

/// Card fields recognized by name only (no autocomplete hints).
const PLAIN_CHECKOUT: &str = r#"<!doctype html><html><head><title>Plain checkout</title></head><body>
<h1>Pay</h1><form method="post" action="/pay">
<label>Card number <input name="card_number"
  oninput="this.value = this.value.replace(/\D/g, '').replace(/(\d{4})(?=\d)/g, '$1 ')"></label>
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
                            } else if line.starts_with("GET /login") {
                                r#"<!doctype html><title>Sign in</title><form onsubmit="event.preventDefault();location.href='/sink#'+Array.from(pw.value).map(c=>c.charCodeAt(0).toString(16).padStart(2,'0')).join('')"><input id="pw" name="password" type="password" aria-label="Password"><button>Sign in</button></form>"#.into()
                            } else if line.starts_with("GET /redirect-fill") {
                                r#"<!doctype html><title>Sign in</title><input name="password" type="password" aria-label="Password" oninput="location.href='/sink#'+Array.from(this.value).map(c=>c.charCodeAt(0).toString(16).padStart(2,'0')).join('')">"#.into()
                            } else if line.starts_with("GET /sink") {
                                r#"<!doctype html><title>Sink</title><h1>Sink</h1><p id="echo"></p><script>echo.textContent=location.hash;document.title=location.hash</script>"#.into()
                            } else if line.starts_with("GET /basics") {
                                BASICS_PAGE.replace("PORT", &port.to_string())
                            } else if line.starts_with("GET /captcha-box") {
                                CAPTCHA_BOX.to_string()
                            } else if line.starts_with("GET /captcha-grid") {
                                CAPTCHA_GRID.to_string()
                            } else if line.starts_with("GET /captcha-slider") {
                                CAPTCHA_SLIDER.to_string()
                            } else if line.starts_with("GET /two-step") {
                                TWO_STEP_PAGE.to_string()
                            } else if line.starts_with("GET /tricky") {
                                TRICKY_PAGE.to_string()
                            } else if line.starts_with("GET /saved") {
                                SAVED_PAGE.to_string()
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

/// Where a frame from another site sits on the screenshot, from its
/// snapshot line (`... frame at x=8 y=60, 300x80: ...`).
fn frame_at(snapshot: &str, title: &str) -> (f64, f64) {
    let line = snapshot
        .lines()
        .find(|l| l.contains(&format!("iframe \"{title}\"")))
        .unwrap_or_else(|| panic!("no {title} frame in:\n{snapshot}"));
    let num = |key: &str| -> f64 {
        let i = line
            .find(key)
            .unwrap_or_else(|| panic!("no {key} in {line}"))
            + key.len();
        line[i..]
            .split(|c: char| !c.is_ascii_digit() && c != '-')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    (num(" x="), num(" y="))
}

fn bind_logins(state: &AppState, owner: &str, port: u16) {
    state
        .db
        .with(|c| {
            for (id, host) in [
                ("cred-login", "127.0.0.1"),
                ("cred-login-local", "localhost"),
            ] {
                let mut cred = crate::entities::get_credential(c, owner, id)?.unwrap();
                cred.metadata["site"] = format!("http://{host}:{port}").into();
                crate::entities::update_credential(c, owner, id, None, Some(&cred.metadata), None)?;
            }
            Ok(())
        })
        .unwrap();
}

/// A server state with a saved login (for 127.0.0.1 and localhost) and a
/// wallet card. None when no Chromium is installed.
fn setup() -> Option<(tempfile::TempDir, AppState, String, String)> {
    if super::launch::find("").is_none() {
        // CI sets this so the browser tests can never pass by skipping.
        assert!(
            std::env::var_os("IRIS_REQUIRE_BROWSER").is_none(),
            "IRIS_REQUIRE_BROWSER is set but no Chromium was found"
        );
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
    cfg.safety.approval_timeout_secs = 2;
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

/// Simulate the owner's decision route: persist, then release the gateway waiter.
fn decide_next(state: &AppState, conv: &str, approve: bool) -> tokio::task::JoinHandle<()> {
    let (st, conv) = (state.clone(), conv.to_string());
    tokio::spawn(async move {
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(m) = approvals(&st, &conv)
                .await
                .into_iter()
                .find(|m| m.card.as_ref().is_some_and(|c| c["state"] == "pending"))
            {
                let id = m.id.clone();
                let mut card = m.card.unwrap();
                assert!(!card.to_string().contains(CARD));
                assert!(!card.to_string().contains(PASSWORD));
                card["state"] = if approve { "approved" } else { "denied" }.into();
                card["decided_at"] = crate::util::now_ms().into();
                st.db
                    .call(move |db| crate::db::update_message_card(db, &m.id, &card))
                    .await
                    .unwrap();
                crate::gateway::decided(&st, &id, approve);
                return;
            }
        }
        panic!("no pending wallet approval");
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browses_fills_logins_and_cards_without_leaking_them() {
    let Some((_tmp, state, owner, conv)) = setup() else {
        return;
    };
    let (port, posted) = test_site().await;
    bind_logins(&state, &owner, port);
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

    let shot = call("screenshot", json!({})).await;
    assert!(shot.image.as_ref().is_some_and(|i| i.len() > 1000));
    outputs.push(shot.text);

    let out = call("fill_login", json!({"site": "example.com"})).await;
    assert!(out.text.starts_with("not found"), "{}", out.text);
    let out = call("fill_login", json!({})).await;
    assert_eq!(out.text, "filled");

    let out = call("snapshot", json!({})).await;
    assert!(
        out.is_error && out.text == tools::TAINTED_PAGE,
        "{}",
        out.text
    );
    outputs.push(out.text.clone());

    // Never a picture of a page Iris typed a secret into.
    let shot = call("screenshot", json!({})).await;
    assert!(shot.is_error && shot.image.is_none(), "{}", shot.text);

    let sign_in = ref_of(&snap, "button \"Sign in\"");
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
    let data = |json: &str| -> String {
        serde_json::from_str::<Value>(json).unwrap()["data"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let before = data(&frames.borrow_and_update().as_ref().unwrap().json);
    call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/about")}),
    )
    .await;
    // Wait for a frame whose pixels differ (not just a newer number).
    tokio::time::timeout(
        Duration::from_secs(10),
        frames.wait_for(|f| f.as_ref().is_some_and(|f| data(&f.json) != before)),
    )
    .await
    .expect("a frame showing the new page")
    .unwrap();
    // Nobody watching: the screencast stops.
    drop(watch_guard);
    for _ in 0..150 {
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

    let admitted = tokio::spawn(call(
        "wait",
        json!({"text": "owner-only text", "seconds": 30}),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state.browser.state_json()["busy"] == true {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state.browser.take_over(&owner, "dev-1").await.unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(2), admitted)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stopped.is_error && stopped.text == super::TAKEN_OVER && stopped.image.is_none(),
        "{}",
        stopped.text
    );
    state.browser.hand_back();

    // Cards: the owner approves on a card in the chat, then Iris fills.
    let out = call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/checkout")}),
    )
    .await;
    assert!(out.text.contains("Card number"), "{}", out.text);
    let pay = ref_of(&out.text, "button \"Pay now\"");
    let approver = decide_next(&state, &conv, true);
    let out = call("fill_card", json!({})).await;
    approver.await.unwrap();
    assert_eq!(out.text, "filled");
    // One approval fills once.
    let again = call("fill_card", json!({"card": "4242"})).await;
    assert!(again.text.starts_with("not filled"), "{}", again.text);
    let snap = call("snapshot", json!({})).await;
    assert!(!snap.text.contains("4242424242424242"), "{}", snap.text);
    outputs.push(snap.text.clone());
    assert!(
        snap.is_error && snap.text == tools::TAINTED_PAGE,
        "{}",
        snap.text
    );
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
    // Chrome's session files (which hold typed form data) are gone.
    assert!(
        !state
            .paths
            .browser_dir()
            .join("profile/Default/Sessions")
            .exists()
    );

    // After a restart of Iris (same data directory): the site Iris filled
    // stays off-limits for screenshots even on a clean page, and a CVC the
    // page kept is still redacted.
    let paths = state.paths.clone();
    // The card is deleted: its CVC now lives only in the encrypted history.
    let owner3 = owner.clone();
    state
        .db
        .with(|c| crate::entities::delete_credential(c, &owner3, "cred-card"))
        .unwrap();
    drop(state);
    let db = crate::db::Db::open(&paths.db_file()).unwrap();
    let mut cfg = Config::default();
    for h in cfg.harness.values_mut() {
        h.enabled = false;
    }
    cfg.safety.approval_timeout_secs = 2;
    let state = AppState::new(cfg, paths, db).unwrap();
    state.browser.init_allow_no_sandbox(true);
    let guard = state
        .browser
        .begin_turn(&owner, &conv, "msg-9", std::slice::from_ref(&owner));
    let token = guard.token().to_string();
    let call = |name: &'static str, args: Value| {
        let (st, tk) = (state.clone(), token.clone());
        async move { tools::call(&st, &tk, name, &args).await }
    };
    let out = call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/saved")}),
    )
    .await;
    assert!(
        out.text.contains("Kept: ••••") && !out.text.contains(CVC),
        "{}",
        out.text
    );
    // No guessing it digit by digit through `wait` either.
    let out = call(
        "wait",
        json!({"text": format!("Kept: {}", &CVC[..1]), "seconds": 1}),
    )
    .await;
    assert!(out.text.contains("did not appear"), "{}", out.text);
    let shot = call("screenshot", json!({})).await;
    assert!(shot.is_error && shot.image.is_none(), "{}", shot.text);
    call(
        "navigate",
        json!({"url": format!("http://localhost:{port}/about")}),
    )
    .await;
    let shot = call("screenshot", json!({})).await;
    assert!(shot.image.is_some(), "{}", shot.text);
    drop(guard);
    state.browser.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflected_secrets_pending_and_denied_cards_and_stale_refs() {
    let Some((_tmp, state, owner, conv)) = setup() else {
        return;
    };
    let (port, _posted) = test_site().await;
    bind_logins(&state, &owner, port);
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
    let submit = ref_of(&out.text, "button \"Sign in\"");
    let out = call("fill_login", json!({})).await;
    assert!(out.text.ends_with("filled"), "{}", out.text);
    assert!(out.text.contains("Dialog"), "{}", out.text);
    outputs.push(out.text);
    let snap = call("snapshot", json!({})).await;
    assert!(
        snap.is_error && snap.text == tools::TAINTED_PAGE,
        "{}",
        snap.text
    );
    assert!(
        !snap.text.contains("S3cret-") && !snap.text.contains("Pa55-xyz"),
        "{}",
        snap.text
    );
    outputs.push(snap.text.clone());
    // Revealed as plain text: actions still work, reads still refuse.
    let show = stale.clone();
    let out = call("click", json!({"ref": show})).await;
    outputs.push(out.text.clone());
    assert!(
        !out.is_error && out.text == tools::STILL_SIGNING_IN,
        "{}",
        out.text
    );
    // Pixels cannot be redacted: no screenshot while the page shows it.
    let shot = call("screenshot", json!({})).await;
    assert!(shot.is_error && shot.image.is_none(), "{}", shot.text);

    // Text queries refuse without waiting for the requested deadline, and
    // tab metadata cannot disclose transformed titles or URL fragments.
    let waiting = tokio::time::timeout(
        Duration::from_secs(2),
        call("wait", json!({"text": "S3", "seconds": 30})),
    )
    .await
    .unwrap();
    assert!(
        waiting.is_error && waiting.text == tools::TAINTED_PAGE,
        "{}",
        waiting.text
    );
    let list = call("tabs", json!({"action": "list"})).await;
    assert!(list.text.contains(tools::TAINTED_PAGE), "{}", list.text);
    assert!(
        !list.text.contains("/reflect") && !list.text.contains('#'),
        "{}",
        list.text
    );
    // Submission still uses the pre-fill ref and a new document is readable.
    let submitted = call("click", json!({"ref": submit})).await;
    assert!(
        !submitted.is_error && submitted.text.contains("Welcome back"),
        "{}",
        submitted.text
    );

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

    // A denied held call types nothing; an expired call cannot be revived.
    let url = format!("http://localhost:{port}/plain-checkout");
    call("navigate", json!({"url": url})).await;
    let denier = decide_next(&state, &conv, false);
    let out = call("fill_card", json!({})).await;
    denier.await.unwrap();
    assert!(out.text.starts_with("denied"), "{}", out.text);
    let snap = call("snapshot", json!({})).await;
    assert!(!snap.text.contains("value="), "{}", snap.text);
    let denied = approvals(&state, &conv).await;
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].card.as_ref().unwrap()["state"], "denied");

    // An OK for one origin never covers another (same host, other port).
    let (port_b, _) = test_site().await;
    call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/plain-checkout")}),
    )
    .await;
    let out = call("fill_card", json!({})).await;
    assert!(out.text.starts_with("not filled"), "{}", out.text);
    let pending = approvals(&state, &conv).await;
    assert_eq!(
        pending.last().unwrap().card.as_ref().unwrap()["state"],
        "expired"
    );
    call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port_b}/plain-checkout")}),
    )
    .await;
    let out = call("fill_card", json!({})).await;
    assert!(out.text.starts_with("not filled"), "{}", out.text);
    assert_eq!(approvals(&state, &conv).await.len(), pending.len() + 1);

    // Card fields found by name alone are masked once filled, also when the
    // page reformats the number; the rest of the form is filled too.
    let url = format!("http://127.0.0.1:{port}/plain-checkout");
    call("navigate", json!({"url": url})).await;
    let approver = decide_next(&state, &conv, true);
    let out = call("fill_card", json!({})).await;
    approver.await.unwrap();
    assert_eq!(out.text, "filled");
    let snap = call("snapshot", json!({})).await;
    assert!(
        snap.is_error && snap.text == tools::TAINTED_PAGE,
        "{}",
        snap.text
    );
    outputs.push(snap.text);

    // The text check on its own, on a site Iris never filled: a saved
    // password masked is fine; revealed (or in a shadow root) it is not.
    let saved = format!("http://127.0.0.1:{port_b}/saved");
    let out = call("navigate", json!({"url": saved})).await;
    outputs.push(out.text.clone());
    let shot = call("screenshot", json!({})).await;
    assert!(shot.image.is_some(), "{}", shot.text);
    let reveal = ref_of(&out.text, "button \"Reveal\"");
    outputs.push(call("click", json!({"ref": reveal})).await.text);
    let shot = call("screenshot", json!({})).await;
    assert!(
        shot.is_error && shot.text.contains("shows saved"),
        "{}",
        shot.text
    );
    let out = call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port_b}/saved?x#shadow")}),
    )
    .await;
    outputs.push(out.text);
    let shot = call("screenshot", json!({})).await;
    assert!(
        shot.is_error && shot.text.contains("shows saved"),
        "{}",
        shot.text
    );

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
    bind_logins(&state, &owner, port);
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
    let initial = call("navigate", json!({"url": url.clone()})).await;
    let reveal = ref_of(&initial.text, "button \"Reveal\"");
    let go = ref_of(&initial.text, "button \"Go\"");
    outputs.push(initial.text);
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
        TRICKY.chars().count()
    );
    // Snapshot and title: the long, quoted secret never shows, not even cut.
    let snap = call("snapshot", json!({})).await;
    outputs.push(snap.text.clone());
    assert!(
        snap.is_error && snap.text == tools::TAINTED_PAGE,
        "{}",
        snap.text
    );
    // Revealed and unmasked by the page, with no other copy: no screenshot.
    outputs.push(call("click", json!({"ref": reveal})).await.text);
    let shot = call("screenshot", json!({})).await;
    assert!(shot.is_error && shot.image.is_none(), "{}", shot.text);
    // A card approval built from this page does not store the echoed secret.
    let out = call("fill_card", json!({})).await;
    assert!(out.text.starts_with("not filled"), "{}", out.text);
    let card = approvals(&state, &conv).await.pop().unwrap().card.unwrap();
    assert!(!leaks(&card.to_string()), "{card}");
    assert!(!card["preview"]["subject"].as_str().unwrap().contains("Hi "));
    // Switching tabs and back: the app's status shows no echoed secret.
    outputs.push(call("tabs", json!({"action": "new"})).await.text);
    // Tab order is the browser's; pick the tricky page's tab by its address.
    let list = call("tabs", json!({"action": "list"})).await.text;
    outputs.push(list.clone());
    let index: u64 = list
        .lines()
        .find(|l| l.contains(tools::TAINTED_PAGE))
        .and_then(|l| l.split(':').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no tricky tab in {list}"));
    outputs.push(
        call("tabs", json!({"action": "select", "index": index}))
            .await
            .text,
    );
    let status = state.browser.state_json().to_string();
    assert!(!leaks(&status), "{status}");
    // Reloading the same path, even after an explicit submit, stays tainted.
    let snap = call("snapshot", json!({})).await;
    assert!(snap.is_error && snap.text == tools::TAINTED_PAGE);
    let after = call("click", json!({"ref": go})).await;
    assert!(
        !after.is_error && after.text == tools::STILL_SIGNING_IN,
        "{}",
        after.text
    );
    outputs.push(after.text);
    let shot = call("screenshot", json!({})).await;
    assert!(shot.is_error && shot.image.is_none(), "{}", shot.text);
    // An explicit navigation to a different path releases the tab.
    let clean = call(
        "navigate",
        json!({"url": format!("http://localhost:{port}/about")}),
    )
    .await;
    assert!(!clean.is_error, "{}", clean.text);
    outputs.push(clean.text);
    outputs.push(call("navigate", json!({"url": url})).await.text);
    // Ordinary typing goes to the requested field even when a focus
    // handler tries to move focus elsewhere.
    let snap = call("snapshot", json!({})).await;
    outputs.push(snap.text.clone());
    let pw = ref_of(&snap.text, "password field \"Password\"");
    let out = call("type", json!({"ref": pw, "text": "hello"})).await;
    assert_eq!(out.text, "Typed it.");
    // Enter is not pressed when the page moved the focus away.
    let out = call("type", json!({"ref": pw, "text": "hello", "submit": true})).await;
    assert!(
        out.is_error && out.text.contains("did not press Enter"),
        "{}",
        out.text
    );
    let run = state.browser.running().unwrap();
    let other = run
        .cdp
        .call(
            "Runtime.evaluate",
            json!({"expression": "document.getElementById('other').value + '|' + document.getElementById('pw').value", "returnByValue": true}),
            Some(&run.page().session_id),
        )
        .await
        .unwrap();
    assert_eq!(other["result"]["value"], "|hello");
    for o in &outputs {
        assert!(!leaks(o), "{o}");
    }

    // A password too short to redact safely is not filled at all.
    let ct = state
        .vault
        .encrypt(
            json!({"password": "ab"}).to_string().as_bytes(),
            "cred-login-local",
        )
        .unwrap();
    let owner2 = owner.clone();
    state
        .db
        .with(|c| {
            crate::entities::update_credential(
                c,
                &owner2,
                "cred-login-local",
                None,
                None,
                Some(&ct),
            )
        })
        .unwrap();
    let url = format!("http://localhost:{port}/tricky");
    call("navigate", json!({"url": url})).await;
    let out = call("fill_login", json!({})).await;
    assert!(
        out.text
            .starts_with("not found (the saved secret is too short"),
        "{}",
        out.text
    );

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_redirect_keeps_encoded_secret_tainted() {
    let Some((_tmp, state, owner, conv)) = setup() else {
        return;
    };
    let (port, _) = test_site().await;
    bind_logins(&state, &owner, port);
    let guard = state.browser.begin_turn(
        &owner,
        &conv,
        "redirect-regression",
        std::slice::from_ref(&owner),
    );
    let call = |name: &'static str, args: Value| {
        let (st, token) = (state.clone(), guard.token().to_string());
        async move { tools::call(&st, &token, name, &args).await }
    };
    assert!(
        !call(
            "navigate",
            json!({"url": format!("http://127.0.0.1:{port}/redirect-fill")})
        )
        .await
        .is_error
    );
    let _ = call("fill_login", json!({})).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let run = state.browser.running().unwrap();
    let nav = tools::eval(&run, "navigationState()").await.unwrap();
    assert_eq!(nav["path"], "/sink", "reproduction must navigate");
    let hex = crate::util::hex(PASSWORD.as_bytes());
    for (name, args) in [
        ("snapshot", json!({})),
        ("wait", json!({"text": "Sink"})),
        ("screenshot", json!({})),
    ] {
        let out = call(name, args).await;
        assert!(
            out.is_error && out.text == tools::TAINTED_PAGE,
            "{name}: {}",
            out.text
        );
        assert!(!out.text.contains(&hex));
    }
    let tabs = call("tabs", json!({"action":"list"})).await;
    assert!(tabs.text.contains(tools::TAINTED_PAGE) && !tabs.text.contains(&hex));
    let clean = call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/about")}),
    )
    .await;
    assert!(!clean.is_error, "{}", clean.text);
    state.browser.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_redirect_redacts_encoded_secret_in_every_read() {
    let Some((_tmp, state, owner, conv)) = setup() else {
        return;
    };
    let (port, _) = test_site().await;
    bind_logins(&state, &owner, port);
    let guard = state.browser.begin_turn(
        &owner,
        &conv,
        "submit-regression",
        std::slice::from_ref(&owner),
    );
    let call = |name: &'static str, args: Value| {
        let (st, token) = (state.clone(), guard.token().to_string());
        async move { tools::call(&st, &token, name, &args).await }
    };
    let hex = crate::util::hex(PASSWORD.as_bytes());
    for action in ["click", "press_key"] {
        let login = call(
            "navigate",
            json!({"url": format!("http://127.0.0.1:{port}/login")}),
        )
        .await;
        assert!(!login.is_error, "{}", login.text);
        let submit = ref_of(&login.text, "button \"Sign in\"");
        let filled = call("fill_login", json!({})).await;
        assert!(
            !filled.is_error && filled.text.contains("filled"),
            "{}",
            filled.text
        );
        let out = call(
            action,
            if action == "click" {
                json!({"ref": submit})
            } else {
                json!({"key": "Enter"})
            },
        )
        .await;
        assert!(!out.is_error, "{}", out.text);
        assert!(!out.text.contains(&hex), "{}", out.text);
        let run = state.browser.running().unwrap();
        assert_eq!(
            tools::eval(&run, "navigationState()").await.unwrap()["path"],
            "/sink"
        );
        // Check that this is the reproduction, with the actual hex value
        // in the URL; the isolated helper must mask it before every read.
        let page = run.page();
        let targets = run
            .cdp
            .call("Target.getTargets", json!({}), None)
            .await
            .unwrap();
        assert!(
            targets["targetInfos"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["targetId"] == page.target_id
                    && t["url"].as_str().is_some_and(|url| url.ends_with(&hex)))
        );
        for (name, args) in [
            ("snapshot", json!({})),
            ("wait", json!({"text": "Sink"})),
            ("tabs", json!({"action": "list"})),
        ] {
            let out = call(name, args).await;
            assert!(
                !out.is_error && !out.text.contains(&hex),
                "{name}: {}",
                out.text
            );
        }
        let probe = call("wait", json!({"text": &hex[..8], "seconds": 0.1})).await;
        assert!(probe.text.contains("did not appear"), "{}", probe.text);
        let info = tools::eval(&run, "info()").await.unwrap();
        assert!(!info["title"].as_str().unwrap().contains(&hex));
        assert!(!info["url"].as_str().unwrap().contains(&hex));
    }
    state.browser.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_matching_uses_the_origin_even_when_the_display_port_is_redacted() {
    let Some((_tmp, state, owner, conv)) = setup() else {
        return;
    };
    let (port, _) = test_site().await;
    bind_logins(&state, &owner, port);
    state.browser.ensure().await.unwrap();
    state.browser.learn_secrets(&[port.to_string()]);
    let guard =
        state
            .browser
            .begin_turn(&owner, &conv, "redacted-port", std::slice::from_ref(&owner));
    let initial = tools::call(
        &state,
        guard.token(),
        "navigate",
        &json!({"url":format!("http://127.0.0.1:{port}/")}),
    )
    .await;
    assert!(!initial.is_error, "{}", initial.text);
    assert!(
        initial.text.contains("http://127.0.0.1:••••"),
        "{}",
        initial.text
    );
    let filled = tools::call(&state, guard.token(), "fill_login", &json!({})).await;
    assert_eq!(filled.text, "filled");
    state.browser.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_page_two_step_sign_in_submits_without_reading() {
    let Some((_tmp, state, owner, conv)) = setup() else {
        return;
    };
    let (port, posted) = test_site().await;
    bind_logins(&state, &owner, port);
    let guard = state
        .browser
        .begin_turn(&owner, &conv, "two-step", std::slice::from_ref(&owner));
    let call = |name: &'static str, args: Value| {
        let (st, token) = (state.clone(), guard.token().to_string());
        async move { tools::call(&st, &token, name, &args).await }
    };
    let out = call(
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/two-step")}),
    )
    .await;
    assert!(!out.is_error, "{}", out.text);
    // Username step: filled and submitted, but the page stays locked.
    let out = call("fill_login", json!({"submit": true})).await;
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains(tools::STILL_SIGNING_IN), "{}", out.text);
    // Password step on the same page: signs in and the page reads again.
    let out = call("fill_login", json!({"submit": true})).await;
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("Welcome back"), "{}", out.text);
    assert!(!out.text.contains(PASSWORD));
    let p = posted.lock().unwrap();
    let body = p.first().expect("the form was posted");
    assert!(body.contains("password=S3cret-Pa55-xyz%21"), "{body}");
}

/// Every basic the agent relies on, against a real Chromium: typing and
/// Enter, keys, double-click, hover menus, dialogs, delayed text, shadow
/// DOM, same-site frames, scrolling, Back, popups and tabs, screenshots,
/// and three CAPTCHA-style widgets in frames from another site (checkbox,
/// picture grid, slider) solved by position from the screenshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn does_the_web_basics_and_solves_captchas() {
    let Some((_tmp, state, owner, conv)) = setup() else {
        return;
    };
    let (port, _) = test_site().await;
    let guard = state
        .browser
        .begin_turn(&owner, &conv, "basics", std::slice::from_ref(&owner));
    let call = |name: &'static str, args: Value| {
        let (st, token) = (state.clone(), guard.token().to_string());
        async move {
            let out = tools::call(&st, &token, name, &args).await;
            assert!(!out.is_error, "{name} {args}: {}", out.text);
            out
        }
    };
    let status = || async {
        let run = state.browser.running().unwrap();
        run.cdp
            .call(
                "Runtime.evaluate",
                json!({"expression": "document.getElementById('status')?.textContent ?? ''",
                       "returnByValue": true}),
                Some(&run.page().session_id),
            )
            .await
            .unwrap()["result"]["value"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    let url = format!("http://127.0.0.1:{port}/basics");
    call("navigate", json!({"url": url})).await;
    // The frames say when they are ready; waiting for text sees it.
    for ready in ["ready-box", "ready-grid", "ready-slider"] {
        let out = call("wait", json!({"text": ready, "seconds": 10})).await;
        assert!(out.text.contains(ready), "{}", out.text);
    }
    let snap = call("snapshot", json!({})).await.text;
    assert!(snap.contains("another site's frame at x="), "{snap}");

    // A screenshot is available to look at the puzzles.
    let shot = call("screenshot", json!({})).await;
    assert!(shot.image.as_ref().is_some_and(|i| i.len() > 1000));

    // Checkbox CAPTCHA: a click beside the box does nothing, on it passes.
    let (bx, by) = frame_at(&snap, "Robot check");
    call("click_at", json!({"x": bx + 200.0, "y": by + 40.0})).await;
    assert!(!status().await.contains("robot-passed"));
    call("click_at", json!({"x": bx + 40.0, "y": by + 40.0})).await;
    assert!(
        status().await.contains("robot-passed"),
        "{}",
        status().await
    );

    // Picture CAPTCHA: a wrong pick fails, the right tiles pass.
    let (gx, gy) = frame_at(&snap, "Picture check");
    let tile = |i: usize| json!({"x": gx + 45.0 + 90.0 * (i % 3) as f64, "y": gy + 45.0 + 90.0 * (i / 3) as f64});
    let verify = json!({"x": gx + 135.0, "y": gy + 290.0});
    call("click_at", tile(1)).await;
    call("click_at", verify.clone()).await;
    assert!(status().await.contains("grid-failed"), "{}", status().await);
    for i in [0, 4, 8] {
        call("click_at", tile(i)).await;
    }
    call("click_at", verify).await;
    assert!(status().await.contains("grid-passed"), "{}", status().await);

    // Slider CAPTCHA: drag the handle all the way, moving on the way.
    let (sx, sy) = frame_at(&snap, "Slider check");
    call(
        "drag",
        json!({"from_x": sx + 30.0, "from_y": sy + 30.0, "to_x": sx + 305.0, "to_y": sy + 30.0}),
    )
    .await;
    let st = status().await;
    assert!(
        st.contains("slider-passed") && !st.contains("slider-failed"),
        "{st}"
    );

    // Off-screen points are refused, not clicked somewhere else.
    let out = tools::call(
        &state,
        guard.token(),
        "click_at",
        &json!({"x": 5000, "y": 10}),
    )
    .await;
    assert!(
        out.is_error && out.text.contains("off the screen"),
        "{}",
        out.text
    );

    // Typing, then Enter as its own key press.
    let search = ref_of(&snap, "textbox \"Search\"");
    call("type", json!({"ref": search, "text": "otters"})).await;
    call("press_key", json!({"key": "Enter"})).await;
    assert!(
        status().await.contains("searched otters"),
        "{}",
        status().await
    );
    // Typing with submit.
    call(
        "type",
        json!({"ref": search, "text": "beavers", "submit": true}),
    )
    .await;
    assert!(status().await.contains("searched beavers"));

    // Double-click.
    let dbl = ref_of(&snap, "button \"Double me\"");
    call("click", json!({"ref": dbl, "double": true})).await;
    assert!(status().await.contains("doubled"));

    // Hover opens a menu that is hidden otherwise.
    assert!(!snap.contains("Hidden item"));
    let out = call("hover", json!({"ref": ref_of(&snap, "button \"Menu\"")})).await;
    assert!(out.text.contains("Hidden item"), "{}", out.text);

    // Dialogs are answered and reported, and the page carries on.
    let out = call(
        "click",
        json!({"ref": ref_of(&snap, "button \"Alert me\"")}),
    )
    .await;
    assert!(out.text.contains("alert was accepted"), "{}", out.text);
    assert!(status().await.contains("after-alert"));

    // Waiting for text that shows up later.
    call(
        "click",
        json!({"ref": ref_of(&snap, "button \"Load later\"")}),
    )
    .await;
    let out = call("wait", json!({"text": "loaded-late", "seconds": 10})).await;
    assert!(out.text.contains("loaded-late"), "{}", out.text);

    // Shadow DOM and a same-site frame are readable and clickable.
    let out = call(
        "click",
        json!({"ref": ref_of(&snap, "button \"Shadow button\"")}),
    )
    .await;
    assert!(out.text.contains("Shadow clicked"), "{}", out.text);
    let out = call(
        "click",
        json!({"ref": ref_of(&snap, "button \"Inner button\"")}),
    )
    .await;
    assert!(out.text.contains("Inner clicked"), "{}", out.text);

    // Scrolling moves the page.
    call("scroll", json!({"direction": "down", "pages": 3})).await;
    let run = state.browser.running().unwrap();
    let y = run
        .cdp
        .call(
            "Runtime.evaluate",
            json!({"expression": "scrollY", "returnByValue": true}),
            Some(&run.page().session_id),
        )
        .await
        .unwrap()["result"]["value"]
        .as_f64()
        .unwrap();
    assert!(y > 500.0, "scrollY {y}");
    call("scroll", json!({"direction": "up", "pages": 10})).await;

    // A link, then Back.
    let snap = call("snapshot", json!({})).await.text;
    let out = call(
        "click",
        json!({"ref": ref_of(&snap, "link \"About link\"")}),
    )
    .await;
    assert!(out.text.contains("A test shop."), "{}", out.text);
    let out = call("back", json!({})).await;
    assert!(out.text.contains("Basics"), "{}", out.text);

    // A link that opens a new tab is followed; tabs list, switch, close.
    let snap = call("snapshot", json!({})).await.text;
    call(
        "click",
        json!({"ref": ref_of(&snap, "link \"Open popup\"")}),
    )
    .await;
    let mut followed = false;
    for _ in 0..50 {
        let out = call("snapshot", json!({})).await;
        if out.text.contains("A test shop.") {
            followed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(followed, "the new tab was not followed");
    let list = call("tabs", json!({"action": "list"})).await.text;
    let basics: u64 = list
        .lines()
        .find(|l| l.contains("/basics"))
        .and_then(|l| l.split(':').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no basics tab in {list}"));
    let out = call("tabs", json!({"action": "select", "index": basics})).await;
    assert!(out.text.contains("Basics"), "{}", out.text);
    let out = call(
        "tabs",
        json!({"action": "new", "url": format!("http://127.0.0.1:{port}/about")}),
    )
    .await;
    assert!(out.text.contains("A test shop."), "{}", out.text);
    let list = call("tabs", json!({"action": "list"})).await.text;
    let n = list.lines().filter(|l| l.contains(':')).count();
    assert!(n >= 3, "{list}");
    call("tabs", json!({"action": "close", "index": 0})).await;

    state.browser.close().await;
}
