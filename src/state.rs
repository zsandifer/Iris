//! Shared server state: config, database, harness registry, model catalog,
//! event hub, and runtime settings stored in the database.

use crate::auth::PairLimiters;
use crate::catalog::{Catalog, codex_cache_path};
use crate::config::{Config, HarnessConfig, Paths};
use crate::db::{self, Db};
use crate::events::Hub;
use crate::harness::runner::{self, Limiter};
use crate::harness::{
    AuthState, DiscoveredModel, Harness, claude::Claude, codex::Codex, gemini::Gemini,
    opencode::OpenCode, resolve_command,
};
use crate::push::VapidKeys;
use crate::ranking::{self, Pin, Rankings, Resolution, Slot};
use crate::secrets::Vault;
use crate::util::{Result, now_ms};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;
use tokio::sync::watch;

#[derive(Debug, Clone, Serialize)]
pub struct HarnessStatus {
    pub name: String,
    pub enabled: bool,
    pub installed: bool,
    /// Resolved executable path.
    pub path: Option<String>,
    /// Output of `<cli> --version`, as reported by the CLI.
    pub version: Option<String>,
    /// ok | auth_required | unknown
    pub auth: AuthState,
    /// Short reason when auth is required (never contains account data).
    pub auth_message: Option<String>,
    pub checked_at: Option<i64>,
    pub isolation: String,
    pub agent_user: Option<String>,
}

pub struct HarnessEntry {
    pub harness: Box<dyn Harness>,
    pub cfg: HarnessConfig,
    pub status: RwLock<HarnessStatus>,
    /// Models the CLI listed at the last probe (OpenCode).
    pub models: RwLock<Vec<DiscoveredModel>>,
    /// Counts sign-in reports from turns and logins, so a probe that
    /// started before one never overwrites it.
    auth_reports: AtomicU64,
    /// Orders concurrent probes: only a probe newer than the last applied
    /// one may set the sign-in state.
    probe_seq: AtomicU64,
    last_probe_applied: AtomicU64,
}

impl HarnessEntry {
    fn new(harness: Box<dyn Harness>, cfg: HarnessConfig) -> Self {
        let status = HarnessStatus {
            name: harness.name().into(),
            enabled: cfg.enabled,
            installed: false,
            path: None,
            version: None,
            auth: AuthState::Unknown,
            auth_message: None,
            checked_at: None,
            isolation: "single_user".into(),
            agent_user: None,
        };
        Self {
            harness,
            cfg,
            status: RwLock::new(status),
            models: RwLock::default(),
            auth_reports: AtomicU64::new(0),
            probe_seq: AtomicU64::new(0),
            last_probe_applied: AtomicU64::new(0),
        }
    }

    pub fn status(&self) -> HarnessStatus {
        self.status
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn program(&self) -> Option<PathBuf> {
        resolve_command(&self.cfg.command)
    }

    /// Enabled, installed, and not known to be signed out.
    pub fn usable(&self) -> bool {
        let s = self.status();
        s.enabled && s.installed && s.auth != AuthState::AuthRequired
    }

    pub fn set_auth(&self, auth: AuthState, message: Option<String>) -> bool {
        let mut s = self.status.write().unwrap_or_else(|p| p.into_inner());
        self.auth_reports.fetch_add(1, Ordering::SeqCst);
        let changed = s.auth != auth;
        s.auth = auth;
        s.auth_message = message;
        changed
    }
}

pub struct Inner {
    pub cfg: Config,
    pub paths: Paths,
    pub db: Db,
    pub hub: Hub,
    pub vault: Vault,
    pub vapid: Option<VapidKeys>,
    pub harnesses: BTreeMap<String, HarnessEntry>,
    pub limiter: Limiter,
    pub pair_limiter: PairLimiters,
    /// Cancel handles of running turns, by conversation id.
    pub running: Mutex<HashMap<String, watch::Sender<bool>>>,
    /// Set once shutdown begins: turns that start afterwards (queued behind
    /// another in the same chat) are cancelled before running a CLI.
    pub stopping: std::sync::atomic::AtomicBool,
    /// Serializes turns within one conversation.
    pub conv_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Bumped after anyone joins or leaves a conversation (or it is
    /// deleted), so running turns re-check who may see them.
    pub membership_generation: AtomicU64,
    catalog: Mutex<(Option<CatalogKey>, Arc<Catalog>)>,
    /// Bumped whenever a CLI reports a different model list.
    models_generation: AtomicU64,
    /// Path of this binary, used to launch `iris mcp`.
    pub exe: PathBuf,
    pub rankings: Rankings,
    pub logins: crate::login::LoginManager,
    pub engine_ops: crate::setup::EngineOps,
    /// Iris's computer: the agent's browser.
    pub browser: crate::browser::Browser,
    /// Connector gateway: turn grants, running connectors, approvals.
    pub gateway: crate::gateway::Gateway,
    /// Connector sign-ins (OAuth) in progress.
    pub oauth: crate::oauth::Flows,
    serving: std::sync::atomic::AtomicBool,
    /// Last broadcast recommendation per slot (to announce changes).
    last_recommended: Mutex<BTreeMap<Slot, Option<String>>>,
}

/// Catalog cache key: (codex cache mtime, discovery generation).
type CatalogKey = (Option<SystemTime>, u64);

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

pub const DEFAULT_FEED_PROMPT: &str = "News, ideas, and finds that fit what I care about (see \
USER.md and PROACTIVE_PREFERENCES.md). Prefer original reporting and creators over aggregators.";

impl AppState {
    pub fn new(cfg: Config, paths: Paths, db: Db) -> Result<Self> {
        if let Some(p) = crate::agent::policy(&cfg.agent)?
            && (p.service_uid != unsafe { libc::geteuid() }
                || p.data_dir != paths.data_dir
                || p.config_file != paths.config_file)
        {
            return Err("service paths or account do not match agent setup".into());
        }
        crate::agent::migrate_workspace(&paths)?;
        let session_boundary = format!("conversation-cwd-v1:{}", cfg.agent.user);
        db.with(|c| {
            if db::get_setting(c, "agent.session_boundary")?.as_deref() != Some(&session_boundary) {
                c.execute("DELETE FROM harness_sessions", [])?;
                db::set_setting(c, "agent.session_boundary", &session_boundary)?;
            }
            Ok(())
        })?;
        let vault = Vault::open(&paths.vault_key_file())?;
        let vapid = if cfg.push.enabled {
            Some(VapidKeys::load_or_create(&paths.vapid_key_file())?)
        } else {
            None
        };
        let split = crate::agent::policy(&cfg.agent)?;
        let mut harnesses = BTreeMap::new();
        for (name, hc) in &cfg.harness {
            let mut hc = hc.clone();
            if let Some(policy) = &split {
                let engine = policy
                    .engines
                    .get(name)
                    .ok_or("engine is missing from agent policy")?;
                if hc.command != *name && std::path::Path::new(&hc.command) != engine {
                    return Err(
                        "configured CLI path must be registered in the root agent policy".into(),
                    );
                }
                hc.command = engine.to_string_lossy().into_owned();
            }
            let h: Box<dyn Harness> = match name.as_str() {
                "claude" => Box::new(Claude::new(hc.clone())),
                "codex" => Box::new(Codex::new(hc.clone())),
                "gemini" => Box::new(Gemini::new(hc.clone())),
                "opencode" => Box::new(OpenCode::new(hc.clone())),
                _ => continue,
            };
            harnesses.insert(name.clone(), HarnessEntry::new(h, hc.clone()));
        }
        for entry in harnesses.values() {
            let mut status = entry.status.write().unwrap_or_else(|p| p.into_inner());
            if !cfg.agent.user.is_empty() {
                status.isolation = "split".into();
                status.agent_user = Some(cfg.agent.user.clone());
            }
        }
        let exe = std::env::current_exe()?;
        crate::harness::set_engines_bin(crate::engines::bin_dir(&cfg.engines_dir(&paths)));
        let rankings_file = cfg.router.rankings_file.trim();
        let rankings = Rankings::load(
            (!rankings_file.is_empty()).then(|| std::path::Path::new(rankings_file)),
        )?;
        let hub = Hub::new();
        let browser = crate::browser::Browser::new(cfg.browser.clone(), &paths, hub.clone());
        browser.init_paused(db.with(|c| db::get_setting(c, "paused"))?.is_some());
        browser.set_disconnected(
            db.with(|c| db::get_setting(c, "browser.disconnected"))?
                .is_some(),
        );
        {
            let (db, vault) = (db.clone(), vault.clone());
            browser.set_secret_source(Arc::new(move || {
                crate::browser::fill::saved_secrets_blocking(&db, &vault)
            }));
        }
        Ok(Self(Arc::new(Inner {
            rankings,
            browser,
            logins: crate::login::LoginManager::default(),
            engine_ops: crate::setup::EngineOps::default(),
            gateway: crate::gateway::Gateway::default(),
            oauth: crate::oauth::Flows::default(),
            serving: std::sync::atomic::AtomicBool::new(false),
            last_recommended: Mutex::default(),
            limiter: Limiter::new(cfg.max_concurrent_turns),
            cfg,
            paths,
            db,
            hub,
            vault,
            vapid,
            harnesses,
            pair_limiter: PairLimiters::default(),
            running: Mutex::default(),
            conv_locks: Mutex::default(),
            stopping: std::sync::atomic::AtomicBool::new(false),
            membership_generation: AtomicU64::new(0),
            catalog: Mutex::new((None, Arc::new(Catalog::default()))),
            models_generation: AtomicU64::new(0),
            exe,
        })))
    }

    /// Model catalog, rebuilt when the codex models cache changes (one
    /// `stat`) or a CLI reported a different model list.
    pub fn catalog(&self) -> Arc<Catalog> {
        let mtime = codex_cache_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        let key = Some((mtime, self.models_generation.load(Ordering::SeqCst)));
        let mut guard = self.catalog.lock().unwrap_or_else(|p| p.into_inner());
        if guard.0 != key {
            let probed: BTreeMap<String, Vec<DiscoveredModel>> = self
                .harnesses
                .iter()
                .map(|(n, e)| {
                    let m = e.models.read().unwrap_or_else(|p| p.into_inner());
                    (n.clone(), m.clone())
                })
                .filter(|(_, m)| !m.is_empty())
                .collect();
            *guard = (key, Arc::new(Catalog::load(&self.cfg, &probed)));
        }
        guard.1.clone()
    }

    /// Replace a harness's discovered models; true if they changed.
    pub fn set_discovered_models(
        &self,
        entry: &HarnessEntry,
        models: Vec<DiscoveredModel>,
    ) -> bool {
        let mut cur = entry.models.write().unwrap_or_else(|p| p.into_inner());
        if *cur == models {
            return false;
        }
        *cur = models;
        self.models_generation.fetch_add(1, Ordering::SeqCst);
        true
    }

    pub fn harness(&self, name: &str) -> Option<&HarnessEntry> {
        self.harnesses.get(name)
    }

    pub fn harness_statuses(&self) -> Vec<HarnessStatus> {
        self.harnesses.values().map(HarnessEntry::status).collect()
    }

    /// Probe each harness: resolve the binary, read `--version`, and ask the
    /// CLI whether it is signed in. Broadcasts status changes.
    pub async fn refresh_harnesses(&self) {
        for entry in self.harnesses.values() {
            self.refresh_one(entry).await;
        }
    }

    /// Re-probe one harness now; returns its sign-in state.
    pub async fn probe_auth(&self, name: &str) -> AuthState {
        match self.harness(name) {
            Some(e) => {
                self.refresh_one(e).await;
                e.status().auth
            }
            None => AuthState::Unknown,
        }
    }

    /// The owner asked to check again (Reconnect, or after an install). A
    /// CLI without a sign-in status command (Gemini) cannot be probed, so
    /// its last turn's sign-in failure is forgotten and the next turn tells.
    pub async fn recheck_auth(&self, name: &str) -> AuthState {
        if let Some(e) = self.harness(name)
            && e.harness.auth_probe_args().is_none()
        {
            e.set_auth(AuthState::Unknown, None);
        }
        self.probe_auth(name).await
    }

    async fn refresh_one(&self, entry: &HarnessEntry) {
        // Probe first, then apply only what the probes found onto the
        // status as it is by then: a turn may report a sign-in change (the
        // only source for Gemini) while a probe runs.
        let program = entry.program();
        let reports = entry.auth_reports.load(Ordering::SeqCst);
        let seq = entry.probe_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let mut version: Option<Option<String>> = None;
        let mut auth: Option<(AuthState, Option<String>)> = None;
        let mut has_keys = false;
        if let (true, Some(program)) = (entry.cfg.enabled, &program) {
            let strip = self.strip_env(entry);
            let keys = crate::keys::env_for(self, entry.harness.name()).await;
            has_keys = !keys.is_empty();
            if let Ok((_, out, _)) = self
                .agent_probe(entry, program, &["--version".into()], &strip, &[])
                .await
            {
                let v = out.lines().next().unwrap_or("").trim();
                version = Some((!v.is_empty()).then(|| v.chars().take(80).collect()));
            }
            if let Some(args) = entry.harness.auth_probe_args() {
                auth = Some(
                    match self.agent_probe(entry, program, &args, &strip, &keys).await {
                        Ok((ok, out, err)) => {
                            let state = entry.harness.parse_auth_probe(ok, &out, &err);
                            let message = (state == AuthState::AuthRequired)
                                .then(|| "The CLI reports it is not signed in".to_string());
                            (state, message)
                        }
                        Err(_) => (AuthState::Unknown, None),
                    },
                );
            }
            // Without a status command the state comes from turns and is
            // kept (see `recheck_auth`).
            self.probe_models(entry, program).await;
        }
        let (before, next) = {
            let mut status = entry.status.write().unwrap_or_else(|p| p.into_inner());
            let before = status.clone();
            status.checked_at = Some(now_ms());
            status.installed = program.is_some();
            if entry.cfg.enabled {
                status.path = program.as_ref().map(|p| p.to_string_lossy().into_owned());
                if program.is_none() {
                    status.version = None;
                    status.auth = AuthState::Unknown;
                }
            }
            if let Some(v) = version {
                status.version = v;
            }
            // Only if nothing newer arrived while probing: a turn or login
            // report, or a later probe that already finished.
            if let Some((state, message)) = auth
                && entry.auth_reports.load(Ordering::SeqCst) == reports
                && entry.last_probe_applied.load(Ordering::SeqCst) < seq
            {
                if has_keys && state == AuthState::AuthRequired {
                    // The owner added an API key; some status commands
                    // only know about the subscription sign-in. Keep what
                    // the key check or turns learned instead.
                    if status.auth != AuthState::Ok {
                        status.auth = AuthState::Unknown;
                    }
                    status.auth_message = None;
                } else {
                    status.auth = state;
                    status.auth_message = message;
                }
                entry.last_probe_applied.store(seq, Ordering::SeqCst);
            }
            (before, status.clone())
        };
        let changed = before.installed != next.installed
            || before.auth != next.auth
            || before.version != next.version;
        if changed {
            self.broadcast_harness(&next);
        }
        if !before.installed && next.installed {
            crate::awareness::finish(
                self,
                "install",
                entry.harness.name(),
                "done",
                &format!(
                    "{} installed ✓",
                    crate::login::engine_label(entry.harness.name())
                ),
            )
            .await;
        }
        if before.auth != next.auth {
            self.auth_transition(entry.harness.name(), before.auth, next.auth)
                .await;
        }
    }

    /// Record a sign-in state learned from a turn, with the same side
    /// effects as a probe (broadcast, reconnect card).
    pub async fn note_auth(&self, name: &str, auth: AuthState, message: Option<String>) {
        let Some(entry) = self.harness(name) else {
            return;
        };
        let before = entry.status().auth;
        if entry.set_auth(auth, message) {
            self.broadcast_harness(&entry.status());
            self.auth_transition(name, before, auth).await;
        }
    }

    /// Reconnect cards: posted when a harness the owner has used goes
    /// signed out, closed when it is signed in again.
    async fn auth_transition(&self, name: &str, before: AuthState, after: AuthState) {
        let key = format!("harness.ever_ok.{name}");
        match after {
            AuthState::Ok => {
                if before != AuthState::Ok {
                    let k = key.clone();
                    let _ = self.db.call(move |c| db::set_setting(c, &k, "1")).await;
                    crate::login::on_signed_in(self, name).await;
                    crate::setup::update_setup_card(self).await;
                }
            }
            AuthState::AuthRequired => {
                let k = key.clone();
                let ever_ok = self
                    .db
                    .call(move |c| db::get_setting(c, &k))
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                if ever_ok {
                    crate::login::on_signed_out(self, name).await;
                }
            }
            AuthState::Unknown => {}
        }
    }

    /// True once `iris serve` is running (CLI commands such as `iris
    /// status` never post cards).
    pub fn is_serving(&self) -> bool {
        self.serving.load(Ordering::SeqCst)
    }

    pub fn set_serving(&self) {
        self.serving.store(true, Ordering::SeqCst);
    }

    /// Owner-requested re-check: also forget sign-in failures of CLIs that
    /// have no status command, so the next turn tries them again.
    pub async fn recheck_harnesses(&self) {
        for e in self.harnesses.values() {
            if e.harness.auth_probe_args().is_none() {
                e.set_auth(AuthState::Unknown, None);
            }
        }
        self.refresh_harnesses().await;
    }

    pub(crate) async fn agent_probe(
        &self,
        entry: &HarnessEntry,
        program: &std::path::Path,
        args: &[String],
        strip: &[&str],
        env: &[(String, String)],
    ) -> Result<(bool, String, String)> {
        let cwd = crate::agent::workdir(&self.cfg.agent, &self.paths, "probe")?;
        if self.cfg.agent.user.is_empty() {
            return runner::probe(program, args, &cwd, strip, env).await;
        }
        let mut inv = crate::harness::Invocation {
            args: args.to_vec(),
            env: env.to_vec(),
            ..Default::default()
        };
        crate::agent::wrap(&self.cfg.agent, entry.harness.name(), "probe", &mut inv)?;
        runner::probe_invocation(program, &inv, &cwd, strip).await
    }

    pub(crate) fn protected_paths(&self) -> Result<Vec<PathBuf>> {
        let mut paths = vec![self.paths.data_dir.clone(), self.paths.config_file.clone()];
        if let Some(parent) = self.paths.config_file.parent() {
            paths.push(parent.into());
        }
        let home = crate::agent::policy(&self.cfg.agent)?
            .map(|p| p.home)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        if let Some(home) = home {
            paths.extend(
                [
                    ".claude/.credentials.json",
                    ".claude.json",
                    ".codex/auth.json",
                    ".gemini/oauth_creds.json",
                    ".gemini/google_accounts.json",
                    ".local/share/opencode/auth.json",
                ]
                .map(|p| home.join(p)),
            );
        }
        // Respect nonstandard CLI homes in single-user mode too.
        if self.cfg.agent.user.is_empty() {
            for (key, file) in [
                ("CODEX_HOME", "auth.json"),
                ("CLAUDE_CONFIG_DIR", ".credentials.json"),
                ("GEMINI_CLI_HOME", ".gemini/oauth_creds.json"),
            ] {
                if let Some(dir) = std::env::var_os(key) {
                    paths.push(PathBuf::from(dir).join(file));
                }
            }
        }
        Ok(paths)
    }

    /// Ask the CLI which models it can use now (OpenCode). Cheap: a local
    /// command, no model request.
    pub async fn probe_models(&self, entry: &HarnessEntry, program: &std::path::Path) -> bool {
        if !entry.cfg.discover_models {
            return false;
        }
        let Some(args) = entry.harness.models_probe_args() else {
            return false;
        };
        let strip = self.strip_env(entry);
        let keys = crate::keys::env_for(self, entry.harness.name()).await;
        match self.agent_probe(entry, program, &args, &strip, &keys).await {
            Ok((true, out, _)) => {
                let models = entry.harness.parse_models(&out);
                self.set_discovered_models(entry, models)
            }
            _ => false,
        }
    }

    /// Broadcast a status change. Server paths are left out because every
    /// user receives it; owners can read them from `GET /api/harnesses`.
    pub fn broadcast_harness(&self, status: &HarnessStatus) {
        let mut status = status.clone();
        status.path = None;
        self.hub.send(
            crate::events::Audience::All,
            json!({"type": "harness.status", "harness": status}),
        );
    }

    pub fn strip_env(&self, entry: &HarnessEntry) -> Vec<&'static str> {
        if self.cfg.strip_api_key_env {
            entry.harness.api_key_env().to_vec()
        } else {
            Vec::new()
        }
    }

    // ---------------------------------------------------------------- slots

    /// The model's CLI can run a turn now and lists the model.
    pub fn model_available(&self, m: &crate::catalog::ModelEntry) -> bool {
        self.harness(&m.harness).is_some_and(|e| e.usable())
    }

    pub async fn slot_pins(&self) -> BTreeMap<Slot, Pin> {
        self.db.call(stored_slot_pins).await.unwrap_or_default()
    }

    /// Owner opt-in (config or app setting) to use Claude in conversations
    /// with more than one person.
    pub async fn allow_claude_for_shared(&self) -> bool {
        self.cfg.router.allow_claude_for_shared
            || self.setting("allow_claude_for_shared").await.as_deref() == Some("true")
    }

    /// Resolve a slot for a personal (`shared == false`) or shared context.
    pub async fn resolve_slot(&self, slot: Slot, shared: bool) -> (Resolution, Option<Pin>) {
        let catalog = self.catalog();
        let pin = self.slot_pins().await.remove(&slot);
        let allow_shared = self.allow_claude_for_shared().await;
        let allowed = |m: &crate::catalog::ModelEntry| {
            crate::router::policy_allows(&m.harness, shared, allow_shared)
        };
        let available = |m: &crate::catalog::ModelEntry| self.model_available(m);
        let res = ranking::resolve(
            &self.rankings,
            slot,
            &catalog,
            &available,
            &allowed,
            pin.as_ref(),
        );
        (res, pin)
    }

    /// All slots as API JSON (personal context; `shared_recommended` adds
    /// the shared-chat answer for `chat`).
    pub async fn slots_json(&self) -> Vec<Value> {
        let catalog = self.catalog();
        let allow_shared = self.allow_claude_for_shared().await;
        let mut out = Vec::new();
        for slot in Slot::ALL {
            let (res, pin) = self.resolve_slot(slot, false).await;
            let allowed = |_: &crate::catalog::ModelEntry| true;
            let mut v =
                ranking::slot_json(&self.rankings, slot, &catalog, &res, pin.as_ref(), &allowed);
            if slot == Slot::Chat {
                let (shared, _) = self.resolve_slot(slot, true).await;
                v["shared"] = json!({
                    "effective": shared.effective,
                    "paused": shared.blocked_by_policy,
                    "allow_claude_for_shared": allow_shared,
                });
            }
            out.push(v);
        }
        out
    }

    /// Recompute recommendations and broadcast `models.changed` when any
    /// slot's automatic choice changed (new model installed, CLI signed in
    /// or out, cache updated). Cheap: one `stat` plus in-memory ranking.
    pub async fn check_recommendations(&self) {
        let mut now = BTreeMap::new();
        for slot in Slot::ALL {
            now.insert(slot, self.resolve_slot(slot, false).await.0.recommended);
        }
        let changed = {
            let mut last = self
                .last_recommended
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let changed = !last.is_empty() && *last != now;
            *last = now;
            changed
        };
        if changed {
            crate::info!("model recommendations changed");
            self.hub.send(
                crate::events::Audience::All,
                json!({"type": "models.changed", "slots": self.slots_json().await}),
            );
        }
    }

    // ------------------------------------------------------------ settings

    pub async fn setting(&self, key: &'static str) -> Option<String> {
        self.db
            .call(move |c| db::get_setting(c, key))
            .await
            .ok()
            .flatten()
    }

    pub async fn agent_name(&self) -> String {
        self.setting("agent_name")
            .await
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.cfg.agent_name.clone())
    }

    /// The owner paused Iris: no new turns or jobs, no connector calls.
    pub async fn is_paused(&self) -> bool {
        self.setting("paused").await.is_some()
    }

    /// Recipients the owner trusts: messages only to them need no
    /// approval (normalized like the gate does).
    pub async fn trusted_recipients(&self) -> Vec<String> {
        self.setting("safety.trusted_recipients")
            .await
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
    }

    pub async fn feed_prompt(&self) -> String {
        self.setting("feed_prompt")
            .await
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_FEED_PROMPT.to_string())
    }

    /// Settings as exposed to clients.
    /// App settings as the API shows them. The feed instructions describe
    /// the owner's interests, so only the owner sees them.
    pub async fn settings_json(&self, owner: bool) -> Value {
        let avatar = self.setting("agent_avatar").await;
        let updated = self
            .setting("agent_avatar_updated")
            .await
            .unwrap_or_default();
        json!({
            "agent_name": self.agent_name().await,
            "avatar_url": format!("/api/avatar?v={}", if avatar.is_some() { updated } else { "default".into() }),
            "avatar_custom": avatar.is_some(),
            "allow_claude_for_shared": self.allow_claude_for_shared().await,
            "feed_prompt": if owner { Some(self.feed_prompt().await) } else { None },
            "voice": {"enabled": false, "status": "not_implemented"},
            "paused": self.is_paused().await,
            "memory": if owner { Some(json!({
                "learning_paused": self.setting(crate::memory::dream::PAUSED).await.as_deref() == Some("true"),
                "morning_note": self.setting(crate::memory::dream::MORNING).await.as_deref() != Some("false"),
            })) } else { None },
        })
    }

    /// Tell every app about changed settings (the owner's with the private
    /// parts) and return the owner's view.
    pub async fn broadcast_settings(&self) -> Value {
        let public = self.settings_json(false).await;
        self.hub.send(
            crate::events::Audience::All,
            json!({"type": "settings.changed", "settings": public}),
        );
        let full = self.settings_json(true).await;
        if let Ok(Some(owner)) = self.db.call(db::owner_user_id).await {
            self.hub.to_users(
                &[owner],
                json!({"type": "settings.changed", "settings": full}),
            );
        }
        full
    }

    /// Per-conversation turn lock.
    pub fn conversation_lock(&self, conversation_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.conv_locks.lock().unwrap_or_else(|p| p.into_inner());
        // Drop locks nobody holds to keep the map small.
        map.retain(|_, l| Arc::strong_count(l) > 1);
        map.entry(conversation_id.to_string()).or_default().clone()
    }
}

pub fn parse_slot_pins(raw: &str) -> BTreeMap<Slot, Pin> {
    let v: BTreeMap<String, Pin> = serde_json::from_str(raw).unwrap_or_default();
    v.into_iter()
        .filter(|(_, p)| !p.model.is_empty())
        .filter_map(|(k, p)| Slot::parse(&k).map(|s| (s, p)))
        .collect()
}

pub fn slot_pins_json(pins: &BTreeMap<Slot, Pin>) -> String {
    let m: BTreeMap<&str, &Pin> = pins.iter().map(|(s, p)| (s.name(), p)).collect();
    serde_json::to_string(&m).unwrap_or_else(|_| "{}".into())
}

/// The slot pins as stored. Earlier versions stored one pinned engine; it
/// becomes the chat slot's pin.
pub fn stored_slot_pins(c: &rusqlite::Connection) -> rusqlite::Result<BTreeMap<Slot, Pin>> {
    Ok(match crate::db::get_setting(c, "slots")? {
        Some(raw) => parse_slot_pins(&raw),
        None => legacy_engine_pin(&crate::db::get_setting(c, "engine")?.unwrap_or_default()),
    })
}

fn legacy_engine_pin(raw: &str) -> BTreeMap<Slot, Pin> {
    let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let mut out = BTreeMap::new();
    if let (Some("pinned"), Some(model)) = (v["mode"].as_str(), v["model"].as_str())
        && !model.is_empty()
    {
        out.insert(
            Slot::Chat,
            Pin {
                model: model.to_string(),
                effort: v["effort"].as_str().map(String::from),
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_upgrade_resets_cli_sessions_once_without_losing_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = Db::open(&paths.db_file()).unwrap();
        let conv = db
            .with(|c| {
                let owner = db::owner_user_id(c)?.unwrap();
                db::ensure_main_conversation(c, &owner)
            })
            .unwrap();
        let message = db
            .with(|c| {
                db::insert_message(
                    c,
                    &db::NewMessage {
                        conversation_id: &conv,
                        role: "user",
                        content: "Synthetic history",
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let session = db::HarnessSession {
            session_id: "old-session".into(),
            synced_at: 0,
            persona_hash: "old-persona".into(),
        };
        db.with(|c| db::set_harness_session(c, &conv, "claude", &session))
            .unwrap();
        let state = AppState::new(Config::default(), paths.clone(), db.clone()).unwrap();
        assert!(
            db.with(|c| db::get_harness_session(c, &conv, "claude"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state.harness("claude").unwrap().status().isolation,
            "single_user"
        );
        db.with(|c| db::set_harness_session(c, &conv, "claude", &session))
            .unwrap();
        let _state = AppState::new(Config::default(), paths, db.clone()).unwrap();
        assert_eq!(
            db.with(|c| db::get_message(c, &message.id))
                .unwrap()
                .unwrap()
                .content,
            "Synthetic history"
        );
        assert!(
            db.with(|c| db::get_harness_session(c, &conv, "claude"))
                .unwrap()
                .is_some()
        );
        assert!(
            db.with(|c| {
                let owner = db::owner_user_id(c)?.unwrap();
                db::get_conversation(c, &owner, &conv)
            })
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn legacy_engine_setting_becomes_chat_pin() {
        assert!(legacy_engine_pin("").is_empty());
        assert!(legacy_engine_pin(r#"{"mode":"automatic"}"#).is_empty());
        let p = legacy_engine_pin(r#"{"mode":"pinned","model":"codex:gpt-6-luna","effort":"low"}"#);
        assert_eq!(p[&Slot::Chat].model, "codex:gpt-6-luna");
        assert_eq!(p[&Slot::Chat].effort.as_deref(), Some("low"));
    }

    #[test]
    fn slot_pins_round_trip() {
        assert!(parse_slot_pins("").is_empty());
        let raw = r#"{"chat":{"model":"codex:gpt-6-luna","effort":"low"},"bogus":{"model":"x"},"feed":{"model":""}}"#;
        let pins = parse_slot_pins(raw);
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[&Slot::Chat].model, "codex:gpt-6-luna");
        assert_eq!(parse_slot_pins(&slot_pins_json(&pins)), pins);
    }
}
