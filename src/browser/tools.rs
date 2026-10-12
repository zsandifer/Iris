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
pub(crate) const TAINTED_PAGE: &str =
    "This page has your password filled in. Submit or leave the page first.";
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
            json!({"ref": r(), "double": {"type": "boolean", "description": "Double-click"}}),
            &["ref"],
        ),
        tool(
            "click_at",
            "Click a point on the latest screenshot (x, y in its pixels). For what the snapshot \
             cannot reach: CAPTCHA checkboxes and picture grids, frames from other sites, maps, \
             canvases. Take a new screenshot afterwards to see the result.",
            json!({"x": {"type": "number", "minimum": 0}, "y": {"type": "number", "minimum": 0},
                   "double": {"type": "boolean", "description": "Double-click"}}),
            &["x", "y"],
        ),
        tool(
            "drag",
            "Drag from one point on the latest screenshot to another (pixels), like a finger: \
             sliders, slide-to-verify puzzles, reordering.",
            json!({"from_x": {"type": "number", "minimum": 0}, "from_y": {"type": "number", "minimum": 0},
                   "to_x": {"type": "number", "minimum": 0}, "to_y": {"type": "number", "minimum": 0}}),
            &["from_x", "from_y", "to_x", "to_y"],
        ),
        tool(
            "hover",
            "Move the mouse over an element (by ref), to open hover menus or tooltips. Returns \
             the updated snapshot.",
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
             `filled` or `not found`. Set `submit` to press Enter in the filled field \
             afterwards (best: the page cannot be read until the sign-in moves on). For \
             two-step forms, call it again on the password step.",
            json!({"site": s("Site the login is for, e.g. example.com (default: the current page)"),
                   "submit": {"type": "boolean", "description": "Press Enter after filling to sign in"}}),
            &[],
        ),
        tool(
            "fill_card",
            "Fill the payment form on the current page with a card from the user's wallet. \
             The user is always asked to approve first (a card in the chat); Iris types the \
             card details itself and you never see them. Answers `filled`, `denied`, \
             `not filled` (expired or cancelled), or `not found`.",
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
  `fill_login` with `submit: true` on the sign-in page. Until the sign-in
  moves to a new page you cannot read the page; if it asks for a password
  next, call `fill_login` again; for codes or checks, ask the user to take
  over. If it answers `not found`, ask the user to add the login in Logins & wallet, or to take
  over the browser and sign in themselves.
- Paying: call `fill_card` on the payment form. The user approves every use
  in the chat; tell them you are waiting for their OK. Do not place an order
  they have not clearly asked for.
- Things the snapshot shows as a frame from another site (CAPTCHAs, embedded
  widgets), maps, and canvases: `screenshot`, then `click_at` or `drag` on
  what you see, and `screenshot` again to check.
- CAPTCHAs: you may solve them for the user. Click the \"I'm not a robot\"
  box; for picture puzzles pick the tiles, then the verify button; drag
  sliders. If a new puzzle keeps coming after two or three tries, ask the
  user to take over.
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
    if !access_allowed(state, &ctx).await {
        return Output::error(super::TURN_ENDED);
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
    // Starting is cancelled too if the turn ends meanwhile.
    let started = tokio::select! {
        r = b.ensure() => r,
        _ = ctx.wait_ended() => return Output::error(super::TURN_ENDED),
    };
    let run = match started {
        Ok(r) => r,
        Err(e) => return Output::error(format!("The browser could not start: {e}")),
    };
    // Starting can take a while: the turn may have ended meanwhile.
    if let Err(e) = b.check_agent(&ctx) {
        return Output::error(e.to_string());
    }

    b.set_busy(true);
    let started = std::time::Instant::now();
    // Takeover invalidates admitted calls too, including captured pixels
    // and polling reads. A hand-back cannot revive an old operation.
    let result = while_agent(b, &ctx, run_tool(state, &ctx, &run, name, args)).await;
    let mut out = match result {
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

/// Cancel an admitted operation on a control change and check once more
/// immediately before releasing its result, even if it completed first.
async fn while_agent<T>(
    b: &super::Browser,
    ctx: &TurnCtx,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let mut control = b.control.subscribe();
    b.check_agent(ctx)?;
    let result = tokio::select! {
        biased;
        _ = ctx.wait_ended() => Err(super::TURN_ENDED.into()),
        _ = control.changed() => Err(super::TAKEN_OVER.into()),
        r = work => r,
    };
    b.check_agent(ctx)?;
    if control.has_changed().unwrap_or(true) {
        return Err(super::TAKEN_OVER.into());
    }
    result
}

/// The managed profile is private to the owner, including its cookies.
pub(crate) async fn access_allowed(state: &AppState, ctx: &TurnCtx) -> bool {
    if ctx.is_ended() || state.is_paused().await || !state.browser.enabled() {
        return false;
    }
    let (user, conv) = (ctx.user_id.clone(), ctx.conversation_id.clone());
    state
        .db
        .call(move |c| {
            if crate::db::owner_user_id(c)?.as_deref() != Some(user.as_str()) {
                return Ok(false);
            }
            let Some(_chat) = crate::db::get_conversation(c, &user, &conv)? else {
                return Ok(false);
            };
            Ok(crate::db::conversation_member_ids(c, &conv)? == [user])
        })
        .await
        .unwrap_or(false)
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
            b.check_agent(ctx)?;
            let before = eval(run, "navigationState()").await?;
            let r = cdp.call("Page.navigate", json!({"url": url}), s).await?;
            if let Some(err) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
                return Ok(Output::error(format!("Could not open {url}: {err}")));
            }
            if r["loaderId"].is_string() {
                wait_load(events, &page.session_id, NAV_TIMEOUT).await;
            }
            release_after_action(run, &page, &before).await?;
            snapshot(run).await
        }
        "snapshot" => snapshot(run).await,
        "click" => {
            let at = format!("point({})", json!(arg("ref")));
            let mut p = eval_mut(run, b, ctx, &at).await?;
            if let Some(e) = p["error"].as_str() {
                return Ok(Output::error(stale_message(e)));
            }
            // Moving the mouse can change the page (a hover menu closing
            // shifts what is below it): move first, then aim again.
            b.check_agent(ctx)?;
            cdp.call(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseMoved", "x": p["x"], "y": p["y"]}),
                s,
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            p = eval_mut(run, b, ctx, &at).await?;
            if let Some(e) = p["error"].as_str() {
                return Ok(Output::error(stale_message(e)));
            }
            let (x, y) = (
                p["x"].as_f64().unwrap_or(0.0),
                p["y"].as_f64().unwrap_or(0.0),
            );
            let before = eval(run, "navigationState()").await?;
            let events = cdp.subscribe();
            let clicks = if a["double"].as_bool().unwrap_or(false) {
                2
            } else {
                1
            };
            click_n(cdp, s, x, y, clicks, &|| b.check_agent(ctx)).await?;
            settle(events, &page, Duration::from_millis(600)).await;
            release_after_action(run, &page, &before).await?;
            let mut out = snapshot_after(run).await?;
            if let Some(c) = p["covered"].as_str() {
                out.text = format!(
                    "(Note: the click landed on an element on top of it: \"{c}\".)\n\n{}",
                    out.text
                );
            }
            Ok(out)
        }
        "click_at" | "drag" | "hover" => {
            let num = |k: &str| a[k].as_f64().filter(|v| v.is_finite() && *v >= 0.0);
            let guard = || b.check_agent(ctx);
            let before = eval(run, "navigationState()").await?;
            let events = cdp.subscribe();
            if name == "hover" {
                let p = eval_mut(run, b, ctx, &format!("point({})", json!(arg("ref")))).await?;
                if let Some(e) = p["error"].as_str() {
                    return Ok(Output::error(stale_message(e)));
                }
                let (x, y) = (
                    p["x"].as_f64().unwrap_or(0.0),
                    p["y"].as_f64().unwrap_or(0.0),
                );
                guard()?;
                cdp.call(
                    "Input.dispatchMouseEvent",
                    json!({"type": "mouseMoved", "x": x, "y": y}),
                    s,
                )
                .await?;
                tokio::time::sleep(Duration::from_millis(300)).await;
                return snapshot_after(run).await;
            }
            let (Some(x), Some(y)) = (
                num(if name == "drag" { "from_x" } else { "x" }),
                num(if name == "drag" { "from_y" } else { "y" }),
            ) else {
                return Ok(Output::error(
                    "Give the point as x and y in screenshot pixels.",
                ));
            };
            let vv = &cdp.call("Page.getLayoutMetrics", json!({}), s).await?["cssVisualViewport"];
            let (w, h) = (
                vv["clientWidth"].as_f64().unwrap_or(super::VIEW_W as f64),
                vv["clientHeight"].as_f64().unwrap_or(VIEW_H as f64),
            );
            let inside = |x: f64, y: f64| x <= w && y <= h;
            if name == "drag" {
                let (Some(tx), Some(ty)) = (num("to_x"), num("to_y")) else {
                    return Ok(Output::error("Give the end point as to_x and to_y."));
                };
                if !inside(x, y) || !inside(tx, ty) {
                    return Ok(Output::error(format!(
                        "That is off the screen (it is {w:.0} by {h:.0}); take a new screenshot."
                    )));
                }
                drag(cdp, s, (x, y), (tx, ty), &guard).await?;
            } else {
                if !inside(x, y) {
                    return Ok(Output::error(format!(
                        "That is off the screen (it is {w:.0} by {h:.0}); take a new screenshot."
                    )));
                }
                let clicks = if a["double"].as_bool().unwrap_or(false) {
                    2
                } else {
                    1
                };
                click_n(cdp, s, x, y, clicks, &guard).await?;
            }
            settle(events, &page, Duration::from_millis(600)).await;
            release_after_action(run, &page, &before).await?;
            Ok(Output::text(if name == "drag" {
                "Dragged. Take a screenshot to see the result."
            } else {
                "Clicked. Take a screenshot to see the result."
            }))
        }
        "type" => {
            let text = a.get("text").and_then(Value::as_str).unwrap_or("");
            if text.chars().count() > 5000 {
                return Ok(Output::error(
                    "That text is too long to type (5000 characters max).",
                ));
            }
            // Inputs and text areas get the text on that exact element in
            // one step; rich editors are focused and typed into.
            let f = eval_mut(
                run,
                b,
                ctx,
                &format!("setText({}, {})", json!(arg("ref")), json!(text)),
            )
            .await?;
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
            if f["set"] != true {
                b.check_agent(ctx)?;
                cdp.call("Input.insertText", json!({"text": text}), s)
                    .await?;
            }
            if a["submit"].as_bool().unwrap_or(false) {
                // Enter goes to whatever has focus: only if it is still
                // the field that was typed into.
                let f = eval_mut(run, b, ctx, &format!("hasFocus({})", json!(arg("ref")))).await?;
                if f["ok"] != true {
                    return Ok(Output::error(
                        "Typed it, but the page moved the focus elsewhere, so I did not press \
                         Enter. Take a new snapshot and click the submit button instead.",
                    ));
                }
                let before = eval(run, "navigationState()").await?;
                let events = cdp.subscribe();
                press_key(cdp, s, "Enter", &|| b.check_agent(ctx)).await?;
                settle(events, &page, Duration::from_millis(800)).await;
                release_after_action(run, &page, &before).await?;
                return snapshot_after(run).await;
            }
            Ok(Output::text("Typed it."))
        }
        "select_option" => {
            let r = eval_mut(
                run,
                b,
                ctx,
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
                None if r["chosen"] == "" => Ok(Output::text("Selected it.")),
                None => Ok(Output::text(format!(
                    "Selected \"{}\".",
                    r["chosen"].as_str().unwrap_or("")
                ))),
            }
        }
        "press_key" => {
            let before = eval(run, "navigationState()").await?;
            let events = cdp.subscribe();
            press_key(cdp, s, arg("key"), &|| b.check_agent(ctx)).await?;
            settle(events, &page, Duration::from_millis(500)).await;
            if arg("key") == "Enter" {
                release_after_action(run, &page, &before).await?;
            }
            snapshot_after(run).await
        }
        "scroll" => {
            let pages = a["pages"].as_f64().unwrap_or(1.0).clamp(0.1, 10.0);
            let dy =
                pages * VIEW_H as f64 * 0.8 * if arg("direction") == "up" { -1.0 } else { 1.0 };
            b.check_agent(ctx)?;
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
            if !history_step(cdp, s, -1, || b.check_agent(ctx)).await? {
                return Ok(Output::error("There is no previous page."));
            }
            wait_load(events, &page.session_id, Duration::from_secs(10)).await;
            snapshot(run).await
        }
        "screenshot" => {
            // Text can be redacted, pixels cannot: check the page (and every
            // frame in it) before, and make sure it is still the same page
            // after, or the picture is thrown away.
            let before = eval(run, "info()").await?;
            let safe = screenshot_safe(b, run, &before).await?;
            if let Some(why) = safe {
                return Ok(Output::error(why));
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
            let after = eval(run, "info()").await?;
            let same = after["doc"] == before["doc"] && run.page().target_id == page.target_id;
            if !same || screenshot_safe(b, run, &after).await?.is_some() {
                return Ok(Output::error(
                    "The page changed while taking the screenshot; try again.",
                ));
            }
            Ok(Output {
                text: format!(
                    "Screenshot of {} ({}).",
                    after["url"].as_str().unwrap_or(""),
                    after["title"].as_str().unwrap_or("")
                ),
                image: shot["data"].as_str().map(String::from),
                is_error: false,
            })
        }
        "wait" => {
            let secs = a["seconds"].as_f64().unwrap_or(0.0).clamp(0.0, 30.0);
            let text = arg("text");
            if text.is_empty() {
                if eval(run, "info()").await?["tainted"] == true {
                    return Err(TAINTED_PAGE.into());
                }
                let secs = if secs == 0.0 { 1.0 } else { secs };
                tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                return Ok(Output::text(format!("Waited {secs} seconds.")));
            }
            let deadline = std::time::Instant::now()
                + Duration::from_secs_f64(if secs == 0.0 { 10.0 } else { secs });
            loop {
                b.check_agent(ctx)?;
                let found = eval(run, &format!("hasText({})", json!(text)))
                    .await?
                    .as_bool()
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
        "tabs" => tabs(state, ctx, run, a).await,
        "fill_login" => {
            let out =
                super::fill::login(state, ctx, run, a.get("site").and_then(Value::as_str)).await?;
            if out.text != "filled" || !a["submit"].as_bool().unwrap_or(false) {
                return Ok(out);
            }
            // Enter goes to whatever has focus: only if it is still a
            // field that was just filled (the last one filled keeps it).
            let fields = eval(run, "loginFields()").await?;
            let mut focused = false;
            for r in [&fields["pass"], &fields["user"]]
                .iter()
                .filter_map(|v| v.as_str())
            {
                let f = eval_mut(run, b, ctx, &format!("hasFocus({})", json!(r))).await?;
                focused |= f["ok"] == true;
            }
            if !focused {
                return Ok(Output::text(
                    "filled, but the page moved the focus elsewhere, so I did not press Enter. \
                     Ask the user to take over and tap the sign-in button.",
                ));
            }
            let before = eval(run, "navigationState()").await?;
            let events = cdp.subscribe();
            press_key(cdp, s, "Enter", &|| b.check_agent(ctx)).await?;
            settle(events, &page, Duration::from_millis(1500)).await;
            release_after_action(run, &page, &before).await?;
            let mut out = snapshot_after(run).await?;
            out.text = format!("filled and submitted.\n\n{}", out.text);
            Ok(out)
        }
        "fill_card" => {
            super::fill::card(state, ctx, run, a.get("card").and_then(Value::as_str)).await
        }
        other => Ok(Output::error(format!("unknown tool {other}"))),
    }
}

async fn release_after_action(run: &Running, page: &super::PageRef, before: &Value) -> Result<()> {
    if run.page().target_id != page.target_id {
        return Ok(());
    }
    let after = eval(run, "navigationState()").await?;
    if let (Some(before), Some(doc), Some(path)) = (
        before["doc"].as_str(),
        after["doc"].as_str(),
        after["path"].as_str(),
    ) {
        run.agent_navigated(&page.target_id, before, doc, path);
    }
    Ok(())
}

/// Why a screenshot of the current page must not be taken, if so: the page
/// or any frame in it belongs to a site Iris filled a secret into, Iris
/// filled one into this very document, or the page shows a secret.
async fn screenshot_safe(
    b: &super::Browser,
    run: &Running,
    info: &Value,
) -> Result<Option<&'static str>> {
    const TAINTED: &str = "No screenshots of this page: it shows a site where Iris has filled saved \
        sign-in or card details. Use snapshot instead.";
    if info["tainted"] == true || run.has_secret_doc(info["doc"].as_str().unwrap_or("")) {
        return Ok(Some(TAINTED_PAGE));
    }
    let page = run.page();
    let tree = run
        .cdp
        .call("Page.getFrameTree", json!({}), Some(&page.session_id))
        .await?;
    let mut origins = Vec::new();
    collect_origins(&tree["frameTree"], &mut origins);
    origins.push(info["origin"].as_str().unwrap_or("").to_string());
    // Frames from other sites may run in their own process (and target).
    if let Ok(t) = run.cdp.call("Target.getTargets", json!({}), None).await {
        for f in t["targetInfos"].as_array().into_iter().flatten() {
            if f["type"] == "iframe"
                && let Some(o) = f["url"].as_str().and_then(url_origin)
            {
                origins.push(o);
            }
        }
    }
    if origins.iter().any(|o| b.is_tainted(o)) {
        return Ok(Some(TAINTED));
    }
    let shown = eval(run, "pageText()").await?;
    if run.shows_secret(shown.as_str().unwrap_or("")) {
        return Ok(Some(
            "No screenshot right now: this page shows saved sign-in or card details. Use \
             snapshot instead.",
        ));
    }
    Ok(None)
}

/// `scheme://host[:port]` of an http(s) URL, as browsers write origins.
fn url_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let hostport = authority.rsplit('@').next()?.to_ascii_lowercase();
    let default = if scheme == "https" { ":443" } else { ":80" };
    let hostport = hostport.strip_suffix(default).unwrap_or(&hostport);
    Some(format!("{scheme}://{hostport}"))
}

fn collect_origins(node: &Value, out: &mut Vec<String>) {
    if let Some(o) = node["frame"]["securityOrigin"].as_str() {
        out.push(o.to_string());
    }
    for c in node["childFrames"].as_array().into_iter().flatten() {
        collect_origins(c, out);
    }
}

fn stale_message(e: &str) -> String {
    match e {
        "rejected" => "The page did not accept that text (it changed or cleared it).".into(),
        "hidden" => "That element is not visible right now; take a new snapshot.".into(),
        _ => "That ref is not on the page anymore; take a new snapshot.".into(),
    }
}

/// Temporary tab metadata sessions must also detach if takeover cancels
/// the read while it is awaiting a CDP response.
struct TabAttachment<'a> {
    cdp: &'a Cdp,
    session_id: String,
}

impl Drop for TabAttachment<'_> {
    fn drop(&mut self) {
        self.cdp.send(
            "Target.detachFromTarget",
            json!({"sessionId": self.session_id}),
            None,
        );
    }
}

async fn tabs(state: &AppState, ctx: &TurnCtx, run: &Arc<Running>, a: &Value) -> Result<Output> {
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
    // That took a moment: still this turn's, and still the agent's?
    if a["action"].as_str().unwrap_or("list") != "list" {
        state.browser.check_agent(ctx)?;
    }
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
            state
                .browser
                .attach_guarded(run, &id, &|| state.browser.check_agent(ctx))
                .await?;
            tokio::time::sleep(Duration::from_millis(800)).await;
            snapshot(run).await
        }
        "select" => {
            let id = pick(index)?;
            state
                .browser
                .attach_guarded(run, &id, &|| state.browser.check_agent(ctx))
                .await?;
            snapshot(run).await
        }
        "close" => {
            let id = pick(index)?;
            cdp.call("Target.closeTarget", json!({"targetId": id}), None)
                .await?;
            if id == run.page().target_id {
                state.browser.switch_to_any(run, &id).await?;
            }
            Ok(Output::text("Closed the tab."))
        }
        _ => {
            let current = run.page();
            let mut lines = Vec::new();
            for (i, (id, _, _)) in pages.iter().enumerate() {
                // Target metadata is raw page text. Check the document in
                // its isolated world before returning a title or full URL.
                let info = if *id == current.target_id {
                    eval(run, "info()").await?
                } else {
                    let attached = cdp
                        .call(
                            "Target.attachToTarget",
                            json!({"targetId": id, "flatten": true}),
                            None,
                        )
                        .await?;
                    let session = attached["sessionId"]
                        .as_str()
                        .ok_or("the tab is not ready")?;
                    let attachment = TabAttachment {
                        cdp,
                        session_id: session.to_string(),
                    };
                    let page = super::PageRef {
                        target_id: id.clone(),
                        session_id: attachment.session_id.clone(),
                    };
                    eval_page(run, &page, "info()", None).await?
                };
                if info["tainted"] == true {
                    // A redirect may encode a filled secret even in its host.
                    // Keep only the index usable for an explicit action.
                    lines.push(format!(
                        "{i}: {TAINTED_PAGE}{}",
                        if *id == current.target_id {
                            " (current)"
                        } else {
                            ""
                        }
                    ));
                } else {
                    lines.push(format!(
                        "{i}: {} - {}{}",
                        info["title"].as_str().unwrap_or(""),
                        info["url"].as_str().unwrap_or(""),
                        if *id == current.target_id {
                            " (current)"
                        } else {
                            ""
                        }
                    ));
                }
            }
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
    eval_inner(run, call, None).await
}

/// `eval` for calls that change the page: the turn and control are checked
/// once more right before the command is sent (after the await for the
/// isolated world).
pub async fn eval_mut(
    run: &Running,
    b: &super::Browser,
    ctx: &TurnCtx,
    call: &str,
) -> Result<Value> {
    eval_inner(run, call, Some((b, ctx))).await
}

async fn eval_inner(
    run: &Running,
    call: &str,
    guard: Option<(&super::Browser, &TurnCtx)>,
) -> Result<Value> {
    eval_page(run, &run.page(), call, guard).await
}

async fn eval_page(
    run: &Running,
    page: &super::PageRef,
    call: &str,
    guard: Option<(&super::Browser, &TurnCtx)>,
) -> Result<Value> {
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
    // Secrets Iris typed go to its isolated world only, so the helpers can
    // redact any copy before formatting it.
    let secrets = run.secrets();
    let prelude = format!(
        "__iris.setSecrets({}); __iris.protect({}, {});\n",
        json!(secrets),
        json!(run.secret_docs()),
        run.tab_tainted(&page.target_id),
    );
    if let Some((b, ctx)) = guard {
        b.check_agent(ctx)?;
    }
    let expression = format!(
        r#"{PAGE_JS}
{prelude}
(() => {{
    try {{ return {{ value: __iris.{call} }}; }}
    catch (e) {{
        return {{ error: __iris.isTainted() ? {} : String(e) }};
    }}
}})()"#,
        json!(TAINTED_PAGE),
    );
    let r = run
        .cdp
        .call(
            "Runtime.evaluate",
            json!({"expression": expression, "contextId": ctx,
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
    let result = &r["result"]["value"];
    if let Some(error) = result["error"].as_str() {
        return Err(error.to_string().into());
    }
    Ok(result["value"].clone())
}

/// Still on a page Iris filled sign-in details into (sites like X keep every
/// sign-in step on one page): the action ran, the page just cannot be read.
pub(crate) const STILL_SIGNING_IN: &str = "Done. The page still has the user's sign-in details in it, so it \
    cannot be read until the sign-in moves to a new page. If it now asks for the password, call \
    fill_login again with submit; if it asks for a code or a check, ask the user to take over \
    the browser. Use `wait` for a few seconds and try `snapshot` again if it may still be loading.";

/// The snapshot after an action. A page that is still locked by a fill is
/// not a failure of the action.
async fn snapshot_after(run: &Running) -> Result<Output> {
    match snapshot(run).await {
        Err(e) if e.to_string() == TAINTED_PAGE => Ok(Output::text(STILL_SIGNING_IN)),
        r => r,
    }
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

/// `guard` runs before each input command (the agent's calls stop the
/// moment the owner takes over or the turn ends; a press is always
/// followed by its release).
pub async fn click_at(
    cdp: &Cdp,
    s: Option<&str>,
    x: f64,
    y: f64,
    guard: &(dyn Fn() -> Result<()> + Sync),
) -> Result<()> {
    guard()?;
    cdp.call(
        "Input.dispatchMouseEvent",
        json!({"type": "mouseMoved", "x": x, "y": y}),
        s,
    )
    .await?;
    guard()?;
    // Press and release are queued together (no await in between), so a
    // cancelled call never leaves the button held down.
    let ev = |t: &str| json!({"type": t, "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1});
    cdp.send("Input.dispatchMouseEvent", ev("mousePressed"), s);
    cdp.call("Input.dispatchMouseEvent", ev("mouseReleased"), s)
        .await?;
    Ok(())
}

/// One or two clicks (a double-click) at a point.
async fn click_n(
    cdp: &Cdp,
    s: Option<&str>,
    x: f64,
    y: f64,
    clicks: u8,
    guard: &(dyn Fn() -> Result<()> + Sync),
) -> Result<()> {
    click_at(cdp, s, x, y, guard).await?;
    if clicks < 2 {
        return Ok(());
    }
    guard()?;
    let ev = |t: &str| json!({"type": t, "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 2});
    cdp.send("Input.dispatchMouseEvent", ev("mousePressed"), s);
    cdp.call("Input.dispatchMouseEvent", ev("mouseReleased"), s)
        .await?;
    Ok(())
}

/// Press, move in small steps (pages watch the movement), release. The
/// release is always sent, even if control changes on the way.
async fn drag(
    cdp: &Cdp,
    s: Option<&str>,
    (x, y): (f64, f64),
    (tx, ty): (f64, f64),
    guard: &(dyn Fn() -> Result<()> + Sync),
) -> Result<()> {
    guard()?;
    let ev = |t: &str, x: f64, y: f64| json!({"type": t, "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1});
    cdp.call(
        "Input.dispatchMouseEvent",
        json!({"type": "mouseMoved", "x": x, "y": y}),
        s,
    )
    .await?;
    guard()?;
    cdp.call("Input.dispatchMouseEvent", ev("mousePressed", x, y), s)
        .await?;
    let mut moved = Ok(());
    const STEPS: u32 = 12;
    for i in 1..=STEPS {
        if let Err(e) = guard() {
            moved = Err(e);
            break;
        }
        let f = i as f64 / STEPS as f64;
        let (mx, my) = (x + (tx - x) * f, y + (ty - y) * f);
        let _ = cdp
            .call("Input.dispatchMouseEvent", ev("mouseMoved", mx, my), s)
            .await;
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    let (rx, ry) = if moved.is_ok() { (tx, ty) } else { (x, y) };
    cdp.call("Input.dispatchMouseEvent", ev("mouseReleased", rx, ry), s)
        .await?;
    moved
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

pub async fn press_key(
    cdp: &Cdp,
    s: Option<&str>,
    key: &str,
    guard: &(dyn Fn() -> Result<()> + Sync),
) -> Result<()> {
    guard()?;
    if let Some((k, code, vk, text)) = key_info(key) {
        let mut down = json!({"type": if text.is_empty() { "rawKeyDown" } else { "keyDown" },
            "key": k, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk});
        if !text.is_empty() {
            down["text"] = text.into();
            down["unmodifiedText"] = text.into();
        }
        // Down and up are queued together (see click_at).
        cdp.send("Input.dispatchKeyEvent", down, s);
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
            cdp.send(
                "Input.dispatchKeyEvent",
                json!({"type": "keyDown", "key": t, "text": t, "unmodifiedText": t}),
                s,
            );
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
pub async fn history_step(
    cdp: &Cdp,
    s: Option<&str>,
    delta: i64,
    guard: impl Fn() -> Result<()>,
) -> Result<bool> {
    let h = cdp.call("Page.getNavigationHistory", json!({}), s).await?;
    let idx = h["currentIndex"].as_i64().unwrap_or(0) + delta;
    let Some(entry) = h["entries"]
        .as_array()
        .and_then(|e| usize::try_from(idx).ok().and_then(|i| e.get(i)))
    else {
        return Ok(false);
    };
    guard()?;
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
        let (owner, conv) = state
            .db
            .with(|c| {
                let owner = crate::db::owner_user_id(c)?.unwrap();
                let conv = crate::db::ensure_main_conversation(c, &owner)?;
                Ok((owner, conv))
            })
            .unwrap();
        let g = b.begin_turn(&owner, &conv, "m", &[]);
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

    #[tokio::test]
    async fn admitted_reads_are_cancelled_and_captured_content_is_discarded_on_takeover() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let b = &state.browser;
        let turn = b.begin_turn("owner", "conv", "m", &[]);
        let ctx = b.turn(turn.token()).unwrap();

        // A screenshot response completes in the same poll as a takeover
        // and immediate hand-back. Checking only the current state leaks it.
        let result = while_agent(b, &ctx, async {
            b.control.send_replace(super::super::Control::User {
                user_id: "owner".into(),
                device_id: "phone".into(),
            });
            b.hand_back();
            Ok(Output {
                text: "captured during takeover".into(),
                image: Some("private pixels".into()),
                is_error: false,
            })
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), super::super::TAKEN_OVER);

        // A polling read must stop without waiting for its deadline.
        let read = while_agent(b, &ctx, async {
            b.control.send_replace(super::super::Control::User {
                user_id: "owner".into(),
                device_id: "phone".into(),
            });
            std::future::pending::<Result<Output>>().await
        });
        let result = tokio::time::timeout(Duration::from_millis(200), read)
            .await
            .unwrap();
        assert_eq!(result.unwrap_err().to_string(), super::super::TAKEN_OVER);
    }

    #[test]
    fn origins_of_urls() {
        assert_eq!(
            url_origin("https://Shop.Test:443/a?b").unwrap(),
            "https://shop.test"
        );
        assert_eq!(
            url_origin("http://u@127.0.0.1:8080/x").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert!(url_origin("about:blank").is_none());
        let mut v = Vec::new();
        collect_origins(
            &json!({"frame": {"securityOrigin": "https://a.test"},
                    "childFrames": [{"frame": {"securityOrigin": "https://b.test"}}]}),
            &mut v,
        );
        assert_eq!(v, ["https://a.test", "https://b.test"]);
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
