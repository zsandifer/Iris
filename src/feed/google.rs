//! The token interface between the Google connector and the feed.
//!
//! When the user has connected Google, the connector keeps a short-lived
//! OAuth access token (scope `youtube.readonly`) for them, sealed with the
//! vault, in the setting `connector_token.google:<user id>` as
//! `{"access_token": "...", "expires_at": <ms>}`. The feed only reads it to
//! list YouTube subscriptions and liked videos; it never logs or returns
//! it. Without a token (Google not connected, or expired) the feed simply
//! uses the YouTube channels in the user's sources.

use crate::state::AppState;
use crate::util::now_ms;

pub fn token_key(user_id: &str) -> String {
    format!("connector_token.google:{user_id}")
}

/// The user's current Google access token, if one is stored and valid for
/// at least another minute.
pub async fn access_token(state: &AppState, user_id: &str) -> Option<String> {
    let key = token_key(user_id);
    let k = key.clone();
    let sealed = state
        .db
        .call(move |c| crate::db::get_setting(c, &k))
        .await
        .ok()
        .flatten()?;
    let raw = state.vault.open_str(&sealed, &key).ok()?;
    parse_token(&raw, now_ms())
}

fn parse_token(raw: &str, now: i64) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let token = v["access_token"].as_str().filter(|t| !t.is_empty())?;
    let expires = v["expires_at"].as_i64().unwrap_or(0);
    (expires > now + 60_000).then(|| token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_expiry() {
        let now = 1_000_000;
        assert_eq!(
            parse_token(r#"{"access_token": "t", "expires_at": 2000000}"#, now).as_deref(),
            Some("t")
        );
        assert_eq!(
            parse_token(r#"{"access_token": "t", "expires_at": 1030000}"#, now),
            None
        );
        assert_eq!(parse_token(r#"{"access_token": ""}"#, now), None);
        assert_eq!(parse_token("garbage", now), None);
    }
}
