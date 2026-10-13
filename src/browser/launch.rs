//! Finding and configuring Playwright MCP, the server that runs Iris's
//! browser.
//!
//! The server: the copy Iris installed from the app (`<engines>/bin/browser`,
//! npm `@playwright/mcp`, see `engines.rs`), then `playwright-mcp` on PATH.
//! Chromium: `browser.command` from the config (set, it always wins); else
//! the build Iris installed at the fixed `<engines>/chromium/app`, the one
//! path the root setup approves for Chrome's sandbox (`apparmor.rs`); else
//! Playwright's own download.
//!
//! Iris only writes the server's config file: a persistent profile under
//! the data directory, a phone-shaped headless window, Chrome's sandbox on
//! unless the owner agreed otherwise. Everything else is Playwright's.

use crate::gateway::upstream::Launch;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The program behind Iris's browser and the Chromium it drives.
#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    /// Playwright MCP's command.
    pub program: PathBuf,
    /// Chromium for it to start (None: Playwright's own download).
    pub chromium: Option<PathBuf>,
    /// The user agent without "HeadlessChrome" (some sites refuse headless
    /// browsers outright), when the Chromium version is known.
    pub user_agent: Option<String>,
}

/// Resolve the server (None: not installed anywhere, or `browser.command`
/// names a Chromium that is not there).
pub fn find(chromium: &str) -> Option<Server> {
    let managed = crate::harness::resolve_command("browser")
        .filter(|p| crate::harness::is_managed(p))
        .and_then(|p| Some(p.parent()?.parent()?.to_path_buf()))
        .filter(|root| crate::engines::browser_ready(root));
    find_in(
        chromium,
        managed.as_deref(),
        &crate::harness::resolve_command,
    )
}

/// The lookup, with the system around it passed in (for tests).
pub(crate) fn find_in(
    chromium: &str,
    managed: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<Server> {
    let program = match managed {
        Some(root) => crate::engines::launcher(root, crate::engines::BROWSER.name),
        None => resolve("playwright-mcp")?,
    };
    let configured = chromium.trim();
    let (chromium, user_agent) = if !configured.is_empty() {
        (Some(resolve(configured)?), None)
    } else {
        match managed {
            Some(root) => (
                crate::engines::browser_executable(root),
                crate::engines::chromium_major(root).map(|v| user_agent(&v)),
            ),
            None => (None, None),
        }
    };
    Some(Server {
        program,
        chromium,
        user_agent,
    })
}

/// Chrome's reduced user agent for this OS and major version.
fn user_agent(major: &str) -> String {
    let platform = if cfg!(target_os = "macos") {
        "Macintosh; Intel Mac OS X 10_15_7"
    } else {
        "X11; Linux x86_64"
    };
    format!(
        "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
    )
}

/// Most bytes Playwright keeps in its output folder (page snapshots, the
/// agent's screenshots) before it removes the oldest.
const OUTPUT_MAX: u64 = 32 * 1024 * 1024;

/// Playwright MCP's config file for Iris's browser.
pub fn config(server: &Server, profile: &Path, out_dir: &Path, no_sandbox: bool) -> Value {
    let mut launch =
        json!({"channel": "chromium", "headless": true, "chromiumSandbox": !no_sandbox});
    if let Some(p) = &server.chromium {
        launch["executablePath"] = p.to_string_lossy().into();
    }
    // Phone-shaped, so the live view fills a phone screen and responsive
    // sites show their mobile layout; scale 2 keeps text sharp there.
    let mut context = json!({
        "viewport": {"width": super::VIEW_W, "height": super::VIEW_H},
        "deviceScaleFactor": super::SCALE,
    });
    if let Some(ua) = &server.user_agent {
        context["userAgent"] = ua.as_str().into();
    }
    json!({
        "browser": {
            "browserName": "chromium",
            "userDataDir": profile,
            "launchOptions": launch,
            "contextOptions": context,
        },
        // Clicks and drags by position: CAPTCHAs, canvases, maps, and the
        // owner's taps while in control.
        "capabilities": ["vision"],
        "outputDir": out_dir,
        "outputMaxSize": OUTPUT_MAX,
        // Pages must not add tools of their own.
        "webmcp": false,
        // Results carry no generated code (it would echo what was typed).
        "codegen": "none",
        // Iris closes the browser when idle (`browser.idle_mins`).
        "timeouts": {"idle": 0},
    })
}

/// How to start the server with the config file at `config`.
pub fn command(server: &Server, config: &Path, cwd: &Path) -> Launch {
    Launch::Stdio {
        command: server.program.to_string_lossy().into_owned(),
        args: vec!["--config".into(), config.to_string_lossy().into_owned()],
        env: BTreeMap::new(),
        cwd: cwd.to_path_buf(),
    }
}

/// Chrome's own words for "the sandbox cannot start here" (for example
/// Ubuntu's AppArmor restriction on unprivileged user namespaces), as
/// Playwright passes them on.
pub fn sandbox_failure(text: &str) -> bool {
    text.contains("No usable sandbox")
        || text.contains("setuid sandbox")
        || text.contains("sandboxing failed")
}

/// What to tell the owner when the browser will not start.
pub fn explain_failure(text: &str) -> String {
    const DEPS: &str = "On the server, run once as root: `npx playwright install-deps chromium` \
        (or install your distribution's Chromium).";
    if let Some(line) = text
        .lines()
        .find(|l| l.contains("error while loading shared libraries"))
    {
        let lib = line
            .split("libraries:")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .unwrap_or("")
            .trim();
        return format!("Chromium is missing system libraries ({lib}). {DEPS}");
    }
    if text.contains("missing dependencies") {
        return format!("Chromium is missing system libraries. {DEPS}");
    }
    if text.contains("is not installed") || text.contains("Executable doesn't exist") {
        return "Chromium is not installed; set up the browser again from Iris's computer in the app."
            .into();
    }
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("Error:"))
        .or_else(|| text.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("the browser exited right after starting");
    crate::util::truncate_chars(error_line(line), 300)
}

/// A Playwright error line without its prefixes.
pub fn error_line(line: &str) -> &str {
    let mut s = line.trim();
    for prefix in ["Error:", "browserBackend.callTool:"] {
        s = s.strip_prefix(prefix).unwrap_or(s).trim();
    }
    s
}

/// Turn off Chrome's own password manager and form autofill in Iris's
/// profile: saved logins and cards live in Iris's vault, and nothing typed
/// into a page (also while the owner has taken over) should be stored by
/// the browser.
pub fn set_profile_prefs(profile: &Path) -> crate::util::Result<()> {
    let file = profile.join("Default").join("Preferences");
    let mut prefs: Value = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let set = |v: &mut Value, path: &[&str], val: bool| {
        let mut cur = v;
        for k in &path[..path.len() - 1] {
            if !cur[*k].is_object() {
                cur[*k] = json!({});
            }
            cur = &mut cur[*k];
        }
        cur[path[path.len() - 1]] = val.into();
    };
    set(&mut prefs, &["credentials_enable_service"], false);
    set(&mut prefs, &["credentials_enable_autosignin"], false);
    set(&mut prefs, &["profile", "password_manager_enabled"], false);
    set(&mut prefs, &["autofill", "profile_enabled"], false);
    set(&mut prefs, &["autofill", "credit_card_enabled"], false);
    crate::config::ensure_private_dir(&profile.join("Default"))?;
    crate::config::write_private_file(&file, prefs.to_string().as_bytes())?;
    Ok(())
}

/// Chrome keeps the open tabs, with what was typed into their forms, in
/// session files for crash recovery. Iris's browser never restores a
/// session, and card numbers the owner typed must not stay on disk: remove
/// them before each start and after each close.
pub fn clear_session_files(profile: &Path) {
    let d = profile.join("Default");
    let _ = std::fs::remove_dir_all(d.join("Sessions"));
    for f in [
        "Current Session",
        "Current Tabs",
        "Last Session",
        "Last Tabs",
    ] {
        let _ = std::fs::remove_file(d.join(f));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_prefers_irises_own_copy_and_the_configured_chromium() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let exe = |p: PathBuf| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let root = d.join("engines");
        let chrome = exe(root.join("chromium/app/chrome"));
        let configured = exe(d.join("custom/chrome"));
        let on_path = exe(d.join("path/playwright-mcp"));
        let path_dir = d.join("path");
        let resolve = move |c: &str| -> Option<PathBuf> {
            if c.contains('/') {
                return Path::new(c).is_file().then(|| PathBuf::from(c));
            }
            Some(path_dir.join(c)).filter(|p| p.is_file())
        };
        // Iris's own copy: its launcher and its Chromium.
        let s = find_in("", Some(&root), &resolve).unwrap();
        assert_eq!(s.program, root.join("bin/browser"));
        assert_eq!(s.chromium, Some(chrome));
        // browser.command always wins; a missing one does not fall through.
        let cfg = configured.to_string_lossy().into_owned();
        assert_eq!(
            find_in(&cfg, Some(&root), &resolve).unwrap().chromium,
            Some(configured)
        );
        assert!(find_in("/nope/chrome", Some(&root), &resolve).is_none());
        // Else Playwright MCP from PATH, with Playwright's own Chromium.
        let s = find_in(" ", None, &resolve).unwrap();
        assert_eq!((s.program, s.chromium), (on_path.clone(), None));
        std::fs::remove_file(&on_path).unwrap();
        assert!(find_in("", None, &resolve).is_none());
    }

    #[test]
    fn config_keeps_the_sandbox_and_a_private_profile() {
        let server = Server {
            program: "/e/bin/browser".into(),
            chromium: Some("/e/chromium/app/chrome".into()),
            user_agent: Some(user_agent("150")),
        };
        let c = config(&server, Path::new("/d/profile"), Path::new("/d/out"), false);
        assert_eq!(c["browser"]["userDataDir"], "/d/profile");
        assert_eq!(c["browser"]["launchOptions"]["chromiumSandbox"], true);
        assert_eq!(c["browser"]["launchOptions"]["headless"], true);
        assert_eq!(
            c["browser"]["launchOptions"]["executablePath"],
            "/e/chromium/app/chrome"
        );
        assert_eq!(c["browser"]["contextOptions"]["viewport"]["width"], 500);
        let ua = c["browser"]["contextOptions"]["userAgent"]
            .as_str()
            .unwrap();
        assert!(ua.contains("Chrome/150.0.0.0") && !ua.contains("Headless"));
        assert_eq!(c["webmcp"], false);
        assert_eq!(c["codegen"], "none");
        assert_eq!(c["outputDir"], "/d/out");
        let c = config(&server, Path::new("/p"), Path::new("/o"), true);
        assert_eq!(c["browser"]["launchOptions"]["chromiumSandbox"], false);
        match command(&server, Path::new("/d/pw.json"), Path::new("/d/work")) {
            Launch::Stdio {
                command, args, cwd, ..
            } => {
                assert_eq!(command, "/e/bin/browser");
                assert_eq!(args, ["--config", "/d/pw.json"]);
                assert_eq!(cwd, Path::new("/d/work"));
            }
            _ => panic!("stdio"),
        }
    }

    #[test]
    fn profile_prefs_turn_off_chromes_password_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        std::fs::create_dir_all(p.join("Default/Sessions")).unwrap();
        std::fs::write(
            p.join("Default/Preferences"),
            r#"{"profile": {"name": "x", "password_manager_enabled": true}, "autofill": 3}"#,
        )
        .unwrap();
        std::fs::write(p.join("Default/Current Session"), "typed").unwrap();
        set_profile_prefs(p).unwrap();
        let v: Value =
            serde_json::from_slice(&std::fs::read(p.join("Default/Preferences")).unwrap()).unwrap();
        assert_eq!(v["profile"]["name"], "x");
        assert_eq!(v["profile"]["password_manager_enabled"], false);
        assert_eq!(v["credentials_enable_service"], false);
        assert_eq!(v["autofill"]["credit_card_enabled"], false);
        clear_session_files(p);
        assert!(!p.join("Default/Sessions").exists());
        assert!(!p.join("Default/Current Session").exists());
        assert!(p.join("Default/Preferences").exists());
    }

    #[test]
    fn explains_launch_failures() {
        // Synthetic samples modeled on Playwright's launch errors.
        let sandbox = "Error: browserBackend.callTool: Target page, context or browser has been closed\n\
            Browser logs:\nChromium sandboxing failed!\n[pid=1][err] [1:1:FATAL:zygote_host_impl_linux.cc(130)] \
            No usable sandbox! If you are running on Ubuntu 23.10+";
        assert!(sandbox_failure(sandbox));
        assert!(!sandbox_failure("Error: net::ERR_NAME_NOT_RESOLVED"));
        let missing = "[pid=1][err] chrome: error while loading shared libraries: libnss3.so: cannot open shared object file";
        assert!(explain_failure(missing).contains("libnss3.so"));
        assert!(explain_failure(missing).contains("install-deps"));
        assert!(
            explain_failure(
                "Error: Browser \"chromium\" is not installed; expected executable at /x"
            )
            .contains("set up the browser again")
        );
        assert_eq!(
            explain_failure(
                "### Error\nError: browserBackend.callTool: something broke\nCall log:"
            ),
            "something broke"
        );
        assert_eq!(
            explain_failure(""),
            "the browser exited right after starting"
        );
    }
}
