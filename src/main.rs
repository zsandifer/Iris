//! Iris: a self-hosted personal AI agent that wraps the official Claude Code
//! and Codex CLIs you already have installed and signed in.

mod auth;
mod catalog;
mod classifier;
mod config;
mod db;
mod demo;
mod engines;
mod entities;
mod events;
mod harness;
mod import;
mod jobs;
mod login;
mod mcp;
mod notify;
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
  iris mcp --data-dir DIR --user ID [--conversation ID] [--shared]
                                   Iris MCP bridge (launched by harnesses)
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
    let args = match Args::parse(std::env::args().skip(1)) {
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
        ["mcp"] => {
            args.check_flags(&["--user", "--conversation", "--shared"])?;
            let user = args.value("--user").ok_or("--user is required")?;
            // The harness launches this with the server's data dir; never
            // create a new data dir from here.
            if !paths.db_file().exists() {
                return Err(format!("no Iris database at {}", paths.db_file().display()).into());
            }
            let db = db::Db::open(&paths.db_file())?;
            mcp::run(mcp::Bridge {
                db,
                paths,
                user_id: user.to_string(),
                conversation_id: args.value("--conversation").map(String::from),
                shared: args.has("--shared"),
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
    db::Db::open(&paths.db_file())?;
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

fn serve(cfg: Config, paths: Paths) -> Result<()> {
    persona::seed(&paths)?;
    // Leftover per-turn files from a crash.
    if let Ok(rd) = std::fs::read_dir(paths.run_dir()) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let db = db::Db::open(&paths.db_file())?;
    let interrupted = db.with(db::mark_interrupted)?;
    if interrupted > 0 {
        warn!("{interrupted} turn(s) were interrupted by the last shutdown");
    }
    let rt = runtime(&cfg)?;
    rt.block_on(async move {
        let state = state::AppState::new(cfg, paths, db)?;
        state.set_serving();
        let probe = state.clone();
        tokio::spawn(async move {
            probe.refresh_harnesses().await;
            probe.check_recommendations().await;
            setup::post_setup_card_if_needed(&probe).await;
        });
        jobs::spawn_scheduler(state.clone());
        server::serve(state).await
    })
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
        cfg.base_url()
    );
    Ok(())
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
