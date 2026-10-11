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

/// Deliver held notifications for users whose quiet hours are over.
/// Called by the minute scheduler.
pub async fn flush_quiet(state: &AppState) {
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
        let (held, reads) = state
            .db
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT payload, created_at FROM push_queue WHERE user_id = ?1 ORDER BY created_at",
                )?;
                let held: Vec<(Value, i64)> = stmt
                    .query_map([&uid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                    .filter_map(|r| r.ok())
                    .filter_map(|(p, at)| serde_json::from_str(&p).ok().map(|v| (v, at)))
                    .collect();
                let mut stmt = c.prepare(
                    "SELECT conversation_id, COALESCE(last_read_at, 0) FROM conversation_members WHERE user_id = ?1",
                )?;
                let reads: std::collections::HashMap<String, i64> = stmt
                    .query_map([&uid], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .filter_map(|r| r.ok())
                    .collect();
                c.execute("DELETE FROM push_queue WHERE user_id = ?1", [&uid])?;
                Ok((held, reads))
            })
            .await
            .unwrap_or_default();
        let name = state.agent_name().await;
        let read = |conv: &str, at: i64| reads.get(conv).is_some_and(|r| *r >= at);
        if let Some(payload) = digest(&name, &held, &read) {
            crate::info!(
                "delivering {} held notification(s) after quiet hours",
                held.len()
            );
            let _ = push_users_now(state, std::slice::from_ref(&u), &payload).await;
        }
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
    let Some(keys) = state.vapid.clone() else {
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
        let (keys, subject, payload) = (
            keys.clone(),
            state.cfg.push.vapid_subject.clone(),
            payload.clone(),
        );
        let (endpoint, p256dh, auth) = (sub.endpoint.clone(), sub.p256dh.clone(), sub.auth.clone());
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
}
