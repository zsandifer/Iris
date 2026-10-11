//! Conversations, messages, and attachments.

use super::{ApiError, ApiResult, AuthUser, JsonBody, ok};
use crate::db::{self, Conversation};
use crate::entities::check_text;
use crate::router::Purpose;
use crate::state::AppState;
use crate::turn;
use crate::util::new_id;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/conversations",
            get(list_conversations).post(create_conversation),
        )
        .route("/conversations/main", get(main_conversation))
        .route(
            "/conversations/{id}",
            get(get_conversation)
                .patch(update_conversation)
                .delete(delete_conversation),
        )
        .route(
            "/conversations/{id}/messages",
            get(list_messages).post(send_message),
        )
        .route(
            "/conversations/{id}/members",
            get(list_members).post(add_member),
        )
        .route(
            "/conversations/{id}/members/{user_id}",
            axum::routing::delete(remove_member),
        )
        .route("/conversations/{id}/cancel", post(cancel_turn))
        .route("/conversations/{id}/read", post(mark_read))
        .route(
            "/attachments",
            get(list_sent_photos).post(upload_attachment),
        )
        .route("/attachments/{id}", get(get_attachment))
        .route("/messages/{id}/decision", post(decide_approval))
        .route("/activity", get(list_activity))
}

/// Load a conversation the user belongs to, or 404.
pub async fn member_conversation(
    state: &AppState,
    user: &AuthUser,
    id: &str,
) -> Result<Conversation, ApiError> {
    let (uid, cid) = (user.id().to_string(), id.to_string());
    state
        .db
        .call(move |c| db::get_conversation(c, &uid, &cid))
        .await?
        .filter(|c| c.kind != "job")
        .ok_or_else(|| ApiError::not_found("conversation"))
}

/// Only the creator may rename, archive, delete, or change members of a
/// group conversation. Other kinds have a single member anyway.
async fn require_creator(
    state: &AppState,
    user: &AuthUser,
    conv: &Conversation,
) -> Result<(), ApiError> {
    if conv.kind != "group" {
        return Ok(());
    }
    if conv_owner(state, &conv.id).await.as_deref() == Some(user.id()) {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "only the person who created this group can do that",
        ))
    }
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    archived: bool,
}

async fn list_conversations(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    let uid = user.id().to_string();
    let convs = state
        .db
        .call(move |c| db::list_conversations(c, &uid, q.archived))
        .await?;
    ok(json!({"conversations": convs}))
}

#[derive(Deserialize)]
struct CreateConversation {
    #[serde(default = "side")]
    kind: String,
    #[serde(default)]
    title: String,
    /// Other users to add (group conversations).
    #[serde(default)]
    member_ids: Vec<String>,
}

fn side() -> String {
    "side".into()
}

async fn create_conversation(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<CreateConversation>,
) -> ApiResult {
    if !matches!(body.kind.as_str(), "side" | "group") {
        return Err(ApiError::bad_request("kind must be 'side' or 'group'"));
    }
    check_text("title", &body.title, 200, false)?;
    // Anyone may start a Together chat; their Iris hosts it.
    if body.kind != "group" && !body.member_ids.is_empty() {
        return Err(ApiError::bad_request(
            "side chats cannot have other members",
        ));
    }
    let uid = user.id().to_string();
    let conv = state
        .db
        .call(move |c| {
            for m in &body.member_ids {
                if db::get_user(c, m)?.is_none() {
                    return Ok(Err(format!("unknown user {m}")));
                }
            }
            let conv = db::create_conversation(c, &uid, &body.kind, body.title.trim(), None)?;
            for m in body.member_ids.iter().filter(|m| **m != uid) {
                c.execute(
                    "INSERT OR IGNORE INTO conversation_members (conversation_id, user_id, role, joined_at)
                     VALUES (?1, ?2, 'member', ?3)",
                    rusqlite::params![conv.id, m, crate::util::now_ms()],
                )?;
            }
            Ok(Ok(db::get_conversation_any(c, &conv.id)?.expect("created")))
        })
        .await??;
    let members = state
        .db
        .call({
            let id = conv.id.clone();
            move |c| db::conversation_member_ids(c, &id)
        })
        .await?;
    state.hub.to_users(
        &members,
        json!({"type": "conversation.created", "conversation": conv}),
    );
    ok(json!({"conversation": conv}))
}

async fn main_conversation(State(state): State<AppState>, user: AuthUser) -> ApiResult {
    let uid = user.id().to_string();
    let id = state
        .db
        .call(move |c| db::ensure_main_conversation(c, &uid))
        .await?;
    let conv = member_conversation(&state, &user, &id).await?;
    ok(json!({"conversation": conv}))
}

async fn get_conversation(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let conv = member_conversation(&state, &user, &id).await?;
    let running = state
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(&id);
    ok(json!({"conversation": conv, "turn_running": running}))
}

#[derive(Deserialize)]
struct UpdateConversation {
    title: Option<String>,
    archived: Option<bool>,
}

async fn update_conversation(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<UpdateConversation>,
) -> ApiResult {
    let conv = member_conversation(&state, &user, &id).await?;
    require_creator(&state, &user, &conv).await?;
    if let Some(t) = &body.title {
        check_text("title", t, 200, false)?;
    }
    let cid = id.clone();
    let conv = state
        .db
        .call(move |c| {
            db::update_conversation(c, &cid, body.title.as_deref().map(str::trim), body.archived)?;
            db::get_conversation_any(c, &cid)
        })
        .await?;
    ok(json!({"conversation": conv}))
}

async fn delete_conversation(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let conv = member_conversation(&state, &user, &id).await?;
    if conv.kind == "main" {
        return Err(ApiError::bad_request("the Main chat cannot be deleted"));
    }
    require_creator(&state, &user, &conv).await?;
    turn::cancel(&state, &id);
    state
        .db
        .call(move |c| db::delete_conversation(c, &id))
        .await?;
    ok(json!({"ok": true}))
}

#[derive(Deserialize)]
struct MessagesQuery {
    before: Option<i64>,
    limit: Option<usize>,
}

async fn list_messages(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> ApiResult {
    member_conversation(&state, &user, &id).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let mut msgs = state
        .db
        .call(move |c| db::list_messages(c, &id, q.before, limit + 1))
        .await?;
    // One extra row tells whether older messages exist.
    let has_more = msgs.len() > limit;
    if has_more {
        msgs.remove(0);
    }
    ok(json!({"messages": msgs, "has_more": has_more}))
}

#[derive(Deserialize)]
pub struct SendMessage {
    #[serde(default)]
    pub text: String,
    /// Attachment ids from `POST /api/attachments`.
    #[serde(default)]
    pub attachments: Vec<String>,
}

pub const MAX_MESSAGE_CHARS: usize = 20_000;
const MAX_ATTACHMENTS: usize = 4;

async fn send_message(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<SendMessage>,
) -> Result<Response, ApiError> {
    let conv = member_conversation(&state, &user, &id).await?;
    check_text("text", &body.text, MAX_MESSAGE_CHARS, false)?;
    if body.text.trim().is_empty() && body.attachments.is_empty() {
        return Err(ApiError::bad_request("text or attachments required"));
    }
    if body.attachments.len() > MAX_ATTACHMENTS {
        return Err(ApiError::bad_request(format!(
            "at most {MAX_ATTACHMENTS} attachments"
        )));
    }
    for a in &body.attachments {
        if !attachment_visible(&state, user.id(), a).await? {
            return Err(ApiError::bad_request(format!("unknown attachment {a}")));
        }
    }
    // Entities touched by the agent belong to the conversation creator
    // (the owner for group chats).
    let owner = conv_owner(&state, &conv.id)
        .await
        .unwrap_or_else(|| user.id().to_string());
    let (user_message, assistant_message) = turn::start_chat_turn(
        &state,
        &conv.id,
        Some(user.id()),
        &body.text,
        &body.attachments,
        Purpose::Chat,
        &owner,
    )
    .await?;
    if conv.member_count > 1 {
        notify_members(&state, &user, &conv, &body.text, &user_message).await;
    }
    Ok((
        StatusCode::ACCEPTED,
        axum::Json(json!({"user_message": user_message, "assistant_message": assistant_message})),
    )
        .into_response())
}

/// In a Together chat, tell the other people (when their app is closed)
/// what was said, like any group chat would.
async fn notify_members(
    state: &AppState,
    user: &AuthUser,
    conv: &Conversation,
    text: &str,
    msg: &db::Message,
) {
    let cid = conv.id.clone();
    let others: Vec<String> = state
        .db
        .call(move |c| db::conversation_member_ids(c, &cid))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|u| u != user.id() && !state.hub.is_online(u))
        .collect();
    if others.is_empty() || !state.cfg.push.notify_replies_when_offline {
        return;
    }
    let body = if text.trim().is_empty() {
        "Sent a photo".to_string()
    } else {
        crate::util::truncate_chars(text.trim(), 180)
    };
    let title = if conv.title.trim().is_empty() {
        user.user.display_name.clone()
    } else {
        format!("{} · {}", user.user.display_name, conv.title.trim())
    };
    crate::notify::push_users(
        state,
        &others,
        json!({"title": title, "body": body, "conversation_id": conv.id,
               "message_id": msg.id, "kind": "together"}),
    );
}

async fn conv_owner(state: &AppState, conv_id: &str) -> Option<String> {
    let id = conv_id.to_string();
    state
        .db
        .call(move |c| {
            c.query_row(
                "SELECT created_by FROM conversations WHERE id = ?1",
                [&id],
                |r| r.get::<_, Option<String>>(0),
            )
        })
        .await
        .ok()
        .flatten()
}

async fn list_members(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    let conv = member_conversation(&state, &user, &id).await?;
    // Whose Iris answers here: the creator's.
    let agent = match conv.created_by.as_deref() {
        Some(host) => match state.user(host).await {
            Some(u) => {
                let (avatar_url, avatar_custom) = state.avatar_url_for(&u).await;
                json!({"user_id": u.id, "name": state.agent_name_for(&u).await,
                       "avatar_url": avatar_url, "avatar_custom": avatar_custom})
            }
            None => serde_json::Value::Null,
        },
        None => serde_json::Value::Null,
    };
    let members = state
        .db
        .call(move |c| {
            let mut stmt = c.prepare(
                "SELECT u.id, u.display_name, m.role, m.joined_at FROM conversation_members m
                 JOIN users u ON u.id = m.user_id WHERE m.conversation_id = ?1
                 ORDER BY m.joined_at",
            )?;
            stmt.query_map([&id], |r| {
                Ok(json!({
                    "user_id": r.get::<_, String>(0)?,
                    "display_name": r.get::<_, String>(1)?,
                    "role": r.get::<_, String>(2)?,
                    "joined_at": r.get::<_, i64>(3)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    ok(json!({"members": members, "agent": agent}))
}

#[derive(Deserialize)]
struct AddMember {
    user_id: String,
}

async fn add_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<AddMember>,
) -> ApiResult {
    let conv = member_conversation(&state, &user, &id).await?;
    if conv.kind != "group" {
        return Err(ApiError::bad_request(
            "only group conversations have members",
        ));
    }
    require_creator(&state, &user, &conv).await?;
    let new_member = body.user_id.clone();
    let added = state
        .db
        .call(move |c| {
            if db::get_user(c, &body.user_id)?.is_none() {
                return Ok(false);
            }
            c.execute(
                "INSERT OR IGNORE INTO conversation_members (conversation_id, user_id, role, joined_at)
                 VALUES (?1, ?2, 'member', ?3)",
                rusqlite::params![id, body.user_id, crate::util::now_ms()],
            )?;
            Ok(true)
        })
        .await?;
    if !added {
        return Err(ApiError::not_found("user"));
    }
    let conv = member_conversation(&state, &user, &conv.id).await?;
    state.hub.to_users(
        &[new_member],
        json!({"type": "conversation.created", "conversation": conv}),
    );
    ok(json!({"ok": true}))
}

async fn remove_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, member)): Path<(String, String)>,
) -> ApiResult {
    let conv = member_conversation(&state, &user, &id).await?;
    if conv.kind != "group" {
        return Err(ApiError::bad_request(
            "only group conversations have members",
        ));
    }
    // Anyone may leave; only the creator may remove others.
    if member != user.id() {
        require_creator(&state, &user, &conv).await?;
    }
    let removed = state
        .db
        .call(move |c| {
            c.execute(
                "DELETE FROM conversation_members WHERE conversation_id = ?1 AND user_id = ?2
                 AND role != 'owner'",
                rusqlite::params![id, member],
            )
        })
        .await?;
    if removed == 0 {
        return Err(ApiError::bad_request("not a removable member"));
    }
    ok(json!({"ok": true}))
}

async fn cancel_turn(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    member_conversation(&state, &user, &id).await?;
    ok(json!({"cancelled": turn::cancel(&state, &id)}))
}

async fn mark_read(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult {
    member_conversation(&state, &user, &id).await?;
    let uid = user.id().to_string();
    state.db.call(move |c| db::mark_read(c, &id, &uid)).await?;
    ok(json!({"ok": true}))
}

// ------------------------------------------------------------- attachments

/// Supported image types, identified by magic bytes.
pub fn sniff_image(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("image/gif", "gif"))
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}

/// Store an uploaded image; returns the attachment row.
pub async fn store_upload(
    state: &AppState,
    user_id: &str,
    bytes: &[u8],
) -> Result<db::Attachment, ApiError> {
    if bytes.is_empty() {
        return Err(ApiError::bad_request("empty upload"));
    }
    if bytes.len() > state.cfg.max_upload_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "upload is too large",
        ));
    }
    let (mime, ext) = sniff_image(bytes).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "only PNG, JPEG, GIF, and WebP images are supported",
        )
    })?;
    let id = new_id();
    let file_name = format!("{id}.{ext}");
    crate::config::write_private_file(&state.paths.uploads_dir().join(&file_name), bytes)?;
    let (uid, sha, size) = (
        user_id.to_string(),
        sha256_hex_bytes(bytes),
        bytes.len() as i64,
    );
    let att = state
        .db
        .call(move |c| db::insert_attachment(c, &id, &uid, mime, size, &sha, &file_name))
        .await?;
    Ok(att)
}

fn sha256_hex_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::util::hex(&Sha256::digest(bytes))
}

async fn upload_attachment(
    State(state): State<AppState>,
    user: AuthUser,
    body: Bytes,
) -> ApiResult {
    let att = store_upload(&state, user.id(), &body).await?;
    ok(json!({"attachment": att}))
}

/// Uploader, or a member of a conversation containing a message that
/// references the attachment.
async fn attachment_visible(state: &AppState, user_id: &str, id: &str) -> Result<bool, ApiError> {
    if id.len() != 32 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(false);
    }
    let (uid, aid) = (user_id.to_string(), id.to_string());
    Ok(state
        .db
        .call(move |c| {
            let Some(a) = db::get_attachment(c, &aid)? else {
                return Ok(false);
            };
            if a.user_id.as_deref() == Some(uid.as_str()) {
                return Ok(true);
            }
            let pattern = format!("%\"{aid}\"%");
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages m
                    JOIN conversation_members cm ON cm.conversation_id = m.conversation_id
                    WHERE cm.user_id = ?1 AND m.attachments LIKE ?2)",
                rusqlite::params![uid, pattern],
                |r| r.get(0),
            )
        })
        .await?)
}

async fn get_attachment(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if !attachment_visible(&state, user.id(), &id).await? {
        return Err(ApiError::not_found("attachment"));
    }
    let aid = id.clone();
    let att = state
        .db
        .call(move |c| db::get_attachment(c, &aid))
        .await?
        .ok_or_else(|| ApiError::not_found("attachment"))?;
    let bytes = tokio::fs::read(state.paths.uploads_dir().join(&att.file_name))
        .await
        .map_err(|_| ApiError::not_found("attachment file"))?;
    Ok(file_response(&att.mime, bytes))
}

/// Binary response with safe headers (never rendered as HTML).
pub fn file_response(mime: &str, bytes: Vec<u8>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    (headers, bytes).into_response()
}

// ---------------------------------------------------------------- library

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<usize>,
    before: Option<i64>,
}

/// Photos the caller sent in chats (the Library's "Photos" section).
async fn list_sent_photos(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<LimitQuery>,
) -> ApiResult {
    let uid = user.id().to_string();
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let photos = state
        .db
        .call(move |c| db::sent_photos(c, &uid, limit))
        .await?;
    ok(json!({"attachments": photos}))
}

// --------------------------------------------------------------- activity

/// What Iris did recently (tool activity of finished turns). The owner also
/// sees scheduled jobs.
async fn list_activity(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<LimitQuery>,
) -> ApiResult {
    let uid = user.id().to_string();
    let owner = user.user.is_owner;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let mut items = state
        .db
        .call(move |c| db::recent_activity(c, &uid, owner, q.before, limit + 1))
        .await?;
    let has_more = items.len() > limit;
    items.truncate(limit);
    ok(json!({"items": items, "has_more": has_more}))
}

// -------------------------------------------------------------- approvals

#[derive(Deserialize)]
struct Decision {
    approve: bool,
}

/// Approve or deny an `approval` card. Only its approver may decide, once.
async fn decide_approval(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<Decision>,
) -> ApiResult {
    let mid = id.clone();
    let msg = state
        .db
        .call(move |c| db::get_message(c, &mid))
        .await?
        .ok_or_else(|| ApiError::not_found("message"))?;
    // 404 unless the caller can see the conversation.
    member_conversation(&state, &user, &msg.conversation_id)
        .await
        .map_err(|_| ApiError::not_found("message"))?;
    let card = msg
        .card
        .filter(|c| c["type"] == "approval")
        .ok_or_else(|| ApiError::bad_request("not an approval card"))?;
    if card["approver_user_id"].as_str() != Some(user.id()) {
        return Err(ApiError::forbidden(
            "only the person this request is for can decide",
        ));
    }
    let state_name = if body.approve { "approved" } else { "denied" };
    let mid = id.clone();
    // Check and update under the one database lock so two taps cannot both win.
    let updated = state
        .db
        .call(move |c| {
            let Some(current) = db::get_message(c, &mid)? else {
                return Ok(None);
            };
            let mut card = current.card.unwrap_or_default();
            if card["state"] != "pending" {
                return Ok(None);
            }
            card["state"] = state_name.into();
            card["decided_at"] = crate::util::now_ms().into();
            db::update_message_card(c, &mid, &card).map(Some)
        })
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::CONFLICT, "conflict", "this was already decided")
        })?;
    crate::login::broadcast_updated(&state, &updated).await;
    ok(json!({"message": updated}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_images() {
        assert_eq!(
            sniff_image(b"\x89PNG\r\n\x1a\nxxxx").unwrap().0,
            "image/png"
        );
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]).unwrap().1, "jpg");
        assert_eq!(sniff_image(b"GIF89a....").unwrap().0, "image/gif");
        assert_eq!(
            sniff_image(b"RIFF\0\0\0\0WEBPVP8 ").unwrap().0,
            "image/webp"
        );
        assert!(sniff_image(b"<svg onload=alert(1)>").is_none());
        assert!(sniff_image(b"").is_none());
    }
}
