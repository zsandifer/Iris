//! Shared server state: config, database, harness registry, model catalog,
//! event hub, and runtime settings stored in the database.

use crate::auth::PairLimiter;
use crate::catalog::{Catalog, codex_cache_path};
use crate::config::{Config, HarnessConfig, Paths, PersonDirs};
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
}

pub struct HarnessEntry {
    pub harness: Box<dyn Harness>,
    pub cfg: HarnessConfig,
    pub status: RwLock<HarnessStatus>,
    /// Models the CLI listed at the last probe (OpenCode).
    pub models: RwLock<Vec<DiscoveredModel>>,
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
        };
        Self {
            harness,
            cfg,
            status: RwLock::new(status),
            models: RwLock::default(),
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
    pub pair_limiter: PairLimiter,
    /// Cancel handles of running turns, by conversation id.
    pub running: Mutex<HashMap<String, watch::Sender<bool>>>,
    /// Serializes turns within one conversation.
    pub conv_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    catalog: Mutex<(Option<CatalogKey>, Arc<Catalog>)>,
    /// Bumped whenever a CLI reports a different model list.
    models_generation: AtomicU64,
    /// Path of this binary, used to launch `iris mcp`.
    pub exe: PathBuf,
    pub rankings: Rankings,
    pub logins: crate::login::LoginManager,
    pub engine_ops: crate::setup::EngineOps,
    serving: std::sync::atomic::AtomicBool,
    /// Last broadcast recommendation per slot (to announce changes).
    last_recommended: Mutex<BTreeMap<Slot, Option<String>>>,
    /// Voice synthesis and transcription run one at a time.
    pub voice_jobs: tokio::sync::Semaphore,
    /// Speech synthesis runs one at a time, apart from transcription, so a
    /// backlog of notes never holds up someone talking to Iris.
    pub tts_jobs: tokio::sync::Semaphore,
    /// Messages whose voice note is being made or waiting (bounded).
    pub tts_pending: Mutex<std::collections::HashSet<String>>,
    /// Voice-note requests admitted (bounded, duplicates included).
    pub tts_waiting: std::sync::atomic::AtomicUsize,
    /// Recordings waiting for `voice_jobs` (bounded).
    pub voice_waiting: std::sync::atomic::AtomicUsize,
    /// Writes to a person's own data hold this shared; removing a person
    /// holds it exclusively, so no write lands after the removal.
    pub people_lock: tokio::sync::RwLock<()>,
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

pub const DEFAULT_FEED_PROMPT: &str = "Write a few short, genuinely interesting posts about \
things I care about (see USER.md and PROACTIVE_PREFERENCES.md): news, ideas, or finds I would \
want to see. Skip anything I have already seen in the feed.";

impl AppState {
    pub fn new(cfg: Config, paths: Paths, db: Db) -> Result<Self> {
        let vault = Vault::open(&paths.vault_key_file())?;
        let vapid = if cfg.push.enabled {
            Some(VapidKeys::load_or_create(&paths.vapid_key_file())?)
        } else {
            None
        };
        let mut harnesses = BTreeMap::new();
        for (name, hc) in &cfg.harness {
            let h: Box<dyn Harness> = match name.as_str() {
                "claude" => Box::new(Claude::new(hc.clone())),
                "codex" => Box::new(Codex::new(hc.clone())),
                "gemini" => Box::new(Gemini::new(hc.clone())),
                "opencode" => Box::new(OpenCode::new(hc.clone())),
                _ => continue,
            };
            harnesses.insert(name.clone(), HarnessEntry::new(h, hc.clone()));
        }
        let exe = std::env::current_exe()?;
        crate::harness::set_engines_bin(crate::engines::bin_dir(&cfg.engines_dir(&paths)));
        let rankings_file = cfg.router.rankings_file.trim();
        let rankings = Rankings::load(
            (!rankings_file.is_empty()).then(|| std::path::Path::new(rankings_file)),
        )?;
        Ok(Self(Arc::new(Inner {
            rankings,
            logins: crate::login::LoginManager::default(),
            engine_ops: crate::setup::EngineOps::default(),
            serving: std::sync::atomic::AtomicBool::new(false),
            last_recommended: Mutex::default(),
            voice_jobs: tokio::sync::Semaphore::new(1),
            voice_waiting: std::sync::atomic::AtomicUsize::new(0),
            tts_jobs: tokio::sync::Semaphore::new(1),
            tts_pending: Mutex::default(),
            tts_waiting: std::sync::atomic::AtomicUsize::new(0),
            people_lock: tokio::sync::RwLock::new(()),
            limiter: Limiter::new(cfg.max_concurrent_turns),
            cfg,
            paths,
            db,
            hub: Hub::new(),
            vault,
            vapid,
            harnesses,
            pair_limiter: PairLimiter::default(),
            running: Mutex::default(),
            conv_locks: Mutex::default(),
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

    async fn refresh_one(&self, entry: &HarnessEntry) {
        {
            let before = entry.status();
            let mut next = before.clone();
            next.checked_at = Some(now_ms());
            match (entry.cfg.enabled, entry.program()) {
                (false, _) => {
                    next.installed = entry.program().is_some();
                }
                (true, None) => {
                    next.installed = false;
                    next.path = None;
                    next.version = None;
                    next.auth = AuthState::Unknown;
                }
                (true, Some(program)) => {
                    next.installed = true;
                    next.path = Some(program.to_string_lossy().into_owned());
                    let cwd = self.paths.workspace_dir();
                    let strip = self.strip_env(entry);
                    if let Ok((_, out, _)) =
                        runner::probe(&program, &["--version".into()], &cwd, &strip).await
                    {
                        let v = out.lines().next().unwrap_or("").trim();
                        next.version = (!v.is_empty()).then(|| v.chars().take(80).collect());
                    }
                    if let Some(args) = entry.harness.auth_probe_args() {
                        match runner::probe(&program, &args, &cwd, &strip).await {
                            Ok((ok, out, err)) => {
                                next.auth = entry.harness.parse_auth_probe(ok, &out, &err);
                                next.auth_message = (next.auth == AuthState::AuthRequired)
                                    .then(|| "The CLI reports it is not signed in".to_string());
                            }
                            Err(_) => next.auth = AuthState::Unknown,
                        }
                    }
                    // Without a status command the state comes from turns
                    // and is kept (see `recheck_harnesses`).
                    self.probe_models(entry, &program).await;
                }
            }
            let changed = before.installed != next.installed
                || before.auth != next.auth
                || before.version != next.version;
            *entry.status.write().unwrap_or_else(|p| p.into_inner()) = next.clone();
            if changed {
                self.broadcast_harness(&next);
            }
            if before.auth != next.auth {
                self.auth_transition(entry.harness.name(), before.auth, next.auth)
                    .await;
            }
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

    /// Ask the CLI which models it can use now (OpenCode). Cheap: a local
    /// command, no model request.
    pub async fn probe_models(&self, entry: &HarnessEntry, program: &std::path::Path) -> bool {
        if !entry.cfg.discover_models {
            return false;
        }
        let Some(args) = entry.harness.models_probe_args() else {
            return false;
        };
        let cwd = self.paths.workspace_dir();
        let strip = self.strip_env(entry);
        match runner::probe(program, &args, &cwd, &strip).await {
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

    /// Short notices for the system prompt about engines that need the
    /// owner's attention, so the agent can mention them conversationally.
    pub fn harness_notices(&self, active: &str) -> Vec<String> {
        self.harnesses
            .values()
            .filter(|e| e.cfg.enabled && e.harness.name() != active)
            .filter(|e| e.status().auth == AuthState::AuthRequired)
            .map(|e| match e.harness.name() {
                "claude" => "The Claude engine on the Iris server is signed out. The owner can \
                    fix it by running `claude` on the server and using /login."
                    .to_string(),
                "codex" => "The Codex (ChatGPT) engine on the Iris server is signed out. The \
                    owner can fix it by running `codex login` on the server."
                    .to_string(),
                "gemini" => "The Gemini engine on the Iris server is signed out. The owner can \
                    fix it by running `gemini` on the server and signing in."
                    .to_string(),
                "opencode" => "The OpenCode engine on the Iris server has no usable provider. \
                    The owner can fix it with `opencode auth login` on the server."
                    .to_string(),
                other => format!("The {other} engine on the Iris server needs to be signed in."),
            })
            .collect()
    }

    // ---------------------------------------------------------------- slots

    /// The model's CLI can run a turn now and lists the model.
    pub fn model_available(&self, m: &crate::catalog::ModelEntry) -> bool {
        self.harness(&m.harness).is_some_and(|e| e.usable())
    }

    pub async fn slot_pins(&self) -> BTreeMap<Slot, Pin> {
        match self.setting("slots").await {
            Some(raw) => parse_slot_pins(&raw),
            // Earlier versions stored one pinned engine; it becomes the
            // chat slot's pin.
            None => legacy_engine_pin(&self.setting("engine").await.unwrap_or_default()),
        }
    }

    /// Engines whose CLI Iris can confine to one person's folders and
    /// configuration, so they may answer other people's turns: Claude Code
    /// (deny rules, `--setting-sources ""`, strict MCP) and Codex 0.162 or
    /// newer (permission profile, `--ignore-user-config`) when its home has
    /// no global AGENTS.md instructions (those reach every turn and cannot
    /// be switched off; only their presence is checked). Gemini CLI and
    /// OpenCode load configuration Iris cannot override.
    pub fn isolating(&self, harness: &str) -> bool {
        match harness {
            "claude" => true,
            "codex" => self.codex_profiles_ok() && !codex_global_instructions(),
            _ => false,
        }
    }

    /// The Codex CLI understands permission profiles (0.162 and newer).
    pub fn codex_profiles_ok(&self) -> bool {
        self.harness("codex")
            .and_then(|e| e.status().version)
            .is_some_and(|v| version_at_least(&v, (0, 162, 0)))
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
        // "Shared" here means other people's turns: same rules as routing.
        let allowed = |m: &crate::catalog::ModelEntry| {
            crate::router::policy_allows(&m.harness, shared, allow_shared)
                && (!shared || self.isolating(&m.harness))
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

    /// The owner's Iris name (server-level messages such as push tests).
    pub async fn agent_name(&self) -> String {
        self.setting("agent_name")
            .await
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.cfg.agent_name.clone())
    }

    pub async fn user(&self, id: &str) -> Option<db::User> {
        let id = id.to_string();
        self.db
            .call(move |c| db::get_user(c, &id))
            .await
            .ok()
            .flatten()
    }

    pub async fn owner_id(&self) -> Option<String> {
        self.db.call(db::owner_user_id).await.ok().flatten()
    }

    /// One person's Iris folders (persona, memory, workspace).
    pub fn dirs_for(&self, user: &db::User) -> PersonDirs {
        self.paths.person_dirs(&user.id, user.is_owner)
    }

    /// A per-person setting (each person's Iris has its own name, picture,
    /// feed prompt, and voice choice).
    pub async fn person_setting(&self, user: &db::User, key: &str) -> Option<String> {
        let k = person_key(&user.id, user.is_owner, key);
        self.db
            .call(move |c| db::get_setting(c, &k))
            .await
            .ok()
            .flatten()
    }

    pub async fn agent_name_for(&self, user: &db::User) -> String {
        self.person_setting(user, "agent_name")
            .await
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.cfg.agent_name.clone())
    }

    pub async fn feed_prompt_for(&self, user: &db::User) -> String {
        self.person_setting(user, "feed_prompt")
            .await
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_FEED_PROMPT.to_string())
    }

    /// The person turned on voice replies.
    pub async fn voice_replies_for(&self, user: &db::User) -> bool {
        self.person_setting(user, "voice_replies").await.as_deref() == Some("true")
    }

    /// `avatar_url` for a person's Iris (the `v` parameter busts caches).
    pub async fn avatar_url_for(&self, user: &db::User) -> (String, bool) {
        let custom = self.person_setting(user, "agent_avatar").await.is_some();
        let v = if custom {
            self.person_setting(user, "agent_avatar_updated")
                .await
                .unwrap_or_default()
        } else {
            "default".into()
        };
        // Always names whose Iris it is, so the URL means the same thing to
        // everyone in a Together chat.
        (format!("/api/avatar?user={}&v={v}", user.id), custom)
    }

    /// Settings as exposed to one person: their own Iris (name, picture,
    /// feed prompt, voice) plus the server-wide policy flag.
    pub async fn settings_json_for(&self, user: &db::User) -> Value {
        let (avatar_url, avatar_custom) = self.avatar_url_for(user).await;
        let root = self.cfg.engines_dir(&self.paths);
        json!({
            "agent_name": self.agent_name_for(user).await,
            "avatar_url": avatar_url,
            "avatar_custom": avatar_custom,
            "allow_claude_for_shared": self.allow_claude_for_shared().await,
            "feed_prompt": self.feed_prompt_for(user).await,
            "voice": {
                "enabled": self.voice_replies_for(user).await,
                "tts": crate::voice::installed(&root, crate::voice::Component::Tts),
                "stt": crate::voice::installed(&root, crate::voice::Component::Stt),
            },
        })
    }

    /// Send each listed person their own `settings.changed`.
    pub async fn broadcast_settings(&self, users: &[db::User]) {
        for u in users {
            let settings = self.settings_json_for(u).await;
            self.hub.to_users(
                std::slice::from_ref(&u.id),
                json!({"type": "settings.changed", "settings": settings}),
            );
        }
    }

    /// A person's Iris name or picture changed: tell the other members of
    /// the Together chats it hosts (`agent.changed`), so their phones show
    /// the new host identity. Nothing else about the person is sent.
    pub async fn broadcast_agent(&self, user: &db::User) {
        let uid = user.id.clone();
        let others: Vec<String> = self
            .db
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT DISTINCT m.user_id FROM conversations v
                     JOIN conversation_members m ON m.conversation_id = v.id
                     WHERE v.created_by = ?1 AND m.user_id != ?1",
                )?;
                stmt.query_map([&uid], |r| r.get(0))?.collect()
            })
            .await
            .unwrap_or_default();
        if others.is_empty() {
            return;
        }
        let (avatar_url, avatar_custom) = self.avatar_url_for(user).await;
        self.hub.to_users(
            &others,
            json!({"type": "agent.changed", "agent": {
                "user_id": user.id, "name": self.agent_name_for(user).await,
                "avatar_url": avatar_url, "avatar_custom": avatar_custom}}),
        );
    }

    /// Everyone on this server, oldest first.
    pub async fn all_users(&self) -> Vec<db::User> {
        self.db.call(db::list_users).await.unwrap_or_default()
    }

    /// Per-conversation turn lock.
    pub fn conversation_lock(&self, conversation_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.conv_locks.lock().unwrap_or_else(|p| p.into_inner());
        // Drop locks nobody holds to keep the map small.
        map.retain(|_, l| Arc::strong_count(l) > 1);
        map.entry(conversation_id.to_string()).or_default().clone()
    }
}

/// Whether a version string such as `codex-cli 0.162.1` is at least `min`.
pub fn version_at_least(v: &str, min: (u64, u64, u64)) -> bool {
    let Some(num) = v
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
    else {
        return false;
    };
    let mut parts = num
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u64>().unwrap_or(0));
    let got = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    got >= min
}

/// Codex's global instruction files exist (`$CODEX_HOME/AGENTS.md` or
/// `AGENTS.override.md`). Only their existence is checked.
fn codex_global_instructions() -> bool {
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".codex")));
    home.is_some_and(|h| h.join("AGENTS.md").exists() || h.join("AGENTS.override.md").exists())
}

/// Settings key for one person's Iris. The owner keeps the original
/// top-level keys (`agent_name`, `agent_avatar`, ...).
pub fn person_key(user_id: &str, is_owner: bool, key: &str) -> String {
    if is_owner {
        key.to_string()
    } else {
        format!("person.{user_id}.{key}")
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

    #[test]
    fn versions_compare() {
        assert!(version_at_least("codex-cli 0.162.1", (0, 162, 0)));
        assert!(version_at_least("1.0.0", (0, 162, 0)));
        assert!(!version_at_least("codex-cli 0.100.0", (0, 162, 0)));
        assert!(!version_at_least("unknown", (0, 162, 0)));
    }
}
