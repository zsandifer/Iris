//! Model catalog: which `<harness>:<model>` ids can be selected, with their
//! supported reasoning efforts. Built from config plus the models each CLI
//! itself reports (the codex models cache, `opencode models`); metadata
//! only.

use crate::config::Config;
use crate::harness::DiscoveredModel;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelEntry {
    /// Stable id used by the API, e.g. "claude:haiku".
    pub id: String,
    pub harness: String,
    /// Value passed to the CLI's model flag.
    pub model: String,
    pub label: String,
    pub description: Option<String>,
    /// Reasoning effort values this model accepts.
    pub efforts: Vec<String>,
    /// Effort the CLI uses when none is passed, if known.
    pub default_effort: Option<String>,
    /// "config" or "discovered" (reported by the CLI).
    pub source: &'static str,
    /// The CLI hides this model from its own picker (still usable).
    pub hidden: bool,
    /// Accepts images; None when the CLI does not say (the harness
    /// capability decides).
    pub images: Option<bool>,
    /// Served from this machine or the local network.
    pub local: bool,
    /// Upstream provider, when the CLI reports one (OpenCode).
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Catalog {
    pub models: Vec<ModelEntry>,
}

/// Parsed `<harness>:<model>` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef<'a> {
    pub harness: &'a str,
    pub model: &'a str,
}

pub fn parse_model_ref(id: &str) -> Option<ModelRef<'_>> {
    let (harness, model) = id.split_once(':')?;
    (!harness.is_empty() && !model.is_empty()).then_some(ModelRef { harness, model })
}

/// The subset of a codex `models_cache.json` entry Iris reads.
#[derive(Debug, Clone, Deserialize)]
pub struct CodexCachedModel {
    pub slug: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<CodexReasoningLevel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexReasoningLevel {
    pub effort: String,
}

#[derive(Deserialize)]
struct CodexCacheFile {
    #[serde(default)]
    models: Vec<CodexCachedModel>,
}

/// Internal/hidden codex models that should never be offered for chat.
const CODEX_EXCLUDED: &[&str] = &["codex-auto-review"];

/// Parse the codex models cache, dropping internal (non-chat) models.
/// Models the CLI hides from its picker are kept but flagged `hidden`.
pub fn parse_codex_cache(json: &str) -> Vec<CodexCachedModel> {
    let Ok(file) = serde_json::from_str::<CodexCacheFile>(json) else {
        return Vec::new();
    };
    file.models
        .into_iter()
        .filter(|m| !m.slug.is_empty() && !CODEX_EXCLUDED.contains(&m.slug.as_str()))
        .collect()
}

/// Codex cache entries as discovered models.
pub fn codex_discovered(json: &str) -> Vec<DiscoveredModel> {
    parse_codex_cache(json)
        .into_iter()
        .map(|c| DiscoveredModel {
            efforts: c
                .supported_reasoning_levels
                .iter()
                .map(|l| l.effort.clone())
                .collect(),
            label: c.display_name,
            description: c.description,
            default_effort: c.default_reasoning_level,
            hidden: c.visibility.as_deref().is_some_and(|v| v != "list"),
            provider: Some("openai".into()),
            model: c.slug,
            ..DiscoveredModel::default()
        })
        .collect()
}

/// `$CODEX_HOME/models_cache.json`, defaulting to `~/.codex`.
pub fn codex_cache_path() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))?;
    Some(home.join("models_cache.json"))
}

pub fn default_label(harness: &str, model: &str) -> String {
    match (harness, model) {
        ("claude", "haiku") => "Claude Haiku".into(),
        ("claude", "sonnet") => "Claude Sonnet".into(),
        ("claude", "opus") => "Claude Opus".into(),
        ("claude", "fable") => "Claude Fable".into(),
        ("gemini", "flash") => "Gemini Flash".into(),
        ("gemini", "pro") => "Gemini Pro".into(),
        ("gemini", "flash-lite") => "Gemini Flash-Lite".into(),
        _ => model.to_string(),
    }
}

impl Catalog {
    /// Build from config. `discovered` holds the models each CLI reported,
    /// by harness name (used only where `discover_models` is on).
    pub fn build(cfg: &Config, discovered: &BTreeMap<String, Vec<DiscoveredModel>>) -> Self {
        let mut models = Vec::new();
        let none = Vec::new();
        for (harness, h) in &cfg.harness {
            if !h.enabled {
                continue;
            }
            let found = if h.discover_models {
                discovered.get(harness).unwrap_or(&none)
            } else {
                &none
            };
            let lookup = |m: &str| found.iter().find(|d| d.model == m);
            let mut push = |model: &str, source: &'static str, d: Option<&DiscoveredModel>| {
                let mut e = ModelEntry {
                    id: format!("{harness}:{model}"),
                    harness: harness.clone(),
                    model: model.to_string(),
                    label: default_label(harness, model),
                    description: None,
                    efforts: h.efforts.clone(),
                    default_effort: None,
                    source,
                    hidden: false,
                    images: None,
                    local: false,
                    provider: None,
                };
                if let Some(d) = d {
                    apply_discovered(&mut e, d);
                }
                models.push(e);
            };
            for model in &h.models {
                push(model, "config", lookup(model));
            }
            for d in found {
                if !h.models.iter().any(|m| m == &d.model) {
                    push(&d.model, "discovered", Some(d));
                }
            }
        }
        Self { models }
    }

    /// Build from config, reading the codex cache from disk when enabled.
    /// `probed` carries models listed by CLI commands (OpenCode).
    pub fn load(cfg: &Config, probed: &BTreeMap<String, Vec<DiscoveredModel>>) -> Self {
        let mut discovered = probed.clone();
        let wants_cache = cfg
            .harness
            .get("codex")
            .is_some_and(|h| h.enabled && h.discover_models);
        if let Some(json) = wants_cache
            .then(codex_cache_path)
            .flatten()
            .and_then(|p: PathBuf| read_small(&p))
        {
            discovered.insert("codex".into(), codex_discovered(&json));
        }
        Self::build(cfg, &discovered)
    }

    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Validate an effort for a model. `None` means "CLI default".
    pub fn check_effort(&self, model_id: &str, effort: Option<&str>) -> Result<(), String> {
        let Some(effort) = effort else { return Ok(()) };
        let entry = self
            .get(model_id)
            .ok_or_else(|| format!("unknown model '{model_id}'"))?;
        if entry.efforts.iter().any(|e| e == effort) {
            Ok(())
        } else {
            Err(format!(
                "model '{model_id}' does not support effort '{effort}' (supported: {})",
                entry.efforts.join(", ")
            ))
        }
    }

    /// Closest supported effort at or below `wanted`, so routing to a model
    /// with fewer levels degrades gracefully instead of failing.
    pub fn clamp_effort(&self, model_id: &str, wanted: &str) -> Option<String> {
        const ORDER: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
        let entry = self.get(model_id)?;
        if entry.efforts.iter().any(|e| e == wanted) {
            return Some(wanted.to_string());
        }
        let rank = ORDER.iter().position(|o| *o == wanted)?;
        ORDER[..rank]
            .iter()
            .rev()
            .find(|o| entry.efforts.iter().any(|e| e == *o))
            .map(|s| s.to_string())
    }
}

fn apply_discovered(entry: &mut ModelEntry, d: &DiscoveredModel) {
    if let Some(label) = &d.label {
        entry.label = label.clone();
    }
    entry.description = d.description.clone();
    if !d.efforts.is_empty() {
        entry.efforts = d.efforts.clone();
    }
    entry.default_effort = d.default_effort.clone();
    entry.hidden = d.hidden;
    entry.images = d.images;
    entry.local = d.local;
    entry.provider = d.provider.clone();
}

fn read_small(path: &Path) -> Option<String> {
    const MAX: u64 = 8 * 1024 * 1024;
    let meta = std::fs::metadata(path).ok()?;
    (meta.len() <= MAX).then(|| std::fs::read_to_string(path).ok())?
}

#[cfg(test)]
mod tests {
    use super::*;

    const CACHE: &str = r#"{
      "fetched_at": "2026-01-01T00:00:00Z",
      "models": [
        {"slug": "gpt-6-luna", "display_name": "GPT-6-Luna",
         "description": "Fast and affordable model for easier tasks.",
         "visibility": "list", "default_reasoning_level": "medium",
         "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}, {"effort": "high"}]},
        {"slug": "gpt-test-extra", "display_name": "GPT-Test-Extra", "visibility": "list",
         "supported_reasoning_levels": [{"effort": "low"}]},
        {"slug": "gpt-hidden", "visibility": "hide"},
        {"slug": "codex-auto-review", "visibility": "list"}
      ]
    }"#;

    #[test]
    fn parses_model_refs() {
        assert_eq!(
            parse_model_ref("claude:haiku"),
            Some(ModelRef {
                harness: "claude",
                model: "haiku"
            })
        );
        assert_eq!(
            parse_model_ref("codex:gpt-6.1-sol").unwrap().model,
            "gpt-6.1-sol"
        );
        assert!(parse_model_ref("auto").is_none());
        assert!(parse_model_ref(":x").is_none());
    }

    fn with_codex(json: &str) -> BTreeMap<String, Vec<DiscoveredModel>> {
        let mut m = BTreeMap::new();
        m.insert("codex".to_string(), codex_discovered(json));
        m
    }

    #[test]
    fn builds_from_config_without_cache() {
        let cat = Catalog::build(&Config::default(), &BTreeMap::new());
        let ids: Vec<_> = cat.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "claude:haiku",
                "claude:sonnet",
                "claude:opus",
                "codex:gpt-6-luna",
                "codex:gpt-6.1-sol",
                "codex:gpt-6-astra",
                "gemini:flash",
                "gemini:pro",
                "gemini:flash-lite",
            ]
        );
        assert_eq!(cat.get("claude:haiku").unwrap().label, "Claude Haiku");
    }

    #[test]
    fn cache_enriches_and_extends_codex_models() {
        let cat = Catalog::build(&Config::default(), &with_codex(CACHE));
        let luna = cat.get("codex:gpt-6-luna").unwrap();
        assert_eq!(luna.label, "GPT-6-Luna");
        assert_eq!(luna.efforts, ["low", "medium", "high"]);
        assert_eq!(luna.default_effort.as_deref(), Some("medium"));
        assert_eq!(
            cat.get("codex:gpt-test-extra").unwrap().source,
            "discovered"
        );
        assert!(cat.get("codex:gpt-hidden").unwrap().hidden);
        assert!(!luna.hidden);
        assert!(cat.get("codex:codex-auto-review").is_none());
    }

    #[test]
    fn disabled_harness_and_garbage_cache() {
        let mut cfg = Config::default();
        cfg.harness.get_mut("codex").unwrap().enabled = false;
        let cat = Catalog::build(&cfg, &with_codex("not json"));
        assert!(cat.models.iter().all(|m| m.harness != "codex"));
        let mut cfg = Config::default();
        cfg.harness.get_mut("codex").unwrap().discover_models = false;
        let cat = Catalog::build(&cfg, &with_codex(CACHE));
        assert!(cat.get("codex:gpt-test-extra").is_none());
    }

    #[test]
    fn effort_validation_and_clamping() {
        let cat = Catalog::build(&Config::default(), &with_codex(CACHE));
        assert!(cat.check_effort("claude:haiku", Some("max")).is_ok());
        assert!(cat.check_effort("claude:haiku", None).is_ok());
        assert!(cat.check_effort("codex:gpt-6-luna", Some("max")).is_err());
        assert_eq!(
            cat.clamp_effort("codex:gpt-6-luna", "xhigh").as_deref(),
            Some("high")
        );
        assert_eq!(
            cat.clamp_effort("codex:gpt-test-extra", "high").as_deref(),
            Some("low")
        );
        assert_eq!(
            cat.clamp_effort("claude:opus", "high").as_deref(),
            Some("high")
        );
        assert_eq!(cat.clamp_effort("nope:x", "high"), None);
    }
}
