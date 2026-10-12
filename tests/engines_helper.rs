//! `iris engines` as the privileged helper: what it creates must be usable
//! by the agent user even under the service's restrictive umask.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn helper_creates_world_readable_engine_dirs_under_a_strict_umask() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    let root = base.join("engines");
    let out = Command::new("/bin/sh")
        .args([
            "-c",
            "umask 077; exec \"$0\" engines uninstall codex --dir \"$1\"",
        ])
        .arg(env!("CARGO_BIN_EXE_iris"))
        .arg(&root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
}

#[test]
fn sudoers_allow_exact_browser_install_update_and_uninstall_commands() {
    let script = include_str!("../deploy/setup-engines.sh");
    // Execute the production rule generator without privileged setup or
    // writes to /etc; use the real parser to validate the resulting file.
    let function = script
        .split_once("write_rule() {")
        .unwrap()
        .1
        .split_once("\nwrite_rule tty")
        .unwrap()
        .0;
    let tmp = tempfile::tempdir().unwrap();
    let sudoers = tmp.path().join("sudoers");
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("set -eu\nwrite_rule() {{{function}\nwrite_rule"))
        .env("AGENT_USER", "agent")
        .env("ADMIN_USER", "iris")
        .env("IRIS_BIN", "/usr/local/bin/iris")
        .env("ENGINES_DIR", "/opt/iris/engines")
        .env("SUDOERS", &sudoers)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let path = sudoers.with_extension("tmp");
    let rules = std::fs::read_to_string(&path).unwrap();
    assert!(!rules.contains('*'));
    let commands = rules
        .lines()
        .last()
        .unwrap()
        .split("NOPASSWD: ")
        .nth(1)
        .unwrap()
        .split(", ")
        .collect::<Vec<_>>();
    let mut expected = vec!["/usr/local/bin/iris engines list --dir /opt/iris/engines".to_string()];
    for op in ["install", "update", "uninstall"] {
        for engine in ["claude", "codex", "gemini", "opencode", "browser"] {
            expected.push(format!(
                "/usr/local/bin/iris engines {op} {engine} --dir /opt/iris/engines"
            ));
        }
    }
    assert_eq!(commands, expected);
    match Command::new("visudo")
        .arg("-c")
        .arg("-f")
        .arg(path)
        .output()
    {
        Ok(out) => assert!(out.status.success(), "{out:?}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("visudo is not installed; exact rule assertions passed");
        }
        Err(e) => panic!("visudo failed: {e}"),
    }
}
