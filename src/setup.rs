//! Engine setup from the app: install, update, and remove the official CLIs
//! (see `engines.rs` for sources and verification), a daily update of the
//! engines Iris installed, and the cards that walk the owner through
//! first-run setup in chat (set up an engine -> sign in -> done).

use crate::agent::{LauncherCheck, LauncherState};
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
    let launcher_outdated = launcher_outdated(state);
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
                "isolation": if state.cfg.agent.user.is_empty() { "single_user" } else { "split" },
                "agent_user": status.as_ref().and_then(|s| s.agent_user.clone()),
                "signed_in_as_agent": if state.cfg.agent.user.is_empty() { None } else { status.as_ref().and_then(|s| match s.auth { AuthState::Unknown => None, other => Some(other == AuthState::Ok) }) },
                "launcher_outdated": launcher_outdated,
                "requires": needs,
                "operation": state.engine_ops.get(e.name),
            })
        })
        .collect()
}

/// The last launcher check, for `GET /api/engines` (null in single-user
/// mode or before the first check).
pub fn launcher_json(state: &AppState) -> Value {
    json!(*state.launcher.lock().unwrap_or_else(|p| p.into_inner()))
}

/// True when the root-owned launcher is known to be out of date; null in
/// single-user mode or when it cannot be told.
fn launcher_outdated(state: &AppState) -> Option<bool> {
    match state
        .launcher
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
    {
        Some(c) if c.state != LauncherState::Unknown => Some(c.state == LauncherState::Outdated),
        _ => None,
    }
}

const LAUNCHER_CARD: &str = "launcher_card";
const LAUNCHER_CARD_KEY: &str = "launcher_card.key";

/// Compare the root-owned launcher's build with this service's (split
/// mode only; at startup and when a turn was refused by the launcher). An
/// outdated launcher gets one card in the owner's Main chat with the fix;
/// once a later check finds it current, that card says so.
pub async fn check_launcher(state: &AppState) -> Option<LauncherCheck> {
    let check = crate::agent::check_launcher(&state.cfg.agent).await?;
    apply_launcher_check(state, &check).await;
    Some(check)
}

async fn apply_launcher_check(state: &AppState, check: &LauncherCheck) {
    let changed = {
        let mut last = state.launcher.lock().unwrap_or_else(|p| p.into_inner());
        let changed = last.as_ref() != Some(check);
        *last = Some(check.clone());
        changed
    };
    match check.state {
        LauncherState::Outdated => {
            if changed {
                crate::warn!(
                    "agent launcher {} is from build {}, this service is build {}; reinstall the launcher",
                    state.cfg.agent.launcher,
                    check.build_id.as_deref().unwrap_or("(unreported)"),
                    check.service_build_id
                );
            }
            post_launcher_card(state, check).await;
        }
        LauncherState::Current => resolve_launcher_card(state).await,
        LauncherState::Unknown => {}
    }
    if changed {
        broadcast(state);
    }
}

/// The binary to install as the launcher: the one this service runs.
fn service_binary(state: &AppState) -> String {
    let exe = state.exe.to_string_lossy();
    exe.strip_suffix(" (deleted)").unwrap_or(&exe).to_string()
}

fn launcher_card(state: &AppState, check: &LauncherCheck) -> Value {
    let launcher = &state.cfg.agent.launcher;
    let why = if check.build_id.is_some() {
        format!(
            "My agent launcher ({launcher}) is from a different build of Iris than the one \
             running now"
        )
    } else {
        format!(
            "My agent launcher ({launcher}) looks like it's from an older version of Iris \
             (it doesn't report its build)"
        )
    };
    json!({
        "type": "info",
        "title": "Update the agent launcher",
        "body": format!(
            "{why}, so it may refuse my requests and I can't reply until it matches. To fix \
             it, run this as root on the server, then restart Iris: install -Dm755 {} {launcher}",
            service_binary(state)
        ),
        "state": "pending",
        "owner_only": true,
    })
}

async fn post_launcher_card(state: &AppState, check: &LauncherCheck) {
    let key = format!(
        "{}:{}",
        check.service_build_id,
        check.build_id.as_deref().unwrap_or("none")
    );
    let card = launcher_card(state, check);
    let posted = state
        .db
        .call(move |c| {
            if db::get_setting(c, LAUNCHER_CARD_KEY)?.as_deref() == Some(key.as_str()) {
                return Ok(None);
            }
            let Some(owner) = db::owner_user_id(c)? else {
                return Ok(None);
            };
            // A card still waiting (another build pair) is rewritten, not repeated.
            let open = match db::get_setting(c, LAUNCHER_CARD)? {
                Some(id) => db::get_message(c, &id)?
                    .filter(|m| m.card.as_ref().is_some_and(|c| c["state"] == "pending")),
                None => None,
            };
            let (m, created) = match open {
                Some(m) => (db::update_message_card(c, &m.id, &card)?, false),
                None => {
                    let main = db::ensure_main_conversation(c, &owner)?;
                    let m = crate::entities::insert_card(c, &main, &card, "assistant")?;
                    db::set_setting(c, LAUNCHER_CARD, &m.id)?;
                    (m, true)
                }
            };
            db::set_setting(c, LAUNCHER_CARD_KEY, &key)?;
            Ok(Some((owner, m, created)))
        })
        .await
        .ok()
        .flatten();
    if let Some((owner, m, created)) = posted {
        let kind = if created {
            "message.created"
        } else {
            "message.updated"
        };
        state.hub.to_users(
            std::slice::from_ref(&owner),
            json!({"type": kind, "conversation_id": m.conversation_id, "message": m}),
        );
    }
}

async fn resolve_launcher_card(state: &AppState) {
    let updated = state
        .db
        .call(move |c| {
            // A later mismatch posts a new card, even with the same builds.
            db::delete_setting(c, LAUNCHER_CARD_KEY)?;
            let Some(id) = db::get_setting(c, LAUNCHER_CARD)? else {
                return Ok(None);
            };
            let Some(m) = db::get_message(c, &id)? else {
                return Ok(None);
            };
            let mut card = m.card.clone().unwrap_or(Value::Null);
            if card["state"] == "done" {
                return Ok(None);
            }
            card["state"] = "done".into();
            card["title"] = "Agent launcher updated".into();
            card["body"] = "My agent launcher matches this version of Iris now.".into();
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
    // Never change Chromium's files under a running browser.
    let browser = name == engines::BROWSER.name;
    if browser {
        state.browser.begin_maintenance().await;
    }
    let result = execute(state, op, name).await;
    if browser {
        state.browser.end_maintenance();
    }
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
    if name == engines::BROWSER.name {
        // Not an AI engine: nothing to sign in to.
        crate::server::browser_broadcast(state);
        return;
    }
    if result.is_ok() {
        if op == "install" {
            crate::awareness::finish(
                state,
                "install",
                name,
                "done",
                &format!("{} installed ✓", crate::login::engine_label(name)),
            )
            .await;
        }
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
    crate::server::browser_broadcast(state);
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
    let due = |key: &'static str| async move {
        state
            .setting(key)
            .await
            .and_then(|s| s.parse::<i64>().ok())
            .is_none_or(|l| now_ms() - l >= DAY_MS)
    };
    let engines_due = ENGINES.iter().any(|e| engines::is_installed(&root, e.name))
        && due("engines.last_update").await;
    // Iris's browser has its own clock: it is only updated while closed
    // (never swapping Chromium's files under it), so it may be due later
    // than the CLIs. A browser visiting arbitrary sites needs its updates.
    let browser_due = engines::is_installed(&root, engines::BROWSER.name)
        && state.browser.running().is_none()
        && due("engines.browser_last_update").await;
    if !engines_due && !browser_due {
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
    let mark = |key: &'static str| async move {
        let now = now_ms().to_string();
        let _ = state.db.call(move |c| db::set_setting(c, key, &now)).await;
    };
    if engines_due {
        mark("engines.last_update").await;
        for e in ENGINES {
            if engines::is_installed(&root, e.name) && state.engine_ops.try_begin(e.name, "update")
            {
                run_op(state, e.name, "update").await;
            }
        }
    }
    if browser_due && state.engine_ops.try_begin(engines::BROWSER.name, "update") {
        mark("engines.browser_last_update").await;
        run_op(state, engines::BROWSER.name, "update").await;
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
            let note = if done && !crate::awareness::resolved(&card) {
                Some(crate::awareness::insert_note(
                    c,
                    &m.conversation_id,
                    "Engine setup completed ✓",
                )?)
            } else {
                None
            };
            card["options"] = Value::Array(options);
            if done {
                card["state"] = "done".into();
                card["body"] =
                    "You're all set. You can add more engines any time in Settings.".into();
            }
            db::update_message_card(c, &id, &card).map(|m| Some((m, note)))
        })
        .await
        .ok()
        .flatten();
    if let Some((m, note)) = updated {
        if let Some(note) = note {
            crate::awareness::broadcast_created(state, &note).await;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(tmp: &tempfile::TempDir) -> AppState {
        let paths = crate::config::Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        AppState::new(
            crate::config::Config::default(),
            paths,
            db::Db::open_in_memory().unwrap(),
        )
        .unwrap()
    }

    fn check(state: LauncherState, build: Option<&str>) -> LauncherCheck {
        LauncherCheck {
            state,
            build_id: build.map(String::from),
            service_build_id: "0a1b2c3d".into(),
        }
    }

    async fn cards(state: &AppState) -> Vec<db::Message> {
        state
            .db
            .call(|c| {
                let owner = db::owner_user_id(c)?.unwrap();
                let main = db::ensure_main_conversation(c, &owner)?;
                Ok(db::list_messages(c, &main, None, 100)?
                    .into_iter()
                    .filter(|m| m.kind == "card")
                    .collect())
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn single_user_mode_never_checks_the_launcher() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        assert!(state.cfg.agent.user.is_empty());
        assert_eq!(check_launcher(&state).await, None);
        assert_eq!(launcher_json(&state), Value::Null);
        for e in engines_json(&state) {
            assert_eq!(e["launcher_outdated"], Value::Null);
        }
        assert!(cards(&state).await.is_empty());
    }

    #[tokio::test]
    async fn outdated_launcher_gets_one_card_until_it_is_current() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        // Unknown (dev build, unreadable launcher): nothing shown.
        apply_launcher_check(&state, &check(LauncherState::Unknown, None)).await;
        assert!(cards(&state).await.is_empty());
        assert_eq!(engines_json(&state)[0]["launcher_outdated"], Value::Null);
        assert_eq!(launcher_json(&state)["state"], "unknown");

        // A launcher from before build ids: one card, posted once.
        let old = check(LauncherState::Outdated, None);
        apply_launcher_check(&state, &old).await;
        apply_launcher_check(&state, &old).await;
        let c = cards(&state).await;
        assert_eq!(c.len(), 1);
        let card = c[0].card.clone().unwrap();
        assert_eq!(card["type"], "info");
        assert_eq!(card["state"], "pending");
        let body = card["body"].as_str().unwrap();
        assert!(body.contains("doesn't report its build"), "{body}");
        assert!(
            body.contains(&format!("install -Dm755 {}", state.exe.display())),
            "{body}"
        );
        assert!(body.ends_with("/usr/local/bin/iris"), "{body}");
        assert_eq!(engines_json(&state)[0]["launcher_outdated"], true);
        assert_eq!(launcher_json(&state)["build_id"], Value::Null);

        // Another build pair while the card is open: rewritten, not repeated.
        apply_launcher_check(&state, &check(LauncherState::Outdated, Some("59d96590"))).await;
        let c = cards(&state).await;
        assert_eq!(c.len(), 1);
        let body = c[0].card.as_ref().unwrap()["body"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(body.contains("different build"), "{body}");

        // Reinstalled: the card says so, and status clears.
        let current = check(LauncherState::Current, Some("0a1b2c3d"));
        apply_launcher_check(&state, &current).await;
        let c = cards(&state).await;
        assert_eq!(c.len(), 1);
        let card = c[0].card.clone().unwrap();
        assert_eq!(card["state"], "done");
        assert_eq!(card["title"], "Agent launcher updated");
        assert_eq!(engines_json(&state)[0]["launcher_outdated"], false);

        // Outdated again later (even the same builds): a new card.
        apply_launcher_check(&state, &old).await;
        let c = cards(&state).await;
        let mut states: Vec<String> = c
            .iter()
            .map(|m| {
                m.card.as_ref().unwrap()["state"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        states.sort();
        assert_eq!(states, ["done", "pending"]);
    }
}
