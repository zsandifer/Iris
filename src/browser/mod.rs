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
        if secret.chars().count() < 3 {
            return;
        }
        let mut forms = vec![secret.to_string()];
        if secret.len() >= 12 && secret.bytes().all(|b| b.is_ascii_digit()) {
            let groups: Vec<&str> = secret
                .as_bytes()
                .chunks(4)
                .map(|c| std::str::from_utf8(c).unwrap_or(""))
                .collect();
            forms.push(groups.join(" "));
            forms.push(groups.join("-"));
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

    /// Replace every remembered secret in text the agent will read.
    pub fn redact(&self, text: &str) -> String {
        let secrets = lock(&self.secrets);
        let mut out = text.to_string();
        for s in secrets.iter() {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), "••••");
            }
        }
        out
    }

    /// Whether text contains a remembered secret.
    pub fn shows_secret(&self, text: &str) -> bool {
        lock(&self.secrets)
            .iter()
            .any(|s| text.contains(s.as_str()))
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
        }))
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

    /// The owner's choice about running without Chrome's sandbox.
    pub fn set_allow_no_sandbox(&self, allow: bool) {
        self.allow_no_sandbox.store(allow, Ordering::SeqCst);
        if allow {
            lock(&self.view).sandbox_unavailable = false;
        }
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

    fn set_page_info(&self, url: Option<&str>, title: Option<&str>) {
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
        if let Err(e) = self.attach(&run, &target).await {
            Self::shut(&run).await;
            return Err(e);
        }
        // Paused (or turned off) while starting: do not publish it.
        if self.is_paused() {
            Self::shut(&run).await;
            return Err("Iris is paused".into());
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
        let _g = run.attach_lock.lock().await;
        if run.page().target_id == target_id {
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
        self.control.send_replace(Control::Agent);
        self.frames.send_replace(None);
        self.broadcast_state();
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
            if !in_turn && !watched && !user && now - last > idle_ms {
                crate::info!("browser: idle; closing to free memory");
                self.close().await;
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
                    let msg = crate::util::truncate_chars(
                        ev.params["message"].as_str().unwrap_or(""),
                        200,
                    );
                    // Alerts and confirms follow from what was just clicked;
                    // prompts would need text, so they are dismissed.
                    let accept = kind != "prompt";
                    lock(&run.dialogs).push(format!(
                        "{kind} \"{msg}\" was {}",
                        if accept { "accepted" } else { "dismissed" }
                    ));
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
                        self.set_page_info(info["url"].as_str(), info["title"].as_str());
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
                        let (b, r) = (self.clone(), run.clone());
                        tokio::spawn(async move {
                            let _ = b.switch_to_any(&r).await;
                        });
                    }
                }
                _ => {}
            }
        }
    }

    /// The current tab closed: use another one (or open a blank tab).
    pub async fn switch_to_any(&self, run: &Running) -> Result<()> {
        let current = run.page().target_id;
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
        self.attach(run, &target).await
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
        let shot = run
            .cdp
            .call(
                "Page.captureScreenshot",
                json!({"format": "jpeg", "quality": mode.quality(), "clip": {
                    "x": vv["pageX"].as_f64().unwrap_or(0.0),
                    "y": vv["pageY"].as_f64().unwrap_or(0.0),
                    "width": w, "height": h, "scale": scale}}),
                s,
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
        match ev["kind"].as_str().unwrap_or("") {
            "tap" => {
                let (x, y) = (
                    frac("x").ok_or("x is required")? * w,
                    frac("y").ok_or("y is required")? * h,
                );
                tools::click_at(cdp, s, x, y).await?;
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
                tools::press_key(cdp, s, key).await?;
            }
            "back" => {
                tools::history_step(cdp, s, -1).await?;
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
        };
        run.remember_secret("hunter2-long");
        run.remember_secret("4242424242424242");
        run.remember_secret("12");
        let text = "alert: hunter2-long / 4242 4242 4242 4242 / 4242-4242-4242-4242 / 4242424242424242 / 12";
        let out = run.redact(text);
        assert!(!out.contains("hunter2") && !out.contains("4242"), "{out}");
        // Too short to redact safely (would mangle ordinary text).
        assert!(out.ends_with("/ 12"));
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
