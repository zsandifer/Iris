//! Model rankings and slots. Iris never hard-codes a model: each slot
//! (`chat`, `chat_strong`, `background`, `feed`, `classifier`) has an
//! ordered preference of model families (`templates/model_rankings.toml`,
//! overridable), and the effective default is the highest-ranked model that
//! is actually available on this machine right now. Newly installed models
//! slot in automatically through family globs; newer versions outrank older
//! ones. Pinned slots are honored as long as the pinned model is usable.

use crate::catalog::{Catalog, ModelEntry};
use crate::util::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path;

pub const BUILTIN_RANKINGS: &str = include_str!("../templates/model_rankings.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    Chat,
    ChatStrong,
    Background,
    Feed,
    Classifier,
}

impl Slot {
    pub const ALL: [Slot; 5] = [
        Slot::Chat,
        Slot::ChatStrong,
        Slot::Background,
        Slot::Feed,
        Slot::Classifier,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Slot::Chat => "chat",
            Slot::ChatStrong => "chat_strong",
            Slot::Background => "background",
            Slot::Feed => "feed",
            Slot::Classifier => "classifier",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.name() == s)
    }

    pub fn description(self) -> &'static str {
        match self {
            Slot::Chat => "Everyday conversations",
            Slot::ChatStrong => "Hard requests (planning, code, research)",
            Slot::Background => "Heartbeat, scheduled jobs, and research helpers",
            Slot::Feed => "Feed generation",
            Slot::Classifier => "Helper for ambiguous effort decisions (off by default)",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Family {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    pub models: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Allowed harnesses, in preference order. Empty: any.
    #[serde(default)]
    pub harnesses: Vec<String>,
    #[serde(default)]
    pub local: Option<bool>,
    #[serde(default)]
    pub min_params_b: Option<f64>,
    /// "newest" (default) or "largest".
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub cost: Option<String>,
    /// Starting reasoning effort for this family's models (`low`,
    /// `medium`, `high`); the classifier may raise it, never lower it.
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RankingsFile {
    #[serde(default)]
    family: Vec<Family>,
    #[serde(default)]
    slots: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Rankings {
    pub families: Vec<Family>,
    pub slots: BTreeMap<Slot, Vec<String>>,
}

/// Name used for matching: OpenCode ids are `provider/model`.
pub fn match_name(m: &ModelEntry) -> String {
    let name = m
        .model
        .rsplit_once('/')
        .map_or(m.model.as_str(), |(_, n)| n);
    name.to_lowercase()
}

/// Case-insensitive glob with `*` and `?`.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Version numbers in a model name: `gpt-6.1-sol` -> [6, 1],
/// `claude-haiku-4-5-20251001` -> [4, 5], `qwen3:32b` -> [3],
/// `gemini-2.5-flash` -> [2, 5]. Parameter sizes and dates are skipped.
pub fn version_of(name: &str) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    let mut started = false;
    for tok in name.split(['-', '_', ':', '/', ' ']) {
        let tok = tok.to_lowercase();
        if tok.is_empty() || params_of_token(&tok).is_some() {
            if started {
                break;
            }
            continue;
        }
        let numeric = |s: &str| {
            !s.is_empty()
                && s.split('.')
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        };
        if numeric(&tok) {
            if tok.len() >= 6 {
                // A date stamp ends the version.
                break;
            }
            out.extend(tok.split('.').filter_map(|p| p.parse::<u32>().ok()));
            started = true;
            continue;
        }
        if !started {
            // Letters followed by a version: qwen3, llama3.3, phi4, r1.
            let digits_at = tok.find(|c: char| c.is_ascii_digit());
            if let Some(i) = digits_at {
                let (head, tail) = tok.split_at(i);
                if !head.is_empty()
                    && head.bytes().all(|b| b.is_ascii_alphabetic())
                    && numeric(tail)
                {
                    out.extend(tail.split('.').filter_map(|p| p.parse::<u32>().ok()));
                    started = true;
                    continue;
                }
            }
        } else {
            break;
        }
    }
    out
}

fn params_of_token(tok: &str) -> Option<f64> {
    let t = tok.strip_suffix('b')?;
    // Mixture of experts: 8x7b -> 56.
    if let Some((n, size)) = t.split_once('x') {
        return Some(n.parse::<f64>().ok()? * size.parse::<f64>().ok()?);
    }
    let t = t.strip_prefix('e').unwrap_or(t);
    t.parse::<f64>().ok().filter(|v| *v > 0.0 && *v < 10_000.0)
}

/// Parameter count in billions, parsed from the name (`qwen3:32b`).
pub fn params_of(name: &str) -> Option<f64> {
    name.to_lowercase()
        .split(['-', '_', ':', '/', ' '])
        .filter_map(params_of_token)
        .reduce(f64::max)
}

fn cmp_version(a: &[u32], b: &[u32]) -> Ordering {
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        match x.cmp(&y) {
            Ordering::Equal => continue,
            o => return o,
        }
    }
    Ordering::Equal
}

impl Family {
    pub fn matches(&self, m: &ModelEntry) -> bool {
        if !self.harnesses.is_empty() && !self.harnesses.contains(&m.harness) {
            return false;
        }
        if self.local.is_some_and(|l| l != m.local) {
            return false;
        }
        let name = match_name(m);
        if let Some(min) = self.min_params_b
            && params_of(&name).is_none_or(|p| p < min)
        {
            return false;
        }
        self.models.iter().any(|g| glob(g, &name)) && !self.exclude.iter().any(|g| glob(g, &name))
    }

    fn harness_rank(&self, harness: &str) -> usize {
        self.harnesses
            .iter()
            .position(|h| h == harness)
            .unwrap_or(self.harnesses.len())
    }
}

/// One model's place in a slot.
#[derive(Debug, Clone)]
pub struct Ranked<'a> {
    pub entry: &'a ModelEntry,
    /// Index of the slot entry it matched; `usize::MAX` if unlisted.
    pub position: usize,
}

impl Rankings {
    pub fn builtin() -> Self {
        Self::parse(BUILTIN_RANKINGS).expect("built-in rankings parse")
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: RankingsFile = toml::from_str(text)?;
        let mut out = Self {
            families: Vec::new(),
            slots: BTreeMap::new(),
        };
        out.merge(file)?;
        Ok(out)
    }

    fn merge(&mut self, file: RankingsFile) -> Result<()> {
        let mut families = file.family;
        families.extend(std::mem::take(&mut self.families));
        self.families = families;
        for (k, v) in file.slots {
            let slot = Slot::parse(&k).ok_or_else(|| format!("unknown slot '{k}' in rankings"))?;
            self.slots.insert(slot, v);
        }
        for (slot, ids) in &self.slots {
            for id in ids {
                if !self.families.iter().any(|f| &f.id == id) {
                    return Err(format!(
                        "rankings slot {} lists unknown family '{id}'",
                        slot.name()
                    )
                    .into());
                }
            }
        }
        Ok(())
    }

    /// Built-in table with an optional operator file layered on top.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut r = Self::builtin();
        if let Some(p) = path {
            let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
            let file: RankingsFile =
                toml::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?;
            r.merge(file).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        Ok(r)
    }

    pub fn family_of(&self, m: &ModelEntry) -> Option<&Family> {
        self.families.iter().find(|f| f.matches(m))
    }

    fn family(&self, id: &str) -> Option<&Family> {
        self.families.iter().find(|f| f.id == id)
    }

    /// Order `models` for a slot: listed families by position, then models
    /// of known but unlisted families, then unknown models.
    pub fn rank<'a>(&'a self, slot: Slot, models: &[&'a ModelEntry]) -> Vec<Ranked<'a>> {
        let list = self.slots.get(&slot).cloned().unwrap_or_default();
        struct Key<'f> {
            /// Family of the slot entry that matched (decides ordering).
            bucket: Option<&'f Family>,
            /// Most specific family (first match in file order).
            specific: Option<&'f Family>,
            /// Position of the specific family in this slot's list.
            specific_pos: usize,
        }
        let pos_of = |f: Option<&Family>| {
            f.and_then(|f| list.iter().position(|id| id == &f.id))
                .unwrap_or(usize::MAX)
        };
        let mut ranked: Vec<(Ranked<'a>, Key<'a>)> = models
            .iter()
            .map(|m| {
                let hit = list
                    .iter()
                    .enumerate()
                    .find_map(|(i, id)| self.family(id).filter(|f| f.matches(m)).map(|f| (i, f)));
                let specific = self.family_of(m);
                let (position, bucket) = match hit {
                    Some((i, f)) => (i, Some(f)),
                    None if specific.is_some() => (usize::MAX - 1, specific),
                    None => (usize::MAX, None),
                };
                (
                    Ranked { entry: m, position },
                    Key {
                        bucket,
                        specific,
                        specific_pos: pos_of(specific),
                    },
                )
            })
            .collect();
        ranked.sort_by(|(a, ka), (b, kb)| {
            let (na, nb) = (match_name(a.entry), match_name(b.entry));
            let size = || {
                params_of(&nb)
                    .unwrap_or(0.0)
                    .partial_cmp(&params_of(&na).unwrap_or(0.0))
                    .unwrap_or(Ordering::Equal)
            };
            let same_family = ka.specific.map(|f| &f.id) == kb.specific.map(|f| &f.id);
            // Versions only compare within one family; different families
            // follow this slot's family order.
            let family_then_version = || {
                if same_family {
                    cmp_version(&version_of(&nb), &version_of(&na))
                } else {
                    ka.specific_pos.cmp(&kb.specific_pos).then_with(|| {
                        let id = |k: &Key| k.specific.map(|f| f.id.clone());
                        id(ka).cmp(&id(kb))
                    })
                }
            };
            let largest = ka.bucket.and_then(|f| f.order.as_deref()) == Some("largest");
            a.position
                .cmp(&b.position)
                .then_with(|| {
                    if largest {
                        size().then_with(family_then_version)
                    } else {
                        family_then_version().then_with(size)
                    }
                })
                .then_with(|| {
                    let h = |r: &Ranked, k: &Key| {
                        k.bucket.map_or(0, |f| f.harness_rank(&r.entry.harness))
                    };
                    h(a, ka).cmp(&h(b, kb))
                })
                .then_with(|| b.entry.local.cmp(&a.entry.local))
                .then_with(|| a.entry.hidden.cmp(&b.entry.hidden))
                .then_with(|| a.entry.id.cmp(&b.entry.id))
        });
        ranked.into_iter().map(|(r, _)| r).collect()
    }
}

/// Provider for display: family, then what the CLI said, then the CLI's
/// own vendor.
pub fn provider_of(m: &ModelEntry, family: Option<&Family>) -> String {
    family
        .and_then(|f| f.provider.clone())
        .or_else(|| m.provider.clone())
        .unwrap_or_else(|| {
            match m.harness.as_str() {
                "claude" => "anthropic",
                "codex" => "openai",
                "gemini" => "google",
                other => other,
            }
            .to_string()
        })
}

/// Default effort for a model: its family's starting effort when the model
/// offers it, else `low` when it exposes efforts, else the lowest it offers
/// above "none"; None when it has no effort setting.
pub fn default_effort(m: &ModelEntry, family: Option<&Family>) -> Option<String> {
    if m.efforts.is_empty() {
        return None;
    }
    if let Some(e) = family.and_then(|f| f.effort.as_deref())
        && m.efforts.iter().any(|x| x == e)
    {
        return Some(e.to_string());
    }
    if m.efforts.iter().any(|e| e == "low") {
        return Some("low".into());
    }
    m.efforts
        .iter()
        .find(|e| !matches!(e.as_str(), "none" | "minimal"))
        .or(m.efforts.first())
        .cloned()
}

/// A slot pin set by the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
}

/// Result of resolving one slot.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// Available models in rank order (catalog ids).
    pub ranked: Vec<String>,
    /// Highest-ranked available model the policy allows.
    pub recommended: Option<String>,
    /// What turns use: the pin when usable, else `recommended`.
    pub effective: Option<String>,
    /// The pin exists but is not usable right now.
    pub pin_unavailable: bool,
    /// Available models exist but the policy allows none of them.
    pub blocked_by_policy: bool,
}

/// Resolve a slot. `available`: the model's CLI is installed, enabled, and
/// not signed out (and the CLI lists the model). `allowed`: policy.
pub fn resolve(
    rankings: &Rankings,
    slot: Slot,
    catalog: &Catalog,
    available: &dyn Fn(&ModelEntry) -> bool,
    allowed: &dyn Fn(&ModelEntry) -> bool,
    pin: Option<&Pin>,
) -> Resolution {
    let models: Vec<&ModelEntry> = catalog.models.iter().filter(|m| available(m)).collect();
    let ranked = rankings.rank(slot, &models);
    let recommended = ranked
        .iter()
        .find(|r| allowed(r.entry))
        .map(|r| r.entry.id.clone());
    let pin_ok = pin.is_some_and(|p| models.iter().any(|m| m.id == p.model && allowed(m)));
    Resolution {
        blocked_by_policy: !ranked.is_empty() && recommended.is_none(),
        ranked: ranked.iter().map(|r| r.entry.id.clone()).collect(),
        effective: if pin_ok {
            pin.map(|p| p.model.clone())
        } else {
            recommended.clone()
        },
        pin_unavailable: pin.is_some() && !pin_ok,
        recommended,
    }
}

/// JSON view of a slot for the API.
pub fn slot_json(
    rankings: &Rankings,
    slot: Slot,
    catalog: &Catalog,
    res: &Resolution,
    pin: Option<&Pin>,
    allowed: &dyn Fn(&ModelEntry) -> bool,
) -> Value {
    let models: Vec<Value> = res
        .ranked
        .iter()
        .enumerate()
        .filter_map(|(i, id)| {
            let m = catalog.get(id)?;
            let family = rankings.family_of(m);
            let cost = if m.local {
                "free".to_string()
            } else {
                family
                    .and_then(|f| f.cost.clone())
                    .unwrap_or_else(|| "unknown".into())
            };
            Some(json!({
                "id": m.id,
                "label": m.label,
                "harness": m.harness,
                "provider": provider_of(m, family),
                "family": family.map(|f| f.id.clone()),
                "known": family.is_some(),
                "local": m.local,
                "cost": cost,
                "efforts": m.efforts,
                "default_effort": default_effort(m, family),
                "images": m.images,
                "rank": i + 1,
                "recommended": res.recommended.as_deref() == Some(id.as_str()),
                "allowed": allowed(m),
            }))
        })
        .collect();
    let label_of = |id: &Option<String>| {
        id.as_deref()
            .and_then(|i| catalog.get(i))
            .map(|m| m.label.clone())
    };
    json!({
        "slot": slot.name(),
        "description": slot.description(),
        "selection": match pin {
            Some(p) => json!({"mode": "pinned", "model": p.model, "effort": p.effort}),
            None => json!({"mode": "automatic"}),
        },
        "recommended": res.recommended,
        "recommended_label": label_of(&res.recommended),
        "effective": res.effective,
        "pin_unavailable": res.pin_unavailable,
        "status": if res.effective.is_some() {
            "ok"
        } else if res.blocked_by_policy {
            "paused_policy"
        } else {
            "no_model"
        },
        "models": models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn entry(harness: &str, model: &str, local: bool) -> ModelEntry {
        ModelEntry {
            id: format!("{harness}:{model}"),
            harness: harness.into(),
            model: model.into(),
            label: model.into(),
            description: None,
            efforts: vec!["low".into(), "medium".into(), "high".into()],
            default_effort: None,
            source: "config",
            hidden: false,
            images: None,
            local,
            provider: None,
        }
    }

    fn ids(r: &[Ranked]) -> Vec<String> {
        r.iter().map(|x| x.entry.id.clone()).collect()
    }

    #[test]
    fn builtin_parses_and_lists_known_families() {
        let r = Rankings::builtin();
        for slot in Slot::ALL {
            assert!(!r.slots[&slot].is_empty(), "{}", slot.name());
        }
        assert!(Rankings::parse("[slots]\nchat = [\"nope\"]").is_err());
        assert!(Rankings::parse("[slots]\nbogus = []").is_err());
    }

    #[test]
    fn globs() {
        assert!(glob("gpt-*-luna*", "gpt-6-luna"));
        assert!(glob("gpt-*-luna*", "GPT-6.1-Luna-preview"));
        assert!(!glob("gpt-*-luna*", "gpt-6-sol"));
        assert!(glob("qwen*", "qwen3:32b"));
        assert!(glob("?hi*", "phi4"));
        assert!(glob("*", ""));
        assert!(!glob("flash", "flash-lite"));
    }

    #[test]
    fn versions_and_sizes() {
        assert_eq!(version_of("gpt-6.1-sol"), [6, 1]);
        assert_eq!(version_of("gpt-6-luna"), [6]);
        assert_eq!(version_of("claude-haiku-4-5-20251001"), [4, 5]);
        assert_eq!(version_of("qwen3:32b"), [3]);
        assert_eq!(version_of("llama3.3-70b"), [3, 3]);
        assert_eq!(version_of("gemini-2.5-flash"), [2, 5]);
        assert_eq!(version_of("gemini-3-flash-preview"), [3]);
        assert_eq!(version_of("gpt-oss-120b"), Vec::<u32>::new());
        assert_eq!(params_of("qwen3:32b"), Some(32.0));
        assert_eq!(params_of("mixtral-8x7b"), Some(56.0));
        assert_eq!(params_of("gemma3n:e4b"), Some(4.0));
        assert_eq!(params_of("haiku"), None);
        assert_eq!(cmp_version(&[6, 1], &[6]), Ordering::Greater);
    }

    #[test]
    fn chat_ranking_prefers_mid_size_models_then_small_ones() {
        let r = Rankings::builtin();
        let models = [
            entry("opencode", "ollama/qwen3:8b", true),
            entry("gemini", "flash", false),
            entry("codex", "gpt-6-luna", false),
            entry("claude", "haiku", false),
            entry("claude", "sonnet", false),
            entry("opencode", "lab/mystery-model", false),
        ];
        let refs: Vec<&ModelEntry> = models.iter().collect();
        assert_eq!(
            ids(&r.rank(Slot::Chat, &refs)),
            [
                "claude:sonnet",
                "claude:haiku",
                "codex:gpt-6-luna",
                "gemini:flash",
                "opencode:ollama/qwen3:8b",
                "opencode:lab/mystery-model"
            ]
        );
        let mid = [
            entry("gemini", "pro", false),
            entry("codex", "gpt-6.1-sol", false),
            entry("claude", "haiku", false),
            entry("claude", "sonnet", false),
        ];
        let mid_refs: Vec<&ModelEntry> = mid.iter().collect();
        assert_eq!(
            ids(&r.rank(Slot::Chat, &mid_refs)),
            [
                "claude:sonnet",
                "codex:gpt-6.1-sol",
                "gemini:pro",
                "claude:haiku"
            ]
        );
        // Background work stays on the small models.
        assert_eq!(ids(&r.rank(Slot::Background, &mid_refs))[0], "claude:haiku");
        assert_eq!(
            ids(&r.rank(Slot::ChatStrong, &refs))[..2],
            ["claude:sonnet", "opencode:ollama/qwen3:8b"]
        );
    }

    #[test]
    fn newer_versions_and_preferred_harness_win() {
        let r = Rankings::builtin();
        let models = [
            entry("codex", "gpt-6-luna", false),
            entry("opencode", "openai/gpt-6.1-luna", false),
            entry("codex", "gpt-6.1-luna", false),
            entry("codex", "gpt-7-luna", false),
        ];
        let refs: Vec<&ModelEntry> = models.iter().collect();
        assert_eq!(
            ids(&r.rank(Slot::Background, &refs)),
            [
                "codex:gpt-7-luna",
                "codex:gpt-6.1-luna",
                "opencode:openai/gpt-6.1-luna",
                "codex:gpt-6-luna"
            ]
        );
    }

    #[test]
    fn large_local_models_rank_for_strong_slot() {
        let r = Rankings::builtin();
        let models = [
            entry("opencode", "ollama/llama3.2:3b", true),
            entry("opencode", "ollama/qwen3:32b", true),
            entry("opencode", "ollama/llama3.3:70b", true),
        ];
        let refs: Vec<&ModelEntry> = models.iter().collect();
        assert_eq!(
            ids(&r.rank(Slot::ChatStrong, &refs)),
            [
                "opencode:ollama/llama3.3:70b",
                "opencode:ollama/qwen3:32b",
                "opencode:ollama/llama3.2:3b"
            ]
        );
    }

    #[test]
    fn flash_lite_is_not_flash() {
        let r = Rankings::builtin();
        let lite = entry("gemini", "flash-lite", false);
        let flash = entry("gemini", "flash", false);
        assert_eq!(r.family_of(&lite).unwrap().id, "gemini-flash-lite");
        assert_eq!(r.family_of(&flash).unwrap().id, "gemini-flash");
        let refs = [&lite, &flash];
        assert_eq!(
            ids(&r.rank(Slot::Chat, &refs)),
            ["gemini:flash", "gemini:flash-lite"]
        );
    }

    #[test]
    fn operator_file_overrides_slots_and_adds_families() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("r.toml");
        std::fs::write(
            &p,
            "[[family]]\nid = \"mine\"\nmodels = [\"my-*\"]\n[slots]\nchat = [\"mine\", \"gpt-luna\"]\n",
        )
        .unwrap();
        let r = Rankings::load(Some(&p)).unwrap();
        let a = entry("opencode", "lab/my-model", false);
        let b = entry("claude", "haiku", false);
        let c = entry("codex", "gpt-6-luna", false);
        assert_eq!(
            ids(&r.rank(Slot::Chat, &[&b, &c, &a])),
            ["opencode:lab/my-model", "codex:gpt-6-luna", "claude:haiku"]
        );
        // Other slots keep the built-in lists.
        assert_eq!(ids(&r.rank(Slot::Background, &[&c, &b]))[0], "claude:haiku");
    }

    #[test]
    fn default_efforts() {
        let mut m = entry("codex", "x", false);
        assert_eq!(default_effort(&m, None).as_deref(), Some("low"));
        m.efforts = vec!["minimal".into(), "high".into()];
        assert_eq!(default_effort(&m, None).as_deref(), Some("high"));
        m.efforts.clear();
        assert_eq!(default_effort(&m, None), None);
        // A family's starting effort, when the model offers it.
        let r = Rankings::builtin();
        let sonnet = entry("claude", "sonnet", false);
        let sol = entry("codex", "gpt-6.1-sol", false);
        let haiku = entry("claude", "haiku", false);
        for (m, want) in [(&sonnet, "medium"), (&sol, "low"), (&haiku, "low")] {
            assert_eq!(
                default_effort(m, r.family_of(m)).as_deref(),
                Some(want),
                "{}",
                m.id
            );
        }
        let mut no_medium = sonnet.clone();
        no_medium.efforts = vec!["low".into(), "high".into()];
        assert_eq!(
            default_effort(&no_medium, r.family_of(&no_medium)).as_deref(),
            Some("low")
        );
    }
}
