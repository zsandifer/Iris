//! Owner-only Google setup. Progress contains only fixed steps and protocol
//! codes; pasted credentials go straight to the existing connector vault path.
use crate::{db, entities, state::AppState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const SETTING: &str = "google.setup";
pub const STEPS: u8 = 6;

/// All Google Console links live here and are supplied to the app by the API.
const GOOGLE_PAGES: &[(&str, &str)] = &[
    ("project", "https://console.cloud.google.com/projectcreate"),
    (
        "apis",
        "https://console.cloud.google.com/flows/enableapi?apiid=gmail.googleapis.com,calendar-json.googleapis.com,drive.googleapis.com,youtube.googleapis.com",
    ),
    // One-at-a-time fallbacks if the combined link doesn't enable all four.
    (
        "api_gmail",
        "https://console.cloud.google.com/apis/library/gmail.googleapis.com",
    ),
    (
        "api_calendar",
        "https://console.cloud.google.com/apis/library/calendar-json.googleapis.com",
    ),
    (
        "api_drive",
        "https://console.cloud.google.com/apis/library/drive.googleapis.com",
    ),
    (
        "api_youtube",
        "https://console.cloud.google.com/apis/library/youtube.googleapis.com",
    ),
    ("branding", "https://console.cloud.google.com/auth/branding"),
    ("audience", "https://console.cloud.google.com/auth/audience"),
    (
        "client",
        "https://console.cloud.google.com/auth/clients/create",
    ),
];

pub fn pages() -> Value {
    Value::Object(
        GOOGLE_PAGES
            .iter()
            .map(|(key, url)| ((*key).into(), json!(url)))
            .collect(),
    )
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Issue {
    RedirectUriMismatch,
    AccessDenied,
    InvalidClient,
    UnauthorizedClient,
    InvalidScope,
    MissingPermissions,
}

impl Issue {
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "redirect_uri_mismatch" => Some(Self::RedirectUriMismatch),
            "access_denied" => Some(Self::AccessDenied),
            "invalid_client" => Some(Self::InvalidClient),
            "unauthorized_client" => Some(Self::UnauthorizedClient),
            "invalid_scope" => Some(Self::InvalidScope),
            "missing_permissions" => Some(Self::MissingPermissions),
            _ => None,
        }
    }

    pub fn fix(self) -> (u8, &'static str) {
        match self {
            Self::RedirectUriMismatch => (
                4,
                "I need the callback address to match exactly. Copy it into Authorized redirect URIs on your Web application key, then sign in again.",
            ),
            Self::AccessDenied => (
                3,
                "I couldn't get permission. If you cancelled, try signing in again. If Google blocked you, choose External and add your own Google account as a test user on Audience.",
            ),
            Self::InvalidClient => (
                6,
                "I couldn't use that login key. Paste the client ID and secret from the same Google client again. If it was deleted, create a new Web application key in step 4.",
            ),
            Self::UnauthorizedClient => (
                4,
                "I need a Web application key. Create one with the callback address shown here, then paste its ID and secret in step 6.",
            ),
            Self::InvalidScope => (
                2,
                "I couldn't request Google access. Turn on all four APIs in your Iris project, then try signing in again.",
            ),
            Self::MissingPermissions => (
                6,
                "I need permission for all four services to connect Google. Sign in again and select Gmail, Calendar, Drive and YouTube access.",
            ),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Progress {
    pub step: u8,
    #[serde(default)]
    pub sign_in: bool,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub issue: Option<Issue>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            step: 1,
            sign_in: false,
            completed: false,
            issue: None,
        }
    }
}

pub fn read(c: &rusqlite::Connection) -> rusqlite::Result<Progress> {
    let progress = db::get_setting(c, SETTING)?
        .and_then(|v| serde_json::from_str::<Progress>(&v).ok())
        .filter(|p| (1..=STEPS).contains(&p.step))
        .unwrap_or_default();
    Ok(progress)
}

pub fn save(c: &rusqlite::Connection, progress: &Progress) -> rusqlite::Result<()> {
    db::set_setting(c, SETTING, &json!(progress).to_string())
}

pub fn callback(public_url: &str) -> Option<String> {
    let base = public_url.trim().trim_end_matches('/');
    let uri = base.parse::<ureq::http::Uri>().ok()?;
    if uri.scheme_str() != Some("https")
        || uri.authority()?.as_str().contains('@')
        || uri.query().is_some()
        || base.contains(['#', '\\'])
        || crate::oauth::check_secure_url(base).is_err()
    {
        return None;
    }
    Some(format!("{base}/api/oauth/callback"))
}

/// A deliberately small projection shared by the app and system_status.
pub fn status(c: &rusqlite::Connection) -> rusqlite::Result<Value> {
    let con = entities::list_connectors(c, false)?
        .into_iter()
        .find(|c| c.service == "google");
    let has_client = con.as_ref().is_some_and(|c| {
        c.config
            .oauth
            .as_ref()
            .is_some_and(|o| o.client_id.is_some())
    });
    let p = if db::get_setting(c, SETTING)?.is_some() {
        let mut p = read(c)?;
        // Saved progress can outlive the connector (removed, then re-added):
        // without a saved key, resume at the paste step instead of sign-in.
        if p.sign_in && !has_client {
            p.sign_in = false;
            p.completed = false;
            p.step = STEPS;
        }
        p
    } else if con.as_ref().is_some_and(|c| {
        c.config
            .oauth
            .as_ref()
            .is_some_and(|o| o.client_id.is_some())
    }) {
        Progress {
            step: STEPS,
            sign_in: true,
            completed: true,
            issue: None,
        }
    } else {
        Progress::default()
    };
    let connected = con.as_ref().is_some_and(|c| {
        c.auth_state == "ok"
            && c.config
                .oauth
                .as_ref()
                .is_some_and(|o| o.access_token.is_some() || o.refresh_token.is_some())
    });
    let phase = if connected && p.completed && p.issue.is_none() {
        "connected"
    } else if p.sign_in {
        "sign_in"
    } else {
        "setup"
    };
    Ok(json!({
        "step": p.step, "total_steps": STEPS, "phase": phase,
        "summary": if phase == "connected" { "Google connected".into() } else { format!("Google setup: step {} of {STEPS}", p.step) },
        "error": p.issue.map(|i| { let (step, message) = i.fix(); json!({"step": step, "message": message}) }),
    }))
}

pub async fn report_issue(
    state: &AppState,
    code: &str,
) -> crate::util::Result<Option<&'static str>> {
    let Some(issue) = Issue::from_code(code) else {
        return Ok(None);
    };
    let (step, message) = issue.fix();
    state
        .db
        .call(move |c| {
            save(
                c,
                &Progress {
                    step,
                    sign_in: matches!(issue, Issue::MissingPermissions),
                    completed: false,
                    issue: Some(issue),
                },
            )
        })
        .await?;
    crate::connectors::broadcast_changed(state).await;
    Ok(Some(message))
}

/// Never Debug/Serialize: the pair may only be handed to connectors::setup.
pub struct Client {
    pub id: String,
    pub secret: String,
}

fn id_valid(s: &str) -> bool {
    s.strip_suffix(".apps.googleusercontent.com")
        .is_some_and(|p| {
            !p.is_empty()
                && p.len() <= 200
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

fn secret_valid(s: &str) -> bool {
    s.strip_prefix("GOCSPX-").is_some_and(|p| {
        !p.is_empty()
            && p.len() <= 200
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

pub fn parse(paste: &str) -> Result<Client, &'static str> {
    if paste.len() > 64 * 1024 {
        return Err(
            "I only need the downloaded client JSON or the client ID and secret. This paste is too long.",
        );
    }
    let paste = paste.trim().trim_start_matches('\u{feff}');
    let (ids, secrets): (Vec<String>, Vec<String>) = if paste.starts_with('{') {
        let v: Value = serde_json::from_str(paste).map_err(
            |_| "I couldn't read that JSON. Copy the whole downloaded file, including the braces.",
        )?;
        let client = v.get("web").or_else(|| v.get("installed")).unwrap_or(&v);
        (
            vec![
                client["client_id"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .into(),
            ],
            vec![
                client["client_secret"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .into(),
            ],
        )
    } else {
        let tokens: Vec<_> = paste
            .split(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.'))
            .collect();
        (
            tokens
                .iter()
                .filter(|s| id_valid(s))
                .map(|s| s.to_string())
                .collect(),
            tokens
                .iter()
                .filter(|s| secret_valid(s))
                .map(|s| s.to_string())
                .collect(),
        )
    };
    let unique = |mut v: Vec<String>| {
        v.sort();
        v.dedup();
        v
    };
    let (ids, secrets) = (unique(ids), unique(secrets));
    if ids.len() > 1 || secrets.len() > 1 {
        return Err(
            "I found more than one login key. Paste just one client's ID and secret together.",
        );
    }
    let id = ids.first().filter(|s| id_valid(s)).ok_or("I need the client ID ending in .apps.googleusercontent.com. Paste it with the secret, or paste the downloaded JSON.")?;
    let secret = secrets.first().filter(|s| secret_valid(s)).ok_or("I found the client ID, but I also need the secret starting with GOCSPX-. Copy both from the same login key.")?;
    Ok(Client {
        id: id.clone(),
        secret: secret.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "123-example.apps.googleusercontent.com";
    const SECRET: &str = "GOCSPX-paste-canary_123";

    #[test]
    fn stale_sign_in_progress_resumes_at_the_paste_step_without_a_connector() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let st = db
            .with(|c| {
                save(
                    c,
                    &Progress {
                        step: STEPS,
                        sign_in: true,
                        completed: true,
                        issue: None,
                    },
                )?;
                status(c)
            })
            .unwrap();
        assert_eq!(st["phase"], "setup");
        assert_eq!(st["step"], STEPS);
    }

    #[test]
    fn parses_google_downloads_and_freeform_pairs() {
        let pair = json!({"client_id": ID, "client_secret": SECRET});
        for text in [
            json!({"web": pair}).to_string(),
            json!({"installed": pair}).to_string(),
            format!("Client ID: {ID}\nClient secret: {SECRET}"),
            format!("{ID} {SECRET}"),
            format!("secret={SECRET}\nID={ID}"),
            format!(" \r\n{SECRET}\t{ID}\n "),
            format!("{ID}\n{SECRET}\n{ID}"),
        ] {
            let client = parse(&text).unwrap();
            assert_eq!(client.id, ID);
            assert_eq!(client.secret, SECRET);
        }
    }

    #[test]
    fn rejects_junk_missing_malformed_and_ambiguous_values_without_echoing() {
        for text in [
            "".into(),
            "junk".into(),
            "{}.".into(),
            ID.into(),
            SECRET.into(),
            format!("{{\"web\": {{\"client_secret\": \"{SECRET}\"}}}}"),
            format!("{ID} {SECRET} GOCSPX-another"),
            format!("bad.id.apps.googleusercontent.com {SECRET}"),
            "x".repeat(65537),
        ] {
            let error = parse(&text).err().unwrap();
            assert!(!error.contains(SECRET));
        }
    }

    #[test]
    fn computes_only_exact_https_public_callbacks() {
        for base in [
            "https://iris.example",
            " https://iris.example/ ",
            "https://iris.example:8443",
        ] {
            assert_eq!(
                callback(base),
                Some(format!(
                    "{}/api/oauth/callback",
                    base.trim().trim_end_matches('/')
                ))
            );
        }
        for base in [
            "",
            "http://iris.example",
            "http://127.0.0.1:7420",
            "https://iris.example?x=1",
            "https://iris.example/#x",
            "https://user@iris.example",
            "https://",
        ] {
            assert!(callback(base).is_none(), "{base}");
        }
    }

    #[test]
    fn maps_only_known_errors_to_steps() {
        for (code, step) in [
            ("redirect_uri_mismatch", 4),
            ("access_denied", 3),
            ("invalid_client", 6),
            ("unauthorized_client", 4),
            ("invalid_scope", 2),
        ] {
            assert_eq!(Issue::from_code(code).unwrap().fix().0, step);
        }
        assert!(Issue::from_code("invalid_client secret-canary").is_none());
    }
}
