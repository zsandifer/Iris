//! `iris mcp-exec`: runs an MCP server with variables from a private file,
//! so connector secrets never appear on a command line.

use std::process::Command;

#[test]
fn mcp_exec_applies_env_file_and_passes_args_through() {
    let tmp = tempfile::tempdir().unwrap();
    let env_file = tmp.path().join("env.json");
    std::fs::write(&env_file, r#"{"API_TOKEN":"s3cret value","OTHER":"x"}"#).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_iris"))
        .args(["mcp-exec"])
        .arg(&env_file)
        .args([
            "/bin/sh",
            "-c",
            r#"printf '%s|%s|%s' "$API_TOKEN" "$OTHER" "$1""#,
            "sh",
            "--flag-like-arg",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "s3cret value|x|--flag-like-arg"
    );
}

#[test]
fn mcp_exec_fails_cleanly() {
    let out = Command::new(env!("CARGO_BIN_EXE_iris"))
        .args(["mcp-exec", "/nonexistent/env.json", "/bin/true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read environment file"));
    let out = Command::new(env!("CARGO_BIN_EXE_iris"))
        .args(["mcp-exec"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
}
