//! Owner-only Google setup. Progress includes the non-secret project ID;
//! pasted credentials go straight to the existing connector vault path.
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
        "https://console.cloud.google.com/flows/enableapi?apiid=gmail.googleapis.com,calendar-json.googleapis.com,drive.googleapis.com,youtube.googleapis.com,docs.googleapis.com,sheets.googleapis.com,slides.googleapis.com,forms.googleapis.com,tasks.googleapis.com,people.googleapis.com",
    ),
    // One-at-a-time fallbacks if the combined link doesn't enable them all.
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
    (
        "api_docs",
        "https://console.cloud.google.com/apis/library/docs.googleapis.com",
    ),
    (
        "api_sheets",
        "https://console.cloud.google.com/apis/library/sheets.googleapis.com",
    ),
    (
        "api_slides",
        "https://console.cloud.google.com/apis/library/slides.googleapis.com",
    ),
    (
        "api_forms",
        "https://console.cloud.google.com/apis/library/forms.googleapis.com",
    ),
    (
        "api_tasks",
        "https://console.cloud.google.com/apis/library/tasks.googleapis.com",
    ),
    (
        "api_people",
        "https://console.cloud.google.com/apis/library/people.googleapis.com",
    ),
    ("branding", "https://console.cloud.google.com/auth/branding"),
    ("audience", "https://console.cloud.google.com/auth/audience"),
    (
        "client",
        "https://console.cloud.google.com/auth/clients/create",
    ),
];

pub fn pages(project_id: Option<&str>) -> Value {
    Value::Object(
        GOOGLE_PAGES
            .iter()
            .map(|(key, url)| {
                let pinned = project_id
                    .filter(|id| project_id_valid(id) && *key != "project")
                    .map_or_else(|| (*url).to_string(), |id| pin_project(url, id));
                ((*key).into(), json!(pinned))
            })
            .collect(),
    )
}

fn pin_project(url: &str, id: &str) -> String {
    let (base, fragment) = url.split_once('#').unwrap_or((url, ""));
    let separator = if base.contains('?') { '&' } else { '?' };
    let mut pinned = format!("{base}{separator}project={id}");
    if url.contains('#') {
        pinned.push('#');
        pinned.push_str(fragment);
    }
    pinned
}

fn project_id_valid(id: &str) -> bool {
    (6..=30).contains(&id.len())
        && id.as_bytes()[0].is_ascii_lowercase()
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Accept a copied ID, Console address, or text around the ID. Never guess
/// another ID when an explicit project= value is invalid or ambiguous.
pub fn parse_project_id(paste: &str) -> Result<String, &'static str> {
    const ERROR: &str = "I need the Project ID shown under your project name: 6–30 lowercase letters, numbers or hyphens, starting with a letter and not ending with a hyphen.";
    if paste.len() > 65536 {
        return Err(ERROR);
    }
    let text = paste.trim();
    if project_id_valid(text) {
        return Ok(text.to_string());
    }
    let explicit: Vec<_> = text
        .split(|c: char| c.is_whitespace() || "?&#\"'<>(),;[]{}".contains(c))
        .filter_map(|s| s.strip_prefix("project="))
        .collect();
    let has_explicit = !explicit.is_empty();
    let tokens: Vec<_> = if !has_explicit {
        text.split(|c: char| !c.is_alphanumeric() && !"-_.".contains(c))
            .map(|id| id.trim_end_matches('.'))
            .filter(|id| {
                id.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                    && !id.eq_ignore_ascii_case("project")
                    && !id.eq_ignore_ascii_case("id")
            })
            .collect()
    } else {
        explicit
    };
    // Auto-generated IDs contain hyphens/digits; prefer them over surrounding
    // prose such as "project". Letter-only IDs still work, including bare IDs.
    let distinctive = !has_explicit
        && tokens
            .iter()
            .any(|id| id.contains(['-', '_']) || id.bytes().any(|b| b.is_ascii_digit()));
    let mut ids = tokens.into_iter().filter(|id| {
        if has_explicit {
            true
        } else if distinctive {
            id.contains(['-', '_']) || id.bytes().any(|b| b.is_ascii_digit())
        } else {
            project_id_valid(id)
        }
    });
    let id = ids.next().ok_or(ERROR)?;
    if !project_id_valid(id) {
        return Err(ERROR);
    }
    if ids.any(|other| other != id) {
        return Err("I found more than one Project ID. Copy just the ID for your Iris project.");
    }
    Ok(id.to_string())
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
                "I couldn't request Google access. Turn on all of the Google APIs in your Iris project, then try signing in again.",
            ),
            Self::MissingPermissions => (
                6,
                "I need permission for every Google service to connect Google. Sign in again and leave every box checked: Gmail, Calendar, Drive, Docs, Sheets, Slides, Forms, Tasks, Contacts and YouTube.",
            ),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Progress {
    pub step: u8,
    #[serde(default)]
    pub project_id: Option<String>,
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
            project_id: None,
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

/// Branding needs a domain, not the callback URL. This small PSL subset is
/// only setup guidance, never a security boundary. Rules modeled on
/// https://publicsuffix.org/list/public_suffix_list.dat; longest match wins.
pub fn authorized_domain(public_url: &str) -> Option<String> {
    let callback = callback(public_url)?;
    let uri = callback.parse::<ureq::http::Uri>().ok()?;
    let host = uri.host()?.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<_> = host.split('.').collect();
    if host.parse::<std::net::IpAddr>().is_ok()
        || host.len() > 253
        || labels.iter().any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return None;
    }
    const SUFFIXES: &str = "ts.net co.uk org.uk me.uk ac.uk com.au net.au org.au
        co.nz org.nz co.jp co.in com.br com.cn com.sg co.za com.mx com.tr
        github.io appspot.com pages.dev vercel.app netlify.app
        com net org edu gov mil int io dev app ai info biz me cloud online site xyz tech
        uk au nz jp in br cn sg za mx tr us ca de fr nl ch it es se no fi eu";
    let suffix_len = SUFFIXES
        .split_ascii_whitespace()
        .filter(|suffix| {
            host == *suffix
                || host
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
        .map(|suffix| suffix.split('.').count())
        .max();
    let start = match suffix_len {
        Some(len) => labels.len().checked_sub(len + 1)?,
        // Unknown suffix: use the requested conservative fallback.
        None => usize::from(labels.len() >= 3),
    };
    Some(labels[start..].join("."))
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
            ..Progress::default()
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
    // Connected with an older grant: the owner can add the newer services.
    let more_access = phase == "connected"
        && con
            .as_ref()
            .is_some_and(crate::connectors::google_needs_more_access);
    Ok(json!({
        "step": p.step, "total_steps": STEPS, "phase": phase,
        "more_access": more_access,
        "project_id": p.project_id,
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
                    ..read(c)?
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
    fn branding_domain_uses_public_suffixes_and_strips_url_parts() {
        for (url, domain) in [
            (
                "https://laptop.tail-example.ts.net:8446",
                "tail-example.ts.net",
            ),
            ("https://tail-example.ts.net/iris/", "tail-example.ts.net"),
            ("https://chat.home.example.com:8446/iris", "example.com"),
            ("https://IRIS.Example.COM./", "example.com"),
            ("https://iris.example.co.uk", "example.co.uk"),
            ("https://iris.example.com.au", "example.com.au"),
            ("https://iris.owner.github.io", "owner.github.io"),
            ("https://iris.home.example", "home.example"),
            ("https://iris.office.home.example", "office.home.example"),
            ("https://iris.example", "iris.example"),
            ("https://example", "example"),
        ] {
            assert_eq!(authorized_domain(url).as_deref(), Some(domain), "{url}");
        }
        for url in [
            "",
            "not a URL",
            "http://iris.example",
            "https://127.0.0.1",
            "https://[::1]",
            "https://user@iris.example",
            "https://iris.example?x=1",
            "https://iris.example#fragment",
            "https://ts.net",
            "https://com",
            "https://iris..example",
            "https://-iris.example",
        ] {
            assert_eq!(authorized_domain(url), None, "{url}");
        }
    }

    #[test]
    fn extracts_and_validates_project_ids() {
        const PROJECT: &str = "example-project-123456-a1";
        assert_eq!(
            parse_project_id("Copied Project ID: abcdef").unwrap(),
            "abcdef"
        );
        for paste in [
            PROJECT.to_string(),
            format!("  {PROJECT}\n"),
            format!("https://console.cloud.google.com/auth/branding?project={PROJECT}"),
            format!(
                "Open https://console.cloud.google.com/flows/enableapi?apiid=gmail.googleapis.com&project={PROJECT}&other=1#top"
            ),
            format!("Project ID: {PROJECT}"),
            format!("Your project is {PROJECT}"),
            format!("Your project is {PROJECT}."),
            format!("project={PROJECT}"),
            format!("{PROJECT}\n{PROJECT}"),
        ] {
            assert_eq!(parse_project_id(&paste).unwrap(), PROJECT, "{paste}");
        }
        for id in [
            "abcdef",
            "project",
            "a12345",
            "a23456789012345678901234567890",
        ] {
            assert_eq!(parse_project_id(id).unwrap(), id);
        }
        for paste in [
            "",
            "junk",
            "abcde",
            "Iris",
            "Example-project-123456",
            "example-project-",
            "123example",
            "-example",
            "example_project",
            "example.project",
            "éxample-project",
            "example-projéct",
            "a234567890123456789012345678901",
            "https://console.cloud.google.com/auth/branding?project=BAD-ID",
            "project=abcde example-project-123456",
            "Your project is Example-project-123456",
            "Your project is example-project-",
            "project ID: UPPERCASE",
            "project=example-project-123456&project=another-project-123456",
            "project=abcdef&project=another-project-123456",
            "example-project-123456 another-project-123456",
        ] {
            assert!(parse_project_id(paste).is_err(), "{paste}");
        }
        assert!(parse_project_id(&"a".repeat(65537)).is_err());
    }

    #[test]
    fn pins_all_later_console_pages_without_losing_queries_or_fragments() {
        let id = "example-project-123456";
        let unpinned = pages(None);
        let pinned = pages(Some(id));
        for (key, url) in GOOGLE_PAGES {
            assert_eq!(unpinned[key], *url);
            let expected = if *key == "project" {
                url.to_string()
            } else {
                format!(
                    "{url}{}project={id}",
                    if url.contains('?') { '&' } else { '?' }
                )
            };
            assert_eq!(pinned[key], expected);
        }
        assert_eq!(pages(Some("invalid&project=other")), unpinned);
        assert_eq!(
            pin_project("https://console.cloud.google.com/auth/audience#top", id),
            format!("https://console.cloud.google.com/auth/audience?project={id}#top")
        );
        assert_eq!(
            pin_project("https://console.cloud.google.com/auth/audience?x=1#top", id),
            format!("https://console.cloud.google.com/auth/audience?x=1&project={id}#top")
        );
    }

    #[test]
    fn old_progress_without_a_project_id_still_resumes() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let progress = db
            .with(|c| {
                db::set_setting(c, SETTING, r#"{"step":3,"sign_in":false}"#)?;
                read(c)
            })
            .unwrap();
        assert_eq!(progress.step, 3);
        assert_eq!(progress.project_id, None);
    }

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
                        ..Progress::default()
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
