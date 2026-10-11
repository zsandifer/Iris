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

pub fn source(name: &str) -> Option<&'static EngineSource> {
    ENGINES.iter().find(|e| e.name == name)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub name: String,
    pub version: Option<String>,
    pub path: Option<String>,
    /// install/update made a change (false: already up to date).
    pub changed: bool,
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
    std::fs::read_to_string(root.join(name).join("VERSION"))
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

fn sh_quote(s: &str) -> String {
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

fn install_npm(root: &Path, name: &str, package: &str, launcher_kind: Launcher) -> Result<Report> {
    let npm = crate::harness::resolve_command("npm").ok_or_else(|| {
        format!("Node.js (npm) is needed to install {name}; install Node.js on the server first")
    })?;
    let prefix = root.join(name);
    std::fs::create_dir_all(&prefix)?;
    let before = installed_version(root, name);
    let out = std::process::Command::new(&npm)
        .args([
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
        .stdin(std::process::Stdio::null())
        .output()?;
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
            let bin = opencode_binary(&prefix)
                .ok_or("the OpenCode package has no binary for this platform")?;
            set_launcher_symlink(root, name, &bin)?;
        }
    }
    if let Some(v) = &version {
        std::fs::write(prefix.join("VERSION"), v)?;
    }
    Ok(Report {
        name: name.into(),
        changed: before != version,
        version,
        path: Some(launcher(root, name).to_string_lossy().into_owned()),
    })
}

/// Install or update an engine (blocking). Idempotent.
pub fn install(root: &Path, name: &str) -> Result<Report> {
    let src = source(name).ok_or_else(|| format!("unknown engine {name}"))?;
    std::fs::create_dir_all(root)?;
    match src.method {
        Method::ClaudeNative => install_claude(root),
        Method::Npm { package, launcher } => install_npm(root, name, package, launcher),
    }
}

/// Remove an engine Iris installed (never anything else).
pub fn uninstall(root: &Path, name: &str) -> Result<()> {
    source(name).ok_or_else(|| format!("unknown engine {name}"))?;
    let _ = std::fs::remove_file(launcher(root, name));
    let dir = root.join(name);
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// `iris engines <install|update|uninstall|list> [name] --dir <root>`:
/// the privileged helper entry point. Prints one JSON object.
pub fn cli(args: &[String], root: &Path) -> Result<()> {
    let out = match (args.first().map(String::as_str), args.get(1)) {
        (Some("install" | "update"), Some(name)) => serde_json::to_value(install(root, name)?)?,
        (Some("uninstall"), Some(name)) => {
            uninstall(root, name)?;
            serde_json::json!({"name": name, "removed": true})
        }
        (Some("list"), _) => serde_json::Value::Array(
            ENGINES
                .iter()
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
                "usage: iris engines <install|update|uninstall> <claude|codex|gemini|opencode> --dir <path> | iris engines list --dir <path>"
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
        assert!(install(root, "../etc").is_err());
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }
}
