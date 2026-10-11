//! Filling saved logins and wallet cards into Iris's browser.
//!
//! The secret goes from the vault straight into the page as typed input
//! (`Input.insertText` over the browser's private pipe). It is never put in
//! a tool result, a message, or a log line, and the filled fields are
//! masked on screen and in snapshots (`page.js`), so the agent cannot read
//! it back. Logins are only filled into the site they were saved for, over
//! HTTPS (plain HTTP only on this machine). Cards always need the owner's
//! OK on an approval card in the chat first.

use super::tools::{Output, eval, eval_mut};
use super::{Running, TurnCtx};
use crate::browser::Browser;
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

/// This machine: `localhost` (and its reserved subdomains) or a loopback
/// IP literal. Never a DNS name that merely starts with "127.".
fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Secrets are only typed into HTTPS pages (or plain HTTP on this machine).
pub fn secure_enough(scheme: &str, host: &str) -> bool {
    scheme == "https" || (scheme == "http" && is_loopback(host))
}

/// The page a fill was checked against (its address was validated when
/// the fill began): secrets go only into that tab's same document, whose
/// origin cannot change. A new page load is a new document.
struct Pin {
    target: String,
    doc: String,
    origin: String,
}

struct PageInfo {
    url: String,
    title: String,
    scheme: String,
    host: String,
    /// The browser's own origin (scheme, host, and port).
    origin: String,
    doc: String,
}

async fn page_info(run: &Running) -> Result<PageInfo> {
    let info = eval(run, "info()").await?;
    let url = info["url"].as_str().unwrap_or("").to_string();
    let (scheme, host) = origin_of(&url).ok_or("this page has no web address")?;
    Ok(PageInfo {
        title: info["title"].as_str().unwrap_or("").to_string(),
        doc: info["doc"].as_str().unwrap_or("").to_string(),
        origin: info["origin"].as_str().unwrap_or("").to_string(),
        url,
        scheme,
        host,
    })
}

const PAGE_CHANGED: &str = "the page changed while filling, so I stopped";
const TOO_SHORT: &str =
    "the saved secret is too short to keep out of what I read; ask the user to take over";

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

/// A credential's metadata and decrypted secret, read together.
async fn load(
    state: &AppState,
    user_id: &str,
    id: &str,
) -> Result<(Credential, serde_json::Map<String, Value>)> {
    let (uid, cid) = (user_id.to_string(), id.to_string());
    let (cred, ct) = state
        .db
        .call(move |c| {
            Ok((
                entities::get_credential(c, &uid, &cid)?,
                entities::get_credential_ciphertext(c, &uid, &cid)?,
            ))
        })
        .await?;
    let (Some(cred), Some(ct)) = (cred, ct) else {
        return Err("that saved item is gone".into());
    };
    let plain = state.vault.decrypt(&ct, id)?;
    Ok((cred, serde_json::from_slice(&plain).unwrap_or_default()))
}

/// Fill the field `r` with `text`, in one step bound to that element (see
/// `fillField` in page.js). Returns false if the field is not usable, and an
/// error if the page changed, the turn ended, or the owner took over:
/// nothing is filled then. Secrets are masked first and remembered for
/// redaction.
async fn type_into(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    pin: &Pin,
    r: &str,
    text: &str,
    secret: bool,
) -> Result<bool> {
    if run.page().target_id != pin.target {
        return Err(PAGE_CHANGED.into());
    }
    let b = &state.browser;
    if secret {
        if text.chars().count() < super::MIN_SECRET {
            return Err(TOO_SHORT.into());
        }
        // Recorded (on disk, encrypted where secret) before the page gets
        // it, or not filled at all: redaction and the screenshot block
        // must survive restarts.
        run.remember_secret(text);
        run.remember_secret_doc(&pin.doc);
        save_history(state, run)?;
        b.taint_origin(&pin.origin)?;
    }
    // Checked again right before the single command that fills.
    let p = eval_mut(
        run,
        b,
        ctx,
        &format!(
            "fillField({}, {}, {secret}, {})",
            json!(r),
            json!(text),
            json!(pin.doc)
        ),
    )
    .await?;
    if p["changed"] == true {
        return Err(PAGE_CHANGED.into());
    }
    Ok(p["ok"] == true)
}

/// Pick a dropdown option (not secret), on the pinned page only.
async fn choose(
    b: &Browser,
    ctx: &TurnCtx,
    run: &Running,
    pin: &Pin,
    r: &str,
    options: Value,
) -> Result<bool> {
    if run.page().target_id != pin.target {
        return Err(PAGE_CHANGED.into());
    }
    let info = eval(run, "info()").await?;
    if info["doc"].as_str() != Some(pin.doc.as_str()) {
        return Err(PAGE_CHANGED.into());
    }
    let r = eval_mut(run, b, ctx, &format!("choose({}, {options})", json!(r))).await?;
    Ok(r["ok"] == true)
}

fn not_found_for(e: crate::util::BoxError) -> Result<Output> {
    let msg = e.to_string();
    if msg == PAGE_CHANGED || msg == TOO_SHORT {
        Ok(Output::text(format!("not found ({msg})")))
    } else {
        Err(e)
    }
}

/// Every secret the redactor must know: the owner's saved passwords,
/// card numbers, and CVCs, and the encrypted history of every secret ever
/// filled (so edited or deleted ones too). Blocking. An unreadable history
/// is an error: the browser then stays off rather than read pages blind.
pub fn load_secrets_blocking(
    db: &crate::db::Db,
    vault: &crate::secrets::Vault,
    history: &std::path::Path,
) -> Result<Vec<String>> {
    let rows = db.with(|c| {
        let Some(owner) = db::owner_user_id(c)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for cred in entities::list_credentials(c, &owner)? {
            if let Some(ct) = entities::get_credential_ciphertext(c, &owner, &cred.id)? {
                out.push((cred.id, ct));
            }
        }
        Ok(out)
    })?;
    let mut secrets = Vec::new();
    for (id, ct) in rows {
        let plain = vault.decrypt(&ct, &id)?;
        let fields: serde_json::Map<String, Value> =
            serde_json::from_slice(&plain).unwrap_or_default();
        // CVCs too: a page may show one it kept, even after a restart (the
        // cost is that the same three digits elsewhere are hidden as well).
        for k in ["password", "number", "cvc"] {
            if let Some(v) = fields.get(k).and_then(Value::as_str) {
                secrets.push(v.to_string());
            }
        }
    }
    match std::fs::read(history) {
        Ok(ct) => {
            let plain = vault.decrypt(&ct, HISTORY_AAD)?;
            let list: Vec<String> = serde_json::from_slice(&plain)?;
            secrets.extend(list);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(secrets)
}

/// File of the encrypted history (see `load_secrets_blocking`).
pub fn history_path(paths: &crate::config::Paths) -> std::path::PathBuf {
    paths.browser_dir().join("redaction.enc")
}

pub const HISTORY_AAD: &str = "browser-redaction-history";

fn history_file(state: &AppState) -> std::path::PathBuf {
    history_path(&state.paths)
}

/// Every secret Iris ever filled (also ones since edited or deleted),
/// encrypted with the vault: pages may show them in later sessions.
fn save_history(state: &AppState, run: &Running) -> Result<()> {
    let body = serde_json::to_vec(&run.originals())?;
    let ct = state.vault.encrypt(&body, HISTORY_AAD)?;
    let file = history_file(state);
    if let Some(dir) = file.parent() {
        crate::config::ensure_private_dir(dir)?;
    }
    crate::config::write_private_file(&file, &ct)
}

/// `fill_login`: answers "filled" or "not found (...)".
pub async fn login(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    site: Option<&str>,
) -> Result<Output> {
    let page = page_info(run).await?;
    let pinned_target = run.page().target_id;
    let host = page.host.clone();
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
    if !secure_enough(&page.scheme, &host) {
        return Ok(Output::text(
            "not found (this page is not secure, so saved logins are not filled here)",
        ));
    }
    let mut matches: Vec<(Credential, String)> = credentials(state, &ctx.user_id, "login")
        .await
        .into_iter()
        .filter_map(|c| {
            let site = c.metadata["site"].as_str().and_then(site_host)?;
            host_matches(&host, &site).then_some((c, site))
        })
        .collect();
    matches.sort_by_key(|(c, _)| std::cmp::Reverse(c.last_used_at.unwrap_or(c.updated_at)));
    let Some((cred, _)) = matches.into_iter().next() else {
        return Ok(Output::text("not found (no saved login for this site)"));
    };
    // The site check uses the very revision whose password is filled.
    let (cred, secret) = load(state, &ctx.user_id, &cred.id).await?;
    if !cred.metadata["site"]
        .as_str()
        .and_then(site_host)
        .is_some_and(|s| host_matches(&host, &s))
    {
        return Ok(Output::text("not found (no saved login for this site)"));
    }
    let pin = Pin {
        target: pinned_target,
        doc: page.doc,
        origin: page.origin,
    };
    let fields = eval(run, "loginFields()").await?;
    let (user_ref, pass_ref) = (fields["user"].as_str(), fields["pass"].as_str());
    let username = cred.metadata["username"].as_str().unwrap_or("").to_string();
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
        match type_into(state, ctx, run, &pin, r, &username, false).await {
            Ok(f) => filled |= f,
            Err(e) => return not_found_for(e),
        }
    }
    if let Some(r) = pass_ref {
        // A password box is there: the sign-in is only filled if it is.
        let ok = !password.is_empty()
            && match type_into(state, ctx, run, &pin, r, &password, true).await {
                Ok(f) => f,
                Err(e) => return not_found_for(e),
            };
        if !ok {
            return Ok(Output::text(if password.is_empty() {
                "not found (no password is saved for this site)"
            } else {
                "not found (the password field would not take it; ask the user to take over)"
            }));
        }
        filled = true;
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

/// Whether an earlier approval card still answers this request: same card
/// revision (an edit needs a new OK), not expired, and not used yet.
fn reusable(c: &Value, created_at: i64, revision: i64, now: i64) -> bool {
    if c["wallet"]["revision"].as_i64() != Some(revision) {
        return false;
    }
    let fresh = now - created_at < APPROVAL_TTL_MS;
    let unused = c["wallet"]["used_at"].is_null();
    match c["state"].as_str() {
        Some("pending") => fresh,
        Some("approved") => fresh && unused,
        Some("denied") => now - c["decided_at"].as_i64().unwrap_or(0) < 2 * 60 * 1000,
        _ => false,
    }
}

/// `fill_card`: answers "filled", "denied", "waiting for approval", or
/// "not found (...)".
pub async fn card(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    hint: Option<&str>,
) -> Result<Output> {
    let b = &state.browser;
    if ctx.audience.is_empty() {
        return Ok(Output::text(
            "not found (cards can only be used in a chat, where the user can approve)",
        ));
    }
    let page = page_info(run).await?;
    let pinned_target = run.page().target_id;
    if !secure_enough(&page.scheme, &page.host) {
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
    // Approvals bind to the browser's exact origin (scheme, host, port).
    let origin = page.origin.clone();
    if origin.is_empty() || origin == "null" || run.shows_secret(&origin) {
        return Ok(Output::text(
            "not found (this page's address cannot be used for a card)",
        ));
    }

    // The owner approves every use of a card, for this card as it is now.
    let revision = card.updated_at;
    let mut approval = latest_approval(state, &ctx.conversation_id, &card.id, &origin).await;
    if !approval.as_ref().is_some_and(|m| {
        m.card
            .as_ref()
            .is_some_and(|c| reusable(c, m.created_at, revision, now_ms()))
    }) {
        approval = Some(ask_approval(state, ctx, run, card, &page, &origin).await?);
    }
    let msg = approval.expect("set above");
    match wait_decision(state, ctx, &msg.id).await.as_deref() {
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
    // While waiting, the owner may have taken over or the page moved on.
    b.check_agent(ctx)?;
    let (card, secret) = load(state, &ctx.user_id, &card.id).await?;
    if card.updated_at != revision {
        return Ok(Output::text(
            "waiting for approval (the card was edited since the user approved; call \
             fill_card again to ask)",
        ));
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
            // Still approved, unused, unexpired, and for this revision.
            if card["state"] != "approved" || !reusable(&card, m.created_at, revision, now_ms()) {
                return Ok(false);
            }
            card["wallet"]["used_at"] = now_ms().into();
            db::update_message_card(c, &mid, &card).map(|_| true)
        })
        .await?;
    if !consumed {
        return Ok(Output::text(
            "waiting for approval (that OK was already used or has expired; call fill_card \
             again to ask)",
        ));
    }
    let pin = Pin {
        target: pinned_target,
        doc: page.doc.clone(),
        origin: page.origin.clone(),
    };
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
    let result = fill_card_fields(state, ctx, run, &pin, &fields, &card, &number, &cvc).await;
    drop((number, cvc));
    let missing = match result {
        Ok(Some(missing)) => missing,
        Ok(None) => {
            return Ok(Output::text(
                "not found (the card number field could not be filled)",
            ));
        }
        Err(e) => return not_found_for(e),
    };
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
    if missing.is_empty() {
        Ok(Output::text("filled"))
    } else {
        Ok(Output::text(format!(
            "filled (but the {} could not be filled in; check the form, or ask the user to \
             take over for it)",
            missing.join(" and ")
        )))
    }
}

/// Fill the card form. None if the number could not be filled (then
/// nothing else is); otherwise the parts that could not be filled.
#[allow(clippy::too_many_arguments)]
async fn fill_card_fields(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    pin: &Pin,
    fields: &Value,
    card: &Credential,
    number: &str,
    cvc: &str,
) -> Result<Option<Vec<&'static str>>> {
    let b = &state.browser;
    let m = &card.metadata;
    let Some(r) = fields["number"].as_str() else {
        return Ok(None);
    };
    if !type_into(state, ctx, run, pin, r, number, true).await? {
        return Ok(None);
    }
    let mut missing = Vec::new();
    if let Some(r) = fields["cvc"].as_str() {
        if cvc.is_empty() || !type_into(state, ctx, run, pin, r, cvc, true).await? {
            missing.push("security code");
        }
    }
    let month = m["exp_month"].as_u64().map(|v| format!("{v:02}"));
    let year = m["exp_year"].as_u64().map(|v| v.to_string());
    let has_exp_field =
        fields["exp"].is_string() || (fields["month"].is_string() && fields["year"].is_string());
    if let (Some(mm), Some(yyyy)) = (&month, &year) {
        let yy = &yyyy[yyyy.len().saturating_sub(2)..];
        let selects = &fields["selects"];
        let mut ok = true;
        if let Some(r) = fields["exp"].as_str() {
            ok &= if selects["exp"].as_bool() == Some(true) {
                let opts = json!([format!("{mm}/{yy}"), format!("{mm}/{yyyy}")]);
                choose(b, ctx, run, pin, r, opts).await?
            } else {
                type_into(state, ctx, run, pin, r, &format!("{mm}/{yy}"), false).await?
            };
        }
        if let (Some(rm), Some(ry)) = (fields["month"].as_str(), fields["year"].as_str()) {
            ok &= if selects["month"].as_bool() == Some(true) {
                let n = mm.trim_start_matches('0');
                choose(b, ctx, run, pin, rm, json!([mm, n])).await?
            } else {
                type_into(state, ctx, run, pin, rm, mm, false).await?
            };
            ok &= if selects["year"].as_bool() == Some(true) {
                choose(b, ctx, run, pin, ry, json!([yyyy, yy])).await?
            } else {
                let year = if fields["shortYear"] == true {
                    yy
                } else {
                    yyyy
                };
                type_into(state, ctx, run, pin, ry, year, false).await?
            };
        }
        if has_exp_field && !ok {
            missing.push("expiry date");
        }
    } else if has_exp_field {
        missing.push("expiry date");
    }
    if let Some(r) = fields["name"].as_str() {
        match m["cardholder"].as_str().filter(|n| !n.is_empty()) {
            Some(name) => {
                if !type_into(state, ctx, run, pin, r, name, false).await? {
                    missing.push("name on the card");
                }
            }
            None => missing.push("name on the card"),
        }
    }
    Ok(Some(missing))
}

/// Post an approval card (built from the real page and card, never from
/// agent text) and notify the owner if they are away.
async fn ask_approval(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    card: &Credential,
    page: &PageInfo,
    origin: &str,
) -> Result<db::Message> {
    let name = card_name(card);
    // Page-derived text is stored: a page may have echoed an earlier secret.
    let (url, page_title) = (run.redact(&page.url), run.redact(&page.title));
    let host = &run.redact(&page.host);
    let exp = match (
        card.metadata["exp_month"].as_u64(),
        card.metadata["exp_year"].as_u64(),
    ) {
        (Some(m), Some(y)) => format!(", expires {m:02}/{}", y % 100),
        _ => String::new(),
    };
    let preview = format!(
        "Fill your {name}{exp} (\"{}\") into the payment form on this page:\n{}\n\n\
         Approving lets me fill it in this once, to finish what you asked for.",
        card.label, url
    );
    let title_text = format!("Use your {name} on {host}");
    let mut value = entities::approval_card(&entities::ApprovalRequest {
        title: &title_text,
        service: "wallet",
        to: host,
        subject: &page_title,
        preview: &preview,
        action_label: "Fill card",
        approver_user_id: &ctx.user_id,
    });
    value["wallet"] = json!({"credential_id": card.id, "origin": origin,
                             "revision": card.updated_at, "used_at": null});
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
/// None if still pending or the turn ended).
async fn wait_decision(state: &AppState, ctx: &TurnCtx, message_id: &str) -> Option<String> {
    let deadline = tokio::time::Instant::now() + APPROVAL_WAIT;
    loop {
        if ctx.is_ended() {
            return None;
        }
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
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    _ = ctx.wait_ended() => return None,
                }
            }
            _ => return None,
        }
    }
}

/// The owner approved a card after `fill_card` stopped waiting (the turn
/// may even be over). Once no turn runs in that chat and the approval is
/// still unused, continue with a short message from the owner, so Iris
/// picks it up (`fill_card` then finds the approval).
pub fn resume_after_approval(state: AppState, message_id: String, conv: String, approver: String) {
    // Pressing Stop after approving means: do not carry on.
    let stops = state.browser.stop_count(&conv);
    tokio::spawn(async move {
        // Give a waiting fill_card its chance to take it first.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15 * 60);
        loop {
            let busy = state
                .running
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&conv);
            if !busy {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let id = message_id.clone();
        let card = state
            .db
            .call(move |c| db::get_message(c, &id))
            .await
            .ok()
            .flatten();
        let still_open = card.is_some_and(|m| {
            m.card.as_ref().is_some_and(|c| {
                c["state"] == "approved"
                    && c["wallet"]["used_at"].is_null()
                    && now_ms() - m.created_at < APPROVAL_TTL_MS
            })
        });
        if !still_open || state.browser.stop_count(&conv) != stops {
            return;
        }
        if let Err(e) = crate::turn::start_chat_turn(
            &state,
            &conv,
            Some(&approver),
            "I approved the card. Go ahead.",
            &[],
            crate::router::Purpose::Chat,
            &approver,
        )
        .await
        {
            crate::warn!("browser: could not continue after card approval {message_id}: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_history_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let state = crate::browser::tools::tests::test_state(tmp.path());
        let file = history_path(&state.paths);
        // No history yet: fine.
        assert!(load_secrets_blocking(&state.db, &state.vault, &file).is_ok());
        // A history that cannot be read: the browser must not start blind.
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"garbage").unwrap();
        assert!(load_secrets_blocking(&state.db, &state.vault, &file).is_err());
        // A good one round-trips.
        let ct = state
            .vault
            .encrypt(br#"["old-password-1"]"#, HISTORY_AAD)
            .unwrap();
        std::fs::write(&file, ct).unwrap();
        let list = load_secrets_blocking(&state.db, &state.vault, &file).unwrap();
        assert!(list.contains(&"old-password-1".to_string()));
    }

    #[test]
    fn approvals_are_bound_to_revision_age_and_single_use() {
        let now = 10_000_000;
        let card = |state: &str, revision: i64, used: Value| {
            json!({"state": state, "decided_at": now - 1000,
                   "wallet": {"revision": revision, "used_at": used}})
        };
        let recent = now - 1000;
        assert!(reusable(&card("approved", 5, Value::Null), recent, 5, now));
        // Edited card, used approval, expired approval: ask again.
        assert!(!reusable(&card("approved", 4, Value::Null), recent, 5, now));
        assert!(!reusable(&card("approved", 5, json!(1)), recent, 5, now));
        let old = now - APPROVAL_TTL_MS - 1;
        assert!(!reusable(&card("approved", 5, Value::Null), old, 5, now));
        assert!(reusable(&card("pending", 5, Value::Null), recent, 5, now));
        // A fresh "no" stands for a little while.
        assert!(reusable(&card("denied", 5, Value::Null), recent, 5, now));
    }

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
        // DNS names that only look like loopback are remote.
        for host in [
            "127.example.com",
            "127.0.0.1.example.com",
            "localhost.example.com",
        ] {
            assert!(!secure_enough("http", host), "{host}");
        }
        assert!(secure_enough("http", "127.8.9.10"));
        assert!(secure_enough("http", "::1"));
        assert!(secure_enough("http", "app.localhost"));
    }
}
