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
/// The engine admin account `deploy/setup-engines.sh` creates; Iris and its
/// agent must run as other accounts.
pub const ENGINE_ADMIN: &str = "iris";
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

/// Owner and mode of a path itself (never following a symlink).
type Meta<'a> = &'a dyn Fn(&Path) -> Option<(u32, u32)>;

fn real_meta(p: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p)
        .ok()
        .map(|m| (m.uid(), m.mode()))
}

/// Whether nobody but the trusted owners can replace `binary` or what it
/// loads from its directory: the path is real (no symlinks), and it, every
/// ancestor, and everything in its directory belong to a trusted owner
/// and are not writable by group or others (symlinks inside the directory
/// only need a trusted owner; their targets are not followed).
/// (`meta` gives owner and mode; tests pass stand-ins.)
fn protected_with(binary: &Path, trusted: &[u32], meta: Meta) -> bool {
    binary.is_absolute()
        && binary.canonicalize().ok().as_deref() == Some(binary)
        && chain_safe(binary, trusted, meta)
        && binary.parent().is_some_and(|d| tree_safe(d, trusted, meta))
}

fn safe_mode(uid: u32, mode: u32, trusted: &[u32]) -> bool {
    trusted.contains(&uid) && mode & 0o022 == 0
}

/// The path and every ancestor (no sticky-directory exception: a
/// world-writable ancestor could swap the whole tree).
fn chain_safe(path: &Path, trusted: &[u32], meta: Meta) -> bool {
    path.ancestors()
        .all(|p| meta(p).is_some_and(|(uid, mode)| safe_mode(uid, mode, trusted)))
}

/// Everything in the browser's directory (the libraries and resources it
/// loads). In a shared system directory such as /usr/bin only its files
/// are checked.
fn tree_safe(dir: &Path, trusted: &[u32], meta: Meta) -> bool {
    const S_IFMT: u32 = 0o170000;
    const S_IFLNK: u32 = 0o120000;
    const S_IFDIR: u32 = 0o040000;
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
            if seen > MAX_ENTRIES {
                return false;
            }
            let Ok(e) = e else {
                return false;
            };
            let path = e.path();
            let Some((uid, mode)) = meta(&path) else {
                return false;
            };
            match mode & S_IFMT {
                S_IFLNK if !trusted.contains(&uid) => return false,
                S_IFLNK => {}
                _ if !safe_mode(uid, mode, trusted) => return false,
                S_IFDIR if recurse => stack.push(path),
                _ => {}
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
    is_protected: &dyn Fn(&Path) -> bool,
    setup_command: &str,
) -> Value {
    if !restricted {
        return Value::Null;
    }
    let real = binary.and_then(|b| b.canonicalize().ok());
    let path = real.as_ref().map(|p| p.to_string_lossy().into_owned());
    if let Some(real) = &real
        && is_protected(real)
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
    chain_safe(&exe, &[0], &real_meta).then_some(exe)
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

/// The uid of an account, if it exists.
pub fn uid_of(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getpwnam_r writes into the buffers we pass; the result is
    // read only on success.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = vec![0 as libc::c_char; 4096];
        let rc = libc::getpwnam_r(c.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out);
        (rc == 0 && !out.is_null()).then_some(pw.pw_uid)
    }
}

/// Facts about this machine that the recovery depends on.
pub struct Host<'a> {
    /// AppArmor restricts unprivileged user namespaces.
    pub restricted: bool,
    /// The account Iris runs as, and its uid.
    pub user: &'a str,
    pub uid: u32,
    /// The isolated agent account (empty: none), and its uid.
    pub agent_user: &'a str,
    pub agent_uid: Option<u32>,
    /// The uid of the engine admin account (`iris`), if it exists.
    pub admin_uid: Option<u32>,
    /// This Iris binary, if only root can replace it.
    pub iris_bin: Option<&'a Path>,
}

/// Iris's browser setup as configured.
pub struct Setup<'a> {
    /// Engines directory.
    pub root: &'a Path,
    /// The privilege split (`engines.helper`) is configured.
    pub split: bool,
    /// `browser.command` is set.
    pub configured: bool,
    /// The browser Iris would start.
    pub browser: Option<&'a Path>,
}

/// `sandbox_fix` for the app (see [`sandbox_fix`]), plus, on the setup
/// route, what else stands in the way: `configured_command`,
/// `install_browser`, and `account_conflict`.
///
/// Trusted owners of a browser's files: root, and with the privilege split
/// the engine admin account, but only if it owns the engines directory and
/// is neither Iris's account nor the agent's.
pub fn recovery(host: &Host, setup: &Setup, meta: Meta) -> Value {
    use crate::engines;
    let mut trusted = vec![0];
    if setup.split
        && let Some(admin) = host.admin_uid
        && meta(setup.root).is_some_and(|(uid, _)| uid == admin)
        && admin != host.uid
        && host.agent_uid != Some(admin)
    {
        trusted.push(admin);
    }
    // Iris's own browser from before the fixed path: its directory changes
    // with every update, so it is moved (updated), never approved in place.
    let app = engines::browser_app_dir(setup.root).canonicalize().ok();
    let downloads = engines::install_dir(setup.root, "browser")
        .canonicalize()
        .ok();
    let old_layout = |p: &Path| {
        downloads.as_ref().is_some_and(|d| p.starts_with(d))
            && !app.as_ref().is_some_and(|a| p.starts_with(a))
    };
    let managed = engines::is_installed(setup.root, "browser");
    let managed_moved = engines::launcher(setup.root, "browser")
        .canonicalize()
        .ok()
        .is_some_and(|p| !old_layout(&p));
    let command = setup_command(host.user, host.iris_bin, setup.split.then_some(setup.root));
    let mut fix = sandbox_fix(
        host.restricted,
        setup.browser,
        &|p| !old_layout(p) && protected_with(p, &trusted, meta),
        &command,
    );
    if fix["kind"] == "setup" {
        // `browser.command` always wins, so it has to go first.
        fix["configured_command"] = setup.configured.into();
        // With the privilege split in place, installing (or moving) Iris's
        // own browser into the protected engines directory is the step the
        // app can offer.
        fix["install_browser"] = (setup.split && (!managed || !managed_moved)).into();
        // The setup reserves this account for engine installs and refuses
        // to run for it.
        let admin = |name: &str, uid: Option<u32>| {
            name == ENGINE_ADMIN || (uid.is_some() && uid == host.admin_uid)
        };
        fix["account_conflict"] = (admin(host.user, Some(host.uid))
            || (!host.agent_user.is_empty() && admin(host.agent_user, host.agent_uid)))
        .into();
    }
    fix
}

/// [`recovery`] for this machine.
pub fn recovery_here(setup: &Setup, agent_user: &str) -> Value {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let user = current_user()
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "IRIS_USER".into());
    let iris_bin = root_owned_iris();
    let host = Host {
        restricted: userns_restricted(),
        user: &user,
        uid,
        agent_user,
        agent_uid: Some(agent_user).filter(|a| !a.is_empty()).and_then(uid_of),
        admin_uid: uid_of(ENGINE_ADMIN),
        iris_bin: iris_bin.as_deref(),
    };
    recovery(&host, setup, &real_meta)
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
        assert!(
            script
                .contains("USERNS_SYSCTL=/proc/sys/kernel/apparmor_restrict_unprivileged_userns\n")
        );
        assert_eq!(
            format!("{}/{PROFILE_NAME}", "/etc/apparmor.d"),
            PROFILE_FILE
        );
        assert!(script.contains("APPARMOR_DIR=/etc/apparmor.d\n"));
        assert!(script.contains(&format!("ADMIN_USER=\"{ENGINE_ADMIN}\"\n")));
        assert!(script.contains("\napprove_browser \"$BROWSER_BIN\"\n"));
    }

    /// Runs the setup script's approval step with stand-ins for the
    /// kernel switch, /etc/apparmor.d, and apparmor_parser.
    struct ScriptRun {
        _tmp: tempfile::TempDir,
        dir: std::path::PathBuf,
    }

    impl ScriptRun {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().canonicalize().unwrap();
            for d in ["bin", "apparmor.d/abi/4.0", "engines"] {
                std::fs::create_dir_all(dir.join(d)).unwrap();
            }
            let parser = dir.join("bin/apparmor_parser");
            std::fs::write(
                &parser,
                "#!/bin/sh\nd=\"$(dirname \"$0\")/..\"\necho \"$*\" >> \"$d/parser.log\"\n\
                 cat \"$2\" > \"$d/loaded\"\n[ ! -f \"$d/parser.fail\" ]\n",
            )
            .unwrap();
            std::fs::set_permissions(&parser, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { _tmp: tmp, dir }
        }

        fn run(&self, sysctl: Option<&str>) -> std::process::Output {
            let script = include_str!("../../deploy/setup-engines.sh");
            let block = script
                .split_once("# --- browser approval: functions only")
                .unwrap()
                .1
                .split_once("# --- end browser approval ---")
                .unwrap()
                .0;
            let sysctl_file = self.dir.join("sysctl");
            match sysctl {
                Some(v) => std::fs::write(&sysctl_file, v).unwrap(),
                None => {
                    let _ = std::fs::remove_file(&sysctl_file);
                }
            }
            let me = std::process::Command::new("id")
                .arg("-un")
                .output()
                .unwrap();
            let path = format!(
                "{}:{}",
                self.dir.join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            );
            std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "set -eu\n#{block}\napprove_browser \"$ENGINES_DIR/chromium/app/chrome\""
                ))
                .env("PATH", path)
                .env("USERNS_SYSCTL", &sysctl_file)
                .env("APPARMOR_DIR", self.dir.join("apparmor.d"))
                .env("ENGINES_DIR", self.dir.join("engines"))
                .env("ADMIN_USER", String::from_utf8_lossy(&me.stdout).trim())
                .output()
                .unwrap()
        }

        fn profile(&self) -> Option<String> {
            std::fs::read_to_string(self.dir.join("apparmor.d/iris-browser")).ok()
        }

        fn loads(&self) -> usize {
            std::fs::read_to_string(self.dir.join("parser.log"))
                .map(|s| s.lines().count())
                .unwrap_or(0)
        }
    }

    #[test]
    fn setup_approves_the_browser_only_where_userns_is_restricted() {
        let s = ScriptRun::new();
        let bin = s.dir.join("engines/chromium/app/chrome");
        let expected = profile_text(&bin).unwrap();
        // No switch, or switched off: nothing loaded or written.
        for sysctl in [None, Some("0\n")] {
            let out = s.run(sysctl);
            assert!(out.status.success(), "{out:?}");
            assert_eq!((s.loads(), s.profile()), (0, None));
        }
        // Restricted: loaded first, then installed; rerunning reloads.
        let out = s.run(Some("1\n"));
        assert!(out.status.success(), "{out:?}");
        assert_eq!(s.loads(), 1);
        assert!(
            std::fs::read_to_string(s.dir.join("parser.log"))
                .unwrap()
                .starts_with("-r ")
        );
        assert_eq!(
            std::fs::read_to_string(s.dir.join("loaded")).unwrap(),
            expected
        );
        assert_eq!(s.profile().as_deref(), Some(expected.as_str()));
        assert!(s.run(Some("1")).status.success());
        assert_eq!(s.loads(), 2);
        // A profile that does not load is not installed; the one in place
        // stays.
        std::fs::write(s.dir.join("apparmor.d/iris-browser"), "previous").unwrap();
        std::fs::write(s.dir.join("parser.fail"), "").unwrap();
        let out = s.run(Some("1"));
        assert!(out.status.success(), "{out:?}");
        assert_eq!(s.loads(), 3);
        assert_eq!(s.profile().as_deref(), Some("previous"));
        std::fs::remove_file(s.dir.join("parser.fail")).unwrap();
        // Without AppArmor 4: nothing loaded.
        std::fs::remove_dir(s.dir.join("apparmor.d/abi/4.0")).unwrap();
        let out = s.run(Some("1"));
        assert!(String::from_utf8_lossy(&out.stderr).contains("AppArmor 4 is missing"));
        assert_eq!(s.loads(), 3);
        std::fs::create_dir(s.dir.join("apparmor.d/abi/4.0")).unwrap();
        // Files under the browser directory others could change: refused.
        let app = s.dir.join("engines/chromium/app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("chrome"), "").unwrap();
        std::fs::set_permissions(
            app.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o775)).unwrap();
        let out = s.run(Some("1"));
        assert!(out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Not approving"),
            "{out:?}"
        );
        assert_eq!(s.loads(), 3);
        std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(app.join("chrome"), std::fs::Permissions::from_mode(0o757))
            .unwrap();
        assert!(String::from_utf8_lossy(&s.run(Some("1")).stderr).contains("Not approving"));
        assert_eq!(s.loads(), 3);
        std::fs::set_permissions(app.join("chrome"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let out = s.run(Some("1"));
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("Not approving"),
            "{out:?}"
        );
        assert_eq!(s.loads(), 4);
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
        assert!(tree_safe(app, &me, &real_meta));
        // Not a trusted owner.
        assert!(!tree_safe(app, &[u32::MAX], &real_meta));
        // A library anyone could replace.
        let lib = app.join("libEGL.so");
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o757)).unwrap();
        assert!(!tree_safe(app, &me, &real_meta));
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A group-writable subdirectory.
        let sub = app.join("locales");
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(!tree_safe(app, &me, &real_meta));
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A symlink inside only needs a trusted owner.
        std::os::unix::fs::symlink("/etc/hostname", app.join("link")).unwrap();
        assert!(tree_safe(app, &me, &real_meta));
    }

    /// Ancestors outside `base` look like root-owned 0755 directories;
    /// inside, the real metadata counts (so tests do not depend on where
    /// the temp directory lives).
    fn meta_below(base: &Path) -> impl Fn(&Path) -> Option<(u32, u32)> + '_ {
        move |p: &Path| {
            if p.starts_with(base) {
                real_meta(p)
            } else {
                Some((0, 0o40755))
            }
        }
    }

    #[test]
    fn protection_needs_every_ancestor_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap().join("base");
        std::fs::create_dir(&base).unwrap();
        let bin = tree(&base);
        let trusted = [0, uid()];
        let meta = meta_below(&base);
        // Baseline: everything owned by trusted accounts, nothing shared.
        assert!(chain_safe(&bin, &trusted, &meta));
        assert!(protected_with(&bin, &trusted, &meta));
        // Files owned by an account that is not trusted (for example the
        // user running Iris in single-user mode).
        let other = |p: &Path| {
            if p.starts_with(&base) {
                real_meta(p).map(|(_, mode)| (4242, mode))
            } else {
                meta(p)
            }
        };
        assert!(!protected_with(&bin, &trusted, &other));
        // A writable ancestor could swap the whole tree; sticky is not
        // enough.
        for mode in [0o777, 0o1777, 0o775, 0o757] {
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(!chain_safe(&bin, &trusted, &meta), "{mode:o}");
            assert!(!protected_with(&bin, &trusted, &meta), "{mode:o}");
        }
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(protected_with(&bin, &trusted, &meta));
        // An ancestor above that is not root's.
        let foreign = |p: &Path| {
            if p == Path::new("/") {
                Some((u32::MAX, 0o40755))
            } else {
                meta(p)
            }
        };
        assert!(!protected_with(&bin, &trusted, &foreign));
        // A world-writable ancestor above.
        let shared = |p: &Path| {
            if p == Path::new("/") {
                Some((0, 0o41777))
            } else {
                meta(p)
            }
        };
        assert!(!protected_with(&bin, &trusted, &shared));
        // The binary itself writable.
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(!protected_with(&bin, &trusted, &meta));
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Through a symlink: the profile would name the wrong path.
        let link = tmp.path().canonicalize().unwrap().join("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        let meta = meta_below(tmp.path());
        assert!(!protected_with(&link.join("app/chrome"), &trusted, &meta));
        assert!(!protected_with(
            Path::new("relative/chrome"),
            &trusted,
            &meta
        ));
        assert!(!protected_with(&base.join("app/missing"), &trusted, &meta));
    }

    /// An engines directory with Iris's browser at the fixed path (or, for
    /// `old`, inside Playwright's per-build download directory).
    fn engines(old: bool) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("engines");
        let rel = if old {
            "chromium/browsers/chromium-1200/chrome-linux64/chrome"
        } else {
            "chromium/app/chrome"
        };
        let bin = root.join(rel);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "").unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::os::unix::fs::symlink(&bin, root.join("bin/browser")).unwrap();
        for p in bin.ancestors().take_while(|p| p.starts_with(tmp.path())) {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (tmp, root, bin)
    }

    /// Everything under `base` owned by `uid` (real modes); above it,
    /// root-owned 0755 directories.
    fn owned_by(base: &Path, uid: u32) -> impl Fn(&Path) -> Option<(u32, u32)> + '_ {
        move |p: &Path| {
            if p.starts_with(base) {
                real_meta(p).map(|(_, mode)| (uid, mode))
            } else {
                Some((0, 0o40755))
            }
        }
    }

    const ADMIN: u32 = 4242;
    const SERVICE: u32 = 1000;
    const AGENT: u32 = 2000;

    fn host<'a>() -> Host<'a> {
        Host {
            restricted: true,
            user: "svc",
            uid: SERVICE,
            agent_user: "",
            agent_uid: None,
            admin_uid: Some(ADMIN),
            iris_bin: Some(Path::new("/usr/local/bin/iris")),
        }
    }

    #[test]
    fn recovery_picks_the_safe_route_for_each_setup() {
        let (_t, root, bin) = engines(false);
        let split = Setup {
            root: &root,
            split: true,
            configured: false,
            browser: Some(&bin),
        };
        // Privilege split, browser owned by the engine admin: a profile for
        // exactly the fixed path.
        let fix = recovery(&host(), &split, &owned_by(&root, ADMIN));
        assert_eq!(fix["kind"], "apparmor", "{fix}");
        assert_eq!(fix["command"], profile_command(&bin).unwrap());
        assert!(fix.get("install_browser").is_none());
        // Not restricted: nothing to suggest.
        let off = Host {
            restricted: false,
            ..host()
        };
        assert_eq!(recovery(&off, &split, &owned_by(&root, ADMIN)), Value::Null);
        // Single-user mode: the service's own files are never trusted.
        let single = Setup {
            split: false,
            ..split
        };
        let fix = recovery(&host(), &single, &owned_by(&root, SERVICE));
        assert_eq!(fix["kind"], "setup");
        assert_eq!(
            fix["command"],
            "sudo sh deploy/setup-engines.sh 'svc' '/usr/local/bin/iris'"
        );
        assert_eq!(fix["install_browser"], false);
        assert_eq!(fix["configured_command"], false);
        assert_eq!(fix["account_conflict"], false);
        // Split configured, but the engines belong to the service account.
        let fix = recovery(&host(), &split, &owned_by(&root, SERVICE));
        assert_eq!(fix["kind"], "setup");
        assert!(
            fix["command"]
                .as_str()
                .unwrap()
                .ends_with(&format!("'{}'", root.display()))
        );
        // ...or to the isolated agent (even if it is the admin uid).
        let agent = Host {
            agent_user: "agent",
            agent_uid: Some(ADMIN),
            ..host()
        };
        let fix = recovery(&agent, &split, &owned_by(&root, ADMIN));
        assert_eq!(fix["kind"], "setup");
        assert_eq!(fix["account_conflict"], true);
        let agent = Host {
            agent_user: "agent",
            agent_uid: Some(AGENT),
            ..host()
        };
        assert_eq!(
            recovery(&agent, &split, &owned_by(&root, AGENT))["kind"],
            "setup"
        );
        assert_eq!(
            recovery(&agent, &split, &owned_by(&root, ADMIN))["kind"],
            "apparmor"
        );
        // ...or to an account that is not the engine admin.
        let other = Host {
            admin_uid: Some(5555),
            ..host()
        };
        assert_eq!(
            recovery(&other, &split, &owned_by(&root, ADMIN))["kind"],
            "setup"
        );
        let none = Host {
            admin_uid: None,
            ..host()
        };
        assert_eq!(
            recovery(&none, &split, &owned_by(&root, ADMIN))["kind"],
            "setup"
        );
        // Iris running as the admin account: the setup refuses it.
        let as_admin = Host {
            user: "iris",
            uid: ADMIN,
            ..host()
        };
        let fix = recovery(&as_admin, &split, &owned_by(&root, ADMIN));
        assert_eq!(fix["kind"], "setup");
        assert_eq!(fix["account_conflict"], true);
        // browser.command picks an unprotected browser: it has to go.
        let (_t2, other_root, other_bin) = engines(false);
        let configured = Setup {
            configured: true,
            browser: Some(&other_bin),
            ..split
        };
        let meta = |p: &Path| {
            if p.starts_with(&other_root) {
                real_meta(p).map(|(_, m)| (SERVICE, m))
            } else {
                owned_by(&root, ADMIN)(p)
            }
        };
        let fix = recovery(&host(), &configured, &meta);
        assert_eq!(fix["kind"], "setup");
        assert_eq!(fix["configured_command"], true);
        assert_eq!(
            fix["install_browser"], false,
            "Iris's own copy is installed"
        );
        // No browser at all: install Iris's own.
        std::fs::remove_file(root.join("bin/browser")).unwrap();
        let fix = recovery(
            &host(),
            &Setup {
                browser: None,
                ..split
            },
            &owned_by(&root, ADMIN),
        );
        assert_eq!(fix["kind"], "setup");
        assert_eq!(fix["install_browser"], true);
    }

    #[test]
    fn recovery_moves_a_browser_from_before_the_fixed_path() {
        let (_t, root, bin) = engines(true);
        let split = Setup {
            root: &root,
            split: true,
            configured: false,
            browser: Some(&bin),
        };
        // Protected, but its path changes with every update: offer the
        // update (which moves it), not a profile for this build.
        let fix = recovery(&host(), &split, &owned_by(&root, ADMIN));
        assert_eq!(fix["kind"], "setup", "{fix}");
        assert_eq!(fix["install_browser"], true);
        assert_eq!(fix["path"], bin.to_string_lossy().as_ref());
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
        let yes = |_: &Path| true;
        let no = |_: &Path| false;
        assert_eq!(sandbox_fix(false, None, &yes, "setup"), Value::Null);
        let none = sandbox_fix(true, None, &yes, "sudo sh deploy/setup-engines.sh x");
        assert_eq!(none["kind"], "setup");
        assert_eq!(none["path"], Value::Null);
        assert_eq!(none["command"], "sudo sh deploy/setup-engines.sh x");
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let bin = tree(&base);
        // Not protected: only the setup route.
        let fix = sandbox_fix(true, Some(&bin), &no, "setup");
        assert_eq!(fix["kind"], "setup");
        assert_eq!(fix["command"], "setup");
        assert_eq!(fix["path"], bin.to_string_lossy().as_ref());
        // Protected: a profile for its real path, also when reached
        // through a symlink (AppArmor attaches to the real path).
        let link = base.join("chrome-link");
        std::os::unix::fs::symlink(&bin, &link).unwrap();
        let checked = std::cell::RefCell::new(Vec::new());
        let record = |p: &Path| {
            checked.borrow_mut().push(p.to_path_buf());
            true
        };
        let fix = sandbox_fix(true, Some(&link), &record, "setup");
        assert_eq!(fix["kind"], "apparmor");
        assert_eq!(checked.borrow().as_slice(), std::slice::from_ref(&bin));
        assert_eq!(fix["path"], bin.to_string_lossy().as_ref());
        let cmd = fix["command"].as_str().unwrap();
        assert_eq!(cmd, profile_command(&bin).unwrap());
        assert!(cmd.contains(&format!("\"{}\"", bin.display())), "{cmd}");
        // Protected but not expressible in a profile: setup route.
        let odd = base.join("a{b}");
        std::fs::create_dir(&odd).unwrap();
        std::fs::write(odd.join("chrome"), "").unwrap();
        let fix = sandbox_fix(true, Some(&odd.join("chrome")), &yes, "setup");
        assert_eq!(fix["kind"], "setup");
    }
}
