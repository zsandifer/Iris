//! Filling saved logins and wallet cards, when the owner asks while in
//! control of Iris's browser.
//!
//! The owner taps a field on the page, then picks a saved item and which
//! of its fields to fill (`browser.input` with `kind: "fill"`). Iris types
//! it into the focused field through the browser server. The agent never
//! gets it: secrets travel from the vault to the page only, never into a
//! tool result, a message, or a log, and the page is closed to the agent
//! until it moves on. Logins are only filled into the site they were saved
//! for (the page's real address, and never into another site's frame on
//! it), cards and secrets only over HTTPS (plain HTTP only on this
//! machine).
//!
//! The agent cannot fill anything itself: it asks the owner to take over.

use crate::entities::{self, Credential};
use crate::state::AppState;
use crate::util::Result;
use serde_json::Value;

/// Parse a web origin. Bare saved sites default to HTTPS, including its
/// default port; paths never broaden the authority being matched.
fn login_origin(site: &str) -> Option<(String, String, u16)> {
    let site = site.trim();
    let url = if site.contains("://") {
        site.to_string()
    } else {
        format!("https://{site}")
    };
    let uri: axum::http::Uri = url.parse().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = uri.authority()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let host = authority
        .host()
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let explicit_port = if authority.as_str().starts_with('[') {
        !authority.as_str().split_once(']')?.1.is_empty()
    } else {
        authority.as_str().contains(':')
    };
    let port = if explicit_port {
        authority.port_u16()?
    } else {
        default_port
    };
    Some((scheme, host, port))
}

pub fn origin_of(url: &str) -> Option<(String, String)> {
    if !url.contains("://") {
        return None;
    }
    let (scheme, host, _) = login_origin(url)?;
    Some((scheme, host))
}

pub fn site_matches(page_url: &str, site: &str, allow_subdomains: bool) -> bool {
    let (Some((ps, ph, pp)), Some((ss, sh, sp))) = (login_origin(page_url), login_origin(site))
    else {
        return false;
    };
    ps == ss
        && pp == sp
        && (ph == sh
            || (allow_subdomains
                && sh.parse::<std::net::IpAddr>().is_err()
                && ph
                    .strip_suffix(&sh)
                    .is_some_and(|prefix| prefix.ends_with('.'))))
}

fn credential_matches(page_url: &str, c: &Credential) -> bool {
    c.metadata["site"]
        .as_str()
        .is_some_and(|site| site_matches(page_url, site, c.metadata["allow_subdomains"] == true))
}

/// This machine: `localhost` (and its reserved subdomains) or a loopback
/// IP literal. Never a DNS name that merely starts with "127.".
fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Secrets are only typed into HTTPS pages (or plain HTTP on this machine).
pub fn secure_enough(scheme: &str, host: &str) -> bool {
    scheme == "https" || (scheme == "http" && is_loopback(host))
}

/// A credential's metadata and decrypted secret, read together.
async fn load(
    state: &AppState,
    user_id: &str,
    id: &str,
) -> Result<(Credential, serde_json::Map<String, Value>)> {
    let (uid, cid) = (user_id.to_string(), id.to_string());
    let (cred, ct) = state
        .db
        .call(move |c| {
            Ok((
                entities::get_credential(c, &uid, &cid)?,
                entities::get_credential_ciphertext(c, &uid, &cid)?,
            ))
        })
        .await?;
    let (Some(cred), Some(ct)) = (cred, ct) else {
        return Err("that saved item is gone".into());
    };
    let plain = state.vault.decrypt(&ct, id)?;
    Ok((cred, serde_json::from_slice(&plain).unwrap_or_default()))
}

/// The value of one field of a saved item, and whether it is private
/// (closes the page to the agent).
fn value_of(
    cred: &Credential,
    secret: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<(String, bool)> {
    let s = |k: &str| {
        secret
            .get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    let m = |k: &str| {
        cred.metadata[k]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    let (value, private) = match (cred.kind.as_str(), field) {
        ("login", "username") => (m("username"), false),
        ("login", "password") => (s("password"), true),
        ("card", "number") => (s("number"), true),
        ("card", "cvc") => (s("cvc"), true),
        ("card", "exp") => (expiry(&cred.metadata), true),
        ("card", "name") => (m("cardholder"), false),
        _ => return Err("that can't be filled".into()),
    };
    value
        .map(|v| (v, private))
        .ok_or_else(|| "that isn't saved for this item".into())
}

/// A card's expiry as MM/YY.
fn expiry(meta: &Value) -> Option<String> {
    let month = meta["exp_month"]
        .as_i64()
        .filter(|m| (1..=12).contains(m))?;
    let year = meta["exp_year"].as_i64().filter(|y| *y > 0)?;
    Some(format!("{month:02}/{:02}", year % 100))
}

/// Fill one field of a saved login or card into the focused field, for
/// the owner in control from `device_id`.
pub async fn fill(
    state: &AppState,
    device_id: &str,
    credential_id: &str,
    field: &str,
) -> Result<()> {
    let user_id = state.browser.controller(device_id)?;
    let (cred, secret) = load(state, &user_id, credential_id).await?;
    let (value, private) = value_of(&cred, &secret, field)?;
    let site = cred.metadata["site"].as_str().unwrap_or("");
    state
        .browser
        .fill_for_owner(
            device_id,
            |field| {
                let url = field.url.as_str();
                let (scheme, host) = origin_of(url).ok_or("Open the site's page first.")?;
                if !secure_enough(&scheme, &host) {
                    return Err("I only fill saved details into secure (https) pages.".into());
                }
                if cred.kind == "login" {
                    if !credential_matches(url, &cred) {
                        return Err(
                            format!("This login is saved for {site}, not this page.").into()
                        );
                    }
                    // The page's address says nothing about a frame from
                    // another site (an ad, a widget) that has focus.
                    if field.frame {
                        return Err(format!(
                            "That field belongs to another site inside this page, so I won't \
                             fill your {site} login there."
                        )
                        .into());
                    }
                }
                Ok(())
            },
            &value,
            private,
        )
        .await?;
    let (uid, cid) = (user_id, cred.id.clone());
    let _ = state
        .db
        .call(move |c| entities::mark_credential_used(c, &uid, &cid))
        .await;
    crate::info!("browser: the owner filled a saved {} field", cred.kind);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_of_saved_items() {
        let cred = |kind: &str, metadata: Value| Credential {
            id: "c".into(),
            kind: kind.into(),
            label: "x".into(),
            metadata,
            created_at: 0,
            updated_at: 0,
            last_used_at: None,
        };
        let secret = |v: Value| v.as_object().unwrap().clone();
        let login = cred("login", json!({"site": "example.test", "username": "sam"}));
        let s = secret(json!({"password": "pw-1234", "notes": "n"}));
        assert_eq!(
            value_of(&login, &s, "username").unwrap(),
            ("sam".into(), false)
        );
        assert_eq!(
            value_of(&login, &s, "password").unwrap(),
            ("pw-1234".into(), true)
        );
        assert!(value_of(&login, &s, "notes").is_err());
        assert!(value_of(&login, &secret(json!({})), "password").is_err());
        let card = cred(
            "card",
            json!({"exp_month": 3, "exp_year": 2031, "cardholder": "S Doe"}),
        );
        let s = secret(json!({"number": "4242424242424242", "cvc": "123"}));
        assert!(value_of(&card, &s, "number").unwrap().1);
        assert_eq!(value_of(&card, &s, "exp").unwrap(), ("03/31".into(), true));
        assert_eq!(
            value_of(&card, &s, "name").unwrap(),
            ("S Doe".into(), false)
        );
        assert!(value_of(&card, &s, "password").is_err());
        assert_eq!(expiry(&json!({"exp_month": 13, "exp_year": 2031})), None);
    }

    #[test]
    fn origins_and_site_matching() {
        assert_eq!(
            origin_of("https://www.Shop.example:8443/cart?x=1").unwrap(),
            ("https".into(), "www.shop.example".into())
        );
        assert_eq!(
            origin_of("http://127.0.0.1:9000/login").unwrap().1,
            "127.0.0.1"
        );
        assert_eq!(origin_of("http://[::1]:80/").unwrap().1, "::1");
        assert!(origin_of("about:blank").is_none());
        assert!(!site_matches(
            "https://example.test",
            "https://example.test:bad",
            true
        ));
        assert!(!site_matches(
            "https://example.test",
            "https://example.test:99999",
            true
        ));
        assert!(site_matches(
            "https://example.test/login",
            "example.test",
            false
        ));
        assert!(site_matches(
            "https://example.test:443/login",
            "https://example.test",
            false
        ));
        assert!(!site_matches(
            "https://example.test:8443/login",
            "example.test",
            false
        ));
        assert!(!site_matches(
            "http://example.test/login",
            "example.test",
            false
        ));
        assert!(!site_matches(
            "https://www.example.test/login",
            "example.test",
            false
        ));
        assert!(!site_matches(
            "https://accounts.example.test/login",
            "example.test",
            false
        ));
        assert!(site_matches(
            "https://accounts.example.test:8443/login",
            "https://example.test:8443",
            true
        ));
        assert!(!site_matches(
            "https://accounts.example.test/login",
            "https://example.test:8443",
            true
        ));
        assert!(!site_matches(
            "https://example.test.evil.test",
            "example.test",
            true
        ));
        assert!(!site_matches(
            "https://badexample.test",
            "example.test",
            true
        ));
        assert!(site_matches(
            "http://[::1]:8888/login",
            "http://[::1]:8888",
            false
        ));
        assert!(!site_matches(
            "http://[::1]:8889/login",
            "http://[::1]:8888",
            false
        ));
        assert!(!site_matches(
            "https://user@example.test",
            "example.test",
            true
        ));
        assert!(secure_enough("https", "example.com"));
        assert!(secure_enough("http", "127.0.0.1"));
        assert!(secure_enough("http", "localhost"));
        assert!(!secure_enough("http", "example.com"));
        assert!(!secure_enough("ftp", "localhost"));
        // DNS names that only look like loopback are remote.
        for host in [
            "127.example.com",
            "127.0.0.1.example.com",
            "localhost.example.com",
        ] {
            assert!(!secure_enough("http", host), "{host}");
        }
        assert!(secure_enough("http", "127.8.9.10"));
        assert!(secure_enough("http", "::1"));
        assert!(secure_enough("http", "app.localhost"));
    }
}
