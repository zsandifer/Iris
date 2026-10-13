//! Installing and updating engines (the official CLIs) from the app.
//!
//! Sources are fixed and official only:
//! - Claude Code: Anthropic's release channel (`downloads.claude.ai`), the
//!   same steps as the official `install.sh`: read `latest`, read that
//!   version's `manifest.json`, download the platform binary, and verify
//!   its SHA-256 against the manifest before it is used.
//! - Codex, Gemini CLI, OpenCode: their official npm packages
//!   (`@openai/codex`, `@google/gemini-cli`, `opencode-ai`); npm verifies
//!   registry integrity hashes. Install scripts are not run.
//!
//! - Iris's browser: Microsoft's Playwright MCP server, the official
//!   `@playwright/mcp` npm package (npm verifies it), then its own
//!   `install-browser chromium` command, which downloads Chromium from
//!   Playwright's download servers into `<root>/chromium/browsers`; the
//!   build then moves to the fixed `<root>/chromium/app`, the one path the
//!   root setup approves for Chrome's sandbox (`deploy/setup-engines.sh`).
//!
//! Everything lands in one engines directory (`<root>/<name>/...`, with
//! launchers in `<root>/bin`) that Iris searches before PATH. Only engines
//! Iris installed there are ever updated or removed; a CLI the operator
//! installed themselves is never touched.
//!
//! Privilege split: with `engines.helper` set, the server never downloads
//! anything itself. It runs the helper (for example `sudo -n -u iris
//! /usr/local/bin/iris engines`), a separate user that owns the engines
//! directory; the agent user can execute the engines but not modify them.
//! See `deploy/setup-engines.sh`. Without a helper (single-user mode) the
//! same code runs in-process as the current user.

use crate::util::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CLAUDE_RELEASES: &str = "https://downloads.claude.ai/claude-code-releases";
/// Largest Claude binary Iris will accept (the manifest size is enforced
/// first; this is a backstop).
const MAX_BINARY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    ClaudeNative,
    Npm {
        package: &'static str,
        launcher: Launcher,
    },
    /// Playwright MCP, then Chromium via its `install-browser chromium`.
    PlaywrightMcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launcher {
    /// A Node.js script in `node_modules/.bin/<name>`.
    NodeScript(&'static str),
    /// OpenCode's native binary from its platform package.
    OpenCodeNative,
}

#[derive(Debug, Clone, Copy)]
pub struct EngineSource {
    pub name: &'static str,
    pub label: &'static str,
    pub method: Method,
    pub source: &'static str,
}

pub const ENGINES: &[EngineSource] = &[
    EngineSource {
        name: "claude",
        label: "Claude Code",
        method: Method::ClaudeNative,
        source: "downloads.claude.ai (Anthropic release manifest, SHA-256 verified)",
    },
    EngineSource {
        name: "codex",
        label: "Codex CLI",
        method: Method::Npm {
            package: "@openai/codex",
            launcher: Launcher::NodeScript("codex"),
        },
        source: "npm: @openai/codex",
    },
    EngineSource {
        name: "gemini",
        label: "Gemini CLI",
        method: Method::Npm {
            package: "@google/gemini-cli",
            launcher: Launcher::NodeScript("gemini"),
        },
        source: "npm: @google/gemini-cli",
    },
    EngineSource {
        name: "opencode",
        label: "OpenCode",
        method: Method::Npm {
            package: "opencode-ai",
            launcher: Launcher::OpenCodeNative,
        },
        source: "npm: opencode-ai",
    },
];

/// Iris's browser (not an AI engine, but installed and updated the same way).
pub const BROWSER: EngineSource = EngineSource {
    name: "browser",
    label: "Browser (Playwright MCP)",
    method: Method::PlaywrightMcp,
    source: "npm: @playwright/mcp, then Chromium from Playwright's official downloads",
};

pub fn source(name: &str) -> Option<&'static EngineSource> {
    ENGINES
        .iter()
        .chain(std::iter::once(&BROWSER))
        .find(|e| e.name == name)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub name: String,
    pub version: Option<String>,
    pub path: Option<String>,
    /// install/update made a change (false: already up to date).
    pub changed: bool,
}

/// Where an engine's files live. The browser's are in `chromium/`, never
/// `browser/`: with `engines.dir` set to the data directory that would be
/// Iris's browser profile (sign-ins), which removing the browser must not
/// touch.
pub fn install_dir(root: &Path, name: &str) -> PathBuf {
    match name {
        "browser" => root.join("chromium"),
        n => root.join(n),
    }
}

pub fn bin_dir(root: &Path) -> PathBuf {
    root.join("bin")
}

/// Launcher path for an engine Iris installed.
pub fn launcher(root: &Path, name: &str) -> PathBuf {
    bin_dir(root).join(name)
}

pub fn is_installed(root: &Path, name: &str) -> bool {
    launcher(root, name).exists()
}

/// Version recorded at install time.
pub fn installed_version(root: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(install_dir(root, name).join("VERSION"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn valid_version(v: &str) -> bool {
    let core = v.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && v.len() <= 64
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30 * 60)))
        .timeout_connect(Some(Duration::from_secs(20)))
        .build()
        .into()
}

fn get_text(url: &str, limit: u64) -> Result<String> {
    let mut res = agent().get(url).call()?;
    Ok(res.body_mut().with_config().limit(limit).read_to_string()?)
}

/// Claude Code's platform id, as used in the release manifest.
pub fn claude_platform() -> Result<String> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => return Err(format!("unsupported OS {other}").into()),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => return Err(format!("unsupported architecture {other}").into()),
    };
    let musl = os == "linux"
        && (Path::new("/lib/libc.musl-x86_64.so.1").exists()
            || Path::new("/lib/libc.musl-aarch64.so.1").exists());
    Ok(if musl {
        format!("{os}-{arch}-musl")
    } else {
        format!("{os}-{arch}")
    })
}

/// (sha256 hex, size) for a platform from a Claude release manifest.
pub fn manifest_entry(manifest: &str, platform: &str) -> Result<(String, u64)> {
    let v: serde_json::Value = serde_json::from_str(manifest)?;
    let p = &v["platforms"][platform];
    let checksum = p["checksum"]
        .as_str()
        .filter(|c| c.len() == 64 && c.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| format!("platform {platform} not in manifest"))?
        .to_lowercase();
    let size = p["size"].as_u64().unwrap_or(0);
    Ok((checksum, size))
}

/// Copy `r` into `w` while hashing; refuses more than `max` bytes.
pub fn copy_hashed(mut r: impl Read, mut w: impl Write, max: u64) -> Result<(String, u64)> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max {
            return Err("download is larger than expected".into());
        }
        h.update(&buf[..n]);
        w.write_all(&buf[..n])?;
    }
    w.flush()?;
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((hex, total))
}

/// Point `<root>/bin/<name>` at `target` atomically.
fn set_launcher_symlink(root: &Path, name: &str, target: &Path) -> Result<()> {
    std::fs::create_dir_all(bin_dir(root))?;
    let link = launcher(root, name);
    let tmp = bin_dir(root).join(format!(".{name}.{}", crate::util::new_id()));
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, &link)?;
    Ok(())
}

/// Write `<root>/bin/<name>` as a small launcher script.
fn set_launcher_script(root: &Path, name: &str, body: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(bin_dir(root))?;
    let tmp = bin_dir(root).join(format!(".{name}.{}", crate::util::new_id()));
    std::fs::write(&tmp, body)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, launcher(root, name))?;
    Ok(())
}

pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn install_claude(root: &Path) -> Result<Report> {
    use std::os::unix::fs::PermissionsExt;
    let platform = claude_platform()?;
    let version = get_text(&format!("{CLAUDE_RELEASES}/latest"), 1024)?
        .trim()
        .to_string();
    if !valid_version(&version) {
        return Err("the Claude release channel returned an unexpected version".into());
    }
    let dir = root.join("claude");
    let target = dir.join(format!("claude-{version}"));
    if installed_version(root, "claude").as_deref() == Some(version.as_str())
        && target.exists()
        && is_installed(root, "claude")
    {
        return Ok(Report {
            name: "claude".into(),
            version: Some(version),
            path: Some(launcher(root, "claude").to_string_lossy().into_owned()),
            changed: false,
        });
    }
    let manifest = get_text(
        &format!("{CLAUDE_RELEASES}/{version}/manifest.json"),
        1 << 20,
    )?;
    let (checksum, size) = manifest_entry(&manifest, &platform)?;
    std::fs::create_dir_all(&dir)?;
    let part = dir.join(format!(".claude-{version}.part"));
    let res = agent()
        .get(&format!("{CLAUDE_RELEASES}/{version}/{platform}/claude"))
        .call()?;
    let max = if size > 0 { size } else { MAX_BINARY_BYTES };
    let file = std::fs::File::create(&part)?;
    let (_, body) = res.into_parts();
    let copied = copy_hashed(body.into_reader(), std::io::BufWriter::new(file), max);
    let (actual, total) = match copied {
        Ok(x) => x,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    if actual != checksum || (size > 0 && total != size) {
        let _ = std::fs::remove_file(&part);
        return Err("Claude Code download failed checksum verification".into());
    }
    std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&part, &target)?;
    set_launcher_symlink(root, "claude", &target)?;
    std::fs::write(dir.join("VERSION"), &version)?;
    // Keep only the version in use.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("claude-") && p != target {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    Ok(Report {
        name: "claude".into(),
        version: Some(version),
        path: Some(launcher(root, "claude").to_string_lossy().into_owned()),
        changed: true,
    })
}

fn cpu_has_avx2() -> bool {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.split_whitespace().any(|w| w == "avx2"))
        .unwrap_or(false)
}

/// OpenCode's platform binary inside its npm install.
fn opencode_binary(prefix: &Path) -> Option<PathBuf> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        o => o,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        a => a,
    };
    let base = format!("opencode-{os}-{arch}");
    let mut names = Vec::new();
    if arch == "x64" && !cpu_has_avx2() {
        names.push(format!("{base}-baseline"));
    }
    names.extend([
        base.clone(),
        format!("{base}-musl"),
        format!("{base}-baseline"),
    ]);
    names
        .into_iter()
        .map(|n| prefix.join("node_modules").join(n).join("bin/opencode"))
        .find(|p| p.is_file())
}

/// How long one `npm install` may take before it is stopped.
const NPM_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Run a command to completion within `limit`, capturing its output. On
/// timeout the whole process group is killed.
fn output_with_timeout(
    mut cmd: std::process::Command,
    limit: Duration,
) -> Result<std::process::Output> {
    use std::os::unix::process::CommandExt;
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn()?;
    let drain = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if std::time::Instant::now() >= deadline {
            if let Ok(pid) = i32::try_from(child.id()) {
                // SAFETY: plain syscall on the process group created above.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = child.wait();
            return Err(format!("timed out after {} seconds", limit.as_secs()).into());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// npm installs go into a fresh `<root>/<name>/pkg-<id>` folder; the
/// launcher switches to it only once it is complete, and older folders are
/// removed after that. A failed or stopped update leaves the working
/// engine untouched.
fn install_npm(root: &Path, name: &str, package: &str, launcher_kind: Launcher) -> Result<Report> {
    let npm = crate::harness::resolve_command("npm").ok_or_else(|| {
        format!("Node.js (npm) is needed to install {name}; install Node.js on the server first")
    })?;
    install_npm_with(root, name, package, launcher_kind, &npm)
}

fn install_npm_with(
    root: &Path,
    name: &str,
    package: &str,
    launcher_kind: Launcher,
    npm: &Path,
) -> Result<Report> {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir)?;
    let before = installed_version(root, name);
    let prefix = dir.join(format!("pkg-{}", crate::util::new_id()));
    std::fs::create_dir_all(&prefix)?;
    let version = match npm_install_into(root, name, package, launcher_kind, npm, &prefix) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&prefix);
            return Err(e);
        }
    };
    if let Some(v) = &version {
        std::fs::write(dir.join("VERSION"), v)?;
    }
    // The launcher points into the new folder now; drop older installs.
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let path = entry.path();
        if path == prefix || entry.file_name() == "VERSION" {
            continue;
        }
        let _ = if entry.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
    Ok(Report {
        name: name.into(),
        changed: before != version,
        version,
        path: Some(launcher(root, name).to_string_lossy().into_owned()),
    })
}

/// Install `package` into `prefix` and point the launcher at it. Returns
/// the installed version.
fn npm_install_into(
    root: &Path,
    name: &str,
    package: &str,
    launcher_kind: Launcher,
    npm: &Path,
    prefix: &Path,
) -> Result<Option<String>> {
    let mut cmd = std::process::Command::new(npm);
    cmd.args([
        "install",
        "--prefix",
        &prefix.to_string_lossy(),
        &format!("{package}@latest"),
        "--no-fund",
        "--no-audit",
        "--ignore-scripts",
        "--omit=dev",
        "--no-update-notifier",
    ])
    .env("npm_config_cache", root.join(".npm-cache"))
    .env("npm_config_update_notifier", "false")
    .stdin(std::process::Stdio::null());
    let out = output_with_timeout(cmd, NPM_TIMEOUT)
        .map_err(|e| format!("npm install of {package} failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .rev()
            .find(|l| l.contains("ERR"))
            .unwrap_or("npm install failed");
        return Err(format!("npm install of {package} failed: {}", line.trim()).into());
    }
    let pkg_json = prefix
        .join("node_modules")
        .join(package)
        .join("package.json");
    let version = std::fs::read_to_string(&pkg_json)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["version"].as_str().map(String::from));
    match launcher_kind {
        Launcher::NodeScript(bin) => {
            let script = prefix.join("node_modules/.bin").join(bin);
            let script = std::fs::canonicalize(&script)
                .map_err(|_| format!("{package} did not provide a `{bin}` command"))?;
            // Pin the node binary so a minimal service PATH still works.
            let node = crate::harness::resolve_command("node")
                .ok_or("Node.js was not found next to npm")?;
            set_launcher_script(
                root,
                name,
                &format!(
                    "#!/bin/sh\n# Installed by Iris from npm ({package}).\nexec {} {} \"$@\"\n",
                    sh_quote(&node.to_string_lossy()),
                    sh_quote(&script.to_string_lossy())
                ),
            )?;
        }
        Launcher::OpenCodeNative => {
            let bin = opencode_binary(prefix)
                .ok_or("the OpenCode package has no binary for this platform")?;
            set_launcher_symlink(root, name, &bin)?;
        }
    }
    Ok(version)
}

/// Where Iris's browser lives once installed: one fixed directory, so the
/// one-time root setup can approve exactly this path for Chrome's sandbox
/// (an AppArmor profile, see `deploy/setup-engines.sh`). Updates replace
/// the directory's contents; the path never changes.
pub fn browser_app_dir(root: &Path) -> PathBuf {
    install_dir(root, "browser").join("app")
}

/// The browser's executable at its fixed path on Linux
/// (`<root>/chromium/app/chrome`).
pub fn browser_binary(root: &Path) -> PathBuf {
    browser_app_dir(root).join("chrome")
}

/// Playwright MCP's entry point inside the browser's install directory.
const MCP_CLI: &str = "node_modules/@playwright/mcp/cli.js";

/// Iris installed the browser as it is now: Playwright MCP behind its
/// launcher, and Chromium (an install from before has only Chromium, and
/// one whose first Chromium download failed has only the server; both are
/// offered setup again).
pub fn browser_ready(root: &Path) -> bool {
    install_dir(root, "browser").join(MCP_CLI).is_file()
        && is_installed(root, "browser")
        && browser_executable(root).is_some()
}

/// The installed Chromium's executable (Linux, then the macOS app bundles
/// Playwright ships).
pub fn browser_executable(root: &Path) -> Option<PathBuf> {
    let app = browser_app_dir(root);
    std::iter::once(browser_binary(root))
        .chain(
            [
                "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                "Chromium.app/Contents/MacOS/Chromium",
            ]
            .iter()
            .map(|c| app.join(c)),
        )
        .find(|p| p.is_file())
}

/// Major version of the Chromium build Iris installed (for a user agent
/// without "HeadlessChrome").
pub fn chromium_major(root: &Path) -> Option<String> {
    let v = std::fs::read_to_string(install_dir(root, "browser").join("CHROMIUM_VERSION")).ok()?;
    let major = v.trim().split('.').next()?;
    (!major.is_empty() && major.len() <= 4 && major.bytes().all(|b| b.is_ascii_digit()))
        .then(|| major.to_string())
}

/// The Chromium build the installed `playwright-core` pins
/// (`browsers.json`): its revision, and its version when given, so an
/// unchanged build is not downloaded again.
fn pinned_chromium(prefix: &Path) -> Option<(String, Option<String>)> {
    let text =
        std::fs::read_to_string(prefix.join("node_modules/playwright-core/browsers.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let entry = v["browsers"]
        .as_array()?
        .iter()
        .find(|b| b["name"] == "chromium")?;
    let rev = entry["revision"]
        .as_str()
        .filter(|r| !r.is_empty() && r.len() <= 12 && r.bytes().all(|b| b.is_ascii_digit()))?;
    let version = entry["browserVersion"]
        .as_str()
        .filter(|v| v.len() <= 32 && v.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
        .map(String::from);
    Some((rev.to_string(), version))
}

/// The newest full Chromium in a Playwright browsers directory, falling
/// back to the headless shell.
fn newest_in_playwright_cache(dir: &Path) -> Option<PathBuf> {
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
                "chrome-linux-arm64/chrome",
                "chrome-linux/chrome",
                "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
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

/// Move a Chromium build Playwright downloaded (`bin`, inside
/// `<browsers>/chromium-<rev>/<platform>/`) to the fixed browser directory,
/// replacing an older build. Returns the executable at its new path.
fn adopt_browser_build(root: &Path, browsers: &Path, bin: &Path) -> Result<PathBuf> {
    let rel = bin
        .strip_prefix(browsers)
        .map_err(|_| "the Chromium download is outside its directory")?;
    let mut parts = rel.components();
    let (Some(build), Some(platform)) = (parts.next(), parts.next()) else {
        return Err("unexpected Chromium download layout".into());
    };
    let exe = parts.as_path().to_path_buf();
    if exe.as_os_str().is_empty() {
        return Err("unexpected Chromium download layout".into());
    }
    let from = browsers.join(build).join(platform);
    let app = browser_app_dir(root);
    let parent = app.parent().ok_or("no browser directory")?;
    let id = crate::util::new_id();
    let staged = parent.join(format!(".app.new-{id}"));
    std::fs::rename(&from, &staged)?;
    let old = parent.join(format!(".app.old-{id}"));
    let had_old = match std::fs::rename(&app, &old) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e.into());
        }
    };
    if let Err(e) = std::fs::rename(&staged, &app) {
        if had_old {
            let _ = std::fs::rename(&old, &app);
        }
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e.into());
    }
    if had_old {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(app.join(exe))
}

/// Iris's browser: the official `@playwright/mcp` package, then its own
/// installer, which downloads Chromium from Playwright's servers into
/// `<root>/chromium/browsers`. The build then moves to the fixed
/// `<root>/chromium/app` (see [`browser_app_dir`]) and the download
/// directory is removed. `<root>/bin/browser` starts the server.
fn install_browser(root: &Path) -> Result<Report> {
    let npm = crate::harness::resolve_command("npm").ok_or(
        "Node.js (npm) is needed to install the browser; install Node.js on the server first",
    )?;
    install_browser_with(root, &npm, &|| {
        crate::harness::resolve_command("node")
            .ok_or_else(|| "Node.js was not found next to npm".into())
    })
}

fn install_browser_with(
    root: &Path,
    npm: &Path,
    node: &dyn Fn() -> Result<PathBuf>,
) -> Result<Report> {
    const PACKAGE: &str = "@playwright/mcp";
    let prefix = install_dir(root, "browser");
    std::fs::create_dir_all(&prefix)?;
    let before = installed_version(root, "browser");
    let mut cmd = std::process::Command::new(npm);
    cmd.args([
        "install",
        "--prefix",
        &prefix.to_string_lossy(),
        &format!("{PACKAGE}@latest"),
        "--no-fund",
        "--no-audit",
        "--ignore-scripts",
        "--omit=dev",
        "--no-update-notifier",
    ])
    .env("npm_config_cache", root.join(".npm-cache"))
    .env("npm_config_update_notifier", "false")
    .stdin(std::process::Stdio::null());
    let out = output_with_timeout(cmd, NPM_TIMEOUT)
        .map_err(|e| format!("npm install of {PACKAGE} failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .rev()
            .find(|l| l.contains("ERR"))
            .unwrap_or("npm install failed");
        return Err(format!("npm install of {PACKAGE} failed: {}", line.trim()).into());
    }
    let cli = prefix.join(MCP_CLI);
    if !cli.is_file() {
        return Err(format!("{PACKAGE} did not provide its server").into());
    }
    let mcp_version =
        std::fs::read_to_string(prefix.join("node_modules/@playwright/mcp/package.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v["version"].as_str().map(String::from))
            .filter(|v| valid_version(v));
    let node = node()?;
    // The server starts from here on (also with the Chromium already in
    // place if its download below fails).
    set_launcher_script(
        root,
        "browser",
        &format!(
            "#!/bin/sh\n# Installed by Iris from npm ({PACKAGE}).\nexec {} {} \"$@\"\n",
            sh_quote(&node.to_string_lossy()),
            sh_quote(&cli.to_string_lossy())
        ),
    )?;
    let pinned = pinned_chromium(&prefix);
    let have = std::fs::read_to_string(prefix.join("CHROMIUM_BUILD"))
        .ok()
        .map(|s| s.trim().to_string());
    let build = match &pinned {
        // The pinned build is already in place: nothing to download.
        Some((rev, _)) if have.as_ref() == Some(rev) && browser_executable(root).is_some() => {
            rev.clone()
        }
        _ => {
            let browsers = prefix.join("browsers");
            let mut cmd = std::process::Command::new(&node);
            cmd.arg(&cli)
                .args(["install-browser", "chromium", "--no-shell"])
                .env("PLAYWRIGHT_BROWSERS_PATH", &browsers)
                .env("PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS", "1")
                .stdin(std::process::Stdio::null());
            let out = output_with_timeout(cmd, NPM_TIMEOUT)
                .map_err(|e| format!("Chromium install failed: {e}"))?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                let line = err
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("Chromium download failed");
                return Err(format!(
                    "Chromium install failed: {}",
                    crate::util::truncate_chars(line.trim(), 200)
                )
                .into());
            }
            let bin = newest_in_playwright_cache(&browsers)
                .ok_or("the Chromium download has no browser for this platform")?;
            let rev = bin
                .strip_prefix(&browsers)
                .ok()
                .and_then(|r| r.components().next())
                .and_then(|c| c.as_os_str().to_str())
                .and_then(|n| n.strip_prefix("chromium-"))
                .unwrap_or("")
                .to_string();
            adopt_browser_build(root, &browsers, &bin)?;
            // The download directory only held this build (now moved) and
            // Playwright's markers.
            let _ = std::fs::remove_dir_all(&browsers);
            std::fs::write(prefix.join("CHROMIUM_BUILD"), &rev)?;
            match pinned
                .as_ref()
                .filter(|(r, _)| *r == rev)
                .and_then(|(_, v)| v.as_ref())
            {
                Some(v) => std::fs::write(prefix.join("CHROMIUM_VERSION"), v)?,
                None => {
                    let _ = std::fs::remove_file(prefix.join("CHROMIUM_VERSION"));
                }
            }
            rev
        }
    };
    let version = format!(
        "{} (Chromium build {build})",
        mcp_version.as_deref().unwrap_or("unknown")
    );
    std::fs::write(prefix.join("VERSION"), &version)?;
    Ok(Report {
        name: "browser".into(),
        changed: before.as_deref() != Some(version.as_str()),
        version: Some(version),
        path: Some(launcher(root, "browser").to_string_lossy().into_owned()),
    })
}

/// Install or update an engine (blocking). Idempotent.
pub fn install(root: &Path, name: &str) -> Result<Report> {
    let src = source(name).ok_or_else(|| format!("unknown engine {name}"))?;
    std::fs::create_dir_all(root)?;
    match src.method {
        Method::ClaudeNative => install_claude(root),
        Method::Npm { package, launcher } => install_npm(root, name, package, launcher),
        Method::PlaywrightMcp => install_browser(root),
    }
}

/// Remove an engine Iris installed (never anything else).
pub fn uninstall(root: &Path, name: &str) -> Result<()> {
    source(name).ok_or_else(|| format!("unknown engine {name}"))?;
    let _ = std::fs::remove_file(launcher(root, name));
    let dir = install_dir(root, name);
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// The helper writes as a separate, privileged user, so it refuses an
/// engines directory that anyone else could swap out or plant symlinks in:
/// the directory and every ancestor must be real directories owned by root
/// or the current user and not writable by others (sticky ancestors such
/// as /tmp excepted). Otherwise the agent user could point `--dir` at a
/// tree it controls whose `bin` links into the protected engines directory.
pub fn check_trusted_root(root: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !root.is_absolute() || root.canonicalize()? != root {
        return Err(format!(
            "{} must be an absolute path without symlinks",
            root.display()
        )
        .into());
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let mut dir = Some(root);
    while let Some(d) = dir {
        let m = std::fs::symlink_metadata(d)?;
        let shared_write = m.mode() & 0o022 != 0;
        let sticky = m.mode() & 0o1000 != 0;
        if !m.is_dir()
            || (m.uid() != 0 && m.uid() != euid)
            || (shared_write && (d == root || !sticky))
        {
            return Err(format!(
                "refusing engines directory {}: {} must be owned by root or this user and not writable by others",
                root.display(),
                d.display()
            )
            .into());
        }
        dir = d.parent();
    }
    Ok(())
}

/// `iris engines <install|update|uninstall|list> [name] --dir <root>`:
/// the privileged helper entry point. Prints one JSON object.
pub fn cli(args: &[String], root: &Path) -> Result<()> {
    if matches!(
        args.first().map(String::as_str),
        Some("install" | "update" | "uninstall")
    ) {
        // The helper may inherit the service's UMask=0077 through sudo, but
        // the agent user must be able to read and run what it installs.
        // SAFETY: umask has no preconditions and cannot fail.
        unsafe {
            libc::umask(0o022);
        }
        std::fs::create_dir_all(root)?;
        check_trusted_root(root)?;
    }
    let out = match (args.first().map(String::as_str), args.get(1)) {
        (Some("install" | "update"), Some(name)) => serde_json::to_value(install(root, name)?)?,
        (Some("uninstall"), Some(name)) => {
            uninstall(root, name)?;
            serde_json::json!({"name": name, "removed": true})
        }
        (Some("list"), _) => serde_json::Value::Array(
            ENGINES
                .iter()
                .chain(std::iter::once(&BROWSER))
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "installed": is_installed(root, e.name),
                        "version": installed_version(root, e.name),
                    })
                })
                .collect(),
        ),
        _ => {
            return Err(
                "usage: iris engines <install|update|uninstall> <claude|codex|gemini|opencode|browser> --dir <path> | iris engines list --dir <path>"
                    .into(),
            );
        }
    };
    println!("{out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(valid_version("2.1.296"));
        assert!(valid_version("1.0.0-beta.1"));
        assert!(!valid_version("<html>"));
        assert!(!valid_version("1.2"));
        assert!(!valid_version("1.2.3; rm -rf /"));
    }

    #[test]
    fn manifest_parsing() {
        let m = r#"{"version": "9.9.9", "platforms": {
            "linux-x64": {"binary": "claude", "checksum": "AB24972e3bc859fab2b46ed4c1e51f7d6130f06d3bd550811a114640de3370d0", "size": 10},
            "linux-arm64": {"checksum": "nothex"}}}"#;
        let (c, s) = manifest_entry(m, "linux-x64").unwrap();
        assert_eq!((c.len(), s), (64, 10));
        assert!(c.starts_with("ab"));
        assert!(manifest_entry(m, "linux-arm64").is_err());
        assert!(manifest_entry(m, "darwin-x64").is_err());
    }

    #[test]
    fn hashed_copy_enforces_size() {
        let mut out = Vec::new();
        let (hex, n) = copy_hashed(&b"abc"[..], &mut out, 10).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(copy_hashed(&b"abcdef"[..], Vec::new(), 5).is_err());
    }

    #[test]
    fn launchers_and_uninstall_stay_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("codex")).unwrap();
        std::fs::write(root.join("codex/VERSION"), "1.2.3\n").unwrap();
        set_launcher_script(root, "codex", "#!/bin/sh\necho hi\n").unwrap();
        assert!(is_installed(root, "codex"));
        assert_eq!(installed_version(root, "codex").as_deref(), Some("1.2.3"));
        let outside = tmp.path().join("keep");
        std::fs::write(&outside, "x").unwrap();
        uninstall(root, "codex").unwrap();
        assert!(!is_installed(root, "codex") && !root.join("codex").exists());
        assert!(outside.exists());
        assert!(uninstall(root, "../etc").is_err());
        // Removing the browser never touches a profile in `browser/` (the
        // layout when engines.dir is the data directory).
        std::fs::create_dir_all(root.join("browser/profile")).unwrap();
        std::fs::create_dir_all(root.join("chromium")).unwrap();
        uninstall(root, "browser").unwrap();
        assert!(root.join("browser/profile").exists() && !root.join("chromium").exists());
        assert!(install(root, "../etc").is_err());
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn browser_builds_move_to_one_fixed_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert_eq!(browser_binary(root), root.join("chromium/app/chrome"));
        // The path the root setup approves is this same one.
        let script = include_str!("../deploy/setup-engines.sh");
        assert!(script.contains("BROWSER_BIN=\"$ENGINES_DIR/chromium/app/chrome\""));
        let browsers = root.join("chromium/browsers");
        let download = |rev: &str, marker: &str| {
            let dir = browsers.join(format!("chromium-{rev}/chrome-linux64"));
            std::fs::create_dir_all(dir.join("locales")).unwrap();
            std::fs::write(dir.join("chrome"), marker).unwrap();
            std::fs::write(
                browsers.join(format!("chromium-{rev}/INSTALLATION_COMPLETE")),
                "",
            )
            .unwrap();
            newest_in_playwright_cache(&browsers).unwrap()
        };
        let bin = adopt_browser_build(root, &browsers, &download("1200", "v1")).unwrap();
        assert_eq!(bin, browser_binary(root));
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "v1");
        assert!(root.join("chromium/app/locales").is_dir());
        assert_eq!(browser_executable(root), Some(bin.clone()));
        std::fs::remove_dir_all(&browsers).unwrap();
        // An update replaces the contents; the path stays.
        let bin2 = adopt_browser_build(root, &browsers, &download("1300", "v2")).unwrap();
        assert_eq!(bin2, bin);
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "v2");
        let leftovers: Vec<_> = std::fs::read_dir(root.join("chromium"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".app"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // Outside the download directory: refused.
        assert!(adopt_browser_build(root, &browsers, Path::new("/bin/sh")).is_err());
        assert!(browser_executable(&root.join("missing")).is_none());
    }

    #[test]
    fn browser_install_update_and_failed_download() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("engines");
        let tools = tmp.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        let script = |name: &str, body: &str| {
            let p = tools.join(name);
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        // npm: "installs" @playwright/mcp (version in mcp) with a
        // playwright-core pinning the revision in rev.
        let npm = script(
            "npm",
            r#"#!/bin/sh
t="$(dirname "$0")"
m="$3/node_modules/@playwright/mcp"
mkdir -p "$m" "$3/node_modules/playwright-core"
echo '// cli' > "$m/cli.js"
printf '{"name": "@playwright/mcp", "version": "%s"}\n' "$(cat "$t/mcp")" > "$m/package.json"
printf '{"browsers": [{"name": "chromium", "revision": "%s", "browserVersion": "150.0.1.2"}]}\n' "$(cat "$t/rev")" > "$3/node_modules/playwright-core/browsers.json"
"#,
        );
        // node cli.js install-browser chromium: downloads that revision (or
        // fails halfway when fail exists), counting its runs.
        let node = script(
            "node",
            r#"#!/bin/sh
t="$(dirname "$0")"
[ "$2" = install-browser ] || exit 2
echo run >> "$t/downloads"
r="$(cat "$t/rev")"
d="$PLAYWRIGHT_BROWSERS_PATH/chromium-$r/chrome-linux64"
mkdir -p "$d"
[ -f "$t/fail" ] && { echo 'Error: download failed' >&2; exit 1; }
echo "chrome $r" > "$d/chrome"
chmod 755 "$d/chrome"
touch "$PLAYWRIGHT_BROWSERS_PATH/chromium-$r/INSTALLATION_COMPLETE"
"#,
        );
        let downloads = || {
            std::fs::read_to_string(tools.join("downloads"))
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };
        let install = || install_browser_with(&root, &npm, &|| Ok(node.clone()));
        let fixed = browser_binary(&root);
        let launcher_text = || std::fs::read_to_string(launcher(&root, "browser")).unwrap();

        std::fs::write(tools.join("mcp"), "0.0.80").unwrap();
        std::fs::write(tools.join("rev"), "1200").unwrap();
        // A first install whose Chromium download fails is not ready (the
        // app keeps offering setup).
        std::fs::write(tools.join("fail"), "").unwrap();
        assert!(install().is_err());
        assert!(!browser_ready(&root));
        std::fs::remove_file(tools.join("fail")).unwrap();
        let r = install().unwrap();
        assert!(r.changed);
        assert_eq!(r.version.as_deref(), Some("0.0.80 (Chromium build 1200)"));
        assert!(browser_ready(&root));
        // The launcher starts Playwright MCP with the node found at install.
        assert!(
            launcher_text().contains("@playwright/mcp/cli.js"),
            "{}",
            launcher_text()
        );
        assert!(launcher_text().contains(&*node.to_string_lossy()));
        assert_eq!(std::fs::read_to_string(&fixed).unwrap(), "chrome 1200\n");
        assert_eq!(chromium_major(&root).as_deref(), Some("150"));
        assert_eq!(
            installed_version(&root, "browser").as_deref(),
            Some("0.0.80 (Chromium build 1200)")
        );
        assert!(
            !root.join("chromium/browsers").exists(),
            "download dir removed"
        );
        assert_eq!(downloads(), 2);

        // Same pinned build: nothing is downloaded; a new server version
        // still counts as a change.
        std::fs::write(tools.join("mcp"), "0.0.81").unwrap();
        let r = install().unwrap();
        assert!(r.changed);
        assert_eq!(r.version.as_deref(), Some("0.0.81 (Chromium build 1200)"));
        assert!(!install().unwrap().changed);
        assert_eq!(downloads(), 2);

        // A failed download keeps the working browser and its version.
        std::fs::write(tools.join("rev"), "1300").unwrap();
        std::fs::write(tools.join("fail"), "").unwrap();
        assert!(install().is_err());
        assert_eq!(std::fs::read_to_string(&fixed).unwrap(), "chrome 1200\n");
        assert_eq!(
            installed_version(&root, "browser").as_deref(),
            Some("0.0.81 (Chromium build 1200)")
        );
        assert!(browser_ready(&root));

        // A new build replaces it at the same path.
        std::fs::remove_file(tools.join("fail")).unwrap();
        let r = install().unwrap();
        assert!(r.changed);
        assert_eq!(r.version.as_deref(), Some("0.0.81 (Chromium build 1300)"));
        assert_eq!(std::fs::read_to_string(&fixed).unwrap(), "chrome 1300\n");
        assert!(!root.join("chromium/browsers").exists());

        // An install from before Playwright MCP (the launcher pointed at
        // Chromium itself) is not ready until updated.
        set_launcher_symlink(&root, "browser", &fixed).unwrap();
        std::fs::remove_dir_all(root.join("chromium/node_modules")).unwrap();
        assert!(!browser_ready(&root));
        install().unwrap();
        assert!(browser_ready(&root));
        assert!(launcher_text().starts_with("#!/bin/sh"));
    }

    #[test]
    fn reads_the_chromium_build_playwright_pins() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path();
        assert_eq!(pinned_chromium(prefix), None);
        let dir = prefix.join("node_modules/playwright-core");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("browsers.json"),
            r#"{"comment": "x", "browsers": [
                {"name": "chromium-headless-shell", "revision": "1100"},
                {"name": "chromium", "revision": "1200", "browserVersion": "150.0.1.2"},
                {"name": "ffmpeg", "revision": "1011"}]}"#,
        )
        .unwrap();
        assert_eq!(
            pinned_chromium(prefix),
            Some(("1200".into(), Some("150.0.1.2".into())))
        );
        std::fs::write(
            dir.join("browsers.json"),
            r#"{"browsers": [{"name": "chromium", "revision": "12/../x"}]}"#,
        )
        .unwrap();
        assert_eq!(pinned_chromium(prefix), None);
        // The version only ever feeds a user agent: digits and dots.
        std::fs::write(
            dir.join("browsers.json"),
            r#"{"browsers": [{"name": "chromium", "revision": "1", "browserVersion": "1; x"}]}"#,
        )
        .unwrap();
        assert_eq!(pinned_chromium(prefix), Some(("1".into(), None)));
        std::fs::create_dir_all(prefix.join("e/chromium")).unwrap();
        std::fs::write(prefix.join("e/chromium/CHROMIUM_VERSION"), "x.1").unwrap();
        assert_eq!(chromium_major(&prefix.join("e")), None);
    }

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
        // Current layouts on other platforms.
        for (dir, bin) in [
            ("a/chromium-1300", "chrome-linux-arm64/chrome"),
            (
                "b/chromium-1300",
                "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            ),
            (
                "c/chromium-1300",
                "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            ),
        ] {
            let p = d.join(dir).join(bin);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            let root = d.join(dir).parent().unwrap().to_path_buf();
            assert_eq!(newest_in_playwright_cache(&root).unwrap(), p);
        }
    }

    #[test]
    fn commands_are_stopped_at_their_time_limit() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "echo out; echo err >&2"]);
        let out = output_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert_eq!(
            (&out.stdout[..], &out.stderr[..]),
            (&b"out\n"[..], &b"err\n"[..])
        );
        let start = std::time::Instant::now();
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30 & sleep 30"]);
        let err = output_with_timeout(cmd, Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_failed_npm_update_keeps_the_working_engine() {
        use std::os::unix::fs::PermissionsExt;
        if crate::harness::resolve_command("node").is_none() {
            eprintln!("skipped: node not installed");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("engines");
        // A stand-in for npm (`npm install --prefix <dir> ...`): installs
        // the version in npm.ver, or fails halfway when npm.fail exists.
        let npm = tmp.path().join("npm");
        std::fs::write(
            &npm,
            r#"#!/bin/sh
prefix="$3"
mkdir -p "$prefix/node_modules/pkg" "$prefix/node_modules/.bin"
[ -f "$0.fail" ] && { echo 'npm ERR! network' >&2; exit 1; }
v=$(cat "$0.ver")
printf '{"version": "%s"}\n' "$v" > "$prefix/node_modules/pkg/package.json"
echo 'console.log(1)' > "$prefix/node_modules/.bin/tool"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
        let install =
            || install_npm_with(&root, "codex", "pkg", Launcher::NodeScript("tool"), &npm);
        std::fs::write(tmp.path().join("npm.ver"), "1.0.0").unwrap();
        let first = install().unwrap();
        assert_eq!(first.version.as_deref(), Some("1.0.0"));
        let launcher_v1 = std::fs::read_to_string(launcher(&root, "codex")).unwrap();
        let script = launcher_v1.split('\'').nth(3).unwrap().to_string();
        assert!(Path::new(&script).is_file(), "{launcher_v1}");
        // The update fails partway: the working engine is untouched.
        std::fs::write(tmp.path().join("npm.fail"), "").unwrap();
        assert!(install().is_err());
        assert_eq!(
            std::fs::read_to_string(launcher(&root, "codex")).unwrap(),
            launcher_v1
        );
        assert_eq!(installed_version(&root, "codex").as_deref(), Some("1.0.0"));
        assert!(Path::new(&script).is_file(), "v1 files are still there");
        let entries = || std::fs::read_dir(root.join("codex")).unwrap().count();
        assert_eq!(entries(), 2, "one install folder plus VERSION");
        // A good update switches over and removes the old install.
        std::fs::remove_file(tmp.path().join("npm.fail")).unwrap();
        std::fs::write(tmp.path().join("npm.ver"), "2.0.0").unwrap();
        let second = install().unwrap();
        assert_eq!(second.version.as_deref(), Some("2.0.0"));
        assert!(second.changed);
        assert!(!Path::new(&script).exists(), "old install removed");
        assert_eq!(entries(), 2);
    }

    #[test]
    fn helper_only_writes_into_a_trusted_root() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("engines");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        check_trusted_root(&root).unwrap();
        // Writable by others: anyone could plant a `bin` symlink.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_trusted_root(&root).is_err());
        let args = ["uninstall".to_string(), "codex".to_string()];
        assert!(cli(&args, &root).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A writable, non-sticky ancestor could swap the whole tree.
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_trusted_root(&root).is_err());
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o1777)).unwrap();
        check_trusted_root(&root).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Paths through symlinks are refused.
        let link = base.join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(check_trusted_root(&link).is_err());
        assert!(check_trusted_root(Path::new("relative")).is_err());
    }
}
