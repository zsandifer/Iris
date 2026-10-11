# Contributing to Iris

Guide for humans and coding agents working on this repository. Read it
before changing code. `CLAUDE.md` imports this file.

## What Iris is

A single Rust binary (`iris`) that turns the official, already-installed
`claude` (Claude Code), `codex` (Codex CLI), `gemini` (Gemini CLI), and
`opencode` (OpenCode, including local models) command-line agents into a
personal agent app backend: conversations, feed, ideas, goals, library,
connectors, credentials vault, push notifications, scheduled jobs.

**Iris is a wrapper, not a harness.** It must never implement an agent loop,
call model APIs directly, execute tools, or grow its own tool system. The
CLI does all reasoning and tool use. Iris builds prompts and command lines,
manages subprocesses, normalizes their output, and stores app state.

## Invariants (do not break these)

1. **No credential handling for subscriptions.** Never read, store, proxy,
   log, or forward Claude.ai or ChatGPT OAuth tokens or the CLIs' credential
   files. Only run the official binaries. Sign-in state comes from the CLI's
   own status command (`claude auth status`, `codex login status`); keep
   only the boolean (that output contains account details).
2. **Individual-use policy.** Claude is never used for conversations with
   more than one human unless the owner opts in
   (`router.allow_claude_for_shared` or the app setting). The owner's own
   scheduled jobs on their own signed-in CLI are ordinary individual use.
   Enforced in `router::policy_allows` and `router::select_engine`; keep
   the tests. If Claude is the only engine, shared chats pause with a
   one-time explanation and opt-in card; nothing else stops working.
3. **No personal data in the repo.** Templates, tests, fixtures, docs, and
   examples must be generic. No real names, emails, hostnames, tailnet
   names, or captured CLI output containing local paths or account data.
   Use synthetic samples modeled on real output.
4. **Secrets stay secret.** Credential and connector secrets are encrypted
   with the vault (`secrets.rs`), never returned by the API, never logged.
   Logs carry ids, sizes, and outcomes, not message content.
5. **Never pin or vendor the CLIs; never touch the operator's own
   installs.** Resolve them from Iris's engines directory, config, or PATH
   and report their versions. Iris installs or updates a CLI only from its
   official source (Anthropic's release manifest with SHA-256 verification;
   the vendors' npm packages), only into its engines directory, and only
   when the owner asks in the app or has `engines.auto_update` on. It never
   updates or removes a CLI it did not install (`engines.rs`, `setup.rs`).
6. **Stay small.** Low idle memory (two worker threads, one SQLite
   connection, no background polling beyond the minute scheduler) and a
   lean dependency tree. Justify new dependencies.
7. **`templates/config.toml` must equal `Config::default()`.** A test
   enforces it; update both together.

## Layout

```
src/
├── main.rs          CLI: init, serve, pair, status, mcp, import openclaw
├── config.rs        config.toml schema, defaults, XDG paths, private file helpers
├── db.rs            SQLite schema/migrations, users, devices, conversations, messages
├── entities.rs      feed, ideas, goals, library, cards, connectors, credentials, push subs
├── auth.rs          pairing codes, device tokens, lockout
├── persona.rs       system files, per-turn system prompt, persona hash
├── catalog.rs       model catalog (config + models the CLIs report)
├── ranking.rs       model families, slot rankings, slot resolution
├── classifier.rs    rule-based effort / escalation classifier
├── router.rs        engine selection per slot + individual-use policy
├── harness/
│   ├── mod.rs       Harness trait, TurnSpec, normalized HarnessEvent
│   ├── claude.rs    Claude Code adapter + stream-json parser
│   ├── codex.rs     Codex adapter + exec --json parser
│   ├── gemini.rs    Gemini CLI adapter + stream-json parser
│   ├── opencode.rs  OpenCode adapter + run --format json parser
│   └── runner.rs    subprocess runner: cap, stdin, timeout, process-group kill
├── turn.rs          the turn pipeline (routing, replay, fallback, persistence)
├── state.rs         shared state, harness registry, settings
├── events.rs        WebSocket event hub
├── server/          axum routes (chat, content, admin), WebSocket, app + /console
├── demo.rs          `iris demo-seed`: fake demo data for screenshots
├── mcp.rs           `iris mcp` stdio MCP bridge (CRUD into the DB)
├── jobs.rs          heartbeat and feed jobs, scheduler
├── feed/            For you pipeline: sources, polite fetching, page extraction,
│                    ranking, adaptive size, urgent pushes (two model calls a day)
├── login.rs         engine sign-in from the app, reconnect cards
├── engines.rs       install/update the official CLIs (official sources only)
├── setup.rs         engine operations, setup card, daily update
├── secrets.rs       XChaCha20-Poly1305 vault
├── push.rs          Web Push: VAPID + RFC 8291 encryption
├── notify.rs        push fan-out, quiet-hours queue and digest
└── import.rs        OpenClaw import
templates/           persona templates, config template, model rankings
docs/API.md          REST, WebSocket, data model, MCP reference
docs/DESIGN.md       app UX spec
docs/SELF-HOSTING.md full setup and operations guide
deploy/iris.service  systemd user unit
deploy/setup-engines.sh  one-time root setup for the engine-install privilege split
```

## Adding a harness adapter

1. Implement `harness::Harness` in `src/harness/<name>.rs`:
   - `build`: argv and stdin for one turn. Inject the system prompt with the
     CLI's own mechanism (never by editing its config files). Keep the user
     prompt off argv (stdin) so it cannot be parsed as flags.
   - `parser`: turn JSONL into `HarnessEvent`s. Emit exactly one terminal
     event (`Done`, `Error`, or `AuthRequired`) per run.
   - `auth_probe_args` / `parse_auth_probe`: the CLI's own sign-in check.
   - `resume_updates_system_prompt`: whether a resumed session sees a new
     system prompt (if not, Iris starts a fresh session when the persona
     changes).
   - `api_key_env`: variables to strip so the CLI uses its subscription.
   - `capabilities`: images, effort, MCP, resume. Features degrade per
     flag (for example images are left out with a conversational note).
   - `models_probe_args` / `parse_models` if the CLI can list its models.
2. Register it in `config::SUPPORTED_HARNESSES`, `HarnessConfig::defaults_for`,
   `state::AppState::new`, and `templates/config.toml`. Add its model
   families to `templates/model_rankings.toml` (with `harnesses` listing
   it) so its models rank where they belong.
3. Add parser tests with synthetic samples modeled on real output, including
   an auth failure and a crash without a terminal event.
4. Verify against the real CLI (see below) and document flags you rely on.

## Testing

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Unit tests cover the router and policy, persona assembly, stream parsing,
pairing/auth, the vault, push encryption (RFC 8291 vector), the MCP bridge,
import, and the subprocess runner.

End-to-end check against the real CLIs (uses a temp data dir; costs a few
tiny requests on your subscriptions):

```sh
T=$(mktemp -d)
iris init --config $T/config.toml --data-dir $T/data
iris serve --config $T/config.toml --data-dir $T/data --bind 127.0.0.1:7999 &
iris pair --config $T/config.toml --data-dir $T/data
# open http://127.0.0.1:7999/ , pair, chat; or script it against docs/API.md
```

Things worth re-verifying when a CLI updates: stream event shapes, `--resume`
and `codex exec resume`, `--system-prompt-snapshot`, `developer_instructions`,
MCP approval (`default_tools_approval_mode`), and the auth-failure output.

## Conventions

- Small, focused commits with descriptive messages.
- Errors shown to end users are friendly and first-person (the agent's
  voice); details go in message metadata.
- End users never see model names, tokens, or effort levels by default.
- Keep `docs/API.md` in sync with routes and events.
