//! Engine selection. End users never see models: each turn uses a slot
//! (`chat`, `chat_strong` for clearly hard requests, `background`, `feed`),
//! the slot's pinned model or highest-ranked available model, and the
//! classifier's effort. The decision (model, effort, slot, signals, and
//! why) is stored in the assistant message metadata only.
//!
//! Policy (individual use): Claude subscriptions are for one person, so
//! conversations with more than one human never use Claude unless the owner
//! opts in (`router.allow_claude_for_shared` or the app setting). The
//! owner's own scheduled jobs are ordinary individual use. Harnesses that
//! are disabled, missing, or signed out are skipped.

use crate::catalog::{Catalog, ModelEntry, parse_model_ref};
use crate::classifier::{Assessment, Effort};
use crate::ranking::{Pin, Rankings, Slot, default_effort};
use serde::Serialize;
use std::collections::BTreeMap;

/// Why a turn is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// A human sent a message.
    Chat,
    /// Heartbeat, feed generation, or another scheduled job.
    Scheduled,
}

/// Individual-use policy: may this harness answer here?
pub fn policy_allows(harness: &str, shared: bool, allow_claude_for_shared: bool) -> bool {
    harness != "claude" || !shared || allow_claude_for_shared
}

pub struct RouteInput<'a> {
    /// Base slot: `Chat` for conversations, `Background`/`Feed` for jobs.
    pub slot: Slot,
    pub has_images: bool,
    /// More than one human in the conversation.
    pub shared: bool,
    pub assessment: &'a Assessment,
}

/// What the router can see about this machine.
pub struct EngineEnv<'a> {
    pub rankings: &'a Rankings,
    pub catalog: &'a Catalog,
    pub pins: &'a BTreeMap<Slot, Pin>,
    /// The model's CLI can run a turn now (and has not failed this turn).
    pub available: &'a dyn Fn(&ModelEntry) -> bool,
    pub allow_claude_for_shared: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteDecision {
    /// "automatic", "pinned", or "fallback" (pin not usable here).
    pub router: &'static str,
    /// Catalog model id, e.g. "claude:haiku".
    pub model: String,
    /// Reasoning effort, or None when the model has no effort setting.
    pub effort: Option<String>,
    pub slot: Slot,
    /// Moved from `chat` to `chat_strong` for a clearly hard request.
    pub escalated: bool,
    /// Classifier score and signals.
    pub score: i32,
    pub signals: Vec<&'static str>,
    /// Human-readable explanation (metadata only).
    pub reason: String,
}

impl RouteDecision {
    pub fn harness(&self) -> &str {
        parse_model_ref(&self.model).map_or("", |r| r.harness)
    }
    pub fn cli_model(&self) -> &str {
        parse_model_ref(&self.model).map_or("", |r| r.model)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoEngine {
    /// Models are available, but policy rules them all out (Claude is the
    /// only engine and this conversation has more than one person).
    PausedPolicy,
    /// Nothing can run.
    NoModel,
}

/// The effort to pass: the wanted level clamped to what the model offers,
/// or the model's default when it offers nothing at or below it.
pub fn choose_effort(catalog: &Catalog, m: &ModelEntry, wanted: Effort) -> Option<String> {
    if m.efforts.is_empty() {
        return None;
    }
    catalog
        .clamp_effort(&m.id, wanted.as_str())
        .or_else(|| default_effort(m))
}

/// Pick the engine for one turn.
pub fn select_engine(input: &RouteInput, env: &EngineEnv) -> Result<RouteDecision, NoEngine> {
    let a = input.assessment;
    let allowed =
        |m: &ModelEntry| policy_allows(&m.harness, input.shared, env.allow_claude_for_shared);
    let usable = |m: &ModelEntry| (env.available)(m) && allowed(m);
    let sees_images = |m: &ModelEntry| m.images != Some(false);
    let slots: Vec<Slot> = if input.slot == Slot::Chat && a.escalate {
        vec![Slot::ChatStrong, Slot::Chat]
    } else {
        vec![input.slot]
    };
    let mut notes: Vec<String> = Vec::new();
    let mut router = "automatic";
    for (i, &slot) in slots.iter().enumerate() {
        if let Some(pin) = env.pins.get(&slot) {
            match env.catalog.get(&pin.model) {
                Some(m) if usable(m) => {
                    let effort = pin
                        .effort
                        .as_deref()
                        .and_then(|e| env.catalog.clamp_effort(&m.id, e))
                        .or_else(|| choose_effort(env.catalog, m, a.effort));
                    notes.insert(0, format!("{} pinned for {}", m.id, slot.name()));
                    return Ok(decision("pinned", m, effort, slot, i, input, notes));
                }
                found => {
                    router = "fallback";
                    let why = match found {
                        None => "unknown model",
                        Some(m) if !allowed(m) => "individual-use policy",
                        Some(_) => "engine unavailable",
                    };
                    notes.push(format!("pinned {} not usable ({why})", pin.model));
                }
            }
        }
        let available: Vec<&ModelEntry> = env.catalog.models.iter().filter(|m| usable(m)).collect();
        let ranked = env.rankings.rank(slot, &available);
        let pick = if input.has_images {
            ranked
                .iter()
                .find(|r| sees_images(r.entry))
                .or(ranked.first())
        } else {
            ranked.first()
        };
        if let Some(r) = pick {
            let m = r.entry;
            // Say when the slot's favorite is down (e.g. signed out).
            let all: Vec<&ModelEntry> = env.catalog.models.iter().filter(|m| allowed(m)).collect();
            if let Some(top) = env.rankings.rank(slot, &all).first()
                && top.entry.id != m.id
                && !(env.available)(top.entry)
            {
                notes.push(format!("{} unavailable, using {}", top.entry.id, m.id));
            }
            if i > 0 {
                notes.push(format!("no {} model available", slots[0].name()));
            }
            let effort = choose_effort(env.catalog, m, a.effort);
            return Ok(decision(router, m, effort, slot, i, input, notes));
        }
    }
    let anything = env.catalog.models.iter().any(|m| (env.available)(m));
    Err(if anything {
        NoEngine::PausedPolicy
    } else {
        NoEngine::NoModel
    })
}

fn decision(
    router: &'static str,
    m: &ModelEntry,
    effort: Option<String>,
    slot: Slot,
    slot_index: usize,
    input: &RouteInput,
    notes: Vec<String>,
) -> RouteDecision {
    let a = input.assessment;
    let mut reason = a.reason();
    if input.has_images {
        reason.push_str("; includes image(s)");
    }
    for n in notes {
        reason.push_str("; ");
        reason.push_str(&n);
    }
    RouteDecision {
        router,
        model: m.id.clone(),
        effort,
        slot,
        escalated: input.slot == Slot::Chat && slot == Slot::ChatStrong && slot_index == 0,
        score: a.score,
        signals: a.signals.clone(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::{Thresholds, assess};
    use crate::config::Config;
    use crate::harness::DiscoveredModel;

    fn dm(model: &str, local: bool, images: Option<bool>, efforts: &[&str]) -> DiscoveredModel {
        DiscoveredModel {
            model: model.into(),
            local,
            images,
            efforts: efforts.iter().map(|s| s.to_string()).collect(),
            ..DiscoveredModel::default()
        }
    }

    /// A machine with only some CLIs installed (and signed in).
    struct Machine {
        catalog: Catalog,
        installed: Vec<&'static str>,
        pins: BTreeMap<Slot, Pin>,
        allow_shared: bool,
    }

    impl Machine {
        fn new(installed: &[&'static str], opencode: Vec<DiscoveredModel>, codex: &[&str]) -> Self {
            let mut found = BTreeMap::new();
            found.insert("opencode".to_string(), opencode);
            found.insert(
                "codex".to_string(),
                codex.iter().map(|m| dm(m, false, None, &[])).collect(),
            );
            Self {
                catalog: Catalog::build(&Config::default(), &found),
                installed: installed.to_vec(),
                pins: BTreeMap::new(),
                allow_shared: false,
            }
        }

        fn route(
            &self,
            slot: Slot,
            text: &str,
            shared: bool,
            images: bool,
        ) -> Result<RouteDecision, NoEngine> {
            let rankings = Rankings::builtin();
            let a = if slot == Slot::Chat {
                assess(text, &[], &Thresholds::default())
            } else {
                assess("", &[], &Thresholds::default())
            };
            let installed = self.installed.clone();
            let available = move |m: &ModelEntry| installed.contains(&m.harness.as_str());
            let env = EngineEnv {
                rankings: &rankings,
                catalog: &self.catalog,
                pins: &self.pins,
                available: &available,
                allow_claude_for_shared: self.allow_shared,
            };
            select_engine(
                &RouteInput {
                    slot,
                    has_images: images,
                    shared,
                    assessment: &a,
                },
                &env,
            )
        }

        fn model(&self, slot: Slot, text: &str) -> String {
            self.route(slot, text, false, false).unwrap().model
        }
    }

    const HARD: &str = "Research heat pumps vs furnaces in depth, compare 10-year costs, and \
                        think carefully about the trade-offs step by step.";

    fn local_models() -> Vec<DiscoveredModel> {
        vec![
            dm("ollama/llama3.2:3b", true, Some(false), &[]),
            dm(
                "ollama/qwen3:32b",
                true,
                Some(false),
                &["low", "medium", "high"],
            ),
            dm("ollama/gemma3:12b", true, Some(true), &[]),
        ]
    }

    #[test]
    fn everyone_installed_follows_rankings() {
        let m = Machine::new(
            &["claude", "codex", "gemini", "opencode"],
            local_models(),
            &[],
        );
        let d = m
            .route(Slot::Chat, "what's a good name for a cat?", false, false)
            .unwrap();
        assert_eq!(
            (d.model.as_str(), d.effort.as_deref()),
            ("claude:haiku", Some("low"))
        );
        assert_eq!(
            (d.slot, d.router, d.escalated),
            (Slot::Chat, "automatic", false)
        );
        assert_eq!((d.harness(), d.cli_model()), ("claude", "haiku"));
        let d = m
            .route(Slot::Chat, "Help me debug my build", false, false)
            .unwrap();
        assert_eq!(
            (d.model.as_str(), d.effort.as_deref()),
            ("claude:haiku", Some("medium"))
        );
        let d = m.route(Slot::Chat, HARD, false, false).unwrap();
        assert_eq!(
            (d.model.as_str(), d.slot, d.escalated),
            ("claude:sonnet", Slot::ChatStrong, true)
        );
        assert_eq!(d.effort.as_deref(), Some("high"));
        // Background and feed may use Claude: it is the owner's own use.
        assert_eq!(m.model(Slot::Background, ""), "claude:haiku");
        assert_eq!(m.model(Slot::Feed, ""), "claude:haiku");
    }

    #[test]
    fn codex_only_machine() {
        let m = Machine::new(&["codex"], vec![], &[]);
        assert_eq!(m.model(Slot::Chat, "hi"), "codex:gpt-6-luna");
        assert_eq!(m.model(Slot::Chat, HARD), "codex:gpt-6.1-sol");
        assert_eq!(m.model(Slot::Background, ""), "codex:gpt-6-luna");
        assert_eq!(m.model(Slot::Feed, ""), "codex:gpt-6-luna");
        let d = m.route(Slot::Chat, "hi", false, false).unwrap();
        assert!(
            d.reason.contains("claude:haiku unavailable"),
            "{}",
            d.reason
        );
        // Shared chats work without Claude.
        assert_eq!(
            m.route(Slot::Chat, "hi all", true, false).unwrap().model,
            "codex:gpt-6-luna"
        );
    }

    #[test]
    fn opencode_only_local_machine() {
        let m = Machine::new(&["opencode"], local_models(), &[]);
        let d = m.route(Slot::Chat, "hi", false, false).unwrap();
        // Newest local model first; no effort for models without variants.
        assert_eq!(d.model, "opencode:ollama/qwen3:32b");
        assert_eq!(d.effort.as_deref(), Some("low"));
        assert_eq!(m.model(Slot::Chat, HARD), "opencode:ollama/qwen3:32b");
        assert_eq!(m.model(Slot::Background, ""), "opencode:ollama/qwen3:32b");
        assert_eq!(m.model(Slot::Feed, ""), "opencode:ollama/qwen3:32b");
        // Shared chats work: no Claude involved.
        assert!(m.route(Slot::Chat, "hi all", true, false).is_ok());
        // Images go to the local model that can see them.
        let d = m.route(Slot::Chat, "what is this?", false, true).unwrap();
        assert_eq!(d.model, "opencode:ollama/gemma3:12b");
        assert_eq!(d.effort, None);
    }

    #[test]
    fn claude_only_machine_pauses_only_shared_chats() {
        let mut m = Machine::new(&["claude"], vec![], &[]);
        assert_eq!(m.model(Slot::Chat, "hi"), "claude:haiku");
        assert_eq!(m.model(Slot::Chat, HARD), "claude:sonnet");
        assert_eq!(m.model(Slot::Background, ""), "claude:haiku");
        assert_eq!(m.model(Slot::Feed, ""), "claude:haiku");
        assert_eq!(
            m.route(Slot::Chat, "hi all", true, false),
            Err(NoEngine::PausedPolicy)
        );
        m.allow_shared = true;
        assert_eq!(
            m.route(Slot::Chat, "hi all", true, false).unwrap().model,
            "claude:haiku"
        );
    }

    #[test]
    fn gemini_only_machine() {
        let m = Machine::new(&["gemini"], vec![], &[]);
        let d = m.route(Slot::Chat, "hi", false, false).unwrap();
        assert_eq!((d.model.as_str(), d.effort), ("gemini:flash", None));
        assert_eq!(m.model(Slot::Chat, HARD), "gemini:pro");
        assert_eq!(m.model(Slot::Background, ""), "gemini:flash");
        assert_eq!(m.model(Slot::Feed, ""), "gemini:flash");
        assert!(m.route(Slot::Chat, "hi all", true, false).is_ok());
    }

    #[test]
    fn nothing_installed() {
        let m = Machine::new(&[], vec![], &[]);
        assert_eq!(
            m.route(Slot::Chat, "hi", false, false),
            Err(NoEngine::NoModel)
        );
    }

    #[test]
    fn installing_a_higher_ranked_model_changes_the_choice() {
        let m = Machine::new(&["opencode"], local_models(), &[]);
        assert_eq!(m.model(Slot::Chat, "hi"), "opencode:ollama/qwen3:32b");
        // Codex appears and lists a newer Luna: it takes over automatically.
        let m = Machine::new(&["opencode", "codex"], local_models(), &["gpt-7-luna"]);
        assert_eq!(m.model(Slot::Chat, "hi"), "codex:gpt-7-luna");
        assert_eq!(m.model(Slot::Background, ""), "codex:gpt-7-luna");
        // A newer local model family version outranks the older one.
        let mut models = local_models();
        models.push(dm("ollama/qwen4:8b", true, None, &[]));
        let m = Machine::new(&["opencode"], models, &[]);
        assert_eq!(m.model(Slot::Chat, "hi"), "opencode:ollama/qwen4:8b");
    }

    #[test]
    fn pins_are_honored_and_fall_back_when_unusable() {
        let mut m = Machine::new(&["claude", "codex"], vec![], &[]);
        m.pins.insert(
            Slot::Chat,
            Pin {
                model: "codex:gpt-6.1-sol".into(),
                effort: Some("high".into()),
            },
        );
        let d = m.route(Slot::Chat, "hi", false, false).unwrap();
        assert_eq!(
            (d.router, d.model.as_str(), d.effort.as_deref()),
            ("pinned", "codex:gpt-6.1-sol", Some("high"))
        );
        // Pinned Claude in a shared chat: policy wins, recorded as fallback.
        m.pins.insert(
            Slot::Chat,
            Pin {
                model: "claude:sonnet".into(),
                effort: None,
            },
        );
        let d = m.route(Slot::Chat, "hi", true, false).unwrap();
        assert_eq!((d.router, d.harness()), ("fallback", "codex"));
        assert!(d.reason.contains("individual-use policy"), "{}", d.reason);
        // Pin on a CLI that is not installed.
        m.installed = vec!["claude"];
        m.pins.insert(
            Slot::Chat,
            Pin {
                model: "codex:gpt-6-luna".into(),
                effort: None,
            },
        );
        let d = m.route(Slot::Chat, "hi", false, false).unwrap();
        assert_eq!((d.router, d.model.as_str()), ("fallback", "claude:haiku"));
        assert!(d.reason.contains("engine unavailable"));
    }

    #[test]
    fn effort_is_clamped_to_model_support() {
        let m = Machine::new(
            &["opencode"],
            vec![dm("lab/only-high", false, None, &["high", "max"])],
            &[],
        );
        let d = m.route(Slot::Chat, "hi", false, false).unwrap();
        assert_eq!(d.effort.as_deref(), Some("high"));
        let cat = Catalog::build(&Config::default(), &BTreeMap::new());
        let luna = cat.get("codex:gpt-6-luna").unwrap();
        assert_eq!(
            choose_effort(&cat, luna, Effort::High).as_deref(),
            Some("high")
        );
    }
}
