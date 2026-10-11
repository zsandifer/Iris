//! Shared server state: config, database, harness registry, model catalog,
//! event hub, and runtime settings stored in the database.

use crate::auth::PairLimiter;
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
}

pub struct HarnessEntry {
    pub harness: Box<dyn Harness>,
    pub cfg: HarnessConfig,
    pub status: RwLock<HarnessStatus>,
    /// Models the CLI listed at the last probe (OpenCode).
    pub models: RwLock<Vec<DiscoveredModel>>,
    /// MCP servers configured in the CLI itself (names only), turned off in
    /// Iris's turns.
    pub foreign_mcp: RwLock<Vec<String>>,
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
            foreign_mcp: RwLock::default(),
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
    /// Bumped after anyone leaves a conversation (or it is deleted), so
    /// running turns stop streaming to people who lost access.
    pub membership_generation: AtomicU64,
    catalog: Mutex<(Option<CatalogKey>, Arc<Catalog>)>,
    /// Bumped whenever a CLI reports a different model list.
    models_generation: AtomicU64,
    /// Path of this binary, used to launch `iris mcp`.
    pub exe: PathBuf,
    pub rankings: Rankings,
    pub logins: crate::login::LoginManager,
    pub engine_ops: crate::setup::EngineOps,
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
            gateway: crate::gateway::Gateway::default(),
            oauth: crate::oauth::Flows::default(),
            serving: std::sync::atomic::AtomicBool::new(false),
            last_recommended: Mutex::default(),
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
                    let keys = crate::keys::env_for(self, entry.harness.name()).await;
                    if let Ok((_, out, _)) =
                        runner::probe(&program, &["--version".into()], &cwd, &strip, &[]).await
                    {
                        let v = out.lines().next().unwrap_or("").trim();
                        next.version = (!v.is_empty()).then(|| v.chars().take(80).collect());
                    }
                    if let Some(args) = entry.harness.auth_probe_args() {
                        match runner::probe(&program, &args, &cwd, &strip, &keys).await {
                            Ok((ok, out, err)) => {
                                next.auth = entry.harness.parse_auth_probe(ok, &out, &err);
                                next.auth_message = (next.auth == AuthState::AuthRequired)
                                    .then(|| "The CLI reports it is not signed in".to_string());
                            }
                            Err(_) => next.auth = AuthState::Unknown,
                        }
                    }
                    if !keys.is_empty() && next.auth == AuthState::AuthRequired {
                        // The owner added an API key; some status commands
                        // only know about the subscription sign-in. Keep
                        // what the key check or turns learned instead.
                        next.auth = if before.auth == AuthState::Ok {
                            AuthState::Ok
                        } else {
                            AuthState::Unknown
                        };
                        next.auth_message = None;
                    }
                    // Without a status command the state comes from turns
                    // and is kept (see `recheck_harnesses`).
                    self.probe_models(entry, &program).await;
                    if let Some(args) = entry.harness.foreign_mcp_probe_args()
                        && let Ok((true, out, _)) =
                            runner::probe(&program, &args, &cwd, &strip, &keys).await
                    {
                        *entry.foreign_mcp.write().unwrap_or_else(|p| p.into_inner()) =
                            entry.harness.parse_foreign_mcp(&out);
                    }
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
        let keys = crate::keys::env_for(self, entry.harness.name()).await;
        match runner::probe(program, &args, &cwd, &strip, &keys).await {
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
                    fix it by adding a Gemini API key in the app (Settings, AI engine), or by \
                    running `gemini` on the server and signing in."
                    .to_string(),
                "opencode" => "The OpenCode engine on the Iris server has no usable provider. \
                    The owner can fix it by adding a provider key in the app (Settings, AI \
                    engine), or with `opencode auth login` on the server."
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
    pub async fn settings_json(&self) -> Value {
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
            "feed_prompt": self.feed_prompt().await,
            "voice": {"enabled": false, "status": "not_implemented"},
            "paused": self.is_paused().await,
        })
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
}
