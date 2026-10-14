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

Connectors (MCP servers the owner connects) are the one place Iris sits in
the tool path, and only as a gateway: the CLI gets a stdio proxy per
connector (`iris mcp-gateway`), and the daemon forwards its calls to the
real servers with secrets from the vault, enforcing the owner's approval
rules (`src/gateway/`). Never hand connector configs or secrets to a CLI,
and never let the gateway decide what to call or rewrite a call: it may
only run a call unchanged, hold it for approval, or refuse it.

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
   Playwright MCP (Iris's browser server, not an agent CLI) is the
   exception to pinning: it stays on a CI-tested release
   (`PLAYWRIGHT_MCP_VERSION`).
6. **Stay small.** Low idle memory (two worker threads, one SQLite
   connection, no background polling beyond the minute scheduler) and a
   lean dependency tree. Justify new dependencies.
7. **`templates/config.toml` must equal `Config::default()`.** A test
   enforces it; update both together.

8. **Agent isolation.** In split mode the agent user never has read access
   to Iris's data directory or configuration. Only the fixed root-owned
   `iris agent-exec` launcher is allowed by sudoers; secrets never go on
   argv. CLI turns, probes and app login run as the configured agent user.
   Memory CRUD and connector calls stay on Iris's side of authenticated,
   turn-scoped MCP proxies.
9. **No implicit project instructions.** Every conversation has its own
   working directory. Project docs/context files never load, including in
   owner chats, jobs, resumed turns, classifiers and key checks. Do not
   restore a shared cwd or silently weaken a CLI's context restrictions.

## Layout

```
src/
├── main.rs          CLI: init, serve, pair, status, mcp, mcp-gateway, import openclaw
├── config.rs        config.toml schema, defaults, XDG paths, private file helpers
├── db.rs            SQLite schema/migrations, users, devices, conversations, messages
├── entities.rs      feed, ideas, goals, library, cards, connectors, credentials, push subs
├── auth.rs          pairing codes, device tokens, lockout
├── persona.rs       system files, per-turn system prompt, persona hash
├── memory/          owner memory: notes in SQLite (history, FTS5), the
│                    generated read-only Markdown export, Markdown import
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
├── gateway/
│   ├── mod.rs       connector gateway: turn grants, socket, approvals, tripwires
│   ├── policy.rs    approval rules: classify calls, recipients, previews
│   ├── upstream.rs  MCP client to connector servers (stdio, Streamable HTTP)
│   └── proxy.rs     `iris mcp-gateway` stdio proxy the CLIs start
├── oauth.rs         connector sign-in: MCP auth discovery, DCR, PKCE, refresh
├── connectors.rs    connector catalog (Google, GitHub, custom) and setup
├── keys.rs          engine API keys (vault) passed to CLIs per turn
├── state.rs         shared state, harness registry, settings
├── events.rs        WebSocket event hub
├── server/          axum routes (chat, content, admin), WebSocket, app + /console
├── demo.rs          `iris demo-seed`: fake demo data for screenshots
├── mcp.rs           `iris mcp` stdio MCP bridge (CRUD into the DB), `iris mcp --browser`
├── browser/         Iris's computer: wrapper around Microsoft's Playwright MCP
│                    (start/stop, reviewed tools for the agent, live view,
│                    take over, owner-initiated login/card fill, AppArmor setup)
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
the connector gateway (approval gate, trusted people, tripwires, pause,
disconnect, against a fake connector), connector sign-in (discovery, DCR,
PKCE, refresh, against a fake provider), import, the subprocess runner, and
the browser wrapper (tool allowlist, take over, private pages, saved-login
fill, sandbox consent, live view, against a stand-in Playwright MCP server).
`IRIS_REAL_BROWSER=<engines dir> cargo test real_playwright -- --ignored`
runs it against the real Playwright MCP and Chromium after
`iris engines install browser --dir <engines dir>` (CI does this).

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
MCP approval (`default_tools_approval_mode`), MCP tool-call timeouts
(Claude Code per-server `timeout` and `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS`,
Codex `tool_timeout_sec`, Gemini CLI and OpenCode `timeout`), and the
auth-failure output. When Playwright MCP updates: its tool names (the
allowlist in `src/browser/tools.rs`), the markdown it returns (page header,
tab list, `### Result`), and its config file keys (`src/browser/launch.rs`). `scripts/e2e/connectors.sh <model>` checks the
approval gate end to end with a fake messenger connector.
`scripts/e2e/browser-loop/` runs real browsing tasks (public sites and
practice logins) against a separate TEST Iris and has a cheap model judge
them; see its README before changing browser code or the agent's browser
guidance.

## Conventions

- Small, focused commits with descriptive messages.
- Errors shown to end users are friendly and first-person (the agent's
  voice); details go in message metadata.
- End users never see model names, tokens, or effort levels by default.
- Keep `docs/API.md` in sync with routes and events.
