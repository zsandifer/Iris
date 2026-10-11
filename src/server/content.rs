//! Feed, ideas, goals, library, and system files.

use super::chat::file_response;
use super::{ApiError, ApiResult, AuthUser, JsonBody, ok};
use crate::db;
use crate::entities::{self, MAX_TITLE, check_text};
use crate::persona::{SYSTEM_FILES, system_file, system_file_path};
use crate::router::Purpose;
use crate::state::AppState;
use crate::turn;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;
use std::path::{Component, PathBuf};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/feed", get(list_feed))
        .route("/feed/status", get(feed_status))
        .route("/feed/activity", post(feed_activity))
        .route("/feed/resume", post(resume_feed))
        .route("/feed/pause", post(pause_feed))
        .route("/feed/sources", get(list_sources).post(add_source))
        .route(
            "/feed/sources/{id}",
            axum::routing::patch(update_source).delete(delete_source),
        )
        .route("/feed/images/{name}", get(feed_image))
        .route("/feed/refresh", post(refresh_feed))
        .route("/feed/prompt", get(get_feed_prompt).put(put_feed_prompt))
        .route("/feed/{id}", axum::routing::delete(delete_feed_post))
        .route("/feed/{id}/like", post(like_feed_post))
        .route("/feed/{id}/discuss", post(discuss_feed_post))
        .route("/ideas", get(list_ideas))
        .route(
            "/ideas/{id}",
            axum::routing::patch(update_idea).delete(delete_idea),
        )
        .route("/ideas/{id}/start", post(start_idea))
        .route("/goals", get(list_goals).post(create_goal))
        .route(
            "/goals/{id}",
            axum::routing::patch(update_goal).delete(delete_goal),
        )
        .route("/library", get(list_library))
        .route(
            "/library/{id}",
            get(get_library_item).delete(delete_library_item),
        )
        .route("/library/{id}/content", get(library_content))
        .route("/files", get(list_files))
        .route(
            "/files/system/{name}",
            get(read_system_file).put(write_system_file),
        )
        .route("/files/browse/{area}", get(browse))
        .route(
            "/files/content/{area}",
            get(read_file).put(write_file).delete(delete_file),
        )
}

fn conversation_created(state: &AppState, user: &AuthUser, conv: &db::Conversation) {
    state.hub.to_users(
        &[user.id().to_string()],
        json!({"type": "conversation.created", "conversation": conv}),
    );
}

fn changed(state: &AppState, user: &AuthUser, kind: &str) {
    state.hub.to_users(
        &[user.id().to_string()],
        json!({"type": "entities.changed", "kinds": [kind]}),
    );
}

// --------------------------------------------------------------------- feed

#[derive(Deserialize)]
struct Page {
    before: Option<i64>,
    limit: Option<usize>,
    /// Only saved (liked) posts.
    #[serde(default)]
    liked: bool,
}

async fn list_feed(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<Page>,
) -> ApiResult {
    let uid = user.id().to_string();
    let limit = q.limit.unwrap_or(30).clamp(1, 100);
    let mut posts = state
        .db
        .call(move |c| entities::list_feed_filtered(c, &uid, q.before, limit + 1, q.liked))
        .await?;
    let has_more = posts.len() > limit;
    posts.truncate(limit);
    let status = crate::feed::status_json(&state, user.id()).await;
    ok(json!({"posts": posts, "has_more": has_more, "status": status}))
}

async fn feed_status(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    ok(json!({"status": crate::feed::status_json(&state, user.id()).await}))
}

#[derive(Deserialize)]
struct Activity {
    event: String,
}

/// The app reports feed opens and reaching the end of today's posts; they
/// drive the adaptive daily size and the two-week pause.
async fn feed_activity(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<Activity>,
) -> ApiResult {
    if !matches!(body.event.as_str(), "opened" | "reached_end") {
        return Err(ApiError::bad_request("event must be opened or reached_end"));
    }
    crate::feed::record_activity(&state, user.id(), &body.event).await?;
    ok(json!({"ok": true}))
}

async fn resume_feed(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    let was_paused = crate::feed::resume(&state, user.id()).await?;
    changed(&state, &user, "feed");
    ok(
        json!({"ok": true, "was_paused": was_paused, "status": crate::feed::status_json(&state, user.id()).await}),
    )
}

/// Pause the daily posts by hand (no chat message; that is only for the
/// automatic pause).
async fn pause_feed(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    let uid = user.id().to_string();
    state
        .db
        .call(move |c| {
            crate::feed::store::update_state(c, &uid, |s| {
                if !s.paused {
                    s.paused = true;
                    s.paused_by_user = true;
                    s.paused_at = Some(crate::util::now_ms());
                }
            })
        })
        .await?;
    changed(&state, &user, "feed");
    ok(json!({"ok": true, "status": crate::feed::status_json(&state, user.id()).await}))
}

async fn list_sources(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    let uid = user.id().to_string();
    let sources = state
        .db
        .call(move |c| crate::feed::store::list_sources(c, &uid))
        .await?;
    ok(json!({"sources": sources}))
}

#[derive(Deserialize)]
struct NewSource {
    /// A link, `r/name`, `@handle`, `user@instance`, `#tag`, or a YouTube
    /// channel link.
    input: String,
    #[serde(default)]
    topic: Option<String>,
}

async fn add_source(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<NewSource>,
) -> ApiResult {
    let (kind, value) =
        crate::feed::sources::parse_input(&body.input).map_err(ApiError::bad_request)?;
    let topic = body
        .topic
        .map(|t| t.trim().chars().take(40).collect::<String>())
        .filter(|t| !t.is_empty());
    let uid = user.id().to_string();
    let (k, v) = (kind.clone(), value.clone());
    let added = state
        .db
        .call(move |c| {
            let added =
                crate::feed::store::add_source(c, &uid, &k, &v, None, topic.as_deref(), "user")?;
            if added.is_none() {
                // Already there (maybe suggested or turned off): make it the
                // user's own and turn it on.
                c.execute(
                    "UPDATE feed_sources SET origin = 'user', enabled = 1, fail_count = 0
                     WHERE user_id = ?1 AND kind = ?2 AND value = ?3",
                    rusqlite::params![uid, k, v],
                )?;
            }
            let all = crate::feed::store::list_sources(c, &uid)?;
            Ok(all.into_iter().find(|s| s.kind == k && s.value == v))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("source"))?;
    ok(json!({"source": added}))
}

#[derive(Deserialize)]
struct SourcePatch {
    enabled: bool,
}

async fn update_source(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<SourcePatch>,
) -> ApiResult {
    let uid = user.id().to_string();
    let source = state
        .db
        .call(move |c| {
            if !crate::feed::store::set_source_enabled(c, &uid, &id, body.enabled)? {
                return Ok(None);
            }
            crate::feed::store::get_source(c, &uid, &id)
        })
        .await?
        .ok_or_else(|| ApiError::not_found("source"))?;
    ok(json!({"source": source}))
}

async fn delete_source(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let uid = user.id().to_string();
    if !state
        .db
        .call(move |c| crate::feed::store::delete_source(c, &uid, &id))
        .await?
    {
        return Err(ApiError::not_found("source"));
    }
    ok(json!({"ok": true}))
}

/// A cached feed photo or icon (the source's original image, stored
/// locally so the phone does not contact third-party sites).
async fn feed_image(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    if !crate::feed::images::valid_name(&name) {
        return Err(ApiError::not_found("image"));
    }
    // Only images of the caller's own posts (names are hex, so the LIKE
    // pattern has no wildcards of its own).
    let (uid, pattern) = (user.id().to_string(), format!("%\"{name}\"%"));
    let mine: bool = state
        .db
        .call(move |c| {
            c.query_row(
                "SELECT EXISTS (SELECT 1 FROM feed_posts WHERE user_id = ?1 AND cached LIKE ?2)",
                rusqlite::params![uid, pattern],
                |r| r.get(0),
            )
        })
        .await?;
    if !mine {
        return Err(ApiError::not_found("image"));
    }
    let mime = crate::feed::images::mime_for(&name).ok_or_else(|| ApiError::not_found("image"))?;
    let path = crate::feed::images::dir(&state.paths.data_dir).join(&name);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found("image"))?;
    let mut res = file_response(mime, bytes);
    res.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, max-age=604800, immutable"),
    );
    Ok(res)
}

#[derive(Deserialize)]
struct Like {
    #[serde(default = "yes")]
    liked: bool,
}

fn yes() -> bool {
    true
}

/// Body is optional: no body (or `{}`) means like; `{"liked": false}`
/// unlikes.
async fn like_feed_post(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult {
    let liked = if body.iter().all(u8::is_ascii_whitespace) {
        true
    } else {
        serde_json::from_slice::<Like>(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?
            .liked
    };
    let uid = user.id().to_string();
    let pid = id.clone();
    if !state
        .db
        .call(move |c| {
            if !entities::set_feed_liked(c, &uid, &pid, liked)? {
                return Ok(false);
            }
            // Saved products are watched for price drops.
            if liked {
                crate::feed::store::watch_post(c, &uid, &pid)?;
            } else {
                crate::feed::store::unwatch_post(c, &uid, &pid)?;
            }
            Ok(true)
        })
        .await?
    {
        return Err(ApiError::not_found("post"));
    }
    changed(&state, &user, "feed");
    ok(json!({"ok": true, "liked": liked}))
}

async fn delete_feed_post(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let uid = user.id().to_string();
    if !state
        .db
        .call(move |c| entities::delete_feed_post(c, &uid, &id))
        .await?
    {
        return Err(ApiError::not_found("post"));
    }
    changed(&state, &user, "feed");
    ok(json!({"ok": true}))
}

/// Open (or reuse) a side chat seeded with the post.
async fn discuss_feed_post(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let uid = user.id().to_string();
    let conv = state
        .db
        .call(move |c| {
            let Some(post) = entities::get_feed_post(c, &uid, &id)? else {
                return Ok(None);
            };
            if let Some(existing) = &post.discuss_conversation_id
                && let Some(conv) = db::get_conversation(c, &uid, existing)?
            {
                return Ok(Some((conv, false)));
            }
            let seed = json!({"type": "feed_post", "id": post.id});
            let conv = db::create_conversation(c, &uid, "side", &post.title, Some(&seed))?;
            // The post becomes context for the first turn (replayed to the
            // harness) and is shown as a card at the top of the chat.
            let card = json!({
                "type": "feed_post",
                "title": post.title,
                "body": crate::util::truncate_chars(&post.body, 4000),
                "url": post.source_url,
                "ref_id": post.id,
            });
            db::insert_message(
                c,
                &db::NewMessage {
                    conversation_id: &conv.id,
                    role: "system",
                    kind: "card",
                    content: &discussion_context(&post),
                    card: Some(&card),
                    ..Default::default()
                },
            )?;
            entities::set_feed_discussion(c, &post.id, &conv.id)?;
            Ok(db::get_conversation_any(c, &conv.id)?.map(|c| (c, true)))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("post"))?;
    let (conv, created) = conv;
    if created {
        conversation_created(&state, &user, &conv);
    }
    ok(json!({"conversation": conv}))
}

/// What the agent sees about a post when the user wants to talk about it.
fn discussion_context(post: &entities::FeedPost) -> String {
    let mut s = format!("Feed post \"{}\": {}", post.title, post.body);
    if let Some(src) = post.source_name.as_deref().filter(|s| !s.is_empty()) {
        s.push_str(&format!("\nSource: {src}"));
    }
    if let Some(url) = post.source_url.as_deref().filter(|s| !s.is_empty()) {
        s.push_str(&format!("\nLink: {url}"));
    }
    if let Some(why) = post.why.as_deref().filter(|s| !s.is_empty()) {
        s.push_str(&format!("\nWhy it was picked: {why}"));
    }
    s
}

async fn refresh_feed(State(state): State<AppState>, user: AuthUser) -> Result<Response, ApiError> {
    user.require_owner()?;
    let started = crate::jobs::trigger(&state, crate::jobs::Job::Feed, true);
    Ok((
        StatusCode::ACCEPTED,
        axum::Json(json!({"started": started})),
    )
        .into_response())
}

async fn get_feed_prompt(State(state): State<AppState>, _user: AuthUser) -> ApiResult {
    ok(json!({"feed_prompt": state.feed_prompt().await}))
}

#[derive(Deserialize)]
struct FeedPrompt {
    feed_prompt: String,
}

async fn put_feed_prompt(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<FeedPrompt>,
) -> ApiResult {
    user.require_owner()?;
    check_text("feed_prompt", &body.feed_prompt, 4000, false)?;
    let v = body.feed_prompt.trim().to_string();
    state
        .db
        .call(move |c| {
            if v.is_empty() {
                db::delete_setting(c, "feed_prompt")
            } else {
                db::set_setting(c, "feed_prompt", &v)
            }
        })
        .await?;
    let settings = state.settings_json().await;
    state.hub.send(
        crate::events::Audience::All,
        json!({"type": "settings.changed", "settings": settings}),
    );
    ok(json!({"feed_prompt": state.feed_prompt().await}))
}

// -------------------------------------------------------------------- ideas

#[derive(Deserialize)]
struct IdeaQuery {
    status: Option<String>,
}

async fn list_ideas(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<IdeaQuery>,
) -> ApiResult {
    let uid = user.id().to_string();
    let ideas = state
        .db
        .call(move |c| entities::list_ideas(c, &uid, q.status.as_deref()))
        .await?;
    ok(json!({"ideas": ideas}))
}

#[derive(Deserialize)]
struct UpdateIdea {
    status: String,
}

async fn update_idea(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<UpdateIdea>,
) -> ApiResult {
    if !entities::IDEA_STATUSES.contains(&body.status.as_str()) {
        return Err(ApiError::bad_request(format!(
            "status must be one of {}",
            entities::IDEA_STATUSES.join(", ")
        )));
    }
    let uid = user.id().to_string();
    let idea = state
        .db
        .call(move |c| {
            entities::update_idea(c, &uid, &id, Some(&body.status), None)?;
            entities::get_idea(c, &uid, &id)
        })
        .await?
        .ok_or_else(|| ApiError::not_found("idea"))?;
    changed(&state, &user, "ideas");
    ok(json!({"idea": idea}))
}

async fn delete_idea(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let uid = user.id().to_string();
    if !state
        .db
        .call(move |c| entities::delete_idea(c, &uid, &id))
        .await?
    {
        return Err(ApiError::not_found("idea"));
    }
    changed(&state, &user, "ideas");
    ok(json!({"ok": true}))
}

/// Tapping an idea: open a side chat and ask the agent to do it.
async fn start_idea(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let uid = user.id().to_string();
    let (idea, conv) = state
        .db
        .call(move |c| {
            let Some(idea) = entities::get_idea(c, &uid, &id)? else {
                return Ok(None);
            };
            let seed = json!({"type": "idea", "id": idea.id});
            let conv = db::create_conversation(c, &uid, "side", &idea.title, Some(&seed))?;
            entities::update_idea(c, &uid, &idea.id, Some("started"), Some(&conv.id))?;
            Ok(Some((idea, conv)))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("idea"))?;
    conversation_created(&state, &user, &conv);
    let prompt = idea.prompt.clone().unwrap_or_else(|| {
        if idea.description.is_empty() {
            idea.title.clone()
        } else {
            format!("{}\n\n{}", idea.title, idea.description)
        }
    });
    let (user_message, assistant_message) = turn::start_chat_turn(
        &state,
        &conv.id,
        Some(user.id()),
        &prompt,
        &[],
        Purpose::Chat,
        user.id(),
    )
    .await?;
    changed(&state, &user, "ideas");
    Ok((
        StatusCode::ACCEPTED,
        axum::Json(json!({
            "conversation": conv,
            "user_message": user_message,
            "assistant_message": assistant_message,
        })),
    )
        .into_response())
}

// -------------------------------------------------------------------- goals

#[derive(Deserialize)]
struct GoalQuery {
    #[serde(default)]
    include_done: bool,
}

async fn list_goals(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<GoalQuery>,
) -> ApiResult {
    let uid = user.id().to_string();
    let goals = state
        .db
        .call(move |c| entities::list_goals(c, &uid, q.include_done))
        .await?;
    ok(json!({"goals": goals}))
}

#[derive(Deserialize)]
struct NewGoal {
    title: String,
    #[serde(default)]
    status_text: String,
}

async fn create_goal(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<NewGoal>,
) -> ApiResult {
    check_text("title", &body.title, MAX_TITLE, true)?;
    check_text("status_text", &body.status_text, 2000, false)?;
    let uid = user.id().to_string();
    let goal = state
        .db
        .call(move |c| entities::create_goal(c, &uid, &body.title, &body.status_text, false))
        .await?;
    changed(&state, &user, "goals");
    ok(json!({"goal": goal}))
}

#[derive(Deserialize)]
struct PatchGoal {
    title: Option<String>,
    status_text: Option<String>,
    done: Option<bool>,
    /// `false` confirms a goal the agent suggested.
    suggested: Option<bool>,
}

async fn update_goal(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<PatchGoal>,
) -> ApiResult {
    if let Some(t) = &body.title {
        check_text("title", t, MAX_TITLE, true)?;
    }
    if let Some(s) = &body.status_text {
        check_text("status_text", s, 2000, false)?;
    }
    let uid = user.id().to_string();
    let goal = state
        .db
        .call(move |c| {
            entities::update_goal(
                c,
                &uid,
                &id,
                &entities::GoalChanges {
                    title: body.title.as_deref(),
                    status_text: body.status_text.as_deref(),
                    done: body.done,
                    suggested: body.suggested,
                },
            )
        })
        .await?
        .ok_or_else(|| ApiError::not_found("goal"))?;
    changed(&state, &user, "goals");
    ok(json!({"goal": goal}))
}

async fn delete_goal(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let uid = user.id().to_string();
    if !state
        .db
        .call(move |c| entities::delete_goal(c, &uid, &id))
        .await?
    {
        return Err(ApiError::not_found("goal"));
    }
    changed(&state, &user, "goals");
    ok(json!({"ok": true}))
}

// ------------------------------------------------------------------ library

#[derive(Deserialize)]
struct LibraryQuery {
    kind: Option<String>,
}

async fn list_library(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<LibraryQuery>,
) -> ApiResult {
    let uid = user.id().to_string();
    let items = state
        .db
        .call(move |c| entities::list_library(c, &uid, q.kind.as_deref()))
        .await?;
    ok(json!({"items": items}))
}

async fn get_library_item(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let uid = user.id().to_string();
    let item = state
        .db
        .call(move |c| entities::get_library_item(c, &uid, &id))
        .await?
        .ok_or_else(|| ApiError::not_found("library item"))?;
    ok(json!({"item": item, "content_url": format!("/api/library/{}/content", item.id)}))
}

async fn library_content(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let uid = user.id().to_string();
    let item = state
        .db
        .call(move |c| entities::get_library_item(c, &uid, &id))
        .await?
        .ok_or_else(|| ApiError::not_found("library item"))?;
    let bytes = tokio::fs::read(state.paths.library_dir().join(&item.file_name))
        .await
        .map_err(|_| ApiError::not_found("library file"))?;
    Ok(file_response(&item.mime, bytes))
}

async fn delete_library_item(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let uid = user.id().to_string();
    let file = state
        .db
        .call(move |c| entities::delete_library_item(c, &uid, &id))
        .await?
        .ok_or_else(|| ApiError::not_found("library item"))?;
    let _ = tokio::fs::remove_file(state.paths.library_dir().join(file)).await;
    changed(&state, &user, "library");
    ok(json!({"ok": true}))
}

// ------------------------------------------------------------- system files

const MAX_TEXT_FILE: usize = 1024 * 1024;

fn modified_ms(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

async fn list_files(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    user.require_owner()?;
    let files: Vec<_> = SYSTEM_FILES
        .iter()
        .map(|f| {
            let meta = std::fs::metadata(system_file_path(&state.paths, f)).ok();
            json!({
                "name": f.name,
                "description": f.description,
                "in_prompt": f.in_prompt,
                "exists": meta.is_some(),
                "size": meta.as_ref().map(|m| m.len()),
                "updated_at": meta.as_ref().and_then(modified_ms),
            })
        })
        .collect();
    ok(json!({
        "system_files": files,
        "folders": [
            {"area": "memory", "description": "Long-term memory notes"},
            {"area": "workspace", "description": "The agent's working folder"},
            {"area": "persona", "description": "Persona files"},
        ],
    }))
}

async fn read_system_file(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
) -> ApiResult {
    user.require_owner()?;
    let f = system_file(&name).ok_or_else(|| ApiError::not_found("system file"))?;
    let content = tokio::fs::read_to_string(system_file_path(&state.paths, f))
        .await
        .unwrap_or_default();
    ok(json!({"name": f.name, "description": f.description, "content": content}))
}

#[derive(Deserialize)]
struct FileContent {
    content: String,
}

async fn write_system_file(
    State(state): State<AppState>,
    user: AuthUser,
    Path(name): Path<String>,
    JsonBody(body): JsonBody<FileContent>,
) -> ApiResult {
    user.require_owner()?;
    let f = system_file(&name).ok_or_else(|| ApiError::not_found("system file"))?;
    if body.content.len() > MAX_TEXT_FILE {
        return Err(ApiError::bad_request("file is too large"));
    }
    crate::config::write_private_file(&system_file_path(&state.paths, f), body.content.as_bytes())?;
    ok(json!({"ok": true, "name": f.name}))
}

fn area_root(state: &AppState, area: &str) -> Result<PathBuf, ApiError> {
    match area {
        "memory" => Ok(state.paths.memory_dir()),
        "workspace" => Ok(state.paths.workspace_dir()),
        "persona" => Ok(state.paths.persona_dir()),
        _ => Err(ApiError::not_found("folder")),
    }
}

/// Resolve `rel` inside `root`, rejecting traversal, hidden names, and
/// symlinks anywhere along the path.
pub fn safe_join(root: &std::path::Path, rel: &str) -> Result<PathBuf, String> {
    let mut out = root.to_path_buf();
    for comp in std::path::Path::new(rel).components() {
        match comp {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                if name.starts_with('.') {
                    return Err("hidden files are not accessible".into());
                }
                out.push(name.as_ref());
                if let Ok(meta) = std::fs::symlink_metadata(&out)
                    && meta.file_type().is_symlink()
                {
                    return Err("symlinks are not accessible".into());
                }
            }
            Component::CurDir => {}
            _ => return Err("invalid path".into()),
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct PathQuery {
    #[serde(default)]
    path: String,
}

async fn browse(
    State(state): State<AppState>,
    user: AuthUser,
    Path(area): Path<String>,
    Query(q): Query<PathQuery>,
) -> ApiResult {
    user.require_owner()?;
    let dir = safe_join(&area_root(&state, &area)?, &q.path)?;
    let mut entries = Vec::new();
    let mut rd = tokio::fs::read_dir(&dir)
        .await
        .map_err(|_| ApiError::not_found("folder"))?;
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type().await else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        let meta = e.metadata().await.ok();
        entries.push(json!({
            "name": name,
            "type": if ft.is_dir() { "dir" } else { "file" },
            "size": meta.as_ref().filter(|_| ft.is_file()).map(|m| m.len()),
            "updated_at": meta.as_ref().and_then(modified_ms),
        }));
    }
    entries.sort_by(|a, b| {
        (a["type"] != "dir", a["name"].as_str()).cmp(&(b["type"] != "dir", b["name"].as_str()))
    });
    ok(json!({"area": area, "path": q.path, "entries": entries}))
}

async fn read_file(
    State(state): State<AppState>,
    user: AuthUser,
    Path(area): Path<String>,
    Query(q): Query<PathQuery>,
) -> ApiResult {
    user.require_owner()?;
    let path = safe_join(&area_root(&state, &area)?, &q.path)?;
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|_| ApiError::not_found("file"))?;
    if !meta.is_file() {
        return Err(ApiError::bad_request("not a file"));
    }
    if meta.len() as usize > MAX_TEXT_FILE {
        return Err(ApiError::bad_request("file is too large to edit here"));
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found("file"))?;
    let content = String::from_utf8(bytes).map_err(|_| ApiError::bad_request("not a text file"))?;
    ok(json!({"area": area, "path": q.path, "content": content}))
}

async fn write_file(
    State(state): State<AppState>,
    user: AuthUser,
    Path(area): Path<String>,
    Query(q): Query<PathQuery>,
    JsonBody(body): JsonBody<FileContent>,
) -> ApiResult {
    user.require_owner()?;
    if q.path.trim().is_empty() {
        return Err(ApiError::bad_request("path is required"));
    }
    if body.content.len() > MAX_TEXT_FILE {
        return Err(ApiError::bad_request("file is too large"));
    }
    let path = safe_join(&area_root(&state, &area)?, &q.path)?;
    if let Some(parent) = path.parent() {
        crate::config::ensure_private_dir(parent)?;
    }
    crate::config::write_private_file(&path, body.content.as_bytes())?;
    ok(json!({"ok": true}))
}

async fn delete_file(
    State(state): State<AppState>,
    user: AuthUser,
    Path(area): Path<String>,
    Query(q): Query<PathQuery>,
) -> ApiResult {
    user.require_owner()?;
    if q.path.trim().is_empty() {
        return Err(ApiError::bad_request("path is required"));
    }
    let path = safe_join(&area_root(&state, &area)?, &q.path)?;
    let meta = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|_| ApiError::not_found("file"))?;
    if meta.is_dir() {
        tokio::fs::remove_dir(&path)
            .await
            .map_err(|_| ApiError::bad_request("folder is not empty"))?;
    } else {
        tokio::fs::remove_file(&path)
            .await
            .map_err(|_| ApiError::not_found("file"))?;
    }
    ok(json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_blocks_escapes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("notes")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("link")).unwrap();
        assert_eq!(
            safe_join(root, "notes/a.md").unwrap(),
            root.join("notes/a.md")
        );
        assert_eq!(safe_join(root, "").unwrap(), root.to_path_buf());
        assert!(safe_join(root, "../etc/passwd").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
        assert!(safe_join(root, "notes/../../x").is_err());
        assert!(safe_join(root, ".git/config").is_err());
        assert!(safe_join(root, "link/passwd").is_err());
    }
}
