//! API tests for People (each person's own Iris), Together chats, the
//! individual-use policy for other people, and voice. Turns run against a
//! stand-in `claude` that records what it was given.

use super::router;
use crate::auth;
use crate::config::{Config, Paths};
use crate::db;
use crate::state::AppState;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

struct H {
    state: AppState,
    app: Router,
    tmp: tempfile::TempDir,
}

/// A stand-in for `claude`: signed in, and every turn writes its command
/// line, working folder, and system prompt into `<dir>/turns/<n>/`.
fn fake_claude(dir: &Path) -> PathBuf {
    let script = dir.join("fake-claude");
    std::fs::create_dir_all(dir.join("turns")).unwrap();
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
case "$1 $2" in
  "--version "*) echo "9.9.9 (Fake Claude)"; exit 0;;
  "auth status") echo '{{"loggedIn": true}}'; exit 0;;
esac
n=$(ls "{turns}" | wc -l)
out="{turns}/$n"
mkdir -p "$out"
printf '%s\n' "$@" > "$out/args"
pwd > "$out/cwd"
prev=""
for a in "$@"; do
  if [ "$prev" = "--append-system-prompt-file" ]; then cp "$a" "$out/prompt"; fi
  if [ "$prev" = "--mcp-config" ]; then cp "$a" "$out/mcp"; fi
  prev="$a"
done
cat > /dev/null
echo '{{"type":"system","subtype":"init","session_id":"fake-session"}}'
echo '{{"type":"assistant","message":{{"id":"m1","role":"assistant","content":[{{"type":"text","text":"Hello from the fake."}}]}}}}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"Hello from the fake.","session_id":"fake-session"}}'
"#,
            turns = dir.join("turns").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A stand-in for the sherpa-onnx voice programs and models.
fn fake_voice(root: &Path) {
    let bin = root.join("sherpa-onnx/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(root.join("sherpa-onnx/VERSION"), "1.0.0").unwrap();
    for (c, v) in [("tts", "voice-1"), ("stt", "whisper-1")] {
        std::fs::create_dir_all(root.join(c)).unwrap();
        std::fs::write(root.join(c).join("VERSION"), v).unwrap();
    }
    // Writes one second of 16 kHz silence to --output-filename.
    let tts = r#"#!/bin/sh
for a in "$@"; do case "$a" in --output-filename=*) out="${a#--output-filename=}";; esac; done
printf 'RIFF\044\175\000\000WAVEfmt \020\000\000\000\001\000\001\000\200\076\000\000\000\175\000\000\002\000\020\000data\000\175\000\000' > "$out"
head -c 32000 /dev/zero >> "$out"
"#;
    let stt = r#"#!/bin/sh
echo "Started" >&2
echo '{"lang": "", "text": " What is the capital of France?", "tokens": []}'
"#;
    for (name, body) in [
        ("sherpa-onnx-offline-tts", tts),
        ("sherpa-onnx-offline", stt),
    ] {
        std::fs::write(bin.join(name), body).unwrap();
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

async fn setup(with_claude: bool) -> H {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_file: tmp.path().join("config.toml"),
        data_dir: tmp.path().join("data"),
    };
    crate::persona::seed(&paths).unwrap();
    let db = crate::db::Db::open(&paths.db_file()).unwrap();
    let mut cfg = Config::default();
    for h in cfg.harness.values_mut() {
        h.enabled = false;
    }
    if with_claude {
        let c = cfg.harness.get_mut("claude").unwrap();
        c.enabled = true;
        c.command = fake_claude(tmp.path()).to_string_lossy().into_owned();
    }
    let state = AppState::new(cfg, paths, db).unwrap();
    state.refresh_harnesses().await;
    H {
        app: router(state.clone()),
        state,
        tmp,
    }
}

async fn send(
    h: &H,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 4 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn send_raw(
    h: &H,
    method: &str,
    uri: &str,
    token: &str,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 4 << 20)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn pair(h: &H, user: &str) -> String {
    let u = user.to_string();
    let (code, _) = h
        .state
        .db
        .with(|c| Ok(auth::create_pairing_code(c, &u)))
        .unwrap()
        .unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/api/pair")
        .header("content-type", "application/json")
        .body(Body::from(json!({"code": code}).to_string()))
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["token"].as_str().unwrap().to_string()
}

/// Owner token, the second person's id, and their token.
async fn owner_and_sam(h: &H) -> (String, String, String) {
    let owner_id = h.state.db.with(db::owner_user_id).unwrap().unwrap();
    let owner = pair(h, &owner_id).await;
    let (s, u) = send(
        h,
        "POST",
        "/api/users",
        &owner,
        Some(json!({"display_name": "Sam"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{u}");
    let sam_id = u["user"]["id"].as_str().unwrap().to_string();
    let sam = pair(h, &sam_id).await;
    (owner, sam_id, sam)
}

/// Send a message and wait for the reply to finish.
async fn turn(h: &H, token: &str, conv: &str, text: &str) -> Value {
    let (s, b) = send(
        h,
        "POST",
        &format!("/api/conversations/{conv}/messages"),
        token,
        Some(json!({"text": text})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let id = b["assistant_message"]["id"].as_str().unwrap().to_string();
    for _ in 0..200 {
        let (_, m) = send(
            h,
            "GET",
            &format!("/api/conversations/{conv}/messages"),
            token,
            None,
        )
        .await;
        if let Some(msg) = m["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == id && x["status"] != "streaming")
        {
            return msg.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("turn never finished");
}

/// What the newest fake-claude turn received: (args, cwd, prompt, MCP config).
fn last_turn(h: &H) -> (String, String, String, String) {
    let turns = h.tmp.path().join("turns");
    let n = std::fs::read_dir(&turns).unwrap().count();
    let dir = turns.join((n - 1).to_string());
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
    (
        read("args"),
        read("cwd").trim().to_string(),
        read("prompt"),
        read("mcp"),
    )
}

async fn main_of(h: &H, token: &str) -> String {
    let (_, me) = send(h, "GET", "/api/me", token, None).await;
    me["main_conversation_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn each_person_has_their_own_iris() {
    let h = setup(false).await;
    let (owner, sam_id, sam) = owner_and_sam(&h).await;
    let dirs = h.state.paths.person_dirs(&sam_id, false);
    assert!(
        dirs.persona.join("IDENTITY.md").is_file(),
        "seeded on creation"
    );

    // Settings: name, feed prompt, voice are per person; the policy is the owner's.
    let (s, b) = send(
        &h,
        "PUT",
        "/api/settings",
        &sam,
        Some(json!({"agent_name": "Juno", "voice_replies": true})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["agent_name"], "Juno");
    assert_eq!(b["voice"]["enabled"], true);
    let (_, o) = send(&h, "GET", "/api/settings", &owner, None).await;
    assert_eq!(
        (o["agent_name"].as_str(), o["voice"]["enabled"].as_bool()),
        (Some("Iris"), Some(false))
    );
    let (s, _) = send(
        &h,
        "PUT",
        "/api/settings",
        &sam,
        Some(json!({"allow_claude_for_shared": true})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    send(
        &h,
        "PUT",
        "/api/feed/prompt",
        &sam,
        Some(json!({"feed_prompt": "Bread baking"})),
    )
    .await;
    let (_, p) = send(&h, "GET", "/api/feed/prompt", &owner, None).await;
    assert_ne!(p["feed_prompt"], "Bread baking");

    // System files are each person's own.
    send(
        &h,
        "PUT",
        "/api/files/system/MEMORY.md",
        &owner,
        Some(json!({"content": "owner-secret"})),
    )
    .await;
    send(
        &h,
        "PUT",
        "/api/files/system/MEMORY.md",
        &sam,
        Some(json!({"content": "sam-fact"})),
    )
    .await;
    let (_, m) = send(&h, "GET", "/api/files/system/MEMORY.md", &sam, None).await;
    assert_eq!(m["content"], "sam-fact");
    let (_, m) = send(&h, "GET", "/api/files/system/MEMORY.md", &owner, None).await;
    assert_eq!(m["content"], "owner-secret");
    assert_eq!(
        std::fs::read_to_string(dirs.memory.join("MEMORY.md")).unwrap(),
        "sam-fact"
    );
    let (s, _) = send(
        &h,
        "PUT",
        "/api/files/content/memory?path=notes/a.md",
        &sam,
        Some(json!({"content": "x"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(dirs.memory.join("notes/a.md").is_file());
    assert!(!h.state.paths.memory_dir().join("notes/a.md").exists());

    // Avatars: each person's own; another person's only once they share a chat.
    let png = b"\x89PNG\r\n\x1a\n0000".to_vec();
    let (s, _) = send_raw(&h, "PUT", "/api/settings/avatar", &sam, png).await;
    assert_eq!(s, StatusCode::OK);
    let (_, sb) = send(&h, "GET", "/api/settings", &sam, None).await;
    assert_eq!(sb["avatar_custom"], true);
    assert!(sb["avatar_url"].as_str().unwrap().contains(&sam_id));
    let (_, ob) = send(&h, "GET", "/api/settings", &owner, None).await;
    assert_eq!(ob["avatar_custom"], false);
    let sam_avatar = format!("/api/avatar?user={sam_id}");
    let (s, _) = send_raw(&h, "GET", &sam_avatar, &owner, Vec::new()).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "no shared chat yet");

    // Sam can start a Together chat; members report whose Iris answers.
    let owner_id = h.state.db.with(db::owner_user_id).unwrap().unwrap();
    let (s, g) = send(
        &h,
        "POST",
        "/api/conversations",
        &sam,
        Some(json!({"kind": "group", "title": "Us", "member_ids": [owner_id]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let gid = g["conversation"]["id"].as_str().unwrap().to_string();
    assert_eq!(g["conversation"]["created_by"], sam_id);
    let (_, mem) = send(
        &h,
        "GET",
        &format!("/api/conversations/{gid}/members"),
        &owner,
        None,
    )
    .await;
    assert_eq!(mem["agent"]["name"], "Juno");
    assert_eq!(mem["agent"]["avatar_custom"], true);
    let (s, bytes) = send_raw(&h, "GET", &sam_avatar, &owner, Vec::new()).await;
    assert_eq!(s, StatusCode::OK);
    assert!(bytes.starts_with(b"\x89PNG"));

    // Everyone can list people; only the owner sees device counts.
    let (_, users) = send(&h, "GET", "/api/users", &sam, None).await;
    assert_eq!(users["users"].as_array().unwrap().len(), 2);
    assert!(users["users"][0]["devices"].is_null());
    let (_, users) = send(&h, "GET", "/api/users", &owner, None).await;
    assert_eq!(users["users"][1]["devices"], 1);

    // Removing Sam deletes their Iris, devices, and the chats they made.
    let (s, _) = send(
        &h,
        "DELETE",
        &format!("/api/users/{owner_id}"),
        &owner,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = send(&h, "DELETE", &format!("/api/users/{sam_id}"), &sam, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let uploads = h.state.paths.uploads_dir();
    assert_eq!(
        std::fs::read_dir(&uploads).unwrap().count(),
        1,
        "Sam's picture"
    );
    let (s, _) = send(&h, "DELETE", &format!("/api/users/{sam_id}"), &owner, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        std::fs::read_dir(&uploads).unwrap().count(),
        0,
        "picture removed"
    );
    assert!(!dirs.persona.exists() && !dirs.memory.exists());
    let (s, _) = send(&h, "GET", "/api/me", &sam, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = send(
        &h,
        "GET",
        &format!("/api/conversations/{gid}"),
        &owner,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let left: i64 = h
        .state
        .db
        .with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM settings WHERE key LIKE 'person.%'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn turns_use_the_right_iris_and_keep_memory_private() {
    let h = setup(true).await;
    let (owner, sam_id, sam) = owner_and_sam(&h).await;
    let sam_dirs = h.state.paths.person_dirs(&sam_id, false);
    std::fs::write(
        h.state.paths.memory_dir().join("MEMORY.md"),
        "owner-secret-fact",
    )
    .unwrap();
    std::fs::write(sam_dirs.memory.join("MEMORY.md"), "sam-private-fact").unwrap();
    send(
        &h,
        "PUT",
        "/api/settings",
        &sam,
        Some(json!({"agent_name": "Juno"})),
    )
    .await;

    // The owner's own chat: Claude may answer (individual use).
    let owner_main = main_of(&h, &owner).await;
    let m = turn(&h, &owner, &owner_main, "hi").await;
    assert_eq!(m["status"], "complete", "{m}");
    let (args, cwd, prompt, _mcp) = last_turn(&h);
    assert!(prompt.contains("owner-secret-fact") && !prompt.contains("sam-private-fact"));
    assert_eq!(cwd, h.state.paths.workspace_dir().display().to_string());
    // Sam's folders and Iris's own state are off limits to the tools.
    assert!(
        args.contains(&format!("Read(/{}/**)", sam_dirs.memory.display())),
        "{args}"
    );
    assert!(args.contains(&format!(
        "Read(/{})",
        h.state.paths.vault_key_file().display()
    )));
    assert!(!args.contains(&format!(
        "Read(/{}/**)",
        h.state.paths.memory_dir().display()
    )));

    // Sam's own Iris: Claude is the owner's subscription, so it waits for the opt-in.
    let sam_main = main_of(&h, &sam).await;
    let m = turn(&h, &sam, &sam_main, "hi").await;
    assert_eq!(m["status"], "error");
    assert_eq!(m["meta"]["error"]["kind"], "paused_policy");
    assert!(
        m["content"]
            .as_str()
            .unwrap()
            .contains("only use it for the owner")
    );
    let (_, msgs) = send(
        &h,
        "GET",
        &format!("/api/conversations/{owner_main}/messages"),
        &owner,
        None,
    )
    .await;
    let card = msgs["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["card"]["type"] == "action")
        .expect("opt-in card for the owner")
        .clone();
    assert!(card["card"]["title"].as_str().unwrap().contains("Sam"));
    assert_eq!(card["card"]["owner_only"], true);

    // With the owner's opt-in, Sam's Iris answers with Sam's memory only.
    send(
        &h,
        "PUT",
        "/api/settings",
        &owner,
        Some(json!({"allow_claude_for_shared": true})),
    )
    .await;
    let m = turn(&h, &sam, &sam_main, "hi again").await;
    assert_eq!(m["status"], "complete", "{m}");
    let (args, cwd, prompt, _mcp) = last_turn(&h);
    assert!(prompt.contains("sam-private-fact") && !prompt.contains("owner-secret-fact"));
    assert!(prompt.contains("# You are Juno") && prompt.contains("called Sam"));
    assert_eq!(cwd, sam_dirs.workspace.display().to_string());
    assert!(args.contains(&format!(
        "Read(/{}/**)",
        h.state.paths.memory_dir().display()
    )));
    assert!(!args.contains(&format!("Read(/{}/**)", sam_dirs.memory.display())));
    assert!(args.contains(&format!("--add-dir\n{}", sam_dirs.memory.display())));

    // A Together chat: nobody's private files, its own folder, cards only.
    let owner_id = h.state.db.with(db::owner_user_id).unwrap().unwrap();
    let (_, g) = send(
        &h,
        "POST",
        "/api/conversations",
        &owner,
        Some(json!({"kind": "group", "title": "Us", "member_ids": [sam_id]})),
    )
    .await;
    let gid = g["conversation"]["id"].as_str().unwrap().to_string();
    let m = turn(&h, &sam, &gid, "hello both").await;
    assert_eq!(m["status"], "complete", "{m}");
    let (args, cwd, prompt, mcp) = last_turn(&h);
    assert!(!prompt.contains("owner-secret-fact") && !prompt.contains("sam-private-fact"));
    assert!(prompt.contains("group conversation with: Owner, Sam"));
    assert_eq!(cwd, h.state.paths.together_dir(&gid).display().to_string());
    for d in [h.state.paths.memory_dir(), sam_dirs.memory.clone()] {
        assert!(
            args.contains(&format!("Read(/{}/**)", d.display())),
            "{args}"
        );
    }
    assert!(
        !args.contains("--add-dir"),
        "no writable memory in shared chats"
    );
    let mcp: Value = serde_json::from_str(&mcp).unwrap();
    let bridge = mcp["mcpServers"]["iris"]["args"].to_string();
    assert!(
        bridge.contains("--shared") && bridge.contains(&owner_id),
        "{bridge}"
    );
}

#[tokio::test]
async fn voice_routes() {
    let h = setup(false).await;
    let (owner, _sam_id, sam) = owner_and_sam(&h).await;
    let main = main_of(&h, &owner).await;
    let reply = h
        .state
        .db
        .with(|c| {
            db::insert_message(
                c,
                &db::NewMessage {
                    conversation_id: &main,
                    role: "assistant",
                    content: "**Paris** is the capital. See https://example.com",
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let speak = json!({"message_id": reply.id}).to_string().into_bytes();
    // Not installed: an honest 503 the app falls back from.
    let (s, b) = send_raw(&h, "POST", "/api/voice/speak", &owner, speak.clone()).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert!(String::from_utf8_lossy(&b).contains("voice_unavailable"));
    let (_, st) = send(&h, "GET", "/api/settings", &owner, None).await;
    assert_eq!(
        (st["voice"]["tts"].as_bool(), st["voice"]["stt"].as_bool()),
        (Some(false), Some(false))
    );

    fake_voice(&h.state.cfg.engines_dir(&h.state.paths));
    let (_, st) = send(&h, "GET", "/api/settings", &owner, None).await;
    assert_eq!(
        (st["voice"]["tts"].as_bool(), st["voice"]["stt"].as_bool()),
        (Some(true), Some(true))
    );
    let (s, wav) = send_raw(&h, "POST", "/api/voice/speak", &owner, speak.clone()).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&wav));
    assert_eq!(crate::voice::wav_info(&wav).unwrap().duration_ms, 1000);
    let cached = std::fs::read_dir(h.state.paths.voice_dir())
        .unwrap()
        .count();
    assert_eq!(cached, 1);
    let (s, _) = send_raw(&h, "POST", "/api/voice/speak", &owner, speak.clone()).await;
    assert_eq!(s, StatusCode::OK);
    // Other people cannot hear replies from chats they are not in.
    let (s, _) = send_raw(&h, "POST", "/api/voice/speak", &sam, speak).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Transcription takes WAV only.
    let (s, _) = send_raw(
        &h,
        "POST",
        "/api/voice/transcribe",
        &sam,
        b"not audio".to_vec(),
    )
    .await;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let (s, b) = send_raw(&h, "POST", "/api/voice/transcribe", &sam, wav).await;
    assert_eq!(s, StatusCode::OK);
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["text"], "What is the capital of France?");
    assert_eq!(v["duration_ms"], 1000);
    let leftovers = std::fs::read_dir(h.state.paths.run_dir()).unwrap().count();
    assert_eq!(leftovers, 0, "recordings are deleted after transcription");
}
