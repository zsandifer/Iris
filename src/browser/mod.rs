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
//! When the owner types into a private field (a password, card details, a
//! one-time code) or fills a saved secret, that page is closed to the
//! agent, so the agent never reads what was typed: the document itself is
//! marked (it stays closed in any tab, and after the address changes
//! within it), and so is its address (a page brought back from history may
//! still hold what was typed), until the browser closes.

pub mod apparmor;
pub mod fill;
pub mod launch;
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
/// disk). The DevTools session is kept on the page for the next frame.
fn frame_code(width: u32, quality: u32) -> String {
    format!(
        "async (page) => {{
  try {{
    const s = page.__irisCast || (page.__irisCast = await page.context().newCDPSession(page));
    const {{ cssVisualViewport: v }} = await s.send('Page.getLayoutMetrics');
    const r = await s.send('Page.captureScreenshot', {{ format: 'jpeg', quality: {quality},
      clip: {{ x: v.pageX, y: v.pageY, width: v.clientWidth, height: v.clientHeight, scale: {width} / v.clientWidth }} }});
    return r.data;
  }} catch (e) {{
    page.__irisCast = undefined;
    throw e;
  }}
}}"
    )
}

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
    card details, or a code), so you cannot read or use it. Wait until they finish and the page \
    moves on (for example after signing in), or open another page.";
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
            self.browser.hub.to_users(
                &ctx.audience,
                json!({"type": "browser.done", "conversation_id": ctx.conversation_id,
                       "message_id": ctx.message_id}),
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
    /// filled private details, kept until the browser closes.
    pages: Vec<String>,
    /// Tabs open when last counted (new ones during take over are followed).
    tabs: usize,
    /// Saved secrets Iris filled this session: hidden from everything the
    /// agent gets back (a page may echo them into its address, title, a
    /// dialog, or its text).
    secrets: Vec<String>,
    /// When the focused field was last checked for typing (the check is a
    /// round trip with Playwright's settle wait, too slow for every key).
    field_checked: Option<std::time::Instant>,
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
    last_used: AtomicI64,
    last_input: AtomicI64,
    /// Serializes actions on the page (tool calls and the owner's input).
    pub(crate) op_lock: tokio::sync::Mutex<()>,
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
})"#;

/// The focused field, followed into shadow roots and same-origin frames:
/// whether it takes private details (passwords, card fields, one-time
/// codes; a field in another site's frame cannot be inspected and counts
/// as private) and whether it is in another site's frame. With `MARK`, a
/// private field marks the document private before anything is typed;
/// with `FORCE` it counts as private whatever it is.
const FIELD_FN: &str = r#"() => {
  const sensitive = (e) => {
    const doc = e.ownerDocument;
    const named = (e.getAttribute('aria-labelledby') || '').split(/\s+/)
      .map((id) => id && doc.getElementById(id)).filter(Boolean).map((l) => l.textContent);
    const a = [e.type, e.autocomplete, e.name, e.id, e.getAttribute('aria-label'), e.placeholder,
      ...Array.from(e.labels || [], (l) => l.textContent), ...named]
      .filter(Boolean).join(' ').toLowerCase();
    if (/password|passcode|cc-|card|cvc|cvv|csc|security code|expir|one-time|otp|\bpin\b|verification|2fa|mfa|totp|authenticat/.test(a)) return true;
    return /\bcode\b/.test(a) && !/postal|zip|promo|coupon|discount|gift|voucher|referral|country|area code|dial/.test(a);
  };
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
  const priv = FORCE || frame || (field && sensitive(e));
  if (MARK && priv) {
    try { Object.defineProperty(window, '__irisPrivate', { value: true }); } catch (_) {}
  }
  return { private: priv, frame };
}"#;

/// Marks the current document private (see `private_page`).
pub(crate) const MARK_JS: &str = "(() => { try { Object.defineProperty(window, '__irisPrivate', { value: true }); } catch (_) {} })()";

/// Run `FIELD_FN` from Playwright's Node side, with the real address.
fn field_code(mark: bool, force: bool) -> String {
    let f = FIELD_FN
        .replace("FORCE", if force { "true" } else { "false" })
        .replace("MARK", if mark { "true" } else { "false" });
    format!(
        "async (page) => ({{ url: page.url(), ...(await page.evaluate({})) }})",
        serde_json::to_string(&format!("({f})()")).unwrap_or_default()
    )
}

/// The focused field, as `FIELD_FN` sees it, and the page's real address.
#[derive(Debug)]
pub(crate) struct Field {
    pub url: String,
    pub private: bool,
    /// In another site's frame.
    #[cfg_attr(not(test), allow(dead_code))]
    pub frame: bool,
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
/// `encodeURIComponent`, or quoted in a snapshot (JSON-style escapes).
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
    let json = serde_json::to_string(secret).unwrap_or_default();
    let json = json[1..json.len().saturating_sub(1).max(1)].to_string();
    let mut forms = vec![form, component, json];
    forms.retain(|f| f != secret && !f.is_empty());
    forms.sort();
    forms.dedup();
    forms
}

/// `text` with saved secrets replaced by `[hidden]`: the gateway's matcher
/// (the value and its common encodings), the forms pages and Playwright
/// write values in (`written_forms`, any length), and what pages do with
/// card details: numbers split into groups by spaces or dashes, and short
/// security codes (only where they stand alone, not inside longer numbers).
pub(crate) fn hide_secrets(text: &str, secrets: &[String]) -> String {
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
            last_used: AtomicI64::new(0),
            last_input: AtomicI64::new(0),
            op_lock: tokio::sync::Mutex::new(()),
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
    pub fn begin_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        message_id: &str,
        audience: &[String],
    ) -> TurnGuard {
        let token = format!("{}{}", new_id(), new_id());
        lock(&self.turns).insert(
            token.clone(),
            Arc::new(TurnCtx {
                user_id: user_id.into(),
                conversation_id: conversation_id.into(),
                message_id: message_id.into(),
                audience: audience.to_vec(),
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
            self.hub.to_users(
                &ctx.audience,
                json!({"type": "browser.active", "conversation_id": ctx.conversation_id,
                       "message_id": ctx.message_id}),
            );
            self.broadcast_state();
        }
    }

    fn active_turns(&self) -> Vec<Value> {
        lock(&self.turns)
            .values()
            .filter(|t| t.used.load(Ordering::SeqCst))
            .map(|t| json!({"conversation_id": t.conversation_id, "message_id": t.message_id}))
            .collect()
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
        let r = self
            .tool(
                run,
                "browser_run_code_unsafe",
                &json!({"code": code}),
                QUICK,
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
        let v = self.run_code(run, PAGE_CODE).await?;
        let url = v["url"].as_str().ok_or("the browser has no open page")?;
        self.set_page_info(Some(url), v["title"].as_str());
        Ok((url.to_string(), v["private"] == true))
    }

    /// The current tab's address.
    pub(crate) async fn current_url(&self, run: &Running) -> Result<String> {
        Ok(self.page_info(run).await?.0)
    }

    /// The focused field (`FIELD_FN`); with `mark`, a private one closes
    /// the page to the agent before anything is typed.
    pub(crate) async fn focused_field(
        &self,
        run: &Running,
        mark: bool,
        force: bool,
    ) -> Result<Field> {
        let v = self.run_code(run, &field_code(mark, force)).await?;
        let field = Field {
            url: v["url"]
                .as_str()
                .ok_or("the browser has no open page")?
                .into(),
            private: v["private"] != false,
            frame: v["frame"] != false,
        };
        if mark && field.private {
            self.remember_private(&field.url);
        }
        Ok(field)
    }

    /// Keep the agent off this address (see `private_page`).
    pub(crate) fn remember_private(&self, url: &str) {
        let mut p = lock(&self.private);
        let key = page_key(url).to_string();
        if !p.pages.contains(&key) {
            if p.pages.len() >= PRIVATE_PAGES_MAX {
                p.pages.remove(0);
            }
            p.pages.push(key);
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
            let shot = self.run_code(run, &frame_code(width, mode.quality())).await;
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
        if kind != "text" && kind != "scroll" {
            // Focus may move: check the field again before more typing.
            lock(&self.private).field_checked = None;
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
            self.focused_field(run, true, false).await?;
            lock(&self.private).field_checked = Some(std::time::Instant::now());
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

    /// Whether the agent must keep off the current page: the owner typed
    /// private details into this document, or on this address.
    pub(crate) async fn private_page(&self, run: &Running) -> Result<bool> {
        if lock(&self.private).pages.is_empty() {
            return Ok(false);
        }
        let (url, marked) = self.page_info(run).await?;
        Ok(marked
            || lock(&self.private)
                .pages
                .iter()
                .any(|p| p == page_key(&url)))
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
        let amex = ["378282246310005".to_string()];
        assert_eq!(hide_secrets("3782 822463 10005", &amex), "[hidden]");
        assert_eq!(hide_secrets("nothing here", &secrets), "nothing here");
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
