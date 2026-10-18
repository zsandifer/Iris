//! Iris's computer: one headless browser with a persistent profile in the
//! data directory (so sign-ins survive restarts), a live view in the app,
//! and take over from the phone.
//!
//! Iris does not automate the browser. Microsoft's Playwright MCP server
//! (`@playwright/mcp`, installed and updated like the CLIs, see
//! `engines.rs`) runs it, and the harness CLI decides what to call. Iris
//! is only the wrapper around that server:
//! - It starts the server on Iris's side (never as the agent) when the
//!   browser is first needed and stops it after `browser.idle_mins`.
//! - Turns in the owner's own conversations get its tools through `iris mcp
//!   --browser`, which forwards over a private socket (`socket.rs`); only
//!   reviewed tools pass, unchanged (`tools.rs`).
//! - Live view: while an app watches, Iris takes screenshots through the
//!   same server and sends new ones to the sockets that asked
//!   (`browser.watch`).
//! - Take over: the owner's taps, scrolls, and typing go to the same page
//!   through the same server; the agent's tools wait until control comes
//!   back. Sites that fight automation are signed in to this way.
//! - Saved logins and cards: filled only when the owner asks while in
//!   control (`fill.rs`).
//!
//! The agent never sees a password, card number or code, and still sees
//! the page around it. A saved secret Iris filled is known to Iris; what
//! the owner types into a private field (a password, card details, a
//! one-time code) is read back from Iris's side before the agent next
//! uses the browser (`capture_typed`). Known secrets are hidden from
//! everything the agent reads, and its screenshots black out every private
//! field and anything showing a secret (`masked_screenshot`), so the page
//! stays usable; only searching its text waits until they are gone. What
//! cannot be read back (a value too short to look for, a frame that could
//! not be read) keeps the old rule: the document is marked when the owner
//! types (it stays closed in any tab, and after the address changes within
//! it), and so is its address (a page brought back from history may still
//! hold what was typed), until the browser closes.

pub mod apparmor;
pub mod fill;
pub mod launch;
pub mod own;
#[cfg(test)]
mod server_tests;
pub mod socket;
pub mod tools;

use crate::config::{BrowserConfig, Paths};
use crate::events::Hub;
use crate::gateway::upstream::Upstream;
use crate::util::{Result, new_id, now_ms};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// The page size in CSS pixels: phone-shaped, so the live view fills a
/// phone screen and responsive sites show their mobile layout.
pub const VIEW_W: u32 = 500;
pub const VIEW_H: u32 = 1000;
/// Device scale factor (sharp text in the full-screen live view).
pub const SCALE: f64 = 2.0;
/// How long a tool call waits while the owner has taken over.
const CONTROL_WAIT: Duration = Duration::from_secs(50);
/// Take-over without any input for this long hands control back.
const TAKEOVER_IDLE_MS: i64 = 5 * 60 * 1000;
/// Longest one call to the server may take (Playwright's own navigation
/// limit is 60 seconds).
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(90);
/// Starting the server and Chromium.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// Iris's own quick calls (tab list, screenshots, input).
const QUICK: Duration = Duration::from_secs(20);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Who drives the browser right now.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Agent,
    User { user_id: String, device_id: String },
}

/// Live view quality. `Full` is the sharp full-screen view, a few frames a
/// second; `Peek` a small thumbnail about once a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    Peek,
    Full,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "peek" => Some(Mode::Peek),
            "full" => Some(Mode::Full),
            _ => None,
        }
    }
    pub fn min_interval(self) -> Duration {
        match self {
            Mode::Peek => Duration::from_millis(1000),
            Mode::Full => Duration::from_millis(300),
        }
    }
    /// Picture width when the viewer does not say (device pixels).
    fn default_width(self) -> u32 {
        match self {
            Mode::Peek => VIEW_W,
            Mode::Full => FRAME_MAX_W,
        }
    }
    /// JPEG quality: text stays readable at the sizes viewers ask for.
    fn quality(self) -> u32 {
        match self {
            Mode::Peek => 50,
            Mode::Full => 55,
        }
    }
}

/// Live view picture widths (device pixels): never sharper than the page
/// itself (`VIEW_W` x `SCALE`), never too small to make out.
const FRAME_MAX_W: u32 = 1000;
const FRAME_MIN_W: u32 = 120;

/// One live-view picture of the visible page at `width` device pixels,
/// through Chrome's own capture (any size, set quality, nothing written to
/// disk). The DevTools session is kept on the page for the next frame; one
/// that stops answering (seen on some sites after they load) is dropped
/// after a few seconds, so the next picture uses a new one.
fn frame_code(width: u32, quality: u32) -> String {
    format!(
        "async (page) => {{
  const within = (p) => Promise.race([p, new Promise((_, no) =>
    setTimeout(() => no(new Error('the page did not answer in time')), {FRAME_STEP_MS}))]);
  let s;
  try {{
    s = page.__irisCast || (page.__irisCast = await within(page.context().newCDPSession(page)));
    const {{ cssVisualViewport: v }} = await within(s.send('Page.getLayoutMetrics'));
    const r = await within(s.send('Page.captureScreenshot', {{ format: 'jpeg', quality: {quality},
      clip: {{ x: v.pageX, y: v.pageY, width: v.clientWidth, height: v.clientHeight, scale: {width} / v.clientWidth }} }}));
    return r.data;
  }} catch (e) {{
    page.__irisCast = undefined;
    if (s) s.detach().catch(() => {{}});
    throw e;
  }}
}}"
    )
}

/// Longest wait for each step of a live-view picture, and for the whole
/// call: a stuck capture holds the live view only this long, not the
/// 20 seconds other quick calls get.
const FRAME_STEP_MS: u64 = 3000;
const FRAME_TIMEOUT: Duration = Duration::from_secs(8);

/// One encoded frame, serialized once for every watcher.
pub struct Frame {
    pub json: String,
}

/// A turn that may use the browser (registered for the duration of the
/// turn; the token reaches `iris mcp --browser` through its environment).
pub struct TurnCtx {
    pub user_id: String,
    pub conversation_id: String,
    pub message_id: String,
    pub audience: Vec<String>,
    /// Preparing an ask-first routine: only tools that look (open pages,
    /// read, scroll) until the owner says yes.
    pub look_only: bool,
    used: AtomicBool,
    /// Set when the turn ends; tool calls still in flight stop at their
    /// next check (waits end at once).
    ended: watch::Sender<bool>,
}

impl TurnCtx {
    pub fn is_ended(&self) -> bool {
        *self.ended.borrow()
    }

    /// Ends with the turn (approvals held for it are cancelled).
    pub(crate) fn cancellation(&self) -> watch::Sender<bool> {
        self.ended.clone()
    }

    /// Resolves when the turn ends.
    pub async fn wait_ended(&self) {
        let mut rx = self.ended.subscribe();
        let _ = rx.wait_for(|e| *e).await;
    }
}

/// Why the agent may not act right now.
pub const TAKEN_OVER: &str = "The user has taken over the browser and is still using it. Wait a \
    bit and try again, or tell them you are waiting for them to hand it back.";
pub const TURN_ENDED: &str = "This browser session has ended.";
pub const PRIVATE_PAGE: &str = "The user typed private details on this page (like a password, \
    card details, or a code) that cannot be hidden from you, so you cannot read or use it. Wait \
    until the page moves on (for example after signing in), or open another page.";
const NOT_INSTALLED: &str = "no browser is installed; set it up from Iris's computer in the app";
const DISCONNECTED: &str =
    "The browser is disconnected; the owner can reopen it in Iris’s computer.";
/// Error when Chrome's sandbox cannot start here and the owner has not
/// agreed to run without it.
pub const SANDBOX_UNAVAILABLE: &str = "Chrome's security sandbox cannot start on this server (the owner can sort this out in Iris's computer in the app)";

/// Unregisters the turn when dropped; tells the chat the browser peek can
/// go away.
pub struct TurnGuard {
    browser: Browser,
    token: String,
}

impl TurnGuard {
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        let ctx = lock(&self.browser.turns).remove(&self.token);
        if let Some(ctx) = &ctx {
            ctx.ended.send_replace(true);
        }
        if let Some(ctx) = ctx
            && ctx.used.load(Ordering::SeqCst)
        {
            // The peek stays a while in a chat someone sees (not a job's).
            let linger = !ctx.audience.is_empty()
                && self.browser.running().is_some()
                && self.browser.control() == Control::Agent;
            if linger {
                lock(&self.browser.recent).insert(
                    ctx.conversation_id.clone(),
                    (ctx.message_id.clone(), now_ms()),
                );
            }
            self.browser.hub.to_users(
                &ctx.audience,
                json!({"type": "browser.done", "conversation_id": ctx.conversation_id,
                       "message_id": ctx.message_id,
                       "linger_ms": if linger { LINGER_MS } else { 0 }}),
            );
            self.browser.broadcast_state();
        }
    }
}

/// The running server (and the Chromium it started).
pub struct Running {
    up: Upstream,
    pub no_sandbox: bool,
}

impl Running {
    fn alive(&self) -> bool {
        self.up.alive()
    }
}

#[derive(Debug, Clone, Default)]
struct View {
    url: String,
    title: String,
    error: Option<String>,
    busy: bool,
    /// The last start failed only because Chrome's sandbox cannot run here.
    sandbox_unavailable: bool,
}

/// Where the owner typed what the agent must not read.
#[derive(Debug, Default)]
struct Private {
    /// Addresses (without fragments) of pages where the owner typed or
    /// filled private details, kept until the browser closes, with the id
    /// of the document they were typed into (empty: unknown).
    pages: Vec<(String, String)>,
    /// Tabs open when last counted (new ones during take over are followed).
    tabs: usize,
    /// Saved secrets Iris filled this session: hidden from everything the
    /// agent gets back (a page may echo them into its address, title, a
    /// dialog, or its text).
    secrets: Vec<String>,
    /// When the focused field was last checked for typing (the check is a
    /// round trip with Playwright's settle wait, too slow for every key).
    field_checked: Option<std::time::Instant>,
    /// Iris filled a saved secret (or an expiry), or captured what the
    /// owner typed, this session: pages are checked for those (`page_access`).
    filled: bool,
    /// The owner typed into a private field since Iris last read what
    /// they typed (`capture_typed`).
    typed_dirty: bool,
    /// Counts the owner's private typing: a document read back is marked
    /// for the generation it was read at, so typing again needs a new read.
    typed_gen: u64,
    /// Some of that typing went into another site's frame, where the field
    /// could not be told apart: every field of such frames is read back,
    /// for the rest of the session (a later edit by arrow keys or a page's
    /// button cannot be told apart either).
    typed_in_frame: bool,
    /// A document was marked private this session: its markers are checked
    /// even once its addresses are forgotten.
    ever_marked: bool,
    /// Whether the field the owner types into takes private details (as
    /// last checked), and the document it is in.
    typing_private: bool,
    typing_doc: String,
    /// What the owner typed into it so far, from the keys Iris passed on:
    /// hidden too, in case the page moves it out of the field before it is
    /// read back (`flush_typed`).
    typed_text: String,
    /// Fields the owner changed (typed into, deleted from), as (document,
    /// edit id): Iris tags the field with the id (`FIELD_FN`), and that
    /// document only counts as read back once a read finds the field again
    /// with every value hideable (the page may have moved or removed it).
    edits: Vec<(String, u64)>,
    /// The last edit id given out.
    edit_seq: u64,
    /// The owner's typing since the last tap or key (`typed_text`): the
    /// edits it made, and whether it began in an empty field (then what was
    /// typed is the whole value, and hiding it covers those edits).
    run_edits: Vec<u64>,
    run_from_empty: bool,
}

/// Whether the agent may use the current page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Open,
    /// The owner typed private details into it that Iris could not read
    /// back (or it was reopened from history): closed until it moves on.
    Typed,
    /// A secret Iris knows (filled, or captured from the owner's typing)
    /// is on it: usable, with the secret hidden from text and blacked out
    /// in pictures; searching its text is not.
    Filled,
}

/// How long one check of the focused field covers the owner's typing,
/// unless they tap, press a key, or move to another page first (a page
/// moving focus by itself is caught at the next check).
pub(crate) const FIELD_CHECK_FOR: Duration = Duration::from_secs(3);

/// Reads the owner's saved secrets (`fill::saved_secrets_blocking`).
pub type SecretSource = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// Most private addresses remembered (the oldest are forgotten first; the
/// documents themselves stay marked).
const PRIVATE_PAGES_MAX: usize = 64;

/// After a turn that used the browser ends, the chat keeps its live peek at
/// least this long (and until the owner writes again, or takes over): the
/// turn often ends by asking them to take over.
pub const LINGER_MS: i64 = 60_000;

/// Shared browser state (cheap to clone through `Arc`).
pub struct Shared {
    cfg: BrowserConfig,
    profile_dir: PathBuf,
    /// The server's working directory; its output folder is inside.
    work_dir: PathBuf,
    out_dir: PathBuf,
    config_file: PathBuf,
    hub: Hub,
    launch_lock: tokio::sync::Mutex<()>,
    list_lock: tokio::sync::Mutex<()>,
    running: Mutex<Option<Arc<Running>>>,
    view: Mutex<View>,
    control: watch::Sender<Control>,
    frames: watch::Sender<Option<Arc<Frame>>>,
    frame_seq: AtomicU64,
    /// Who watches: the mode and picture width (device pixels) each wants.
    watchers: Mutex<HashMap<u64, (Mode, u32)>>,
    next_watcher: AtomicU64,
    casting: AtomicBool,
    turns: Mutex<HashMap<String, Arc<TurnCtx>>>,
    /// The last finished turn that used the browser, per conversation:
    /// (message id, ended at). Its chat peek stays for a while.
    recent: Mutex<HashMap<String, (String, i64)>>,
    last_used: AtomicI64,
    last_input: AtomicI64,
    /// Serializes actions on the page (tool calls and the owner's input).
    pub(crate) op_lock: tokio::sync::Mutex<()>,
    /// One picture at a time: a live-view picture taken at the same moment
    /// as the agent's changes its scale (and the agent's masks are painted
    /// for the scale Iris asked for).
    pub(crate) capture_lock: tokio::sync::Mutex<()>,
    /// "Pause Iris" in the app: no turns, jobs, or browsing until resumed.
    paused: AtomicBool,
    /// Disconnect everything disables browser tools until the owner reopens it.
    disconnected: AtomicBool,
    /// The owner agreed (in the app) to run without Chrome's sandbox on a
    /// system where it cannot start.
    allow_no_sandbox: AtomicBool,
    /// The browser is being installed, updated, or removed.
    maintenance: AtomicBool,
    /// Orders changes of the saved safety switches (pause, sandbox).
    pub switch_lock: tokio::sync::Mutex<()>,
    /// The server's tools the agent may use (asked once per install).
    tools: Mutex<Option<Vec<Value>>>,
    private: Mutex<Private>,
    secret_source: Mutex<Option<SecretSource>>,
    /// Tests run a stand-in server.
    #[cfg(test)]
    server_override: Mutex<Option<launch::Server>>,
}

#[derive(Clone)]
pub struct Browser(pub Arc<Shared>);

impl std::ops::Deref for Browser {
    type Target = Shared;
    fn deref(&self) -> &Shared {
        &self.0
    }
}

/// Removes a watcher when the app stops watching (or its socket closes).
pub struct WatchGuard {
    browser: Browser,
    id: u64,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        lock(&self.browser.watchers).remove(&self.id);
    }
}

/// All text parts of a tool result.
pub(crate) fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn is_error(result: &Value) -> bool {
    result["isError"] == true
}

/// A tool result's text, or its error as a short message.
fn ok_text(result: &Value) -> Result<String> {
    let text = text_of(result);
    if !is_error(result) {
        return Ok(text);
    }
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("Error:"))
        .unwrap_or("the browser could not do that");
    Err(crate::util::truncate_chars(launch::error_line(line), 200).into())
}

/// The value after `prefix` on a line of `text` (Playwright's page header:
/// `- Page URL: ...`).
fn line_value<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.trim_start().strip_prefix(prefix))
        .map(str::trim)
}

/// How many tabs a `browser_tabs` result lists (`- 0: [Title](url)`).
fn tab_count(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            l.strip_prefix("- ")
                .and_then(|r| r.split_once(':'))
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        })
        .count()
}

/// The `### Page` section of a result (Playwright's page header), which
/// comes before anything the page wrote itself.
fn page_section(text: &str) -> Option<&str> {
    let start = if text.starts_with("### Page\n") {
        0
    } else {
        text.find("\n### Page\n")? + 1
    };
    let body = &text[start + "### Page\n".len()..];
    Some(&body[..body.find("\n###").unwrap_or(body.len())])
}

/// The first JSON value after `### Result` (Iris's own page checks).
fn result_value(text: &str) -> Option<Value> {
    let after = &text[text.find("### Result")? + "### Result".len()..];
    let end = after.find("\n###").unwrap_or(after.len());
    serde_json::from_str(after[..end].trim()).ok()
}

/// An address without its fragment (a fragment never loads a new page).
fn page_key(url: &str) -> &str {
    url.split('#').next().unwrap_or(url)
}

/// Iris's own look at the page, run by Playwright's Node side (not
/// through the page's scripts): the real address and title, and whether
/// this document was marked private. `PAGE_FN` is evaluated in the page.
const PAGE_CODE: &str = r#"async (page) => ({
  url: page.url(),
  title: await page.title(),
  private: await page.evaluate("window.__irisPrivate === true"),
  captured: await page.evaluate("window.__irisCaptured_GEN === true"),
  closed: await page.evaluate("window.__irisClosed === true"),
})"#;

/// `PAGE_CODE` asking whether this document was read back after the
/// owner's latest private typing (`generation`).
fn page_code(generation: u64) -> String {
    PAGE_CODE.replace("GEN", &generation.to_string())
}

/// Whether a field takes private details (passwords, card fields, one-time
/// codes), from its type, autocomplete, name, id, labels and placeholder.
const SENSITIVE_FN: &str = r#"(e) => {
    const doc = e.ownerDocument;
    const named = (e.getAttribute('aria-labelledby') || '').split(/\s+/)
      .map((id) => id && doc.getElementById(id)).filter(Boolean).map((l) => l.textContent);
    const a = [e.type, e.autocomplete, e.name, e.id, e.getAttribute('aria-label'), e.placeholder,
      ...Array.from(e.labels || [], (l) => l.textContent), ...named]
      .filter(Boolean).join(' ').toLowerCase();
    if (/password|passcode|cc-|card|cvc|cvv|csc|security code|expir|one-time|otp|\bpin\b|verification|2fa|mfa|totp|authenticat/.test(a)) return true;
    return /\bcode\b/.test(a) && !/postal|zip|promo|coupon|discount|gift|voucher|referral|country|area code|dial/.test(a);
  }"#;

/// A field that takes private details other than an expiry date (`SENSITIVE`
/// is `SENSITIVE_FN`): an expiry month or year is not hidden (see the API
/// docs), and two digits are too short to look for, so typing into one
/// neither closes the page nor holds back reading the page's fields back.
/// Pictures still black out every `SENSITIVE_FN` field.
const SECRET_FIELD_FN: &str = r#"(e) => (SENSITIVE)(e) && !(EXPIRY)(e)"#;

/// A field that takes only an expiry month, year or date (see
/// `SECRET_FIELD_FN`).
const EXPIRY_ONLY_FN: &str = r#"(e) => {
    const doc = e.ownerDocument;
    const named = (e.getAttribute('aria-labelledby') || '').split(/\s+/)
      .map((id) => id && doc.getElementById(id)).filter(Boolean).map((l) => l.textContent);
    const a = [e.type, e.autocomplete, e.name, e.id, e.getAttribute('aria-label'), e.placeholder,
      ...Array.from(e.labels || [], (l) => l.textContent), ...named]
      .filter(Boolean).join(' ').toLowerCase();
    return /cc-exp|expir/.test(a)
      && !/password|passcode|cvc|cvv|csc|security|one-time|otp|\bpin\b|verification|2fa|mfa|totp|authenticat|\bcode\b|number|cc-csc/.test(a);
  }"#;

/// `SECRET_FIELD_FN`, ready to insert.
fn secret_field_fn() -> String {
    SECRET_FIELD_FN
        .replace("SENSITIVE", SENSITIVE_FN)
        .replace("EXPIRY", EXPIRY_ONLY_FN)
}

/// What the owner typed into private fields, read from Iris's side when
/// the agent next uses the browser, so it can be hidden like a saved
/// secret instead of closing the page: per tab, the values of its
/// private fields and private editable areas in every frame. In other
/// sites' frames, where the check before typing could not tell which
/// field the owner typed into (`ALLFRAMES`), every field and editable
/// area that holds something. A tab whose frames were all read and whose
/// values are all long enough to look for counts as read back; Iris marks
/// it so (`MARK_CAPTURED_CODE`) only after storing its values, and only
/// once every field the owner changed there was found again holding a
/// value long enough to look for (`edits`: the edit ids on such fields).
/// `MIN` is `fill::MIN_HIDDEN`.
const CAPTURE_CODE: &str = r#"async (page) => { /* iris:capture */
  const origin = (u) => { try { return new URL(u).origin; } catch (_) { return ''; } };
  const hideable = (v) => [...v].length >= MIN || /^[0-9]{3}$/.test(v);
  const out = [];
  const pages = page.context().pages();
  for (let i = 0; i < pages.length; i++) {
    const p = pages[i];
    const top = p.mainFrame();
    let ok = true;
    const values = [];
    const edits = [];
    for (const f of p.frames()) {
      const other = f !== top && origin(f.url()) !== origin(top.url());
      const r = await f.evaluate((all) => {
        const sensitive = SENSITIVE;
        const expiryOnly = EXPIRY;
        const vals = [];
        const edits = [];
        const walk = (root) => {
          for (const e of root.querySelectorAll('input, textarea, [contenteditable]')) {
            const editable = e.tagName !== 'INPUT' && e.tagName !== 'TEXTAREA';
            if (editable && !e.isContentEditable) continue;
            if (!editable && ['button', 'submit', 'reset', 'checkbox', 'radio', 'file', 'hidden', 'image', 'range', 'color'].includes(e.type)) continue;
            if (!((all && !expiryOnly(e)) || e.__irisTyped === true || sensitive(e))) continue;
            const v = String(editable ? e.innerText || '' : e.value || '').trim();
            if (v) vals.push(v);
            // A changed field counts as found again only holding a value
            // long enough to look for (one the page emptied may have moved
            // it elsewhere).
            const kept = [...v].length >= MIN || /^[0-9]{3}$/.test(v);
            if (kept && Array.isArray(e.__irisEdits)) edits.push(...e.__irisEdits.filter((n) => typeof n === 'number'));
          }
          for (const e of root.querySelectorAll('*')) if (e.shadowRoot) walk(e.shadowRoot);
        };
        walk(document);
        return { vals, edits };
      }, other && ALLFRAMES).catch(() => null);
      if (r === null) {
        if (!f.isDetached()) ok = false;
        continue;
      }
      values.push(...r.vals);
      edits.push(...r.edits);
    }
    const captured = ok && values.every(hideable);
    const state = await p.evaluate(() => ({ marked: window.__irisPrivate === true, doc: String(window.__irisDoc || '') }))
      .catch(() => ({ marked: false, doc: '' }));
    out.push({ i, url: p.url(), captured, ...state, edits, values: values.filter(hideable) });
  }
  return out;
}"#;

fn capture_code(all_frames: bool) -> String {
    CAPTURE_CODE
        .replace("EXPIRY", EXPIRY_ONLY_FN)
        .replace("SENSITIVE", &secret_field_fn())
        .replace("MIN", &fill::MIN_HIDDEN.to_string())
        .replace("ALLFRAMES", if all_frames { "true" } else { "false" })
}

/// Marks the tabs `WANT` (`{i, url}`, still at that address) as read back
/// for typing generation `GEN`; answers the indexes marked.
const MARK_CAPTURED_CODE: &str = r#"async (page) => { /* iris:markcaptured */
  const want = WANT;
  const pages = page.context().pages();
  const done = [];
  for (const w of want) {
    const p = pages[w.i];
    if (!p || p.url() !== w.url) continue;
    const ok = await p.evaluate(() => {
      try { Object.defineProperty(window, '__irisCaptured_GEN', { value: true }); } catch (_) {}
      return window.__irisCaptured_GEN === true;
    }).catch(() => false);
    if (ok) done.push(w.i);
  }
  return done;
}"#;

/// Every field and editable area of every frame of the current tab, in
/// document order (`null` for a frame that cannot be read): its value and
/// whether it takes private details. Compared before and after an action
/// on a page holding a secret (`hide_changed`).
const FIELDS_CODE: &str = r#"async (page) => { /* iris:fields */
  const out = [];
  for (const f of page.frames()) {
    const r = await f.evaluate(() => {
      const sensitive = SENSITIVE;
      const vals = [];
      const walk = (root) => {
        for (const e of root.querySelectorAll('input, textarea, [contenteditable]')) {
          const editable = e.tagName !== 'INPUT' && e.tagName !== 'TEXTAREA';
          if (editable && !e.isContentEditable) continue;
          vals.push([String(editable ? e.innerText || '' : e.value || ''), e.type === 'password' || e.__irisTyped === true || sensitive(e)]);
        }
        for (const e of root.querySelectorAll('*')) if (e.shadowRoot) walk(e.shadowRoot);
      };
      walk(document);
      return vals;
    }).catch(() => null);
    out.push(r);
  }
  return out;
}"#;

/// `SHOT_CODE` for these secret forms and screenshot options.
pub(crate) fn shot_code(groups: &[Vec<String>], opts: &Value) -> Result<String> {
    Ok(SHOT_CODE
        .replace("SECRETS", &serde_json::to_string(groups)?)
        .replace("OPTS", &opts.to_string())
        .replace("SENSITIVE", SENSITIVE_FN))
}

/// Paint `rects` (x, y, width, height in CSS pixels of an area `css_w` by
/// `css_h`) black on a PNG picture of that area, a pixel wider on every
/// side. The picture's scale is its size over the area's; one that does
/// not match on both sides is refused rather than masked in the wrong place.
pub(crate) fn paint_masks(
    png_bytes: &[u8],
    css_w: f64,
    css_h: f64,
    rects: &[[f64; 4]],
) -> Result<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut pixels = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or("the picture is too large")?
    ];
    let info = reader.next_frame(&mut pixels)?;
    let (w, h) = (info.width as usize, info.height as usize);
    let channels = info.color_type.samples();
    if css_w <= 0.0 || css_h <= 0.0 {
        return Err("the picture has no size".into());
    }
    let (sx, sy) = (w as f64 / css_w, h as f64 / css_h);
    if (sx - sy).abs() > 0.02 * sx.max(sy) {
        return Err("the picture does not have the page's shape".into());
    }
    let stride = info.line_size;
    for &[x, y, rw, rh] in rects {
        if !(x.is_finite() && y.is_finite() && rw.is_finite() && rh.is_finite()) {
            return Err("a private area has no position".into());
        }
        let clamp = |v: f64, max: usize| v.max(0.0).min(max as f64) as usize;
        let (x0, x1) = (
            clamp((x * sx).floor() - 1.0, w),
            clamp(((x + rw) * sx).ceil() + 1.0, w),
        );
        let (y0, y1) = (
            clamp((y * sy).floor() - 1.0, h),
            clamp(((y + rh) * sy).ceil() + 1.0, h),
        );
        for row in y0..y1 {
            let line = &mut pixels[row * stride..row * stride + w * channels];
            for px in line[x0 * channels..x1 * channels].chunks_mut(channels) {
                // Black, fully opaque.
                match channels {
                    2 | 4 => {
                        let a = channels - 1;
                        px[..a].fill(0);
                        px[a] = 255;
                    }
                    _ => px.fill(0),
                }
            }
        }
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, info.width, info.height);
        encoder.set_color(info.color_type);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let rows: Vec<u8> = pixels
            .chunks(stride)
            .take(h)
            .flat_map(|l| l[..w * channels].iter().copied())
            .collect();
        writer.write_image_data(&rows)?;
    }
    Ok(out)
}

/// What `FIELDS_CODE` found: per frame, (value, private) of each field.
pub(crate) type Fields = Vec<Option<Vec<(String, bool)>>>;

/// The agent's picture of the page, and where its private details are:
/// every private field (as `SENSITIVE_FN` sees them, or typed into by the
/// owner), every field or text holding one of `SECRETS` (every written
/// form `hide_secrets` knows, see `mask_form_groups`) in each frame, and any
/// frame Iris cannot look into, whole, as rectangles in the picture's
/// pixels. Iris paints them black itself (`paint_masks`): nothing is drawn
/// in the page, and nothing there depends on the secrets (the page lists
/// its fields and text nodes the same way for every page; matching happens
/// here, positions come from Playwright). The picture is taken with Chrome's
/// own capture at a fixed scale, between two measurements that must agree
/// (a page moving meanwhile is measured again). Only what is on screen is
/// pictured. `OPTS`: `target` (a snapshot ref; its part on screen),
/// `scale` (`css`, the default, or `device`).
const SHOT_CODE: &str = r#"async (page) => { /* iris:shot */
  // Every written form of each secret, one group per secret.
  const groups = SECRETS;
  const secrets = groups.flat();
  const opts = OPTS;
  // Compared without regard to case or the kind of white space (a card
  // number may be grouped with non-breaking spaces).
  const norm = (x) => String(x).toLowerCase().replace(/\s+/g, ' ');
  const low = secrets.map(norm);
  const scanPage = () => {
    const sensitive = SENSITIVE;
    const xpath = (el) => {
      // An element inside a shadow root is named by its outermost host.
      let e = el;
      for (let r = e.getRootNode(); r && r.host; r = e.getRootNode()) e = r.host;
      const steps = [];
      for (; e && e.nodeType === 1; e = e.parentElement) {
        let i = 1;
        for (let s = e.previousElementSibling; s; s = s.previousElementSibling) i++;
        steps.unshift('*[' + i + ']');
      }
      return '/' + steps.join('/');
    };
    const up = (e) => e.parentElement || (e.parentNode && e.parentNode.host) || null;
    const inside = (b, r) => b.left <= r.left + 1 && b.top <= r.top + 1 && b.right >= r.right - 1 && b.bottom >= r.bottom - 1;
    const fields = [];
    const texts = [];
    const walk = (root) => {
      for (const e of root.querySelectorAll('input, textarea, select, [contenteditable]')) {
        if (e.type === 'password' || e.__irisTyped === true || sensitive(e)) fields.push(xpath(e));
      }
      const w = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      for (let n = w.nextNode(); n; n = w.nextNode()) {
        if (!n.data.trim()) continue;
        const range = document.createRange();
        range.selectNodeContents(n);
        const r = range.getBoundingClientRect();
        let e = n.parentElement || (n.parentNode && n.parentNode.host) || null;
        while (e && e !== document.documentElement && !inside(e.getBoundingClientRect(), r)) e = up(e);
        texts.push([n.data, xpath(e || document.documentElement), r.width > 0 && r.height > 0]);
      }
      for (const e of root.querySelectorAll('*')) if (e.shadowRoot) walk(e.shadowRoot);
    };
    walk(document);
    return { fields, texts };
  };
  // Every rectangle to black out, in CSS pixels of the viewport.
  const measure = async () => {
    const targets = [];
    for (const f of page.frames()) {
      const scan = await f.evaluate(scanPage).catch(() => null);
      if (scan === null) {
        if (f.isDetached()) continue;
        // Cannot look inside: the whole frame.
        const el = await f.frameElement().catch(() => null);
        if (!el) throw new Error('the page could not be checked for private details');
        targets.push(el);
        continue;
      }
      for (const x of scan.fields) targets.push(f.locator('xpath=' + x));
      if (!secrets.length) continue;
      const seen = new Set();
      const cover = (x) => {
        if (!seen.has(x)) {
          seen.add(x);
          targets.push(f.locator('xpath=' + x));
        }
      };
      for (const [t, x] of scan.texts) {
        const l = norm(t);
        if (low.some((s) => l.includes(s))) cover(x);
      }
      // Text split over several nodes (a number styled group by group): the
      // drawn nodes in page order, joined with a space and with nothing;
      // every node a match spans is blacked out (a hidden copy elsewhere in
      // the element does not take its place).
      const drawn = scan.texts.filter((t) => t[2]);
      for (const sep of [' ', '']) {
        let stream = '';
        const owner = [];
        drawn.forEach(([t], k) => {
          const piece = norm(t).trim();
          if (stream && sep) {
            stream += sep;
            owner.push(-1);
          }
          stream += piece;
          for (let c = 0; c < piece.length; c++) owner.push(k);
        });
        // (Not short codes: digits of neighbouring prices would match.)
        for (const s of low) {
          if (s.length < 6) continue;
          for (let at = stream.indexOf(s); at !== -1; at = stream.indexOf(s, at + 1)) {
            const ks = new Set(owner.slice(at, at + s.length).filter((k) => k >= 0));
            if (ks.size > 1) for (const k of ks) cover(drawn[k][1]);
          }
        }
      }
      const fields = f.locator('input, textarea');
      const values = await fields.evaluateAll((els) => els.map((e) => String(e.value))).catch(() => null);
      if (values === null) throw new Error('the page could not be checked for private details');
      values.forEach((v, i) => {
        const l = norm(v);
        if (low.some((s) => l.includes(s))) targets.push(fields.nth(i));
      });
      // Text split over several elements: Playwright's own text search (in
      // its isolated world, out of the page's reach), one per secret (a
      // child matching another secret must not hide this one's element),
      // any white space matching any.
      for (const g of groups) {
        const alts = g.map((s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&').replace(/\s+/g, '\\s+'));
        targets.push(f.getByText(new RegExp(alts.join('|'), 'i')));
      }
    }
    const rects = [];
    const box = async (h) => {
      const b = await h.boundingBox({ timeout: 2000 }).catch(() => undefined);
      if (b === undefined) throw new Error('the page could not be checked for private details');
      return b && b.width > 0 && b.height > 0 ? b : null;
    };
    for (const t of targets) {
      const each = typeof t.all === 'function' ? await t.all() : [t];
      for (const h of each) {
        let b = await box(h);
        // Shown without a box of its own (display: contents, its parts laid
        // out in its parent's): the nearest ancestor with one. Playwright
        // tells, in its isolated world.
        if (!b && typeof h.isVisible === 'function' && await h.isVisible().catch(() => true)) {
          let up = h;
          for (let k = 0; k < 20 && !b; k++) {
            up = up.locator('xpath=..');
            b = await box(up);
          }
          if (!b) throw new Error('the page could not be checked for private details');
        }
        if (b) rects.push([b.x, b.y, b.width, b.height]);
      }
    }
    return rects;
  };
  const same = (a, b) => a.length === b.length && a.every((r, i) => r.every((v, k) => Math.abs(v - b[i][k]) < 1));
  // The element asked for, scrolled into view first.
  const target = opts.target ? page.locator('aria-ref=' + opts.target) : null;
  if (target) await target.scrollIntoViewIfNeeded({ timeout: 5000 });
  const s = await page.context().newCDPSession(page);
  try {
    for (let attempt = 0; attempt < 3; attempt++) {
      const before = await measure();
      const m = await s.send('Page.getLayoutMetrics');
      const vv = m.cssVisualViewport;
      // Only what is on screen: capturing beyond it can lay the page out
      // again (and move what was measured). An element: its part on screen.
      let x0 = 0, y0 = 0, x1 = vv.clientWidth, y1 = vv.clientHeight;
      if (target) {
        const b = await target.boundingBox({ timeout: 5000 });
        if (!b) throw new Error('that element is not on the page');
        x0 = Math.max(0, b.x); y0 = Math.max(0, b.y);
        x1 = Math.min(vv.clientWidth, b.x + b.width); y1 = Math.min(vv.clientHeight, b.y + b.height);
        if (x1 <= x0 || y1 <= y0) throw new Error('that element is not on the screen');
      }
      const clip = { x: vv.pageX + x0, y: vv.pageY + y0, width: x1 - x0, height: y1 - y0 };
      const [ox, oy] = [x0, y0];
      // In CSS pixels unless device pixels are asked for, as Playwright's
      // own (the device's pixel ratio from Chrome, not the page).
      const ratio = m.visualViewport && m.visualViewport.clientWidth > 0 ? m.visualViewport.clientWidth / vv.clientWidth : 1;
      const scale = opts.scale === 'device' ? 1 : 1 / ratio;
      const shot = await s.send('Page.captureScreenshot', { format: 'png', clip: { ...clip, scale }, captureBeyondViewport: false });
      const after = await measure();
      if (!same(before, after)) continue;
      const rects = before.concat(after).map(([x, y, w, h]) => [x - ox, y - oy, w, h]);
      return { data: shot.data, width: clip.width, height: clip.height, rects };
    }
    throw new Error('the page kept moving');
  } finally {
    s.detach().catch(() => {});
  }
}"#;

/// The focused field, followed into shadow roots and same-origin frames:
/// whether it takes private details (passwords, card fields, one-time
/// codes; a field in another site's frame cannot be inspected and counts
/// as private; so does one the owner typed private details into) and
/// whether it is in another site's frame. With `MARK`, a private field
/// marks the document private before anything is typed and is tagged with
/// edit id `EDIT` (`edit`: tagged; `empty`: it held nothing); with
/// `FORCE` it counts as private whatever it is.
const FIELD_FN: &str = r#"() => {
  const sensitive = SENSITIVE;
  let e = document.activeElement;
  let frame = false;
  let inner = false;
  for (;;) {
    while (e && e.shadowRoot && e.shadowRoot.activeElement) e = e.shadowRoot.activeElement;
    if (!e || (e.tagName !== 'IFRAME' && e.tagName !== 'FRAME')) break;
    let d = null;
    try { d = e.contentDocument; } catch (_) {}
    if (!d) { frame = true; break; }
    e = d.activeElement;
    inner = true;
  }
  const field = !!e && e !== e.ownerDocument.body && e !== e.ownerDocument.documentElement;
  // Focus inside a frame but on no field of it: where typing goes is unclear.
  if (inner && !field) frame = true;
  const priv = FORCE || frame || (field && (e.__irisTyped === true || sensitive(e)));
  let edit = false;
  if (MARK && priv) {
    try { Object.defineProperty(window, '__irisPrivate', { value: true }); } catch (_) {}
    // The field itself is remembered as private, whatever it looks like
    // later (a "show password" button makes it an ordinary text field).
    try { if (field) Object.defineProperty(e, '__irisTyped', { value: true }); } catch (_) {}
    // This change, so a read can tell it found this very field again.
    try {
      if (field && !frame) {
        e.__irisEdits = (Array.isArray(e.__irisEdits) ? e.__irisEdits : []).concat([EDIT]);
        edit = true;
      }
    } catch (_) {}
    try {
      if (!window.__irisDoc) Object.defineProperty(window, '__irisDoc', { value: Math.random().toString(36).slice(2) + Date.now().toString(36) });
    } catch (_) {}
  }
  const held = field && !frame ? String(e.isContentEditable ? e.textContent || '' : e.value || '').trim() : 'unknown';
  return { private: priv, frame, edit, empty: held === '', doc: String(window.__irisDoc || '') };
}"#;

/// What every field holds (inputs, text areas) and the text of every
/// frame of the page (also in open shadow roots), to look for saved
/// secrets Iris filled. `null` when a frame could not be read (counted as
/// holding one).
const SECRET_CHECK_CODE: &str = r#"async (page) => {
  const out = [];
  for (const f of page.frames()) {
    const r = await f.evaluate(() => {
      const vals = [document.body ? document.body.innerText : ''];
      const walk = (root) => {
        for (const e of root.querySelectorAll('*')) {
          if (e.tagName === 'INPUT' || e.tagName === 'TEXTAREA') vals.push(String(e.value));
          if (e.shadowRoot) {
            vals.push(e.shadowRoot.textContent || '');
            walk(e.shadowRoot);
          }
        }
      };
      walk(document);
      return vals;
    }).catch(() => null);
    if (r === null) {
      if (f.isDetached()) continue;
      return null;
    }
    out.push(...r);
  }
  return out.join('\n');
}"#;

/// Closes the current document to the agent for good (until it is gone):
/// it holds something private Iris cannot hide, which no read-back undoes.
pub(crate) const CLOSE_JS: &str = "(() => { try { Object.defineProperty(window, '__irisPrivate', { value: true }); } catch (_) {} try { Object.defineProperty(window, '__irisClosed', { value: true }); } catch (_) {} })()";

/// Run `FIELD_FN` from Playwright's Node side, with the real address.
/// In another site's frame (Playwright evaluates there; the page cannot):
/// the focused field of the innermost focused document, tagged as typed
/// into and with edit id `EDIT` (`empty`: it held nothing; an expiry-only
/// field is not tagged, `edit: false`). `null` when this frame does not
/// hold the focus.
const FRAME_FIELD_FN: &str = r#"() => {
  if (!document.hasFocus()) return null;
  let e = document.activeElement;
  while (e && e.shadowRoot && e.shadowRoot.activeElement) e = e.shadowRoot.activeElement;
  if (!e || e.tagName === 'IFRAME' || e.tagName === 'FRAME' || e === document.body || e === document.documentElement) return null;
  // An expiry month or year is not private (`SECRET_FIELD_FN`).
  if ((EXPIRY)(e)) return { edit: false, empty: false };
  try { Object.defineProperty(e, '__irisTyped', { value: true }); } catch (_) {}
  e.__irisEdits = (Array.isArray(e.__irisEdits) ? e.__irisEdits : []).concat([EDIT]);
  const held = String(e.isContentEditable ? e.textContent || '' : e.value || '').trim();
  return { edit: true, empty: held === '' };
}"#;

fn field_code(mark: bool, force: bool, edit: u64) -> String {
    let f = FIELD_FN
        .replace("SENSITIVE", &secret_field_fn())
        .replace("FORCE", if force { "true" } else { "false" })
        .replace("MARK", if mark { "true" } else { "false" })
        .replace("EDIT", &edit.to_string());
    let tag = FRAME_FIELD_FN
        .replace("EDIT", &edit.to_string())
        .replace("EXPIRY", EXPIRY_ONLY_FN);
    // Typing into another site's frame: its field is tagged from that
    // frame (`edit: "unknown"` when it cannot be found: nothing read later
    // can count as finding it again).
    format!(
        "async (page) => {{ const r = await page.evaluate({}); \
         if ({mark} && r.private && r.frame) {{ r.edit = 'unknown'; \
         for (const f of page.frames()) {{ if (f === page.mainFrame()) continue; \
         const t = await f.evaluate({}).catch(() => null); \
         if (t) {{ r.edit = t.edit; r.empty = t.empty; break; }} }} }} \
         return {{ url: page.url(), ...r }}; }}",
        serde_json::to_string(&format!("({f})()")).unwrap_or_default(),
        serde_json::to_string(&format!("({tag})()")).unwrap_or_default(),
    )
}

/// The focused field, as `FIELD_FN` sees it, and the page's real address.
#[derive(Debug)]
pub(crate) struct Field {
    pub url: String,
    pub private: bool,
    /// In another site's frame.
    pub frame: bool,
    /// The id Iris gave the document when it was marked (empty: none).
    pub doc: String,
    /// The edit id the field was tagged with (marking a private field).
    pub edit: Option<u64>,
    /// It held nothing.
    pub empty: bool,
}

/// Keys the owner can press while in control (names as in the DOM).
const KEYS: &[&str] = &[
    "Enter",
    "Tab",
    "Escape",
    "Backspace",
    "Delete",
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "Home",
    "End",
    "PageUp",
    "PageDown",
];

/// How a value shows up once a page or Playwright has handled it: sent by
/// a form (`application/x-www-form-urlencoded`), escaped by
/// `encodeURIComponent`, or quoted in a snapshot (JSON-style escapes;
/// snapshots also trim values and collapse runs of whitespace).
fn written_forms(secret: &str) -> Vec<String> {
    let encode = |keep: &dyn Fn(u8) -> bool, space_plus: bool| -> String {
        secret
            .bytes()
            .map(|b| {
                if b == b' ' && space_plus {
                    "+".to_string()
                } else if b.is_ascii_alphanumeric() || keep(b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    };
    let form = encode(&|b| b"*-._".contains(&b), true);
    let component = encode(&|b| b"-_.!~*'()".contains(&b), false);
    let quoted = |s: &str| {
        let json = serde_json::to_string(s).unwrap_or_default();
        json[1..json.len().saturating_sub(1).max(1)].to_string()
    };
    // Snapshots trim a value and collapse its runs of whitespace.
    let collapsed = secret.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut forms = vec![form, component, quoted(secret)];
    if collapsed != secret && collapsed.chars().count() >= 4 {
        forms.push(quoted(&collapsed));
        forms.push(collapsed);
    }
    forms.retain(|f| f != secret && !f.is_empty());
    forms.sort();
    forms.dedup();
    forms
}

/// What the owner typed into a private field, as Iris passed it on: hidden
/// like a secret too, should the page move it out of the field before it
/// is read back (another document may show it). Typing that began in an
/// empty field covers its edits; any other change needs the field read
/// again (`Private::edits`).
fn flush_typed(p: &mut Private) {
    let typed = std::mem::take(&mut p.typed_text);
    let typed = typed.trim();
    let edits = std::mem::take(&mut p.run_edits);
    if fill::watchable(typed) {
        add_secret(p, typed);
        // Typed into an empty field (Backspace only taking back the last
        // character): what was typed is what it holds, so it is covered
        // even should the page remove the field by itself (a code sent as
        // soon as it is complete).
        if std::mem::take(&mut p.run_from_empty) {
            p.edits.retain(|(_, id)| !edits.contains(id));
        }
    }
    p.run_from_empty = false;
}

/// A private value read back or typed: hidden from now on.
fn add_secret(p: &mut Private, value: &str) {
    if !p.secrets.iter().any(|s| s == value) {
        p.secrets.push(value.to_string());
    }
    p.filled = true;
}

/// A card number written in groups (`4111 1111 1111 1111`, `4111-1111-…`):
/// its digits, so that every other way of writing it is hidden too.
fn card_digits(s: &str) -> Option<String> {
    let digits: String = s.chars().filter(char::is_ascii_digit).collect();
    let grouped = s
        .chars()
        .all(|c| c.is_ascii_digit() || c == ' ' || c == '-');
    (grouped && digits.len() != s.len() && (12..=19).contains(&digits.len())).then_some(digits)
}

/// `secrets` with the digits of card numbers written in groups.
fn with_card_digits(secrets: &[String]) -> Vec<String> {
    let mut out = secrets.to_vec();
    for d in secrets.iter().filter_map(|s| card_digits(s)) {
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// Every written form of `secrets` that `hide_secrets` hides, to mask in
/// pictures, one group per secret: each secret, its encodings, and card
/// numbers written in groups (also one known only in groups).
pub(crate) fn mask_form_groups(secrets: &[String]) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<String>> = Vec::new();
    for secret in secrets {
        let mut out: Vec<String> = Vec::new();
        for s in std::iter::once(secret.clone()).chain(card_digits(secret)) {
            let s = &s;
            let n = s.chars().count();
            let mut forms = vec![s.clone()];
            if n >= 4 {
                forms.extend(written_forms(s));
            }
            if n >= 8 {
                forms.extend(crate::gateway::encoded_secret_forms(s));
            }
            let digits = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            if digits && s.len() >= 12 {
                let groups: &[usize] = if s.len() == 15 {
                    &[4, 6, 5]
                } else {
                    &[4, 4, 4, 4, 4]
                };
                let mut parts = Vec::new();
                let mut at = 0;
                for g in groups {
                    if at >= s.len() {
                        break;
                    }
                    let end = (at + g).min(s.len());
                    parts.push(&s[at..end]);
                    at = end;
                }
                for sep in [" ", "-", "  ", " - "] {
                    forms.push(parts.join(sep));
                }
            }
            for f in forms {
                if !f.is_empty() && !out.contains(&f) {
                    out.push(f);
                }
            }
        }
        if !out.is_empty() {
            groups.push(out);
        }
    }
    groups
}

/// `text` with saved secrets replaced by `[hidden]`: the gateway's matcher
/// (the value and its common encodings), the forms pages and Playwright
/// write values in (`written_forms`, any length), and what pages do with
/// card details: numbers split into groups by spaces or dashes (also one
/// known only in groups, and groups shown apart, `hide_split_cards`), and
/// short security codes (only where they stand alone, not inside longer
/// numbers).
pub(crate) fn hide_secrets(text: &str, secrets: &[String]) -> String {
    let secrets = &with_card_digits(secrets)[..];
    let mut out = crate::gateway::redact(text, secrets);
    for s in secrets.iter().filter(|s| s.chars().count() >= 4) {
        for f in written_forms(s) {
            // Escapes may be written in either case (%2b, %2B).
            out = if f.contains('%') {
                crate::gateway::replace_case_insensitive(&out, &f, "[hidden]")
            } else {
                out.replace(&f, "[hidden]")
            };
        }
    }
    for s in secrets {
        let digits = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if digits && s.len() >= 12 {
            let groups: &[usize] = if s.len() == 15 {
                &[4, 6, 5]
            } else {
                &[4, 4, 4, 4, 4]
            };
            let mut parts = Vec::new();
            let mut at = 0;
            for g in groups {
                if at >= s.len() {
                    break;
                }
                let end = (at + g).min(s.len());
                parts.push(&s[at..end]);
                at = end;
            }
            for sep in [" ", "-", "  ", " - "] {
                out = out.replace(&parts.join(sep), "[hidden]");
            }
        } else if digits && (3..=4).contains(&s.len()) {
            out = hide_standalone(&out, s);
        }
    }
    hide_split_cards(&out, secrets)
}

/// Card numbers shown group by group in separate parts of a page (a card
/// preview: a snapshot lists each group as its own entry): runs of digits
/// standing apart (not part of a name such as `e12`), close together, that
/// make up at least eight digits of a known card number in order are
/// hidden, each run.
fn hide_split_cards(text: &str, secrets: &[String]) -> String {
    let cards: Vec<&String> = secrets
        .iter()
        .filter(|s| (12..=19).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    if cards.is_empty() {
        return text.to_string();
    }
    let b = text.as_bytes();
    let word = |i: usize| b[i].is_ascii_alphanumeric() || b[i] == b'_';
    // Runs of digits not touching a letter.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let alone = (start == 0 || !word(start - 1)) && (i == b.len() || !word(i));
        if alone {
            runs.push((start, i));
        }
    }
    const NEAR: usize = 80;
    let mut hide = vec![false; runs.len()];
    for first in 0..runs.len() {
        let mut digits = String::new();
        for last in first..runs.len() {
            if last > first && runs[last].0 - runs[last - 1].1 > NEAR {
                break;
            }
            digits.push_str(&text[runs[last].0..runs[last].1]);
            if digits.len() > 19 || !cards.iter().any(|c| c.contains(&digits)) {
                break;
            }
            if last > first && digits.len() >= 8 {
                hide[first..=last].iter_mut().for_each(|h| *h = true);
            }
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (k, (start, end)) in runs.iter().enumerate() {
        if hide[k] {
            out.push_str(&text[at..*start]);
            out.push_str("[hidden]");
            at = *end;
        }
    }
    out.push_str(&text[at..]);
    out
}

/// Replace `code` where no other letter or digit touches it (so neither a
/// longer number nor a snapshot ref like `e737` is cut).
fn hide_standalone(text: &str, code: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut i = 0;
    while let Some(pos) = text[i..].find(code) {
        let start = i + pos;
        let end = start + code.len();
        let before = start > 0 && b[start - 1].is_ascii_alphanumeric();
        let after = end < b.len() && b[end].is_ascii_alphanumeric();
        if !before && !after {
            out.push_str(&text[last..start]);
            out.push_str("[hidden]");
            last = end;
        }
        i = end;
    }
    out.push_str(&text[last..]);
    out
}

/// An address the owner typed: http(s), or a bare domain (https).
pub fn normalize_url(raw: &str) -> Result<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 4000 || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("enter a web address".into());
    }
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(s.to_string());
    }
    if tools::has_scheme(s) && !tools::host_port(s) {
        return Err("only web pages (http or https) can be opened".into());
    }
    Ok(format!("https://{s}"))
}

impl Browser {
    pub fn new(cfg: BrowserConfig, paths: &Paths, hub: Hub) -> Self {
        let dir = paths.browser_dir();
        // Leftovers of Iris's earlier browser layer (encrypted redaction
        // history, hashed origins, scratch space): no longer used.
        let _ = std::fs::remove_file(dir.join("redaction.enc"));
        let _ = std::fs::remove_file(dir.join("tainted-origins.json"));
        let _ = std::fs::remove_dir_all(dir.join("tmp"));
        let work_dir = dir.join("work");
        Self(Arc::new(Shared {
            cfg,
            profile_dir: dir.join("profile"),
            out_dir: work_dir.join("out"),
            work_dir,
            config_file: dir.join("playwright.json"),
            hub,
            launch_lock: tokio::sync::Mutex::new(()),
            list_lock: tokio::sync::Mutex::new(()),
            running: Mutex::new(None),
            view: Mutex::default(),
            control: watch::Sender::new(Control::Agent),
            frames: watch::Sender::new(None),
            frame_seq: AtomicU64::new(0),
            watchers: Mutex::default(),
            next_watcher: AtomicU64::new(1),
            casting: AtomicBool::new(false),
            turns: Mutex::default(),
            recent: Mutex::default(),
            last_used: AtomicI64::new(0),
            last_input: AtomicI64::new(0),
            op_lock: tokio::sync::Mutex::new(()),
            capture_lock: tokio::sync::Mutex::new(()),
            paused: AtomicBool::new(false),
            disconnected: AtomicBool::new(false),
            allow_no_sandbox: AtomicBool::new(false),
            maintenance: AtomicBool::new(false),
            switch_lock: tokio::sync::Mutex::new(()),
            tools: Mutex::new(None),
            private: Mutex::default(),
            secret_source: Mutex::new(None),
            #[cfg(test)]
            server_override: Mutex::new(None),
        }))
    }

    /// Playwright MCP and the Chromium it should start (see `launch.rs`).
    fn server(&self) -> Option<launch::Server> {
        #[cfg(test)]
        if let Some(s) = lock(&self.server_override).clone() {
            return Some(s);
        }
        launch::find(&self.cfg.command)
    }

    /// Stop/Pause: the turns of a conversation (or all) end now for the
    /// browser, even before the CLI has exited.
    pub fn end_turns(&self, conversation_id: Option<&str>) {
        for t in lock(&self.turns).values() {
            if conversation_id.is_none_or(|c| c == t.conversation_id) {
                t.ended.send_replace(true);
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled && !self.disconnected.load(Ordering::SeqCst)
    }

    /// Playwright MCP (and the configured Chromium, if any) is installed.
    pub fn available(&self) -> bool {
        self.server().is_some()
    }

    /// The Chromium the server would start, when Iris knows it (Chrome's
    /// sandbox fix names its real path).
    pub fn chromium(&self) -> Option<PathBuf> {
        self.server().and_then(|s| s.chromium)
    }

    pub fn running(&self) -> Option<Arc<Running>> {
        lock(&self.running).clone().filter(|r| r.alive())
    }

    pub fn control(&self) -> Control {
        self.control.borrow().clone()
    }

    pub fn set_disconnected(&self, disconnected: bool) {
        self.disconnected.store(disconnected, Ordering::SeqCst);
        if disconnected {
            self.end_turns(None);
        }
        self.broadcast_state();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Startup: restore the saved pause switch.
    pub fn init_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    pub fn allows_no_sandbox(&self) -> bool {
        self.allow_no_sandbox.load(Ordering::SeqCst)
    }

    /// The owner's choice about running without Chrome's sandbox. Taking
    /// it back also closes an unsandboxed browser, including one that is
    /// starting right now.
    pub async fn set_allow_no_sandbox(&self, allow: bool) {
        self.allow_no_sandbox.store(allow, Ordering::SeqCst);
        if allow {
            lock(&self.view).sandbox_unavailable = false;
            return;
        }
        let unsandboxed = {
            let _g = self.launch_lock.lock().await;
            self.running().is_some_and(|r| r.no_sandbox)
        };
        if unsandboxed {
            self.close().await;
        }
    }

    /// Startup: restore the owner's sandbox choice.
    pub fn init_allow_no_sandbox(&self, allow: bool) {
        self.allow_no_sandbox.store(allow, Ordering::SeqCst);
    }

    /// While the browser's files are being installed, updated, or removed,
    /// it is closed and cannot start.
    pub async fn begin_maintenance(&self) {
        self.maintenance.store(true, Ordering::SeqCst);
        self.close().await;
        // A new version may offer other tools.
        *lock(&self.tools) = None;
    }

    pub fn end_maintenance(&self) {
        self.maintenance.store(false, Ordering::SeqCst);
    }

    /// Pause (or resume) Iris. Pausing closes the browser; the caller stops
    /// running turns.
    pub async fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
        if paused {
            self.close().await;
        }
        self.broadcast_state();
    }

    // ------------------------------------------------------------- turns

    /// Register a turn that is offered the browser tools.
    #[cfg(test)]
    pub fn begin_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        message_id: &str,
        audience: &[String],
    ) -> TurnGuard {
        self.begin_turn_with(user_id, conversation_id, message_id, audience, false)
    }

    /// [`Self::begin_turn`], optionally look-only (preparing an ask-first
    /// routine).
    pub fn begin_turn_with(
        &self,
        user_id: &str,
        conversation_id: &str,
        message_id: &str,
        audience: &[String],
        look_only: bool,
    ) -> TurnGuard {
        let token = format!("{}{}", new_id(), new_id());
        lock(&self.turns).insert(
            token.clone(),
            Arc::new(TurnCtx {
                user_id: user_id.into(),
                conversation_id: conversation_id.into(),
                message_id: message_id.into(),
                audience: audience.to_vec(),
                look_only,
                used: AtomicBool::new(false),
                ended: watch::Sender::new(false),
            }),
        );
        TurnGuard {
            browser: self.clone(),
            token,
        }
    }

    pub fn turn(&self, token: &str) -> Option<Arc<TurnCtx>> {
        if token.is_empty() {
            return None;
        }
        lock(&self.turns).get(token).cloned()
    }

    /// First browser tool call of a turn: show the peek in that chat.
    pub fn mark_used(&self, ctx: &TurnCtx) {
        self.last_used.store(now_ms(), Ordering::SeqCst);
        if !ctx.used.swap(true, Ordering::SeqCst) {
            // This turn's peek replaces the conversation's earlier one.
            lock(&self.recent).remove(&ctx.conversation_id);
            self.hub.to_users(
                &ctx.audience,
                json!({"type": "browser.active", "conversation_id": ctx.conversation_id,
                       "message_id": ctx.message_id}),
            );
            self.broadcast_state();
        }
    }

    /// Turns whose chat shows the browser peek: running ones that used it,
    /// then the last finished one per conversation (with when it ended and
    /// how much longer it stays at least).
    fn active_turns(&self) -> Vec<Value> {
        let mut out: Vec<Value> = lock(&self.turns)
            .values()
            .filter(|t| t.used.load(Ordering::SeqCst))
            .map(|t| json!({"conversation_id": t.conversation_id, "message_id": t.message_id}))
            .collect();
        let now = now_ms();
        out.extend(lock(&self.recent).iter().map(|(conv, (msg, ended))| {
            json!({"conversation_id": conv, "message_id": msg, "ended_at": ended,
                   "linger_ms": (ended + LINGER_MS - now).clamp(0, LINGER_MS)})
        }));
        out
    }

    /// Wait until the agent may drive (the owner is not in control). False
    /// if the owner keeps control for a while or the turn ends.
    pub async fn wait_for_agent(&self, ctx: &TurnCtx) -> bool {
        let mut rx = self.control.subscribe();
        tokio::select! {
            r = tokio::time::timeout(CONTROL_WAIT, rx.wait_for(|c| *c == Control::Agent)) => {
                r.is_ok_and(|r| r.is_ok()) && !ctx.is_ended()
            }
            _ = ctx.wait_ended() => false,
        }
    }

    /// Checked right before every action on the page: the turn is still
    /// running, Iris is not paused, and the owner has not taken over.
    pub fn check_agent(&self, ctx: &TurnCtx) -> Result<()> {
        if ctx.is_ended() {
            return Err(TURN_ENDED.into());
        }
        if self.disconnected.load(Ordering::SeqCst) {
            return Err(DISCONNECTED.into());
        }
        if self.is_paused() {
            return Err("The owner paused Iris, so the browser is off.".into());
        }
        if self.control() != Control::Agent {
            return Err(TAKEN_OVER.into());
        }
        Ok(())
    }

    pub fn set_busy(&self, busy: bool) {
        let changed = {
            let mut v = lock(&self.view);
            let changed = v.busy != busy;
            v.busy = busy;
            changed
        };
        if changed {
            self.broadcast_state();
        }
    }

    // ------------------------------------------------------------ status

    pub fn state_json(&self) -> Value {
        let v = lock(&self.view).clone();
        let run = self.running();
        let control = self.control();
        json!({
            "enabled": self.cfg.enabled,
            "disconnected": self.disconnected.load(Ordering::SeqCst),
            "available": self.available(),
            "running": run.is_some(),
            "paused": self.is_paused(),
            "url": if run.is_some() { v.url } else { String::new() },
            "title": if run.is_some() { v.title } else { String::new() },
            "controller": match &control { Control::Agent => "agent", Control::User { .. } => "user" },
            "controller_device_id": match &control { Control::User { device_id, .. } => Some(device_id.clone()), _ => None },
            "busy": v.busy && run.is_some(),
            "active": self.active_turns(),
            "watchers": lock(&self.watchers).len(),
            "streaming": run.is_some() && self.casting.load(Ordering::SeqCst),
            "sandbox": run.as_ref().map(|r| !r.no_sandbox),
            "sandbox_unavailable": v.sandbox_unavailable,
            "allow_no_sandbox": self.allows_no_sandbox(),
            "error": v.error,
        })
    }

    /// Tell the owner's apps (only the owner can see Iris's computer).
    pub fn broadcast_state(&self) {
        self.hub.send(
            crate::events::Audience::Owner,
            json!({"type": "browser.state", "browser": self.state_json()}),
        );
    }

    fn set_page_info(&self, url: Option<&str>, title: Option<&str>) {
        // A page may carry a saved secret in its address or title: never
        // published, even to the owner's app.
        let secrets = self.filled_secrets();
        let hide = |t: &str| {
            if secrets.is_empty() {
                t.to_string()
            } else {
                hide_secrets(t, &secrets)
            }
        };
        let (url, title) = (url.map(hide), title.map(hide));
        let (url, title) = (url.as_deref(), title.as_deref());
        let changed = {
            let mut v = lock(&self.view);
            let mut changed = false;
            if let Some(u) = url
                && v.url != u
            {
                v.url = u.to_string();
                changed = true;
            }
            if let Some(t) = title
                && v.title != t
            {
                v.title = t.to_string();
                changed = true;
            }
            changed
        };
        if changed {
            self.broadcast_state();
        }
    }

    /// Keep the app's address bar current from a tool result's page
    /// header (`current_url` corrects it from the browser itself).
    fn note_page(&self, result: &Value) {
        let text = text_of(result);
        let Some(page) = page_section(&text) else {
            return;
        };
        let url = line_value(page, "- Page URL:");
        let title = line_value(page, "- Page Title:");
        if url.is_some() || title.is_some() {
            self.set_page_info(url, title);
        }
    }

    // ------------------------------------------------------------ launch

    /// Why the browser cannot be used right now, if it cannot.
    fn usable(&self) -> Result<()> {
        if !self.cfg.enabled {
            return Err(
                "the browser is turned off in the server's config ([browser] enabled)".into(),
            );
        }
        if self.disconnected.load(Ordering::SeqCst) {
            return Err(DISCONNECTED.into());
        }
        if self.is_paused() {
            return Err("Iris is paused".into());
        }
        Ok(())
    }

    /// The running browser, starting it if needed.
    pub async fn ensure(&self) -> Result<Arc<Running>> {
        /// Learned once per run: this system cannot start Chrome's sandbox.
        static NO_SANDBOX: AtomicBool = AtomicBool::new(false);
        self.usable()?;
        if let Some(r) = self.running() {
            return Ok(r);
        }
        let _g = self.launch_lock.lock().await;
        if self.maintenance.load(Ordering::SeqCst) {
            return Err("the browser is being updated; try again in a minute".into());
        }
        // Pause may have come while waiting for the lock.
        self.usable()?;
        if let Some(r) = self.running() {
            return Ok(r);
        }
        let server = self.server().ok_or(NOT_INSTALLED)?;
        crate::info!("browser: starting {}", server.program.display());
        let mut no_sandbox = self.allows_no_sandbox() && NO_SANDBOX.load(Ordering::Relaxed);
        let run = loop {
            match self.launch(&server, no_sandbox).await {
                Ok(run) => break run,
                Err(Start::Sandbox) if self.allows_no_sandbox() => {
                    crate::warn!(
                        "browser: Chrome's sandbox is unavailable on this system; running \
                         without it (allowed by the owner)"
                    );
                    NO_SANDBOX.store(true, Ordering::Relaxed);
                    no_sandbox = true;
                }
                Err(e) => {
                    let (sandbox, msg) = match e {
                        Start::Sandbox => (true, SANDBOX_UNAVAILABLE.to_string()),
                        Start::Failed(m) => (false, m),
                    };
                    crate::warn!("browser: could not start: {msg}");
                    {
                        let mut v = lock(&self.view);
                        v.sandbox_unavailable = sandbox;
                        v.error = Some(msg.clone());
                    }
                    self.broadcast_state();
                    return Err(msg.into());
                }
            }
        };
        // Paused, or the owner took back running without the sandbox,
        // while it was starting: do not publish it.
        let stop = self.usable().err().or_else(|| {
            (run.no_sandbox && !self.allows_no_sandbox()).then(|| SANDBOX_UNAVAILABLE.into())
        });
        if let Some(e) = stop {
            self.shut(&run).await;
            return Err(e);
        }
        {
            let mut v = lock(&self.view);
            v.error = None;
            v.sandbox_unavailable = false;
            v.url = "about:blank".into();
            v.title = String::new();
        }
        // Saved secrets are hidden from the first result on (a site may
        // show one it kept from an earlier visit); read before the browser
        // is published to anyone.
        let source = lock(&self.secret_source).clone();
        let saved = match source {
            Some(f) => tokio::task::spawn_blocking(move || f())
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        *lock(&self.private) = Private {
            secrets: saved,
            ..Private::default()
        };
        *lock(&self.running) = Some(run.clone());
        self.last_used.store(now_ms(), Ordering::SeqCst);
        let b = self.clone();
        let r = run.clone();
        tokio::spawn(async move { b.idle_loop(r).await });
        self.update_screencast();
        self.broadcast_state();
        crate::info!("browser: ready");
        Ok(run)
    }

    /// Start the server with Iris's config at `config` (its own file, so
    /// a second server only asked for its tool list never changes the
    /// running one's).
    async fn start_server(
        &self,
        server: &launch::Server,
        no_sandbox: bool,
        config: &Path,
    ) -> Result<Upstream> {
        for dir in [&self.profile_dir, &self.work_dir] {
            crate::config::ensure_private_dir(dir)?;
        }
        if let Some(dir) = config.parent() {
            crate::config::ensure_private_dir(dir)?;
        }
        let cfg = launch::config(server, &self.profile_dir, &self.out_dir, no_sandbox);
        crate::config::write_private_file(config, cfg.to_string().as_bytes())?;
        Upstream::start(
            launch::command(server, config, &self.work_dir),
            START_TIMEOUT,
        )
        .await
        .map_err(|e| format!("the browser server did not start: {e}").into())
    }

    /// Start the server and Chromium (which otherwise starts on the first
    /// call), so problems show here.
    async fn launch(
        &self,
        server: &launch::Server,
        no_sandbox: bool,
    ) -> std::result::Result<Arc<Running>, Start> {
        // Nothing of an earlier session's output or typed form data stays.
        let _ = std::fs::remove_dir_all(&self.out_dir);
        launch::clear_session_files(&self.profile_dir);
        if let Err(e) = launch::set_profile_prefs(&self.profile_dir) {
            crate::warn!("browser: could not set profile preferences: {e}");
        }
        let up = self
            .start_server(server, no_sandbox, &self.config_file)
            .await
            .map_err(|e| Start::Failed(e.to_string()))?;
        let opened = up
            .call_tool("browser_tabs", &json!({"action": "list"}), START_TIMEOUT)
            .await;
        let problem = match &opened {
            Ok(r) if !is_error(r) => None,
            Ok(r) => Some(text_of(r)),
            Err(e) => Some(format!("{e}\n{}", up.stderr_tail())),
        };
        if let Some(text) = problem {
            up.close().await;
            return Err(if !no_sandbox && launch::sandbox_failure(&text) {
                Start::Sandbox
            } else {
                Start::Failed(launch::explain_failure(&text))
            });
        }
        Ok(Arc::new(Running { up, no_sandbox }))
    }

    /// One call to the server, for Iris itself or forwarded for the agent
    /// (the result as the server sent it).
    pub(crate) async fn tool(
        &self,
        run: &Running,
        name: &str,
        args: &Value,
        timeout: Duration,
    ) -> Result<Value> {
        let r = run.up.call_tool(name, args, timeout).await;
        // Nothing Playwright writes is for anyone: screenshots come back
        // inline, and its console logs may hold what a page printed.
        self.clear_output();
        let r = r.map_err(|e| format!("the browser did not answer ({e})"))?;
        self.note_page(&r);
        Ok(r)
    }

    /// Empty Playwright's output folder (it is recreated as needed). Also
    /// run once a minute and after an abandoned call, for files written
    /// after their call stopped waiting.
    pub(crate) fn clear_output(&self) {
        for e in std::fs::read_dir(&self.out_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = e.path();
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }

    /// Run Iris's own fixed Playwright code (never the agent's) and read
    /// its JSON result.
    pub(crate) async fn run_code(&self, run: &Running, code: &str) -> Result<Value> {
        self.run_code_within(run, code, QUICK).await
    }

    async fn run_code_within(&self, run: &Running, code: &str, timeout: Duration) -> Result<Value> {
        let r = self
            .tool(
                run,
                "browser_run_code_unsafe",
                &json!({"code": code}),
                timeout,
            )
            .await?;
        let text = ok_text(&r)?;
        self.remove_output(&text);
        result_value(&text).ok_or_else(|| "the browser gave no answer".into())
    }

    /// The current tab: its address and title (from the browser, not from
    /// anything the page wrote) and whether the owner typed private
    /// details into this document.
    pub(crate) async fn page_info(&self, run: &Running) -> Result<(String, bool)> {
        let (url, marked, captured, closed) = self.page_state(run).await?;
        Ok((url, closed || (marked && !captured)))
    }

    /// The current tab's address, whether the owner typed private details
    /// into this document, whether Iris read them back since, and whether
    /// it was closed for good (`CLOSE_JS`).
    async fn page_state(&self, run: &Running) -> Result<(String, bool, bool, bool)> {
        let generation = lock(&self.private).typed_gen;
        let v = self.run_code(run, &page_code(generation)).await?;
        let url = v["url"].as_str().ok_or("the browser has no open page")?;
        self.set_page_info(Some(url), v["title"].as_str());
        Ok((
            url.to_string(),
            v["private"] == true,
            v["captured"] == true,
            v["closed"] == true,
        ))
    }

    /// The current tab's address.
    pub(crate) async fn current_url(&self, run: &Running) -> Result<String> {
        Ok(self.page_info(run).await?.0)
    }

    /// Read back what the owner typed into private fields (in every tab
    /// and frame), so it is hidden from the agent like a saved secret and
    /// the page stays usable. A tab read completely is marked as captured;
    /// its address is forgotten when the typing happened in that very
    /// document. Anything not read keeps its page closed.
    pub(crate) async fn capture_typed(&self, run: &Running) -> Result<()> {
        let (generation, all_frames) = {
            let p = lock(&self.private);
            (p.typed_gen, p.typed_in_frame)
        };
        let v = self.run_code(run, &capture_code(all_frames)).await?;
        let tabs = v.as_array().ok_or("the browser gave no answer")?.clone();
        // The values first: a tab is only marked as read back once they are
        // hidden (a read cut short marks nothing).
        {
            let mut p = lock(&self.private);
            for t in &tabs {
                for value in t["values"].as_array().into_iter().flatten() {
                    let Some(value) = value.as_str() else {
                        continue;
                    };
                    add_secret(&mut p, value);
                }
            }
            if p.typed_gen != generation {
                // Typed again meanwhile: read again next time.
                return Ok(());
            }
        }
        // A field the owner changed and this read did not find again (the
        // page may have moved its value out of it, or removed it) keeps its
        // document from counting as read back.
        let found = |t: &Value, id: u64| {
            t["edits"]
                .as_array()
                .is_some_and(|e| e.iter().any(|n| n.as_u64() == Some(id)))
        };
        let complete = |t: &Value| {
            let p = lock(&self.private);
            p.edits
                .iter()
                .filter(|(d, _)| t["doc"] == d.as_str())
                .all(|(_, id)| found(t, *id))
        };
        let want: Vec<Value> = tabs
            .iter()
            .filter(|t| t["captured"] == true && complete(t))
            .map(|t| json!({"i": t["i"], "url": t["url"]}))
            .collect();
        let marked: Vec<u64> = if want.is_empty() {
            Vec::new()
        } else {
            let code = MARK_CAPTURED_CODE
                .replace("WANT", &Value::Array(want).to_string())
                .replace("GEN", &generation.to_string());
            let done = self.run_code(run, &code).await?;
            done.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .collect()
        };
        let mut p = lock(&self.private);
        for t in &tabs {
            // Addresses typed into in a document now read back are no longer
            // needed (its address may have changed since, in a single-page
            // site).
            let doc = t["doc"].as_str().unwrap_or("");
            if t["marked"] == true
                && !doc.is_empty()
                && t["i"].as_u64().is_some_and(|i| marked.contains(&i))
            {
                p.pages.retain(|(_, d)| d != doc);
                // Its changed fields were found again, every value hidden.
                p.edits.retain(|(d, _)| d != doc);
            }
        }
        // Read again next time while a private document could not be read
        // back (a frame that failed may answer later; the page may change).
        let all_read = tabs
            .iter()
            .all(|t| t["marked"] != true || t["i"].as_u64().is_some_and(|i| marked.contains(&i)));
        if p.typed_gen == generation {
            p.typed_dirty = !all_read;
        }
        Ok(())
    }

    /// Every field of the current tab and whether it takes private details
    /// (`FIELDS_CODE`), read on Iris's side.
    pub(crate) async fn private_fields(&self, run: &Running) -> Result<Fields> {
        let v = self
            .run_code(run, &FIELDS_CODE.replace("SENSITIVE", &secret_field_fn()))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// After an action on a page holding a secret: a private field (or one
    /// that held a secret) now holding something else that is not hidden
    /// (a number field's arrows turned 737 into 738, the page cut a value)
    /// is hidden too, or the page closes when it cannot be (answers true:
    /// the action's result must not reach the agent). Run before the
    /// action's result reaches the agent.
    pub(crate) async fn hide_changed(&self, run: &Running, before: &Fields) -> bool {
        let after = match self.private_fields(run).await {
            Ok(a) => a,
            Err(e) => {
                crate::warn!("browser: could not check the page after an action: {e}");
                self.close_current(run).await;
                return true;
            }
        };
        let secrets = self.filled_secrets();
        let holds = |v: &str| !v.is_empty() && hide_secrets(v, &secrets) != v;
        let mut changed: Vec<String> = Vec::new();
        let mut unknown = false;
        for (i, a) in after.iter().enumerate() {
            let b = before.get(i).and_then(Option::as_ref);
            let Some(a) = a else {
                unknown |= b.is_some_and(|b| b.iter().any(|(v, _)| holds(v)));
                continue;
            };
            match b {
                Some(b) if b.len() == a.len() => {
                    for ((v, private), (old, _)) in a.iter().zip(b) {
                        if v != old && !v.trim().is_empty() && !holds(v) && (*private || holds(old))
                        {
                            changed.push(v.clone());
                        }
                    }
                }
                _ => {
                    let olds: Vec<&String> = b.into_iter().flatten().map(|(v, _)| v).collect();
                    for (v, private) in a {
                        if *private && !v.trim().is_empty() && !holds(v) && !olds.contains(&v) {
                            changed.push(v.clone());
                        }
                    }
                }
            }
        }
        let mut close = unknown;
        for v in changed {
            if fill::watchable(&v) {
                self.remember_secret(&v);
                self.note_filled();
            } else {
                close = true;
            }
        }
        if close {
            self.close_current(run).await;
        }
        close
    }

    /// Close the current document to the agent (as if the owner typed
    /// something private into it that Iris cannot hide).
    pub(crate) async fn close_current(&self, run: &Running) {
        let url = self
            .page_info(run)
            .await
            .map(|(u, _)| u)
            .unwrap_or_default();
        if !url.is_empty() {
            self.remember_private(&url);
        }
        let close = format!(
            "async (page) => {{ /* iris:close */ await page.evaluate({}); return true; }}",
            serde_json::to_string(CLOSE_JS).unwrap_or_default()
        );
        let _ = self.run_code(run, &close).await;
    }

    /// The owner (or a fill Iris cannot hide) put private details into the
    /// page: earlier reads no longer count, and the next look reads again.
    pub(crate) fn note_private_input(&self, in_frame: bool) {
        let mut p = lock(&self.private);
        p.typed_dirty = true;
        p.typed_gen += 1;
        p.typed_in_frame |= in_frame;
    }

    /// A picture of the page for the agent, with private details blacked
    /// out (`SHOT_CODE`), as a tool result.
    pub(crate) async fn masked_screenshot(&self, run: &Running, args: &Value) -> Result<Value> {
        use base64::Engine as _;
        let target = args["target"].as_str().or_else(|| args["ref"].as_str());
        if let Some(t) = target
            && !tools::is_ref(t)
        {
            return Err("Use a snapshot ref (like e12) as `target` here.".into());
        }
        let scale = if args["scale"] == "device" {
            "device"
        } else {
            "css"
        };
        let opts = json!({"target": target, "scale": scale});
        let code = shot_code(&mask_form_groups(&self.filled_secrets()), &opts)?;
        let v = {
            let _one = self.capture_lock.lock().await;
            self.run_code_within(run, &code, CALL_TIMEOUT).await?
        };
        let b64 = base64::engine::general_purpose::STANDARD;
        let raw = b64.decode(v["data"].as_str().ok_or("the browser gave no picture")?)?;
        let rects: Vec<[f64; 4]> = serde_json::from_value(v["rects"].clone())?;
        let size = |k: &str| v[k].as_f64().ok_or("the browser gave no picture size");
        let painted = paint_masks(&raw, size("width")?, size("height")?, &rects)?;
        let what = if target.is_some() {
            "element (its part on screen)"
        } else if args["fullPage"] == true {
            "screen (not the full page while private details are known: scroll and take \
             another for more)"
        } else {
            "viewport"
        };
        Ok(json!({"content": [
            {"type": "text", "text": format!("### Result\nScreenshot of the {what}. \
                Passwords, card details and codes are blacked out.")},
            {"type": "image", "data": b64.encode(painted), "mimeType": "image/png"},
        ]}))
    }

    /// The focused field (`FIELD_FN`); with `mark`, a private one closes
    /// the page to the agent before anything is typed.
    pub(crate) async fn focused_field(
        &self,
        run: &Running,
        mark: bool,
        force: bool,
    ) -> Result<Field> {
        let edit = {
            let mut p = lock(&self.private);
            p.edit_seq += 1;
            p.edit_seq
        };
        let v = self.run_code(run, &field_code(mark, force, edit)).await?;
        let field = Field {
            url: v["url"]
                .as_str()
                .ok_or("the browser has no open page")?
                .into(),
            private: v["private"] != false,
            frame: v["frame"] != false,
            doc: v["doc"].as_str().unwrap_or("").into(),
            // (A field Iris could not find in another site's frame still
            // needs reading again: never found, it keeps the page closed.)
            edit: (v["edit"] == true || v["edit"] == "unknown").then_some(edit),
            empty: v["empty"] == true,
        };
        if mark && field.private {
            {
                let mut p = lock(&self.private);
                p.ever_marked = true;
                if field.edit.is_some() {
                    let e = (field.doc.clone(), edit);
                    p.edits.push(e);
                }
            }
            self.remember_private_in(&field.url, &field.doc);
            self.note_private_input(field.frame);
        }
        Ok(field)
    }

    /// Keep the agent off this address (see `page_access`).
    pub(crate) fn remember_private(&self, url: &str) {
        self.remember_private_in(url, "");
    }

    /// Keep the agent off this address, typed into in document `doc`.
    fn remember_private_in(&self, url: &str, doc: &str) {
        let mut p = lock(&self.private);
        p.ever_marked = true;
        let entry = (page_key(url).to_string(), doc.to_string());
        if !p.pages.contains(&entry) {
            if p.pages.len() >= PRIVATE_PAGES_MAX {
                p.pages.remove(0);
            }
            p.pages.push(entry);
        }
    }

    /// The server's tools the agent may use. Asked once per install (a
    /// short-lived server answers when the browser is closed; Chromium does
    /// not start for it).
    pub async fn tool_list(&self) -> Result<Vec<Value>> {
        if let Some(t) = lock(&self.tools).clone() {
            return Ok(t);
        }
        let _g = self.list_lock.lock().await;
        if let Some(t) = lock(&self.tools).clone() {
            return Ok(t);
        }
        let all = match self.running() {
            Some(run) => run.up.list_tools(QUICK).await,
            None => {
                let server = self.server().ok_or(NOT_INSTALLED)?;
                let config = self.config_file.with_file_name("playwright-tools.json");
                let up = self.start_server(&server, false, &config).await?;
                let all = up.list_tools(QUICK).await;
                up.close().await;
                all
            }
        }
        .map_err(|e| format!("the browser did not list its tools ({e})"))?;
        let tools = tools::allowed_definitions(all);
        *lock(&self.tools) = Some(tools.clone());
        Ok(tools)
    }

    /// Close the browser (gracefully, so the profile and cookies are saved).
    /// Waits for a start in progress, so nothing launched before the close
    /// survives it.
    pub async fn close(&self) {
        let _g = self.launch_lock.lock().await;
        let run = lock(&self.running).take();
        let Some(run) = run else {
            return;
        };
        crate::info!("browser: closing");
        self.shut(&run).await;
        self.closed();
    }

    /// Close `run` only if it is still the browser and still idle, checked
    /// under the lifecycle lock (activity may have resumed meanwhile).
    async fn close_if_idle(&self, run: &Arc<Running>, idle_ms: i64) -> bool {
        let _g = self.launch_lock.lock().await;
        let current = lock(&self.running)
            .as_ref()
            .is_some_and(|r| Arc::ptr_eq(r, run));
        if !current {
            return true;
        }
        if self.in_use(idle_ms) {
            return false;
        }
        crate::info!("browser: idle; closing to free memory");
        lock(&self.running).take();
        self.shut(run).await;
        self.closed();
        true
    }

    fn in_use(&self, idle_ms: i64) -> bool {
        lock(&self.turns)
            .values()
            .any(|t| t.used.load(Ordering::SeqCst))
            || !lock(&self.watchers).is_empty()
            || self.control() != Control::Agent
            || now_ms()
                - self
                    .last_used
                    .load(Ordering::SeqCst)
                    .max(self.last_input.load(Ordering::SeqCst))
                <= idle_ms
    }

    async fn shut(&self, run: &Running) {
        let _ = run
            .up
            .call_tool("browser_close", &json!({}), Duration::from_secs(5))
            .await;
        run.up.close().await;
    }

    /// After the browser is gone: no session files or output stay, and
    /// nobody is in control of it.
    fn closed(&self) {
        launch::clear_session_files(&self.profile_dir);
        let _ = std::fs::remove_dir_all(&self.out_dir);
        *lock(&self.private) = Private::default();
        lock(&self.recent).clear();
        self.control.send_replace(Control::Agent);
        self.frames.send_replace(None);
        self.broadcast_state();
    }

    async fn idle_loop(&self, run: Arc<Running>) {
        let idle_ms = (self.cfg.idle_mins as i64) * 60_000;
        // Once a minute (a server that died is already skipped by
        // `running()`; this only tidies up after it).
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if !run.alive() {
                break;
            }
            self.clear_output();
            if let Control::User { .. } = self.control()
                && now_ms() - self.last_input.load(Ordering::SeqCst) > TAKEOVER_IDLE_MS
            {
                crate::info!("browser: take-over idle; handing control back");
                self.hand_back();
            }
            if !self.in_use(idle_ms) && self.close_if_idle(&run, idle_ms).await {
                return;
            }
        }
        // Crashed or closed: forget it so the next use starts a fresh one.
        let mine = {
            let mut cur = lock(&self.running);
            let mine = cur.as_ref().is_some_and(|r| Arc::ptr_eq(r, &run));
            if mine {
                *cur = None;
            }
            mine
        };
        if mine {
            crate::warn!("browser: the browser server stopped");
            run.up.close().await;
            self.closed();
        }
    }

    // ----------------------------------------------------------- watching

    /// Start watching (one per app socket), with pictures `width` device
    /// pixels wide (the viewer's size on screen; the mode's default if not
    /// given). Frames arrive through the returned receiver; dropping the
    /// guard stops watching.
    pub fn watch(
        &self,
        mode: Mode,
        width: Option<u32>,
    ) -> (WatchGuard, watch::Receiver<Option<Arc<Frame>>>) {
        let id = self.next_watcher.fetch_add(1, Ordering::SeqCst);
        let width = width
            .unwrap_or(mode.default_width())
            .clamp(FRAME_MIN_W, FRAME_MAX_W);
        lock(&self.watchers).insert(id, (mode, width));
        self.update_screencast();
        (
            WatchGuard {
                browser: self.clone(),
                id,
            },
            self.frames.subscribe(),
        )
    }

    /// Run the live view while anyone watches the running browser.
    fn update_screencast(&self) {
        let Some(run) = self.running() else {
            return;
        };
        if lock(&self.watchers).is_empty() || self.casting.swap(true, Ordering::SeqCst) {
            return;
        }
        let b = self.clone();
        tokio::spawn(async move {
            b.cast(&run).await;
            b.casting.store(false, Ordering::SeqCst);
            // A watcher may have come just as the loop ended.
            if !lock(&b.watchers).is_empty() {
                b.update_screencast();
            }
        });
    }

    /// Pictures of the current tab at the best mode and the largest width
    /// anyone watches, published only when the picture changed. The page
    /// header refreshes every couple of seconds.
    async fn cast(&self, run: &Arc<Running>) {
        use std::hash::{Hash, Hasher};
        let mut last: Option<(Mode, u32, u64)> = None;
        let mut header_at: Option<tokio::time::Instant> = None;
        let mut failed = 0u32;
        loop {
            let (mode, width) = {
                let w = lock(&self.watchers);
                let Some(mode) = w.values().map(|(m, _)| *m).max() else {
                    return;
                };
                (
                    mode,
                    w.values().map(|(_, px)| *px).max().unwrap_or(FRAME_MAX_W),
                )
            };
            if !self.is_current(run) {
                return;
            }
            let started = tokio::time::Instant::now();
            if header_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
                header_at = Some(started);
                let _ = self.current_url(run).await;
                // A sign-in window the page opened while the owner is in
                // control: show it to them (if no input is running).
                if matches!(self.control(), Control::User { .. })
                    && let Ok(_op) = self.op_lock.try_lock()
                {
                    let _ = self.follow_new_tab(run).await;
                }
            }
            let shot = {
                let _one = self.capture_lock.lock().await;
                self.run_code_within(run, &frame_code(width, mode.quality()), FRAME_TIMEOUT)
                    .await
            };
            // Failures are logged (the first, then every 50th; no page
            // content), so a live view that stays blank can be explained.
            match &shot {
                Err(e) if self.is_current(run) => {
                    failed += 1;
                    if failed % 50 == 1 {
                        crate::warn!(
                            "browser: live view picture failed ({failed} in a row): {}",
                            crate::util::truncate_chars(&e.to_string(), 200)
                        );
                    }
                }
                Ok(_) if failed > 0 => {
                    crate::info!("browser: live view pictures back after {failed} failures");
                    failed = 0;
                }
                _ => {}
            }
            if let Ok(Value::String(data)) = shot {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                data.hash(&mut h);
                let key = (mode, width, h.finish());
                if last != Some(key) {
                    last = Some(key);
                    self.publish_frame(mode, &data);
                }
            }
            tokio::time::sleep(mode.min_interval().saturating_sub(started.elapsed())).await;
        }
    }

    fn is_current(&self, run: &Arc<Running>) -> bool {
        run.alive()
            && lock(&self.running)
                .as_ref()
                .is_some_and(|r| Arc::ptr_eq(r, run))
    }

    /// Playwright saves every screenshot to its output folder; the live
    /// view's are deleted right away (`- [Screenshot ...](out/page-....jpeg)`).
    fn remove_output(&self, text: &str) {
        for line in text.lines() {
            let Some(start) = line.find("](") else {
                continue;
            };
            let Some(rel) = line[start + 2..].strip_suffix(')') else {
                continue;
            };
            let rel = Path::new(rel);
            let plain = rel.components().all(|c| {
                matches!(
                    c,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            });
            let path = self.work_dir.join(rel);
            if plain && path.starts_with(&self.out_dir) {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn publish_frame(&self, mode: Mode, data: &str) {
        let seq = self.frame_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let json = json!({
            "type": "browser.frame",
            "seq": seq,
            "mode": if mode == Mode::Full { "full" } else { "peek" },
            "width": VIEW_W,
            "height": VIEW_H,
            "data": data,
        })
        .to_string();
        self.frames.send_replace(Some(Arc::new(Frame { json })));
    }

    // ----------------------------------------------------------- take over

    pub async fn take_over(&self, user_id: &str, device_id: &str) -> Result<()> {
        self.ensure().await?;
        self.last_input.store(now_ms(), Ordering::SeqCst);
        lock(&self.private).field_checked = None;
        // The owner is here now: finished turns' peeks have done their job.
        lock(&self.recent).clear();
        self.control.send_replace(Control::User {
            user_id: user_id.into(),
            device_id: device_id.into(),
        });
        crate::info!("browser: owner took over");
        self.broadcast_state();
        // The owner gets the tab the agent was on (it selects a sign-in
        // window before asking); tabs open now are not new.
        let b = self.clone();
        tokio::spawn(async move {
            let _op = b.op_lock.lock().await;
            if let Some(run) = b.running()
                && let Ok(n) = b.count_tabs(&run).await
            {
                lock(&b.private).tabs = n;
            }
        });
        Ok(())
    }

    pub fn revoke_device(&self, id: &str) {
        if matches!(self.control(), Control::User { device_id, .. } if device_id == id) {
            self.hand_back();
        }
    }

    pub fn hand_back(&self) {
        // Before the agent can act again: what the owner typed is hidden, and
        // private details they may have changed any way (a page's buttons,
        // arrow keys) are read back before its turn.
        if self.control() != Control::Agent {
            {
                let mut p = lock(&self.private);
                flush_typed(&mut p);
                // The agent may change the page meanwhile: the next typing
                // checks the field again.
                p.field_checked = None;
            }
            self.note_private_input(false);
        }
        if self.control.send_replace(Control::Agent) != Control::Agent {
            crate::info!("browser: control handed back");
        }
        self.broadcast_state();
    }

    /// The user in control from `device_id` (an error if that device does
    /// not have control).
    pub(crate) fn controller(&self, device_id: &str) -> Result<String> {
        match self.control() {
            Control::User {
                user_id,
                device_id: d,
            } if d == device_id => Ok(user_id),
            _ => Err("take over first".into()),
        }
    }

    /// Run the owner's input on the page: in control from `device_id`,
    /// one input at a time.
    pub(crate) async fn owner_input<'a>(
        &'a self,
        device_id: &str,
    ) -> Result<(Arc<Running>, tokio::sync::MutexGuard<'a, ()>)> {
        self.controller(device_id)?;
        let run = self.running().ok_or("the browser is not running")?;
        self.last_input.store(now_ms(), Ordering::SeqCst);
        let op = self.op_lock.lock().await;
        // Control may have changed while waiting.
        self.controller(device_id)?;
        Ok((run, op))
    }

    /// One of Iris's own calls for the owner; its result only matters as
    /// success or a short error. A dialog or file picker the page opened
    /// is answered first (the owner cannot see either in the live view),
    /// and the call is tried again if it was refused because of it.
    async fn act(&self, run: &Running, name: &str, args: Value) -> Result<()> {
        let mut r = self.tool(run, name, &args, CALL_TIMEOUT).await?;
        self.remove_output(&text_of(&r));
        if self.answer_modal(run, &text_of(&r), true).await && is_error(&r) {
            r = self.tool(run, name, &args, CALL_TIMEOUT).await?;
            self.remove_output(&text_of(&r));
            self.answer_modal(run, &text_of(&r), true).await;
        }
        ok_text(&r).map(|_| ())
    }

    /// Answer what blocks the page, as Iris always has: alerts and
    /// confirmations are accepted (they follow from what was just tapped),
    /// prompts dismissed (they would need text), and file pickers
    /// cancelled (files on the server are never uploaded). Whether
    /// anything was answered. Dialogs only with `dialogs` (the agent
    /// answers its own).
    pub(crate) async fn answer_modal(&self, run: &Running, text: &str, dialogs: bool) -> bool {
        let Some(modal) = text.find("### Modal state").map(|i| &text[i..]) else {
            return false;
        };
        let mut answered = false;
        if modal.contains("can be handled by browser_file_upload") {
            let r = self
                .tool(run, "browser_file_upload", &json!({}), QUICK)
                .await;
            if let Ok(r) = &r {
                self.remove_output(&text_of(r));
            }
            answered |= r.is_ok_and(|r| !is_error(&r));
        }
        if let Some(line) = modal
            .lines()
            .find(|l| dialogs && l.contains("can be handled by browser_handle_dialog"))
        {
            let accept = !line.trim_start().starts_with("- [\"prompt\"");
            let r = self
                .tool(
                    run,
                    "browser_handle_dialog",
                    &json!({"accept": accept}),
                    QUICK,
                )
                .await;
            if let Ok(r) = &r {
                self.remove_output(&text_of(r));
            }
            answered |= r.is_ok_and(|r| !is_error(&r));
        }
        answered
    }

    async fn count_tabs(&self, run: &Running) -> Result<usize> {
        let r = self
            .tool(run, "browser_tabs", &json!({"action": "list"}), QUICK)
            .await?;
        Ok(tab_count(&ok_text(&r)?))
    }

    /// While the owner is in control, a tab the page opened (a sign-in or
    /// payment window) becomes the one they see and use. Call with the
    /// page held (`op_lock`).
    async fn follow_new_tab(&self, run: &Running) -> Result<()> {
        let n = self.count_tabs(run).await?;
        let seen = std::mem::replace(&mut lock(&self.private).tabs, n);
        if n > seen && n > 1 && matches!(self.control(), Control::User { .. }) {
            self.act(
                run,
                "browser_tabs",
                json!({"action": "select", "index": n - 1}),
            )
            .await?;
        }
        Ok(())
    }

    /// Input from the owner's phone while they have control. Coordinates
    /// are fractions (0..1) of the live view image.
    pub async fn user_input(&self, device_id: &str, ev: &Value) -> Result<()> {
        let (run, _op) = self.owner_input(device_id).await?;
        let frac = |k: &str| {
            ev[k]
                .as_f64()
                .filter(|v| v.is_finite())
                .map(|v| v.clamp(0.0, 1.0))
        };
        let still = || self.controller(device_id).map(|_| ());
        let kind = ev["kind"].as_str().unwrap_or("");
        if kind == "key" && !KEYS.iter().any(|k| ev["key"] == *k) {
            return Err("that key is not supported".into());
        }
        if kind != "text" && kind != "scroll" {
            // Focus may move: check the field again before more typing.
            let mut p = lock(&self.private);
            p.field_checked = None;
            // What was typed so far is complete (Backspace edits it).
            let backspace = kind == "key" && ev["key"] == "Backspace";
            if backspace && p.typing_private {
                p.typed_text.pop();
            } else if !backspace {
                flush_typed(&mut p);
            }
        }
        // What the owner changed is read back before anything that may
        // submit it or take its field away (a tap on Review, Enter): the
        // value is still in its field now.
        let edits = kind == "key" && matches!(ev["key"].as_str(), Some("Backspace" | "Delete"));
        if !matches!(kind, "text" | "scroll") && !edits && lock(&self.private).typed_dirty {
            if let Err(e) = self.capture_typed(&run).await {
                crate::warn!("browser: could not read back what the owner typed: {e}");
            }
            still()?;
        }
        match kind {
            "tap" => {
                let x = frac("x").ok_or("x is required")? * VIEW_W as f64;
                let y = frac("y").ok_or("y is required")? * VIEW_H as f64;
                // One click (not down and up): Playwright stops waiting for
                // it when the page opens a dialog.
                self.act(&run, "browser_mouse_click_xy", json!({"x": x, "y": y}))
                    .await?;
                // The tap may have opened a window (followed later too, by
                // the live view, if it opens slowly).
                let _ = self.follow_new_tab(&run).await;
            }
            "scroll" => {
                let x = frac("x").unwrap_or(0.5) * VIEW_W as f64;
                let y = frac("y").unwrap_or(0.5) * VIEW_H as f64;
                let delta = |k: &str| ev[k].as_f64().unwrap_or(0.0).clamp(-4000.0, 4000.0);
                self.act(&run, "browser_mouse_move_xy", json!({"x": x, "y": y}))
                    .await?;
                still()?;
                self.act(
                    &run,
                    "browser_mouse_wheel",
                    json!({"deltaX": delta("dx"), "deltaY": delta("dy")}),
                )
                .await?;
            }
            "text" => {
                let text = ev["text"].as_str().unwrap_or("");
                if text.chars().count() > 2000 {
                    return Err("text is too long".into());
                }
                self.type_text(&run, device_id, text).await?;
            }
            "key" => {
                let key = ev["key"].as_str().unwrap_or("");
                let key = KEYS
                    .iter()
                    .find(|k| **k == key)
                    .ok_or("that key is not supported")?;
                // Deleting edits the field like typing does: a private one is
                // marked first (and read back again before the agent's turn).
                if matches!(*key, "Backspace" | "Delete") {
                    let field = self.focused_field(&run, true, false).await?;
                    // Taking back what was just typed stays part of that
                    // typing; anything else is a change to read again.
                    {
                        let mut p = lock(&self.private);
                        if let Some(id) = field.edit
                            && *key == "Backspace"
                            && !p.run_edits.is_empty()
                        {
                            p.run_edits.push(id);
                        }
                    }
                    still()?;
                }
                self.act(&run, "browser_press_key", json!({"key": key}))
                    .await?;
            }
            "back" => {
                self.act(&run, "browser_navigate_back", json!({})).await?;
            }
            "navigate" => {
                let url = normalize_url(ev["url"].as_str().unwrap_or(""))?;
                self.act(&run, "browser_navigate", json!({"url": url}))
                    .await?;
            }
            _ => return Err("unknown input kind".into()),
        }
        // A tap or key may change a private value without typing (a page's
        // keypad or stepper, an arrow key): read back again before the next
        // one, which may submit it and take its field away.
        if !matches!(kind, "text" | "scroll") && lock(&self.private).ever_marked {
            self.note_private_input(false);
        }
        Ok(())
    }

    /// Type for the owner into the focused field. When that field is
    /// private, the page is closed to the agent first (checked again after
    /// `FIELD_CHECK_FOR`, or any tap or key). Printable ASCII goes through the keyboard
    /// key by key; anything else is inserted as text, into whichever frame
    /// has focus.
    pub(crate) async fn type_text(&self, run: &Running, device_id: &str, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let fresh = lock(&self.private)
            .field_checked
            .is_some_and(|t| t.elapsed() < FIELD_CHECK_FOR);
        if !fresh {
            let field = self.focused_field(run, true, false).await?;
            let mut p = lock(&self.private);
            p.field_checked = Some(std::time::Instant::now());
            if !field.private || field.doc != p.typing_doc {
                flush_typed(&mut p);
            }
            if let Some(id) = field.edit {
                if p.run_edits.is_empty() && p.typed_text.is_empty() {
                    p.run_from_empty = field.empty;
                }
                p.run_edits.push(id);
            }
            p.typing_private = field.private;
            p.typing_doc = field.doc;
        }
        {
            let mut p = lock(&self.private);
            if p.typing_private {
                p.typed_text.push_str(text);
            }
        }
        let mut other = String::new();
        for c in text.chars().chain(std::iter::once('\0')) {
            let key = (' '..='~').contains(&c);
            if (key || c == '\0') && !other.is_empty() {
                self.controller(device_id)?;
                let code = format!(
                    "async (page) => {{ await page.keyboard.insertText({}); }}",
                    serde_json::to_string(&other)?
                );
                self.act(run, "browser_run_code_unsafe", json!({"code": code}))
                    .await?;
                other.clear();
            }
            if key {
                self.controller(device_id)?;
                self.act(run, "browser_press_key", json!({"key": c.to_string()}))
                    .await?;
            } else if c != '\0' {
                other.push(c);
            }
        }
        Ok(())
    }

    /// The agent holds the page again: what the owner typed is complete
    /// (also when their last key raced handing back).
    pub(crate) fn owner_typing_done(&self) {
        flush_typed(&mut lock(&self.private));
    }

    /// The focused field must be checked again before the owner's typing.
    pub(crate) fn forget_field_check(&self) {
        lock(&self.private).field_checked = None;
    }

    /// Hide a saved secret Iris filled from what the agent gets back.
    pub(crate) fn remember_secret(&self, secret: &str) {
        let mut p = lock(&self.private);
        if !p.secrets.iter().any(|s| s == secret) {
            p.secrets.push(secret.to_string());
        }
    }

    /// The saved secrets Iris filled this session.
    pub(crate) fn filled_secrets(&self) -> Vec<String> {
        lock(&self.private).secrets.clone()
    }

    /// Where the owner's saved secrets come from (set once at startup).
    pub fn set_secret_source(&self, source: SecretSource) {
        *lock(&self.secret_source) = Some(source);
    }

    /// What the agent may do on the current page: the owner typed private
    /// details into this document that Iris could not read back, or a
    /// document at this address that is gone (history may bring it back
    /// with what was typed) (closed); a secret Iris knows is on it (usable,
    /// hidden); or open. What the owner typed since the last look is read
    /// back first (`capture_typed`).
    pub(crate) async fn page_access(&self, run: &Running) -> Result<Access> {
        if lock(&self.private).typed_dirty
            && let Err(e) = self.capture_typed(run).await
        {
            crate::warn!("browser: could not read back what the owner typed: {e}");
        }
        let (typed, filled) = {
            let p = lock(&self.private);
            (p.ever_marked || !p.pages.is_empty(), p.filled)
        };
        if typed {
            let (url, marked, captured, closed) = self.page_state(run).await?;
            // An address typed into stays closed for every document there
            // until the one typed into was read back (another one read back
            // there does not count: history or the site may bring back what
            // was typed).
            if closed
                || (!captured && marked)
                || lock(&self.private)
                    .pages
                    .iter()
                    .any(|(p, _)| p == page_key(&url))
            {
                return Ok(Access::Typed);
            }
        }
        if filled && self.shows_secret(run).await? {
            return Ok(Access::Filled);
        }
        Ok(Access::Open)
    }

    /// Whether a saved secret is still on the current page: in a field or
    /// the text of any frame (also in shadow roots). Unknown counts as yes.
    pub(crate) async fn shows_secret(&self, run: &Running) -> Result<bool> {
        let secrets = self.filled_secrets();
        if secrets.is_empty() {
            return Ok(false);
        }
        Ok(match self.run_code(run, SECRET_CHECK_CODE).await? {
            Value::String(t) => hide_secrets(&t, &secrets) != t,
            _ => true,
        })
    }

    /// Iris filled a saved secret: pages are checked for it from now on.
    pub(crate) fn note_filled(&self) {
        lock(&self.private).filled = true;
    }

    /// Close the browser gracefully (the profile, so sign-ins, is kept) and
    /// open it again: for the agent when the browser is stuck. Every tab
    /// closes; what the owner typed goes with them.
    pub async fn restart(&self) -> Result<Arc<Running>> {
        crate::info!("browser: restarting");
        self.close().await;
        self.ensure().await
    }
}

/// Why a start failed.
enum Start {
    /// Chrome's sandbox cannot start on this system.
    Sandbox,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser() -> (tempfile::TempDir, Browser) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("c.toml"),
            data_dir: tmp.path().join("d"),
        };
        let b = Browser::new(BrowserConfig::default(), &paths, Hub::new());
        (tmp, b)
    }

    fn png_of(w: u32, h: u32, color: png::ColorType, fill: u8) -> Vec<u8> {
        let mut out = Vec::new();
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(color);
        e.set_depth(png::BitDepth::Eight);
        let mut wr = e.write_header().unwrap();
        let n = (w * h) as usize * color.samples();
        wr.write_image_data(&vec![fill; n]).unwrap();
        drop(wr);
        out
    }

    fn pixels_of(png_bytes: &[u8]) -> (u32, u32, usize, Vec<u8>) {
        let mut d = png::Decoder::new(std::io::Cursor::new(png_bytes))
            .read_info()
            .unwrap();
        let mut buf = vec![0; d.output_buffer_size().unwrap()];
        let info = d.next_frame(&mut buf).unwrap();
        (info.width, info.height, info.color_type.samples(), buf)
    }

    #[test]
    fn masks_are_painted_black_where_the_page_shows_private_details() {
        // A white 10x10 picture of a 10x10 area; one private box at (3,3).
        let white = png_of(10, 10, png::ColorType::Rgba, 255);
        let out = paint_masks(&white, 10.0, 10.0, &[[3.0, 3.0, 2.0, 2.0]]).unwrap();
        let (w, h, c, px) = pixels_of(&out);
        assert_eq!((w, h, c), (10, 10, 4));
        let at = |x: usize, y: usize| &px[(y * 10 + x) * 4..(y * 10 + x) * 4 + 4];
        // The box and a pixel around it are black and opaque; the rest is not.
        for (x, y) in [(2, 2), (3, 3), (4, 4), (5, 5)] {
            assert_eq!(at(x, y), [0, 0, 0, 255], "({x},{y})");
        }
        for (x, y) in [(1, 1), (6, 6), (0, 9), (9, 0)] {
            assert_eq!(at(x, y), [255, 255, 255, 255], "({x},{y})");
        }
        // A picture at twice the area's size: the box is scaled with it.
        let big = png_of(20, 20, png::ColorType::Rgb, 255);
        let out = paint_masks(&big, 10.0, 10.0, &[[3.0, 3.0, 2.0, 2.0]]).unwrap();
        let (_, _, c, px) = pixels_of(&out);
        assert_eq!(c, 3);
        let at = |x: usize, y: usize| &px[(y * 20 + x) * 3..(y * 20 + x) * 3 + 3];
        // (6..10 doubled, a pixel wider: 5..11.)
        assert_eq!(at(5, 5), [0, 0, 0]);
        assert_eq!(at(10, 10), [0, 0, 0]);
        assert_eq!(at(4, 4), [255, 255, 255]);
        assert_eq!(at(11, 11), [255, 255, 255]);
        // Off the picture, clamped; a picture not the area's shape, refused.
        assert!(paint_masks(&white, 10.0, 10.0, &[[-5.0, -5.0, 100.0, 1.0]]).is_ok());
        assert!(paint_masks(&white, 10.0, 20.0, &[]).is_err());
        assert!(paint_masks(&white, 10.0, 10.0, &[[f64::NAN, 0.0, 1.0, 1.0]]).is_err());
        assert!(paint_masks(b"not a png", 10.0, 10.0, &[]).is_err());
    }

    #[tokio::test]
    async fn turns_show_and_hide_the_peek() {
        let (_t, b) = browser();
        let mut rx = b.hub.subscribe();
        let g = b.begin_turn("u", "c", "m", &["u".to_string()]);
        let ctx = b.turn(g.token()).unwrap();
        assert!(b.turn("").is_none() && b.turn("nope").is_none());
        assert!(b.state_json()["active"].as_array().unwrap().is_empty());
        b.mark_used(&ctx);
        b.mark_used(&ctx);
        let first: Value = serde_json::from_str(&rx.recv().await.unwrap().json).unwrap();
        assert_eq!(first["type"], "browser.active");
        assert_eq!(first["message_id"], "m");
        assert_eq!(b.state_json()["active"][0]["conversation_id"], "c");
        drop(g);
        let mut kinds = Vec::new();
        while let Ok(e) = rx.try_recv() {
            let v: Value = serde_json::from_str(&e.json).unwrap();
            kinds.push(v["type"].as_str().unwrap().to_string());
        }
        // Exactly one "active" for two calls, then "done" when the turn ends.
        assert_eq!(kinds.iter().filter(|k| *k == "browser.active").count(), 0);
        assert!(kinds.contains(&"browser.done".to_string()));
        assert!(b.state_json()["active"].as_array().unwrap().is_empty());
    }

    #[test]
    fn revoked_controller_hands_control_back() {
        let (_tmp, browser) = browser();
        browser.control.send_replace(Control::User {
            user_id: "owner".into(),
            device_id: "device".into(),
        });
        browser.revoke_device("other-device");
        assert!(matches!(browser.control(), Control::User { .. }));
        assert_eq!(browser.controller("device").unwrap(), "owner");
        assert!(browser.controller("other-device").is_err());
        browser.revoke_device("device");
        assert_eq!(browser.control(), Control::Agent);
    }

    #[tokio::test]
    async fn take_over_pauses_the_agent_until_handed_back() {
        let (_t, b) = browser();
        let g = b.begin_turn("u", "c", "m", &[]);
        let ctx = b.turn(g.token()).unwrap();
        assert!(b.wait_for_agent(&ctx).await);
        assert!(b.check_agent(&ctx).is_ok());
        b.control.send_replace(Control::User {
            user_id: "u".into(),
            device_id: "d".into(),
        });
        assert_eq!(b.state_json()["controller"], "user");
        assert_eq!(b.check_agent(&ctx).unwrap_err().to_string(), TAKEN_OVER);
        let (b2, c2) = (b.clone(), ctx.clone());
        let waiter = tokio::spawn(async move { b2.wait_for_agent(&c2).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        b.hand_back();
        assert!(waiter.await.unwrap());
        // Ending the turn ends a wait at once and blocks further actions.
        b.control.send_replace(Control::User {
            user_id: "u".into(),
            device_id: "d".into(),
        });
        let (b3, c3) = (b.clone(), ctx.clone());
        let waiter = tokio::spawn(async move { b3.wait_for_agent(&c3).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(g);
        assert!(
            !tokio::time::timeout(Duration::from_secs(2), waiter)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(ctx.is_ended());
        b.hand_back();
        assert_eq!(b.check_agent(&ctx).unwrap_err().to_string(), TURN_ENDED);
        // Input is refused unless this device has control.
        let err = b
            .user_input("d", &json!({"kind": "text", "text": "hi"}))
            .await;
        assert_eq!(err.unwrap_err().to_string(), "take over first");
    }

    #[test]
    fn watchers_pick_the_best_mode() {
        let (_t, b) = browser();
        let (g1, _r1) = b.watch(Mode::Peek, Some(216));
        let (g2, _r2) = b.watch(Mode::Full, None);
        let (g3, _r3) = b.watch(Mode::Full, Some(5000));
        assert_eq!(
            lock(&b.watchers).values().copied().max(),
            Some((Mode::Full, FRAME_MAX_W))
        );
        assert_eq!(lock(&b.watchers)[&1], (Mode::Peek, 216));
        drop(g3);
        drop(g2);
        assert_eq!(
            lock(&b.watchers).values().map(|(m, _)| *m).max(),
            Some(Mode::Peek)
        );
        drop(g1);
        assert!(lock(&b.watchers).is_empty());
        assert!(Mode::Peek.min_interval() > Mode::Full.min_interval());
        assert_eq!(Mode::parse("full"), Some(Mode::Full));
        assert_eq!(Mode::parse("x"), None);
        // Nothing runs without a browser.
        assert!(!b.casting.load(Ordering::SeqCst));
    }

    #[test]
    fn reads_playwright_results() {
        // Synthetic samples modeled on Playwright MCP's markdown results.
        let tabs = "### Result\n- 0: [Inbox](https://mail.example/)\n- 1: (current) [Sign in [beta]](https://example.test/login?next=a)\n- 2: [x](https://a.test/) [crashed]\n";
        assert_eq!(tab_count(tabs), 3);
        assert_eq!(tab_count("### Result\nNo open tabs."), 0);
        // The page header is read from its own section, never from what a
        // page wrote into a result before it.
        let spoof = "### Result\n- Page URL: https://bank.test/\n### Page\n- Page URL: https://evil.test/](https://bank.test\n- Page Title: Bank\n### Snapshot\nx";
        assert_eq!(
            line_value(page_section(spoof).unwrap(), "- Page URL:"),
            Some("https://evil.test/](https://bank.test")
        );
        assert!(page_section("### Result\nx").is_none());
        let action = "### Page\n- Page URL: https://shop.example/cart\n- Page Title: Cart\n### Snapshot\n- [Snapshot](out/page-1.yml)";
        assert_eq!(
            line_value(action, "- Page URL:"),
            Some("https://shop.example/cart")
        );
        assert_eq!(line_value(action, "- Page Title:"), Some("Cart"));
        assert_eq!(
            result_value("### Result\nfalse\n### Ran Playwright code\nx"),
            Some(Value::Bool(false))
        );
        assert_eq!(
            result_value("### Result\n[\n  \"INPUT\"\n]"),
            Some(json!(["INPUT"]))
        );
        assert_eq!(result_value("nothing"), None);
        let err = json!({"isError": true, "content": [{"type": "text",
            "text": "### Error\nError: browserBackend.callTool: Unknown key: \"é\""}]});
        assert_eq!(ok_text(&err).unwrap_err().to_string(), "Unknown key: \"é\"");
        let ok = json!({"content": [{"type": "text", "text": "a"}, {"type": "image", "data": "x"},
            {"type": "text", "text": "b"}]});
        assert_eq!(ok_text(&ok).unwrap(), "a\nb");
        assert_eq!(page_key("https://a.test/x#step2"), "https://a.test/x");
    }

    #[test]
    fn only_the_live_views_own_screenshots_are_removed() {
        let (_t, b) = browser();
        std::fs::create_dir_all(&b.out_dir).unwrap();
        let shot = b.out_dir.join("page-1.jpeg");
        std::fs::write(&shot, "x").unwrap();
        let outside = b.work_dir.join("keep.txt");
        std::fs::write(&outside, "x").unwrap();
        b.remove_output("- [Screenshot of viewport](out/../keep.txt)\n- [x](/etc/passwd)");
        assert!(outside.exists());
        b.remove_output("### Result\n- [Screenshot of viewport](out/page-1.jpeg)");
        assert!(!shot.exists());
    }

    #[test]
    fn filled_secrets_are_hidden_in_what_pages_echo() {
        let secrets = [
            "Pw-secret-42".to_string(),
            "4111111111111111".to_string(),
            "737".to_string(),
        ];
        let t = hide_secrets(
            "q=Pw-secret-42 card 4111 1111 1111 1111 or 4111-1111-1111-1111 cvc 737; flight 7370 and Boeing 737",
            &secrets,
        );
        assert!(!t.contains("Pw-secret") && !t.contains("4111"));
        assert!(t.contains("flight 7370"));
        let refs = hide_secrets("- textbox [ref=e737]\n- button [ref=f2e737]: 737", &secrets);
        assert_eq!(
            refs,
            "- textbox [ref=e737]\n- button [ref=f2e737]: [hidden]"
        );
        assert!(t.contains("cvc [hidden]") && t.contains("Boeing [hidden]"));
        // Short passwords as forms send them, and as snapshots quote them.
        let short = [
            "Go*od+42".to_string(),
            "Pw!2345".to_string(),
            "pw+42".to_string(),
            "S3c\"ret{42}".to_string(),
        ];
        let t = hide_secrets(
            "p=Go*od%2B42 x=Pw%212345 y=pw%2b42 heading \"S3c\\\"ret{42}\" z=Go%2Aod%2B42",
            &short,
        );
        assert_eq!(t.matches("[hidden]").count(), 5);
        // Snapshots trim values and collapse whitespace inside them.
        let spaced = [
            "red  fox  ocean".to_string(),
            "  synthetic-passphrase  ".to_string(),
        ];
        let t = hide_secrets(
            "- textbox \"Password\": red fox ocean\n- textbox: synthetic-passphrase",
            &spaced,
        );
        // (Never printed: it would be the secret if the check failed.)
        assert!(!t.contains("fox") && !t.contains("passphrase"));
        let amex = ["378282246310005".to_string()];
        assert_eq!(hide_secrets("3782 822463 10005", &amex), "[hidden]");
        assert_eq!(hide_secrets("nothing here", &secrets), "nothing here");
        // A card number known only in groups (typed or saved that way) is
        // hidden however a page writes it, and so are its picture masks.
        let grouped = ["5555 4444 3333 1111".to_string()];
        for shown in [
            "5555444433331111",
            "5555-4444-3333-1111",
            "5555 4444 3333 1111",
        ] {
            assert_eq!(hide_secrets(shown, &grouped), "[hidden]", "{shown}");
        }
        let forms = mask_form_groups(&grouped);
        assert_eq!(forms.len(), 1);
        for f in ["5555444433331111", "5555-4444-3333-1111"] {
            assert!(forms[0].contains(&f.to_string()), "{f}");
        }
        // Shown group by group in a card preview (a snapshot lists each
        // group apart): every group is hidden, nothing else.
        let preview = "- generic [ref=e5]:\n  - generic [ref=e6]: \"5555\"\n  - generic [ref=e7]: \"4444\"\n  - generic [ref=e8]: \"3333\"\n  - generic [ref=e9]: \"1111\"\n- generic [ref=e10]: Expires 12/26, order 2024";
        let t = hide_secrets(preview, &grouped);
        assert!(
            !t.contains("5555") && !t.contains("4444") && !t.contains("3333"),
            "{t}"
        );
        assert_eq!(t.matches("[hidden]").count(), 4, "{t}");
        assert!(
            t.contains("[ref=e9]") && t.contains("Expires 12/26, order 2024"),
            "{t}"
        );
        // Groups far apart, or digits that are not this card's in order, stay.
        let apart = format!("5555 {} 4444", "x".repeat(100));
        assert_eq!(hide_secrets(&apart, &grouped), apart);
        assert_eq!(hide_secrets("1111 3333 4444", &grouped), "1111 3333 4444");
        // Not a card number: left as it is.
        assert_eq!(card_digits("12 34"), None);
        assert_eq!(card_digits("5555444433331111"), None);
        assert_eq!(card_digits("pass 5555 4444 3333"), None);
    }

    #[test]
    fn owner_addresses_are_web_pages() {
        assert_eq!(normalize_url("example.com").unwrap(), "https://example.com");
        assert_eq!(
            normalize_url("localhost:3000/x").unwrap(),
            "https://localhost:3000/x"
        );
        assert_eq!(
            normalize_url(" http://a.test/x ").unwrap(),
            "http://a.test/x"
        );
        for bad in [
            "",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "a b",
            "chrome://settings",
            "javascript:0;alert(1)",
            "java\nscript:alert(1)",
            "localhost:80x",
        ] {
            assert!(normalize_url(bad).is_err(), "{bad}");
        }
    }
}
