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
  `PROACTIVE_PREFERENCES.md`, and `HEARTBEAT.md` live in your data directory
  and are editable from the app. Memory is stored as notes in the database;
  `memory/MEMORY.md` (what the agent always keeps in mind) and one file per
  topic are a read-only copy Iris rewrites after every change. They are assembled into
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
- Optional, for Iris's own browser: Node.js to install it from the app
  (Microsoft's Playwright MCP server and its Chromium), or `playwright-mcp`
  on `PATH`
- [uv](https://docs.astral.sh/uv/) (`uvx`) on the server if you want to
  connect Google (Gmail, Calendar, Drive, Docs, Sheets, Slides, Forms, Tasks,
  Contacts, YouTube read-only)

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
| `[agent]` | `user = ""`, `launcher = "/usr/local/bin/iris"` | Optional OS isolation for CLI turns, status probes and sign-in. |
| `[heartbeat]` | disabled | Proactive check-in with active hours. |
| `[dream]` | enabled, 3am, 4 messages | Nightly private memory tidying (`hour` 0–23 local, `min_messages` 1–80). |
| `[feed]` | enabled | The For you pipeline: checks hourly between 06:00 and 22:00, publishes one batch a day. |
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
├── memory/          # read-only Markdown copy of memory: MEMORY.md, you.md, people.md, ...
├── memory-backup/   # Markdown notes as they were before Iris imported them
├── workspace-legacy/ # old shared cwd, quarantined on upgrade
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
installs through a narrow sudo rule. The rule includes install, update, and
uninstall for `browser` (Playwright MCP and its Chromium); rerun the script if your rule
was created before browser support was added. Where AppArmor restricts
user namespaces (Ubuntu 23.10 and later), the script also approves Iris's
own browser for Chrome's sandbox (see
[Iris's computer](#iriss-computer-browser)); rerun it to add that to an
existing setup. The user running Iris can
then run the engines but cannot modify them, and never needs download or
install rights.

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

### Isolate the agent from Iris's private data

Use a dedicated Linux account for CLI reasoning and tools. The Iris service
account owns its database, vault key, config, persona and memory (directories
0700, files 0600). The `iris-agent` account owns only its CLI home and its
conversation directories. Linux permissions enforce the boundary even if a
CLI's tool permissions are bypassed. This protects secrets stored by Iris;
the agent still receives the system prompt, user messages, selected engine
API keys, and approved connector results needed for its turn.

Initialize Iris and stop its service as your existing service account first.
The following commands are examples with the synthetic service account
`iris-service`; replace it with the account that already runs Iris. Run the
root commands through your administrator:

```sh
# Build from the updated checkout, then as iris-service:
cargo build --release
install -Dm755 target/release/iris ~/.local/bin/iris
iris init
systemctl --user stop iris

# As root, from the checkout; install the root-owned launcher first:
install -Dm755 target/release/iris /usr/local/bin/iris
# Installs engine administration AND calls setup-agent-user.sh:
sh deploy/setup-engines.sh iris-service /usr/local/bin/iris /opt/iris/engines iris-agent
```

The setup is idempotent. It creates `iris-agent` and its dedicated group,
`/var/lib/iris-agent/home` (0700), a workspace parent (service-owned 0730,
agent group), and a turn runtime directory (service-owned 0750, agent group).
The launcher creates `workspace/<conversation-id>` as `iris-agent`, mode
0700. The service cannot read these directories directly; library saves use
a bounded, descriptor-relative file read by the fixed launcher. The script
validates the exact sudoers rule with `visudo -cf`, installs the root-owned
path policy `/etc/iris-agent.json`, locks down data/config permissions, and
prints every change. It adds the service user to the agent group for sharing
only per-turn files and installs `NoNewPrivileges=no` as a service drop-in.
It grants **no sudo permissions to the agent account**. It supports one Iris
service installation per machine. For custom data/config locations, supply
arguments five and six to `setup-engines.sh` (or run
`setup-agent-user.sh SERVICE AGENT BINARY DATA CONFIG ENGINES` separately).
Keep managed engines outside the private data directory, readable by the
agent and writable only by Iris's engine administrator. The root policy can
register other absolute CLI paths; their real targets and every ancestor
must be protected from agent, group and world writes.

Add the printed engine settings and these settings to the service account's
config:

```toml
[engines]
dir = "/opt/iris/engines"
helper = ["sudo", "-n", "-u", "iris", "/usr/local/bin/iris", "engines"]

[agent]
user = "iris-agent"
launcher = "/usr/local/bin/iris"
```

Restart the service user's **user manager**, so its supplementary group
membership is refreshed (restarting only the Iris unit is insufficient).
This stops that user's other user services too; the administrator can run
`loginctl terminate-user iris-service`, after the user has saved their work.
Then sign in again as `iris-service`, run `systemctl --user daemon-reload`
and `systemctl --user start iris`. Existing CLI sign-ins belong to the old
account: use the app to sign Claude/Codex in again. Gemini's interactive
sign-in and OpenCode provider setup must also run **as iris-agent**, with
`HOME=/var/lib/iris-agent/home`, using `runuser` from an administrator's
terminal. Never copy subscription credential files. `iris status`,
`/api/harnesses` and `/api/engines` report the mode, CLI user, and auth state
for that account; Gemini has no auth-status command, so its status stays
unknown until a turn reports it.

After engines are installed and Iris has created its vault key, run as the
service account:

```sh
sh deploy/setup-agent-user.sh --check iris-service iris-agent /usr/local/bin/iris
sh scripts/e2e/isolation.sh iris-agent
```

These read-only checks run the exact launcher as `iris-agent`, prove that
opening `vault.key`, `iris.db` and config returns `PermissionDenied`, and
verify its UID. They never use arbitrary sudo commands. Root setup is
required before enabling the split; Iris refuses mismatched accounts,
paths, unsafe permissions or unregistered CLI paths.

All conversations now have separate working directories. In single-user
mode the workspace tree is a sibling of the data directory (`iris-workspace`
by default), keeping it outside the harnesses' denied data paths. On first
startup old `data/workspace` contents are renamed to
`data/workspace-legacy` with mode 0700. Existing contents in an unscoped new
workspace are preserved as `data/workspace-legacy-unscoped`. No files are
executed or copied into conversation directories, and an existing archive
is never overwritten. Old CLI sessions are invalidated once so their
implicit instructions are not resumed; conversation history remains in
Iris. Changing the agent account also resets CLI sessions.

The agent reads and updates Markdown memory through the existing Iris MCP
bridge's owner-only `read_memory` / `write_memory` tools, rather than direct
file access. Split-mode MCP stdio processes are authenticated per-turn
proxies: the actual bridge and gateway proxies run on Iris's side. Only
prompt/config/image files and proxy sockets are shared with the agent group
(0750 directories, 0640 files, 0660 sockets); they are removed when the turn
ends or at the next startup after a crash. Connector secrets remain in Iris.

Harness restrictions apply in **both** modes, including private and resumed
turns:

- Claude disables CLAUDE.md and auto-memory loading, disables all settings
  sources, uses strict MCP configuration, denies Read/Edit/Write/Glob/Grep
  on Iris data/config and CLI credential files, and denies Bash entirely
  (shell path patterns cannot cover arbitrary command syntax).
- Codex retains `workspace-write`, limits writable roots to the conversation,
  excludes `/tmp` and `$TMPDIR`, disables project documents with
  `project_doc_max_bytes=0`, and disables CLI memories/apps/plugins. Its named
  `permissions.iris` profile denies reads of Iris data/config and credential
  files. Verified against installed Codex CLI 0.162.1 help and its strict
  config parser: `sandbox_workspace_write.denied_paths` is unsupported;
  named filesystem profiles support `deny`. See the
  [official config reference](https://developers.openai.com/codex/config-reference/).
- Gemini 0.63's `context.fileName=[]` **does not disable GEMINI.md**. Iris
  uses `GEMINI_RESTRICTED_MODE=true`, which disables initial and just-in-time
  project context, and CLI policy rules deny shell and local file tools.
  Extensions are off. Restricted mode also disables MCP and workspace
  settings, so Gemini cannot use Iris memory, app or connector tools;
  its capabilities report `mcp=false`. Google sign-in still belongs to the
  agent account. A user setting requiring folder trust can make Gemini refuse
  headless runs; Iris never bypasses this by trusting project files. See
  [Gemini's trust behavior](https://github.com/google-gemini/gemini-cli/blob/v0.63.0/packages/core/src/utils/trust.ts)
  and [memory context implementation](https://github.com/google-gemini/gemini-cli/blob/v0.63.0/packages/core/src/context/memoryContextManager.ts).
- OpenCode disables project configuration, Claude context and external
  plugins (`--pure`) in every turn. Version 1.19 still attaches nested
  AGENTS.md through its built-in Read tool, so Iris withholds Read, Glob,
  Grep, Bash and external-directory access; app/connector MCP tools remain
  available. Edit rules deny private data/config/credential paths. See
  [OpenCode's instruction loading](https://github.com/anomalyco/opencode/blob/v1.19.0/packages/opencode/src/session/instruction.ts).

Split-mode launch refuses nonempty global Codex/Gemini/OpenCode instruction
files, which those CLIs cannot completely suppress. Keep the dedicated CLI
home free of global instruction files. CLI updates may change these
capabilities; reverify the documented switches before relaxing restrictions.

With `[agent] user = ""`, Iris continues to run CLIs under its own account
and existing sign-ins remain usable. The conversation layout and harness
rules still apply, but **there is no OS boundary**: arbitrary code executing
as that account can read the vault and database. Use the split for secret
protection. It does not isolate different conversations from each other at
the Unix-user level, restrict network access, or protect the CLI's own
subscription credentials against arbitrary code running as that CLI user.

Rollback: as root run
`sh deploy/setup-agent-user.sh --uninstall iris-service iris-agent /usr/local/bin/iris`,
set `[agent] user = ""`, and restart Iris. The script removes its policy,
sudoers rule and service drop-in. It retains the account, home, workspaces
and service group membership to avoid deleting sign-ins or generated files;
remove those manually only after saving anything you need. Sign in as the
service account again. The engine-admin split is independent and remains.

## Iris's computer (browser)

Iris's browser is Microsoft's open-source Playwright MCP server
(`@playwright/mcp`), which runs one headless Chromium. Iris does not
automate the browser itself: it starts that server on its own side, hands
its tools to the agent in your own conversations as the `browser` MCP
server (open pages, read them, click, type, fill forms, screenshots, tabs),
and lets you watch it live and take over from the app (Iris hub, Iris's
computer).

- **Getting a browser.** Tap "Set up the browser" in Iris's computer: Iris
  installs the official `@playwright/mcp` npm package, pinned to a release
  that passed the real browser test in CI (a weekly workflow moves the pin
  after a new release passes it), into its engines
  directory (`<engines dir>/chromium`) and runs its own
  `install-browser chromium`, which downloads Chromium from Playwright's
  official servers (needs Node.js). The build lands at one fixed path,
  `<engines dir>/chromium/app/chrome`, so the one-time root setup can
  approve it for Chrome's sandbox (below); `<engines dir>/bin/browser`
  starts the server. Iris updates both daily with the other engines it
  installed, while the browser is closed. Or install the server yourself
  (`npm install -g @playwright/mcp`, then `npx playwright install chromium`);
  Iris uses `playwright-mcp` from `PATH` when it has no copy of its own.
  `browser.command` picks another Chromium or Chrome for it.
- **What the agent may use.** Playwright MCP's page tools pass through
  unchanged. Iris refuses the ones that reach beyond the page:
  `browser_run_code_unsafe` (it runs code inside the server, on Iris's
  side), `browser_evaluate` (page scripts can read cookies and stored
  tokens), file upload and drop, network request details (headers carry
  cookies), console messages, resizing and closing the browser, writing
  results to files, and addresses that are not `http`/`https`. Tools a new
  version adds stay hidden until they are reviewed in `src/browser/tools.rs`.
- **System libraries.** A fresh server may lack the libraries Chromium
  needs; the app then shows the error. Fix it once as root with
  `npx playwright install-deps chromium` (or install your distribution's
  Chromium package).
- **Sign-ins persist.** The profile lives in `<data dir>/browser/profile`,
  so sites you signed in to stay signed in. Chrome's own password manager
  and autofill are turned off there. Chrome's session files (open tabs with
  what was typed into their forms) and Playwright's output folder (page
  snapshots, screenshots) are deleted before each start and after each
  close.
- **Memory.** The browser starts on first use and closes after
  `browser.idle_mins` (default 10) without use, so it costs nothing while
  idle. When it gets stuck, the agent can restart it (`restart_browser`:
  closed gracefully and opened again, sign-ins kept). A running browser with one tab takes a few hundred MB (the
  Node.js server plus Chromium).
- **Live view and take over.** While you watch, Iris takes screenshots
  through the server (about 1 per second for the small peek in chat, about
  3 full screen) and sends only pictures that changed. The peek in chat is
  only the picture; tap it for the full view and Take over. It stays after
  the agent's turn ends (which is often when it asks you to take over) for
  at least a minute and until you write again or take over. While the agent
  waits for a slow page, the picture waits too. Take over forwards your
  taps, scrolling, and typing to the same page; the agent's browser tools
  wait until you hand back.
- **Saved logins and cards.** The agent never sees or types passwords,
  card numbers, or codes. It calls `fill_saved_login` / `fill_saved_card`
  with the fields' element refs, and Iris types the saved values into them
  through Playwright on its own side; a card needs your OK on an approval
  card in the chat every time. If no login is saved for the site, it can
  post a card in the chat where you type one in (it goes straight to Logins
  & wallet). For codes, checks, or sites that fight automated browsers, it
  asks you to take over and sign in yourself; for X it asks right away
  (X's sign-in tends to stall for automated browsers after the username,
  and no browser setting we tried changes that). The key button in the take-over bar fills a field you tapped
  from Logins & wallet. Logins only go into fields on the site they were
  saved for, and
  everything only over HTTPS (plain HTTP only on this machine), and never
  into another site's frame inside the page. When you type into a
  password, card, or code field, the agent cannot use that page at all (in
  any tab) until it moves on to a new page, for example after the sign-in;
  it can still open another page, but not that address again until the
  browser closes. A saved password or card Iris filled closes the page only
  while it is still on it, in a field or its text (sent or cleared forms
  open it again, even when the address stays the same); meanwhile the agent can still read the
  page, with the secret hidden, to see whether the sign-in worked. The agent never gets your saved
  logins or cards. While you are in control, dialogs the page shows are
  answered for you (alerts and confirmations accepted, prompts dismissed),
  file pickers are cancelled, and a window the page opens (a sign-in
  popup) becomes the one you see.
- **Who gets it.** Only the owner's own conversations and jobs (the profile
  holds the owner's sign-ins); never shared chats.
- **Sandbox.** Iris always starts Chrome with its sandbox. Ubuntu 23.10
  and later restrict unprivileged user namespaces with AppArmor
  (`kernel.apparmor_restrict_unprivileged_userns = 1`), and Chrome's
  sandbox needs one, so it only starts for a browser an AppArmor profile
  allows. The recommended setup approves Iris's own browser once:

  1. Run the engines privilege split, `deploy/setup-engines.sh`, as root
     (see [Engines from the app](#engines-from-the-app)).
     Where the restriction is on, it writes `/etc/apparmor.d/iris-browser`
     for exactly `<engines dir>/chromium/app/chrome` and loads it with
     `apparmor_parser -r` (rerunning it is safe; without the restriction or
     AppArmor 4 it does nothing). It refuses if anything already under
     `<engines dir>/chromium` belongs to another account or is writable by
     group or others. The engines directory belongs to the
     `iris` admin user, so the user running Iris cannot put another program
     at that path.
  2. Add the printed `[engines]` settings, restart Iris, and tap "Set up the
     browser" in Iris's computer. Iris now uses that copy unless
     `browser.command` is set (it always wins; clear it to use Iris's copy).
     A browser Iris installed before this layout (or before Playwright MCP)
     is replaced on its next update (Iris's computer offers **Update the
     browser**).

  The script refuses to run when the account running Iris, or the CLI agent
  account, is the `iris` engine admin itself: that account owns the
  approved browser.

  If the sandbox still cannot start, Iris's computer explains and shows the
  exact command for this machine, with **Copy**, **Try again**, and **Run
  without the sandbox**:

  - If the browser Iris found is protected (it, its directory, and every
    folder above it are owned by root or the engines admin user and not
    writable by others; for example `/usr/bin/chromium` or Iris's own copy
    from before the profile existed), the command installs the same
    profile for that browser's real path:

    ```sh
    sudo tee /etc/apparmor.d/iris-browser >/dev/null <<'EOF'
    # Iris's browser: allow Chrome's sandbox (user namespaces) for this one program.
    abi <abi/4.0>,
    include <tunables/global>

    profile iris-browser "/opt/iris/engines/chromium/app/chrome" flags=(unconfined) {
      userns,
    }
    EOF
    sudo apparmor_parser -r /etc/apparmor.d/iris-browser
    ```
  - Otherwise (for example Playwright's cache in your home directory, or
    engines in Iris's data directory in single-user mode) it shows the
    `setup-engines.sh` command instead. A profile for a path your own user
    can write would give user namespaces to any program copied there,
    undoing the system's protection, so Iris never suggests one.

  **Run without the sandbox** remains: only if you tap it (a saved choice,
  `POST /api/browser/sandbox`) does Iris start Chromium without it
  (Playwright's `chromiumSandbox: false`). Iris
  itself never writes AppArmor profiles or runs anything as root. See
  Chromium's `docs/security/apparmor-userns-restrictions.md` for background.

  Moving from a hand-written profile (an earlier version of this guide
  suggested `/etc/apparmor.d/iris-chrome` for a Playwright cache path):
  once Iris uses its own approved copy, remove the old one with
  `sudo apparmor_parser -R /etc/apparmor.d/iris-chrome && sudo rm /etc/apparmor.d/iris-chrome`.
- **Turning it off.** `[browser] enabled = false` removes the tools.
## Connections and approvals

Connections (MCP servers) give Iris your services: Google (Gmail, Calendar,
Drive, Docs and more), GitHub, one-tap services (Notion, Todoist, Linear,
Sentry, Stripe and more), or any MCP server. They are grouped by
Productivity, Development, Money and Lifestyle. Add them in the app under
Iris, Connections, or
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

**Your choices per service.** Each service's page in Connections lists its
actions in plain words, under "Read permissions" and "Write and delete
permissions", each set to **Allow** (runs on its own), **Ask** (waits for
your OK), or **Off** (Iris refuses it, and a tool whose every action is off
is hidden from the AI). Purchases always ask. Google signs in once for
every service, so Iris enforces these choices itself: each Google service
(Gmail, Calendar, Drive...) has its own page and a switch to stop using it
entirely, and actions are told apart by what a call really does (creating
an event is not the same action as creating one that invites people, and
updating an event notifies its guests unless the call says not to). Google
starts with reads allowed and every change asking.

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
**Disconnect everything** turns every connection off at once, ends browser
access for running turns, and closes and disconnects the browser. Reopen it in Iris’s
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

- **Google**: open **Iris hub → Connections → Google**, or ask “help me
  set up Google” and tap the chat card. One setup and one sign-in connect
  Gmail, Calendar, Drive, Docs, Sheets, Slides, Forms, Tasks, Contacts and
  YouTube read-only. Iris uses your **own** Google
  OAuth client so Gmail actions pass through Iris’s approval gate. Google
  secrets and tokens stay encrypted on Iris’s side; the agent never gets them.

  The wizard shows one step per screen, with Open, Copy, Done and Back.
  Your place is saved on the server, so you can leave and resume; Iris can
  coach you from the current step without seeing your key.

  1. Create a Google Cloud project named **Iris**. Keep that project selected
     in Google’s project picker for the remaining steps.
  2. Enable all ten APIs (Gmail, Calendar, Drive, YouTube Data, Docs,
     Sheets, Slides, Forms, Tasks, and People for Contacts) together with
     the Open button; single links for each are under it.
  3. On **Branding**, fill all three required fields: **App name**: **Iris**;
     **User support email**: pick your Gmail; **Developer contact information**:
     your Gmail (at the very bottom of the page, easy to miss). In **Authorized
     domains**, enter the exact domain the wizard shows: domain only, no
     `https://`, port or path. Leave the logo, **App domain** links, home page
     and privacy policy empty; adding links there can trigger Google verification.
     Tap **Save** at the bottom. On **Audience**, choose **External** and add
     your own Google account as a test user.
  4. Create a **Web application** login key. Copy the wizard’s exact callback
     into **Authorized redirect URIs**. Iris requires `public_url` to be its
     HTTPS address first; the screen links to instructions. See also
     [using Iris from your phone](#using-iris-from-your-phone). Download the
     client JSON or copy its client ID and secret for step 6.
  5. On **Audience**, choose **Publish app** and confirm **In production**.
     Otherwise Google’s testing mode signs you out every seven days.
     If **Publish app** is greyed out, return to step 3, fill all three Branding
     fields and the **Authorized domains** entry, then **Save**.
     Your app is still for your own private use. During sign-in, the warning
     **“Google hasn’t verified this app”** is expected: choose
     **Advanced → Go to Iris**.
  6. Paste the downloaded client JSON (web or installed format), or the
     client ID and secret together, into the **one box in the app**.
     Labels, line breaks and either order work. Never paste the key in chat.

  Then tap **Sign in with Google**, open its sign-in page, approve access
  with every box checked, and return to Iris for **Connected ✓**. Reported sign-in
  errors take you back to the step to fix; if Google stops on its own page,
  use **Google showed a problem** in Iris. Done records your progress; Iris
  cannot check the settings in your Google Cloud console.

  Sign-in and Console links open in your real browser (Safari from an iOS
  home-screen app), where your saved passwords and signed-in accounts live.
  Every link also offers **Copy link**. Iris’s managed browser is never used
  for these sign-ins.

  The server needs `uv`: Google actions use the existing
  [Google Workspace MCP server](https://github.com/taylorwilsdon/google_workspace_mcp)
  (`uvx workspace-mcp`) in external OAuth mode, with a fresh token on each
  request. YouTube read-only also powers subscriptions and likes in For you.
  No client secret ships with Iris. Not included: Google Chat (its API only
  works for Business or Enterprise Workspace accounts and needs a Chat app
  configured in your project), Custom Search (closed to new customers, and
  it needs its own API key and search engine; your AI engine can already
  search the web), and Apps Script (scripts run with your account's access
  and could act without asking you first). Gmail filters are included;
  connections made before that ask you to sign in again.

  **Already connected before Docs, Sheets and the rest?** Google keeps
  working as before. Connections shows “Sign in again to add Docs, Sheets
  and more”: open Google, tap **Turn on Google services** (enabling the APIs
  that are already on is harmless), then sign in again with every box
  checked. Iris then turns on the new services. If you changed the Google
  server's arguments under Advanced settings, Iris leaves them as they are;
  add `docs sheets slides forms tasks contacts` after `--tools` yourself.
- **One tap** (Notion, Todoist, Airtable, monday.com, Miro, Canva, Cal.com,
  Granola, Fireflies, Zapier, Linear, Sentry, Cloudflare, Supabase,
  Netlify, Neon, Hugging Face, Readwise, Stripe, Mercury):
  Iris connects to the service's own hosted MCP server, registers itself
  with the service, and you approve access on the service's page. Nothing to
  create or paste. These start with “Reads freely · asks before any change”;
  Stripe and Mercury ask before every action. Disconnect signs Iris out and
  keeps your choices, so connecting again is one sign-in.
  If Iris is not on HTTPS, you paste the address your browser ends on back
  into the app, as with any sign-in.
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
- **For you** (`[feed]`, on by default): once a day Iris reads your
  sources (RSS/Atom feeds, YouTube channels, subreddits, Hacker News,
  Bluesky and Mastodon), ranks what is new with plain rules (your likes,
  topics, freshness, variety), and publishes about 10 posts with the
  pages' own photos. Only two small model calls a day: one on the
  `background` slot turns your notes, likes, and recent chats into topics
  and source suggestions, and one batched call on the `feed` slot writes
  short summaries for the posts that need them (with the CLI's tools
  turned off, since feed text comes from strangers). Add or remove sources in
  the app (Settings, For you). The size grows toward 30 if you keep
  reaching the end and shrinks if you rarely look; after two weeks unused
  the feed pauses itself and tells you once in chat. It only notifies you
  for big personal things: a real price drop on something you saved, or an
  event soon near you. "Talk about it" on a post opens a side chat seeded
  with it.
  - Fetching is polite: Iris names itself in its User-Agent, follows
    robots.txt for pages and photos, waits between requests to a site,
    honors rate limits, caps sizes and times, and never fetches private or
    tailnet addresses. Photos and icons are cached under
    `<data dir>/feed-images` so your phone does not contact those sites.
  - Reddit lets unauthenticated readers make about one request a minute,
    so Iris reads at most 4 subreddits a run, rotating through them.
  - YouTube subscriptions and likes are used when the Google connector has
    stored a token; otherwise add channels as sources.
- **Ideas**: the agent may propose things it could do for you; tapping one
  starts a chat that does it.

Dream (`[dream]`) tidies memory after at least four new messages you wrote
in your own private chats, using the Background engine with tools disabled.
It runs within the 3am local hour by default, and skips idle days. Invalid updates leave notes and progress alone;
failed attempts retry the following night. Its morning-card data includes
per-change undo; dedicated app rendering follows separately. Owner settings
`memory_learning_paused` and `memory_morning_note` on `PUT /api/settings`
control automatic learning and morning cards. Pausing learning also blocks
agent memory tools from writing; you can still read, edit and undo notes.
The minute scheduler archives expired notes and low-score incidental notes
without a model call, even while learning is paused. Score decay requires at
least 90 days without use/edit and protects pinned, profile/preference and
importance 4–5 notes. Explicit validity expiry applies to all notes. Archived
notes remain in the database with undo history.

The owner can download all current and archived notes from
`GET /api/memory/export` as `iris-memory.md`. A confirmed
`POST /api/memory/forget-everything` with `{ "confirm": true }` clears learned
notes, undo history, generated note content and historical memory card/chip
payloads. It also discards owner CLI sessions and blocks pre-reset turns from
saving notes. Dream skips old evidence; new assertions remain learnable.
Chats, persona files, `memory-backup/` import originals and other backups are
retained; remove backup copies separately if you need them erased.
The Memory settings UI follows in the UI PR.
`POST /api/jobs/dream/run` skips the time gate, keeping all privacy, activity,
enabled and pause checks.

The heartbeat is off by default. Both jobs respect active hours and can be
triggered manually (`POST /api/jobs/{heartbeat|feed}/run`, or "Find
something now" in the app).

**Quiet hours** (Settings, Notifications) hold notifications instead of
dropping them; when they end you get one summary notification.

## Security model

- Binds to loopback by default. To use it from your phone, put a private
  HTTPS front on it such as `tailscale serve`
  ([phone setup](#using-iris-from-your-phone)). Do not expose it to the
  internet.
- Every route except the static console page, `/api/health`, and
  `/api/pair` requires a device token. Devices pair with a one-time code from
  `iris pair`; only SHA-256 hashes of codes and tokens are stored. Five bad
  codes lock that source IP for 15 minutes; `iris pair` clears the lockout.
  Other source IPs and outstanding unused codes remain usable.
- Saved logins, wallet cards, connector secrets and tokens, and engine API
  keys are encrypted with XChaCha20-Poly1305 using a key in `vault.key`
  (0600). The API never returns them.
- Connections run on Iris's side behind the approval gate; the CLI gets
  authenticated per-turn proxies. In split mode Linux permissions prevent
  the agent account from reading Iris's vault, database and private files.
  Single-user mode has no such OS boundary.
- Data directory files are created with owner-only permissions. Memory
  reads and writes go through the owner-only Iris MCP bridge.
- CLI turns use a separate working directory for each conversation, a
  timeout and process-group management. Project documents are disabled for
  every turn. Claude and Codex have private-path deny rules; Gemini runs
  restricted with MCP disabled; OpenCode's file-read and shell tools are
  withheld. The precise CLI limitations are documented in the isolation
  setup above.
- Engines installed from the app come only from official sources and are
  verified (checksum or npm integrity); see the privilege split above.
- Iris's browser (Playwright MCP) runs on Iris's side and drives Chromium
  over a private pipe (no DevTools port is opened). The agent reaches it
  only through `iris mcp --browser`, which forwards over a Unix socket in
  the private run directory, accepted only for a running turn, and only
  for reviewed tools (no code execution in the server, no page scripts, no
  file access, no request headers). Saved secrets go from the vault straight
  into the page, typed by Iris (the agent names the fields and never sees
  the values; cards need your OK each time); a page
  where you typed or filled private details is closed to the agent until
  it moves on. Only `http`/`https` pages can be opened. When a turn ends or
  you take over, its pending browser reads stop, and an action already
  sent finishes first (opening a page: at most 8 seconds; other actions:
  up to the call's 90-second limit).
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
`HEARTBEAT.md`) into the data directory, and imports `MEMORY.md` and
`memory/**/*.md` as memory notes (one note per bullet or paragraph; notes
Iris already has are not added again). OpenClaw's `AGENTS.md` is not
imported. The source is never modified. Existing persona files are kept
unless you pass `--force`. Symlinks, hidden files, and non-text files are
skipped.

When you upgrade from a version that kept memory as Markdown files, Iris
imports the files in `memory/` the same way on its first start, copies the
originals to `memory-backup/`, and from then on writes `memory/` itself.

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
- **CLI context controls**: project instructions are disabled for every
  turn, and all conversations have separate directories. Gemini restricted
  mode disables MCP; OpenCode's built-in file reads are withheld because
  they can inject nested instruction files. See the isolation setup above.
- **Gemini CLI and OpenCode sign-in from the app**: both need one step on
  the server (see above).

## License

Dual-licensed under either of [Apache License 2.0](../LICENSE-APACHE) or
[MIT](../LICENSE-MIT), at your option.


### Memory evaluation

`cargo test synthetic_memory_eval_five_categories` runs synthetic fixtures in
`tests/fixtures/memory/eval.json` through a real subprocess running a fake Claude
CLI, the normal Dream pipeline, SQLite, forgetting and recall. Cases cover
extraction, evidence spanning sessions, temporal expiry, corrections and
abstention. Fixed CLI proposals test host behavior, not model intelligence;
this is a reproducible regression suite, not a LongMemEval benchmark score.

For an optional real model smoke test, sign in using the official Claude CLI,
then run `scripts/e2e/memory/real-cli.sh` (or set `IRIS_MEMORY_EVAL_CLI` to its
binary path). It makes five small subscription-backed CLI turns with tools
and project context disabled and temporary app data only. No live Iris data
or CLI credentials are read or modified by Iris. The ignored test checks
relevant recall, absence of outdated/invented facts, provenance and progress;
results may vary with the CLI/model. It is not run by CI.
