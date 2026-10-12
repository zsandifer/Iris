//! Fan-out of Web Push notifications to a user's subscribed devices.
//! Delivery runs on the blocking pool and never delays the caller.
//!
//! Quiet hours delay notifications instead of dropping them: they wait in
//! `push_queue` and go out as one digest when the quiet hours end.

use crate::entities;
use crate::push::{SendResult, send_blocking};
use crate::state::AppState;
use crate::util::{new_id, now_ms, truncate_chars};
use rusqlite::params;
use serde_json::{Value, json};

/// Most notifications kept per user during quiet hours (oldest dropped).
const MAX_QUEUED: i64 = 50;

/// Push `payload` to every subscription of `users` (fire and forget).
/// Users inside their quiet hours get it later, in a digest.
pub fn push_users(state: &AppState, users: &[String], payload: Value) {
    if !state.cfg.push.enabled || state.vapid.is_none() || users.is_empty() {
        return;
    }
    let state = state.clone();
    let users = users.to_vec();
    tokio::spawn(async move {
        let mut awake = Vec::with_capacity(users.len());
        for u in users {
            if in_quiet_hours(&state, &u).await {
                enqueue(&state, &u, &payload).await;
            } else {
                awake.push(u);
            }
        }
        if !awake.is_empty() {
            let _ = push_users_now(&state, &awake, &payload).await;
        }
    });
}

async fn enqueue(state: &AppState, user_id: &str, payload: &Value) {
    let (uid, raw) = (user_id.to_string(), payload.to_string());
    let _ = state
        .db
        .call(move |c| {
            c.execute(
                "INSERT INTO push_queue (id, user_id, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![new_id(), uid, raw, now_ms()],
            )?;
            c.execute(
                "DELETE FROM push_queue WHERE user_id = ?1 AND id NOT IN (
                     SELECT id FROM push_queue WHERE user_id = ?1
                     ORDER BY created_at DESC LIMIT ?2)",
                params![uid, MAX_QUEUED],
            )?;
            Ok(())
        })
        .await;
}

/// One notification summing up what was held during quiet hours. Items
/// about chats the user has read since are left out. None when nothing is
/// left.
pub fn digest(
    agent_name: &str,
    held: &[(Value, i64)],
    read: &dyn Fn(&str, i64) -> bool,
) -> Option<Value> {
    let fresh: Vec<&Value> = held
        .iter()
        .filter(|(p, at)| p["conversation_id"].as_str().is_none_or(|c| !read(c, *at)))
        .map(|(p, _)| p)
        .collect();
    match fresh.as_slice() {
        [] => None,
        [one] => {
            let mut p = (*one).clone();
            p["digest"] = json!(true);
            Some(p)
        }
        many => {
            let lines: Vec<String> = many
                .iter()
                .rev()
                .take(3)
                .map(|p| {
                    let b = p["body"].as_str().unwrap_or("");
                    let t = p["title"].as_str().unwrap_or("");
                    truncate_chars(if b.is_empty() { t } else { b }, 70)
                })
                .collect();
            let chats: std::collections::BTreeSet<&str> = many
                .iter()
                .filter_map(|p| p["conversation_id"].as_str())
                .collect();
            let mut out = json!({
                "title": agent_name,
                "body": format!("While it was quiet: {} updates. {}", many.len(), lines.join(" · ")),
                "kind": "digest",
                "count": many.len(),
                "digest": true,
            });
            // Open the one chat they all came from, else the app.
            if chats.len() == 1 && many.iter().all(|p| p["conversation_id"].is_string()) {
                out["conversation_id"] = json!(chats.into_iter().next());
            } else if many.iter().all(|p| p["url"].is_string()) {
                out["url"] = many[0]["url"].clone();
            }
            Some(out)
        }
    }
}

static FLUSHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Deliver held notifications in the background (called by the minute
/// scheduler; never delays it, never runs twice at once).
pub fn spawn_flush_quiet(state: &AppState) {
    use std::sync::atomic::Ordering;
    if FLUSHING.swap(true, Ordering::SeqCst) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        flush_quiet(&state).await;
        FLUSHING.store(false, Ordering::SeqCst);
    });
}

/// Held notifications older than this are dropped if they still cannot be
/// delivered (the push service keeps failing).
const MAX_HOLD_MS: i64 = 48 * 3_600_000;

/// Deliver held notifications for users whose quiet hours are over. Rows
/// are removed only once the digest was delivered (or the user has no
/// device to deliver to); a failed delivery is retried on the next tick.
pub async fn flush_quiet(state: &AppState) {
    // Push turned off (or no keys): keep everything for when it is back.
    if !state.cfg.push.enabled || state.vapid.is_none() {
        return;
    }
    let users: Vec<String> = state
        .db
        .call(|c| {
            let mut stmt = c.prepare("SELECT DISTINCT user_id FROM push_queue")?;
            stmt.query_map([], |r| r.get(0))?.collect()
        })
        .await
        .unwrap_or_default();
    for u in users {
        if in_quiet_hours(state, &u).await {
            continue;
        }
        let uid = u.clone();
        let Ok((held, reads, devices)) = state
            .db
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT id, payload, created_at FROM push_queue WHERE user_id = ?1 ORDER BY created_at",
                )?;
                let held: Vec<(String, Value, i64)> = stmt
                    .query_map([&uid], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                    })?
                    .filter_map(|r| r.ok())
                    .map(|(id, p, at)| (id, serde_json::from_str(&p).unwrap_or(Value::Null), at))
                    .collect();
                let mut stmt = c.prepare(
                    "SELECT conversation_id, COALESCE(last_read_at, 0) FROM conversation_members WHERE user_id = ?1",
                )?;
                let reads: std::collections::HashMap<String, i64> = stmt
                    .query_map([&uid], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .filter_map(|r| r.ok())
                    .collect();
                let devices = entities::list_push_subscriptions(c, &uid)?.len();
                Ok((held, reads, devices))
            })
            .await
        else {
            continue;
        };
        if held.is_empty() {
            continue;
        }
        let name = state.agent_name().await;
        let read = |conv: &str, at: i64| reads.get(conv).is_some_and(|r| *r >= at);
        // Chats the user has left since (or was removed from) are dropped:
        // no previews after access ends.
        let keep = |p: &Value| {
            p.is_object()
                && p["conversation_id"]
                    .as_str()
                    .is_none_or(|c| reads.contains_key(c))
        };
        let items: Vec<(Value, i64)> = held
            .iter()
            .filter(|(_, p, _)| keep(p))
            .map(|(_, p, at)| (p.clone(), *at))
            .collect();
        let done = match digest(&name, &items, &read) {
            None => true,
            Some(_) if devices == 0 => true,
            Some(payload) => {
                let (delivered, _) =
                    push_users_now(state, std::slice::from_ref(&u), &payload).await;
                if delivered > 0 {
                    crate::info!(
                        "delivered {} held notification(s) after quiet hours",
                        held.len()
                    );
                    true
                } else {
                    crate::warn!("held notifications not delivered yet; will retry");
                    false
                }
            }
        };
        let now = now_ms();
        let ids: Vec<String> = held
            .iter()
            .filter(|(_, p, at)| done || !keep(p) || now - at > MAX_HOLD_MS)
            .map(|(id, _, _)| id.clone())
            .collect();
        let _ = state
            .db
            .call(move |c| {
                for id in &ids {
                    c.execute("DELETE FROM push_queue WHERE id = ?1", [id])?;
                }
                Ok(())
            })
            .await;
    }
}

/// Settings key holding a user's quiet hours.
pub fn quiet_hours_key(user_id: &str) -> String {
    format!("quiet_hours:{user_id}")
}

/// A user's quiet hours, `{"start": "HH:MM", "end": "HH:MM"}` in the
/// server's local time, or `None`.
pub async fn quiet_hours(state: &AppState, user_id: &str) -> Option<Value> {
    let key = quiet_hours_key(user_id);
    let raw = state
        .db
        .call(move |c| crate::db::get_setting(c, &key))
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

pub async fn in_quiet_hours(state: &AppState, user_id: &str) -> bool {
    let Some(q) = quiet_hours(state, user_id).await else {
        return false;
    };
    let parse = |k: &str| q[k].as_str().and_then(crate::config::parse_hhmm);
    match (parse("start"), parse("end")) {
        (Some(start), Some(end)) if start != end => {
            crate::jobs::in_window(crate::jobs::local_minutes_now(), start, end)
        }
        _ => false,
    }
}

/// Push and wait; returns (delivered, failed) counts.
pub async fn push_users_now(state: &AppState, users: &[String], payload: &Value) -> (usize, usize) {
    let (Some(keys), Some(subject)) = (state.vapid.clone(), state.cfg.push_subject()) else {
        return (0, 0);
    };
    let users_q = users.to_vec();
    let subs = state
        .db
        .call(move |c| {
            let mut all = Vec::new();
            for u in &users_q {
                all.extend(entities::list_push_subscriptions(c, u)?);
            }
            Ok(all)
        })
        .await
        .unwrap_or_default();
    let (mut delivered, mut failed) = (0, 0);
    for sub in subs {
        let (keys, subject, payload) = (keys.clone(), subject.clone(), payload.clone());
        let (endpoint, p256dh, auth) = (sub.endpoint.clone(), sub.p256dh.clone(), sub.auth.clone());
        #[cfg(test)]
        if let Some(outcome) = test_sink::take(&endpoint, &payload) {
            if outcome {
                delivered += 1;
            } else {
                failed += 1;
            }
            continue;
        }
        let res = tokio::task::spawn_blocking(move || {
            send_blocking(&keys, &subject, &endpoint, &p256dh, &auth, &payload)
        })
        .await;
        let id = sub.id.clone();
        match res {
            Ok(Ok(SendResult::Delivered)) => {
                delivered += 1;
                let _ = state
                    .db
                    .call(move |c| entities::record_push_result(c, &id, true))
                    .await;
            }
            Ok(Ok(SendResult::Expired)) => {
                failed += 1;
                let _ = state
                    .db
                    .call(move |c| entities::delete_push_by_id(c, &id))
                    .await;
            }
            Ok(Ok(SendResult::Failed(status))) => {
                failed += 1;
                crate::warn!("push to subscription {} failed with HTTP {status}", sub.id);
                let _ = state
                    .db
                    .call(move |c| entities::record_push_result(c, &id, false))
                    .await;
            }
            Ok(Err(e)) => {
                failed += 1;
                crate::warn!("push to subscription {} failed: {e}", sub.id);
                let _ = state
                    .db
                    .call(move |c| entities::record_push_result(c, &id, false))
                    .await;
            }
            Err(_) => failed += 1,
        }
    }
    (delivered, failed)
}

/// Tests record pushes to `https://sink.test/<name>/<ok|fail>` instead of
/// sending them.
#[cfg(test)]
pub mod test_sink {
    use serde_json::Value;
    use std::sync::Mutex;

    static SENT: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());

    pub fn take(endpoint: &str, payload: &Value) -> Option<bool> {
        let rest = endpoint.strip_prefix("https://sink.test/")?;
        SENT.lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((endpoint.to_string(), payload.clone()));
        Some(rest.ends_with("/ok"))
    }

    pub fn sent(prefix: &str) -> Vec<Value> {
        SENT.lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(e, _)| e.starts_with(prefix))
            .map(|(_, p)| p.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests() {
        let msg = |c: &str, b: &str| json!({"title": "Iris", "body": b, "conversation_id": c, "kind": "reply"});
        let none = |_: &str, _: i64| false;
        assert_eq!(digest("Iris", &[], &none), None);
        let one = digest("Iris", &[(msg("c1", "Hello"), 10)], &none).unwrap();
        assert_eq!(
            (one["body"].as_str(), one["digest"].as_bool()),
            (Some("Hello"), Some(true))
        );

        let held = vec![
            (msg("c1", "First"), 10),
            (msg("c1", "Second"), 20),
            (msg("c1", "Third"), 30),
        ];
        let d = digest("Iris", &held, &none).unwrap();
        assert_eq!(d["count"], 3);
        assert_eq!(d["conversation_id"], "c1");
        assert!(
            d["body"]
                .as_str()
                .unwrap()
                .starts_with("While it was quiet: 3 updates. Third")
        );

        // Chats read after the notification was held are skipped.
        let read = |c: &str, at: i64| c == "c1" && at <= 20;
        let d = digest("Iris", &held, &read).unwrap();
        assert_eq!(d["body"], "Third");

        let mixed = vec![
            (msg("c1", "Reply"), 10),
            (
                json!({"title": "Price drop", "body": "Shoe is $80", "kind": "feed_urgent", "url": "/#/foryou"}),
                20,
            ),
        ];
        let d = digest("Iris", &mixed, &none).unwrap();
        assert!(d.get("conversation_id").is_none() && d.get("url").is_none());
        assert_eq!(d["kind"], "digest");
    }

    fn state_with_push() -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = crate::config::Config::default();
        for h in cfg.harness.values_mut() {
            h.enabled = false;
        }
        cfg.feed.enabled = false;
        cfg.push.vapid_subject = "mailto:admin@example.com".into();
        (AppState::new(cfg, paths, db).unwrap(), tmp)
    }

    fn queued(state: &AppState) -> i64 {
        state
            .db
            .with(|c| c.query_row("SELECT COUNT(*) FROM push_queue", [], |r| r.get(0)))
            .unwrap()
    }

    #[tokio::test]
    async fn digest_content_and_delivery() {
        let (state, _tmp) = state_with_push();
        let owner = state.db.with(crate::db::owner_user_id).unwrap().unwrap();
        let (main, other) = state
            .db
            .with(|c| {
                let main = crate::db::ensure_main_conversation(c, &owner)?;
                let conv = crate::db::create_conversation(c, &owner, "side", "x", None)?;
                c.execute(
                    "DELETE FROM conversation_members WHERE conversation_id = ?1",
                    [&conv.id],
                )?;
                Ok((main, conv.id))
            })
            .unwrap();
        let add_device = |endpoint: &str| {
            state
                .db
                .with(|c| {
                    c.execute("DELETE FROM push_subscriptions", [])?;
                    let d = crate::db::insert_device(c, &owner, "phone", &crate::util::new_id())?;
                    entities::upsert_push_subscription(c, &owner, &d.id, endpoint, "k", "a")
                })
                .unwrap();
        };
        let held = |body: &str, conv: &str| json!({"title": "Iris", "body": body, "conversation_id": conv});

        // The push service fails: nothing is lost.
        add_device("https://sink.test/digest/fail");
        enqueue(&state, &owner, &held("secret", &other)).await;
        enqueue(&state, &owner, &held("hello", &main)).await;
        enqueue(&state, &owner, &held("again", &main)).await;
        flush_quiet(&state).await;
        assert_eq!(
            queued(&state),
            2,
            "member items kept; the other chat's dropped"
        );

        // It works now: one digest, without the other chat's text.
        add_device("https://sink.test/digest/ok");
        flush_quiet(&state).await;
        assert_eq!(queued(&state), 0);
        let sent = test_sink::sent("https://sink.test/digest/");
        assert!(
            sent.iter().all(|p| !p.to_string().contains("secret")),
            "{sent:?}"
        );
        let last = sent.last().unwrap();
        assert_eq!(last["count"], 2);
        assert_eq!(last["conversation_id"], json!(main));
    }

    #[tokio::test]
    async fn queue_is_kept_until_delivered() {
        let (state, _tmp) = state_with_push();
        let owner = state.db.with(crate::db::owner_user_id).unwrap().unwrap();
        // Quiet now: held, not sent.
        let now = crate::jobs::local_minutes_now();
        let hhmm = |m: u32| format!("{:02}:{:02}", (m % 1440) / 60, m % 60);
        let quiet = json!({"start": hhmm(now + 1440 - 5), "end": hhmm(now + 5)}).to_string();
        let key = quiet_hours_key(&owner);
        state
            .db
            .with(|c| crate::db::set_setting(c, &key, &quiet))
            .unwrap();
        enqueue(
            &state,
            &owner,
            &json!({"title": "Iris", "body": "Hello", "kind": "reply"}),
        )
        .await;
        flush_quiet(&state).await;
        assert_eq!(queued(&state), 1, "still quiet");

        // Quiet hours over, but the only device's push service is down: kept.
        state
            .db
            .with(|c| crate::db::delete_setting(c, &key))
            .unwrap();
        state
            .db
            .with(|c| {
                let d = crate::db::insert_device(c, &owner, "phone", "hash-1")?;
                entities::upsert_push_subscription(
                    c,
                    &owner,
                    &d.id,
                    "https://127.0.0.1:9/push",
                    "bad",
                    "bad",
                )
            })
            .unwrap();
        flush_quiet(&state).await;
        assert_eq!(queued(&state), 1, "retried on the next tick");

        // No device left to deliver to: nothing to wait for.
        state
            .db
            .with(|c| c.execute("DELETE FROM push_subscriptions", []))
            .unwrap();
        flush_quiet(&state).await;
        assert_eq!(queued(&state), 0);

        // A reply from a chat the user is no longer in is never delivered.
        let other = state
            .db
            .with(|c| {
                let conv = crate::db::create_conversation(c, &owner, "side", "x", None)?;
                c.execute(
                    "DELETE FROM conversation_members WHERE conversation_id = ?1",
                    [&conv.id],
                )?;
                Ok(conv.id)
            })
            .unwrap();
        // With a (failing) device again: the member chat's reply is kept
        // for a retry, the other one is dropped without a push attempt.
        state
            .db
            .with(|c| {
                let d = crate::db::insert_device(c, &owner, "phone2", "hash-2")?;
                entities::upsert_push_subscription(
                    c,
                    &owner,
                    &d.id,
                    "https://127.0.0.1:9/push2",
                    "bad",
                    "bad",
                )
            })
            .unwrap();
        let main = state
            .db
            .with(|c| crate::db::ensure_main_conversation(c, &owner))
            .unwrap();
        let gone = json!({"title": "Iris", "body": "secret", "conversation_id": other});
        let mine = json!({"title": "Iris", "body": "hello", "conversation_id": main});
        enqueue(&state, &owner, &gone).await;
        enqueue(&state, &owner, &mine).await;
        flush_quiet(&state).await;
        let left: Vec<String> = state
            .db
            .with(|c| {
                let mut st = c.prepare("SELECT payload FROM push_queue")?;
                st.query_map([], |r| r.get(0))?.collect()
            })
            .unwrap();
        assert_eq!(left.len(), 1);
        assert!(left[0].contains("hello") && !left[0].contains("secret"));
    }
}
