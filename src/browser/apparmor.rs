//! Letting Chrome's sandbox start where AppArmor restricts unprivileged
//! user namespaces (Ubuntu 23.10 and later).
//!
//! Chrome's sandbox needs a user namespace. With
//! `kernel.apparmor_restrict_unprivileged_userns = 1` only programs with an
//! AppArmor profile that allows `userns` may create one. The one-time root
//! setup (`deploy/setup-engines.sh`) installs such a profile for exactly
//! Iris's own browser at its fixed path in the protected engines directory.
//! When the sandbox still cannot start, the app shows the owner the exact
//! command for this machine (see [`sandbox_fix`]). Iris itself never writes
//! a profile or runs anything as root.
//!
//! A profile is only ever suggested for a binary that no unprivileged user
//! can replace: the file, its directory's contents, and every ancestor must
//! be owned by root (or the engines admin user of the privilege split) and
//! not writable by group or others. Otherwise any program copied to that
//! path would get user namespaces, undoing the system's hardening.

use serde_json::{Value, json};
use std::path::{Component, Path};

/// Name of the profile and of its file in `/etc/apparmor.d`.
pub const PROFILE_NAME: &str = "iris-browser";
/// Where the profile is installed.
pub const PROFILE_FILE: &str = "/etc/apparmor.d/iris-browser";
/// The kernel switch Ubuntu uses for the restriction.
const RESTRICT_SYSCTL: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
/// Most entries checked in a browser's directory before giving up.
const MAX_ENTRIES: usize = 20_000;

/// Whether this system restricts unprivileged user namespaces with
/// AppArmor (the switch exists and is on).
pub fn userns_restricted() -> bool {
    restricted_from(std::fs::read_to_string(RESTRICT_SYSCTL).ok().as_deref())
}

fn restricted_from(sysctl: Option<&str>) -> bool {
    sysctl.is_some_and(|v| v.trim() == "1")
}

/// Whether a path can go into a profile as is: absolute, normalized, and
/// only characters that have no meaning in AppArmor's path patterns
/// (letters, digits, `/ . _ + -`, and spaces, which the quotes cover).
/// Globs, variables (`@{...}`), quotes, and backslashes are refused rather
/// than escaped.
pub fn profile_path_ok(path: &Path) -> bool {
    let Some(s) = path.to_str() else {
        return false;
    };
    s.starts_with('/')
        && s.len() > 1
        && s.len() <= 1024
        && !s.ends_with('/')
        && !s.contains("//")
        && !s.contains("/./")
        && !s.ends_with("/.")
        && path
            .components()
            .skip(1)
            .all(|c| matches!(c, Component::Normal(_)))
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '+' | '-' | ' '))
}

/// The AppArmor profile that lets exactly `binary` create user namespaces
/// (and otherwise leaves it unconfined, like the profiles Ubuntu ships for
/// Chrome). Byte for byte what `deploy/setup-engines.sh` writes.
pub fn profile_text(binary: &Path) -> Option<String> {
    if !profile_path_ok(binary) {
        return None;
    }
    Some(format!(
        "# Iris's browser: allow Chrome's sandbox (user namespaces) for this one program.\n\
         abi <abi/4.0>,\n\
         include <tunables/global>\n\
         \n\
         profile {PROFILE_NAME} \"{}\" flags=(unconfined) {{\n  userns,\n}}\n",
        binary.display()
    ))
}

/// Shell commands that install and load the profile for `binary`.
pub fn profile_command(binary: &Path) -> Option<String> {
    let profile = profile_text(binary)?;
    Some(format!(
        "sudo tee {PROFILE_FILE} >/dev/null <<'EOF'\n{profile}EOF\n\
         sudo apparmor_parser -r {PROFILE_FILE}"
    ))
}

/// Whether nobody but the trusted owners can replace `binary` or what it
/// loads from its directory: the path is real (no symlinks), and it, every
/// ancestor, and everything in its directory belong to a trusted owner
/// and are not writable by group or others (symlinks inside the directory
/// only need a trusted owner; their targets are not followed).
pub fn protected(binary: &Path, trusted: &[u32]) -> bool {
    binary.is_absolute()
        && binary.canonicalize().ok().as_deref() == Some(binary)
        && chain_safe(binary, trusted)
        && binary.parent().is_some_and(|d| tree_safe(d, trusted))
}

fn safe_meta(m: &std::fs::Metadata, trusted: &[u32]) -> bool {
    use std::os::unix::fs::MetadataExt;
    trusted.contains(&m.uid()) && m.mode() & 0o022 == 0
}

/// The path and every ancestor (no sticky-directory exception: a
/// world-writable ancestor could swap the whole tree).
fn chain_safe(path: &Path, trusted: &[u32]) -> bool {
    path.ancestors()
        .all(|p| std::fs::symlink_metadata(p).is_ok_and(|m| safe_meta(&m, trusted)))
}

/// Everything in the browser's directory (the libraries and resources it
/// loads). In a shared system directory such as /usr/bin only its files
/// are checked.
fn tree_safe(dir: &Path, trusted: &[u32]) -> bool {
    use std::os::unix::fs::MetadataExt;
    let recurse = !matches!(
        dir.to_str(),
        Some("/usr/bin" | "/bin" | "/usr/local/bin" | "/usr/sbin" | "/sbin")
    );
    let mut seen = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            return false;
        };
        for e in entries {
            seen += 1;
            let Some(m) = e.ok().and_then(|e| {
                std::fs::symlink_metadata(e.path())
                    .ok()
                    .map(|m| (e.path(), m))
            }) else {
                return false;
            };
            let (path, m) = m;
            if seen > MAX_ENTRIES {
                return false;
            }
            if m.file_type().is_symlink() {
                if !trusted.contains(&m.uid()) {
                    return false;
                }
            } else if !safe_meta(&m, trusted) {
                return false;
            } else if m.is_dir() && recurse {
                stack.push(path);
            }
        }
    }
    true
}

/// How the owner can let the sandbox start on this machine, for the app
/// (owner only; holds paths, never secrets). Null where the cause is not
/// AppArmor's restriction.
///
/// - `apparmor`: the browser Iris uses is protected; `command` installs a
///   profile for exactly its real path.
/// - `setup`: it is not (for example a browser in a home directory);
///   `command` runs the one-time root setup, after which Iris installs its
///   own browser into the protected engines directory.
pub fn sandbox_fix(
    restricted: bool,
    binary: Option<&Path>,
    trusted: &[u32],
    setup_command: &str,
) -> Value {
    if !restricted {
        return Value::Null;
    }
    let real = binary.and_then(|b| b.canonicalize().ok());
    let path = real.as_ref().map(|p| p.to_string_lossy().into_owned());
    if let Some(real) = &real
        && protected(real, trusted)
        && let Some(command) = profile_command(real)
    {
        return json!({"kind": "apparmor", "path": path, "command": command});
    }
    json!({"kind": "setup", "path": path, "command": setup_command})
}

/// The one-time root setup for this machine: runs `deploy/setup-engines.sh`
/// (from the Iris source folder) for the account Iris runs as. The script
/// needs a root-owned `iris` binary; if this one is not, it is installed
/// first. `engines_dir` is passed when the privilege split already uses a
/// directory (the script's default is `/opt/iris/engines`).
pub fn setup_command(user: &str, iris_bin: Option<&Path>, engines_dir: Option<&Path>) -> String {
    use crate::engines::sh_quote;
    let mut out = String::new();
    let bin = match iris_bin {
        Some(b) => b.to_string_lossy().into_owned(),
        None => {
            out.push_str("sudo install -Dm755 target/release/iris /usr/local/bin/iris\n");
            "/usr/local/bin/iris".into()
        }
    };
    out.push_str(&format!(
        "sudo sh deploy/setup-engines.sh {} {}",
        sh_quote(user),
        sh_quote(&bin)
    ));
    if let Some(d) = engines_dir {
        out.push(' ');
        out.push_str(&sh_quote(&d.to_string_lossy()));
    }
    out
}

/// This Iris binary, if only root can replace it (what the setup script
/// requires before it lets sudo run it).
pub fn root_owned_iris() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    chain_safe(&exe, &[0]).then_some(exe)
}

/// The account Iris runs as.
pub fn current_user() -> Option<String> {
    // SAFETY: getpwuid_r writes into the buffers we pass; the name is read
    // only on success, while `buf` is alive.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = vec![0 as libc::c_char; 4096];
        let rc = libc::getpwuid_r(
            libc::geteuid(),
            &mut pw,
            buf.as_mut_ptr(),
            buf.len(),
            &mut out,
        );
        if rc != 0 || out.is_null() || pw.pw_name.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(pw.pw_name)
            .to_str()
            .ok()
            .map(String::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn detects_the_userns_restriction() {
        assert!(restricted_from(Some("1\n")));
        assert!(!restricted_from(Some("0\n")));
        assert!(!restricted_from(Some("")));
        assert!(!restricted_from(None));
    }

    #[test]
    fn profile_names_exactly_one_quoted_path() {
        let p = profile_text(Path::new("/opt/iris/engines/chromium/app/chrome")).unwrap();
        assert_eq!(
            p,
            "# Iris's browser: allow Chrome's sandbox (user namespaces) for this one program.\n\
             abi <abi/4.0>,\n\
             include <tunables/global>\n\
             \n\
             profile iris-browser \"/opt/iris/engines/chromium/app/chrome\" flags=(unconfined) {\n  \
             userns,\n}\n"
        );
        // Spaces stay inside the quotes.
        let p = profile_text(Path::new("/opt/My Apps/chrome-linux64/chrome")).unwrap();
        assert!(p.contains("profile iris-browser \"/opt/My Apps/chrome-linux64/chrome\" flags"));
        // Anything AppArmor would read as a pattern, variable, or quote is
        // refused rather than escaped; so are relative and odd paths.
        for bad in [
            "/opt/*/chrome",
            "/opt/chrome?",
            "/opt/{a,b}/chrome",
            "/opt/[ab]/chrome",
            "/opt/^x/chrome",
            "@{HOME}/chrome",
            "/home/@{USER}/chrome",
            "/opt/\"x/chrome",
            "/opt/x\\/chrome",
            "/opt/x\nprofile y /bin/sh {/chrome",
            "/opt/x,/chrome",
            "chrome",
            "/opt/../bin/sh",
            "/opt/./chrome",
            "/opt//chrome",
            "/opt/chrome/",
            "/",
        ] {
            assert!(profile_text(Path::new(bad)).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn setup_script_writes_the_same_profile() {
        let script = include_str!("../../deploy/setup-engines.sh");
        let function = script
            .split_once("browser_profile() {")
            .unwrap()
            .1
            .split_once("\n}\n")
            .unwrap()
            .0;
        let path = "/opt/iris/engines/chromium/app/chrome";
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "set -eu\nbrowser_profile() {{{function}\n}}\nbrowser_profile \"$1\""
            ))
            .arg("sh")
            .arg(path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            profile_text(Path::new(path)).unwrap()
        );
        // The script only accepts engines directories the profile can name.
        assert!(script.contains("*[!A-Za-z0-9/._-]*)"));
        assert!(script.contains("apparmor_restrict_unprivileged_userns"));
        assert!(script.contains("apparmor_parser -r"));
        assert!(script.contains(&format!("APPARMOR_PROFILE={PROFILE_FILE}")));
    }

    #[test]
    fn command_writes_and_loads_the_profile() {
        let c = profile_command(Path::new("/opt/iris/engines/chromium/app/chrome")).unwrap();
        let lines: Vec<&str> = c.lines().collect();
        assert_eq!(
            lines[0],
            "sudo tee /etc/apparmor.d/iris-browser >/dev/null <<'EOF'"
        );
        assert_eq!(lines[lines.len() - 2], "EOF");
        assert_eq!(
            lines[lines.len() - 1],
            "sudo apparmor_parser -r /etc/apparmor.d/iris-browser"
        );
        assert!(c.contains("\"/opt/iris/engines/chromium/app/chrome\""));
        assert!(profile_command(Path::new("/opt/*/chrome")).is_none());
    }

    fn uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// A browser directory (`app/chrome`, a library, a subdirectory).
    fn tree(base: &Path) -> std::path::PathBuf {
        let app = base.join("app");
        std::fs::create_dir_all(app.join("locales")).unwrap();
        let bin = app.join("chrome");
        std::fs::write(&bin, "").unwrap();
        std::fs::write(app.join("libEGL.so"), "").unwrap();
        for p in [
            base,
            &app,
            &app.join("locales"),
            &bin,
            &app.join("libEGL.so"),
        ] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin
    }

    #[test]
    fn everything_the_browser_loads_must_be_protected() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tree(&tmp.path().canonicalize().unwrap());
        let app = bin.parent().unwrap();
        let me = [uid()];
        assert!(tree_safe(app, &me));
        // Not a trusted owner.
        assert!(!tree_safe(app, &[u32::MAX]));
        // A library anyone could replace.
        let lib = app.join("libEGL.so");
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o757)).unwrap();
        assert!(!tree_safe(app, &me));
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A group-writable subdirectory.
        let sub = app.join("locales");
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(!tree_safe(app, &me));
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A symlink inside only needs a trusted owner.
        std::os::unix::fs::symlink("/etc/hostname", app.join("link")).unwrap();
        assert!(tree_safe(app, &me));
    }

    #[test]
    fn writable_or_symlinked_paths_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap().join("base");
        std::fs::create_dir(&base).unwrap();
        let bin = tree(&base);
        // Every ancestor's owner trusted, so only modes decide below.
        let mut trusted: Vec<u32> = bin
            .ancestors()
            .map(|p| std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(p).unwrap()))
            .collect();
        trusted.push(uid());
        for mode in [0o777, 0o1777, 0o775] {
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(!chain_safe(&bin, &trusted), "{mode:o}");
            assert!(!protected(&bin, &trusted), "{mode:o}");
        }
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Through a symlink: the profile would name the wrong path.
        let link = tmp.path().canonicalize().unwrap().join("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert!(!protected(&link.join("app/chrome"), &trusted));
        assert!(!protected(Path::new("relative/chrome"), &trusted));
    }

    #[test]
    fn setup_command_for_this_machine() {
        assert_eq!(
            setup_command("svc", Some(Path::new("/usr/local/bin/iris")), None),
            "sudo sh deploy/setup-engines.sh 'svc' '/usr/local/bin/iris'"
        );
        assert_eq!(
            setup_command("o'brien", None, Some(Path::new("/srv/iris engines"))),
            "sudo install -Dm755 target/release/iris /usr/local/bin/iris\n\
             sudo sh deploy/setup-engines.sh 'o'\\''brien' '/usr/local/bin/iris' '/srv/iris engines'"
        );
        assert!(current_user().is_some_and(|u| !u.is_empty()));
    }

    #[test]
    fn fix_suggests_a_profile_only_for_a_protected_browser() {
        assert_eq!(sandbox_fix(false, None, &[0], "setup"), Value::Null);
        let none = sandbox_fix(true, None, &[0], "sudo sh deploy/setup-engines.sh x");
        assert_eq!(none["kind"], "setup");
        assert_eq!(none["command"], "sudo sh deploy/setup-engines.sh x");
        // A browser the current user (or anyone but root) could replace
        // never gets a profile, only the setup route.
        let tmp = tempfile::tempdir().unwrap();
        let bin = tree(&tmp.path().canonicalize().unwrap());
        let fix = sandbox_fix(true, Some(&bin), &[0], "setup");
        assert_eq!(fix["kind"], "setup");
        assert_eq!(fix["path"], bin.to_string_lossy().as_ref());
        // A root-owned one gets a profile for its real path.
        if let Some(sh) = ["/usr/bin/sh", "/bin/sh"]
            .iter()
            .filter_map(|p| Path::new(p).canonicalize().ok())
            .find(|p| protected(p, &[0]))
        {
            let fix = sandbox_fix(true, Some(&sh), &[0], "setup");
            assert_eq!(fix["kind"], "apparmor");
            let cmd = fix["command"].as_str().unwrap();
            assert!(cmd.contains(&format!("\"{}\"", sh.display())), "{cmd}");
        }
    }
}
