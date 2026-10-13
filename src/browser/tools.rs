//! The browser tools the agent gets: Playwright MCP's own, served by `iris
//! mcp --browser` (started by the harness CLI) and forwarded over the
//! private socket to the server that runs the browser.
//!
//! Iris passes calls through unchanged or refuses them; it never decides
//! what to call. Refused: tools outside [`ALLOWED`], calls that would read
//! or write files on the server, addresses that are not web pages, and any
//! use of a page where the owner just typed private details.

use super::{Browser, Control, TurnCtx};
use crate::state::AppState;
use crate::util::Result;
use serde_json::{Value, json};

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

/// Tools that only change which page is open, so they stay usable while
/// the current page holds the owner's private details.
const NAVIGATION: &[&str] = &["browser_navigate", "browser_navigate_back", "browser_tabs"];

/// The definitions the agent sees: the allowed ones, without their
/// `filename` option (results are never written to files for the agent).
pub fn allowed_definitions(all: Vec<Value>) -> Vec<Value> {
    all.into_iter()
        .filter(|t| t["name"].as_str().is_some_and(|n| ALLOWED.contains(&n)))
        .map(|mut t| {
            if let Some(props) = t["inputSchema"]["properties"].as_object_mut() {
                props.remove("filename");
            }
            if let Some(req) = t["inputSchema"]["required"].as_array_mut() {
                req.retain(|r| r != "filename");
            }
            t
        })
        .collect()
}

/// Whether `s` starts with a URL scheme (`javascript:`, `file:`), as
/// opposed to a bare host (with or without a port).
pub(crate) fn has_scheme(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once(':') else {
        return false;
    };
    let scheme_like = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    scheme_like && !rest.starts_with(|c: char| c.is_ascii_digit())
}

/// Web pages only: http(s), a bare domain, or a blank page.
fn web_address(url: &str) -> bool {
    let s = url.trim().to_ascii_lowercase();
    s.starts_with("http://") || s.starts_with("https://") || s == "about:blank" || !has_scheme(&s)
}

/// Refuse what Iris never forwards.
fn check_args(name: &str, args: &Value) -> std::result::Result<(), String> {
    if !args.is_object() {
        return Err("arguments must be an object".into());
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
        return Err("Only web pages (http or https) can be opened.".into());
    }
    Ok(())
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
  `browser_select_option`, `browser_press_key`. Action results only link to
  files you cannot open: call `browser_snapshot` to see the page.
- `browser_take_screenshot` when you need to see the page. For what the
  snapshot cannot reach (CAPTCHA boxes and picture grids, frames from other
  sites, maps, canvases), click or drag by position with
  `browser_mouse_click_xy` / `browser_mouse_drag_xy` (CSS pixels of the
  screenshot), then screenshot again to check.
- Signing in, paying, and verification codes are the user's: never type a
  password, card number, or code, and never ask for one in chat. Ask the user
  to take over the browser in the app and do that step themselves (they can
  fill their saved logins and cards from there), then wait. Sign-ins stay in
  the browser, so this is usually needed once per site. Sites that fight
  automated browsers (for example X) always need the user to sign in.
- While the user has control, tools answer that they are waiting; tell the
  user you are waiting for them to hand it back. A page where they typed
  private details stays closed to you until it moves on.
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
        Ok(tools) => json!({"tools": tools}),
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
    if !ALLOWED.contains(&name) {
        return error(format!("unknown tool {name}"));
    }
    let args = if args.is_null() {
        json!({})
    } else {
        args.clone()
    };
    if let Err(e) = check_args(name, &args) {
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
    if !NAVIGATION.contains(&name) {
        match b.private_page(&run).await {
            Ok(false) => {}
            Ok(true) => return error(super::PRIVATE_PAGE),
            Err(e) => return error(e.to_string()),
        }
    }
    b.set_busy(true);
    let started = std::time::Instant::now();
    let result = while_agent(b, &ctx, b.tool(&run, name, &args, super::CALL_TIMEOUT)).await;
    b.set_busy(false);
    let out = result.unwrap_or_else(|e| error(e.to_string()));
    if name == "browser_navigate" && out["isError"] != true {
        b.left_private_page();
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

/// Cancel an admitted call on a control change and check once more
/// immediately before releasing its result, even if it completed first.
async fn while_agent<T>(
    b: &Browser,
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
        assert!(check_args("browser_navigate", &json!({"url": "localhost:8080/x"})).is_ok());
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
}
