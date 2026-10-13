//! Filling saved logins and wallet cards, when the owner asks while in
//! control of Iris's browser.
//!
//! The owner taps a field on the page, then picks a saved item and which
//! of its fields to fill (`browser.input` with `kind: "fill"`). Iris types
//! it into the focused field through the browser server. The agent never
//! gets it: secrets travel from the vault to the page only, never into a
//! tool result, a message, or a log, and the page is closed to the agent
//! until it moves on. Logins are only filled into the site they were saved
//! for (the page's real address, and never into another site's frame on
//! it), cards and secrets only over HTTPS (plain HTTP only on this
//! machine).
//!
//! The agent fills them too, without ever seeing them: `fill_saved_login`
//! and `fill_saved_card` (Iris's own tools next to Playwright's) take the
//! element refs of the fields, and Iris types each value into its field
//! through Playwright on Iris's side. Logins go only into fields whose own
//! frame is the saved site; cards only over HTTPS and only after the owner
//! approves that use on a card in the chat. Filling a password or card
//! details closes the page to the agent (see `private_page`) until it moves
//! on; `submit` presses Enter in the last field so a sign-in can.

use super::{Running, TurnCtx};
use crate::db;
use crate::entities::{self, Credential};
use crate::state::AppState;
use crate::util::{Result, now_ms};
use serde_json::{Value, json};

/// An approval is good for one fill within this time.
const APPROVAL_TTL_MS: i64 = 30 * 60 * 1000;

/// Parse a web origin. Bare saved sites default to HTTPS, including its
/// default port; paths never broaden the authority being matched.
fn login_origin(site: &str) -> Option<(String, String, u16)> {
    let site = site.trim();
    let url = if site.contains("://") {
        site.to_string()
    } else {
        format!("https://{site}")
    };
    let uri: axum::http::Uri = url.parse().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = uri.authority()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let host = authority
        .host()
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let explicit_port = if authority.as_str().starts_with('[') {
        !authority.as_str().split_once(']')?.1.is_empty()
    } else {
        authority.as_str().contains(':')
    };
    let port = if explicit_port {
        authority.port_u16()?
    } else {
        default_port
    };
    Some((scheme, host, port))
}

pub fn origin_of(url: &str) -> Option<(String, String)> {
    if !url.contains("://") {
        return None;
    }
    let (scheme, host, _) = login_origin(url)?;
    Some((scheme, host))
}

pub fn site_matches(page_url: &str, site: &str, allow_subdomains: bool) -> bool {
    let (Some((ps, ph, pp)), Some((ss, sh, sp))) = (login_origin(page_url), login_origin(site))
    else {
        return false;
    };
    ps == ss
        && pp == sp
        && (ph == sh
            || (allow_subdomains
                && sh.parse::<std::net::IpAddr>().is_err()
                && ph
                    .strip_suffix(&sh)
                    .is_some_and(|prefix| prefix.ends_with('.'))))
}

fn credential_matches(page_url: &str, c: &Credential) -> bool {
    c.metadata["site"]
        .as_str()
        .is_some_and(|site| site_matches(page_url, site, c.metadata["allow_subdomains"] == true))
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

/// The value of one field of a saved item, and whether it is private
/// (closes the page to the agent).
fn value_of(
    cred: &Credential,
    secret: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<(String, bool)> {
    let s = |k: &str| {
        secret
            .get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    let m = |k: &str| {
        cred.metadata[k]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    let (value, private) = match (cred.kind.as_str(), field) {
        ("login", "username") => (m("username"), false),
        ("login", "password") => (s("password"), true),
        ("card", "number") => (s("number"), true),
        ("card", "cvc") => (s("cvc"), true),
        ("card", "exp") => (expiry(&cred.metadata), true),
        ("card", "name") => (m("cardholder"), false),
        _ => return Err("that can't be filled".into()),
    };
    value
        .map(|v| (v, private))
        .ok_or_else(|| "that isn't saved for this item".into())
}

/// A card's expiry as MM/YY.
fn expiry(meta: &Value) -> Option<String> {
    let month = meta["exp_month"]
        .as_i64()
        .filter(|m| (1..=12).contains(m))?;
    let year = meta["exp_year"].as_i64().filter(|y| *y > 0)?;
    Some(format!("{month:02}/{:02}", year % 100))
}

/// Fill one field of a saved login or card into the focused field, for
/// the owner in control from `device_id`.
pub async fn fill(
    state: &AppState,
    device_id: &str,
    credential_id: &str,
    field: &str,
) -> Result<()> {
    let user_id = state.browser.controller(device_id)?;
    let (cred, secret) = load(state, &user_id, credential_id).await?;
    let (value, private) = value_of(&cred, &secret, field)?;
    let site = cred.metadata["site"].as_str().unwrap_or("");
    state
        .browser
        .fill_for_owner(
            device_id,
            |field| {
                let url = field.url.as_str();
                let (scheme, host) = origin_of(url).ok_or("Open the site's page first.")?;
                if !secure_enough(&scheme, &host) {
                    return Err("I only fill saved details into secure (https) pages.".into());
                }
                if cred.kind == "login" {
                    if !credential_matches(url, &cred) {
                        return Err(
                            format!("This login is saved for {site}, not this page.").into()
                        );
                    }
                    // The page's address says nothing about a frame from
                    // another site (an ad, a widget) that has focus.
                    if field.frame {
                        return Err(format!(
                            "That field belongs to another site inside this page, so I won't \
                             fill your {site} login there."
                        )
                        .into());
                    }
                }
                Ok(())
            },
            &value,
            private,
        )
        .await?;
    let (uid, cid) = (user_id, cred.id.clone());
    let _ = state
        .db
        .call(move |c| entities::mark_credential_used(c, &uid, &cid))
        .await;
    crate::info!("browser: the owner filled a saved {} field", cred.kind);
    Ok(())
}

// ------------------------------------------------------- agent fills

/// Iris's own tools for the agent, listed after Playwright's.
pub fn definitions() -> Vec<Value> {
    let r = |d: &str| json!({"type": "string", "description": d});
    vec![
        json!({
            "name": "fill_saved_login",
            "description": "Sign in with the user's saved login for this site. Give the refs \
                (from browser_snapshot) of the username and/or password fields; Iris types the \
                saved values in itself and you never see them. Set `submit` to press Enter in \
                the last field filled. After a password is filled you cannot read the page \
                until it moves on, so usually submit. Answers `filled`, or `not found (...)`.",
            "inputSchema": {"type": "object", "properties": {
                "username_ref": r("Ref of the username or email field"),
                "password_ref": r("Ref of the password field"),
                "login": r("Which saved login, by its name or username (only if several are saved for this site)"),
                "submit": {"type": "boolean", "description": "Press Enter after filling to sign in"},
            }},
        }),
        json!({
            "name": "fill_saved_card",
            "description": "Fill the payment form with a card from the user's wallet. The user \
                approves every use first (a card in the chat; tell them you are waiting for \
                their OK). Give the refs (from browser_snapshot) of the fields the form has; \
                Iris types the card in itself and you never see it. After filling you cannot \
                read the page until it moves on. Answers `filled`, `denied`, `not filled \
                (...)`, or `not found (...)`.",
            "inputSchema": {"type": "object", "properties": {
                "number_ref": r("Ref of the card number field"),
                "exp_ref": r("Ref of a single expiry field (MM/YY)"),
                "exp_month_ref": r("Ref of a separate expiry month field or list"),
                "exp_year_ref": r("Ref of a separate expiry year field or list"),
                "cvc_ref": r("Ref of the security code (CVC) field"),
                "name_ref": r("Ref of the name on card field"),
                "card": r("Which card, by its name or last 4 digits (only if several are saved)"),
                "submit": {"type": "boolean", "description": "Press Enter in the last field after filling"},
            }, "required": ["number_ref"]},
        }),
    ]
}

pub const TOOLS: &[&str] = &["fill_saved_login", "fill_saved_card"];

fn text(t: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": t.into()}]})
}

fn error(t: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": t.into()}], "isError": true})
}

/// A Playwright element ref (`e12`, or `f1e12` inside a frame).
fn valid_ref(r: &str) -> bool {
    let r = r.strip_prefix('f').map_or(Some(r), |rest| {
        let n = rest.find('e')?;
        (n > 0 && rest[..n].bytes().all(|b| b.is_ascii_digit())).then(|| &rest[n..])
    });
    r.and_then(|r| r.strip_prefix('e'))
        .is_some_and(|n| !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()))
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

/// One saved item of several, by the agent's hint (`None`: the only one).
fn choose<'a>(
    items: &'a [Credential],
    hint: Option<&str>,
    matches: impl Fn(&Credential, &str) -> bool,
) -> std::result::Result<&'a Credential, Vec<&'a Credential>> {
    let hint = hint.map(str::trim).filter(|h| !h.is_empty());
    let chosen: Vec<&Credential> = match hint {
        Some(h) => items.iter().filter(|c| matches(c, h)).collect(),
        None => items.iter().collect(),
    };
    match chosen.as_slice() {
        [one] => Ok(*one),
        _ => Err(chosen),
    }
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

/// Where a field is: the page's and the field's own frame's real
/// addresses (from Playwright's Node side), and what kind of element.
#[derive(Debug, Clone)]
struct Target {
    top: String,
    frame: String,
    tag: String,
}

/// Iris's fixed Playwright code for one field: find it by ref; with
/// `value` (candidates, the first that fits), check it is still where it
/// was (`expect`), mark the page private (`mark`), fill it, and press
/// Enter (`submit`). The values travel to Playwright only, inside this
/// code; the result never contains them.
fn field_code(
    target: &str,
    fill: Option<(&Target, &[String], bool, bool)>,
) -> std::result::Result<String, serde_json::Error> {
    let sel = serde_json::to_string(&format!("aria-ref={target}"))?;
    let find = format!(
        "const h = await page.locator({sel}).elementHandle({{ timeout: 3000 }}).catch(() => null);
  if (!h) return {{ found: false }};
  const frame = await h.ownerFrame();
  const info = {{ found: true, top: page.url(), frame: frame ? frame.url() : '',
    tag: await h.evaluate((e) => e.isContentEditable ? 'editable' : e.tagName.toLowerCase()),
    max: await h.evaluate((e) => e.maxLength > 0 ? e.maxLength : null) }};"
    );
    let Some((at, values, mark, submit)) = fill else {
        return Ok(format!("async (page) => {{\n  {find}\n  return info;\n}}"));
    };
    Ok(format!(
        "async (page) => {{
  {find}
  if (info.top !== {top} || info.frame !== {frame} || info.tag !== {tag}) return {{ moved: true }};
  if ({mark}) await page.evaluate({mark_js});
  const values = {values};
  let filled = false;
  if (info.tag === 'select') {{
    for (const v of values) {{
      if (await h.selectOption(v).then(() => true, () => false)) {{ filled = true; break; }}
      if (await h.selectOption({{ label: v }}).then(() => true, () => false)) {{ filled = true; break; }}
    }}
  }} else {{
    const v = values.find((v) => !info.max || v.length <= info.max) ?? values[0];
    filled = await h.fill(v).then(() => true, () => false);
  }}
  if (filled && {submit}) await h.press('Enter').catch(() => {{}});
  return {{ filled }};
}}",
        top = serde_json::to_string(&at.top)?,
        frame = serde_json::to_string(&at.frame)?,
        tag = serde_json::to_string(&at.tag)?,
        mark_js = serde_json::to_string(super::MARK_JS)?,
        values = serde_json::to_string(values)?,
    ))
}

async fn locate(state: &AppState, run: &Running, target: &str) -> Result<Option<Target>> {
    if !valid_ref(target) {
        return Ok(None);
    }
    let v = state
        .browser
        .run_code(run, &field_code(target, None)?)
        .await?;
    if v["found"] != true {
        return Ok(None);
    }
    let tag = v["tag"].as_str().unwrap_or("").to_string();
    if !matches!(tag.as_str(), "input" | "textarea" | "select" | "editable") {
        return Ok(None);
    }
    Ok(Some(Target {
        top: v["top"].as_str().unwrap_or("").into(),
        frame: v["frame"].as_str().unwrap_or("").into(),
        tag,
    }))
}

/// Type `values` (candidates) into a located field. Whether it was filled.
async fn put(
    state: &AppState,
    run: &Running,
    target: &str,
    at: &Target,
    values: &[String],
    private: bool,
    submit: bool,
) -> Result<bool> {
    if private {
        // Closed to the agent before anything is typed.
        state.browser.remember_private(&at.top);
    }
    let code = field_code(target, Some((at, values, private, submit)))?;
    let v = state.browser.run_code(run, &code).await?;
    if v["moved"] == true {
        return Err(PAGE_CHANGED.into());
    }
    Ok(v["filled"] == true)
}

const PAGE_CHANGED: &str = "the page changed while filling, so I stopped";

/// `fill_saved_login`.
pub async fn agent_login(state: &AppState, ctx: &TurnCtx, run: &Running, args: &Value) -> Value {
    match login_inner(state, ctx, run, args).await {
        Ok(v) => v,
        Err(e) => error(format!("not filled ({e})")),
    }
}

async fn login_inner(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    args: &Value,
) -> Result<Value> {
    let b = &state.browser;
    let refs: Vec<(&str, &str)> = [("username", "username_ref"), ("password", "password_ref")]
        .iter()
        .filter_map(|(f, k)| args[*k].as_str().map(|r| (*f, r.trim())))
        .filter(|(_, r)| !r.is_empty())
        .collect();
    if refs.is_empty() {
        return Ok(error(
            "Give username_ref and/or password_ref from browser_snapshot.",
        ));
    }
    // Every field first, so nothing is typed into a form that is not all
    // the saved site's.
    let mut targets = Vec::new();
    for (field, r) in &refs {
        let Some(at) = locate(state, run, r).await? else {
            return Ok(text(format!(
                "not found (no field for ref {r}; take a new browser_snapshot)"
            )));
        };
        targets.push((*field, *r, at));
    }
    let page = targets[0].2.top.clone();
    let logins: Vec<Credential> = credentials(state, &ctx.user_id, "login")
        .await
        .into_iter()
        .filter(|c| credential_matches(&page, c))
        .collect();
    let cred = match choose(&logins, args["login"].as_str(), |c, h| {
        let h = h.to_lowercase();
        c.label.to_lowercase().contains(&h)
            || c.metadata["username"]
                .as_str()
                .is_some_and(|u| u.to_lowercase() == h)
    }) {
        Ok(c) => c.clone(),
        Err(many) if many.is_empty() => {
            return Ok(text(
                "not found (no saved login for this site; ask the user to add it in Logins & \
                 wallet, or to take over the browser and sign in)",
            ));
        }
        Err(many) => {
            let names: Vec<String> = many.iter().map(|c| format!("\"{}\"", c.label)).collect();
            return Ok(error(format!(
                "Several logins are saved for this site: {}. Call fill_saved_login again with \
                 `login` set to one of them.",
                names.join(", ")
            )));
        }
    };
    for (_, r, at) in &targets {
        let secure = origin_of(&at.frame).is_some_and(|(s, h)| secure_enough(&s, &h));
        if !secure || !credential_matches(&at.frame, &cred) || !credential_matches(&at.top, &cred) {
            return Ok(text(format!(
                "not found (the field {r} is not on {} over a secure connection, so I won't \
                 fill the login there)",
                cred.metadata["site"].as_str().unwrap_or("the saved site")
            )));
        }
    }
    let (cred, secret) = load(state, &ctx.user_id, &cred.id).await?;
    let submit = args["submit"] == true;
    let last = targets.len() - 1;
    for (i, (field, r, at)) in targets.iter().enumerate() {
        b.check_agent(ctx)?;
        let (value, private) = value_of(&cred, &secret, field)?;
        if !put(state, run, r, at, &[value], private, submit && i == last).await? {
            return Ok(text(format!(
                "not filled (the {field} field {r} could not be filled)"
            )));
        }
    }
    drop(secret);
    let (uid, cid) = (ctx.user_id.clone(), cred.id.clone());
    let _ = state
        .db
        .call(move |c| entities::mark_credential_used(c, &uid, &cid))
        .await;
    crate::info!("browser: filled saved login {} for the agent", cred.id);
    Ok(text(if submit {
        "filled and submitted"
    } else {
        "filled"
    }))
}

/// A card the owner approved for this page.
pub struct Approved {
    card: Credential,
    message_id: String,
    origin: (String, String),
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

/// `fill_saved_card`, first step (without holding the page): which card,
/// and the owner's OK for it on this page. A tool result instead when
/// there is nothing to fill.
pub async fn approve_card(
    state: &AppState,
    ctx: &TurnCtx,
    args: &Value,
) -> std::result::Result<Approved, Value> {
    if ctx.audience.is_empty() {
        return Err(text(
            "not found (cards can only be used in a chat, where the user can approve)",
        ));
    }
    let b = &state.browser;
    let run = b
        .ensure()
        .await
        .map_err(|e| error(format!("The browser could not start: {e}")))?;
    let (page, _) = b.page_info(&run).await.map_err(|e| error(e.to_string()))?;
    let Some(origin) = origin_of(&page).filter(|(s, h)| secure_enough(s, h)) else {
        return Err(text(
            "not found (this page is not secure, so cards are not filled here)",
        ));
    };
    let cards = credentials(state, &ctx.user_id, "card").await;
    let card = match choose(&cards, args["card"].as_str(), |c, h| {
        let digits: String = h.chars().filter(char::is_ascii_digit).collect();
        c.label.to_lowercase().contains(&h.to_lowercase())
            || (digits.len() == 4 && c.metadata["last4"].as_str() == Some(&digits))
    }) {
        Ok(c) => c.clone(),
        Err(many) if many.is_empty() => {
            return Err(text("not found (no saved card like that)"));
        }
        Err(many) => {
            let names: Vec<String> = many
                .iter()
                .map(|c| format!("\"{}\" ({})", c.label, card_name(c)))
                .collect();
            return Err(error(format!(
                "Several cards are saved: {}. Call fill_saved_card again with `card` set to one \
                 of the names or the last 4 digits.",
                names.join(", ")
            )));
        }
    };
    let (answer, msg) = ask_approval(state, ctx, &card, &page, &origin).await;
    match answer {
        crate::gateway::Answer::Approved => {}
        crate::gateway::Answer::Denied => {
            return Err(text(
                "denied (the user said no; do not use the card or try another way)",
            ));
        }
        crate::gateway::Answer::Expired | crate::gateway::Answer::Cancelled => {
            return Err(text(
                "not filled (the approval expired or was cancelled; nothing was done)",
            ));
        }
    }
    let Some(msg) = msg else {
        return Err(text("not filled (the approval was cancelled)"));
    };
    Ok(Approved {
        card,
        message_id: msg.id,
        origin,
    })
}

/// Post an approval card (built from the real page and card, never from
/// agent text) and notify the owner if they are away.
async fn ask_approval(
    state: &AppState,
    ctx: &TurnCtx,
    card: &Credential,
    url: &str,
    origin: &(String, String),
) -> (crate::gateway::Answer, Option<db::Message>) {
    let host = &origin.1;
    use crate::gateway::policy::reveal_hidden;
    let name = reveal_hidden(&card_name(card));
    let exp = match (
        card.metadata["exp_month"].as_u64(),
        card.metadata["exp_year"].as_u64(),
    ) {
        (Some(m), Some(y)) => format!(", expires {m:02}/{}", y % 100),
        _ => String::new(),
    };
    let url = reveal_hidden(url);
    let host = reveal_hidden(host);
    let preview = format!(
        "Fill your {name}{exp} (\"{}\") into the payment form on this page:\n{url}\n\n\
         Approving lets me fill it in this once, to finish what you asked for.",
        reveal_hidden(&card.label),
    );
    let title_text = format!("Use your {name} on {host}");
    let mut value = entities::approval_card(&entities::ApprovalRequest {
        title: &title_text,
        service: "wallet",
        to: &host,
        subject: "",
        preview: &preview,
        action_label: "Fill card",
        approver_user_id: &ctx.user_id,
    });
    value["wallet"] = json!({"credential_id": card.id, "origin": format!("{}://{host}", origin.0),
                             "revision": card.updated_at, "used_at": null});
    crate::gateway::approve_browser_card(state, ctx, value).await
}

/// `fill_saved_card`, second step (holding the page): fill the approved
/// card into the fields the agent named.
pub async fn agent_card(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    args: &Value,
    approved: Approved,
) -> Value {
    match card_inner(state, ctx, run, args, approved).await {
        Ok(v) => v,
        Err(e) => error(format!("not filled ({e})")),
    }
}

async fn card_inner(
    state: &AppState,
    ctx: &TurnCtx,
    run: &Running,
    args: &Value,
    approved: Approved,
) -> Result<Value> {
    let b = &state.browser;
    const FIELDS: &[(&str, &str)] = &[
        ("number", "number_ref"),
        ("exp", "exp_ref"),
        ("exp_month", "exp_month_ref"),
        ("exp_year", "exp_year_ref"),
        ("cvc", "cvc_ref"),
        ("name", "name_ref"),
    ];
    let mut targets = Vec::new();
    for (field, key) in FIELDS {
        let Some(r) = args[*key].as_str().map(str::trim).filter(|r| !r.is_empty()) else {
            continue;
        };
        let Some(at) = locate(state, run, r).await? else {
            return Ok(text(format!(
                "not filled (no field for ref {r}; take a new browser_snapshot and call \
                 fill_saved_card again)"
            )));
        };
        // The page the owner approved, and every field over HTTPS.
        let field_secure = origin_of(&at.frame).is_some_and(|(s, h)| secure_enough(&s, &h));
        if origin_of(&at.top).as_ref() != Some(&approved.origin) || !field_secure {
            return Ok(text(
                "not filled (the page is not the one the user approved, or a field is not \
                 secure)",
            ));
        }
        targets.push((*field, r, at));
    }
    if !targets.iter().any(|(f, _, _)| *f == "number") {
        return Ok(error("Give number_ref (from browser_snapshot)."));
    }
    b.check_agent(ctx)?;
    let revision = approved.card.updated_at;
    let (card, secret) = load(state, &ctx.user_id, &approved.card.id).await?;
    if card.updated_at != revision {
        return Ok(text(
            "not filled (the card was edited since the user approved; call fill_saved_card \
             again to ask)",
        ));
    }
    // Consume the approval before typing, so one OK fills once.
    let mid = approved.message_id.clone();
    let consumed = state
        .db
        .call(move |c| {
            let Some(m) = db::get_message(c, &mid)? else {
                return Ok(false);
            };
            let mut card = m.card.unwrap_or_default();
            if card["state"] != "approved" || !reusable(&card, m.created_at, revision, now_ms()) {
                return Ok(false);
            }
            card["wallet"]["used_at"] = now_ms().into();
            db::update_message_card(c, &mid, &card).map(|_| true)
        })
        .await?;
    if !consumed {
        return Ok(text(
            "not filled (that OK was already used or has expired; call fill_saved_card again \
             to ask)",
        ));
    }
    let month = card.metadata["exp_month"]
        .as_i64()
        .filter(|m| (1..=12).contains(m));
    let year = card.metadata["exp_year"].as_i64().filter(|y| *y > 0);
    let submit = args["submit"] == true;
    let last = targets.len() - 1;
    let mut missing = Vec::new();
    for (i, (field, r, at)) in targets.iter().enumerate() {
        b.check_agent(ctx)?;
        let values: Vec<String> = match *field {
            "exp_month" => month
                .map(|m| vec![format!("{m:02}"), m.to_string()])
                .unwrap_or_default(),
            "exp_year" => year
                .map(|y| vec![y.to_string(), format!("{:02}", y % 100)])
                .unwrap_or_default(),
            f => value_of(&card, &secret, f)
                .map(|(v, _)| vec![v])
                .unwrap_or_default(),
        };
        if values.is_empty() {
            missing.push(*field);
            continue;
        }
        // Everything but the name closes the page to the agent.
        let private = *field != "name";
        if !put(state, run, r, at, &values, private, submit && i == last).await? {
            if *field == "number" {
                return Ok(text(
                    "not filled (the card number field could not be filled)",
                ));
            }
            missing.push(*field);
        }
    }
    drop(secret);
    let (uid, cid) = (ctx.user_id.clone(), card.id.clone());
    let _ = state
        .db
        .call(move |c| entities::mark_credential_used(c, &uid, &cid))
        .await;
    crate::info!(
        "browser: filled wallet card {} for the agent after approval {}",
        card.id,
        approved.message_id
    );
    Ok(text(if missing.is_empty() {
        "filled".to_string()
    } else {
        format!(
            "filled (but {} could not be filled; ask the user to take over for it)",
            missing.join(", ").replace('_', " ")
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn element_refs_only() {
        for ok in ["e1", "e123", "f1e5", "f12e3"] {
            assert!(valid_ref(ok), "{ok}");
        }
        for bad in [
            "",
            "e",
            "f",
            "fe1",
            "e1x",
            "1",
            "e1\"); x(\"",
            "aria-ref=e1",
            "#id",
            "f1",
        ] {
            assert!(!valid_ref(bad), "{bad}");
        }
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
        assert!(reusable(&card("denied", 5, Value::Null), recent, 5, now));
    }

    #[test]
    fn fields_of_saved_items() {
        let cred = |kind: &str, metadata: Value| Credential {
            id: "c".into(),
            kind: kind.into(),
            label: "x".into(),
            metadata,
            created_at: 0,
            updated_at: 0,
            last_used_at: None,
        };
        let secret = |v: Value| v.as_object().unwrap().clone();
        let login = cred("login", json!({"site": "example.test", "username": "sam"}));
        let s = secret(json!({"password": "pw-1234", "notes": "n"}));
        assert_eq!(
            value_of(&login, &s, "username").unwrap(),
            ("sam".into(), false)
        );
        assert_eq!(
            value_of(&login, &s, "password").unwrap(),
            ("pw-1234".into(), true)
        );
        assert!(value_of(&login, &s, "notes").is_err());
        assert!(value_of(&login, &secret(json!({})), "password").is_err());
        let card = cred(
            "card",
            json!({"exp_month": 3, "exp_year": 2031, "cardholder": "S Doe"}),
        );
        let s = secret(json!({"number": "4242424242424242", "cvc": "123"}));
        assert!(value_of(&card, &s, "number").unwrap().1);
        assert_eq!(value_of(&card, &s, "exp").unwrap(), ("03/31".into(), true));
        assert_eq!(
            value_of(&card, &s, "name").unwrap(),
            ("S Doe".into(), false)
        );
        assert!(value_of(&card, &s, "password").is_err());
        assert_eq!(expiry(&json!({"exp_month": 13, "exp_year": 2031})), None);
    }

    #[test]
    fn origins_and_site_matching() {
        assert_eq!(
            origin_of("https://www.Shop.example:8443/cart?x=1").unwrap(),
            ("https".into(), "www.shop.example".into())
        );
        assert_eq!(
            origin_of("http://127.0.0.1:9000/login").unwrap().1,
            "127.0.0.1"
        );
        assert_eq!(origin_of("http://[::1]:80/").unwrap().1, "::1");
        assert!(origin_of("about:blank").is_none());
        assert!(!site_matches(
            "https://example.test",
            "https://example.test:bad",
            true
        ));
        assert!(!site_matches(
            "https://example.test",
            "https://example.test:99999",
            true
        ));
        assert!(site_matches(
            "https://example.test/login",
            "example.test",
            false
        ));
        assert!(site_matches(
            "https://example.test:443/login",
            "https://example.test",
            false
        ));
        assert!(!site_matches(
            "https://example.test:8443/login",
            "example.test",
            false
        ));
        assert!(!site_matches(
            "http://example.test/login",
            "example.test",
            false
        ));
        assert!(!site_matches(
            "https://www.example.test/login",
            "example.test",
            false
        ));
        assert!(!site_matches(
            "https://accounts.example.test/login",
            "example.test",
            false
        ));
        assert!(site_matches(
            "https://accounts.example.test:8443/login",
            "https://example.test:8443",
            true
        ));
        assert!(!site_matches(
            "https://accounts.example.test/login",
            "https://example.test:8443",
            true
        ));
        assert!(!site_matches(
            "https://example.test.evil.test",
            "example.test",
            true
        ));
        assert!(!site_matches(
            "https://badexample.test",
            "example.test",
            true
        ));
        assert!(site_matches(
            "http://[::1]:8888/login",
            "http://[::1]:8888",
            false
        ));
        assert!(!site_matches(
            "http://[::1]:8889/login",
            "http://[::1]:8888",
            false
        ));
        assert!(!site_matches(
            "https://user@example.test",
            "example.test",
            true
        ));
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
