//! Engine setup from the app: install, update, and remove the official CLIs
//! (see `engines.rs` for sources and verification), a daily update of the
//! engines Iris installed, and the cards that walk the owner through
//! first-run setup in chat (set up an engine -> sign in -> done).

use crate::db;
use crate::engines::{self, ENGINES};
use crate::harness::AuthState;
use crate::state::AppState;
use crate::util::{Result, now_ms};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EngineOp {
    /// install | update | uninstall
    pub op: &'static str,
    /// running | done | failed
    pub state: &'static str,
    pub error: Option<String>,
    pub version: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Default)]
pub struct EngineOps {
    ops: Mutex<HashMap<String, EngineOp>>,
}

impl EngineOps {
    pub fn get(&self, name: &str) -> Option<EngineOp> {
        self.ops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .cloned()
    }

    fn set(&self, name: &str, op: EngineOp) {
        self.ops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.to_string(), op);
    }

    fn try_begin(&self, name: &str, op: &'static str) -> bool {
        let mut m = self.ops.lock().unwrap_or_else(|p| p.into_inner());
        if m.get(name).is_some_and(|o| o.state == "running") {
            return false;
        }
        m.insert(
            name.to_string(),
            EngineOp {
                op,
                state: "running",
                error: None,
                version: None,
                started_at: now_ms(),
                finished_at: None,
            },
        );
        true
    }
}

pub fn engines_root(state: &AppState) -> PathBuf {
    state.cfg.engines_dir(&state.paths)
}

/// Engines for the Settings screen.
pub fn engines_json(state: &AppState) -> Vec<Value> {
    let root = engines_root(state);
    let npm = crate::harness::resolve_command("npm").is_some();
    ENGINES
        .iter()
        .map(|e| {
            let status = state.harness(e.name).map(|h| h.status());
            let managed = engines::is_installed(&root, e.name);
            let installed = status.as_ref().is_some_and(|s| s.installed);
            let needs = match e.method {
                engines::Method::Npm { .. } if !npm => Some("nodejs"),
                _ => None,
            };
            json!({
                "name": e.name,
                "label": e.label,
                "source": e.source,
                "installed": installed,
                // Installed by Iris (updatable/removable from the app).
                "managed": managed,
                // Installed by the operator; Iris never modifies it.
                "external": installed && !managed,
                "version": status.as_ref().and_then(|s| s.version.clone()),
                "managed_version": engines::installed_version(&root, e.name),
                "auth": status.as_ref().map(|s| s.auth),
                "requires": needs,
                "operation": state.engine_ops.get(e.name),
            })
        })
        .collect()
}

/// Run an engine operation through the privileged helper.
async fn via_helper(state: &AppState, op: &str, name: &str) -> Result<engines::Report> {
    let helper = &state.cfg.engines.helper;
    let root = engines_root(state);
    let mut cmd = tokio::process::Command::new(&helper[0]);
    cmd.args(&helper[1..])
        .args([op, name, "--dir"])
        .arg(&root)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(30 * 60), cmd.output())
        .await
        .map_err(|_| "engine helper timed out")??;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .find(|l| l.starts_with("error:"))
            .or(err.lines().last())
            .unwrap_or("engine helper failed")
            .trim_start_matches("error:")
            .trim()
            .to_string();
        return Err(line.into());
    }
    if op == "uninstall" {
        return Ok(engines::Report {
            name: name.into(),
            version: None,
            path: None,
            changed: true,
        });
    }
    let last = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'));
    let v: Value = serde_json::from_str(last.unwrap_or("{}"))?;
    Ok(engines::Report {
        name: name.into(),
        version: v["version"].as_str().map(String::from),
        path: v["path"].as_str().map(String::from),
        changed: v["changed"].as_bool().unwrap_or(true),
    })
}

async fn execute(state: &AppState, op: &'static str, name: &str) -> Result<engines::Report> {
    if !state.cfg.engines.helper.is_empty() {
        return via_helper(state, op, name).await;
    }
    let root = engines_root(state);
    let n = name.to_string();
    tokio::task::spawn_blocking(move || match op {
        "uninstall" => engines::uninstall(&root, &n).map(|_| engines::Report {
            name: n.clone(),
            version: None,
            path: None,
            changed: true,
        }),
        _ => engines::install(&root, &n),
    })
    .await
    .map_err(|e| format!("engine task failed: {e}"))?
}

/// Start an install/update/uninstall in the background. Returns false if
/// one is already running for this engine.
pub fn start_op(state: &AppState, name: &str, op: &'static str) -> Result<bool> {
    engines::source(name).ok_or_else(|| format!("unknown engine {name}"))?;
    if op == "uninstall" && !engines::is_installed(&engines_root(state), name) {
        return Err("Iris only removes engines it installed".into());
    }
    if !state.engine_ops.try_begin(name, op) {
        return Ok(false);
    }
    let (st, n) = (state.clone(), name.to_string());
    tokio::spawn(async move {
        run_op(&st, &n, op).await;
    });
    Ok(true)
}

async fn run_op(state: &AppState, name: &str, op: &'static str) {
    broadcast(state);
    crate::info!("engine {name}: {op} started");
    let result = execute(state, op, name).await;
    let mut entry = state.engine_ops.get(name).unwrap_or(EngineOp {
        op,
        state: "running",
        error: None,
        version: None,
        started_at: now_ms(),
        finished_at: None,
    });
    entry.finished_at = Some(now_ms());
    match &result {
        Ok(r) => {
            crate::info!(
                "engine {name}: {op} done ({})",
                r.version.as_deref().unwrap_or("-")
            );
            entry.state = "done";
            entry.version = r.version.clone();
        }
        Err(e) => {
            crate::warn!("engine {name}: {op} failed: {e}");
            entry.state = "failed";
            entry.error = Some(crate::util::truncate_chars(&e.to_string(), 300));
        }
    }
    state.engine_ops.set(name, entry);
    if result.is_ok() {
        let auth = state.recheck_auth(name).await;
        if op == "install" && auth != AuthState::Ok {
            crate::login::post_signin_card(state, name, true).await;
        }
        update_setup_card(state).await;
        state.check_recommendations().await;
    }
    broadcast(state);
}

fn broadcast(state: &AppState) {
    state.hub.send(
        crate::events::Audience::All,
        json!({"type": "engines.changed", "engines": engines_json(state)}),
    );
}

/// Daily: update the engines Iris installed (never anyone else's), when
/// no turn is running. Checked each scheduler minute.
pub async fn maybe_auto_update(state: &AppState) {
    if !state.cfg.engines.auto_update {
        return;
    }
    let root = engines_root(state);
    if !ENGINES.iter().any(|e| engines::is_installed(&root, e.name)) {
        return;
    }
    let last = state
        .setting("engines.last_update")
        .await
        .and_then(|s| s.parse::<i64>().ok());
    if last.is_some_and(|l| now_ms() - l < DAY_MS) {
        return;
    }
    let busy = !state
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_empty();
    if busy {
        return;
    }
    let now = now_ms().to_string();
    let _ = state
        .db
        .call(move |c| db::set_setting(c, "engines.last_update", &now))
        .await;
    for e in ENGINES {
        if engines::is_installed(&root, e.name) && state.engine_ops.try_begin(e.name, "update") {
            run_op(state, e.name, "update").await;
        }
    }
}

/// First run: no engine can answer yet. Post one "Set up an AI engine" card
/// in the owner's Main chat (install options, then sign-in cards follow).
pub async fn post_setup_card_if_needed(state: &AppState) {
    if !state.is_serving() || state.harnesses.values().any(|h| h.usable()) {
        return;
    }
    let options = setup_options(state);
    let card = json!({
        "type": "engine_setup",
        "title": "Set up an AI engine",
        "body": "I need at least one AI engine to think with. Pick one to install on your \
                 server; I'll ask you to sign in right after. Claude needs a Claude plan, Codex a \
                 ChatGPT plan, Gemini a Google account; OpenCode works with many providers, \
                 including free and local models.",
        "options": options,
        "state": "needs_setup",
        "owner_only": true,
    });
    let posted = state
        .db
        .call(move |c| {
            if db::get_setting(c, "setup_card")?.is_some() {
                return Ok(None);
            }
            let Some(owner) = db::owner_user_id(c)? else {
                return Ok(None);
            };
            let main = db::ensure_main_conversation(c, &owner)?;
            let m = crate::entities::insert_card(c, &main, &card, "assistant")?;
            db::set_setting(c, "setup_card", &m.id)?;
            Ok(Some((owner, m)))
        })
        .await
        .ok()
        .flatten();
    if let Some((owner, m)) = posted {
        state.hub.to_users(
            std::slice::from_ref(&owner),
            json!({"type": "message.created", "conversation_id": m.conversation_id, "message": m}),
        );
    }
}

fn setup_options(state: &AppState) -> Vec<Value> {
    engines_json(state)
        .into_iter()
        .map(|e| {
            let name = e["name"].as_str().unwrap_or("").to_string();
            json!({
                "harness": name,
                "label": e["label"],
                "installed": e["installed"],
                "requires": e["requires"],
                "action": {"method": "POST", "path": format!("/api/engines/{name}/install")},
            })
        })
        .collect()
}

/// Refresh the setup card (installed flags; done once an engine works).
pub async fn update_setup_card(state: &AppState) {
    let options = setup_options(state);
    let done = state
        .harnesses
        .values()
        .any(|h| h.status().auth == AuthState::Ok);
    let updated = state
        .db
        .call(move |c| {
            let Some(id) = db::get_setting(c, "setup_card")? else {
                return Ok(None);
            };
            let Some(m) = db::get_message(c, &id)? else {
                return Ok(None);
            };
            let mut card = m.card.clone().unwrap_or(Value::Null);
            card["options"] = Value::Array(options);
            if done {
                card["state"] = "done".into();
                card["body"] =
                    "You're all set. You can add more engines any time in Settings.".into();
            }
            db::update_message_card(c, &id, &card).map(Some)
        })
        .await
        .ok()
        .flatten();
    if let Some(m) = updated {
        let conv = m.conversation_id.clone();
        let members = state
            .db
            .call(move |c| db::conversation_member_ids(c, &conv))
            .await
            .unwrap_or_default();
        state.hub.to_users(
            &members,
            json!({"type": "message.updated", "conversation_id": m.conversation_id, "message": m}),
        );
    }
}
