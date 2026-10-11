//! Configuration file and on-disk layout.
//!
//! Runtime state never lives in the repository:
//! - config: `$XDG_CONFIG_HOME/iris/config.toml` (default `~/.config/iris`)
//! - data:   `$XDG_DATA_HOME/iris` (default `~/.local/share/iris`) holding
//!   `iris.db`, `persona/`, `memory/`, `workspace/` (harness cwd), `uploads/`,
//!   `library/`, and private key files.

use crate::util::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const CONFIG_TEMPLATE: &str = include_str!("../templates/config.toml");

/// Harness adapters compiled into this binary.
pub const SUPPORTED_HARNESSES: &[&str] = &["claude", "codex", "gemini", "opencode"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub bind: String,
    pub public_url: String,
    pub agent_name: String,
    pub max_concurrent_turns: usize,
    pub turn_timeout_secs: u64,
    pub worker_threads: usize,
    pub strip_api_key_env: bool,
    pub max_upload_bytes: usize,
    pub harness: BTreeMap<String, HarnessConfig>,
    pub router: RouterConfig,
    pub heartbeat: JobConfig,
    pub feed: JobConfig,
    pub push: PushConfig,
    pub engines: EnginesConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnginesConfig {
    /// Where engines installed from the app live. Empty: `<data dir>/engines`.
    pub dir: String,
    /// Command prefix that runs `iris engines ...` as a separate admin user
    /// (privilege split). Empty: install in-process as the current user.
    pub helper: Vec<String>,
    /// Update engines Iris installed once a day.
    pub auto_update: bool,
}

impl Default for EnginesConfig {
    fn default() -> Self {
        Self {
            dir: String::new(),
            helper: Vec::new(),
            auto_update: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessConfig {
    pub enabled: bool,
    /// Executable name (resolved from PATH) or absolute path.
    pub command: String,
    pub models: Vec<String>,
    pub efforts: Vec<String>,
    /// Also offer the models the CLI itself reports (codex models cache,
    /// `opencode models`).
    pub discover_models: bool,
    /// Codex: sandbox policy (`read-only`, `workspace-write`, ...).
    /// OpenCode: the same names map to edit/shell permission rules.
    pub sandbox: String,
    /// Claude: `--permission-mode`. Gemini: `--approval-mode`.
    pub permission_mode: String,
    /// Claude only: tools pre-approved with `--allowedTools`. The Iris MCP
    /// bridge and enabled connectors are always added.
    pub allowed_tools: Vec<String>,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterConfig {
    /// Claude subscriptions are for individual use: conversations with more
    /// than one person never use Claude unless this is true (or the owner
    /// turns it on in the app). Leave it off unless your plan allows it.
    pub allow_claude_for_shared: bool,
    /// Optional TOML file layered over the built-in model rankings.
    pub rankings_file: String,
    /// Ask the `classifier` slot's model about messages the rule-based
    /// effort classifier finds ambiguous. Costs one small extra request per
    /// such message; off by default.
    pub classifier_model: bool,
    /// Message length thresholds (characters) for the effort classifier.
    pub long_chars: usize,
    pub very_long_chars: usize,
}

/// Router keys from earlier versions (fixed model tiers). Ignored with a
/// warning so existing config files keep working.
const LEGACY_ROUTER_KEYS: &[&str] = &[
    "default",
    "default_effort",
    "strong",
    "strong_effort",
    "deep",
    "deep_effort",
    "shared_default",
    "shared_default_effort",
    "shared_strong",
    "shared_strong_effort",
    "shared_deep",
    "shared_deep_effort",
    "strong_keywords",
    "deep_keywords",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobConfig {
    pub enabled: bool,
    pub interval_mins: u64,
    /// Local wall-clock window ("HH:MM") in which the job may run.
    pub active_start: String,
    pub active_end: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PushConfig {
    pub enabled: bool,
    /// VAPID `sub` claim: a `mailto:` or `https:` URL push services can use
    /// to contact the operator.
    pub vapid_subject: String,
    /// Also push assistant replies when no app window is connected.
    pub notify_replies_when_offline: bool,
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            command: String::new(),
            models: Vec::new(),
            efforts: strings(&["low", "medium", "high"]),
            discover_models: false,
            sandbox: "read-only".into(),
            permission_mode: "acceptEdits".into(),
            allowed_tools: Vec::new(),
            extra_args: Vec::new(),
        }
    }
}

impl HarnessConfig {
    pub fn claude_default() -> Self {
        Self {
            command: "claude".into(),
            models: strings(&["haiku", "sonnet", "opus"]),
            efforts: strings(&["low", "medium", "high", "xhigh", "max"]),
            allowed_tools: strings(&["Read", "Glob", "Grep", "WebSearch", "WebFetch"]),
            ..Self::default()
        }
    }

    pub fn codex_default() -> Self {
        Self {
            command: "codex".into(),
            models: strings(&["gpt-6-luna", "gpt-6.1-sol", "gpt-6-astra"]),
            efforts: strings(&["low", "medium", "high", "xhigh"]),
            discover_models: true,
            sandbox: "workspace-write".into(),
            ..Self::default()
        }
    }

    pub fn gemini_default() -> Self {
        Self {
            command: "gemini".into(),
            // Aliases the CLI resolves to the newest version it offers.
            models: strings(&["flash", "pro", "flash-lite"]),
            efforts: Vec::new(),
            sandbox: String::new(),
            permission_mode: "auto_edit".into(),
            ..Self::default()
        }
    }

    pub fn opencode_default() -> Self {
        Self {
            command: "opencode".into(),
            models: Vec::new(),
            efforts: Vec::new(),
            discover_models: true,
            sandbox: "workspace-write".into(),
            permission_mode: String::new(),
            ..Self::default()
        }
    }

    pub fn defaults_for(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::claude_default()),
            "codex" => Some(Self::codex_default()),
            "gemini" => Some(Self::gemini_default()),
            "opencode" => Some(Self::opencode_default()),
            _ => None,
        }
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            allow_claude_for_shared: false,
            rankings_file: String::new(),
            classifier_model: false,
            long_chars: 1200,
            very_long_chars: 6000,
        }
    }
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_mins: 60,
            active_start: "08:00".into(),
            active_end: "22:00".into(),
        }
    }
}

impl JobConfig {
    fn feed_default() -> Self {
        Self {
            interval_mins: 360,
            ..Self::default()
        }
    }
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            vapid_subject: String::new(),
            notify_replies_when_offline: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let harness = SUPPORTED_HARNESSES
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    HarnessConfig::defaults_for(n).expect("supported"),
                )
            })
            .collect();
        Self {
            bind: "127.0.0.1:7420".into(),
            public_url: String::new(),
            agent_name: "Iris".into(),
            max_concurrent_turns: 2,
            turn_timeout_secs: 600,
            worker_threads: 2,
            strip_api_key_env: true,
            max_upload_bytes: 10 * 1024 * 1024,
            harness,
            router: RouterConfig::default(),
            heartbeat: JobConfig::default(),
            feed: JobConfig::feed_default(),
            push: PushConfig::default(),
            engines: EnginesConfig::default(),
        }
    }
}

/// Parse "HH:MM" into minutes since midnight.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let mut table: toml::Table = toml::from_str(text)?;
        // A partial `[harness.codex]` (or `[feed]`) table must be layered over
        // its specific defaults, not the generic struct defaults, so merge at
        // the TOML level before deserializing.
        let user_harness = match table.remove("harness") {
            Some(toml::Value::Table(t)) => t,
            Some(_) => return Err("`harness` must be a table".into()),
            None => toml::Table::new(),
        };
        let mut merged = toml::Table::new();
        for name in SUPPORTED_HARNESSES {
            let defaults = HarnessConfig::defaults_for(name).expect("supported harness");
            merged.insert(
                name.to_string(),
                toml::Value::Table(toml::Table::try_from(defaults)?),
            );
        }
        for (name, value) in user_harness {
            let toml::Value::Table(overrides) = value else {
                return Err(format!("harness.{name} must be a table").into());
            };
            if !SUPPORTED_HARNESSES.contains(&name.as_str()) {
                return Err(format!(
                    "unknown harness '{name}' (supported: {})",
                    SUPPORTED_HARNESSES.join(", ")
                )
                .into());
            }
            if let Some(toml::Value::Table(base)) = merged.get_mut(&name) {
                base.extend(overrides);
            }
        }
        table.insert("harness".into(), toml::Value::Table(merged));
        if let Some(toml::Value::Table(router)) = table.get_mut("router") {
            let legacy: Vec<String> = router
                .keys()
                .filter(|k| LEGACY_ROUTER_KEYS.contains(&k.as_str()))
                .cloned()
                .collect();
            for k in &legacy {
                router.remove(k);
            }
            if !legacy.is_empty() {
                crate::warn!(
                    "config: ignoring router.{} (models now come from rankings; see router.rankings_file)",
                    legacy.join(", router.")
                );
            }
        }
        if let Some(toml::Value::Table(feed)) = table.remove("feed") {
            let mut base = toml::Table::try_from(JobConfig::feed_default())?;
            base.extend(feed);
            table.insert("feed".into(), toml::Value::Table(base));
        }
        let cfg: Config = table.try_into()?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()).into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display()).into()),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.max_concurrent_turns == 0 {
            return Err("max_concurrent_turns must be at least 1".into());
        }
        if self.turn_timeout_secs == 0 {
            return Err("turn_timeout_secs must be at least 1".into());
        }
        if self.agent_name.trim().is_empty() {
            return Err("agent_name must not be empty".into());
        }
        for (name, h) in &self.harness {
            if h.command.trim().is_empty() {
                return Err(format!("harness.{name}.command is empty").into());
            }
        }
        if !self.engines.helper.is_empty() && self.engines.dir.trim().is_empty() {
            return Err("engines.helper needs engines.dir (a directory the helper owns)".into());
        }
        if !self.router.rankings_file.trim().is_empty() {
            crate::ranking::Rankings::load(Some(Path::new(self.router.rankings_file.trim())))?;
        }
        for (name, job) in [("heartbeat", &self.heartbeat), ("feed", &self.feed)] {
            if job.interval_mins == 0 {
                return Err(format!("{name}.interval_mins must be at least 1").into());
            }
            if parse_hhmm(&job.active_start).is_none() || parse_hhmm(&job.active_end).is_none() {
                return Err(format!("{name}.active_start/active_end must be HH:MM").into());
            }
        }
        Ok(())
    }

    /// Engines directory (absolute).
    pub fn engines_dir(&self, paths: &Paths) -> PathBuf {
        let d = self.engines.dir.trim();
        if d.is_empty() {
            paths.data_dir.join("engines")
        } else {
            absolutize(PathBuf::from(d))
        }
    }

    /// The VAPID contact sent to push services: `push.vapid_subject` (a
    /// `mailto:` or `https:` URL), else an `https:` `public_url`. Apple
    /// rejects pushes without one, so without it push is unavailable.
    pub fn push_subject(&self) -> Option<String> {
        let valid = |s: &str| {
            s.starts_with("mailto:") && s.len() > 7 || s.starts_with("https://") && s.len() > 8
        };
        let subject = self.push.vapid_subject.trim();
        if valid(subject) {
            return Some(subject.to_string());
        }
        let url = self.public_url.trim().trim_end_matches('/');
        url.starts_with("https://")
            .then(|| url.to_string())
            .filter(|u| valid(u))
    }

    /// URL printed by `iris pair`.
    pub fn base_url(&self) -> String {
        if !self.public_url.trim().is_empty() {
            return self.public_url.trim().trim_end_matches('/').to_string();
        }
        let bind = self.bind.replace("0.0.0.0", "127.0.0.1");
        format!("http://{bind}")
    }
}

/// Resolved filesystem locations.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn resolve(config_override: Option<PathBuf>, data_override: Option<PathBuf>) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| ".".into());
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
        };
        let config_file = config_override
            .or_else(|| std::env::var_os("IRIS_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("iris/config.toml"));
        let data_dir = data_override
            .or_else(|| std::env::var_os("IRIS_DATA_DIR").map(PathBuf::from))
            .unwrap_or_else(|| xdg("XDG_DATA_HOME", ".local/share").join("iris"));
        Self {
            config_file: absolutize(config_file),
            data_dir: absolutize(data_dir),
        }
    }

    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("iris.db")
    }
    pub fn persona_dir(&self) -> PathBuf {
        self.data_dir.join("persona")
    }
    pub fn memory_dir(&self) -> PathBuf {
        self.data_dir.join("memory")
    }
    /// Working directory for harness subprocesses. Claude Code scopes
    /// sessions by cwd, so this must stay stable for `--resume` to work.
    pub fn workspace_dir(&self) -> PathBuf {
        self.data_dir.join("workspace")
    }
    /// User-uploaded attachments (images, avatar).
    pub fn uploads_dir(&self) -> PathBuf {
        self.data_dir.join("uploads")
    }
    /// Artifacts and media produced by the agent.
    pub fn library_dir(&self) -> PathBuf {
        self.data_dir.join("library")
    }
    /// Private per-turn runtime files (system prompt, MCP config).
    pub fn run_dir(&self) -> PathBuf {
        self.data_dir.join("run")
    }
    /// 32-byte key for the credentials vault (0600).
    pub fn vault_key_file(&self) -> PathBuf {
        self.data_dir.join("vault.key")
    }
    /// VAPID key pair for Web Push (0600).
    pub fn vapid_key_file(&self) -> PathBuf {
        self.data_dir.join("vapid.json")
    }
}

fn absolutize(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        return p;
    }
    std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
}

/// Create a directory (and parents) with owner-only permissions.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !path.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let mut perms = std::fs::metadata(path)?.permissions();
    if perms.mode() & 0o077 != 0 {
        perms.set_mode(0o700);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Atomically write a file readable only by the owner.
/// Create `path` (0600) with `contents` only if it does not exist yet, as
/// one atomic step: a complete private file is hard-linked into place, which
/// fails if another process created it first. Returns false if it existed.
/// For keys: two processes starting at once must not each write their own.
pub fn create_private_file_once(path: &Path, contents: &[u8]) -> Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}.tmp", crate::util::new_id()));
    let tmp = path.with_file_name(tmp_name);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // Unique temp name so concurrent writers never interleave; rename is atomic.
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}.tmp", crate::util::new_id()));
    let tmp = path.with_file_name(tmp_name);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(contents)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_matches_defaults() {
        let parsed = Config::parse(CONFIG_TEMPLATE).expect("template parses");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn empty_config_is_default() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn partial_tables_keep_specific_defaults() {
        let cfg =
            Config::parse("[harness.codex]\nenabled = false\n[feed]\nenabled = true\n").unwrap();
        let codex = &cfg.harness["codex"];
        assert!(!codex.enabled);
        assert_eq!(codex.command, "codex");
        assert!(codex.models.contains(&"gpt-6-luna".to_string()));
        assert!(codex.discover_models);
        assert_eq!(codex.efforts, HarnessConfig::codex_default().efforts);
        assert_eq!(cfg.harness["claude"], HarnessConfig::claude_default());
        assert!(cfg.feed.enabled);
        assert_eq!(cfg.feed.interval_mins, 360);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Config::parse("bnid = \"x\"").is_err());
        assert!(Config::parse("[harness.nope]\ncommand = \"nope\"").is_err());
        assert!(Config::parse("max_concurrent_turns = 0").is_err());
        assert!(Config::parse("[heartbeat]\nactive_start = \"25:00\"").is_err());
        assert!(Config::parse("agent_name = \" \"").is_err());
    }

    #[test]
    fn legacy_router_keys_are_ignored() {
        let cfg = Config::parse(
            "[router]\ndefault = \"claude:haiku\"\nstrong_keywords = [\"plan\"]\nallow_claude_for_shared = true\n",
        )
        .unwrap();
        assert!(cfg.router.allow_claude_for_shared);
        assert!(Config::parse("[router]\nbogus = 1\n").is_err());
    }

    #[test]
    fn hhmm_parsing() {
        assert_eq!(parse_hhmm("08:30"), Some(510));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("8"), None);
    }

    #[test]
    fn push_needs_a_contact() {
        let mut cfg = Config::default();
        assert_eq!(cfg.push_subject(), None, "default config has none");
        cfg.public_url = "http://100.64.0.2:7420".into();
        assert_eq!(cfg.push_subject(), None);
        cfg.public_url = "https://iris.example.ts.net/".into();
        assert_eq!(
            cfg.push_subject().as_deref(),
            Some("https://iris.example.ts.net")
        );
        cfg.push.vapid_subject = "mailto:admin@example.com".into();
        assert_eq!(
            cfg.push_subject().as_deref(),
            Some("mailto:admin@example.com")
        );
        cfg.push.vapid_subject = "admin@example.com".into();
        assert_eq!(
            cfg.push_subject().as_deref(),
            Some("https://iris.example.ts.net")
        );
    }

    #[test]
    fn base_url_prefers_public_url() {
        let mut cfg = Config::default();
        assert_eq!(cfg.base_url(), "http://127.0.0.1:7420");
        cfg.public_url = "https://iris.example.ts.net/".into();
        assert_eq!(cfg.base_url(), "https://iris.example.ts.net");
    }
}
