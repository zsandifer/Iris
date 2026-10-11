//! Filling saved logins and wallet cards into Iris's browser.
//!
//! The secret goes from the vault straight into the page as typed input
//! (`Input.insertText` over the browser's private pipe). It is never put in
//! a tool result, a message, or a log line, and the filled fields are
//! masked on screen and in snapshots (`page.js`), so the agent cannot read
//! it back. Logins are only filled into the site they were saved for, over
//! HTTPS (plain HTTP only on this machine). Cards always need the owner's
//! OK on an approval card in the chat first.

use super::tools::{Output, eval};
use super::{Running, TurnCtx};
use crate::db;
use crate::entities::{self, Credential};
use crate::state::AppState;
use crate::util::{Result, now_ms};
use serde_json::{Value, json};
use std::time::Duration;

/// How long `fill_card` waits for the owner's decision in one call.
#[cfg(not(test))]
const APPROVAL_WAIT: Duration = Duration::from_secs(45);
#[cfg(test)]
const APPROVAL_WAIT: Duration = Duration::from_secs(3);
/// An approval is good for one fill within this time.
const APPROVAL_TTL_MS: i64 = 30 * 60 * 1000;

/// (scheme, host) of a URL, lowercased; host without port or `www.`.
pub fn origin_of(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let hostport = authority.rsplit('@').next()?;
    let host = if let Some(v6) = hostport.strip_prefix('[') {
        v6.split(']').next()?.to_string()
    } else {
        hostport.split(':').next()?.to_string()
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    (!host.is_empty()).then(|| (scheme.to_ascii_lowercase(), host))
}

/// A saved site ("example.com", "https://www.example.com/login") as a host.
pub fn site_host(site: &str) -> Option<String> {
    let s = site.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains("://") {
        return origin_of(s).map(|(_, h)| h);
    }
    origin_of(&format!("https://{s}")).map(|(_, h)| h)
}

/// The page's host is the saved site or one of its subdomains.
pub fn host_matches(page_host: &str, site: &str) -> bool {
    page_host == site
        || page_host
            .strip_suffix(site)
            .is_some_and(|rest| rest.ends_with('.'))
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host == "::1" || host.starts_with("127.")
}

/// Secrets are only typed into HTTPS pages (or plain HTTP on this machine).
pub fn secure_enough(scheme: &str, host: &str) -> bool {
    scheme == "https" || (scheme == "http" && is_loopback(host))
}

async fn page_origin(run: &Running) -> Result<(String, String, String, String)> {
    let info = eval(run, "info()").await?;
    let url = info["url"].as_str().unwrap_or("").to_string();
    let title = info["title"].as_str().unwrap_or("").to_string();
    let (scheme, host) = origin_of(&url).ok_or("this page has no web address")?;
    Ok((url, title, scheme, host))
}

async fn credentials(state: &AppState, user_id: &str, kind: &'static str) -> Vec<Credential> {
    let uid = user_id.to_string();
    state
        .db
        .call(move |c| entities::list_credentials(c, &uid))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.kind == kind)
        .collect()
}

async fn secret_fields(
    state: &AppState,
    user_id: &str,
    id: &str,
) -> Result<serde_json::Map<String, Value>> {
    let (uid, cid) = (user_id.to_string(), id.to_string());
    let ct = state
        .db
        .call(move |c| entities::get_credential_ciphertext(c, &uid, &cid))
        .await?
        .ok_or("credential is gone")?;
    let plain = state.vault.decrypt(&ct, id)?;
    Ok(serde_json::from_slice(&plain).unwrap_or_default())
}

/// Type `text` into the field `r` (replacing its content).
async fn type_into(run: &Running, r: &str, text: &str, mask: bool) -> Result<bool> {
    let f = eval(run, &format!("focus({})", json!(r))).await?;
    if f["error"].is_string() {
        return Ok(false);
    }
    let page = run.page();
    if mask {
        // Mask before typing so no frame of the live view shows it.
        eval(run, &format!("mask({})", json!(r))).await?;
    }
    run.cdp
        .call(
            "Input.insertText",
            json!({"text": text}),
            Some(page.session_id.as_str()),
        )
        .await?;
    Ok(true)
}

/// `fill_login`: answers "filled" or "not found (...)".
pub async fn login(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    site: Option<&str>,
) -> Result<Output> {
    let (_, _, scheme, host) = page_origin(run).await?;
    if let Some(want) = site.filter(|s| !s.trim().is_empty()) {
        match site_host(want) {
            Some(w) if host_matches(&host, &w) => {}
            _ => {
                return Ok(Output::text(
                    "not found (the current page is not that site; open its sign-in page first)",
                ));
            }
        }
    }
    if !secure_enough(&scheme, &host) {
        return Ok(Output::text(
            "not found (this page is not secure, so saved logins are not filled here)",
        ));
    }
    let mut matches: Vec<Credential> = credentials(state, &ctx.user_id, "login")
        .await
        .into_iter()
        .filter(|c| {
            c.metadata["site"]
                .as_str()
                .and_then(site_host)
                .is_some_and(|s| host_matches(&host, &s))
        })
        .collect();
    matches.sort_by_key(|c| std::cmp::Reverse(c.last_used_at.unwrap_or(c.updated_at)));
    let Some(cred) = matches.into_iter().next() else {
        return Ok(Output::text("not found (no saved login for this site)"));
    };
    let fields = eval(run, "loginFields()").await?;
    let (user_ref, pass_ref) = (fields["user"].as_str(), fields["pass"].as_str());
    let username = cred.metadata["username"].as_str().unwrap_or("").to_string();
    let secret = secret_fields(state, &ctx.user_id, &cred.id).await?;
    let password = secret
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    drop(secret);
    let mut filled = false;
    if let Some(r) = user_ref
        && !username.is_empty()
    {
        filled |= type_into(run, r, &username, false).await?;
    }
    if let Some(r) = pass_ref
        && !password.is_empty()
    {
        filled |= type_into(run, r, &password, true).await?;
    }
    drop(password);
    if !filled {
        return Ok(Output::text("not found (no sign-in field on this page)"));
    }
    let (uid, cid) = (ctx.user_id.clone(), cred.id.clone());
    let _ = state
        .db
        .call(move |c| entities::mark_credential_used(c, &uid, &cid))
        .await;
    crate::info!("browser: filled saved login {}", cred.id);
    Ok(Output::text("filled"))
}

fn card_name(c: &Credential) -> String {
    let brand = c.metadata["brand"].as_str().unwrap_or("card");
    let mut b = brand.to_string();
    if let Some(f) = b.get_mut(..1) {
        f.make_ascii_uppercase();
    }
    format!(
        "{b} ending {}",
        c.metadata["last4"].as_str().unwrap_or("????")
    )
}

/// The most recent approval card for this card on this site.
async fn latest_approval(
    state: &AppState,
    conv: &str,
    cred_id: &str,
    origin: &str,
) -> Option<db::Message> {
    let (conv, cid, origin) = (conv.to_string(), cred_id.to_string(), origin.to_string());
    state
        .db
        .call(move |c| {
            let id: Option<String> = c
                .query_row(
                    "SELECT id FROM messages
                     WHERE conversation_id = ?1 AND kind = 'card'
                       AND json_extract(card, '$.type') = 'approval'
                       AND json_extract(card, '$.wallet.credential_id') = ?2
                       AND json_extract(card, '$.wallet.origin') = ?3
                     ORDER BY created_at DESC LIMIT 1",
                    rusqlite::params![conv, cid, origin],
                    |r| r.get(0),
                )
                .ok();
            match id {
                Some(id) => db::get_message(c, &id),
                None => Ok(None),
            }
        })
        .await
        .ok()
        .flatten()
}

/// `fill_card`: answers "filled", "denied", "waiting for approval", or
/// "not found (...)".
pub async fn card(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    hint: Option<&str>,
) -> Result<Output> {
    if ctx.audience.is_empty() {
        return Ok(Output::text(
            "not found (cards can only be used in a chat, where the user can approve)",
        ));
    }
    let (url, title, scheme, host) = page_origin(run).await?;
    if !secure_enough(&scheme, &host) {
        return Ok(Output::text(
            "not found (this page is not secure, so cards are not filled here)",
        ));
    }
    let cards = credentials(state, &ctx.user_id, "card").await;
    let hint = hint.map(str::trim).filter(|h| !h.is_empty());
    let chosen: Vec<&Credential> = match hint {
        Some(h) => {
            let digits: String = h.chars().filter(char::is_ascii_digit).collect();
            let low = h.to_lowercase();
            cards
                .iter()
                .filter(|c| {
                    c.label.to_lowercase().contains(&low)
                        || (digits.len() == 4 && c.metadata["last4"].as_str() == Some(&digits))
                })
                .collect()
        }
        None => cards.iter().collect(),
    };
    let card = match chosen.as_slice() {
        [] => return Ok(Output::text("not found (no saved card like that)")),
        [one] => *one,
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|c| format!("\"{}\" ({})", c.label, card_name(c)))
                .collect();
            return Ok(Output::error(format!(
                "Several cards are saved: {}. Call fill_card again with `card` set to one of \
                 the names or the last 4 digits.",
                names.join(", ")
            )));
        }
    };
    let fields = eval(run, "cardFields()").await?;
    if fields["number"].as_str().is_none() {
        return Ok(Output::text(
            "not found (no card number field on this page)",
        ));
    }
    let origin = format!("{scheme}://{host}");

    // The owner approves every use of a card.
    let mut approval = latest_approval(state, &ctx.conversation_id, &card.id, &origin).await;
    let reusable = |m: &db::Message| {
        let c = m.card.as_ref().cloned().unwrap_or_default();
        let fresh = now_ms() - m.created_at < APPROVAL_TTL_MS;
        let unused = c["wallet"]["used_at"].is_null();
        match c["state"].as_str() {
            Some("pending") => fresh,
            Some("approved") => fresh && unused,
            Some("denied") => now_ms() - c["decided_at"].as_i64().unwrap_or(0) < 2 * 60 * 1000,
            _ => false,
        }
    };
    if !approval.as_ref().is_some_and(reusable) {
        approval = Some(ask_approval(state, ctx, card, &url, &title, &host, &origin).await?);
    }
    let msg = approval.expect("set above");
    let decided = wait_decision(state, &msg.id).await;
    match decided.as_deref() {
        Some("approved") => {}
        Some("denied") => {
            return Ok(Output::text(
                "denied (the user said no; do not use the card or try another way)",
            ));
        }
        _ => {
            return Ok(Output::text(
                "waiting for approval (the user was asked in the chat; tell them, and call \
                 fill_card again once they approve)",
            ));
        }
    }
    // Consume the approval before typing, so one OK fills once.
    let mid = msg.id.clone();
    let consumed = state
        .db
        .call(move |c| {
            let Some(m) = db::get_message(c, &mid)? else {
                return Ok(false);
            };
            let mut card = m.card.unwrap_or_default();
            if card["state"] != "approved" || !card["wallet"]["used_at"].is_null() {
                return Ok(false);
            }
            card["wallet"]["used_at"] = now_ms().into();
            db::update_message_card(c, &mid, &card).map(|_| true)
        })
        .await?;
    if !consumed {
        return Ok(Output::text(
            "waiting for approval (that OK was already used; call fill_card again to ask)",
        ));
    }
    let secret = secret_fields(state, &ctx.user_id, &card.id).await?;
    let number = secret
        .get("number")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let cvc = secret
        .get("cvc")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    drop(secret);
    let m = &card.metadata;
    let month = m["exp_month"].as_u64().map(|v| format!("{v:02}"));
    let year = m["exp_year"].as_u64().map(|v| v.to_string());
    let mut filled = false;
    if let Some(r) = fields["number"].as_str() {
        filled |= type_into(run, r, &number, true).await?;
    }
    if let (Some(r), false) = (fields["cvc"].as_str(), cvc.is_empty()) {
        type_into(run, r, &cvc, true).await?;
    }
    drop((number, cvc));
    if let (Some(mm), Some(yyyy)) = (&month, &year) {
        let yy = &yyyy[yyyy.len().saturating_sub(2)..];
        let selects = &fields["selects"];
        if let Some(r) = fields["exp"].as_str() {
            if selects["exp"].as_bool() == Some(true) {
                let opts = json!([format!("{mm}/{yy}"), format!("{mm}/{yyyy}")]);
                eval(run, &format!("choose({}, {opts})", json!(r))).await?;
            } else {
                type_into(run, r, &format!("{mm}/{yy}"), false).await?;
            }
        }
        if let (Some(rm), Some(ry)) = (fields["month"].as_str(), fields["year"].as_str()) {
            if selects["month"].as_bool() == Some(true) {
                let n = mm.trim_start_matches('0');
                eval(run, &format!("choose({}, {})", json!(rm), json!([mm, n]))).await?;
            } else {
                type_into(run, rm, mm, false).await?;
            }
            if selects["year"].as_bool() == Some(true) {
                eval(
                    run,
                    &format!("choose({}, {})", json!(ry), json!([yyyy, yy])),
                )
                .await?;
            } else {
                type_into(run, ry, yyyy, false).await?;
            }
        }
    }
    if let (Some(r), Some(name)) = (fields["name"].as_str(), m["cardholder"].as_str())
        && !name.is_empty()
    {
        type_into(run, r, name, false).await?;
    }
    if !filled {
        return Ok(Output::text(
            "not found (the card number field could not be filled)",
        ));
    }
    let (uid, cid) = (ctx.user_id.clone(), card.id.clone());
    let _ = state
        .db
        .call(move |c| entities::mark_credential_used(c, &uid, &cid))
        .await;
    crate::info!(
        "browser: filled wallet card {} after approval {}",
        card.id,
        msg.id
    );
    Ok(Output::text("filled"))
}

/// Post an approval card (built from the real page and card, never from
/// agent text) and notify the owner if they are away.
async fn ask_approval(
    state: &AppState,
    ctx: &TurnCtx,
    card: &Credential,
    url: &str,
    title: &str,
    host: &str,
    origin: &str,
) -> Result<db::Message> {
    let name = card_name(card);
    let exp = match (
        card.metadata["exp_month"].as_u64(),
        card.metadata["exp_year"].as_u64(),
    ) {
        (Some(m), Some(y)) => format!(", expires {m:02}/{}", y % 100),
        _ => String::new(),
    };
    let preview = format!(
        "Fill your {name}{exp} (\"{}\") into the payment form on this page:\n{url}\n\n\
         Approving lets me fill it in this once, to finish what you asked for.",
        card.label
    );
    let title_text = format!("Use your {name} on {host}");
    let mut value = entities::approval_card(&entities::ApprovalRequest {
        title: &title_text,
        service: "wallet",
        to: host,
        subject: title,
        preview: &preview,
        action_label: "Fill card",
        approver_user_id: &ctx.user_id,
    });
    value["wallet"] = json!({"credential_id": card.id, "origin": origin, "used_at": null});
    let conv = ctx.conversation_id.clone();
    let msg = state
        .db
        .call(move |c| entities::insert_card(c, &conv, &value, "assistant"))
        .await?;
    state.hub.to_users(
        &ctx.audience,
        json!({"type": "message.created", "conversation_id": msg.conversation_id, "message": msg}),
    );
    if !state.hub.is_online(&ctx.user_id) {
        let title = state.agent_name().await;
        crate::notify::push_users(
            state,
            std::slice::from_ref(&ctx.user_id),
            json!({
                "title": title,
                "body": format!("Can I use your {name} on {host}? Tap to approve or deny."),
                "conversation_id": msg.conversation_id,
                "message_id": msg.id,
                "kind": "approval",
            }),
        );
    }
    crate::info!("browser: asked to approve card {} ({})", card.id, msg.id);
    Ok(msg)
}

/// Poll the approval card's state for a while ("approved"/"denied", or
/// None if still pending).
async fn wait_decision(state: &AppState, message_id: &str) -> Option<String> {
    let deadline = tokio::time::Instant::now() + APPROVAL_WAIT;
    loop {
        let id = message_id.to_string();
        let st = state
            .db
            .call(move |c| db::get_message(c, &id))
            .await
            .ok()
            .flatten()
            .and_then(|m| m.card)
            .and_then(|c| c["state"].as_str().map(String::from));
        match st.as_deref() {
            Some("approved") | Some("denied") => return st,
            Some("pending") if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_and_site_matching() {
        assert_eq!(
            origin_of("https://www.Shop.example:8443/cart?x=1").unwrap(),
            ("https".into(), "shop.example".into())
        );
        assert_eq!(
            origin_of("http://user@127.0.0.1:9000/login").unwrap().1,
            "127.0.0.1"
        );
        assert_eq!(origin_of("http://[::1]:80/").unwrap().1, "::1");
        assert!(origin_of("about:blank").is_none());
        assert_eq!(site_host("example.com").unwrap(), "example.com");
        assert_eq!(
            site_host("https://www.example.com/login").unwrap(),
            "example.com"
        );
        assert_eq!(site_host("  ").as_deref(), None);
        assert!(host_matches("example.com", "example.com"));
        assert!(host_matches("accounts.example.com", "example.com"));
        // No lookalike or parent-domain matches.
        assert!(!host_matches("badexample.com", "example.com"));
        assert!(!host_matches("example.com.evil.test", "example.com"));
        assert!(!host_matches("example.com", "accounts.example.com"));
        assert!(secure_enough("https", "example.com"));
        assert!(secure_enough("http", "127.0.0.1"));
        assert!(secure_enough("http", "localhost"));
        assert!(!secure_enough("http", "example.com"));
        assert!(!secure_enough("ftp", "localhost"));
    }
}
