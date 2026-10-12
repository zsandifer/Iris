//! OS privilege split. Only the fixed `iris agent-exec` entry point is in
//! sudoers. Requests (including API keys) travel over stdin, never argv or sudo's environment.
use crate::config::{AgentConfig, Paths, ensure_private_dir, write_private_file};
use crate::harness::{Invocation, McpServer, McpTransport};
use crate::util::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const POLICY: &str = "/etc/iris-agent.json";
const MAX_REQUEST: usize = 32 * 1024 * 1024;
const MAX_FILE: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub service_uid: u32,
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub runtime: PathBuf,
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub launcher: PathBuf,
    pub engines: BTreeMap<String, PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    op: String,
    scope: String,
    harness: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    path: String,
}

pub fn valid_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= 64
        && scope != "legacy"
        && scope
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn trusted(path: &Path, owners: &[u32]) -> Result<()> {
    if !path.is_absolute() || path.canonicalize()? != path {
        return Err("launcher policy paths must be absolute and free of symlinks".into());
    }
    for p in path.ancestors() {
        let m = std::fs::symlink_metadata(p)?;
        if !owners.contains(&m.uid()) || m.mode() & 0o022 != 0 {
            return Err(format!("untrusted launcher path: {}", p.display()).into());
        }
    }
    Ok(())
}

pub fn policy(cfg: &AgentConfig) -> Result<Option<Policy>> {
    if cfg.user.is_empty() {
        return Ok(None);
    }
    trusted(Path::new(POLICY), &[0])?;
    let p: Policy = serde_json::from_slice(&std::fs::read(POLICY)?)?;
    if p.user != cfg.user || p.launcher != Path::new(&cfg.launcher) {
        return Err(
            "agent configuration does not match /etc/iris-agent.json; run root setup".into(),
        );
    }
    validate_policy(&p)?;
    Ok(Some(p))
}

fn validate_policy(p: &Policy) -> Result<()> {
    if p.uid == 0 || p.service_uid == 0 || p.uid == p.service_uid || !valid_scope(&p.user) {
        return Err("agent and service must be distinct non-root accounts".into());
    }
    trusted(&p.launcher, &[0])?;
    for dir in [&p.workspace, &p.runtime, &p.home] {
        if !dir.is_absolute() || dir.canonicalize()? != *dir {
            return Err("agent directory must be an absolute real directory".into());
        }
    }
    let home = std::fs::metadata(&p.home)?;
    if !home.is_dir() || home.uid() != p.uid || home.gid() != p.gid || home.mode() & 0o077 != 0 {
        return Err("agent HOME must be agent-owned and private".into());
    }
    if p.workspace.starts_with(&p.data_dir)
        || p.runtime.starts_with(&p.data_dir)
        || p.home.starts_with(&p.data_dir)
        || p.data_dir.starts_with(&p.home)
    {
        return Err("agent paths must be outside Iris's data directory".into());
    }
    if unsafe { libc::geteuid() } == p.service_uid {
        for path in [&p.data_dir, &p.config_file] {
            if !path.is_absolute() || path.canonicalize()? != *path {
                return Err("private service paths must be real absolute paths".into());
            }
            let m = std::fs::symlink_metadata(path)?;
            if m.uid() != p.service_uid || m.mode() & 0o077 != 0 {
                return Err("Iris data/config must be private to the service user".into());
            }
        }
    }
    trusted(&p.runtime, &[0, p.service_uid])?;
    trusted(
        p.workspace.parent().ok_or("workspace parent missing")?,
        &[0, p.service_uid],
    )?;
    let runtime = std::fs::metadata(&p.runtime)?;
    if runtime.uid() != p.service_uid || runtime.gid() != p.gid || runtime.mode() & 0o027 != 0 {
        return Err(
            "turn runtime must be service-owned and group-readable, never agent-writable".into(),
        );
    }
    // The agent cannot replace conversation directories in their parent.
    let workspace = std::fs::metadata(&p.workspace)?;
    if workspace.uid() != p.service_uid || workspace.gid() != p.gid || workspace.mode() & 0o007 != 0
    {
        return Err("workspace parent must be service-owned with the agent group".into());
    }
    Ok(())
}

fn allowed_env(harness: &str, name: &str) -> bool {
    crate::keys::option(harness, name).is_some()
        || matches!(
            name,
            "CLAUDE_CODE_DISABLE_CLAUDE_MDS"
                | "CLAUDE_CODE_DISABLE_AUTO_MEMORY"
                | "CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS"
                | "DISABLE_AUTOUPDATER"
                | "GEMINI_RESTRICTED_MODE"
                | "GEMINI_SYSTEM_MD"
                | "OPENCODE_CONFIG"
                | "OPENCODE_DISABLE_CLAUDE_CODE"
                | "OPENCODE_DISABLE_PROJECT_CONFIG"
                | "OPENCODE_DISABLE_CLAUDE_CODE_PROMPT"
                | "BROWSER"
                | "NO_COLOR"
        )
}

fn validate_request(r: &Request, p: &Policy) -> Result<()> {
    if !valid_scope(&r.scope) {
        return Err("invalid conversation id".into());
    }
    if !matches!(r.op.as_str(), "exec" | "prepare" | "read" | "check") {
        return Err("unknown agent operation".into());
    }
    if r.op == "exec"
        && (!crate::config::SUPPORTED_HARNESSES.contains(&r.harness.as_str())
            || !p.engines.contains_key(&r.harness))
    {
        return Err("engine is not registered by root setup".into());
    }
    if r.args.iter().any(|a| a.contains('\0')) || r.args.len() > 4096 {
        return Err("invalid CLI arguments".into());
    }
    for (name, value) in &r.env {
        if !allowed_env(&r.harness, name) || value.contains('\0') {
            return Err("environment variable is not allowed in agent launcher".into());
        }
        if matches!(name.as_str(), "GEMINI_SYSTEM_MD" | "OPENCODE_CONFIG") {
            let path = Path::new(value);
            if !path.starts_with(&p.runtime) || path.canonicalize()? != path {
                return Err("CLI settings must be per-turn files".into());
            }
            let m = std::fs::metadata(path)?;
            if m.uid() != p.service_uid || m.mode() & 0o022 != 0 {
                return Err("CLI settings must be service-owned and read-only to the agent".into());
            }
        }
    }
    Ok(())
}

fn frame(r: &Request, stdin: &[u8]) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(r)?;
    if json.len() > MAX_REQUEST {
        return Err("agent request too large".into());
    }
    let mut out = Vec::with_capacity(8 + json.len() + stdin.len());
    out.extend_from_slice(&(json.len() as u64).to_le_bytes());
    out.extend(json);
    out.extend_from_slice(stdin);
    Ok(out)
}

fn read_request(input: &mut impl Read) -> Result<Request> {
    let mut len = [0u8; 8];
    input.read_exact(&mut len)?;
    let n = u64::from_le_bytes(len);
    if n > MAX_REQUEST as u64 {
        return Err("agent request too large".into());
    }
    let mut json = vec![0; n as usize];
    input.read_exact(&mut json)?;
    Ok(serde_json::from_slice(&json)?)
}

pub fn wrap(cfg: &AgentConfig, harness: &str, scope: &str, inv: &mut Invocation) -> Result<()> {
    if cfg.user.is_empty() {
        return Ok(());
    }
    let r = Request {
        op: "exec".into(),
        scope: scope.into(),
        harness: harness.into(),
        args: std::mem::take(&mut inv.args),
        env: std::mem::take(&mut inv.env),
        ..Default::default()
    };
    inv.stdin = frame(&r, &inv.stdin)?;
    inv.program = Some("/usr/bin/sudo".into());
    inv.args = vec![
        "-n".into(),
        "-u".into(),
        cfg.user.clone(),
        "--".into(),
        cfg.launcher.clone(),
        "agent-exec".into(),
    ];
    inv.cwd = Some("/".into());
    Ok(())
}

fn operation(cfg: &AgentConfig, r: &Request) -> Result<Vec<u8>> {
    let mut cmd = std::process::Command::new("/usr/bin/sudo");
    cmd.args(["-n", "-u", &cfg.user, "--", &cfg.launcher, "agent-exec"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .current_dir("/")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    child
        .stdin
        .take()
        .ok_or("missing stdin")?
        .write_all(&frame(r, &[])?)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err("agent launcher failed; check root setup and service permissions".into());
    }
    Ok(output.stdout)
}

pub fn workdir(cfg: &AgentConfig, paths: &Paths, scope: &str) -> Result<PathBuf> {
    if !valid_scope(scope) {
        return Err("invalid conversation id".into());
    }
    if let Some(p) = policy(cfg)? {
        operation(
            cfg,
            &Request {
                op: "prepare".into(),
                scope: scope.into(),
                ..Default::default()
            },
        )?;
        Ok(p.workspace.join(scope))
    } else {
        migrate_workspace(paths)?;
        let dir = paths.workspace_dir().join(scope);
        ensure_private_dir(&dir)?;
        if dir.canonicalize()? != dir {
            return Err("conversation directory cannot be a symlink".into());
        }
        Ok(dir)
    }
}

pub fn migrate_workspace(paths: &Paths) -> Result<()> {
    let workspace = paths.workspace_dir();
    let marker = workspace.join(".iris-conversations");
    if marker.exists() {
        return Ok(());
    }
    // The previous release kept an unscoped cwd inside the private data tree.
    for (source, name) in [
        (paths.data_dir.join("workspace"), "workspace-legacy"),
        (workspace.clone(), "workspace-legacy-unscoped"),
    ] {
        if !source.exists() {
            continue;
        }
        if std::fs::symlink_metadata(&source)?.file_type().is_symlink() {
            return Err("workspace migration refuses symlinks".into());
        }
        if std::fs::read_dir(&source)?.next().is_none() {
            continue;
        }
        let legacy = paths.data_dir.join(name);
        if legacy.exists() {
            return Err(
                format!("workspace migration needs attention: {name} already exists").into(),
            );
        }
        std::fs::rename(&source, &legacy)?;
        ensure_private_dir(&legacy)?;
    }
    ensure_private_dir(&workspace)?;
    write_private_file(&marker, b"1\n")
}

/// Called only by `serve`, after it has acquired the private data-dir lock.
pub fn clear_runtime(cfg: &AgentConfig) -> Result<()> {
    let Some(p) = policy(cfg)? else {
        return Ok(());
    };
    for entry in std::fs::read_dir(&p.runtime)? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.uid() != p.service_uid {
            return Err("unexpected owner in agent runtime directory".into());
        }
        if meta.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

pub fn turn_dir(cfg: &AgentConfig, paths: &Paths, turn: &str) -> Result<PathBuf> {
    let Some(p) = policy(cfg)? else {
        return Ok(paths.run_dir());
    };
    let dir = p.runtime.join(turn);
    std::fs::create_dir(&dir)?;
    group_readable(&dir, p.gid, 0o750)?;
    Ok(dir)
}

fn group_readable(path: &Path, gid: u32, mode: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: valid C string; a service user may change its own file to its supplementary group.
    if unsafe { libc::chown(c.as_ptr(), u32::MAX, gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

pub fn expose_files(cfg: &AgentConfig, inv: &Invocation) -> Result<()> {
    if let Some(p) = policy(cfg)? {
        for path in &inv.temp_files {
            group_readable(path, p.gid, 0o640)?;
        }
    }
    Ok(())
}

/// Open every component relative to a directory descriptor, without following
/// symlinks. This also closes rename/symlink races while saving a library file.
pub fn read_scoped_file(root: &Path, path: &str) -> Result<Vec<u8>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let path = Path::new(path);
    let relative = if path.is_absolute() {
        path.strip_prefix(root)
            .map_err(|_| "file is outside this conversation")?
    } else {
        path
    };
    let parts: Vec<_> = relative
        .components()
        .map(|c| match c {
            std::path::Component::Normal(name) => Ok(name),
            _ => Err("file path must not contain traversal"),
        })
        .collect::<std::result::Result<_, _>>()?;
    if parts.is_empty() {
        return Err("file path is empty".into());
    }
    let mut dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?;
    for (i, name) in parts.iter().enumerate() {
        let name = std::ffi::CString::new(name.as_bytes())?;
        let last = i + 1 == parts.len();
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: dir is a live descriptor and name is a valid C string.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: openat returned a new descriptor, now owned by this File.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        if last {
            let meta = file.metadata()?;
            if !meta.is_file() || meta.len() > MAX_FILE {
                return Err("only regular files up to 20 MB can be saved".into());
            }
            let mut bytes = Vec::new();
            (&mut file).take(MAX_FILE + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_FILE {
                return Err("file is larger than 20 MB".into());
            }
            return Ok(bytes);
        }
        dir = file;
    }
    Err("invalid file path".into())
}

pub fn read_file(cfg: &AgentConfig, scope: &str, path: &str) -> Result<Vec<u8>> {
    operation(
        cfg,
        &Request {
            op: "read".into(),
            scope: scope.into(),
            path: path.into(),
            ..Default::default()
        },
    )
}

/// The launcher is executed as the target user. It never runs as root.
pub fn exec() -> Result<()> {
    use std::os::unix::process::CommandExt;
    trusted(Path::new(POLICY), &[0])?;
    let p: Policy = serde_json::from_slice(&std::fs::read(POLICY)?)?;
    validate_policy(&p)?;
    // SAFETY: identity queries have no preconditions.
    if unsafe { libc::geteuid() } != p.uid || unsafe { libc::getegid() } != p.gid {
        return Err("agent-exec must run as the configured agent user".into());
    }
    use std::os::fd::FromRawFd;
    // Use an unbuffered duplicate: std::io::Stdin could read ahead and swallow
    // part of the prompt that the CLI must receive after exec.
    let fd = unsafe { libc::dup(libc::STDIN_FILENO) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: dup returned a new descriptor owned by this File.
    let mut stdin = unsafe { std::fs::File::from_raw_fd(fd) };
    let r = read_request(&mut stdin)?;
    drop(stdin);
    validate_request(&r, &p)?;
    // SAFETY: no threads have been started in the launcher.
    unsafe {
        libc::umask(0o077);
    }
    let cwd = p.workspace.join(&r.scope);
    if !cwd.exists() {
        if r.op == "check" {
            return Err("probe directory is missing; rerun root setup".into());
        }
        std::fs::create_dir(&cwd)?;
    }
    if cwd.canonicalize()? != cwd {
        return Err("conversation path cannot contain symlinks".into());
    }
    let m = std::fs::metadata(&cwd)?;
    if m.uid() != p.uid || m.mode() & 0o077 != 0 {
        return Err("conversation directory must be agent-owned and private".into());
    }
    match r.op.as_str() {
        "prepare" => return Ok(()),
        "check" => {
            for path in [
                p.data_dir.join("vault.key"),
                p.data_dir.join("iris.db"),
                p.config_file.clone(),
            ] {
                match std::fs::File::open(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
                    _ => {
                        return Err(
                            "agent can access private data, or denial was not verified".into()
                        );
                    }
                }
            }
            println!("{{\"isolated\":true,\"uid\":{}}}", p.uid);
            return Ok(());
        }
        "read" => {
            let bytes = read_scoped_file(&cwd, &r.path)?;
            std::io::stdout().write_all(&bytes)?;
            return Ok(());
        }
        _ => {}
    }
    let context_files: &[&str] = match r.harness.as_str() {
        "codex" => &[".codex/AGENTS.md", ".codex/AGENTS.override.md"],
        "gemini" => &[".gemini/GEMINI.md"],
        "opencode" => &[".config/opencode/AGENTS.md"],
        _ => &[],
    };
    if context_files
        .iter()
        .any(|f| std::fs::metadata(p.home.join(f)).is_ok_and(|m| m.is_file() && m.len() > 0))
    {
        return Err("CLI global context cannot be suppressed; remove its global instruction files before using Iris".into());
    }
    let program = p.engines.get(&r.harness).ok_or("unregistered engine")?;
    // Resolve Iris's managed symlinks, then trust every component of the actual binary.
    let program = program.canonicalize()?;
    let owner = std::fs::metadata(&program)?.uid();
    if owner == p.uid {
        return Err("CLI must not be writable by the agent user".into());
    }
    trusted(&program, &[0, owner])?;
    // SAFETY: this single-threaded helper sets the mask before creating CLI files.
    unsafe {
        libc::umask(0o077);
    }
    let err = cli_command(&program, &cwd, &p, &r).exec();
    Err(err.into())
}

fn cli_command(program: &Path, cwd: &Path, p: &Policy, r: &Request) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command
        .args(&r.args)
        .env_clear()
        .env("HOME", &p.home)
        .env("USER", &p.user)
        .env("LOGNAME", &p.user)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("PWD", cwd)
        .envs(r.env.iter().cloned())
        .current_dir(cwd);
    command
}

/// Removes shared prompt/config/image files on every exit path, including
/// adapter failures and routing fallbacks.
pub struct TurnFiles {
    dir: PathBuf,
}
impl TurnFiles {
    pub fn new(cfg: &AgentConfig, _scope: &str, dir: &Path) -> Option<Self> {
        (!cfg.user.is_empty()).then(|| Self { dir: dir.into() })
    }
}
impl Drop for TurnFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn copy_images(spec: &mut crate::harness::TurnSpec) -> Result<Vec<PathBuf>> {
    if !spec.split {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for (i, image) in spec.images.iter_mut().enumerate() {
        let dest = spec.run_dir.join(format!(
            "attachment-{i}.{}",
            match image.mime.as_str() {
                "image/png" => "png",
                "image/gif" => "gif",
                "image/webp" => "webp",
                _ => "jpg",
            }
        ));
        write_private_file(&dest, &std::fs::read(&image.path)?)?;
        image.path = dest.clone();
        files.push(dest);
    }
    Ok(files)
}

/// A per-turn, authenticated stdio relay. The actual Iris MCP bridge and
/// gateway proxies run as the service user; no agent process opens the DB.
pub struct Relays {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    dir: PathBuf,
}
impl Drop for Relays {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub async fn relay_servers(
    cfg: &AgentConfig,
    dir: &Path,
    servers: &mut [McpServer],
) -> Result<Option<Relays>> {
    let Some(p) = policy(cfg)? else {
        return Ok(None);
    };
    let mut guard = Relays {
        tasks: Vec::new(),
        dir: dir.into(),
    };
    for (i, server) in servers.iter_mut().enumerate() {
        let socket = dir.join(format!("mcp{i}.sock"));
        let listener = tokio::net::UnixListener::bind(&socket)?;
        group_readable(&socket, p.gid, 0o660)?;
        let token = crate::util::new_id();
        let McpTransport::Stdio { command, args, env } = &server.transport;
        let (command, args, env, auth) =
            (command.clone(), args.clone(), env.clone(), token.clone());
        guard.tasks.push(tokio::spawn(serve_relay(
            listener, command, args, env, auth, p.uid,
        )));
        server.transport = McpTransport::Stdio {
            command: cfg.launcher.clone(),
            args: vec!["mcp-relay".into(), socket.to_string_lossy().into_owned()],
            env: BTreeMap::from([("IRIS_RELAY_TOKEN".into(), token)]),
        };
    }
    Ok(Some(guard))
}

async fn serve_relay(
    listener: tokio::net::UnixListener,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    auth: String,
    uid: u32,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // A CLI may restart its MCP connection during a turn. Bind identity and
    // token here; no user/conversation/connector arguments come from the peer.
    while let Ok((mut stream, _)) = listener.accept().await {
        if !stream.peer_cred().is_ok_and(|cred| cred.uid() == uid) {
            continue;
        }
        let mut supplied = vec![0; auth.len()];
        if !matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stream.read_exact(&mut supplied)
            )
            .await,
            Ok(Ok(_))
        ) || supplied != auth.as_bytes()
        {
            continue;
        }
        let mut cmd = tokio::process::Command::new(&command);
        cmd.args(&args)
            .envs(&env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let Ok(mut child) = cmd.spawn() else {
            break;
        };
        let (mut reader, mut writer) = stream.split();
        let (Some(mut input), Some(mut output)) = (child.stdin.take(), child.stdout.take()) else {
            break;
        };
        tokio::select! {
            _ = tokio::io::copy(&mut reader, &mut input) => {},
            _ = tokio::io::copy(&mut output, &mut writer) => {},
        }
        let _ = input.shutdown().await;
        let _ = child.kill().await;
    }
}

pub fn relay_stdio(args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err("usage: iris mcp-relay SOCKET".into());
    }
    let mut stream = std::os::unix::net::UnixStream::connect(&args[0])?;
    stream.write_all(std::env::var("IRIS_RELAY_TOKEN")?.as_bytes())?;
    let mut input = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin(), &mut input);
        let _ = input.shutdown(std::net::Shutdown::Write);
    });
    std::io::copy(&mut stream, &mut std::io::stdout())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy() -> Policy {
        Policy {
            user: "iris-agent".into(),
            uid: 1234,
            gid: 1234,
            service_uid: 2345,
            home: "/var/lib/iris-agent/home".into(),
            workspace: "/var/lib/iris-agent/workspace".into(),
            runtime: "/var/lib/iris-agent/turns".into(),
            data_dir: "/private/iris".into(),
            config_file: "/private/config/config.toml".into(),
            launcher: "/usr/local/bin/iris".into(),
            engines: BTreeMap::from([("codex".into(), "/opt/iris/engines/bin/codex".into())]),
        }
    }

    #[test]
    fn launch_keeps_secrets_off_argv_and_preserves_stdin() {
        for split in [false, true] {
            let cfg = AgentConfig {
                user: if split {
                    "iris-agent".into()
                } else {
                    String::new()
                },
                ..Default::default()
            };
            let mut inv = Invocation {
                args: vec!["exec".into(), "-".into()],
                env: vec![("CODEX_API_KEY".into(), "synthetic-secret".into())],
                stdin: b"user prompt\n".to_vec(),
                ..Default::default()
            };
            wrap(&cfg, "codex", "conversation-1", &mut inv).unwrap();
            if split {
                assert_eq!(inv.program.as_deref(), Some(Path::new("/usr/bin/sudo")));
                assert_eq!(
                    inv.args,
                    [
                        "-n",
                        "-u",
                        "iris-agent",
                        "--",
                        "/usr/local/bin/iris",
                        "agent-exec"
                    ]
                );
                assert!(inv.env.is_empty());
                let mut input = inv.stdin.as_slice();
                let req = read_request(&mut input).unwrap();
                assert_eq!(req.scope, "conversation-1");
                assert_eq!(req.args, ["exec", "-"]);
                assert_eq!(req.env[0].1, "synthetic-secret");
                assert_eq!(input, b"user prompt\n");
                validate_request(&req, &test_policy()).unwrap();
            } else {
                assert!(inv.program.is_none());
                assert_eq!(inv.args, ["exec", "-"]);
                assert_eq!(inv.stdin, b"user prompt\n");
                assert_eq!(inv.env[0].1, "synthetic-secret");
            }
            assert!(!inv.args.iter().any(|a| a.contains("synthetic-secret")));
        }
    }

    #[test]
    fn request_validation_rejects_escape_and_environment_injection() {
        let p = test_policy();
        let mut r = Request {
            op: "exec".into(),
            scope: "conversation-1".into(),
            harness: "codex".into(),
            ..Default::default()
        };
        validate_request(&r, &p).unwrap();
        for scope in [
            "",
            ".",
            "..",
            "../owner",
            "owner/child",
            "legacy",
            "/absolute",
            "space here",
        ] {
            r.scope = scope.into();
            assert!(validate_request(&r, &p).is_err(), "{scope}");
        }
        r.scope = "conversation-1".into();
        for name in [
            "HOME",
            "PATH",
            "LD_PRELOAD",
            "NODE_OPTIONS",
            "SUDO_USER",
            "IRIS_DATA_DIR",
            "CODEX_HOME",
            "IRIS_GATEWAY_TOKEN",
        ] {
            r.env = vec![(name.into(), "x".into())];
            assert!(validate_request(&r, &p).is_err(), "{name}");
        }
        r.env.clear();
        r.harness = "/bin/sh".into();
        assert!(validate_request(&r, &p).is_err());
        r.harness = "codex".into();
        r.op = "chown".into();
        assert!(validate_request(&r, &p).is_err());
        r.op = "exec".into();
        r.args = vec!["nul\0arg".into()];
        assert!(validate_request(&r, &p).is_err());
    }

    #[test]
    fn framed_input_is_bounded_and_rejects_unknown_fields() {
        for bytes in [
            Vec::new(),
            vec![0; 8],
            (MAX_REQUEST as u64 + 1).to_le_bytes().to_vec(),
        ] {
            assert!(read_request(&mut bytes.as_slice()).is_err());
        }
        let json = br#"{"op":"exec","arbitrary_command":"sh"}"#;
        let mut bytes = (json.len() as u64).to_le_bytes().to_vec();
        bytes.extend(json);
        assert!(read_request(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn conversation_workdirs_are_distinct_and_legacy_is_quarantined() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config/config.toml"),
            data_dir: tmp.path().join("data"),
        };
        let old = paths.data_dir.join("workspace");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("AGENTS.md"), "synthetic injected instructions").unwrap();
        migrate_workspace(&paths).unwrap();
        let legacy = paths.data_dir.join("workspace-legacy");
        assert_eq!(
            std::fs::read_to_string(legacy.join("AGENTS.md")).unwrap(),
            "synthetic injected instructions"
        );
        assert_eq!(std::fs::metadata(&legacy).unwrap().mode() & 0o777, 0o700);
        let owner = workdir(&AgentConfig::default(), &paths, "owner-chat").unwrap();
        let shared = workdir(&AgentConfig::default(), &paths, "shared-chat").unwrap();
        assert_ne!(owner, shared);
        assert!(!owner.starts_with(&paths.data_dir));
        for dir in [owner, shared] {
            assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
            assert!(!dir.join("AGENTS.md").exists());
        }
        migrate_workspace(&paths).unwrap();
        assert!(legacy.join("AGENTS.md").exists());
        assert!(workdir(&AgentConfig::default(), &paths, "../escape").is_err());
    }

    #[test]
    fn migration_never_overwrites_an_existing_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        let old = paths.data_dir.join("workspace");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("keep"), "original").unwrap();
        std::fs::create_dir(paths.data_dir.join("workspace-legacy")).unwrap();
        assert!(migrate_workspace(&paths).is_err());
        assert!(old.join("keep").exists());
    }

    #[test]
    fn file_reads_are_scoped_and_do_not_follow_any_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("conversation");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/file.md"), "report").unwrap();
        std::fs::write(tmp.path().join("other.md"), "private").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("other.md"), root.join("link.md")).unwrap();
        std::os::unix::fs::symlink(root.join("nested"), root.join("directory-link")).unwrap();
        assert_eq!(
            read_scoped_file(&root, "nested/file.md").unwrap(),
            b"report"
        );
        for path in ["../other.md", "link.md", "directory-link/file.md", "nested"] {
            assert!(read_scoped_file(&root, path).is_err(), "{path}");
        }
        assert!(read_scoped_file(&root, tmp.path().join("other.md").to_str().unwrap()).is_err());
        let f = std::fs::File::create(root.join("large")).unwrap();
        f.set_len(MAX_FILE + 1).unwrap();
        assert!(read_scoped_file(&root, "large").is_err());
    }

    #[test]
    fn sudoers_is_exact_and_visudo_validates_it_when_available() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/setup-agent-user.sh");
        let out = std::process::Command::new("sh")
            .arg(&script)
            .args([
                "--render-sudoers",
                "iris-service",
                "iris-agent",
                "/usr/local/bin/iris",
            ])
            .output()
            .unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains(
            "iris-service ALL=(iris-agent) NOPASSWD: NOSETENV: /usr/local/bin/iris agent-exec"
        ));
        assert!(!text.contains('*'));
        assert!(!text.contains("env_keep"));
        let invalid = std::process::Command::new("sh")
            .arg(script)
            .args([
                "--render-sudoers",
                "service ALL",
                "iris-agent",
                "/usr/local/bin/iris",
            ])
            .output()
            .unwrap();
        assert!(!invalid.status.success());
        if let Some(visudo) = crate::harness::resolve_command("visudo").or_else(|| {
            ["/usr/sbin/visudo", "/sbin/visudo"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
        }) {
            let tmp = tempfile::tempdir().unwrap();
            let rule = tmp.path().join("sudoers");
            std::fs::write(&rule, text).unwrap();
            assert!(
                std::process::Command::new(visudo)
                    .args(["-cf"])
                    .arg(rule)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }

    #[test]
    fn shared_files_are_removed_even_after_adapter_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("turn");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("prompt"), "synthetic system prompt").unwrap();
        let cfg = AgentConfig {
            user: "iris-agent".into(),
            ..Default::default()
        };
        // Drop requires no privileged helper or CLI process.
        let guard = TurnFiles::new(&cfg, "conversation", &dir).unwrap();
        drop(guard);
        assert!(!dir.exists());
        assert!(TurnFiles::new(&AgentConfig::default(), "conversation", tmp.path()).is_none());
    }
    #[tokio::test]
    async fn relays_require_the_turn_token_and_expire_with_the_turn() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("turn");
        std::fs::create_dir(&dir).unwrap();
        let socket = dir.join("relay.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let token = "synthetic-turn-token";
        let task = tokio::spawn(serve_relay(
            listener,
            "/bin/cat".into(),
            Vec::new(),
            BTreeMap::new(),
            token.into(),
            unsafe { libc::geteuid() },
        ));
        let guard = Relays {
            tasks: vec![task],
            dir,
        };
        let mut wrong = tokio::net::UnixStream::connect(&socket).await.unwrap();
        wrong.write_all(&vec![b'x'; token.len()]).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), wrong.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        stream.write_all(token.as_bytes()).await.unwrap();
        stream.write_all(b"rpc payload").await.unwrap();
        let mut reply = vec![0; 11];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream.read_exact(&mut reply),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reply, b"rpc payload");
        drop(guard);
        assert!(!socket.exists());
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(tokio::net::UnixStream::connect(socket).await.is_err());
    }
    #[test]
    fn launcher_uses_the_agent_home_and_an_explicit_environment() {
        let p = test_policy();
        let cwd = p.workspace.join("conversation");
        let r = Request {
            args: vec!["exec".into()],
            env: vec![("CODEX_API_KEY".into(), "synthetic-key".into())],
            ..Default::default()
        };
        let cmd = cli_command(Path::new("/opt/iris/engines/bin/codex"), &cwd, &p, &r);
        let env: BTreeMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_str().unwrap(), v.unwrap().to_str().unwrap()))
            .collect();
        assert_eq!(env["HOME"], "/var/lib/iris-agent/home");
        assert_eq!(env["USER"], "iris-agent");
        assert_eq!(env["PWD"], "/var/lib/iris-agent/workspace/conversation");
        assert_eq!(env["PATH"], "/usr/local/bin:/usr/bin:/bin");
        assert_eq!(env["CODEX_API_KEY"], "synthetic-key");
        assert_eq!(env.len(), 7);
        assert_eq!(cmd.get_current_dir(), Some(cwd.as_path()));
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["exec"]);
    }
    #[test]
    fn shared_turn_files_are_group_readable_without_world_access() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("prompt.md");
        write_private_file(&file, b"synthetic context").unwrap();
        // Same-group chown of a file we own does not need privileges.
        let gid = unsafe { libc::getegid() };
        let owner = std::fs::metadata(&file).unwrap().uid();
        group_readable(&file, gid, 0o640).unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(meta.uid(), owner);
        assert_eq!(meta.gid(), gid);
        assert_eq!(meta.mode() & 0o777, 0o640);
        group_readable(tmp.path(), gid, 0o750).unwrap();
        assert_eq!(std::fs::metadata(tmp.path()).unwrap().mode() & 0o777, 0o750);
    }
}
