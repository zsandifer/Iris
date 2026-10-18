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
                password) for the site open in your browser, in Logins & wallet, or lets you \
                use one they already saved. The site is taken from the current page; they type \
                the login in and you never see it. fill_saved_login posts this card by itself \
                when nothing is saved for a sign-in page; use this when they ask for help \
                saving a login. Then end your reply: the chat continues by itself once they \
                save it.",
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

/// Why no card was posted.
#[derive(Debug, PartialEq)]
pub enum NotPosted {
    /// A job or routine run: nobody would see the card.
    NoChat,
    /// No web page is open.
    NoSite,
    /// The page is not HTTPS, so a saved login would never be filled there.
    Insecure,
    Failed(String),
}

/// A card asking for a login, new or already waiting in this chat (with
/// the sites it names, for the agent: "shop.test and auth.test").
#[derive(Debug, PartialEq)]
pub enum Posted {
    New(String),
    Already(String),
}

/// Post a `save_login` card for the sites of `urls` (the page, then any
/// frame holding the fields; taken from the browser, never from the
/// agent) in the turn's chat, unless one for the same site is still
/// waiting there. The card opens Logins & wallet with them filled in, or
/// lets the owner use a login they already saved.
pub async fn post_save_login_card(
    state: &AppState,
    ctx: &TurnCtx,
    urls: &[&str],
    label: Option<String>,
    username: Option<String>,
) -> std::result::Result<Posted, NotPosted> {
    if ctx.audience.is_empty() {
        return Err(NotPosted::NoChat);
    }
    let mut sites: Vec<(String, String)> = Vec::new();
    for url in urls {
        let Some((site, host)) = super::fill::site_of(url) else {
            continue;
        };
        if !super::fill::secure_url(url) {
            return Err(NotPosted::Insecure);
        }
        if !sites.iter().any(|(s, _)| *s == site) {
            sites.push((site, host));
        }
    }
    let Some((site, host)) = sites.first().cloned() else {
        return Err(NotPosted::NoSite);
    };
    let also: Vec<String> = sites[1..].iter().map(|(s, _)| s.clone()).collect();
    let named = sites
        .iter()
        .map(|(_, h)| h.as_str())
        .collect::<Vec<_>>()
        .join(" and ");
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
    if !also.is_empty() {
        card["also_sites"] = json!(also);
    }
    if let Some(l) = clean(label) {
        card["label"] = l.into();
    }
    if let Some(u) = clean(username) {
        card["username"] = u.into();
    }
    let conv = ctx.conversation_id.clone();
    let made = state
        .db
        .call(move |c| {
            // One waiting card per site in a chat.
            let waiting = c
                .prepare(
                    "SELECT 1 FROM messages WHERE conversation_id = ?1 AND kind = 'card'
                       AND json_extract(card, '$.type') = 'save_login'
                       AND json_extract(card, '$.state') = 'pending'
                       AND json_extract(card, '$.site') = ?2",
                )?
                .exists(rusqlite::params![conv, site])?;
            if waiting {
                return Ok(None);
            }
            crate::entities::insert_card(c, &conv, &card, "assistant").map(Some)
        })
        .await;
    let msg = match made {
        Ok(Some(m)) => m,
        Ok(None) => return Ok(Posted::Already(named)),
        Err(e) => return Err(NotPosted::Failed(e.to_string())),
    };
    state.hub.to_users(
        &ctx.audience,
        json!({"type": "message.created", "conversation_id": msg.conversation_id, "message": msg}),
    );
    crate::info!("browser: posted a save-login card {}", msg.id);
    Ok(Posted::New(named))
}

/// `ask_to_save_login`: a `save_login` card for the current page's site.
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
    match post_save_login_card(state, ctx, &[url.as_str()], label, username).await {
        Ok(Posted::New(host)) => text(format!(
            "posted (a card in the chat asks the user to save a login for {host}; they type it in \
             Logins & wallet and you never see it. End your reply: the chat continues by itself \
             once they save it, and then fill_saved_login can use it.)"
        )),
        Ok(Posted::Already(host)) => text(format!(
            "not posted (a card asking the user to save a login for {host} is already in the \
             chat; wait for them)"
        )),
        Err(NotPosted::NoChat) => {
            text("not posted (only in a chat with the user, who saves it there)")
        }
        Err(NotPosted::NoSite) => {
            text("not posted (open the site's sign-in page in the browser first)")
        }
        Err(NotPosted::Insecure) => text(
            "not posted (this page is not secure (https), so a saved login would never be filled \
             here)",
        ),
        Err(NotPosted::Failed(e)) => error(format!("not posted ({e})")),
    }
}
