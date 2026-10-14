//! Iris: a self-hosted personal AI agent that wraps the official Claude Code
//! and Codex CLIs you already have installed and signed in.

mod agent;
mod auth;
mod awareness;
mod browser;
mod catalog;
mod classifier;
mod config;
mod connectors;
mod db;
mod demo;
mod engines;
mod entities;
mod events;
mod feed;
mod gateway;
mod google_setup;
mod harness;
mod import;
mod jobs;
mod keys;
mod login;
mod mcp;
mod memory;
mod notify;
mod oauth;
mod persona;
mod push;
mod ranking;
mod router;
mod secrets;
mod server;
mod setup;
mod state;
mod turn;
mod util;

use config::{Config, Paths};
use std::path::PathBuf;
use util::Result;

const USAGE: &str = "\
iris - self-hosted personal AI agent (wraps the claude, codex, gemini, and opencode CLIs)

Usage:
  iris init                        Create config and data directory
  iris serve [--bind ADDR]         Run the API server
  iris pair [--user ID]            Create a one-time device pairing code
  iris status                      Show harness versions and sign-in state
  iris import openclaw <PATH> [--dry-run] [--force]
                                   Copy persona/memory files from OpenClaw
  iris mcp --data-dir DIR --user ID [--conversation ID] [--shared] [--workdir DIR]
                                   Iris MCP bridge (launched by harnesses)
  iris mcp --browser --data-dir DIR
                                   Browser tools for one turn (launched by
                                   harnesses; needs IRIS_BROWSER_TOKEN)
  iris mcp-gateway --data-dir DIR --connector NAME
                                   Connector gateway proxy (launched by
                                   harnesses; token in IRIS_GATEWAY_TOKEN)
  iris mcp-exec ENV_FILE COMMAND [ARGS...]
                                   Run an MCP server with the environment in
                                   ENV_FILE (launched by harnesses, so
                                   secrets stay off command lines)
  iris engines <install|update|uninstall> <NAME> [--dir PATH]
  iris engines list [--dir PATH]   Install/update the official CLIs (claude,
                                   codex, gemini, opencode) into the engines
                                   directory; prints JSON. Also the entry
                                   point of the privileged helper.
  iris demo-seed --assets DIR       Fill a fresh data directory with fake demo
                                   content (screenshots, trying the app);
                                   DIR is scripts/demo/images
  iris version

Global options:
  --config PATH     Config file (default $XDG_CONFIG_HOME/iris/config.toml)
  --data-dir PATH   Data directory (default $XDG_DATA_HOME/iris)
";

/// Minimal argument parser: positional words plus `--flag [value]`.
struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

const VALUE_FLAGS: &[&str] = &[
    "--config",
    "--data-dir",
    "--bind",
    "--user",
    "--conversation",
    "--dir",
    "--assets",
    "--connector",
    "--workdir",
    "--message",
];

impl Args {
    fn parse(raw: impl Iterator<Item = String>) -> std::result::Result<Self, String> {
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let mut it = raw.peekable();
        while let Some(a) = it.next() {
            if let Some((k, v)) = a.split_once('=').filter(|_| a.starts_with("--")) {
                flags.push((k.to_string(), Some(v.to_string())));
            } else if VALUE_FLAGS.contains(&a.as_str()) {
                let v = it.next().ok_or(format!("{a} needs a value"))?;
                flags.push((a, Some(v)));
            } else if a.starts_with('-') && a.len() > 1 {
                flags.push((a, None));
            } else {
                positional.push(a);
            }
        }
        Ok(Self { positional, flags })
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.as_deref())
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }

    fn check_flags(&self, allowed: &[&str]) -> std::result::Result<(), String> {
        for (k, _) in &self.flags {
            if !allowed.contains(&k.as_str()) && !["--config", "--data-dir"].contains(&k.as_str()) {
                return Err(format!("unknown option {k}"));
            }
        }
        Ok(())
    }
}

fn main() {
    // `mcp-exec` passes the server's own arguments through untouched, so it
    // bypasses the option parser.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().map(String::as_str) == Some("agent-exec") {
        if raw.len() != 1 {
            eprintln!("agent-exec accepts no arguments");
            std::process::exit(2);
        }
        if let Err(e) = agent::exec() {
            eprintln!("agent-exec: {e}");
            std::process::exit(1);
        }
        return;
    }
    if raw.first().map(String::as_str) == Some("mcp-relay") {
        if let Err(e) = agent::relay_stdio(&raw[1..]) {
            eprintln!("mcp-relay: {e}");
            std::process::exit(1);
        }
        return;
    }
    if raw.first().map(String::as_str) == Some("mcp-exec") {
        let err = mcp_exec(&raw[1..]);
        eprintln!("iris mcp-exec: {err}");
        std::process::exit(127);
    }
    let args = match Args::parse(raw.into_iter()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    let paths = Paths::resolve(
        args.value("--config").map(PathBuf::from),
        args.value("--data-dir").map(PathBuf::from),
    );
    let cmd: Vec<&str> = args.positional.iter().map(String::as_str).collect();
    if args.has("--help") || args.has("-h") {
        print!("{USAGE}");
        return Ok(());
    }
    match cmd.as_slice() {
        [] | ["help"] => {
            print!("{USAGE}");
            Ok(())
        }
        ["version"] => {
            println!("iris {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        ["init"] => {
            args.check_flags(&[])?;
            init(&paths)
        }
        ["serve"] => {
            args.check_flags(&["--bind"])?;
            let mut cfg = Config::load(&paths.config_file)?;
            if let Some(b) = args.value("--bind") {
                cfg.bind = b.to_string();
            }
            serve(cfg, paths)
        }
        ["pair"] => {
            args.check_flags(&["--user"])?;
            pair(&paths, args.value("--user"))
        }
        ["status"] => {
            args.check_flags(&[])?;
            status(paths)
        }
        ["mcp"] if args.has("--browser") => {
            args.check_flags(&["--browser"])?;
            let bridge = mcp::BrowserBridge {
                socket: browser::socket::path(&paths),
                token: std::env::var("IRIS_BROWSER_TOKEN").unwrap_or_default(),
            };
            mcp::serve_stdio(|req| bridge.handle(req))
        }
        ["mcp"] => {
            args.check_flags(&[
                "--user",
                "--conversation",
                "--message",
                "--shared",
                "--workdir",
            ])?;
            let user = args.value("--user").ok_or("--user is required")?;
            // The harness launches this with the server's data dir; never
            // create a new data dir from here.
            if !paths.db_file().exists() {
                return Err(format!("no Iris database at {}", paths.db_file().display()).into());
            }
            let db = db::Db::open(&paths.db_file())?;
            let agent = Config::load(&paths.config_file)?.agent;
            mcp::run(mcp::Bridge {
                db,
                paths,
                user_id: user.to_string(),
                conversation_id: args.value("--conversation").map(String::from),
                message_id: args.value("--message").map(String::from),
                shared: args.has("--shared"),
                workdir: args.value("--workdir").map(PathBuf::from),
                agent,
                status_token: std::env::var("IRIS_STATUS_TOKEN").ok(),
            })
        }
        ["mcp-gateway"] => {
            args.check_flags(&["--connector"])?;
            let connector = args.value("--connector").ok_or("--connector is required")?;
            let token = std::env::var(gateway::TOKEN_ENV)
                .map_err(|_| format!("{} is not set", gateway::TOKEN_ENV))?;
            gateway::proxy::run(gateway::proxy::Proxy {
                socket: gateway::socket_path(&paths),
                connector: connector.to_string(),
                token,
            })
        }
        ["engines", rest @ ..] => {
            args.check_flags(&["--dir"])?;
            let root = match args.value("--dir") {
                Some(d) => PathBuf::from(d),
                None => Config::load(&paths.config_file)?.engines_dir(&paths),
            };
            if !root.is_absolute() {
                return Err("--dir must be an absolute path".into());
            }
            let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
            engines::cli(&rest, &root)
        }
        ["import", "openclaw", source] => {
            args.check_flags(&["--dry-run", "--force"])?;
            let opts = import::Options {
                dry_run: args.has("--dry-run"),
                force: args.has("--force"),
            };
            let items = import::import_openclaw(&PathBuf::from(source), &paths, &opts)?;
            print!("{}", import::report(&items, opts.dry_run));
            Ok(())
        }
        ["demo-seed"] => {
            args.check_flags(&["--assets"])?;
            let assets = args.value("--assets").ok_or("--assets DIR is required")?;
            demo::seed(&paths, std::path::Path::new(assets))?;
            println!(
                "Seeded demo content into {}. Run iris serve and iris pair to look around.",
                paths.data_dir.display()
            );
            Ok(())
        }
        ["import", ..] => Err("usage: iris import openclaw <PATH> [--dry-run] [--force]".into()),
        other => Err(format!("unknown command '{}'\n\n{USAGE}", other.join(" ")).into()),
    }
}

/// Replace this process with `COMMAND ARGS...`, adding the variables from
/// the private JSON file `ENV_FILE`. Returns only on failure.
fn mcp_exec(raw: &[String]) -> String {
    use std::os::unix::process::CommandExt;
    let [env_file, command, rest @ ..] = raw else {
        return "usage: iris mcp-exec ENV_FILE COMMAND [ARGS...]".into();
    };
    let env: std::collections::BTreeMap<String, String> = match std::fs::read(env_file)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
    {
        Ok(env) => env,
        Err(e) => return format!("cannot read environment file: {e}"),
    };
    let err = std::process::Command::new(command)
        .args(rest)
        .envs(env)
        .exec();
    format!("cannot run {command}: {err}")
}

fn init(paths: &Paths) -> Result<()> {
    if let Some(dir) = paths.config_file.parent() {
        config::ensure_private_dir(dir)?;
    }
    if paths.config_file.exists() {
        Config::load(&paths.config_file)?;
        println!("config:   {} (kept)", paths.config_file.display());
    } else {
        config::write_private_file(&paths.config_file, config::CONFIG_TEMPLATE.as_bytes())?;
        println!("config:   {} (created)", paths.config_file.display());
    }
    let created = persona::seed(paths)?;
    let db = db::Db::open(&paths.db_file())?;
    prepare_memory(&db, paths)?;
    secrets::Vault::open(&paths.vault_key_file())?;
    push::VapidKeys::load_or_create(&paths.vapid_key_file())?;
    println!("data:     {}", paths.data_dir.display());
    if created.is_empty() {
        println!("persona:  existing files kept");
    } else {
        println!("persona:  created {}", created.join(", "));
    }
    println!(
        "\nNext steps:\n  1. Edit persona files in {} (or import: iris import openclaw <path>)\n  \
         2. Make sure at least one of `claude`, `codex`, `gemini`, or `opencode` is installed and signed in\n  \
         3. iris serve, then iris pair to connect a device",
        paths.persona_dir().display()
    );
    Ok(())
}

fn runtime(cfg: &Config) -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.worker_threads.max(1))
        .max_blocking_threads(16)
        .thread_keep_alive(std::time::Duration::from_secs(10))
        .enable_all()
        .build()?)
}

/// Hold an exclusive lock on the data directory for this process's
/// lifetime, so a second `iris serve` cannot clean up the first one's
/// per-turn files or mark its replies interrupted.
fn lock_data_dir(paths: &Paths) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    config::ensure_private_dir(&paths.data_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(paths.data_dir.join("serve.lock"))?;
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(format!(
            "another `iris serve` is already using {}",
            paths.data_dir.display()
        )
        .into());
    }
    Ok(file)
}

fn serve(cfg: Config, paths: Paths) -> Result<()> {
    let _lock = lock_data_dir(&paths)?;
    agent::clear_runtime(&cfg.agent)?;
    persona::seed(&paths)?;
    // Leftover per-turn files from a crash.
    if let Ok(rd) = std::fs::read_dir(paths.run_dir()) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let db = db::Db::open(&paths.db_file())?;
    prepare_memory(&db, &paths)?;
    let interrupted = db.with(db::mark_interrupted)?;
    if interrupted > 0 {
        warn!("{interrupted} turn(s) were interrupted by the last shutdown");
    }
    let rt = runtime(&cfg)?;
    rt.block_on(async move {
        let state = state::AppState::new(cfg, paths, db)?;
        state
            .browser
            .init_allow_no_sandbox(state.setting(server::ALLOW_NO_SANDBOX).await.is_some());
        state.set_serving();
        gateway::expire_stale_approvals(&state).await;
        gateway::listen(state.clone())?;
        let probe = state.clone();
        tokio::spawn(async move {
            probe.refresh_harnesses().await;
            // Reconcile cards left by an older daemon against persisted connection state.
            if let Ok(connectors) = probe.db.call(|c| entities::list_connectors(c, false)).await {
                for connector in connectors {
                    login::on_connector_updated(&probe, &connector.id).await;
                }
            }
            probe.check_recommendations().await;
            setup::post_setup_card_if_needed(&probe).await;
            // Jobs start once engines are known; before the first probe
            // every engine looks missing and a due job would fail.
            jobs::spawn_scheduler(probe);
        });
        server::serve(state).await
    })
}

/// Import Markdown notes Iris did not generate and rewrite the memory export.
fn prepare_memory(db: &db::Db, paths: &Paths) -> Result<()> {
    let report = memory::files::prepare(db, paths)?;
    if report.files > 0 {
        info!(
            "memory: imported {} new notes ({} already known) from {} files; originals in {}",
            report.counts.added,
            report.counts.existing,
            report.files,
            report
                .backup
                .as_deref()
                .unwrap_or(paths.data_dir.as_path())
                .display()
        );
    }
    Ok(())
}

fn pair(paths: &Paths, user: Option<&str>) -> Result<()> {
    let cfg = Config::load(&paths.config_file)?;
    persona::seed(paths)?;
    let db = db::Db::open(&paths.db_file())?;
    let (code, expires_at, name) = db
        .with(|c| {
            let uid = match user {
                Some(u) => u.to_string(),
                None => db::owner_user_id(c)?.expect("owner exists"),
            };
            let name = db::get_user(c, &uid)?.map(|u| u.display_name);
            Ok((uid, name))
        })
        .and_then(|(uid, name)| {
            let name = name.ok_or_else(|| format!("no user {uid}"))?;
            let (code, exp) = db.with(|c| Ok(auth::create_pairing_code(c, &uid)))??;
            Ok((code, exp, name))
        })?;
    let mins = (expires_at - util::now_ms()) / 60_000;
    println!("Pairing code for {name}: {code}");
    println!(
        "Open {} and enter the code (valid for {mins} minutes).",
        pairing_url(&cfg, &db)?
    );
    Ok(())
}

fn pairing_url(cfg: &Config, db: &db::Db) -> Result<String> {
    if !cfg.public_url.trim().is_empty() {
        return Ok(cfg.base_url());
    }
    Ok(db
        .with(|c| db::get_setting(c, "server.bound_url"))?
        .unwrap_or_else(|| cfg.base_url()))
}

fn status(paths: Paths) -> Result<()> {
    let cfg = Config::load(&paths.config_file)?;
    persona::seed(&paths)?;
    let db = db::Db::open(&paths.db_file())?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let state = state::AppState::new(cfg, paths, db)?;
        state.refresh_harnesses().await;
        println!(
            "isolation: {}{}",
            if state.cfg.agent.user.is_empty() {
                "single_user"
            } else {
                "split"
            },
            if state.cfg.agent.user.is_empty() {
                String::new()
            } else {
                format!(" (CLI user: {})", state.cfg.agent.user)
            }
        );
        for h in state.harness_statuses() {
            println!(
                "{:<8} enabled={:<5} installed={:<5} auth={:<13} version={} path={}",
                h.name,
                h.enabled,
                h.installed,
                serde_json::to_value(h.auth)?.as_str().unwrap_or("?"),
                h.version.as_deref().unwrap_or("-"),
                h.path.as_deref().unwrap_or("-"),
            );
        }
        let catalog = state.catalog();
        println!("\nmodels:");
        for m in &catalog.models {
            println!(
                "  {:<40} available={:<5} efforts={}{}{}",
                m.id,
                state.model_available(m),
                m.efforts.join(","),
                if m.local { " (local)" } else { "" },
                if m.hidden {
                    " (hidden in CLI picker)"
                } else {
                    ""
                }
            );
        }
        println!("\nslots (automatic recommendation, or pinned model):");
        for slot in ranking::Slot::ALL {
            let (res, pin) = state.resolve_slot(slot, false).await;
            println!(
                "  {:<12} {}{}",
                slot.name(),
                res.effective.as_deref().unwrap_or("(none available)"),
                match (&pin, res.pin_unavailable) {
                    (Some(_), true) => " (pinned model unavailable; using recommendation)",
                    (Some(_), false) => " (pinned)",
                    _ => "",
                }
            );
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Args {
        Args::parse(s.split_whitespace().map(String::from)).unwrap()
    }

    #[test]
    fn pairing_uses_actual_listener_or_public_url() {
        let db = db::Db::open_in_memory().unwrap();
        let mut cfg = Config::default();
        db.with(|c| db::set_setting(c, "server.bound_url", "http://127.0.0.1:7999"))
            .unwrap();
        assert_eq!(pairing_url(&cfg, &db).unwrap(), "http://127.0.0.1:7999");
        cfg.public_url = "https://iris.example.test".into();
        assert_eq!(pairing_url(&cfg, &db).unwrap(), "https://iris.example.test");
    }

    #[test]
    fn one_server_per_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        let first = lock_data_dir(&paths).unwrap();
        let err = lock_data_dir(&paths).unwrap_err().to_string();
        assert!(err.contains("already using"), "{err}");
        drop(first);
        assert!(lock_data_dir(&paths).is_ok(), "released with the process");
    }

    #[test]
    fn parses_args() {
        let a = parse("import openclaw /x --dry-run --data-dir /d");
        assert_eq!(a.positional, ["import", "openclaw", "/x"]);
        assert!(a.has("--dry-run"));
        assert_eq!(a.value("--data-dir"), Some("/d"));
        let a = parse("serve --bind=127.0.0.1:9");
        assert_eq!(a.value("--bind"), Some("127.0.0.1:9"));
        assert!(a.check_flags(&["--bind"]).is_ok());
        assert!(parse("serve --bogus").check_flags(&["--bind"]).is_err());
        assert!(Args::parse(["--config".to_string()].into_iter()).is_err());
    }
}
