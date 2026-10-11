//! Fan-out of Web Push notifications to a user's subscribed devices.
//! Delivery runs on the blocking pool and never delays the caller.

use crate::entities;
use crate::push::{SendResult, send_blocking};
use crate::state::AppState;
use serde_json::Value;

/// Push `payload` to every subscription of `users` (fire and forget).
/// Users inside their quiet hours are skipped (the message is still in the
/// app; only the notification is dropped).
pub fn push_users(state: &AppState, users: &[String], payload: Value) {
    if !state.cfg.push.enabled || state.vapid.is_none() || users.is_empty() {
        return;
    }
    let state = state.clone();
    let users = users.to_vec();
    tokio::spawn(async move {
        let mut awake = Vec::with_capacity(users.len());
        for u in users {
            if !in_quiet_hours(&state, &u).await {
                awake.push(u);
            }
        }
        if !awake.is_empty() {
            let _ = push_users_now(&state, &awake, &payload).await;
        }
    });
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

async fn in_quiet_hours(state: &AppState, user_id: &str) -> bool {
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
