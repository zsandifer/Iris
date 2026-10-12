# Self-hosting Iris

The full technical guide. For the overview, see the [README](../README.md).

Iris is a self-hosted personal AI agent: one small Rust binary that gives you
a chat-first agent app (main chat, side chats, a feed, ideas, goals, a
library, push notifications) on your own machine.

Iris is a **wrapper, not a harness**. Like T3 Code, it drives the official
command-line agents, signed in with your own accounts:

- [Claude Code](https://docs.claude.com/en/docs/claude-code) (`claude`)
- [Codex CLI](https://github.com/openai/codex) (`codex`)
- [Gemini CLI](https://github.com/google-gemini/gemini-cli) (`gemini`)
- [OpenCode](https://opencode.ai) (`opencode`), which also reaches local
  open-weight models through Ollama or LM Studio

Any one of them is enough, and any mix works: there is no provider lock-in.
Iris ranks the models that are really available on your machine and picks
the best one for each job, and you can install, update, and sign in to the
CLIs from the app.

The CLI does all reasoning and tool use. Iris assembles the persona and
context, picks a model, streams the reply to your devices, and keeps the app
state (conversations, goals, feed, and so on) in SQLite.

> Status: early. The phone app (an installable web app) is served at `/`;
> shared chats with more than one person are a preview. A bare API test
> console is also built in at `/console`.

## Contents

- [How it works](#how-it-works)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Using Iris from your phone](#using-iris-from-your-phone)
- [Configuration](#configuration)
- [Engines and models](#engines-and-models)
- [Engines from the app](#engines-from-the-app)
- [Iris's computer (browser)](#iriss-computer-browser)
- [Connections and approvals](#connections-and-approvals)
- [Proactive jobs](#proactive-jobs)
- [Security model](#security-model)
- [Subscription terms](#subscription-terms)
- [Importing from OpenClaw](#importing-from-openclaw)
- [Running as a service](#running-as-a-service)
- [Development](#development)
- [Not done yet](#not-done-yet)
- [License](#license)

## How it works

```
phone / browser ──HTTPS (Tailscale) or loopback──▶ iris serve (axum, SQLite)
                                                     │  per message:
                                                     │  1. pick slot, model, effort
                                                     │  2. build system prompt from persona files
                                                     │  3. spawn the CLI with MCP config
                                                     ▼
          claude -p ... | codex exec --json ... | gemini -o stream-json | opencode run --format json
                                                     │
                                                     ├─▶ iris mcp (stdio) ─▶ goals, ideas, feed,
                                                     │                       library, cards
                                                     └─▶ iris mcp-gateway (stdio, one per connector)
                                                           │ private socket, per-turn token
                                                           ▼
                                        iris serve: approval gate, tripwires, activity log
                                                           │ secrets and tokens from the vault
                                                           ▼
                                        connector servers (Gmail, GitHub, any MCP server)
```

- **Persona**: `IDENTITY.md`, `SOUL.md`, `USER.md`, `TOOLS.md`,
  `PROACTIVE_PREFERENCES.md`, `HEARTBEAT.md`, and `memory/MEMORY.md` live in
  your data directory and are editable from the app. They are assembled into
  a system prompt for every turn and injected through each CLI's own
  mechanism (`--append-system-prompt-file` for Claude Code,
  `GEMINI_SYSTEM_MD` for Gemini CLI, an `instructions` file for OpenCode),
  before your message. Codex only accepts developer instructions on its
  command line, which other local users can read, so for Codex the prompt
  is sent on stdin at the start of each new session instead.
- **Sessions**: each conversation keeps a CLI session per harness and resumes
  it (`--resume`, `codex exec resume`, `-s`). If a conversation switches harness,
  Iris replays the messages that harness has not seen.
- **App bridge**: every turn gets a small MCP server, `iris mcp`, so the agent
  can create goals, ideas, feed posts, library items, and cards (for example
  a "Connect Google" card). It is a CRUD bridge into Iris's database, not a
  tool layer.
- **Connectors**: services you connect in the app (Google, GitHub, any MCP
  server) are reached through Iris's gateway, never handed to the CLI. Iris
  runs them with their secrets from the encrypted vault, so the agent never
  sees a password or token, and holds anything that sends, buys, deletes,
  exports, or reaches someone new until you approve it on a card. See
  [Connections and approvals](#connections-and-approvals).
- **Streaming**: CLI output is normalized into text deltas and tool activity
  and pushed to your devices over a WebSocket.

See [docs/API.md](API.md) for every route, event, and data type.

## Requirements

- Linux (macOS should work but is untested)
- Rust 1.88 or newer to build
- Any one of these CLIs, or none yet (you can install them from the app):
  - Claude Code 2.1 or newer
  - Codex CLI 0.162 or newer
  - Gemini CLI 0.63 or newer
  - OpenCode 1.19 or newer
- Node.js (with npm) on the server if you want to install Codex, Gemini CLI,
  or OpenCode from the app (Claude Code installs natively)
- Optional, for Iris's own browser: Chromium or Chrome, or Node.js to
  install Chromium from the app
- [uv](https://docs.astral.sh/uv/) (`uvx`) on the server if you want to
  connect Google (Gmail, Calendar, Drive)

Iris resolves the CLIs from its engines directory, then `PATH` (plus
`~/.local/bin` and similar), and reports their versions. It never pins or
vendors them and never modifies a CLI you installed yourself; it installs
and updates only the copies it installed (see
[Engines from the app](#engines-from-the-app)).

## Quick start

```sh
cargo build --release
install -Dm755 target/release/iris ~/.local/bin/iris

iris init        # writes ~/.config/iris/config.toml and ~/.local/share/iris/
iris status      # shows CLI versions, sign-in state, and available models
iris serve       # listens on 127.0.0.1:7420
```

In another terminal:

```sh
iris pair        # prints a one-time code, valid for 10 minutes
```

Open `http://127.0.0.1:7420/`, enter the code, and start chatting. A bare
API test console is still available at `/console`.

Edit the persona files in `~/.local/share/iris/persona/` (or through
`GET/PUT /api/files/system/{name}`), especially `USER.md`.

## Using Iris from your phone

The phone app needs HTTPS: browsers only allow installing it, its offline
support, and push notifications on a secure origin (`localhost` is the
only exception). Iris itself serves plain HTTP on a private address, so put
an HTTPS front on your private network. With [Tailscale](https://tailscale.com):

1. In the Tailscale admin console, turn on MagicDNS and HTTPS certificates.
2. Keep Iris on loopback (the default `bind = "127.0.0.1:7420"`) and let
   Tailscale serve it to your tailnet only:

   ```sh
   tailscale serve --bg 7420
   ```

   This prints an address such as `https://<machine>.<tailnet>.ts.net/`
   with a valid certificate, reachable only from your own devices.
3. Set `public_url` in `config.toml` to that address so `iris pair` prints
   it (it also serves as the contact push services require, unless you set
   `[push] vapid_subject`), then restart Iris.
4. On your phone (signed in to the same tailnet), open the address, enter
   the pairing code, and add Iris to your home screen.

Any other HTTPS reverse proxy on a private network works the same way.
Binding Iris to a Tailscale IP directly gives you plain HTTP: chat works in
the browser, but installing the app and notifications do not. Never expose
Iris to the open internet.

## Configuration

`iris init` writes a fully commented `config.toml` containing the defaults
([templates/config.toml](../templates/config.toml)). Highlights:

| Key | Default | Meaning |
|-----|---------|---------|
| `bind` | `127.0.0.1:7420` | Listen address. Keep it on loopback behind an HTTPS front such as `tailscale serve` ([phone setup](#using-iris-from-your-phone)). |
| `public_url` | `""` | HTTPS URL printed by `iris pair` (e.g. your `ts.net` name). |
| `agent_name` | `Iris` | Default name; renaming in the app overrides it immediately. |
| `max_concurrent_turns` | `2` | Cap on CLI subprocesses; extra turns queue. |
| `turn_timeout_secs` | `600` | A turn running longer is killed with its process group. |
| `strip_api_key_env` | `true` | Removes `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`/... from CLI environments so they use your subscription sign-in. |
| `[harness.*]` | | `command`, `models`, `efforts`, permission mode, sandbox, extra args. |
| `[router]` | | Shared-chat opt-in, rankings override, optional model classifier (below). |
| `[engines]` | | Engines directory, privileged install helper, daily auto-update. |
| `[heartbeat]`, `[feed]` | disabled | Scheduled jobs with active hours. |
| `[push]` | enabled | Web Push (VAPID). Needs `vapid_subject` (a `mailto:` URL) unless `public_url` is `https:`. |
| `[safety]` | | Approval timeout (300 s) and tripwires: sends (5), calls (200), and data read (25 MB) per connector per hour. |

Paths follow XDG: `$XDG_CONFIG_HOME/iris/config.toml` and
`$XDG_DATA_HOME/iris/`. Override with `--config`, `--data-dir`, or
`IRIS_CONFIG` / `IRIS_DATA_DIR`.

Data directory layout:

```
~/.local/share/iris/
├── iris.db          # SQLite (WAL), 0600
├── persona/         # IDENTITY.md, SOUL.md, USER.md, TOOLS.md, ...
├── memory/          # MEMORY.md plus notes the agent reads and writes
├── workspace/       # working directory for the CLIs
├── library/         # artifacts and media the agent saved
├── uploads/         # images you sent, custom avatar
├── run/             # per-turn private files (prompt, MCP config), gateway.sock
├── connectors/      # private working folders of connector programs
├── engines/         # CLIs installed from the app (single-user mode)
├── vault.key        # 32-byte key for encrypted secrets, 0600
├── vapid.json       # Web Push key pair, 0600
└── serve.lock       # held by the running `iris serve`
```

## Engines and models

End users never pick models. Each turn uses a **slot**: `chat` (everyday
conversation), `chat_strong` (clearly hard requests), `background`
(heartbeat), `feed`, and an optional `classifier`. A slot's model is the
highest-ranked model that is available on this machine right now (its CLI is
installed, enabled, and signed in, and the CLI lists the model), unless you
pin one in the app (`PUT /api/models/slots/{slot}`). Pinned slots never
change on their own; automatic ones follow what is installed, so a newly
installed CLI or a newer model (say `gpt-7-luna`) takes over automatically.

The ranking table is built in ([templates/model_rankings.toml](../templates/model_rankings.toml))
and can be extended with your own file (`router.rankings_file`). It matches
model families by pattern, so new versions slot in by themselves:

| Slot | Preference (abridged) |
|------|-----------------------|
| chat | Claude Haiku, GPT Luna, Gemini Flash, local models, ... |
| chat_strong | Claude Sonnet, GPT Sol, Gemini Pro, large local models, ... |
| background, feed | Claude Haiku, GPT Luna, Gemini Flash, local models, ... |

Effort starts at `low`. A rule-based classifier (no extra model call) raises
it to `medium` or `high` for code, research, planning, math, long or
multi-step requests, and when you say an answer was wrong, retry, or keep
working on the same problem; it drops back to `low` when the conversation
turns casual. Clearly hard requests also move to `chat_strong`. The choice
(slot, model, effort, and why) is stored in message metadata only.

If a CLI is missing or signed out, Iris uses the next-ranked model, even in
the middle of a turn, and tells you once with a "Reconnect" card. Features
degrade per CLI: if the model answering cannot see images, the agent says
so; models without an effort setting simply get none.

`iris status` lists every model, whether it is available, and each slot's
current choice.

## Engines from the app

After pairing your phone you should not need the server again:

- **Install and update.** Settings lists Claude Code, Codex, Gemini CLI, and
  OpenCode. Installing downloads only from the official source (Claude Code
  from Anthropic's release channel with SHA-256 verification against its
  manifest; the others from their official npm packages) into Iris's engines
  directory. Iris updates the engines it installed once a day
  (`engines.auto_update`) and never touches CLIs you installed yourself.
- **Sign in.** Right after an install, and whenever a CLI you use is signed
  out, Iris posts a card in your Main chat (and a push notification). Tapping
  it runs the CLI's own login on the server: for Claude you open the link,
  approve, and paste the code back into the app; for Codex you open the link
  and enter the code shown. Iris passes the code to the CLI and never stores
  or logs it, and never reads the CLI's credential files.
- **API keys.** Gemini CLI and OpenCode providers (OpenRouter, Anthropic,
  OpenAI, Google, Groq, DeepSeek, Mistral, xAI, OpenCode Zen) sign in with
  an API key you paste into their card or Settings, AI engine. Claude Code
  and Codex can optionally use an API key instead of your subscription
  (billed per use; while a key is saved, that engine uses it). Keys are
  encrypted in the vault, checked once with a tiny request (a refused key
  is not kept), and passed to the engine's CLI through its own environment
  variable on each run. Subscription sign-ins are never touched.
- **First run.** If no engine works yet, a "Set up an AI engine" card walks
  you through install, then sign-in.

For a stricter setup, `deploy/setup-engines.sh` (run once as root) creates a
separate `iris` system user that owns the engines directory and performs
installs through a narrow sudo rule. The user running Iris can then run the
engines but cannot modify them, and never needs download or install rights.

### What still needs the server

- Building and installing Iris itself, `iris init`, and the first
  `iris pair` (or running it as a service).
- Node.js, if you want Codex, Gemini CLI, or OpenCode installed from the
  app.
- Signing in to **Gemini CLI** with a Google account instead of an API key
  (it can only do that in an interactive terminal: run `gemini` once), and
  local models for **OpenCode** (configuring Ollama / LM Studio). API keys
  for both are added from the app.
- `uv`, if you want to connect Google.
- The optional privilege split (`deploy/setup-engines.sh`, as root).

## Iris's computer (browser)

Iris launches and owns one headless Chromium for the agent. Every engine
gets it as the `browser` MCP tools (open pages, read them, click, type,
screenshots, tabs), and you can watch it live and take over from the app
(Iris hub, Iris's computer). Iris itself is not an agent here: it carries
out each tool call the CLI makes, nothing more.

- **Getting a browser.** Tap "Set up the browser" in Iris's computer: Iris
  installs the official `playwright-core` npm package into its engines
  directory and runs its own `install chromium`, which downloads Chromium
  from Playwright's official servers (needs Node.js). Iris updates it daily
  with the other engines it installed, while the browser is closed. Or
  install Chromium or Chrome yourself; Iris looks at `browser.command`, then
  `PATH` (`chromium`, `google-chrome`, ...; snap packages are skipped because
  their sandbox cannot use Iris's profile), then Playwright's cache
  (`~/.cache/ms-playwright`).
- **System libraries.** A fresh server may lack the libraries Chromium
  needs; the app then shows the error. Fix it once as root with
  `npx playwright install-deps chromium` (or install your distribution's
  Chromium package).
- **Sign-ins persist.** The profile lives in `<data dir>/browser/profile`,
  so sites you or Iris signed in to stay signed in. Chrome's own password
  manager and autofill are turned off there. Chrome's session files
  (open tabs with what was typed into their forms) are deleted before each
  start and after each close.
- **Memory.** The browser starts on first use and closes after
  `browser.idle_mins` (default 10) without use, so it costs nothing while
  idle. A running browser with one tab takes roughly 200 to 400 MB.
- **Live view and take over.** Frames stream over the app's WebSocket only
  while you watch (about 1 per second for the small peek in chat, up to
  about 6 full screen, only when the page changes). Take over forwards your
  taps, scrolling, and typing; the agent's browser tools wait until you hand
  back.
- **Saved logins and cards.** The agent asks Iris to fill a login
  (`fill_login`) or card (`fill_card`) on the current page; Iris types it in
  from the vault and the agent only hears `filled` or `not found`. Logins go
  only to the site they were saved for, over HTTPS. Every card use asks you
  first with an approval card in the chat.
- **Who gets it.** Only the owner's own conversations and jobs (the profile
  holds the owner's sign-ins); never shared chats.
- **Sandbox.** Iris always starts Chrome with its sandbox. Some systems
  cannot start it (for example Ubuntu 23.10 and later, which restrict
  unprivileged user namespaces with AppArmor). Then the browser does not
  start, and Iris's computer explains and offers **Run without the
  sandbox**: only if you tap it (a saved choice, `POST /api/browser/sandbox`)
  does Iris run Chromium without it. The safer fix is one step as root, for
  example an AppArmor profile that allows user namespaces for that one
  binary (see Chromium's `docs/security/apparmor-userns-restrictions.md`):

  ```sh
  # replace the path with the browser Iris uses (GET /api/browser shows the
  # error; `ls -l <engines dir>/bin/browser` shows the installed one)
  sudo tee /etc/apparmor.d/iris-chrome <<'EOF2'
  abi <abi/4.0>,
  include <tunables/global>
  profile iris-chrome /path/to/chrome flags=(unconfined) {
    userns,
  }
  EOF2
  sudo apparmor_parser -r /etc/apparmor.d/iris-chrome
  ```
- **Approval lifetime.** Wallet fills use the gateway’s approval wait and
  cancellation rules. Denial, expiry, Stop, Pause, or Disconnect prevents
  the held fill; a late approval never restarts a turn.
- **Turning it off.** `[browser] enabled = false` removes the tools.
## Connections and approvals

Connections (MCP servers) give Iris your services: Gmail, Calendar, Drive,
GitHub, or any MCP server. Add them in the app under Iris, Connections, or
just ask in chat ("help me set up Google"); Iris posts a Connect card that
opens the step-by-step setup.

**How they run.** A connection is never handed to the AI CLI. Each turn
gets one small proxy per connection (`iris mcp-gateway`) that forwards to
`iris serve` over a private socket with a token valid only for that turn.
Iris runs the real server itself: a program it starts with the
connection's secrets in its environment (in a private folder outside the
agent's workspace), or a remote server it calls with the connection's
token. Secrets and tokens live in the encrypted vault and never reach the
agent. This works the same with Claude Code, Codex, Gemini CLI, and
OpenCode, and MCP servers you configured in a CLI yourself are switched
off in Iris's turns so nothing goes around the gateway. Codex's hosted
ChatGPT apps and plugins are also disabled in every turn, including your
own chats and scheduled jobs, with `features.apps=false` and
`features.plugins=false` (verified against Codex CLI 0.162.1). Connect
those services through Iris to use its approval rules. Only your own
personal chats and your scheduled jobs use connections; shared chats and
other people's chats never do. Idle
connection programs stop after `safety.idle_minutes`.

**The approval gate.** Iris looks at every call's real tool name, the
server's own hints, and the real arguments:

- Reading, searching, and drafting run on their own.
- Sending email or messages (and posting, replying, sharing, inviting,
  publishing pull requests, issues, or comments) asks first, unless every
  recipient is on your **trusted people** list and the service cannot add
  others (reply-all always asks).
- Buying or paying always asks. Deleting and bulk exports ask.
- Anything else that reaches someone new (for example a calendar invite to a
  new address) asks; once you approve someone, they are no longer new.

Asking means Iris holds the call, posts an approval card built from the
exact arguments (who, subject, the exact text, and the complete raw
request), sends a push, and waits up to `safety.approval_timeout_secs`. On
approve the held call runs unchanged, after Iris checks again that the
connection is still on and allowed; on deny, timeout, when the turn ends,
or when you pause Iris or turn the connection off, it never runs and the
agent is told. Multi-purpose tools are judged by the operation they are
asked to do, and an operation Iris can't place asks first. Each connection's rules can be
changed in plain words (reads freely and asks before sending, buying, or
deleting; asks before any change; only reads; asks every time), and each of
its actions can be set to run, ask, or never. Purchases always ask.

**Tripwires.** More than 5 sends, 200 calls, or 25 MB of data read through
one connection in an hour (all in `[safety]`) pauses that connection and
notifies you with a Resume button. Every call is in the activity log
(Settings, Activity log, Connected services).

**Pause Iris** (Iris's computer) cancels whatever is running (including
waiting approvals, and stops connection programs) and starts nothing new
(chats, check-ins, connection calls) until you resume. A request already
handed to a remote service before the pause cannot be recalled.
**Disconnect everything** turns every connection off at once, cancels browser
approvals, and closes and disconnects the browser. Reopen it in Iris’s
computer to enable browser tools again.

**Signing in to services.** Connections that use OAuth (the MCP sign-in
standard, OAuth 2.1 with PKCE) sign in on the service's own page and come
back to Iris at `<your Iris address>/api/oauth/callback`. When Iris is on
an https address (for example with `tailscale serve`, or `public_url`), it
completes by itself. Otherwise the provider sends your browser to
`http://127.0.0.1:<port>/api/oauth/callback`, which won't load on your
phone; copy that page's address back into the app. Iris registers itself
automatically with services that allow it (dynamic client registration);
for others you paste the client ID and secret of an app you create there.
Tokens are refreshed automatically; when a service refuses them, a
Reconnect card signs you in again.

Use HTTPS for connector URLs that receive OAuth tokens or saved headers.
Plain HTTP is allowed only for local servers at `localhost` or a loopback
IP (`127.0.0.1`, `[::1]`). This also applies to OAuth discovery; a server
elsewhere on your private network needs HTTPS to receive credentials.

- **Google** (Gmail, Calendar, Drive) runs the open-source
  [Google Workspace MCP server](https://github.com/taylorwilsdon/google_workspace_mcp)
  (`uvx workspace-mcp`) on this machine, in its mode where Iris does the
  sign-in and hands it a fresh token on each request. You need `uv` on the
  server and your own Google OAuth client: the app walks you through
  creating a Google Cloud project, turning on the three APIs, setting up
  the consent screen (add yourself as a test user; publish it to avoid
  re-signing in every 7 days), and creating a "Web application" client
  with the redirect URI the app shows. Paste the client ID and secret, then
  sign in. No client secret ships with Iris.
- **GitHub** uses GitHub's remote MCP server with a fine-grained personal
  access token, or your own GitHub OAuth app.
- **Custom**: any MCP server by web address (with sign-in, a header such as
  `Authorization`, or nothing) or by program (`npx ...`, `uvx ...`) with
  secrets as environment variables.

To try the gate end to end with a real engine and a fake messenger
(nothing is ever sent), run `scripts/e2e/connectors.sh` (needs Node.js and
Python 3).

## Proactive jobs

- **Heartbeat** (`[heartbeat]`): on a schedule, the agent works through
  `HEARTBEAT.md`. If something needs your attention it posts in your Main
  chat and sends a push notification; otherwise it stays silent.
- **Feed** (`[feed]`): the agent writes posts about your interests, guided by
  an editable feed prompt and the posts you liked. "Discuss" on a post opens
  a side chat seeded with it. It may also propose **Ideas**; tapping one
  starts a chat that does it.

Both are off by default, respect active hours, and can be triggered manually
(`POST /api/jobs/{heartbeat|feed}/run`).

## Security model

- Binds to loopback by default. To use it from your phone, put a private
  HTTPS front on it such as `tailscale serve`
  ([phone setup](#using-iris-from-your-phone)). Do not expose it to the
  internet.
- Every route except the static console page, `/api/health`, and
  `/api/pair` requires a device token. Devices pair with a one-time code from
  `iris pair`; only SHA-256 hashes of codes and tokens are stored. Five bad
  codes lock pairing for 15 minutes and burn outstanding codes.
- Saved logins, wallet cards, connector secrets and tokens, and engine API
  keys are encrypted with XChaCha20-Poly1305 using a key in `vault.key`
  (0600). The API never returns them.
- Connections run on Iris's side behind the approval gate; the CLI only
  gets a proxy and a per-turn token. Note that the agent's CLI runs as the
  same user as Iris: the gate stops a confused or misled agent from acting
  without you through connections, but an agent with unrestricted shell
  access could read anything that user can. Keep the CLIs' sandboxes on.
- Data directory files are created with owner-only permissions.
- The CLIs run with your user's permissions, in their own process groups,
  with a timeout. Claude Code runs headless with `--permission-mode
  acceptEdits --permission-prompts none` and a small allow-list; Codex runs
  with its `workspace-write` sandbox limited to `workspace/` and `memory/`;
  Gemini CLI runs with `--approval-mode auto_edit` (anything needing approval
  is not run); OpenCode, which has no OS sandbox, may edit files in
  `workspace/` and `memory/` while shell commands stay behind an approval
  that headless runs reject. Adjust all of them in `config.toml`.
- Engines installed from the app come only from official sources and are
  verified (checksum or npm integrity); see the privilege split above.
- Iris's browser is driven over a private pipe (no DevTools port is opened),
  and its tools reach the server through a Unix socket in the private run
  directory, accepted only for a running turn. Saved secrets go from the
  vault straight into the page (set on the exact field, on the same page
  and site that were checked); filled fields are masked on
  screen and in what the agent reads, any copy a page echoes (a dialog,
  another field) is redacted from tool results, and no screenshot is taken
  while a page shows one. Only `http`/`https` pages can be opened. When a
  turn ends or you take over, its pending browser actions stop.
- "Pause Iris" in Iris's computer stops running turns, closes the browser,
  and holds chats and jobs until you turn it back on.

## Subscription terms

Iris only runs the official, unmodified, signed-in CLIs. It never reads,
stores, proxies, or forwards Claude.ai or ChatGPT OAuth tokens or session
credentials; it only asks each CLI whether it is signed in (`claude auth
status`, `codex login status`) and keeps nothing but the yes/no answer.

Claude subscriptions are for individual use. Iris never uses Claude in
conversations with more than one person unless you opt in (in the app, or
`router.allow_claude_for_shared`); those use another engine. Your own
scheduled jobs (heartbeat, feed) on your own signed-in CLI are individual
use and may use Claude. If Claude is your only engine, group chats pause
with a one-time explanation and an opt-in card; everything else keeps
working. Only opt in if your plan allows shared use, and check the current
terms of each provider yourself.

## Importing from OpenClaw

```sh
iris import openclaw ~/openclaw-workspace --dry-run
iris import openclaw ~/openclaw-workspace
```

Copies persona files (`IDENTITY.md`, `SOUL.md`, `USER.md`, `TOOLS.md`,
`HEARTBEAT.md`), `MEMORY.md`, and `memory/**/*.md` into the data directory.
The source is never modified. Existing files are kept unless you pass
`--force`. Symlinks, hidden files, and non-text files are skipped.

## Running as a service

See [deploy/iris.service](../deploy/iris.service) for a systemd user unit and
install steps. Engines installed from the app are found automatically. For
CLIs you installed yourself, make sure the unit's `PATH` includes where they
live, or set absolute `command` paths in `config.toml`.

## Development

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Contributor notes, architecture, and invariants are in
[AGENTS.md](../AGENTS.md).

## Not done yet

- **Voice replies**: `POST /api/voice/speak` is a placeholder (returns 501).
- **Per-person memory**: people you add chat with Iris in their own chats
  and in shared chats, but only the owner has long-term memory, persona
  notes, and connectors.
- **Logins and cards inside cross-origin frames** (for example embedded
  payment widgets) cannot be filled; take over and type them yourself.
- **Full engine isolation in other people's chats**: MCP servers the
  engines have configured themselves (including claude.ai connectors) are
  off in every Iris turn; in other people's chats Iris also switches off
  Claude Code's CLAUDE.md files and auto memory, Codex's ChatGPT apps,
  plugins, and memories, Gemini CLI's extensions, and OpenCode's plugins
  and Claude Code files. Codex always loads a global `AGENTS.md` (or
  `AGENTS.override.md`) from its home folder and cannot be told not to, so
  while one exists Iris does not use Codex for other people's chats.
  Gemini CLI's global `GEMINI.md` context still applies, so keep personal
  details out of it if you add people.
- **Gemini CLI and OpenCode sign-in from the app**: both need one step on
  the server (see above).

## License

Dual-licensed under either of [Apache License 2.0](../LICENSE-APACHE) or
[MIT](../LICENSE-MIT), at your option.
