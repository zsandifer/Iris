//! Iris's computer: one headless Chromium that Iris launches and owns, with
//! a persistent profile in the data directory (so sign-ins survive
//! restarts), a live view for the app, and take-over from the phone.
//!
//! Iris is not an agent here either: the harness CLI decides what to do and
//! calls the `browser` MCP tools (`iris mcp --browser`, see `tools.rs`);
//! Iris only carries out each call over the DevTools protocol. The tools
//! reach this process through a private Unix socket in the run directory.
//!
//! - Live view: CDP screencast frames go to app sockets that asked to watch
//!   (`browser.watch` over the WebSocket), throttled per mode (a small,
//!   slow thumbnail for the chat peek; sharper and faster full screen), and
//!   the screencast stops when nobody watches.
//! - Take over: the owner's taps, scrolls, and typing are forwarded as real
//!   input events; the agent's browser tools wait until the owner hands
//!   control back.
//! - Saved logins and cards (`fill.rs`): filled from the vault straight
//!   into the page; tool results only say `filled` or `not found`.
//!
//! The browser starts on first use and closes after `browser.idle_mins`.

pub mod cdp;
#[cfg(test)]
mod e2e_tests;
pub mod fill;
pub mod launch;
pub mod socket;
pub mod tools;

use crate::config::{BrowserConfig, Paths};
use crate::events::Hub;
use crate::util::{Result, new_id, now_ms};
use cdp::{Cdp, Event};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

/// Window of Iris's browser: phone-shaped, so the live view fills a phone
/// screen and responsive sites show their mobile layout. (500 is Chrome's
/// narrowest window; a real window size and scale factor, rather than
/// emulation, keep every frame the same size across navigations.)
pub const VIEW_W: u32 = 500;
pub const VIEW_H: u32 = 1000;
/// Device scale factor (sharp text in the live view on phones).
pub const SCALE: f64 = 2.0;
/// How long a tool call waits while the owner has taken over.
const CONTROL_WAIT: Duration = Duration::from_secs(50);
/// Take-over without any input for this long hands control back.
const TAKEOVER_IDLE_MS: i64 = 5 * 60 * 1000;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Who drives the browser right now.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Agent,
    User { user_id: String, device_id: String },
}

/// Live view quality. A socket watching in `Full` gets every frame up to
/// ~6 per second; `Peek` gets a small thumbnail about once a second.
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
            Mode::Full => Duration::from_millis(160),
        }
    }
    fn max_width(self) -> u32 {
        match self {
            Mode::Peek => 360,
            Mode::Full => (VIEW_W as f64 * SCALE) as u32,
        }
    }
    fn quality(self) -> u32 {
        match self {
            Mode::Peek => 45,
            Mode::Full => 60,
        }
    }
}

/// Loads every secret the redactor must know (the owner's saved ones and
/// the encrypted history of filled ones). Blocking; runs off the runtime.
pub type SecretSource = Arc<dyn Fn() -> Result<Vec<String>> + Send + Sync>;

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

/// The running browser.
pub struct Running {
    pub cdp: Arc<Cdp>,
    child: Mutex<Option<tokio::process::Child>>,
    page: Mutex<PageRef>,
    pub no_sandbox: bool,
    pub version: String,
    /// Screencast actually running: (session, mode).
    cast: Mutex<Option<(String, Mode)>>,
    /// Serializes screencast changes (latest wish wins).
    cast_lock: tokio::sync::Mutex<()>,
    /// Serializes switching tabs.
    attach_lock: tokio::sync::Mutex<()>,
    /// Dialogs Iris answered since the last tool result.
    dialogs: Mutex<Vec<String>>,
    /// Viewport size reported with the last frame (CSS pixels).
    last_meta: Mutex<(f64, f64)>,
    /// Secrets Iris typed into pages in this browser session, so any copy a
    /// page reflects (a dialog, another field, the title) is redacted from
    /// what the agent reads. Memory only; gone when the browser closes.
    secrets: Mutex<Vec<String>>,
    originals: Mutex<Vec<String>>,
    /// Documents Iris filled a secret into: never screenshotted (a page can
    /// paint a value it was given in ways no text check sees).
    secret_docs: Mutex<std::collections::HashSet<String>>,
    /// Serializes recovering from a closed tab.
    recover_lock: tokio::sync::Mutex<()>,
    /// The owner's saved secrets were loaded for redaction (a page may show
    /// one it kept from an earlier session).
    pub(crate) seeded: AtomicBool,
}

/// Shortest secret Iris fills: shorter ones could not be told apart from
/// ordinary text when redacting, so Iris will not fill them.
pub const MIN_SECRET: usize = 3;

fn origin_hash(origin: &str) -> String {
    use sha2::Digest;
    crate::util::hex(&sha2::Sha256::digest(origin.as_bytes()))
}

/// The saved list of filled origins: empty if there is none yet, None if
/// it exists but cannot be read.
fn load_tainted(file: &std::path::Path) -> Option<std::collections::HashSet<String>> {
    match std::fs::read(file) {
        Ok(b) => serde_json::from_slice(&b).ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Default::default()),
        Err(_) => None,
    }
}

fn is_card_number(s: &str) -> bool {
    (12..=19).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// Replace `digits` written with any separators between them (spaces,
/// dashes, dots); also says whether anything was replaced.
fn redact_digit_run(text: &str, digits: &str) -> (String, bool) {
    let want: Vec<char> = digits.chars().collect();
    let chars: Vec<char> = text.chars().collect();
    let sep = |c: char| c.is_whitespace() || matches!(c, '-' | '.');
    let mut out = String::with_capacity(text.len());
    let (mut i, mut hit) = (0, false);
    while i < chars.len() {
        if chars[i] == want[0] {
            let (mut j, mut k) = (i + 1, 1);
            while k < want.len() && j < chars.len() {
                if chars[j] == want[k] {
                    k += 1;
                } else if !sep(chars[j]) {
                    break;
                }
                j += 1;
            }
            if k == want.len() {
                out.push_str("••••");
                hit = true;
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    (out, hit)
}

/// Percent-decoding that never fails (bad escapes kept), as UTF-8 or,
/// for pages in legacy encodings, Windows-1252.
fn decode_pct(s: &str, plus_space: bool, legacy: bool) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' if plus_space => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    if !legacy && let Ok(s) = String::from_utf8(out.clone()) {
        return s;
    }
    const CP1252: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    out.iter()
        .map(|&b| {
            if (0x80..0xa0).contains(&b) {
                CP1252[(b - 0x80) as usize]
            } else {
                b as char
            }
        })
        .collect()
}

/// Every reading of a possibly escaped string: `+` kept or as a space,
/// UTF-8 or Windows-1252.
fn decodings(s: &str) -> [String; 4] {
    [
        decode_pct(s, false, false),
        decode_pct(s, true, false),
        decode_pct(s, false, true),
        decode_pct(s, true, true),
    ]
}

#[cfg(test)]
fn decode_loose(s: &str) -> String {
    decode_pct(s, true, false)
}

/// Percent-encoding as JavaScript's `encodeURIComponent`.
fn uri_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct PageRef {
    pub target_id: String,
    pub session_id: String,
}

impl Running {
    pub fn page(&self) -> PageRef {
        lock(&self.page).clone()
    }

    pub fn take_dialogs(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.dialogs))
    }

    /// Remember a secret Iris typed (and common spaced forms of card
    /// numbers) for redaction.
    pub fn remember_secret(&self, secret: &str) {
        if secret.chars().count() < MIN_SECRET {
            return;
        }
        {
            let mut o = lock(&self.originals);
            if !o.iter().any(|x| x == secret) {
                o.push(secret.to_string());
            }
        }
        let mut forms = vec![secret.to_string()];
        // As it appears in addresses and submitted forms.
        let enc = uri_encode(secret);
        forms.push(enc.replace("%20", "+"));
        forms.push(enc);
        let folded = secret.split_whitespace().collect::<Vec<_>>().join(" ");
        if folded.chars().count() >= 3 && folded != secret {
            forms.push(folded);
        }
        let mut s = lock(&self.secrets);
        for f in forms {
            if !s.contains(&f) {
                s.push(f);
            }
        }
        // Longest first, so a secret containing another is replaced whole.
        s.sort_by_key(|x| std::cmp::Reverse(x.len()));
    }

    /// Replace every remembered secret in text the agent will read. A line
    /// that still holds one once percent-decoded (an address shown in some
    /// other escaping) loses its query, or the whole line.
    pub fn redact(&self, text: &str) -> String {
        let secrets = lock(&self.secrets);
        if secrets.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for s in secrets.iter() {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), "••••");
            }
        }
        // Card numbers in any grouping ("3782 822463 10005").
        for s in secrets.iter().filter(|s| is_card_number(s)) {
            out = redact_digit_run(&out, s).0;
        }
        let leaks = |t: &str| {
            decodings(t).iter().any(|d| {
                secrets.iter().any(|s| {
                    d.contains(s.as_str()) || (is_card_number(s) && redact_digit_run(d, s).1)
                })
            })
        };
        if !out.lines().any(leaks) {
            return out;
        }
        out.split('\n')
            .map(|line| match line.find('?') {
                _ if !leaks(line) => line.to_string(),
                // Drop the query, unless what is left still shows it.
                Some(q) if !leaks(&line[..q]) => format!("{}?••••", &line[..q]),
                _ => "••••".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The screencast running now, if any.
    pub fn casting(&self) -> Option<Mode> {
        lock(&self.cast).as_ref().map(|(_, m)| *m)
    }

    pub fn remember_secret_doc(&self, doc: &str) {
        lock(&self.secret_docs).insert(doc.to_string());
    }

    pub fn has_secret_doc(&self, doc: &str) -> bool {
        lock(&self.secret_docs).contains(doc)
    }

    /// The secrets themselves (not their encoded forms), for the encrypted
    /// redaction history.
    pub fn originals(&self) -> Vec<String> {
        lock(&self.originals).clone()
    }

    /// The remembered secrets (to hand to the page helpers' isolated world).
    pub fn secrets(&self) -> Vec<String> {
        lock(&self.secrets).clone()
    }

    /// Whether text contains a remembered secret (also percent-encoded).
    pub fn shows_secret(&self, text: &str) -> bool {
        let secrets = lock(&self.secrets);
        let all = decodings(text);
        secrets.iter().any(|s| {
            std::iter::once(text)
                .chain(all.iter().map(String::as_str))
                .any(|t| t.contains(s.as_str()) || (is_card_number(s) && redact_digit_run(t, s).1))
        })
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

/// Shared browser state (cheap to clone through `Arc`).
pub struct Shared {
    cfg: BrowserConfig,
    profile_dir: PathBuf,
    tmp_dir: PathBuf,
    hub: Hub,
    launch_lock: tokio::sync::Mutex<()>,
    running: Mutex<Option<Arc<Running>>>,
    view: Mutex<View>,
    control: watch::Sender<Control>,
    frames: watch::Sender<Option<Arc<Frame>>>,
    frame_seq: AtomicU64,
    watchers: Mutex<HashMap<u64, Mode>>,
    next_watcher: AtomicU64,
    turns: Mutex<HashMap<String, Arc<TurnCtx>>>,
    last_used: AtomicI64,
    last_input: AtomicI64,
    /// Serializes actions on the page (tool calls and fills).
    pub(crate) op_lock: tokio::sync::Mutex<()>,
    /// "Pause Iris" in the app: no turns, jobs, or browsing until resumed.
    paused: AtomicBool,
    /// The owner agreed (in the app) to run without Chrome's sandbox on a
    /// system where it cannot start.
    allow_no_sandbox: AtomicBool,
    /// Chromium is being installed, updated, or removed.
    maintenance: AtomicBool,
    /// Origins Iris ever filled a secret into (kept on disk; not secret).
    /// The agent never gets screenshots of them: a page can keep a value
    /// and paint it later, even after a restart.
    /// None: the list on disk exists but could not be read.
    tainted: Mutex<Option<std::collections::HashSet<String>>>,
    tainted_file: PathBuf,
    /// Orders changes of the saved safety switches (pause, sandbox).
    pub switch_lock: tokio::sync::Mutex<()>,
    secret_source: Mutex<Option<SecretSource>>,
    /// Bumped by Stop per conversation: work queued before it (a card
    /// approval's continuation) is dropped.
    stops: Mutex<HashMap<String, u64>>,
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
        self.browser.update_screencast();
    }
}

impl Browser {
    pub fn new(cfg: BrowserConfig, paths: &Paths, hub: Hub) -> Self {
        let dir = paths.browser_dir();
        Self(Arc::new(Shared {
            cfg,
            profile_dir: dir.join("profile"),
            tmp_dir: dir.join("tmp"),
            hub,
            launch_lock: tokio::sync::Mutex::new(()),
            running: Mutex::new(None),
            view: Mutex::default(),
            control: watch::Sender::new(Control::Agent),
            frames: watch::Sender::new(None),
            frame_seq: AtomicU64::new(0),
            watchers: Mutex::default(),
            next_watcher: AtomicU64::new(1),
            turns: Mutex::default(),
            last_used: AtomicI64::new(0),
            last_input: AtomicI64::new(0),
            op_lock: tokio::sync::Mutex::new(()),
            paused: AtomicBool::new(false),
            allow_no_sandbox: AtomicBool::new(false),
            maintenance: AtomicBool::new(false),
            tainted: Mutex::new(load_tainted(&dir.join("tainted-origins.json"))),
            tainted_file: dir.join("tainted-origins.json"),
            switch_lock: tokio::sync::Mutex::new(()),
            secret_source: Mutex::new(None),
            stops: Mutex::default(),
        }))
    }

    pub fn set_secret_source(&self, source: SecretSource) {
        *lock(&self.secret_source) = Some(source);
    }

    /// Know every saved and earlier filled secret before a session shows
    /// anything (fails closed if they cannot be read).
    async fn seed(&self, run: &Running) -> Result<()> {
        let source = lock(&self.secret_source).clone();
        if let Some(source) = source {
            let list = tokio::task::spawn_blocking(move || source())
                .await
                .map_err(|e| e.to_string())??;
            for s in list {
                run.remember_secret(&s);
            }
        }
        run.seeded.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// A credential was saved or edited: a running session learns it now.
    pub fn learn_secrets(&self, secrets: &[String]) {
        if let Some(run) = self.running() {
            for s in secrets {
                run.remember_secret(s);
            }
        }
    }

    /// Stop was pressed in a conversation.
    pub fn note_stop(&self, conversation_id: &str) {
        *lock(&self.stops)
            .entry(conversation_id.to_string())
            .or_default() += 1;
    }

    pub fn stop_count(&self, conversation_id: &str) -> u64 {
        lock(&self.stops).get(conversation_id).copied().unwrap_or(0)
    }

    /// Remember (on disk, before anything is filled) that Iris fills a
    /// secret into `origin`. Origins are stored hashed. An error means the
    /// fill must not happen.
    pub fn taint_origin(&self, origin: &str) -> Result<()> {
        let h = origin_hash(origin);
        let mut t = lock(&self.tainted);
        let Some(set) = t.as_mut() else {
            return Err("the browser's safety list could not be read".into());
        };
        if set.contains(&h) {
            return Ok(());
        }
        let mut next = set.clone();
        next.insert(h);
        if let Some(dir) = self.tainted_file.parent() {
            crate::config::ensure_private_dir(dir)?;
        }
        crate::config::write_private_file(&self.tainted_file, &serde_json::to_vec(&next)?)?;
        *set = next;
        Ok(())
    }

    /// True for origins Iris filled a secret into, and for everything when
    /// the saved list could not be read (fail closed).
    pub fn is_tainted(&self, origin: &str) -> bool {
        lock(&self.tainted)
            .as_ref()
            .is_none_or(|set| set.contains(&origin_hash(origin)))
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

    #[cfg(test)]
    pub async fn launch_lock_for_test(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.launch_lock.lock().await
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn executable(&self) -> Option<PathBuf> {
        launch::find(&self.cfg.command)
    }

    pub fn running(&self) -> Option<Arc<Running>> {
        lock(&self.running).clone().filter(|r| !r.cdp.is_closed())
    }

    pub fn control(&self) -> Control {
        self.control.borrow().clone()
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

    /// While Chromium's files are being installed, updated, or removed, the
    /// browser is closed and cannot start.
    pub async fn begin_maintenance(&self) {
        self.maintenance.store(true, Ordering::SeqCst);
        self.close().await;
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
            "available": self.executable().is_some(),
            "running": run.is_some(),
            "paused": self.is_paused(),
            "url": if run.is_some() { v.url } else { String::new() },
            "title": if run.is_some() { v.title } else { String::new() },
            "controller": match &control { Control::Agent => "agent", Control::User { .. } => "user" },
            "controller_device_id": match &control { Control::User { device_id, .. } => Some(device_id.clone()), _ => None },
            "busy": v.busy && run.is_some(),
            "active": self.active_turns(),
            "watchers": lock(&self.watchers).len(),
            "streaming": run.as_ref().and_then(|r| r.casting()).is_some(),
            "sandbox": run.as_ref().map(|r| !r.no_sandbox),
            "sandbox_unavailable": v.sandbox_unavailable,
            "allow_no_sandbox": self.allows_no_sandbox(),
            "version": run.as_ref().map(|r| r.version.clone()),
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

    /// Every page address and title shown in the app goes through here,
    /// redacted (a page may echo a filled secret into either).
    fn set_page_info(&self, run: &Running, url: Option<&str>, title: Option<&str>) {
        let url = url.map(|u| run.redact(u));
        let title = title.map(|t| run.redact(t));
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

    // ------------------------------------------------------------ launch

    /// The running browser, starting it if needed.
    pub async fn ensure(&self) -> Result<Arc<Running>> {
        if !self.cfg.enabled {
            return Err(
                "the browser is turned off in the server's config ([browser] enabled)".into(),
            );
        }
        if self.is_paused() {
            return Err("Iris is paused".into());
        }
        if let Some(r) = self.running() {
            return Ok(r);
        }
        let _g = self.launch_lock.lock().await;
        if self.maintenance.load(Ordering::SeqCst) {
            return Err("the browser is being updated; try again in a minute".into());
        }
        // Pause may have come while waiting for the lock.
        if self.is_paused() {
            return Err("Iris is paused".into());
        }
        if let Some(r) = self.running() {
            return Ok(r);
        }
        let program = self.executable().ok_or(
            "no browser is installed; set it up from Iris's computer in the app \
             (or install Chromium on the server)",
        )?;
        crate::config::ensure_private_dir(&self.profile_dir)?;
        crate::config::ensure_private_dir(&self.tmp_dir)?;
        crate::info!("browser: starting {}", program.display());
        let launched = launch::launch(&launch::LaunchOptions {
            program: &program,
            profile_dir: &self.profile_dir,
            tmp_dir: &self.tmp_dir,
            width: VIEW_W,
            height: VIEW_H,
            scale: SCALE,
            allow_no_sandbox: self.allows_no_sandbox(),
        })
        .await;
        let launched = match launched {
            Ok(l) => l,
            Err(e) => {
                crate::warn!("browser: could not start: {e}");
                {
                    let mut v = lock(&self.view);
                    v.sandbox_unavailable = e.to_string() == launch::SANDBOX_UNAVAILABLE;
                    v.error = Some(e.to_string());
                }
                self.broadcast_state();
                return Err(e);
            }
        };
        let cdp = launched.cdp.clone();
        let version = cdp
            .call("Browser.getVersion", json!({}), None)
            .await
            .ok()
            .and_then(|v| v["product"].as_str().map(String::from))
            .unwrap_or_default();
        let run = Arc::new(Running {
            cdp: cdp.clone(),
            child: Mutex::new(Some(launched.child)),
            page: Mutex::default(),
            no_sandbox: launched.no_sandbox,
            version,
            cast: Mutex::new(None),
            cast_lock: tokio::sync::Mutex::new(()),
            attach_lock: tokio::sync::Mutex::new(()),
            dialogs: Mutex::default(),
            last_meta: Mutex::new((VIEW_W as f64, VIEW_H as f64)),
            secrets: Mutex::default(),
            originals: Mutex::default(),
            secret_docs: Mutex::default(),
            recover_lock: tokio::sync::Mutex::new(()),
            seeded: AtomicBool::new(false),
        });
        let events = cdp.subscribe();
        cdp.call("Target.setDiscoverTargets", json!({"discover": true}), None)
            .await?;
        let targets = cdp.call("Target.getTargets", json!({}), None).await?;
        let first = targets["targetInfos"]
            .as_array()
            .and_then(|t| t.iter().find(|t| t["type"] == "page"))
            .and_then(|t| t["targetId"].as_str())
            .map(String::from);
        let target = match first {
            Some(t) => t,
            None => cdp
                .call("Target.createTarget", json!({"url": "about:blank"}), None)
                .await?["targetId"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        };
        // Secrets first: nothing from a page is read or shown before the
        // redactor knows them all.
        if let Err(e) = self.seed(&run).await {
            crate::warn!("browser: could not load its safety data: {e}");
            Self::shut(&run).await;
            return Err("the browser's safety data could not be read".into());
        }
        if let Err(e) = self.attach(&run, &target).await {
            Self::shut(&run).await;
            return Err(e);
        }
        // Paused, or the owner took back running without the sandbox,
        // while it was starting: do not publish it.
        if self.is_paused() {
            Self::shut(&run).await;
            return Err("Iris is paused".into());
        }
        if run.no_sandbox && !self.allows_no_sandbox() {
            Self::shut(&run).await;
            return Err(launch::SANDBOX_UNAVAILABLE.into());
        }
        {
            let mut v = lock(&self.view);
            v.error = None;
            v.sandbox_unavailable = false;
            v.url = "about:blank".into();
            v.title = String::new();
        }
        *lock(&self.running) = Some(run.clone());
        self.last_used.store(now_ms(), Ordering::SeqCst);
        let b = self.clone();
        let r = run.clone();
        tokio::spawn(async move { b.event_loop(r, events).await });
        let b = self.clone();
        let r = run.clone();
        tokio::spawn(async move { b.idle_loop(r).await });
        self.update_screencast();
        self.broadcast_state();
        crate::info!("browser: ready");
        Ok(run)
    }

    /// Attach to a page target and make it the current tab.
    pub async fn attach(&self, run: &Running, target_id: &str) -> Result<()> {
        self.attach_if(run, target_id, None, &|| Ok(())).await
    }

    /// Tab switch by the agent: abandoned if it lost control meanwhile.
    pub async fn attach_guarded(
        &self,
        run: &Running,
        target_id: &str,
        guard: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        self.attach_if(run, target_id, None, guard).await
    }

    /// Attach, but only if the current tab is still `expect` (when given):
    /// another path may already have switched tabs.
    async fn attach_if(
        &self,
        run: &Running,
        target_id: &str,
        expect: Option<&str>,
        guard: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        let _g = run.attach_lock.lock().await;
        let current = run.page().target_id;
        if current == target_id || expect.is_some_and(|e| e != current) {
            return Ok(());
        }
        let cdp = &run.cdp;
        let a = cdp
            .call(
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
                None,
            )
            .await?;
        let session = a["sessionId"]
            .as_str()
            .ok_or("could not attach to the tab")?
            .to_string();
        let s = Some(session.as_str());
        cdp.call("Page.enable", json!({}), s).await?;
        // Behave like the focused window it is to the user: pages get focus
        // events (headless pages otherwise never do).
        let _ = cdp
            .call(
                "Emulation.setFocusEmulationEnabled",
                json!({"enabled": true}),
                s,
            )
            .await;
        let ua = cdp
            .call("Browser.getVersion", json!({}), None)
            .await
            .ok()
            .and_then(|v| v["userAgent"].as_str().map(String::from))
            .unwrap_or_default();
        if ua.contains("HeadlessChrome") {
            // Some sites refuse "headless" browsers outright.
            let _ = cdp
                .call(
                    "Emulation.setUserAgentOverride",
                    json!({"userAgent": ua.replace("HeadlessChrome", "Chrome")}),
                    s,
                )
                .await;
        }
        if let Err(e) = guard() {
            let _ = cdp
                .call(
                    "Target.detachFromTarget",
                    json!({"sessionId": session}),
                    None,
                )
                .await;
            return Err(e);
        }
        let old = std::mem::replace(
            &mut *lock(&run.page),
            PageRef {
                target_id: target_id.to_string(),
                session_id: session.clone(),
            },
        );
        if !old.session_id.is_empty() {
            let _ = cdp
                .call(
                    "Target.detachFromTarget",
                    json!({"sessionId": old.session_id}),
                    None,
                )
                .await;
        }
        // Move the screencast to the new tab if someone is watching.
        self.update_screencast();
        if let Ok(info) = cdp
            .call("Target.getTargetInfo", json!({"targetId": target_id}), None)
            .await
        {
            self.set_page_info(
                run,
                info["targetInfo"]["url"].as_str(),
                info["targetInfo"]["title"].as_str(),
            );
        }
        Ok(())
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
        Self::shut(&run).await;
        launch::clear_session_files(&self.profile_dir);
        self.control.send_replace(Control::Agent);
        self.frames.send_replace(None);
        self.broadcast_state();
    }

    /// Close `run` only if it is still the browser and still idle, checked
    /// under the lifecycle lock (activity may have resumed meanwhile).
    async fn close_if_idle(&self, run: &Arc<Running>, idle_ms: i64) -> bool {
        let _g = self.launch_lock.lock().await;
        let current = lock(&self.running)
            .as_ref()
            .is_some_and(|r| Arc::ptr_eq(r, run));
        let busy = lock(&self.turns)
            .values()
            .any(|t| t.used.load(Ordering::SeqCst))
            || !lock(&self.watchers).is_empty()
            || self.control() != Control::Agent
            || now_ms()
                - self
                    .last_used
                    .load(Ordering::SeqCst)
                    .max(self.last_input.load(Ordering::SeqCst))
                <= idle_ms;
        if !current || busy {
            return !current;
        }
        crate::info!("browser: idle; closing to free memory");
        lock(&self.running).take();
        Self::shut(run).await;
        launch::clear_session_files(&self.profile_dir);
        self.frames.send_replace(None);
        self.broadcast_state();
        true
    }

    async fn shut(run: &Running) {
        let _ = run
            .cdp
            .call_timeout("Browser.close", json!({}), None, Duration::from_secs(2))
            .await;
        let child = lock(&run.child).take();
        if let Some(mut child) = child
            && tokio::time::timeout(Duration::from_secs(3), child.wait())
                .await
                .is_err()
        {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    async fn idle_loop(&self, run: Arc<Running>) {
        let idle_ms = (self.cfg.idle_mins as i64) * 60_000;
        loop {
            tokio::select! {
                _ = run.cdp.wait_closed() => break,
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
            let now = now_ms();
            if let Control::User { .. } = self.control()
                && now - self.last_input.load(Ordering::SeqCst) > TAKEOVER_IDLE_MS
            {
                crate::info!("browser: take-over idle; handing control back");
                self.hand_back();
            }
            let in_turn = lock(&self.turns)
                .values()
                .any(|t| t.used.load(Ordering::SeqCst));
            let watched = !lock(&self.watchers).is_empty();
            let user = self.control() != Control::Agent;
            let last = self
                .last_used
                .load(Ordering::SeqCst)
                .max(self.last_input.load(Ordering::SeqCst));
            if !in_turn
                && !watched
                && !user
                && now - last > idle_ms
                && self.close_if_idle(&run, idle_ms).await
            {
                break;
            }
        }
        // Crashed or closed: forget it so the next use starts a fresh one.
        let mut cur = lock(&self.running);
        if cur.as_ref().is_some_and(|r| Arc::ptr_eq(r, &run)) {
            *cur = None;
            drop(cur);
            self.frames.send_replace(None);
            self.control.send_replace(Control::Agent);
            self.broadcast_state();
        }
    }

    // ------------------------------------------------------------ events

    async fn event_loop(&self, run: Arc<Running>, mut rx: broadcast::Receiver<Arc<Event>>) {
        loop {
            let ev = tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = run.cdp.wait_closed() => break,
            };
            let page = run.page();
            let current = ev.session_id.as_deref() == Some(page.session_id.as_str());
            match ev.method.as_str() {
                "Page.screencastFrame" if current => self.on_frame(&run, &ev.params, &page),
                "Page.screencastFrame" => {
                    // A frame from a tab we left: acknowledge so it stops.
                    run.cdp.send(
                        "Page.screencastFrameAck",
                        json!({"sessionId": ev.params["sessionId"]}),
                        ev.session_id.as_deref(),
                    );
                }
                "Page.javascriptDialogOpening" if current => {
                    let kind = ev.params["type"].as_str().unwrap_or("alert");
                    // Redact before cutting, so no prefix of a secret survives.
                    let msg = crate::util::truncate_chars(
                        &run.redact(ev.params["message"].as_str().unwrap_or("")),
                        200,
                    );
                    // Alerts and confirms follow from what was just clicked;
                    // prompts would need text, so they are dismissed.
                    let accept = kind != "prompt";
                    {
                        // A page can open dialogs endlessly: keep a few.
                        let mut d = lock(&run.dialogs);
                        if d.len() >= 5 {
                            d.remove(0);
                        }
                        d.push(format!(
                            "{kind} \"{msg}\" was {}",
                            if accept { "accepted" } else { "dismissed" }
                        ));
                    }
                    let cdp = run.cdp.clone();
                    let s = page.session_id.clone();
                    tokio::spawn(async move {
                        let _ = cdp
                            .call(
                                "Page.handleJavaScriptDialog",
                                json!({"accept": accept}),
                                Some(&s),
                            )
                            .await;
                    });
                }
                "Target.targetInfoChanged" => {
                    let info = &ev.params["targetInfo"];
                    if info["targetId"].as_str() == Some(page.target_id.as_str()) {
                        self.set_page_info(&run, info["url"].as_str(), info["title"].as_str());
                    }
                }
                "Target.targetCreated" => {
                    let info = &ev.params["targetInfo"];
                    // A link or script opened a new tab from the current
                    // one: follow it, like a person would.
                    if info["type"] == "page"
                        && info["openerId"].as_str() == Some(page.target_id.as_str())
                        && let Some(t) = info["targetId"].as_str()
                    {
                        let (b, r, t) = (self.clone(), run.clone(), t.to_string());
                        tokio::spawn(async move {
                            let _ = b.attach(&r, &t).await;
                        });
                    }
                }
                "Target.targetDestroyed" | "Target.detachedFromTarget" => {
                    let gone = ev.params["targetId"].as_str() == Some(page.target_id.as_str())
                        || ev.params["sessionId"].as_str() == Some(page.session_id.as_str());
                    if gone {
                        let (b, r, t) = (self.clone(), run.clone(), page.target_id.clone());
                        tokio::spawn(async move {
                            let _ = b.switch_to_any(&r, &t).await;
                        });
                    }
                }
                _ => {}
            }
        }
    }

    /// The tab `gone` closed: use another one (or open a blank tab). Runs
    /// once however many paths notice; a no-op if it was already replaced.
    pub async fn switch_to_any(&self, run: &Running, gone: &str) -> Result<()> {
        let _g = run.recover_lock.lock().await;
        if run.page().target_id != gone {
            return Ok(());
        }
        let current = gone.to_string();
        let targets = run.cdp.call("Target.getTargets", json!({}), None).await?;
        let other = targets["targetInfos"]
            .as_array()
            .and_then(|t| {
                t.iter()
                    .find(|t| t["type"] == "page" && t["targetId"].as_str() != Some(&current))
            })
            .and_then(|t| t["targetId"].as_str())
            .map(String::from);
        let target = match other {
            Some(t) => t,
            None => run
                .cdp
                .call("Target.createTarget", json!({"url": "about:blank"}), None)
                .await?["targetId"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        };
        self.attach_if(run, &target, Some(gone), &|| Ok(())).await
    }

    fn on_frame(&self, run: &Arc<Running>, p: &Value, page: &PageRef) {
        let ack_id = p["sessionId"].clone();
        let meta = &p["metadata"];
        let (w, h) = (
            meta["deviceWidth"].as_f64().unwrap_or(VIEW_W as f64),
            meta["deviceHeight"].as_f64().unwrap_or(VIEW_H as f64),
        );
        *lock(&run.last_meta) = (w, h);
        let mode = lock(&run.cast)
            .as_ref()
            .filter(|(s, _)| *s == page.session_id)
            .map(|(_, m)| *m);
        if let (Some(mode), Some(data)) = (mode, p["data"].as_str()) {
            self.publish_frame(mode, w, h, data);
        }
        // Acknowledge at once: the browser drops frames while acks are
        // outstanding, which could leave a stale picture once the page goes
        // still. Each app socket is throttled instead (ws.rs), always
        // ending on the newest frame.
        run.cdp.send(
            "Page.screencastFrameAck",
            json!({"sessionId": ack_id}),
            Some(&page.session_id),
        );
    }

    fn publish_frame(&self, mode: Mode, w: f64, h: f64, data: &str) {
        let seq = self.frame_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let json = json!({
            "type": "browser.frame",
            "seq": seq,
            "mode": if mode == Mode::Full { "full" } else { "peek" },
            "width": w,
            "height": h,
            "data": data,
        })
        .to_string();
        self.frames.send_replace(Some(Arc::new(Frame { json })));
    }

    /// The screencast only sends frames when the page changes, so a new
    /// watcher of a still page gets one picture taken right away.
    async fn publish_still(&self, run: &Running, mode: Mode) -> Result<()> {
        let page = run.page();
        let s = Some(page.session_id.as_str());
        let m = run.cdp.call("Page.getLayoutMetrics", json!({}), s).await?;
        let vv = &m["cssVisualViewport"];
        let (w, h) = (
            vv["clientWidth"].as_f64().unwrap_or(VIEW_W as f64),
            vv["clientHeight"].as_f64().unwrap_or(VIEW_H as f64),
        );
        let scale = (mode.max_width() as f64 / (w * SCALE)).min(1.0);
        // Bounded: a page that never paints must not hold up the live view.
        let shot = run
            .cdp
            .call_timeout(
                "Page.captureScreenshot",
                json!({"format": "jpeg", "quality": mode.quality(), "clip": {
                    "x": vv["pageX"].as_f64().unwrap_or(0.0),
                    "y": vv["pageY"].as_f64().unwrap_or(0.0),
                    "width": w, "height": h, "scale": scale}}),
                s,
                Duration::from_secs(3),
            )
            .await?;
        if let Some(data) = shot["data"].as_str() {
            self.publish_frame(mode, w, h, data);
        }
        Ok(())
    }

    // ----------------------------------------------------------- watching

    /// Start watching (one per app socket). Frames arrive through the
    /// returned receiver; dropping the guard stops watching.
    pub fn watch(&self, mode: Mode) -> (WatchGuard, watch::Receiver<Option<Arc<Frame>>>) {
        let id = self.next_watcher.fetch_add(1, Ordering::SeqCst);
        lock(&self.watchers).insert(id, mode);
        self.update_screencast();
        (
            WatchGuard {
                browser: self.clone(),
                id,
            },
            self.frames.subscribe(),
        )
    }

    /// Start, restart, or stop the screencast to match the watchers.
    fn update_screencast(&self) {
        let Some(run) = self.running() else {
            return;
        };
        let b = self.clone();
        tokio::spawn(async move { b.reconcile_screencast(&run).await });
    }

    /// One change at a time, always toward the latest wish: the best mode
    /// any watcher wants, on the current tab.
    async fn reconcile_screencast(&self, run: &Running) {
        let _g = run.cast_lock.lock().await;
        let want = lock(&self.watchers).values().copied().max();
        let page = run.page();
        if page.session_id.is_empty() {
            return;
        }
        let have = lock(&run.cast).clone();
        let target = want.map(|m| (page.session_id.clone(), m));
        if have == target {
            return;
        }
        let cdp = &run.cdp;
        // A screencast on a tab we left ended when we detached from it.
        if let Some((sess, _)) = &have
            && *sess == page.session_id
        {
            let _ = cdp.call("Page.stopScreencast", json!({}), Some(sess)).await;
        }
        *lock(&run.cast) = None;
        if let Some(m) = want {
            let w = m.max_width();
            let started = cdp
                .call(
                    "Page.startScreencast",
                    json!({"format": "jpeg", "quality": m.quality(), "maxWidth": w,
                           "maxHeight": w * 2, "everyNthFrame": 1}),
                    Some(&page.session_id),
                )
                .await;
            if started.is_ok() {
                *lock(&run.cast) = target;
                let _ = self.publish_still(run, m).await;
            }
        }
    }

    // ----------------------------------------------------------- take over

    pub async fn take_over(&self, user_id: &str, device_id: &str) -> Result<()> {
        self.ensure().await?;
        self.last_input.store(now_ms(), Ordering::SeqCst);
        self.control.send_replace(Control::User {
            user_id: user_id.into(),
            device_id: device_id.into(),
        });
        crate::info!("browser: owner took over");
        self.broadcast_state();
        Ok(())
    }

    pub fn hand_back(&self) {
        if self.control.send_replace(Control::Agent) != Control::Agent {
            crate::info!("browser: control handed back");
        }
        self.broadcast_state();
    }

    /// Input from the owner's phone while they have control. Coordinates
    /// are fractions (0..1) of the live view image.
    pub async fn user_input(&self, device_id: &str, ev: &Value) -> Result<()> {
        match self.control() {
            Control::User { device_id: d, .. } if d == device_id => {}
            _ => return Err("take over first".into()),
        }
        let run = self.running().ok_or("the browser is not running")?;
        self.last_input.store(now_ms(), Ordering::SeqCst);
        let page = run.page();
        let s = Some(page.session_id.as_str());
        let (w, h) = *lock(&run.last_meta);
        let frac = |k: &str| {
            ev[k]
                .as_f64()
                .filter(|v| v.is_finite())
                .map(|v| v.clamp(0.0, 1.0))
        };
        let cdp = &run.cdp;
        // Control may change while an input is under way: check before
        // every step.
        let still = || match self.control() {
            Control::User { device_id: d, .. } if d == device_id => Ok(()),
            _ => Err::<(), crate::util::BoxError>("take over first".into()),
        };
        match ev["kind"].as_str().unwrap_or("") {
            "tap" => {
                let (x, y) = (
                    frac("x").ok_or("x is required")? * w,
                    frac("y").ok_or("y is required")? * h,
                );
                tools::click_at(cdp, s, x, y, &still).await?;
            }
            "scroll" => {
                let x = frac("x").unwrap_or(0.5) * w;
                let y = frac("y").unwrap_or(0.5) * h;
                let dy = ev["dy"].as_f64().unwrap_or(0.0).clamp(-4000.0, 4000.0);
                let dx = ev["dx"].as_f64().unwrap_or(0.0).clamp(-4000.0, 4000.0);
                cdp.call(
                    "Input.dispatchMouseEvent",
                    json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy}),
                    s,
                )
                .await?;
            }
            "text" => {
                let text = ev["text"].as_str().unwrap_or("");
                if text.chars().count() > 2000 {
                    return Err("text is too long".into());
                }
                if !text.is_empty() {
                    cdp.call("Input.insertText", json!({"text": text}), s)
                        .await?;
                }
            }
            "key" => {
                let key = ev["key"].as_str().unwrap_or("");
                tools::press_key(cdp, s, key, &still).await?;
            }
            "back" => {
                tools::history_step(cdp, s, -1, still).await?;
            }
            "navigate" => {
                let url = tools::normalize_url(ev["url"].as_str().unwrap_or(""))?;
                cdp.call("Page.navigate", json!({"url": url}), s).await?;
            }
            _ => return Err("unknown input kind".into()),
        }
        Ok(())
    }
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
        assert!(err.is_err());
    }

    #[test]
    fn secrets_are_redacted_in_every_form() {
        let run = Running {
            cdp: Cdp::detached(),
            child: Mutex::new(None),
            page: Mutex::default(),
            no_sandbox: false,
            version: String::new(),
            cast: Mutex::new(None),
            cast_lock: tokio::sync::Mutex::new(()),
            attach_lock: tokio::sync::Mutex::new(()),
            dialogs: Mutex::default(),
            last_meta: Mutex::new((0.0, 0.0)),
            secrets: Mutex::default(),
            originals: Mutex::default(),
            secret_docs: Mutex::default(),
            recover_lock: tokio::sync::Mutex::new(()),
            seeded: AtomicBool::new(false),
        };
        run.remember_secret("hunter2-long");
        run.remember_secret("4242424242424242");
        run.remember_secret("a b+c/d?e");
        let text = "alert: hunter2-long / 4242 4242 4242 4242 / 4242-4242-4242-4242 / \
                    4242424242424242 / ?p=a+b%2Bc%2Fd%3Fe / a%20b%2Bc%2Fd%3Fe";
        let out = run.redact(text);
        assert!(!out.contains("hunter2") && !out.contains("4242"), "{out}");
        assert!(!out.contains("b%2Bc"), "{out}");
        // Shown with other escaping (a browser's display form of an address).
        let shown = "Page: x\nhttp://h/p?pw=a+b%2Bc/d%3Fe&x=1\nnext line";
        let out = run.redact(shown);
        assert_eq!(out, "Page: x\nhttp://h/p?••••\nnext line");
        assert!(run.shows_secret("t?q=a%20b%2Bc%2Fd?e"));
        assert_eq!(decode_loose("a%2"), "a%2");
        // Any card grouping, and a secret in an address path.
        run.remember_secret("378282246310005");
        assert_eq!(run.redact("Amex 3782 822463 10005."), "Amex ••••.");
        assert_eq!(run.redact("t\t3782\t8224\n6310005"), "t\t••••");
        run.remember_secret("pa/ss?word");
        assert_eq!(run.redact("https://shop.test/pa%2fss%3fword?x=1"), "••••");
        assert_eq!(decode_loose("p%c3%A4ss"), "päss");
        assert_eq!(decode_loose("p%E4ss"), "päss");
        // '+' as written, and Windows-1252 bytes.
        run.remember_secret("päss+word");
        assert_eq!(run.redact("https://s.test/p%C3%A4ss+word"), "••••");
        run.remember_secret("p€ssword-long");
        assert_eq!(run.redact("x?q=p%80ssword-long"), "x?••••");
        assert_eq!(decode_loose("%41+%zz"), "A %zz");
        // Shorter than MIN_SECRET is never remembered: such secrets are
        // never filled (fill.rs refuses them).
        run.remember_secret("12");
        assert!(!run.secrets().contains(&"12".to_string()));
        assert!(run.shows_secret("x hunter2-long y"));
        assert!(!run.shows_secret("nothing here"));
    }

    #[test]
    fn watchers_pick_the_best_mode() {
        let (_t, b) = browser();
        let (g1, _r1) = b.watch(Mode::Peek);
        let (g2, _r2) = b.watch(Mode::Full);
        assert_eq!(lock(&b.watchers).values().max(), Some(&Mode::Full));
        drop(g2);
        assert_eq!(lock(&b.watchers).values().max(), Some(&Mode::Peek));
        drop(g1);
        assert!(lock(&b.watchers).is_empty());
        assert!(Mode::Peek.min_interval() > Mode::Full.min_interval());
        assert_eq!(Mode::parse("full"), Some(Mode::Full));
        assert_eq!(Mode::parse("x"), None);
    }
}
