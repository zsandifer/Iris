//! The `browser` MCP tools. Definitions are served by `iris mcp --browser`
//! (launched by the harness CLI); each call is forwarded over the private
//! socket to the server, which carries it out here on Iris's browser.
//!
//! Every action returns plain text the agent can act on (usually a fresh
//! page snapshot with element refs). Saved-login and card fills only ever
//! answer `filled` / `not found` (see `fill.rs`).

use super::cdp::{Cdp, Event};
use super::{Running, SCALE, TurnCtx, VIEW_H};
use crate::state::AppState;
use crate::util::Result;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// MCP server name (tool prefix `mcp__browser__`).
pub const SERVER: &str = "browser";
/// Name used instead when an older install already has a connector called
/// `browser` (that connector keeps working).
pub const SERVER_ALT: &str = "iris_browser";
/// Helpers injected into an isolated world of each page.
const PAGE_JS: &str = include_str!("page.js");
const SNAPSHOT_CHARS: usize = 12_000;
const NAV_TIMEOUT: Duration = Duration::from_secs(20);

/// Result of one tool call.
#[derive(Debug, Default)]
pub struct Output {
    pub text: String,
    /// Base64 JPEG (screenshot).
    pub image: Option<String>,
    pub is_error: bool,
}

impl Output {
    pub fn text(t: impl Into<String>) -> Self {
        Self {
            text: t.into(),
            ..Default::default()
        }
    }
    pub fn error(t: impl Into<String>) -> Self {
        Self {
            text: t.into(),
            is_error: true,
            ..Default::default()
        }
    }

    /// MCP `tools/call` result.
    pub fn to_mcp(&self) -> Value {
        let mut content = vec![json!({"type": "text", "text": self.text})];
        if let Some(img) = &self.image {
            content.push(json!({"type": "image", "data": img, "mimeType": "image/jpeg"}));
        }
        json!({"content": content, "isError": self.is_error})
    }
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    })
}

pub fn definitions() -> Vec<Value> {
    let s = |d: &str| json!({"type": "string", "description": d});
    let r = || s("Element ref from the latest snapshot, e.g. \"e12\"");
    vec![
        tool(
            "navigate",
            "Open a web page in your browser (a real Chromium the user can watch live from \
             their phone). Returns a text snapshot of the page with element refs.",
            json!({"url": s("http(s) URL; a bare domain like example.com works too")}),
            &["url"],
        ),
        tool(
            "snapshot",
            "Read the current page: text, headings, and interactive elements (links, buttons, \
             fields) with refs you can click or type into.",
            json!({}),
            &[],
        ),
        tool(
            "click",
            "Click an element (by ref from the latest snapshot). Returns the updated snapshot.",
            json!({"ref": r()}),
            &["ref"],
        ),
        tool(
            "type",
            "Type text into a field (by ref), replacing what is there. Set `submit` to press \
             Enter afterwards. Never type passwords or card numbers: use fill_login / fill_card.",
            json!({"ref": r(), "text": s("Text to type"),
                   "submit": {"type": "boolean", "description": "Press Enter after typing"}}),
            &["ref", "text"],
        ),
        tool(
            "select_option",
            "Choose an option in a dropdown (by ref), by its visible text or value.",
            json!({"ref": r(), "value": s("Option text or value")}),
            &["ref", "value"],
        ),
        tool(
            "press_key",
            "Press a key on the focused element: Enter, Tab, Escape, Backspace, Delete, \
             ArrowUp/Down/Left/Right, Home, End, PageUp, PageDown, Space, or one character.",
            json!({"key": s("Key name")}),
            &["key"],
        ),
        tool(
            "scroll",
            "Scroll the page (loads lazy content; useful before a screenshot).",
            json!({"direction": {"type": "string", "enum": ["up", "down"]},
                   "pages": {"type": "number", "minimum": 0.1, "maximum": 10,
                             "description": "How far, in screen heights (default 1)"}}),
            &["direction"],
        ),
        tool("back", "Go back to the previous page.", json!({}), &[]),
        tool(
            "screenshot",
            "See the visible part of the page as an image (for layouts, images, or anything \
             the text snapshot misses).",
            json!({}),
            &[],
        ),
        tool(
            "wait",
            "Wait for text to appear on the page, or for a number of seconds (max 30).",
            json!({"text": s("Text to wait for"),
                   "seconds": {"type": "number", "minimum": 0.1, "maximum": 30}}),
            &[],
        ),
        tool(
            "tabs",
            "List, open, switch to, or close tabs.",
            json!({"action": {"type": "string", "enum": ["list", "new", "select", "close"]},
                   "index": {"type": "integer", "minimum": 0, "description": "Tab number from list"},
                   "url": s("URL for a new tab")}),
            &["action"],
        ),
        tool(
            "fill_login",
            "Sign in with the user's saved login for the current page's site. Iris types the \
             username and password into the page itself; you never see them. Answers \
             `filled` or `not found`. Afterwards click the sign-in button (for two-step \
             forms, call it again on the password step).",
            json!({"site": s("Site the login is for, e.g. example.com (default: the current page)")}),
            &[],
        ),
        tool(
            "fill_card",
            "Fill the payment form on the current page with a card from the user's wallet. \
             The user is always asked to approve first (a card in the chat); Iris types the \
             card details itself and you never see them. Answers `filled`, `denied`, \
             `waiting for approval`, or `not found`.",
            json!({"card": s("Which card, by its name or last 4 digits (needed only if several are saved)")}),
            &[],
        ),
    ]
}

/// System prompt section when the browser is attached.
pub fn guide(server: &str) -> String {
    GUIDE.replace("the `browser` tools", &format!("the `{server}` tools"))
}

const GUIDE: &str = "
## Your browser

You have your own web browser (the `browser` tools), which the user can watch
live and take over from their phone. Use it for anything that needs a real
page: signing in, forms, shopping carts, sites that need JavaScript. For
quick reading, ordinary web search or fetch tools are fine.

- `navigate`, then work from the snapshot's refs (`click`, `type`,
  `select_option`, `press_key`); `screenshot` when you need to see the page.
- Signing in: never ask the user for a password and never type one. Call
  `fill_login` on the sign-in page; it answers `filled` or `not found`. If
  `not found`, ask the user to add the login in Logins & wallet, or to take
  over the browser and sign in themselves.
- Paying: call `fill_card` on the payment form. The user approves every use
  in the chat; tell them you are waiting for their OK. Do not place an order
  they have not clearly asked for.
- If a tool says the user has taken over, wait and try again later.
";

// ----------------------------------------------------------------- calls

/// Carry out one tool call for a registered turn.
pub async fn call(state: &AppState, token: &str, name: &str, args: &Value) -> Output {
    let b = &state.browser;
    let Some(ctx) = b.turn(token) else {
        return Output::error(super::TURN_ENDED);
    };
    if !definitions().iter().any(|d| d["name"] == name) {
        return Output::error(format!("unknown tool {name}"));
    }
    if b.is_paused() {
        return Output::error(
            "The owner has paused Iris's computer, so the browser is off. Tell them if you \
             need it.",
        );
    }
    b.mark_used(&ctx);
    // Wait while the owner has control, then hold the page; control is
    // checked again once the page is ours, so a call queued behind another
    // never runs after a take-over.
    let _op = loop {
        if !b.wait_for_agent(&ctx).await {
            return if ctx.is_ended() {
                Output::error(super::TURN_ENDED)
            } else {
                Output::text(super::TAKEN_OVER)
            };
        }
        let guard = tokio::select! {
            g = b.op_lock.lock() => g,
            _ = ctx.wait_ended() => return Output::error(super::TURN_ENDED),
        };
        if b.control() == super::Control::Agent {
            break guard;
        }
    };
    if let Err(e) = b.check_agent(&ctx) {
        return Output::error(e.to_string());
    }
    let run = match b.ensure().await {
        Ok(r) => r,
        Err(e) => return Output::error(format!("The browser could not start: {e}")),
    };
    b.set_busy(true);
    let started = std::time::Instant::now();
    let mut out = match run_tool(state, &ctx, &run, name, args).await {
        Ok(o) => o,
        Err(e) => Output::error(format!("{e}")),
    };
    b.set_busy(false);
    let dialogs = run.take_dialogs();
    if !dialogs.is_empty() {
        out.text = format!("(Dialog: {})\n\n{}", dialogs.join("; "), out.text);
    }
    // A page can echo a filled secret (a dialog, another field, its title):
    // never let it reach the agent.
    out.text = run.redact(&out.text);
    crate::info!(
        "browser: {name} {} in {}ms",
        if out.is_error { "failed" } else { "ok" },
        started.elapsed().as_millis()
    );
    out
}

async fn run_tool(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Arc<Running>,
    name: &str,
    a: &Value,
) -> Result<Output> {
    let cdp = &run.cdp;
    let page = run.page();
    let s = Some(page.session_id.as_str());
    let arg = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("");
    let b = &state.browser;
    match name {
        "navigate" => {
            let url = normalize_url(arg("url"))?;
            let events = cdp.subscribe();
            let r = cdp.call("Page.navigate", json!({"url": url}), s).await?;
            if let Some(err) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
                return Ok(Output::error(format!("Could not open {url}: {err}")));
            }
            if r["loaderId"].is_string() {
                wait_load(events, &page.session_id, NAV_TIMEOUT).await;
            }
            snapshot(run).await
        }
        "snapshot" => snapshot(run).await,
        "click" => {
            let p = eval(run, &format!("point({})", json!(arg("ref")))).await?;
            if let Some(e) = p["error"].as_str() {
                return Ok(Output::error(stale_message(e)));
            }
            let (x, y) = (
                p["x"].as_f64().unwrap_or(0.0),
                p["y"].as_f64().unwrap_or(0.0),
            );
            let events = cdp.subscribe();
            b.check_agent(ctx)?;
            click_at(cdp, s, x, y).await?;
            settle(events, &page, Duration::from_millis(600)).await;
            let mut out = snapshot(run).await?;
            if let Some(c) = p["covered"].as_str() {
                out.text = format!(
                    "(Note: the click landed on an element on top of it: \"{c}\".)\n\n{}",
                    out.text
                );
            }
            Ok(out)
        }
        "type" => {
            let text = a.get("text").and_then(Value::as_str).unwrap_or("");
            if text.chars().count() > 5000 {
                return Ok(Output::error(
                    "That text is too long to type (5000 characters max).",
                ));
            }
            let f = eval(run, &format!("focus({})", json!(arg("ref")))).await?;
            if let Some(e) = f["error"].as_str() {
                return Ok(Output::error(if e == "select" {
                    "That is a dropdown; use select_option.".into()
                } else {
                    stale_message(e)
                }));
            }
            if f["ok"] != true {
                return Ok(Output::error(
                    "That field did not take the focus (something else on the page did); take \
                     a new snapshot.",
                ));
            }
            b.check_agent(ctx)?;
            cdp.call("Input.insertText", json!({"text": text}), s)
                .await?;
            if a["submit"].as_bool().unwrap_or(false) {
                let events = cdp.subscribe();
                press_key(cdp, s, "Enter").await?;
                settle(events, &page, Duration::from_millis(800)).await;
                return snapshot(run).await;
            }
            Ok(Output::text("Typed it."))
        }
        "select_option" => {
            let r = eval(
                run,
                &format!("choose({}, {})", json!(arg("ref")), json!(arg("value"))),
            )
            .await?;
            match r["error"].as_str() {
                Some("no such option") => Ok(Output::error(
                    "No option matches that; check the options in the snapshot.",
                )),
                Some("not a select") => Ok(Output::error(
                    "That is not a dropdown; click it and pick from what appears instead.",
                )),
                Some(e) => Ok(Output::error(stale_message(e))),
                None => Ok(Output::text(format!(
                    "Selected \"{}\".",
                    r["chosen"].as_str().unwrap_or("")
                ))),
            }
        }
        "press_key" => {
            let events = cdp.subscribe();
            press_key(cdp, s, arg("key")).await?;
            settle(events, &page, Duration::from_millis(500)).await;
            snapshot(run).await
        }
        "scroll" => {
            let pages = a["pages"].as_f64().unwrap_or(1.0).clamp(0.1, 10.0);
            let dy =
                pages * VIEW_H as f64 * 0.8 * if arg("direction") == "up" { -1.0 } else { 1.0 };
            cdp.call(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseWheel", "x": 200, "y": 400, "deltaX": 0, "deltaY": dy}),
                s,
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            Ok(Output::text(format!(
                "Scrolled {}.",
                if dy < 0.0 { "up" } else { "down" }
            )))
        }
        "back" => {
            let events = cdp.subscribe();
            if !history_step(cdp, s, -1).await? {
                return Ok(Output::error("There is no previous page."));
            }
            wait_load(events, &page.session_id, Duration::from_secs(10)).await;
            snapshot(run).await
        }
        "screenshot" => {
            // Text can be redacted, pixels cannot: no picture while the page
            // shows a secret Iris filled (outside the masked fields).
            let shown = eval(run, "pageText()").await?;
            if run.shows_secret(shown.as_str().unwrap_or("")) {
                return Ok(Output::error(
                    "No screenshot right now: this page shows saved sign-in or card details. \
                     Use snapshot instead.",
                ));
            }
            let m = cdp.call("Page.getLayoutMetrics", json!({}), s).await?;
            let vv = &m["cssVisualViewport"];
            let clip = json!({
                "x": vv["pageX"].as_f64().unwrap_or(0.0),
                "y": vv["pageY"].as_f64().unwrap_or(0.0),
                "width": vv["clientWidth"].as_f64().unwrap_or(super::VIEW_W as f64),
                "height": vv["clientHeight"].as_f64().unwrap_or(VIEW_H as f64),
                "scale": 1.0 / SCALE,
            });
            let shot = cdp
                .call(
                    "Page.captureScreenshot",
                    json!({"format": "jpeg", "quality": 70, "clip": clip}),
                    s,
                )
                .await?;
            let info = eval(run, "info()").await.unwrap_or_default();
            Ok(Output {
                text: format!(
                    "Screenshot of {} ({}).",
                    info["url"].as_str().unwrap_or(""),
                    info["title"].as_str().unwrap_or("")
                ),
                image: shot["data"].as_str().map(String::from),
                is_error: false,
            })
        }
        "wait" => {
            let secs = a["seconds"].as_f64().unwrap_or(0.0).clamp(0.0, 30.0);
            let text = arg("text");
            if text.is_empty() {
                let secs = if secs == 0.0 { 1.0 } else { secs };
                tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                return Ok(Output::text(format!("Waited {secs} seconds.")));
            }
            let deadline = std::time::Instant::now()
                + Duration::from_secs_f64(if secs == 0.0 { 10.0 } else { secs });
            loop {
                let found = eval(run, &format!("hasText({})", json!(text)))
                    .await
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if found {
                    return snapshot(run).await;
                }
                if std::time::Instant::now() >= deadline {
                    return Ok(Output::text(format!("\"{text}\" did not appear.")));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        "tabs" => tabs(state, run, a).await,
        "fill_login" => {
            super::fill::login(state, ctx, run, a.get("site").and_then(Value::as_str)).await
        }
        "fill_card" => {
            super::fill::card(state, ctx, run, a.get("card").and_then(Value::as_str)).await
        }
        other => Ok(Output::error(format!("unknown tool {other}"))),
    }
}

fn stale_message(e: &str) -> String {
    match e {
        "hidden" => "That element is not visible right now; take a new snapshot.".into(),
        _ => "That ref is not on the page anymore; take a new snapshot.".into(),
    }
}

async fn tabs(state: &AppState, run: &Arc<Running>, a: &Value) -> Result<Output> {
    let cdp = &run.cdp;
    let list = |v: &Value| -> Vec<(String, String, String)> {
        v["targetInfos"]
            .as_array()
            .map(|t| {
                t.iter()
                    .filter(|t| t["type"] == "page")
                    .map(|t| {
                        (
                            t["targetId"].as_str().unwrap_or("").to_string(),
                            t["title"].as_str().unwrap_or("").to_string(),
                            t["url"].as_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let pages = list(&cdp.call("Target.getTargets", json!({}), None).await?);
    let index = a["index"].as_u64().map(|i| i as usize);
    let pick = |i: Option<usize>| -> Result<String> {
        i.and_then(|i| pages.get(i))
            .map(|p| p.0.clone())
            .ok_or_else(|| "no tab with that number; list the tabs first".into())
    };
    match a["action"].as_str().unwrap_or("list") {
        "new" => {
            let url = match a["url"].as_str().filter(|u| !u.is_empty()) {
                Some(u) => normalize_url(u)?,
                None => "about:blank".into(),
            };
            let t = cdp
                .call("Target.createTarget", json!({"url": url}), None)
                .await?;
            let id = t["targetId"].as_str().unwrap_or_default().to_string();
            state.browser.attach(run, &id).await?;
            tokio::time::sleep(Duration::from_millis(800)).await;
            snapshot(run).await
        }
        "select" => {
            let id = pick(index)?;
            state.browser.attach(run, &id).await?;
            snapshot(run).await
        }
        "close" => {
            let id = pick(index)?;
            cdp.call("Target.closeTarget", json!({"targetId": id}), None)
                .await?;
            if id == run.page().target_id {
                state.browser.switch_to_any(run).await?;
            }
            Ok(Output::text("Closed the tab."))
        }
        _ => {
            let current = run.page().target_id;
            let lines: Vec<String> = pages
                .iter()
                .enumerate()
                .map(|(i, (id, title, url))| {
                    format!(
                        "{i}: {title} - {url}{}",
                        if *id == current { " (current)" } else { "" }
                    )
                })
                .collect();
            Ok(Output::text(lines.join("\n")))
        }
    }
}

// --------------------------------------------------------------- helpers

/// Only web pages: no `javascript:`, `file:`, `chrome:` or data URLs.
pub fn normalize_url(raw: &str) -> Result<String> {
    let u = raw.trim();
    if u.is_empty() || u.len() > 4000 || u.chars().any(char::is_whitespace) {
        return Err("give a web address like https://example.com".into());
    }
    let lower = u.to_ascii_lowercase();
    if lower == "about:blank" {
        return Ok(lower);
    }
    if let Some((scheme, _)) = lower.split_once("://") {
        return if scheme == "http" || scheme == "https" {
            Ok(u.to_string())
        } else {
            Err("only http and https pages can be opened".into())
        };
    }
    // "host:port/..." is fine; any other "scheme:" (javascript:, data:,
    // view-source:, ...) is not.
    if let Some((before, after)) = lower.split_once(':') {
        let port = after.starts_with(|c: char| c.is_ascii_digit()) && !before.contains('/');
        if !port {
            return Err("only http and https pages can be opened".into());
        }
    }
    Ok(format!("https://{u}"))
}

/// Evaluate `__iris.<call>` in Iris's isolated world of the current page.
pub async fn eval(run: &Running, call: &str) -> Result<Value> {
    let page = run.page();
    let s = Some(page.session_id.as_str());
    let world = run
        .cdp
        .call(
            "Page.createIsolatedWorld",
            json!({"frameId": page.target_id, "worldName": "iris"}),
            s,
        )
        .await?;
    let ctx = world["executionContextId"]
        .as_i64()
        .ok_or("the page is not ready")?;
    let r = run
        .cdp
        .call(
            "Runtime.evaluate",
            json!({"expression": format!("{PAGE_JS}\n__iris.{call}"), "contextId": ctx,
                   "returnByValue": true}),
            s,
        )
        .await?;
    if let Some(ex) = r.get("exceptionDetails") {
        let msg = ex["exception"]["description"]
            .as_str()
            .or(ex["text"].as_str())
            .unwrap_or("script error");
        return Err(format!(
            "page script failed: {}",
            crate::util::truncate_chars(msg, 200)
        )
        .into());
    }
    Ok(r["result"]["value"].clone())
}

pub async fn snapshot(run: &Running) -> Result<Output> {
    let v = eval(run, &format!("snapshot({SNAPSHOT_CHARS})")).await?;
    let mut text = format!(
        "Page: {}\nURL: {}\n\n{}",
        v["title"].as_str().unwrap_or(""),
        v["url"].as_str().unwrap_or(""),
        v["text"].as_str().unwrap_or("")
    );
    if v["text"].as_str().is_none_or(str::is_empty) {
        text.push_str("(The page is empty.)");
    }
    if v["truncated"].as_bool().unwrap_or(false) {
        text.push_str(
            "\n\n(Snapshot cut short; the page is long. Use `wait` with text, scroll, or a \
             screenshot to find more.)",
        );
    }
    Ok(Output::text(text))
}

pub async fn click_at(cdp: &Cdp, s: Option<&str>, x: f64, y: f64) -> Result<()> {
    cdp.call(
        "Input.dispatchMouseEvent",
        json!({"type": "mouseMoved", "x": x, "y": y}),
        s,
    )
    .await?;
    for t in ["mousePressed", "mouseReleased"] {
        cdp.call(
            "Input.dispatchMouseEvent",
            json!({"type": t, "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1}),
            s,
        )
        .await?;
    }
    Ok(())
}

/// (key, code, Windows virtual key code, text) for named keys.
fn key_info(key: &str) -> Option<(&'static str, &'static str, i64, &'static str)> {
    Some(match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => ("Enter", "Enter", 13, "\r"),
        "tab" => ("Tab", "Tab", 9, ""),
        "escape" | "esc" => ("Escape", "Escape", 27, ""),
        "backspace" => ("Backspace", "Backspace", 8, ""),
        "delete" => ("Delete", "Delete", 46, ""),
        "arrowup" | "up" => ("ArrowUp", "ArrowUp", 38, ""),
        "arrowdown" | "down" => ("ArrowDown", "ArrowDown", 40, ""),
        "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", 37, ""),
        "arrowright" | "right" => ("ArrowRight", "ArrowRight", 39, ""),
        "home" => ("Home", "Home", 36, ""),
        "end" => ("End", "End", 35, ""),
        "pageup" => ("PageUp", "PageUp", 33, ""),
        "pagedown" => ("PageDown", "PageDown", 34, ""),
        "space" | " " => (" ", "Space", 32, " "),
        _ => return None,
    })
}

pub async fn press_key(cdp: &Cdp, s: Option<&str>, key: &str) -> Result<()> {
    if let Some((k, code, vk, text)) = key_info(key) {
        let mut down = json!({"type": if text.is_empty() { "rawKeyDown" } else { "keyDown" },
            "key": k, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk});
        if !text.is_empty() {
            down["text"] = text.into();
            down["unmodifiedText"] = text.into();
        }
        cdp.call("Input.dispatchKeyEvent", down, s).await?;
        cdp.call(
            "Input.dispatchKeyEvent",
            json!({"type": "keyUp", "key": k, "code": code, "windowsVirtualKeyCode": vk,
                   "nativeVirtualKeyCode": vk}),
            s,
        )
        .await?;
        return Ok(());
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => {
            let t = c.to_string();
            cdp.call(
                "Input.dispatchKeyEvent",
                json!({"type": "keyDown", "key": t, "text": t, "unmodifiedText": t}),
                s,
            )
            .await?;
            cdp.call(
                "Input.dispatchKeyEvent",
                json!({"type": "keyUp", "key": t}),
                s,
            )
            .await?;
            Ok(())
        }
        _ => Err(format!("unknown key {key}").into()),
    }
}

/// Go back (-1) or forward (+1). False when there is nowhere to go.
pub async fn history_step(cdp: &Cdp, s: Option<&str>, delta: i64) -> Result<bool> {
    let h = cdp.call("Page.getNavigationHistory", json!({}), s).await?;
    let idx = h["currentIndex"].as_i64().unwrap_or(0) + delta;
    let Some(entry) = h["entries"]
        .as_array()
        .and_then(|e| usize::try_from(idx).ok().and_then(|i| e.get(i)))
    else {
        return Ok(false);
    };
    cdp.call(
        "Page.navigateToHistoryEntry",
        json!({"entryId": entry["id"]}),
        s,
    )
    .await?;
    Ok(true)
}

/// Wait for the page's load event (bounded).
async fn wait_load(mut events: broadcast::Receiver<Arc<Event>>, session: &str, timeout: Duration) {
    let _ = tokio::time::timeout(timeout, async {
        loop {
            match events.recv().await {
                Ok(e)
                    if e.session_id.as_deref() == Some(session)
                        && e.method == "Page.loadEventFired" =>
                {
                    break;
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
}

/// After an action: if it started a navigation within `window`, wait for
/// the new page to load; otherwise give the page a moment to react.
async fn settle(
    mut events: broadcast::Receiver<Arc<Event>>,
    page: &super::PageRef,
    window: Duration,
) {
    let started = tokio::time::timeout(window, async {
        loop {
            match events.recv().await {
                Ok(e)
                    if e.session_id.as_deref() == Some(page.session_id.as_str())
                        && (e.method == "Page.frameStartedLoading"
                            || e.method == "Page.frameRequestedNavigation")
                        && e.params["frameId"].as_str() == Some(page.target_id.as_str()) =>
                {
                    return true;
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    if started {
        wait_load(events, &page.session_id, NAV_TIMEOUT).await;
    } else {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn test_state(tmp: &std::path::Path) -> AppState {
        let paths = crate::config::Paths {
            config_file: tmp.join("config.toml"),
            data_dir: tmp.join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = crate::config::Config::default();
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        AppState::new(cfg, paths, db).unwrap()
    }

    #[tokio::test]
    async fn queued_calls_respect_take_over_and_turn_end() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let b = state.browser.clone();
        let g = b.begin_turn("u", "c", "m", &[]);
        let token = g.token().to_string();
        // Another call holds the page.
        let held = b.op_lock.lock().await;
        let (st, tk) = (state.clone(), token.clone());
        let queued =
            tokio::spawn(
                async move { call(&st, &tk, "navigate", &json!({"url": "a.test"})).await },
            );
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The owner takes over while the call waits for the page.
        b.control.send_replace(super::super::Control::User {
            user_id: "u".into(),
            device_id: "d".into(),
        });
        drop(held);
        tokio::time::sleep(Duration::from_millis(200)).await;
        // It went back to waiting instead of running under the owner.
        assert!(!queued.is_finished());
        // The turn ends: the call gives up at once, having done nothing.
        drop(g);
        let out = tokio::time::timeout(Duration::from_secs(2), queued)
            .await
            .unwrap()
            .unwrap();
        assert!(
            out.is_error && out.text == super::super::TURN_ENDED,
            "{}",
            out.text
        );
        assert!(b.running().is_none());
        // Unknown and expired tokens are refused.
        assert!(call(&state, &token, "snapshot", &json!({})).await.is_error);
        assert!(call(&state, "", "snapshot", &json!({})).await.is_error);
    }

    #[test]
    fn urls_are_web_pages_only() {
        assert_eq!(normalize_url("example.com").unwrap(), "https://example.com");
        assert_eq!(
            normalize_url(" http://127.0.0.1:8080/a?b=1 ").unwrap(),
            "http://127.0.0.1:8080/a?b=1"
        );
        assert_eq!(
            normalize_url("localhost:3000/x").unwrap(),
            "https://localhost:3000/x"
        );
        for bad in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "file:///etc/passwd",
            "chrome://settings",
            "data:text/html,hi",
            "view-source:https://x.com",
            "",
            "a b.com",
            "ftp://x.com",
        ] {
            assert!(normalize_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tool_list_and_results() {
        let names: Vec<String> = definitions()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        for n in [
            "navigate",
            "snapshot",
            "click",
            "type",
            "fill_login",
            "fill_card",
        ] {
            assert!(names.contains(&n.to_string()));
        }
        let o = Output {
            text: "shot".into(),
            image: Some("AAAA".into()),
            is_error: false,
        };
        let v = o.to_mcp();
        assert_eq!(v["content"][1]["type"], "image");
        assert_eq!(Output::error("x").to_mcp()["isError"], true);
        assert!(key_info("Enter").is_some() && key_info("Shift+Q").is_none());
    }
}
