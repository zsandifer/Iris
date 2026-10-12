//! API keys for engines, added from the app: the way to sign in to Gemini
//! CLI and OpenCode providers without a terminal, and an optional
//! alternative to the Claude or ChatGPT subscription for Claude Code and
//! Codex. Keys are sealed with the vault, never returned or logged, and
//! passed to the engine's own CLI through its documented environment
//! variable for each turn (after `strip_api_key_env` removes inherited
//! ones). Subscription sign-ins (OAuth) are never touched: those stay with
//! each CLI's own login.

use crate::entities;
use crate::state::AppState;
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct KeyOption {
    /// Environment variable the CLI reads.
    pub env: &'static str,
    /// Provider name shown in the app.
    pub label: &'static str,
    /// Where the owner creates a key.
    pub url: &'static str,
    /// OpenCode provider id the key belongs to (model ids `provider/...`).
    pub provider: &'static str,
}

const fn k(env: &'static str, label: &'static str, url: &'static str) -> KeyOption {
    KeyOption {
        env,
        label,
        url,
        provider: "",
    }
}

const fn p(
    env: &'static str,
    label: &'static str,
    url: &'static str,
    provider: &'static str,
) -> KeyOption {
    KeyOption {
        env,
        label,
        url,
        provider,
    }
}

const CLAUDE: &[KeyOption] = &[k(
    "ANTHROPIC_API_KEY",
    "Anthropic",
    "https://console.anthropic.com/settings/keys",
)];
const CODEX: &[KeyOption] = &[k(
    "CODEX_API_KEY",
    "OpenAI",
    "https://platform.openai.com/api-keys",
)];
const GEMINI: &[KeyOption] = &[k(
    "GEMINI_API_KEY",
    "Google AI Studio",
    "https://aistudio.google.com/apikey",
)];
const OPENCODE: &[KeyOption] = &[
    p(
        "OPENROUTER_API_KEY",
        "OpenRouter",
        "https://openrouter.ai/keys",
        "openrouter",
    ),
    p(
        "ANTHROPIC_API_KEY",
        "Anthropic",
        "https://console.anthropic.com/settings/keys",
        "anthropic",
    ),
    p(
        "OPENAI_API_KEY",
        "OpenAI",
        "https://platform.openai.com/api-keys",
        "openai",
    ),
    p(
        "GOOGLE_GENERATIVE_AI_API_KEY",
        "Google",
        "https://aistudio.google.com/apikey",
        "google",
    ),
    p(
        "GROQ_API_KEY",
        "Groq",
        "https://console.groq.com/keys",
        "groq",
    ),
    p(
        "DEEPSEEK_API_KEY",
        "DeepSeek",
        "https://platform.deepseek.com/api_keys",
        "deepseek",
    ),
    p(
        "MISTRAL_API_KEY",
        "Mistral",
        "https://console.mistral.ai/api-keys",
        "mistral",
    ),
    p("XAI_API_KEY", "xAI", "https://console.x.ai", "xai"),
    p(
        "OPENCODE_API_KEY",
        "OpenCode Zen",
        "https://opencode.ai/auth",
        "opencode",
    ),
];

/// Keys each engine accepts. Claude Code reads `ANTHROPIC_API_KEY` in
/// headless runs; `codex exec` reads `CODEX_API_KEY`; Gemini CLI reads
/// `GEMINI_API_KEY`; OpenCode reads each provider's variable.
pub fn options(harness: &str) -> &'static [KeyOption] {
    match harness {
        "claude" => CLAUDE,
        "codex" => CODEX,
        "gemini" => GEMINI,
        "opencode" => OPENCODE,
        _ => &[],
    }
}

fn aad(harness: &str, env: &str) -> String {
    format!("engine_key:{harness}:{env}")
}

pub fn option(harness: &str, env: &str) -> Option<KeyOption> {
    options(harness).iter().copied().find(|o| o.env == env)
}

/// Save (or replace) a key.
/// The key as it will be saved, or why it can't be.
pub fn check(harness: &str, env: &str, value: &str) -> Result<String, String> {
    let value = value.trim();
    if option(harness, env).is_none() {
        return Err(format!("{env} is not a key this engine uses"));
    }
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_whitespace) {
        return Err("paste the key exactly as shown (one line, no spaces)".into());
    }
    Ok(value.to_string())
}

pub async fn set(state: &AppState, harness: &str, env: &str, value: &str) -> Result<(), String> {
    let value = check(harness, env, value)?;
    let value = value.as_str();
    let sealed = state
        .vault
        .seal_str(value, &aad(harness, env))
        .map_err(|e| e.to_string())?;
    let (h, e) = (harness.to_string(), env.to_string());
    state
        .db
        .call(move |c| entities::set_engine_key(c, &h, &e, &sealed))
        .await
        .map_err(|e| e.to_string())?;
    crate::info!("engine {harness}: API key saved");
    Ok(())
}

pub async fn delete(state: &AppState, harness: &str, env: &str) -> bool {
    let (h, e) = (harness.to_string(), env.to_string());
    state
        .db
        .call(move |c| entities::delete_engine_key(c, &h, &e))
        .await
        .unwrap_or(false)
}

/// Saved keys (names only) for the app.
pub async fn list_json(state: &AppState, harness: &str) -> Value {
    let h = harness.to_string();
    let saved = state
        .db
        .call(move |c| entities::engine_keys(c, &h))
        .await
        .unwrap_or_default();
    let keys: Vec<Value> = saved
        .iter()
        .map(|(env, _, at)| {
            json!({
                "env": env,
                "label": option(harness, env).map(|o| o.label).unwrap_or(env.as_str()),
                "saved_at": at,
            })
        })
        .collect();
    json!({"keys": keys, "key_options": options(harness)})
}

/// Decrypted keys to pass to the engine for one run.
pub async fn env_for(state: &AppState, harness: &str) -> Vec<(String, String)> {
    let h = harness.to_string();
    let saved = state
        .db
        .call(move |c| entities::engine_keys(c, &h))
        .await
        .unwrap_or_default();
    saved
        .into_iter()
        .filter(|(env, _, _)| option(harness, env).is_some())
        .filter_map(|(env, sealed, _)| {
            state
                .vault
                .open_str(&sealed, &aad(harness, &env))
                .ok()
                .map(|v| (env, v))
        })
        .collect()
}

/// Check a candidate key (not saved yet) the only reliable way: one tiny
/// real request through the engine's own CLI (a one-word reply, no tools,
/// no session), with the candidate in place of any saved key for `env`.
pub async fn verify(
    state: &AppState,
    harness: &str,
    env: &str,
    candidate: &str,
) -> crate::harness::AuthState {
    use crate::harness::runner::{self, RunRequest};
    use crate::harness::{AuthState, HarnessEvent, TurnSpec};
    let Some(entry) = state.harness(harness) else {
        return AuthState::Unknown;
    };
    let Some(program) = entry.program() else {
        return AuthState::Unknown;
    };
    let mut keys: Vec<(String, String)> = env_for(state, harness)
        .await
        .into_iter()
        .filter(|(k, _)| k != env)
        .collect();
    keys.push((env.to_string(), candidate.to_string()));
    let strip = state.strip_env(entry);
    let provider = option(harness, env).map(|o| o.provider).unwrap_or("");
    let model = if provider.is_empty() {
        // The engine's own model the background slot ranks first (quick,
        // cheap).
        let catalog = state.catalog();
        let mine: Vec<&crate::catalog::ModelEntry> = catalog
            .models
            .iter()
            .filter(|m| m.harness == harness)
            .collect();
        state
            .rankings
            .rank(crate::ranking::Slot::Background, &mine)
            .first()
            .map(|r| r.entry.model.clone())
    } else {
        // OpenCode lists a provider's models once its key is present: a
        // model of the provider this key is for.
        let args = entry.harness.models_probe_args().unwrap_or_default();
        match state
            .agent_probe(entry, &program, &args, &strip, &keys)
            .await
        {
            Ok((true, out, _)) => entry
                .harness
                .parse_models(&out)
                .into_iter()
                .map(|m| m.model)
                .find(|m| m.starts_with(&format!("{provider}/"))),
            _ => None,
        }
    };
    let Some(model) = model else {
        return AuthState::Unknown;
    };
    let spec = TurnSpec {
        turn_id: crate::util::new_id(),
        prompt: "Reply with the single word OK.".into(),
        images: Vec::new(),
        system_prompt: "Reply with one word.".into(),
        model,
        effort: None,
        resume: None,
        mcp_servers: Vec::new(),
        cwd: match crate::agent::workdir(&state.cfg.agent, &state.paths, "keycheck") {
            Ok(p) => p,
            Err(_) => return AuthState::Unknown,
        },
        writable_dirs: Vec::new(),
        run_dir: match crate::agent::turn_dir(
            &state.cfg.agent,
            &state.paths,
            &crate::util::new_id(),
        ) {
            Ok(p) => p,
            Err(_) => return AuthState::Unknown,
        },
        scope: "keycheck".into(),
        key_env: keys.iter().map(|(k, _)| k.clone()).collect(),
        // A one-word check needs none of the CLI's own tools or memory.
        isolated: true,
        protected_paths: match state.protected_paths() {
            Ok(p) => p,
            Err(_) => return AuthState::Unknown,
        },
        split: !state.cfg.agent.user.is_empty(),
        own_mcp_servers: match crate::turn::own_mcp_servers(state, entry, &program).await {
            Some(names) => names,
            None => return AuthState::Unknown,
        },
    };
    let _turn_files = crate::agent::TurnFiles::new(&state.cfg.agent, "keycheck", &spec.run_dir);
    let Ok(mut invocation) = entry.harness.build(&spec) else {
        return AuthState::Unknown;
    };
    invocation.env.extend(keys);
    if crate::agent::expose_files(&state.cfg.agent, &invocation)
        .and_then(|_| crate::agent::wrap(&state.cfg.agent, harness, "keycheck", &mut invocation))
        .is_err()
    {
        return AuthState::Unknown;
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let run = runner::run(
        RunRequest {
            program: &program,
            invocation: &invocation,
            cwd: &spec.cwd,
            env_remove: &strip,
            timeout: std::time::Duration::from_secs(90),
        },
        entry.harness.parser(&[]),
        cancel_rx,
        tx,
    )
    .await;

    if spec.split {
        let _ = std::fs::remove_dir_all(&spec.run_dir);
    }
    for f in &invocation.temp_files {
        let _ = std::fs::remove_file(f);
    }
    let mut result = AuthState::Unknown;
    if run.is_ok() {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                HarnessEvent::Done => result = AuthState::Ok,
                HarnessEvent::AuthRequired { .. } => result = AuthState::AuthRequired,
                _ => {}
            }
        }
    }
    crate::info!("engine {harness}: key check {result:?}");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_per_engine() {
        assert_eq!(options("gemini")[0].env, "GEMINI_API_KEY");
        assert_eq!(options("codex")[0].env, "CODEX_API_KEY");
        assert!(
            options("opencode")
                .iter()
                .any(|o| o.env == "OPENROUTER_API_KEY")
        );
        assert!(option("gemini", "OPENAI_API_KEY").is_none());
        assert!(options("nope").is_empty());
    }
}
