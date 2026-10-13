//! Iris's own browser tools besides the fills (`fill.rs`):
//! `restart_browser` (close and reopen the browser, keeping its profile,
//! without offering Playwright's `browser_close`) and `ask_to_save_login`
//! (a card in the chat where the owner saves a login for the site open in
//! the browser; the agent never sees what they type).

use super::{Running, TurnCtx};
use crate::state::AppState;
use serde_json::{Value, json};

pub const RESTART: &str = "restart_browser";
pub const SAVE_LOGIN: &str = "ask_to_save_login";
pub const TOOLS: &[&str] = &[RESTART, SAVE_LOGIN];

pub fn definitions() -> Vec<Value> {
    let r = |d: &str| json!({"type": "string", "description": d});
    vec![
        json!({
            "name": RESTART,
            "description": "Close your browser and open it again, when it is stuck, crashed, or \
                misbehaving (pages never load, tools time out). Sign-ins are kept; every tab \
                closes, so open the page again with browser_navigate afterwards.",
            "inputSchema": {"type": "object", "properties": {}},
        }),
        json!({
            "name": SAVE_LOGIN,
            "description": "Post a card in this chat where the user saves a login (username and \
                password) for the site open in your browser, to Logins & wallet. The site is \
                taken from the current page; they type the login into the card and you never \
                see it. Use it when they want help saving a login, or when fill_saved_login \
                answers `not found` and they would rather save one than sign in themselves. \
                Then wait for them: once saved, fill_saved_login can use it.",
            "inputSchema": {"type": "object", "properties": {
                "label": r("A short name for the login, like the site's name (optional; the user can change it)"),
                "username": r("The username or email to prefill, only if the user told you (optional; never a password)"),
            }},
        }),
    ]
}

fn text(t: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": t.into()}]})
}

fn error(t: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": t.into()}], "isError": true})
}

/// An optional short text argument: trimmed, at most `max` characters, no
/// control characters. Err: a tool result.
fn arg(args: &Value, key: &str, max: usize) -> std::result::Result<Option<String>, Value> {
    let Some(v) = args.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let s = v
        .as_str()
        .ok_or_else(|| error(format!("`{key}` must be text")))?
        .trim();
    if s.chars().count() > max || s.chars().any(char::is_control) {
        return Err(error(format!(
            "`{key}` must be at most {max} characters on one line"
        )));
    }
    Ok((!s.is_empty()).then(|| s.to_string()))
}

/// `ask_to_save_login`: a `save_login` card for the current page's site
/// (from the browser itself, never from the agent), in the turn's chat.
pub async fn ask_to_save_login(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    args: &Value,
) -> Value {
    if ctx.audience.is_empty() {
        return text("not posted (only in a chat with the user, who saves it there)");
    }
    let (label, username) = match (arg(args, "label", 100), arg(args, "username", 300)) {
        (Ok(l), Ok(u)) => (l, u),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let url = match state.browser.current_url(run).await {
        Ok(u) => u,
        Err(e) => return error(format!("not posted ({e})")),
    };
    let Some((site, host)) = super::fill::site_of(&url) else {
        return text("not posted (open the site's sign-in page in the browser first)");
    };
    if !super::fill::secure_url(&url) {
        return text(
            "not posted (this page is not secure (https), so a saved login would never be filled \
             here)",
        );
    }
    // A label or username that echoes a saved secret is dropped.
    let secrets = state.browser.filled_secrets();
    let clean = |v: Option<String>| v.filter(|s| super::hide_secrets(s, &secrets) == *s);
    let mut card = json!({
        "type": "save_login",
        "title": format!("Save a login for {host}"),
        "site": site,
        "host": host,
        "state": "pending",
        "owner_only": true,
        "turn_message_id": ctx.message_id,
    });
    if let Some(l) = clean(label) {
        card["label"] = l.into();
    }
    if let Some(u) = clean(username) {
        card["username"] = u.into();
    }
    let conv = ctx.conversation_id.clone();
    let made = state
        .db
        .call(move |c| crate::entities::insert_card(c, &conv, &card, "assistant"))
        .await;
    let msg = match made {
        Ok(m) => m,
        Err(e) => return error(format!("not posted ({e})")),
    };
    state.hub.to_users(
        &ctx.audience,
        json!({"type": "message.created", "conversation_id": msg.conversation_id, "message": msg}),
    );
    crate::info!("browser: posted a save-login card {}", msg.id);
    text(format!(
        "posted (a card in the chat asks the user to save a login for {host}; they type it there \
         and you never see it. Once they have saved it, fill_saved_login can use it.)"
    ))
}
