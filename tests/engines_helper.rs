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
