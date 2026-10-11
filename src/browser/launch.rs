//! Finding and starting Chromium.
//!
//! Lookup order: `browser.command` from the config, the copy Iris
//! installed from the app (`<engines>/bin/browser`, see `engines.rs`),
//! Chromium or Chrome on PATH (snap wrappers are skipped: their sandbox
//! cannot use a profile under a hidden directory), then Playwright's
//! browser cache (`$PLAYWRIGHT_BROWSERS_PATH` or `~/.cache/ms-playwright`).

use super::cdp::Cdp;
use crate::util::Result;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;

const PATH_NAMES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome-stable",
    "google-chrome",
    "chrome",
];

/// Resolve the browser executable (None: not installed anywhere).
pub fn find(command: &str) -> Option<PathBuf> {
    let command = command.trim();
    if !command.is_empty() {
        return crate::harness::resolve_command(command);
    }
    if let Some(p) =
        crate::harness::resolve_command("browser").filter(|p| crate::harness::is_managed(p))
    {
        return Some(p);
    }
    for name in PATH_NAMES {
        if let Some(p) = crate::harness::resolve_command(name) {
            let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if real.starts_with("/snap") || p.starts_with("/snap") {
                continue;
            }
            return Some(p);
        }
    }
    let mac = [
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ];
    if let Some(p) = mac.iter().map(PathBuf::from).find(|p| p.is_file()) {
        return Some(p);
    }
    playwright_cache_dirs()
        .iter()
        .find_map(|d| newest_in_playwright_cache(d))
}

fn playwright_cache_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH").map(PathBuf::from)
        && p.is_absolute()
    {
        out.push(p);
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        out.push(home.join(".cache/ms-playwright"));
        out.push(home.join("Library/Caches/ms-playwright"));
    }
    out
}

/// The newest full Chromium in a Playwright browsers directory, falling
/// back to the headless shell.
pub fn newest_in_playwright_cache(dir: &Path) -> Option<PathBuf> {
    let mut builds: Vec<(u32, bool, PathBuf)> = Vec::new();
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let (shell, rev) = if let Some(r) = name.strip_prefix("chromium_headless_shell-") {
            (true, r)
        } else if let Some(r) = name.strip_prefix("chromium-") {
            (false, r)
        } else {
            continue;
        };
        let Ok(rev) = rev.parse::<u32>() else {
            continue;
        };
        let candidates: &[&str] = if shell {
            &[
                "chrome-headless-shell-linux64/chrome-headless-shell",
                "chrome-headless-shell-linux-arm64/chrome-headless-shell",
                "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                "chrome-headless-shell-mac-x64/chrome-headless-shell",
                "chrome-linux/headless_shell",
            ]
        } else {
            &[
                "chrome-linux64/chrome",
                "chrome-linux/chrome",
                "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
                "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
            ]
        };
        if let Some(p) = candidates
            .iter()
            .map(|c| e.path().join(c))
            .find(|p| p.is_file())
        {
            builds.push((rev, shell, p));
        }
    }
    // Full Chromium first (better site compatibility), newest first.
    builds.sort_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)));
    builds.into_iter().next().map(|(_, _, p)| p)
}

/// A started browser process with its protocol connection.
pub struct Launched {
    pub child: tokio::process::Child,
    pub cdp: Arc<Cdp>,
    /// The Chrome sandbox was unavailable and is off for this run.
    pub no_sandbox: bool,
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe2 fills two new descriptors we immediately own.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: both descriptors were just created and are owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Chrome's own words for "the sandbox cannot start here" (for example
/// Ubuntu's AppArmor restriction on unprivileged user namespaces).
fn sandbox_failure(stderr: &str) -> bool {
    stderr.contains("No usable sandbox") || stderr.contains("setuid sandbox")
}

/// What to tell the owner when Chromium will not start.
pub fn explain_failure(stderr: &str) -> String {
    if let Some(lib) = stderr
        .lines()
        .find(|l| l.contains("error while loading shared libraries"))
    {
        let lib = lib
            .split("libraries:")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .unwrap_or("")
            .trim()
            .to_string();
        return format!(
            "Chromium is missing system libraries ({lib}). On the server, run once as root: \
             `npx playwright install-deps chromium` (or install your distribution's Chromium)."
        );
    }
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.contains("ERROR") || l.contains("FATAL"))
        .unwrap_or("the browser exited right after starting");
    crate::util::truncate_chars(line.trim(), 300)
}

pub struct LaunchOptions<'a> {
    pub program: &'a Path,
    pub profile_dir: &'a Path,
    pub tmp_dir: &'a Path,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

/// Start the browser. Tries with Chrome's sandbox first and retries
/// without it only if the sandbox itself cannot start on this machine.
pub async fn launch(opts: &LaunchOptions<'_>) -> Result<Launched> {
    use std::sync::atomic::{AtomicBool, Ordering};
    /// Learned once per run: this system cannot start Chrome's sandbox.
    static NO_SANDBOX: AtomicBool = AtomicBool::new(false);
    release_stale_profile_lock(opts.profile_dir);
    let known = NO_SANDBOX.load(Ordering::Relaxed);
    match spawn(opts, known).await {
        Ok(l) => Ok(l),
        Err(SpawnError::Sandbox) => {
            crate::warn!(
                "browser: Chrome's sandbox is unavailable on this system; running without it"
            );
            NO_SANDBOX.store(true, Ordering::Relaxed);
            spawn(opts, true).await.map_err(|e| e.into_message().into())
        }
        Err(e) => Err(e.into_message().into()),
    }
}

enum SpawnError {
    Sandbox,
    Other(String),
}

impl SpawnError {
    fn into_message(self) -> String {
        match self {
            SpawnError::Sandbox => "Chrome's sandbox could not start".into(),
            SpawnError::Other(m) => m,
        }
    }
}

impl<E: std::fmt::Display> From<E> for SpawnError {
    fn from(e: E) -> Self {
        SpawnError::Other(e.to_string())
    }
}

async fn spawn(
    opts: &LaunchOptions<'_>,
    no_sandbox: bool,
) -> std::result::Result<Launched, SpawnError> {
    let (child_in, to_browser) = pipe()?;
    let (from_browser, child_out) = pipe()?;
    let headless_shell = opts
        .program
        .file_name()
        .is_some_and(|n| n.to_string_lossy().contains("headless"));
    let mut cmd = tokio::process::Command::new(opts.program);
    if !headless_shell {
        cmd.arg("--headless=new");
    }
    cmd.args([
        "--remote-debugging-pipe",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-sync",
        "--disable-default-apps",
        "--disable-component-update",
        "--disable-background-timer-throttling",
        "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication",
        "--password-store=basic",
        "--use-mock-keychain",
        "--mute-audio",
        "--hide-scrollbars",
    ])
    .arg(format!("--user-data-dir={}", opts.profile_dir.display()))
    .arg(format!("--window-size={},{}", opts.width, opts.height))
    .arg(format!("--force-device-scale-factor={}", opts.scale));
    if no_sandbox {
        cmd.arg("--no-sandbox");
    }
    cmd.arg("about:blank");
    // Chrome keeps scratch files and its singleton socket in TMPDIR. Use
    // Iris's own directory unless the socket path would be too long for
    // Unix sockets (about 108 bytes).
    if opts.tmp_dir.as_os_str().len() <= 60 {
        cmd.env("TMPDIR", opts.tmp_dir);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let (cin, cout) = (child_in.as_raw_fd(), child_out.as_raw_fd());
    // SAFETY: only async-signal-safe calls (fcntl, dup2, setsid) between
    // fork and exec. The pipe ends move to fds 3 and 4, where Chrome's
    // `--remote-debugging-pipe` expects them; dup2 clears close-on-exec.
    unsafe {
        cmd.pre_exec(move || {
            let a = libc::fcntl(cin, libc::F_DUPFD_CLOEXEC, 10);
            let b = libc::fcntl(cout, libc::F_DUPFD_CLOEXEC, 10);
            if a < 0 || b < 0 || libc::dup2(a, 3) < 0 || libc::dup2(b, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Own process group, so signals to Iris's terminal skip it and
            // it can be stopped as a group.
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(child_in);
    drop(child_out);
    let cdp = Cdp::start(to_browser, from_browser)?;
    let mut stderr = child.stderr.take();
    // Ready when the protocol answers; a crash at start closes the pipe.
    let ready = tokio::time::timeout(
        Duration::from_secs(20),
        cdp.call("Browser.getVersion", serde_json::json!({}), None),
    )
    .await;
    match ready {
        Ok(Ok(_)) => {
            // Keep draining stderr so Chrome never blocks on a full pipe.
            if let Some(mut e) = stderr {
                tokio::spawn(async move {
                    let mut sink = vec![0u8; 8192];
                    while matches!(e.read(&mut sink).await, Ok(n) if n > 0) {}
                });
            }
            Ok(Launched {
                child,
                cdp,
                no_sandbox,
            })
        }
        _ => {
            let _ = child.start_kill();
            let mut text = String::new();
            if let Some(e) = stderr.as_mut() {
                let mut buf = Vec::new();
                let _ = tokio::time::timeout(Duration::from_secs(2), e.read_to_end(&mut buf)).await;
                text = String::from_utf8_lossy(&buf).into_owned();
            }
            let _ = child.wait().await;
            if !no_sandbox && sandbox_failure(&text) {
                return Err(SpawnError::Sandbox);
            }
            Err(SpawnError::Other(explain_failure(&text)))
        }
    }
}

/// Chrome refuses to share a profile: if a previous Iris run left a browser
/// behind (crash), stop it so this run can use the profile.
fn release_stale_profile_lock(profile: &Path) {
    let lock = profile.join("SingletonLock");
    let Ok(target) = std::fs::read_link(&lock) else {
        return;
    };
    let Some(pid) = target
        .to_string_lossy()
        .rsplit('-')
        .next()
        .and_then(|p| p.parse::<i32>().ok())
    else {
        return;
    };
    let ours = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| {
            String::from_utf8_lossy(&c).contains(&format!("--user-data-dir={}", profile.display()))
        })
        .unwrap_or(false);
    if ours {
        crate::warn!("browser: stopping a browser left over from an earlier run");
        // SAFETY: plain kill(2) on a pid whose command line names our profile.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if !Path::new(&format!("/proc/{pid}")).exists() {
                break;
            }
        }
    }
    // A lock pointing at a dead process is cleaned up by Chrome itself.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_newest_full_chromium_in_playwright_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        for (dir, bin) in [
            ("chromium-1100", "chrome-linux64/chrome"),
            ("chromium-1200", "chrome-linux64/chrome"),
            (
                "chromium_headless_shell-1300",
                "chrome-headless-shell-linux64/chrome-headless-shell",
            ),
            ("firefox-1500", "firefox/firefox"),
        ] {
            let p = d.join(dir).join(bin);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
        }
        assert_eq!(
            newest_in_playwright_cache(d).unwrap(),
            d.join("chromium-1200/chrome-linux64/chrome")
        );
        std::fs::remove_dir_all(d.join("chromium-1100")).unwrap();
        std::fs::remove_dir_all(d.join("chromium-1200")).unwrap();
        assert!(
            newest_in_playwright_cache(d)
                .unwrap()
                .ends_with("chrome-headless-shell")
        );
        assert!(newest_in_playwright_cache(&d.join("missing")).is_none());
    }

    #[test]
    fn explains_launch_failures() {
        let missing = "chrome: error while loading shared libraries: libnss3.so: cannot open shared object file";
        assert!(explain_failure(missing).contains("libnss3.so"));
        assert!(explain_failure(missing).contains("install-deps"));
        assert!(sandbox_failure(
            "[1:1:FATAL:zygote_host_impl_linux.cc(128)] No usable sandbox! If you are running on Ubuntu"
        ));
        assert!(!sandbox_failure("all good"));
        assert_eq!(
            explain_failure(""),
            "the browser exited right after starting"
        );
    }
}
