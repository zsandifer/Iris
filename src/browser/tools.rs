//! The browser tools the agent gets: Playwright MCP's own, served by `iris
//! mcp --browser` (started by the harness CLI) and forwarded over the
//! private socket to the server that runs the browser, plus Iris's own
//! `fill_saved_login` / `fill_saved_card` (`fill.rs`), `restart_browser`
//! and `ask_to_save_login` (`own.rs`).
//!
//! Iris passes Playwright calls through unchanged or refuses them; it never
//! decides what to call. Refused: tools outside [`ALLOWED`], calls that would read
//! or write files on the server, addresses that are not web pages, and any
//! use of a page where the owner just typed private details.

use super::{Access, Browser, Control, TurnCtx};
use crate::state::AppState;
use crate::util::Result;
use serde_json::{Value, json};
use std::time::Duration;

/// MCP server name (tool prefix `mcp__browser__`).
pub const SERVER: &str = "browser";
/// Name used instead when an older install already has a connector called
/// `browser` (that connector keeps working).
pub const SERVER_ALT: &str = "iris_browser";

/// Playwright MCP tools the agent may use. Everything else the server
/// offers is hidden and refused: `browser_run_code_unsafe` (runs code in
/// the server, which is Iris's side), `browser_evaluate` (page scripts can
/// read cookies and stored tokens), `browser_file_upload` and
/// `browser_drop` (read files on the server), `browser_network_request(s)`
/// (headers carry cookies), `browser_console_messages`, `browser_resize`
/// (the live view has a fixed size), `browser_close` (Iris opens and closes
/// the browser), and whatever a later version adds until it is reviewed
/// here.
pub const ALLOWED: &[&str] = &[
    "browser_navigate",
    "browser_navigate_back",
    "browser_tabs",
    "browser_snapshot",
    "browser_find",
    "browser_take_screenshot",
    "browser_click",
    "browser_hover",
    "browser_drag",
    "browser_type",
    "browser_fill_form",
    "browser_select_option",
    "browser_press_key",
    "browser_handle_dialog",
    "browser_wait_for",
    "browser_mouse_click_xy",
    "browser_mouse_drag_xy",
    "browser_mouse_move_xy",
    "browser_mouse_down",
    "browser_mouse_up",
    "browser_mouse_wheel",
];

/// What a turn preparing an ask-first routine may use: open, read, and
/// scroll pages. Clicking, typing, filling and dialogs wait for the
/// owner's OK (a click can place an order).
const LOOK: &[&str] = &[
    "browser_navigate",
    "browser_navigate_back",
    "browser_tabs",
    "browser_snapshot",
    "browser_find",
    "browser_take_screenshot",
    "browser_wait_for",
    "browser_hover",
    "browser_mouse_move_xy",
    "browser_mouse_wheel",
    super::own::RESTART,
];

const LOOK_ONLY: &str = "Not done: this routine checks with the owner before acting, and this \
     step only prepares, so the browser can only open, read and scroll pages right now. Note \
     what you would click or type, then end your reply with a NEEDS_OK: line saying exactly what \
     you will do; you can do it once the owner says yes.";

/// Tools that only change which page is open (or answer a dialog), so
/// they stay usable while the current page holds the owner's private
/// details.
const NAVIGATION: &[&str] = &[
    "browser_navigate",
    "browser_navigate_back",
    "browser_tabs",
    "browser_handle_dialog",
];

/// Everything of the page's text and field values (all frames), to check
/// for saved secrets Iris filled before a picture of it is taken.
const PAGE_TEXT_CODE: &str = "async (page) => {
  const out = [];
  for (const f of page.frames()) {
    out.push(await f.locator('body').innerText({ timeout: 2000 }).catch(() => ''));
    out.push(...(await f.locator('input, textarea').evaluateAll((els) => els.map((e) => e.value)).catch(() => [])));
  }
  return out.join('\\n').slice(0, 4000000);
}";

const SHOWS_SECRET: &str = "No screenshot right now: this page shows saved sign-in or card \
    details. Use browser_snapshot instead.";
const SEARCH_SECRET: &str = "This page shows saved sign-in or card details, so searching it \
    or naming elements by their text is not available. Use browser_snapshot and its refs.";

/// Calls whose outcome depends on what the page shows, beyond the
/// redacted text: pictures, searches, waiting for text, and targets
/// written as selectors instead of snapshot refs.
fn probes_page(name: &str, args: &Value) -> bool {
    fn selector_target(v: &Value) -> bool {
        match v {
            Value::Object(m) => m.iter().any(|(k, v)| {
                let k = k.to_ascii_lowercase();
                (k.ends_with("target") || k.contains("selector") || k == "ref")
                    && v.as_str().is_some_and(|t| !is_ref(t))
                    || selector_target(v)
            }),
            Value::Array(a) => a.iter().any(selector_target),
            _ => false,
        }
    }
    matches!(
        name,
        "browser_take_screenshot" | "browser_find" | "browser_wait_for"
    ) || selector_target(args)
}

/// A snapshot ref (`e12`, `f1e3`).
fn is_ref(s: &str) -> bool {
    let s = s.trim();
    let rest = match s.strip_prefix('f') {
        Some(r) => match r.find('e') {
            Some(i) if i > 0 && r[..i].bytes().all(|b| b.is_ascii_digit()) => &r[i..],
            _ => return false,
        },
        None => s,
    };
    rest.strip_prefix('e')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Tools that only read the page. Everything else acts on it, and Playwright
/// keeps going with an action (waiting for its target) even when its answer
/// is no longer wanted.
const READS: &[&str] = &[
    "browser_snapshot",
    "browser_find",
    "browser_take_screenshot",
    "browser_wait_for",
];

/// How long a take-over or Stop keeps the page for an action already sent
/// to the browser: Playwright cannot be told to stop one, so an action
/// that changes the page (a click, typing, a multi-field fill) keeps it
/// until it finishes (at most the call's own limit); opening a page only
/// for a while, since a slow page finishing its load under the owner's
/// hands changes nothing they typed.
fn drain_for(name: &str) -> Duration {
    if READS.contains(&name) || name == super::own::SAVE_LOGIN {
        Duration::ZERO
    } else if matches!(name, "browser_navigate" | "browser_navigate_back") {
        Duration::from_secs(8)
    } else {
        super::CALL_TIMEOUT
    }
}

/// The definitions the agent sees: the allowed ones, without their
/// `filename` option (results are never written to files for the agent).
pub fn allowed_definitions(all: Vec<Value>) -> Vec<Value> {
    all.into_iter()
        .filter(|t| t["name"].as_str().is_some_and(|n| ALLOWED.contains(&n)))
        .map(|mut t| {
            // `get_mut`, not `t[..]`: indexing a Value mutably inserts null
            // for a missing key, and MCP clients reject `"required": null`.
            if let Some(schema) = t.get_mut("inputSchema").and_then(Value::as_object_mut) {
                if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                    props.remove("filename");
                }
                if let Some(req) = schema.get_mut("required").and_then(Value::as_array_mut) {
                    req.retain(|r| r != "filename");
                }
            }
            t
        })
        .collect()
}

/// Whether `s` starts with a URL scheme (`javascript:`, `file:`), the way
/// a browser's URL parser reads it. That includes `host:port`, which
/// parsers take as a scheme too.
pub(crate) fn has_scheme(s: &str) -> bool {
    let Some((scheme, _)) = s.split_once(':') else {
        return false;
    };
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// A bare `host:port` address (`localhost:8080/x`), as the owner may type
/// it: a host name, then only digits up to the path.
pub(crate) fn host_port(s: &str) -> bool {
    let Some((host, rest)) = s.split_once(':') else {
        return false;
    };
    let port_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
        && (1..=5).contains(&port_len)
        && rest[..port_len].chars().all(|c| c.is_ascii_digit())
}

/// Web pages only: http(s), a bare domain without a port, or a blank
/// page. Whitespace and control characters are refused outright: browsers
/// drop them while parsing, so `java\nscript:` would still run script.
fn web_address(url: &str) -> bool {
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let s = url.to_ascii_lowercase();
    s.starts_with("http://") || s.starts_with("https://") || s == "about:blank" || !has_scheme(&s)
}

/// Refuse what Iris never forwards.
fn check_args(name: &str, args: &Value) -> std::result::Result<(), String> {
    if !args.is_object() {
        return Err("arguments must be an object".into());
    }
    // Playwright reads output options from `_meta` (JSON results escape
    // values a second time, past what redaction recognizes).
    if args.get("_meta").is_some() {
        return Err("Output options (`_meta`) are not available here.".into());
    }
    if args.get("filename").is_some() {
        return Err(
            "Saving to files is not available here; leave out `filename` to get the result \
             directly."
                .into(),
        );
    }
    if matches!(name, "browser_navigate" | "browser_tabs")
        && let Some(url) = args.get("url")
        && !url.as_str().is_some_and(web_address)
    {
        return Err(
            "Only web pages (http or https) can be opened; write the full address, like \
             https://example.com or http://localhost:8080."
                .into(),
        );
    }
    Ok(())
}

/// A call naming arguments its tool does not have, when it would fail
/// anyway (a required one is missing, or none it knows was given).
/// Playwright ignores unknown names and then fails on the missing one with
/// a message models misread ("expected string, received undefined → at
/// target"), so the same wrong call came back several times. Iris refuses
/// it with the tool's real argument names instead; it never renames them.
fn unknown_args(def: &Value, name: &str, args: &Value) -> Option<String> {
    let props = def["inputSchema"]["properties"].as_object()?;
    let given = args.as_object()?;
    let unknown: Vec<&str> = given
        .keys()
        .map(String::as_str)
        .filter(|k| !props.contains_key(*k))
        .collect();
    if unknown.is_empty() {
        return None;
    }
    let required: Vec<&str> = def["inputSchema"]["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|r| !given.contains_key(*r))
        .collect();
    if missing.is_empty() && given.keys().any(|k| props.contains_key(k)) {
        return None;
    }
    let quote = |v: &[&str]| {
        v.iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut known: Vec<String> = props
        .keys()
        .filter(|k| *k != "filename")
        .map(|k| {
            if required.contains(&k.as_str()) {
                format!("`{k}` (required)")
            } else {
                format!("`{k}`")
            }
        })
        .collect();
    known.sort_by_key(|k| !k.ends_with("(required)"));
    let mut out = format!(
        "{name} has no argument {}. Its arguments: {}.",
        quote(&unknown),
        known.join(", ")
    );
    if props.values().any(|p| {
        p["description"]
            .as_str()
            .is_some_and(|d| d.contains("element reference from the page snapshot"))
    }) {
        out.push_str(" Element refs from browser_snapshot (like e12) go in `target`.");
    }
    Some(out)
}

fn error(text: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}], "isError": true})
}

/// System prompt section when the browser is attached.
pub fn guide(server: &str) -> String {
    GUIDE.replace("the `browser` tools", &format!("the `{server}` tools"))
}

const GUIDE: &str = "
## Your browser

You have your own web browser (the `browser` tools, Playwright), which the
user can watch live and take over from their phone. Use it for anything that
needs a real page: forms, shopping carts, sites that need JavaScript, sites
they are signed in to. For quick reading, ordinary web search or fetch tools
are fine.

- `browser_navigate`, then `browser_snapshot` to read the page and get element
  refs; act with `browser_click`, `browser_type`, `browser_fill_form`,
  `browser_select_option`, `browser_press_key`. Action results do not show
  the page: call `browser_snapshot` to see it, and pass its refs (like
  `e12`) as `target`.
- `browser_take_screenshot` when you need to see the page. For what the
  snapshot cannot reach (CAPTCHA boxes and picture grids, frames from other
  sites, maps, canvases), click or drag by position with
  `browser_mouse_click_xy` / `browser_mouse_drag_xy` (CSS pixels of the
  screenshot), then screenshot again to check.
- Signing in: never type a password, card number, or code yourself, and
  never ask for one in chat. Call `fill_saved_login` with the refs of the
  username and password fields and `submit: true` (or `submit_ref`, the
  sign-in button's ref, when Enter does not sign in); Iris fills the user's
  saved login and you never see it (for two-step forms, call it again on the
  password step). Its answer says whether the password left the page; then
  take a `browser_snapshot` to see whether you are signed in. If it answers
  `not found`, offer to save one: `ask_to_save_login` posts a card where the
  user types it in (you never see it), then wait for them. If the site asks
  for a code or a check, or fights automated browsers, ask the user to take
  over the browser in the app and sign in themselves, then wait (if the
  sign-in opened in a new tab or window, select it with `browser_tabs` first:
  the user sees the tab you are on). Sign-ins stay in the browser, so this is
  usually needed once per site.
- X (x.com, twitter.com): its sign-in stalls in your browser (checks you
  cannot pass). Do not try to sign in there yourself, not even the username:
  ask the user right away to take over and sign in, then wait.
- Paying: call `fill_saved_card` with the refs of the payment fields. The user
  approves every use in the chat; tell them you are waiting for their OK.
- While the user has control, tools answer that they are waiting; tell the
  user you are waiting for them to hand it back. A page where the user typed
  a password or card stays closed to you until it moves on (you can still
  fill saved details there, or open another page). A page where Iris filled
  a saved password or card is closed only while it is still on the page:
  meanwhile `browser_snapshot` works, with saved details shown as [hidden].
- If the browser is stuck or crashed (pages never load, tools keep timing
  out), call `restart_browser`: it closes and reopens it, sign-ins kept. Then
  open the page again.
- CAPTCHAs: you may solve them. Click the \"I'm not a robot\" box; for picture
  puzzles pick the tiles, then the verify button; drag sliders. If a new
  puzzle keeps coming after two or three tries, ask the user to take over.
- Do not place an order or send anything they have not clearly asked for.
";

/// `tools/list` for a registered turn.
pub async fn list(state: &AppState, token: &str) -> Value {
    if state.browser.turn(token).is_none() {
        return json!({"tools": []});
    }
    match state.browser.tool_list().await {
        Ok(mut tools) => {
            tools.extend(super::fill::definitions());
            tools.extend(super::own::definitions());
            json!({"tools": tools})
        }
        Err(e) => {
            crate::warn!("browser: tools unavailable: {e}");
            json!({"tools": []})
        }
    }
}

/// Carry out one tool call for a registered turn: the MCP `tools/call`
/// result, as the server sent it, or a refusal.
pub async fn call(state: &AppState, token: &str, name: &str, args: &Value) -> Value {
    let b = &state.browser;
    let Some(ctx) = b.turn(token) else {
        return error(super::TURN_ENDED);
    };
    let fill = super::fill::TOOLS.contains(&name);
    let own = super::own::TOOLS.contains(&name);
    if !ALLOWED.contains(&name) && !fill && !own {
        return error(format!("unknown tool {name}"));
    }
    if ctx.look_only && !LOOK.contains(&name) {
        return error(LOOK_ONLY);
    }
    let args = if args.is_null() {
        json!({})
    } else {
        args.clone()
    };
    if let Err(e) = check_args(name, &args) {
        return error(e);
    }
    if !fill
        && let Ok(defs) = b.tool_list().await
        && let Some(def) = defs.iter().find(|d| d["name"] == name)
        && let Some(e) = unknown_args(def, name, &args)
    {
        return error(e);
    }
    if b.is_paused() {
        return error(
            "The owner has paused Iris's computer, so the browser is off. Tell them if you \
             need it.",
        );
    }
    if !access_allowed(state, &ctx).await {
        return error(super::TURN_ENDED);
    }
    b.mark_used(&ctx);
    // A card needs the owner's OK first, asked without holding the page.
    let mut approved = None;
    if name == "fill_saved_card" {
        match super::fill::approve_card(state, &ctx, &args).await {
            Ok(a) => approved = Some(a),
            Err(out) => return out,
        }
    }
    // Wait while the owner has control, then hold the page; control is
    // checked again once the page is ours, so a call queued behind another
    // never runs after a take-over.
    let _op = loop {
        if !b.wait_for_agent(&ctx).await {
            return if ctx.is_ended() {
                error(super::TURN_ENDED)
            } else {
                error(super::TAKEN_OVER)
            };
        }
        let guard = tokio::select! {
            g = b.op_lock.lock() => g,
            _ = ctx.wait_ended() => return error(super::TURN_ENDED),
        };
        if b.control() == Control::Agent {
            break guard;
        }
    };
    if let Err(e) = b.check_agent(&ctx) {
        return error(e.to_string());
    }
    // Closing and reopening is run to the end (a start cut short could
    // leave a half-started browser); the page is held meanwhile.
    if name == super::own::RESTART {
        b.set_busy(true);
        let restarted = b.restart().await;
        b.set_busy(false);
        if let Err(e) = restarted {
            return error(format!("The browser could not restart: {e}"));
        }
        if let Err(e) = b.check_agent(&ctx) {
            return error(e.to_string());
        }
        return json!({"content": [{"type": "text", "text": "restarted (one blank tab; sign-ins \
            are kept: open the page again with browser_navigate)"}]});
    }
    // Starting is cancelled too if the turn ends meanwhile.
    let started = tokio::select! {
        r = b.ensure() => r,
        _ = ctx.wait_ended() => return error(super::TURN_ENDED),
    };
    let run = match started {
        Ok(r) => r,
        Err(e) => return error(format!("The browser could not start: {e}")),
    };
    // Starting can take a while: the turn may have ended meanwhile.
    if let Err(e) = b.check_agent(&ctx) {
        return error(e.to_string());
    }
    // Filling saved details reads nothing, so it may go on on a page that
    // already holds some (the password step after the username); asking
    // to save a login reads only the page's address. A saved secret Iris
    // filled is hidden from snapshots, so the page can still be read while
    // it is there (to see whether a sign-in went through).
    if !NAVIGATION.contains(&name) && !fill && !own {
        match b.page_access(&run).await {
            Ok(Access::Open) => {}
            Ok(Access::Filled) => match name {
                "browser_snapshot" => {}
                "browser_take_screenshot" => return error(SHOWS_SECRET),
                _ if probes_page(name, &args) => return error(SEARCH_SECRET),
                _ => return error(super::FILLED_PAGE),
            },
            Ok(Access::Typed) => return error(super::PRIVATE_PAGE),
            Err(e) => return error(e.to_string()),
        }
    }
    // Saved secrets may show on later pages (echoed into the text or a
    // field): no pictures of those, and no searching them (a match or not
    // tells what is hidden, a character at a time).
    let secrets = b.filled_secrets();
    if !secrets.is_empty() && probes_page(name, &args) {
        let shown = match b.run_code(&run, PAGE_TEXT_CODE).await {
            Ok(v) => v.as_str().unwrap_or("").to_string(),
            Err(e) => return error(e.to_string()),
        };
        if super::hide_secrets(&shown, &secrets) != shown {
            return error(if name == "browser_take_screenshot" {
                SHOWS_SECRET
            } else {
                SEARCH_SECRET
            });
        }
    }
    b.set_busy(true);
    let started = std::time::Instant::now();
    let drain = drain_for(name);
    let work = async {
        match (name, approved) {
            ("fill_saved_login", _) => Ok(super::fill::agent_login(state, &ctx, &run, &args).await),
            ("fill_saved_card", Some(a)) => {
                Ok(super::fill::agent_card(state, &ctx, &run, &args, a).await)
            }
            (super::own::SAVE_LOGIN, _) => {
                Ok(super::own::ask_to_save_login(state, &ctx, &run, &args).await)
            }
            _ => b.tool(&run, name, &args, super::CALL_TIMEOUT).await,
        }
    };
    let result = while_agent(b, &ctx, drain, work).await;
    b.set_busy(false);
    if result.is_err() {
        // An abandoned call may still write its file (a screenshot) after
        // Iris stopped waiting for it.
        let late = b.clone();
        tokio::spawn(async move {
            for wait in [2, 10, 30] {
                tokio::time::sleep(Duration::from_secs(wait)).await;
                late.clear_output();
            }
        });
    }
    let mut out = result.unwrap_or_else(|e| error(e.to_string()));
    // A file picker would block every tool, and the agent may not upload
    // files: Iris cancels it.
    let text = super::text_of(&out);
    if text.contains("can be handled by browser_file_upload")
        && b.answer_modal(&run, &text, false).await
        && let Some(parts) = out.get_mut("content").and_then(Value::as_array_mut)
    {
        parts.push(json!({"type": "text",
            "text": "(Iris closed the file picker: uploading files is not available.)"}));
    }
    // A picture is checked again once taken: the page may have shown a
    // saved secret, or become private, while it was being taken (text is
    // redacted below; pixels cannot be).
    if name == "browser_take_screenshot" && out["isError"] != true {
        let access = b.page_access(&run).await.unwrap_or(Access::Typed);
        let shown = if access == Access::Filled {
            Some(true)
        } else if secrets.is_empty() {
            Some(false)
        } else {
            b.run_code(&run, PAGE_TEXT_CODE).await.ok().map(|v| {
                let t = v.as_str().unwrap_or("");
                super::hide_secrets(t, &secrets) != t
            })
        };
        if access == Access::Typed {
            out = error(super::PRIVATE_PAGE);
        } else if shown != Some(false) {
            out = error(SHOWS_SECRET);
        }
    }
    // Files Playwright wrote for this call (a screenshot) are not for the
    // agent: gone at once.
    b.remove_output(&super::text_of(&out));
    // Anything else that could carry them (addresses, titles, dialogs,
    // page text) has them hidden.
    let secrets = b.filled_secrets();
    if !secrets.is_empty()
        && let Some(parts) = out.get_mut("content").and_then(Value::as_array_mut)
    {
        for p in parts.iter_mut() {
            if let Some(t) = p["text"].as_str() {
                let hidden = super::hide_secrets(t, &secrets);
                p["text"] = hidden.into();
            }
        }
    }
    crate::info!(
        "browser: {name} {} in {}ms",
        if out["isError"] == true {
            "failed"
        } else {
            "ok"
        },
        started.elapsed().as_millis()
    );
    out
}

/// Give up on an admitted call on a control change or when the turn ends,
/// and check once more immediately before releasing its result, even if
/// it completed first. An action already sent keeps the page (the caller
/// holds it) for up to `drain` while it finishes, so it cannot take effect
/// under the owner's hands; reads are dropped at once.
async fn while_agent<T>(
    b: &Browser,
    ctx: &TurnCtx,
    drain: Duration,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let mut control = b.control.subscribe();
    b.check_agent(ctx)?;
    tokio::pin!(work);
    let stopped = tokio::select! {
        biased;
        _ = ctx.wait_ended() => Err(super::TURN_ENDED),
        _ = control.changed() => Err(super::TAKEN_OVER),
        r = &mut work => Ok(r),
    };
    let result = match stopped {
        Ok(r) => r,
        Err(why) => {
            if !drain.is_zero() {
                let _ = tokio::time::timeout(drain, &mut work).await;
            }
            return Err(why.into());
        }
    };
    b.check_agent(ctx)?;
    if control.has_changed().unwrap_or(true) {
        return Err(super::TAKEN_OVER.into());
    }
    result
}

/// The browser profile is private to the owner, including its cookies.
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    pub(crate) fn test_state(tmp: &Path) -> AppState {
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

    #[test]
    fn calls_with_unknown_argument_names_are_refused_with_the_real_ones() {
        let target = json!({"type": "string",
            "description": "Exact target element reference from the page snapshot, or a unique element selector"});
        let click = json!({"name": "browser_click", "inputSchema": {"type": "object",
            "properties": {"element": {"type": "string"}, "target": target,
                           "doubleClick": {"type": "boolean"}},
            "required": ["target"], "additionalProperties": false}});
        let navigate = json!({"name": "browser_navigate", "inputSchema": {"type": "object",
            "properties": {"url": {"type": "string"}}, "required": ["url"]}});
        let find = json!({"name": "browser_find", "inputSchema": {"type": "object",
            "properties": {"text": {"type": "string"}, "regex": {"type": "string"},
                           "filename": {"type": "string"}}}});
        // The old Playwright name for the element: refused, naming `target`.
        let e = unknown_args(&click, "browser_click", &json!({"ref": "e9"})).unwrap();
        assert!(e.contains("no argument `ref`"), "{e}");
        assert!(e.contains("`target` (required)"), "{e}");
        assert!(e.contains("go in `target`"), "{e}");
        let e = unknown_args(
            &click,
            "browser_click",
            &json!({"element": "Add", "ref": "e9"}),
        );
        assert!(e.is_some());
        let e = unknown_args(
            &navigate,
            "browser_navigate",
            &json!({"file_path": "https://example.test/"}),
        )
        .unwrap();
        assert!(
            e.contains("`url` (required)") && !e.contains("go in `target`"),
            "{e}"
        );
        // Nothing it knows: refused (Playwright would only say "provide text").
        let e = unknown_args(&find, "browser_find", &json!({"query": "Cart"})).unwrap();
        assert!(
            e.contains("`text`") && e.contains("`regex`") && !e.contains("filename"),
            "{e}"
        );
        // Calls that would work go through, extra names and all.
        for (def, args) in [
            (&click, json!({"target": "e9"})),
            (&click, json!({"target": "e9", "ref": "e9"})),
            (&navigate, json!({"url": "https://example.test/"})),
            (&find, json!({"text": "Cart", "query": "Cart"})),
            (&find, json!({})),
        ] {
            assert_eq!(unknown_args(def, "t", &args), None, "{args}");
        }
        // No schema to compare with: nothing to say.
        assert_eq!(
            unknown_args(&json!({"name": "x"}), "x", &json!({"ref": "e1"})),
            None
        );
    }

    #[test]
    fn only_reviewed_tools_reach_the_agent() {
        let tool = |name: &str| {
            json!({"name": name, "inputSchema": {"type": "object",
                "properties": {"type": {}, "filename": {}}, "required": ["filename"]}})
        };
        let all = [
            "browser_navigate",
            "browser_take_screenshot",
            "browser_run_code_unsafe",
            "browser_evaluate",
            "browser_file_upload",
            "browser_network_request",
            "browser_close",
            "browser_something_new",
        ]
        .map(tool)
        .to_vec();
        let shown = allowed_definitions(all);
        let names: Vec<_> = shown.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["browser_navigate", "browser_take_screenshot"]);
        assert!(
            shown[1]["inputSchema"]["properties"]
                .get("filename")
                .is_none()
        );
        assert!(shown[1]["inputSchema"]["properties"].get("type").is_some());
        assert_eq!(shown[1]["inputSchema"]["required"], json!([]));
        // A schema without `required` (or without a schema) stays that way.
        let bare = allowed_definitions(vec![
            json!({"name": "browser_navigate", "inputSchema": {"type": "object", "properties": {}}}),
            json!({"name": "browser_snapshot"}),
        ]);
        assert!(bare[0]["inputSchema"].get("required").is_none());
        assert!(bare[1].get("inputSchema").is_none());
        for unsafe_tool in [
            "browser_run_code_unsafe",
            "browser_evaluate",
            "browser_file_upload",
            "browser_drop",
            "browser_network_requests",
            "browser_network_request",
            "browser_console_messages",
        ] {
            assert!(!ALLOWED.contains(&unsafe_tool), "{unsafe_tool}");
        }
    }

    #[test]
    fn refuses_files_and_non_web_addresses() {
        assert!(check_args("browser_navigate", &json!({"url": "https://a.test"})).is_ok());
        assert!(check_args("browser_navigate", &json!({"url": "example.com"})).is_ok());
        assert!(check_args("browser_navigate", &json!({"url": "localhost/x"})).is_ok());
        assert!(
            check_args(
                "browser_navigate",
                &json!({"url": "http://localhost:8080/x"})
            )
            .is_ok()
        );
        assert!(
            check_args(
                "browser_tabs",
                &json!({"action": "new", "url": "about:blank"})
            )
            .is_ok()
        );
        assert!(check_args("browser_tabs", &json!({"action": "list"})).is_ok());
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "chrome://settings",
            "view-source:https://a.test",
            " FILE:///x",
            // Browsers read these as scripts too: a digit after the colon,
            // and tabs or line breaks they remove while parsing.
            "javascript:0;document.body.textContent=document.cookie",
            "JAVASCRIPT:1",
            "java\nscript:alert(1)",
            "java\tscript:alert(1)",
            "\u{1}javascript:alert(1)",
            // Parsed as a `localhost:` scheme, not a host and port.
            "localhost:8080/x",
            "",
        ] {
            assert!(
                check_args("browser_navigate", &json!({"url": bad})).is_err(),
                "{bad}"
            );
            assert!(
                check_args("browser_tabs", &json!({"action": "new", "url": bad})).is_err(),
                "{bad}"
            );
        }
        assert!(check_args("browser_navigate", &json!({"url": 3})).is_err());
        assert!(
            check_args(
                "browser_take_screenshot",
                &json!({"filename": "../../vault.key"})
            )
            .is_err()
        );
        assert!(check_args("browser_snapshot", &json!([])).is_err());
        assert!(check_args("browser_find", &json!({"_meta": {"json": true}})).is_err());
    }

    #[test]
    fn page_probes_are_known() {
        assert!(probes_page("browser_find", &json!({"text": "x"})));
        assert!(probes_page("browser_wait_for", &json!({"text": "x"})));
        assert!(probes_page("browser_take_screenshot", &json!({})));
        assert!(!probes_page("browser_click", &json!({"target": "e12"})));
        assert!(!probes_page(
            "browser_click",
            &json!({"target": "f2e7", "element": "Sign in"})
        ));
        assert!(probes_page(
            "browser_click",
            &json!({"target": "text=/p=E/"})
        ));
        assert!(probes_page(
            "browser_fill_form",
            &json!({"fields": [{"target": "e3", "value": "x"}, {"target": "#q", "value": "y"}]})
        ));
        assert!(probes_page(
            "browser_drag",
            &json!({"startTarget": "e1", "endTarget": "css=div"})
        ));
        assert!(!probes_page("browser_snapshot", &json!({})));
    }

    #[test]
    fn actions_keep_the_page_until_they_finish() {
        assert_eq!(drain_for("browser_snapshot"), Duration::ZERO);
        assert_eq!(drain_for("browser_take_screenshot"), Duration::ZERO);
        assert_eq!(drain_for("browser_navigate"), Duration::from_secs(8));
        for action in [
            "browser_click",
            "browser_fill_form",
            "browser_type",
            "fill_saved_card",
        ] {
            assert_eq!(drain_for(action), super::super::CALL_TIMEOUT, "{action}");
        }
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
        let queued = tokio::spawn(async move {
            call(
                &st,
                &tk,
                "browser_navigate",
                &json!({"url": "https://a.test"}),
            )
            .await
        });
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
        assert_eq!(out["isError"], true);
        assert_eq!(
            out["content"][0]["text"].as_str(),
            Some(super::super::TURN_ENDED)
        );
        assert!(b.running().is_none());
        // Unknown and expired tokens are refused.
        assert_eq!(
            call(&state, &token, "browser_snapshot", &json!({})).await["isError"],
            true
        );
        assert_eq!(
            call(&state, "", "browser_snapshot", &json!({})).await["isError"],
            true
        );
    }

    #[tokio::test]
    async fn admitted_calls_stop_on_take_over_and_actions_finish_first() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let b = &state.browser;
        let turn = b.begin_turn("owner", "conv", "m", &[]);
        let ctx = b.turn(turn.token()).unwrap();
        let take_over = || {
            b.control.send_replace(super::super::Control::User {
                user_id: "owner".into(),
                device_id: "phone".into(),
            });
        };

        // A screenshot completes in the same poll as a take over and an
        // immediate hand back. Checking only the current state leaks it.
        let result = while_agent(b, &ctx, Duration::ZERO, async {
            take_over();
            b.hand_back();
            Ok(json!({"content": [{"type": "image", "data": "private pixels"}]}))
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), super::super::TAKEN_OVER);

        // A read stops without waiting for its deadline.
        b.hand_back();
        let read = while_agent(b, &ctx, Duration::ZERO, async {
            take_over();
            std::future::pending::<Result<Value>>().await
        });
        let result = tokio::time::timeout(Duration::from_millis(200), read)
            .await
            .unwrap();
        assert_eq!(result.unwrap_err().to_string(), super::super::TAKEN_OVER);

        // An action already sent finishes before the page is handed on
        // (its caller holds the page meanwhile), and its result is
        // still not released.
        b.hand_back();
        let done = AtomicBool::new(false);
        let result = while_agent(b, &ctx, Duration::from_secs(5), async {
            take_over();
            tokio::time::sleep(Duration::from_millis(150)).await;
            done.store(true, Ordering::SeqCst);
            Ok(json!({}))
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), super::super::TAKEN_OVER);
        assert!(done.load(Ordering::SeqCst));
        // One that never finishes is given up after the drain time.
        b.hand_back();
        let action = while_agent(b, &ctx, Duration::from_millis(100), async {
            take_over();
            std::future::pending::<Result<Value>>().await
        });
        let result = tokio::time::timeout(Duration::from_secs(2), action)
            .await
            .unwrap();
        assert_eq!(result.unwrap_err().to_string(), super::super::TAKEN_OVER);
        // The turn ending stops it the same way.
        b.hand_back();
        let done = AtomicBool::new(false);
        let result = while_agent(b, &ctx, Duration::from_secs(5), async {
            ctx.ended.send_replace(true);
            tokio::time::sleep(Duration::from_millis(50)).await;
            done.store(true, Ordering::SeqCst);
            Ok(json!({}))
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), super::super::TURN_ENDED);
        assert!(done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn refused_calls_never_reach_the_browser() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let (owner, conv) = state
            .db
            .with(|c| {
                let owner = crate::db::owner_user_id(c)?.unwrap();
                let conv = crate::db::ensure_main_conversation(c, &owner)?;
                Ok((owner, conv))
            })
            .unwrap();
        let g = state.browser.begin_turn(&owner, &conv, "m", &[]);
        let text = |v: Value| v["content"][0]["text"].as_str().unwrap().to_string();
        let r = call(
            &state,
            g.token(),
            "browser_run_code_unsafe",
            &json!({"code": "1"}),
        )
        .await;
        assert_eq!(text(r), "unknown tool browser_run_code_unsafe");
        let r = call(
            &state,
            g.token(),
            "browser_navigate",
            &json!({"url": "file:///x"}),
        )
        .await;
        assert!(text(r).contains("Only web pages"));
        let r = call(&state, "bogus", "browser_snapshot", &json!({})).await;
        assert_eq!(text(r), super::super::TURN_ENDED);
        // Not used: the chat never showed the peek for refused calls.
        assert!(
            state.browser.state_json()["active"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(list(&state, "bogus").await, json!({"tools": []}));
    }

    #[tokio::test]
    async fn preparing_an_ask_first_routine_can_only_look() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let b = &state.browser;
        let turn = b.begin_turn_with("owner", "conv", "m", &[], true);
        // Clicking, typing, filling and dialogs wait for the owner's OK.
        for (name, args) in [
            (
                "browser_click",
                json!({"ref": "e9", "element": "Place order"}),
            ),
            ("browser_type", json!({"ref": "e2", "text": "hi"})),
            ("browser_press_key", json!({"key": "Enter"})),
            ("browser_handle_dialog", json!({"accept": true})),
            ("browser_mouse_click_xy", json!({"x": 1, "y": 2})),
            ("fill_saved_card", json!({})),
        ] {
            let out = call(&state, turn.token(), name, &args).await;
            assert_eq!(out["isError"], true, "{name}");
            assert_eq!(
                out["content"][0]["text"].as_str(),
                Some(LOOK_ONLY),
                "{name}"
            );
        }
        // Looking is not refused for that reason.
        let out = call(&state, turn.token(), "browser_snapshot", &json!({})).await;
        assert_ne!(out["content"][0]["text"].as_str(), Some(LOOK_ONLY));
        // An ordinary turn may click.
        let normal = b.begin_turn("owner", "conv2", "m2", &[]);
        let out = call(
            &state,
            normal.token(),
            "browser_click",
            &json!({"ref": "e9"}),
        )
        .await;
        assert_ne!(out["content"][0]["text"].as_str(), Some(LOOK_ONLY));
    }
}
