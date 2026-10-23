//! Routines (owner only). The owner's edits apply at once; Iris's
//! proposals wait for `/routines/changes/{id}/approve`.

use super::{ApiError, ApiResult, AuthUser, JsonBody, ok};
use crate::config::{ensure_private_dir, write_private_file};
use crate::db;
use crate::entities;
use crate::routines::{self, Spec, State, docs, run};
use crate::state::AppState;
use crate::util::{new_id, now_ms};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State as AxState};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/routines", get(list).post(create))
        .route("/routines/preview", post(preview))
        .route("/routines/docs", post(upload_doc))
        .route("/routines/changes/{id}/approve", post(approve))
        .route("/routines/changes/{id}/decline", post(decline))
        .route("/routines/{id}", get(get_one).patch(update).delete(remove))
        .route("/routines/{id}/run", post(run_now))
        .route("/routines/{id}/pause", post(pause))
        .route("/routines/{id}/resume", post(resume))
        .route("/routines/{id}/docs/{doc_id}", delete(remove_doc))
}

fn changed(state: &AppState, owner: &str) {
    state.hub.to_users(
        &[owner.to_string()],
        json!({"type": "entities.changed", "kinds": ["routines"]}),
    );
}

async fn broadcast(state: &AppState, cards: &[db::Message]) {
    for m in cards {
        crate::login::broadcast_updated(state, m).await;
    }
}

async fn routine_out(state: &AppState, owner: &str, id: &str) -> ApiResult {
    let (o, i) = (owner.to_string(), id.to_string());
    let v = state
        .db
        .call(move |c| {
            let Some(r) = routines::get_routine(c, &o, &i)? else {
                return Ok(None);
            };
            let mut v = routines::routine_json(c, &r, now_ms())?;
            v["runs"] = run::list_runs(c, &i, 50)?
                .iter()
                .map(run::run_json)
                .collect::<Vec<_>>()
                .into();
            Ok(Some(v))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("routine"))?;
    ok(json!({"routine": v}))
}

fn doc_ids(body: &Value) -> Result<Option<Vec<String>>, ApiError> {
    match body.get("doc_ids") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .ok_or_else(|| ApiError::bad_request("doc_ids must be document ids"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(ApiError::bad_request("doc_ids must be a list")),
    }
}

async fn list(AxState(state): AxState<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    let owner = user.id().to_string();
    let (routines, tz) = state
        .db
        .call(move |c| {
            let now = now_ms();
            let list = routines::list_routines(c, &owner)?
                .iter()
                .map(|r| routines::routine_json(c, r, now))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((list, routines::default_tz(c)))
        })
        .await?;
    ok(json!({
        "routines": routines,
        "default_tz": tz,
        "max_doc_bytes": state.cfg.max_upload_bytes,
    }))
}

async fn get_one(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    routine_out(&state, user.id(), &id).await
}

async fn create(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult {
    user.require_owner()?;
    let owner = user.id().to_string();
    let docs = doc_ids(&body)?.unwrap_or_default();
    let paused = body.get("state").and_then(Value::as_str) == Some("paused");
    let id = state
        .db
        .call(move |c| {
            let now = now_ms();
            let mut spec = match Spec::from_json(&body, &routines::default_tz(c)) {
                Ok(s) => s,
                Err(e) => return Ok(Err(e)),
            };
            if let Err(e) = spec.validate(now) {
                return Ok(Err(e));
            }
            for d in &docs {
                if let Err(e) = routines::check_doc(c, &owner, d) {
                    return Ok(Err(e));
                }
            }
            let want = if paused { State::Paused } else { State::Active };
            let (state, next) = routines::settle(&spec, want, 0, now);
            let r = routines::insert_routine(c, &owner, &spec, state, next, "owner")?;
            if let Err(e) = routines::set_docs(c, &owner, &r.id, &docs) {
                return Ok(Err(e));
            }
            db::set_setting(c, routines::TZ_SETTING, &spec.tz)?;
            Ok(Ok(r.id))
        })
        .await??;
    changed(&state, user.id());
    routine_out(&state, user.id(), &id).await
}

/// Apply an owner's change at once: spec fields, `state` ("active" or
/// "paused"), and `doc_ids`.
async fn apply(state: &AppState, owner: &str, id: &str, body: Value) -> ApiResult {
    let docs = doc_ids(&body)?;
    let mut patch = body;
    if let Some(o) = patch.as_object_mut() {
        o.remove("doc_ids");
    }
    let (o, i) = (owner.to_string(), id.to_string());
    state
        .db
        .call(move |c| {
            let now = now_ms();
            let Some(r) = routines::get_routine(c, &o, &i)? else {
                return Ok(Err(ApiError::not_found("routine")));
            };
            let (mut spec, want) = match routines::apply_patch(&r.spec, &patch) {
                Ok(x) => x,
                Err(e) => return Ok(Err(e.into())),
            };
            if let Err(e) = spec.validate(now) {
                return Ok(Err(e.into()));
            }
            let want = match (r.state, want) {
                // Still waiting for its first OK (the owner sets it up from
                // its card, or with approve).
                (State::PendingApproval, _) => State::PendingApproval,
                (_, Some(s)) => s,
                (State::Done, None) => State::Active,
                (s, None) => s,
            };
            let (st, next) = routines::settle(&spec, want, r.run_count, now);
            if let Some(d) = &docs
                && let Err(e) = routines::set_docs(c, &o, &i, d)
            {
                return Ok(Err(e.into()));
            }
            routines::save_routine(c, &i, &spec, st, next)?;
            db::set_setting(c, routines::TZ_SETTING, &spec.tz)?;
            Ok(Ok(()))
        })
        .await??;
    changed(state, owner);
    routine_out(state, owner, id).await
}

async fn update(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult {
    user.require_owner()?;
    if !body.is_object() {
        return Err(ApiError::bad_request("expected an object of changes"));
    }
    apply(&state, user.id(), &id, body).await
}

async fn pause(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    apply(&state, user.id(), &id, json!({"state": "paused"})).await
}

async fn resume(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    apply(&state, user.id(), &id, json!({"state": "active"})).await
}

async fn remove(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    let (o, i) = (user.id().to_string(), id.clone());
    let deleted = state
        .db
        .call(move |c| routines::delete_routine(c, &o, &i))
        .await?
        .ok_or_else(|| ApiError::not_found("routine"))?;
    routines::run::stop_turns(&state, &deleted.stop);
    broadcast(&state, &deleted.cards).await;
    changed(&state, user.id());
    ok(json!({"deleted": true}))
}

async fn run_now(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    if state.is_paused().await {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "paused",
            "I'm paused right now. Resume me first, then I can run it.",
        ));
    }
    let (o, i) = (user.id().to_string(), id.clone());
    let run_id = state
        .db
        .call(move |c| Ok(run::claim_manual(c, &o, &i, now_ms())))
        .await?
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, "conflict", e))?;
    run::spawn_run(&state, run_id.clone(), run::Step::Prepare);
    changed(&state, user.id());
    let r = run_id.clone();
    let run = state.db.call(move |c| run::get_run(c, &r)).await?;
    Ok(axum::Json(json!({"run": run.as_ref().map(run::run_json)})))
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
}

/// Upload a reference document (raw body; `?name=` is the file name). It
/// is saved to the Library with its text, ready to attach with `doc_ids`.
async fn upload_doc(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    Query(q): Query<UploadQuery>,
    body: Bytes,
) -> ApiResult {
    user.require_owner()?;
    if body.len() > state.cfg.max_upload_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!(
                "That file is too big. Documents can be up to {} MB.",
                state.cfg.max_upload_bytes / (1024 * 1024)
            ),
        ));
    }
    let name: String = q
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(entities::MAX_TITLE)
        .collect();
    let docs::Extracted {
        kind,
        text,
        summary,
    } = docs::extract_bounded(name.clone(), body.to_vec(), !cfg!(test))
        .await
        .map_err(|e| {
            let status = match e {
                docs::DocError::Unsupported(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
                docs::DocError::Busy | docs::DocError::Unavailable => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            // The type and size, never the name or content.
            crate::info!(
                "routine document not read ({}): {} bytes, {:?}",
                e.code(),
                body.len(),
                docs::doc_type(&name)
            );
            ApiError::new(status, e.code(), e.message())
        })?;
    let id = new_id();
    let file_name = format!("{id}.{}", kind.ext());
    let lib = state.paths.library_dir();
    let (path, bytes) = (lib.join(&file_name), body.to_vec());
    tokio::task::spawn_blocking(move || {
        ensure_private_dir(&lib)?;
        write_private_file(&path, &bytes)
    })
    .await
    .map_err(|e| ApiError::from(crate::util::BoxError::from(e.to_string())))??;
    let (owner, size) = (user.id().to_string(), body.len() as i64);
    let doc = state
        .db
        .call(move |c| {
            db::write_tx(c, |c| {
                let item = entities::insert_library_item(
                    c,
                    &owner,
                    &entities::NewLibraryItem {
                        id: &id,
                        kind: "artifact",
                        title: &name,
                        description: "Reference document for a routine",
                        mime: kind.mime(),
                        file_name: &file_name,
                        size,
                        conversation_id: None,
                    },
                )?;
                c.execute(
                    "UPDATE library_items SET extracted_text = ?2, extract_summary = ?3
                     WHERE id = ?1",
                    rusqlite::params![item.id, text, summary],
                )?;
                Ok(routines::Doc {
                    id: item.id,
                    title: item.title,
                    mime: item.mime,
                    size: item.size,
                    chars: text.chars().count() as i64,
                    summary,
                })
            })
        })
        .await?;
    state.hub.to_users(
        &[user.id().to_string()],
        json!({"type": "entities.changed", "kinds": ["library"]}),
    );
    ok(json!({"doc": doc}))
}

async fn remove_doc(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    Path((id, doc_id)): Path<(String, String)>,
) -> ApiResult {
    user.require_owner()?;
    let (o, i) = (user.id().to_string(), id.clone());
    let removed = state
        .db
        .call(move |c| {
            if routines::get_routine(c, &o, &i)?.is_none() {
                return Ok(false);
            }
            routines::unlink_doc(c, &i, &doc_id)
        })
        .await?;
    if !removed {
        return Err(ApiError::not_found("document"));
    }
    changed(&state, user.id());
    routine_out(&state, user.id(), &id).await
}

/// Next runs of a draft schedule (the editor's live preview).
async fn preview(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    JsonBody(mut body): JsonBody<Value>,
) -> ApiResult {
    user.require_owner()?;
    let count = body
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(3)
        .clamp(1, 10) as usize;
    let Some(o) = body.as_object_mut() else {
        return Err(ApiError::bad_request("expected a draft schedule"));
    };
    // Only the schedule matters here.
    o.entry("title").or_insert_with(|| "Preview".into());
    o.entry("instructions").or_insert_with(|| "Preview".into());
    for k in ["title", "instructions"] {
        if o[k].as_str().is_none_or(|s| s.trim().is_empty()) {
            o.insert(k.into(), "Preview".into());
        }
    }
    let tz = state.db.call(|c| Ok(routines::default_tz(c))).await?;
    let now = now_ms();
    let mut spec = Spec::from_json(&body, &tz)?;
    let plan = spec.validate(now)?;
    // Runs the routine already had count toward `max_runs`.
    let done = body["done"].as_i64().unwrap_or(0).max(0);
    let runs: Vec<Value> = plan
        .upcoming(now, count, spec.max_runs.map(|m| m - done))
        .into_iter()
        .map(|t| json!({"at": t, "text": plan.local_text(t)}))
        .collect();
    let cron_text = match &plan.when {
        routines::schedule::When::Cron(c) => Some(c.describe()),
        _ => None,
    };
    ok(json!({
        "runs": runs,
        "schedule_text": spec.schedule_text(),
        "cron_text": cron_text,
        "tz": spec.tz,
    }))
}

async fn approve(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    user.require_owner()?;
    // Optional body: {"routine": {...}}, the owner's edited version of a
    // proposed new routine.
    let edited = if body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        let v: Value =
            serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("expected JSON"))?;
        v.get("routine").filter(|r| !r.is_null()).cloned()
    };
    let owner = user.id().to_string();
    let decided = state
        .db
        .call(move |c| {
            let edited = match edited {
                Some(v) => match Spec::from_json(&v, &routines::default_tz(c)) {
                    Ok(s) => {
                        let docs = match doc_ids(&v) {
                            Ok(d) => d,
                            Err(_) => return Ok(Err("doc_ids must be a list of ids".to_string())),
                        };
                        Some((s, docs))
                    }
                    Err(e) => return Ok(Err(e)),
                },
                None => None,
            };
            let docs = edited.as_ref().and_then(|(_, d)| d.clone());
            // Checked first: a failed attach must not leave it approved.
            if let Some(d) = &docs
                && let Err(e) = routines::check_docs(c, &owner, d)
            {
                return Ok(Err(e));
            }
            let res = routines::approve_change(c, &owner, &id, edited.map(|(s, _)| s), now_ms());
            if let (Ok(d), Some(docs)) = (&res, docs)
                && let Some(r) = &d.routine
                && let Err(e) = routines::set_docs(c, &owner, &r.id, &docs)
            {
                return Ok(Err(e));
            }
            Ok(res)
        })
        .await?
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, "conflict", e))?;
    routines::run::stop_turns(&state, &decided.stop);
    broadcast(&state, &decided.cards).await;
    changed(&state, user.id());
    match decided.routine {
        Some(r) => routine_out(&state, user.id(), &r.id).await,
        None => ok(json!({"routine": null, "deleted": true})),
    }
}

async fn decline(
    AxState(state): AxState<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    let owner = user.id().to_string();
    let decided = state
        .db
        .call(move |c| Ok(routines::decline_change(c, &owner, &id)))
        .await?
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, "conflict", e))?;
    broadcast(&state, &decided.cards).await;
    changed(&state, user.id());
    ok(json!({"declined": true}))
}
