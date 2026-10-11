//! Voice routes: a reply as a voice note (`POST /voice/speak`) and voice
//! input (`POST /voice/transcribe`). Both run on this server with the
//! local voice components (`crate::voice`); audio never leaves it.

use super::chat::member_conversation;
use super::{ApiError, AuthUser, JsonBody};
use crate::db::{self, Message};
use crate::state::AppState;
use crate::util::new_id;
use crate::voice::{self, Component};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

/// Longest voice message accepted for transcription.
const MAX_INPUT_MS: u64 = 5 * 60 * 1000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/voice/speak", post(speak))
        .route("/voice/transcribe", post(transcribe))
}

fn unavailable(what: &str) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "voice_unavailable",
        format!("{what} is not set up on this server"),
    )
}

#[derive(Deserialize)]
struct SpeakBody {
    message_id: String,
}

/// A finished reply as a voice note (WAV), synthesized once and cached.
async fn speak(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(body): JsonBody<SpeakBody>,
) -> Result<Response, ApiError> {
    let id = body.message_id.clone();
    let msg = state
        .db
        .call(move |c| db::get_message(c, &id))
        .await?
        .ok_or_else(|| ApiError::not_found("message"))?;
    member_conversation(&state, &user, &msg.conversation_id)
        .await
        .map_err(|_| ApiError::not_found("message"))?;
    if msg.role != "assistant" || msg.kind != "text" || msg.status == "streaming" {
        return Err(ApiError::bad_request("only finished replies can be spoken"));
    }
    let root = state.cfg.engines_dir(&state.paths);
    if !voice::installed(&root, Component::Tts) {
        return Err(unavailable("voice replies"));
    }
    let (bytes, duration_ms) = voice_note(&state, &msg).await?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    if let Ok(v) = HeaderValue::from_str(&duration_ms.to_string()) {
        headers.insert("x-voice-duration-ms", v);
    }
    Ok((headers, bytes).into_response())
}

fn cache_path(state: &AppState, msg: &Message, text: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let h = crate::util::hex(&Sha256::digest(text.as_bytes())[..6]);
    state.paths.voice_dir().join(format!("{}-{h}.wav", msg.id))
}

/// The voice note for a reply: from the cache, or synthesized now (one
/// synthesis at a time on the server). Returns the WAV and its length.
pub async fn voice_note(state: &AppState, msg: &Message) -> Result<(Vec<u8>, u64), ApiError> {
    let text = voice::speech_text(&msg.content);
    if text.is_empty() {
        return Err(ApiError::bad_request("this reply has nothing to say aloud"));
    }
    let path = cache_path(state, msg, &text);
    let read = |p: PathBuf| async move {
        let bytes = tokio::fs::read(&p).await.ok()?;
        let ms = voice::wav_info(&bytes)?.duration_ms;
        Some((bytes, ms))
    };
    if let Some(hit) = read(path.clone()).await {
        return Ok(hit);
    }
    // A bounded number of requests may wait (duplicates included); the
    // same note is made once.
    let _admitted = Waiting::enter_tts(state).ok_or_else(busy)?;
    let _pending = Pending::enter(state, &msg.id).ok_or_else(busy)?;
    let _permit = tokio::time::timeout(MAX_WAIT, state.tts_jobs.acquire())
        .await
        .map_err(|_| busy())?
        .map_err(|_| busy())?;
    if let Some(hit) = read(path.clone()).await {
        return Ok(hit);
    }
    crate::config::ensure_private_dir(&state.paths.voice_dir())?;
    // Named after the message so removing its person also finds it, and
    // deleted however this ends (including a dropped request).
    let tmp = TempFile(
        state
            .paths
            .voice_dir()
            .join(format!(".{}-{}.tmp.wav", msg.id, new_id())),
    );
    let root = state.cfg.engines_dir(&state.paths);
    let started = std::time::Instant::now();
    let res = voice::synthesize(&root, &text, &tmp.0).await;
    if let Err(e) = res {
        crate::warn!("voice note for {} failed: {e}", msg.id);
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "voice_failed",
            "I couldn't make a voice note for that one.",
        ));
    }
    tokio::fs::rename(&tmp.0, &path)
        .await
        .map_err(|e| ApiError::from(crate::util::BoxError::from(format!("voice cache: {e}"))))?;
    // The message may have been deleted (its person removed) while this
    // note was being made: then the note must not stay behind.
    let id = msg.id.clone();
    let still_there = state
        .db
        .call(move |c| db::get_message(c, &id))
        .await
        .ok()
        .flatten()
        .is_some();
    if !still_there {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(ApiError::not_found("message"));
    }
    let out = read(path)
        .await
        .ok_or_else(|| ApiError::from(crate::util::BoxError::from("voice note unreadable")))?;
    crate::info!(
        "voice note for {}: {} chars, {} ms audio in {} ms",
        msg.id,
        text.chars().count(),
        out.1,
        started.elapsed().as_millis()
    );
    Ok(out)
}

/// Delete the cached voice notes of these messages (their chats were
/// deleted). Notes are named `<message id>-<hash>.wav`.
pub async fn forget_notes(state: &AppState, message_ids: &[String]) {
    if message_ids.is_empty() {
        return;
    }
    let ids: std::collections::HashSet<&str> = message_ids.iter().map(String::as_str).collect();
    let Ok(mut rd) = tokio::fs::read_dir(state.paths.voice_dir()).await else {
        return;
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        if name
            .trim_start_matches('.')
            .split_once('-')
            .is_some_and(|(id, _)| ids.contains(id))
        {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
}

/// After a reply: if anyone in the conversation wants voice replies and
/// the server can speak, make the voice note now so it plays instantly.
pub fn prepare_for(state: &AppState, members: &[String], msg: &Message) {
    if msg.role != "assistant" || msg.kind != "text" || msg.content.trim().is_empty() {
        return;
    }
    let root = state.cfg.engines_dir(&state.paths);
    if !voice::installed(&root, Component::Tts) {
        return;
    }
    let (st, members, msg) = (state.clone(), members.to_vec(), msg.clone());
    tokio::spawn(async move {
        let mut wanted = false;
        for m in &members {
            if let Some(u) = st.user(m).await
                && st.voice_replies_for(&u).await
            {
                wanted = true;
                break;
            }
        }
        // Ahead-of-time notes are a nicety: skip them when busy.
        let full = st
            .tts_pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
            >= MAX_TTS_PENDING;
        if wanted && !full {
            let _ = voice_note(&st, &msg).await;
        }
    });
}

/// Voice notes allowed to be waiting or in progress at once, and voice-note
/// requests (several can wait for the same note).
const MAX_TTS_PENDING: usize = 4;
const MAX_TTS_REQUESTS: usize = 8;

/// A message's place among the voice notes being made. Entering for a
/// message already there is allowed (it waits, then finds the cache).
struct Pending<'a> {
    state: &'a AppState,
    id: Option<String>,
}

impl<'a> Pending<'a> {
    fn enter(state: &'a AppState, id: &str) -> Option<Self> {
        let mut set = state.tts_pending.lock().unwrap_or_else(|p| p.into_inner());
        if set.contains(id) {
            return Some(Self { state, id: None });
        }
        if set.len() >= MAX_TTS_PENDING {
            return None;
        }
        set.insert(id.to_string());
        Some(Self {
            state,
            id: Some(id.to_string()),
        })
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            self.state
                .tts_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(id);
        }
    }
}

/// Recordings allowed to wait for the voice program, and for how long.
const MAX_WAITING: usize = 3;
const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

fn busy() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "voice_busy",
        "I'm still busy with other voice messages. Try again in a moment.",
    )
}

/// A place in the bounded queue of waiting recordings.
struct Waiting<'a>(&'a std::sync::atomic::AtomicUsize);

impl<'a> Waiting<'a> {
    fn enter(state: &'a AppState) -> Option<Self> {
        Self::enter_counter(&state.voice_waiting, MAX_WAITING)
    }

    /// Voice-note requests waiting or in progress, duplicates included.
    fn enter_tts(state: &'a AppState) -> Option<Self> {
        Self::enter_counter(&state.tts_waiting, MAX_TTS_REQUESTS)
    }

    fn enter_counter(n: &'a std::sync::atomic::AtomicUsize, max: usize) -> Option<Self> {
        use std::sync::atomic::Ordering;
        n.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            (v < max).then_some(v + 1)
        })
        .ok()
        .map(|_| Self(n))
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A file deleted when dropped.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Voice input: a WAV recording (16-bit PCM, any rate; the app records
/// and converts on the phone) in, text out.
async fn transcribe(
    State(state): State<AppState>,
    _user: AuthUser,
    body: axum::body::Body,
) -> Result<Response, ApiError> {
    let root = state.cfg.engines_dir(&state.paths);
    if !voice::installed(&root, Component::Stt) {
        return Err(unavailable("voice input"));
    }
    // Admission before the upload is read: only a few recordings may wait
    // for the voice program, so memory stays bounded.
    let _slot = Waiting::enter(&state).ok_or_else(busy)?;
    let too_long = || {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "that recording is too long",
        )
    };
    let body = axum::body::to_bytes(body, state.cfg.max_upload_bytes)
        .await
        .map_err(|_| too_long())?;
    let info = voice::wav_info(&body).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "send the recording as WAV (16-bit PCM)",
        )
    })?;
    if info.duration_ms > MAX_INPUT_MS {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "voice messages can be up to 5 minutes",
        ));
    }
    // Wait for a free slot before writing anything, and delete the
    // recording however the request ends (including a dropped connection).
    let _permit = tokio::time::timeout(MAX_WAIT, state.voice_jobs.acquire())
        .await
        .map_err(|_| busy())?
        .map_err(|_| busy())?;
    let file = TempFile(
        state
            .paths
            .run_dir()
            .join(format!("{}.voice.wav", new_id())),
    );
    crate::config::write_private_file(&file.0, &body)?;
    let res = voice::transcribe(&root, &file.0).await;
    drop(file);
    match res {
        Ok(text) => {
            crate::info!(
                "transcribed {} ms of audio ({} chars)",
                info.duration_ms,
                text.chars().count()
            );
            Ok(axum::Json(json!({"text": text, "duration_ms": info.duration_ms})).into_response())
        }
        Err(e) => {
            crate::warn!("transcription failed: {e}");
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "voice_failed",
                "I couldn't make out that recording.",
            ))
        }
    }
}
