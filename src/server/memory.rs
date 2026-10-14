//! Owner-only Memory screen backend. No UI or model calls.
use super::{ApiError, ApiResult, AuthUser, JsonBody, ok};
use crate::memory::{Actor, recall};
use crate::state::AppState;
use axum::{
    Router,
    extract::{Path, Query, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/memory", get(list).post(create))
        .route("/memory/{id}", get(detail).patch(edit).delete(forget))
        .route("/memory/history/{id}/undo", post(undo))
        .route("/memory/morning/{id}/undo", post(undo_morning))
}

fn error(e: crate::util::BoxError) -> ApiError {
    if let Some(rusqlite::Error::ToSqlConversionFailure(reason)) =
        e.downcast_ref::<rusqlite::Error>()
    {
        return ApiError::bad_request(reason.to_string());
    }
    ApiError::from(e)
}

fn changed(state: &AppState) {
    state.hub.send(
        crate::events::Audience::Owner,
        json!({"type":"entities.changed", "kinds":["memory"]}),
    );
}

async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<recall::Filter>,
) -> ApiResult {
    user.require_owner()?;
    ok(state
        .db
        .call(move |c| recall::list(c, &q, false))
        .await
        .map_err(error)?)
}

async fn detail(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> ApiResult {
    user.require_owner()?;
    let out = state
        .db
        .call(move |c| {
            if crate::memory::get(c, id)?.is_none() {
                return Ok(None);
            }
            recall::detail(c, id, false).map(Some)
        })
        .await
        .map_err(error)?;
    ok(out.ok_or_else(|| ApiError::not_found("Memory"))?)
}

async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<recall::Edit>,
) -> ApiResult {
    user.require_owner()?;
    let paths = state.paths.clone();
    let out = state
        .db
        .call(move |c| {
            recall::mutate(c, &paths, |c| {
                recall::save(
                    c,
                    &body,
                    None,
                    Actor::Owner,
                    &json!({"type":"owner"}),
                    &crate::util::new_id(),
                )
            })
        })
        .await
        .map_err(error)?;
    changed(&state);
    ok(out)
}

async fn edit(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    JsonBody(body): JsonBody<recall::Edit>,
) -> ApiResult {
    user.require_owner()?;
    let paths = state.paths.clone();
    let out = state
        .db
        .call(move |c| {
            recall::mutate(c, &paths, |c| {
                recall::save(
                    c,
                    &body,
                    Some(id),
                    Actor::Owner,
                    &json!({"type":"owner"}),
                    &crate::util::new_id(),
                )
            })
        })
        .await
        .map_err(error)?;
    changed(&state);
    ok(out)
}

async fn forget(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> ApiResult {
    user.require_owner()?;
    let paths = state.paths.clone();
    let out = state
        .db
        .call(move |c| {
            recall::mutate(c, &paths, |c| {
                recall::forget(c, id, Actor::Owner, &crate::util::new_id())
            })
        })
        .await
        .map_err(error)?;
    changed(&state);
    ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UndoBody {}

async fn undo(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    JsonBody(_): JsonBody<UndoBody>,
) -> ApiResult {
    user.require_owner()?;
    let paths = state.paths.clone();
    let out: Value = state
        .db
        .call(move |c| recall::mutate(c, &paths, |c| recall::undo(c, id, &crate::util::new_id())))
        .await
        .map_err(error)?;
    changed(&state);
    ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MorningUndo {
    history_id: i64,
}

async fn undo_morning(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<MorningUndo>,
) -> ApiResult {
    user.require_owner()?;
    let paths = state.paths.clone();
    let owner = user.id().to_string();
    let out = state
        .db
        .call(move |c| {
            recall::mutate(c, &paths, |c| {
                crate::memory::dream::undo_card(c, &id, body.history_id, &owner)
            })
        })
        .await
        .map_err(error)?;
    changed(&state);
    ok(out)
}
