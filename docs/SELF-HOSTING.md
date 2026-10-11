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

> Status: early. The backend and API are usable; the real phone app is being
> designed separately. A bare test console is built in so you can try it.

## Contents

- [How it works](#how-it-works)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Using Iris from your phone](#using-iris-from-your-phone)
- [Configuration](#configuration)
- [Engines and models](#engines-and-models)
- [Engines from the app](#engines-from-the-app)
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
                                                     └─▶ iris mcp (stdio) ─▶ goals, ideas, feed,
                                                                             library, cards
```

- **Persona**: `IDENTITY.md`, `SOUL.md`, `USER.md`, `TOOLS.md`,
  `PROACTIVE_PREFERENCES.md`, `HEARTBEAT.md`, and `memory/MEMORY.md` live in
  your data directory and are editable from the app. They are assembled into
  a system prompt for every turn and injected through each CLI's own
  mechanism (`--append-system-prompt-file` for Claude Code,
  `developer_instructions` for Codex, `GEMINI_SYSTEM_MD` for Gemini CLI, an
  `instructions` file for OpenCode), before your message.
- **Sessions**: each conversation keeps a CLI session per harness and resumes
  it (`--resume`, `codex exec resume`, `-s`). If a conversation switches harness,
  Iris replays the messages that harness has not seen.
- **App bridge**: every turn gets a small MCP server, `iris mcp`, so the agent
  can create goals, ideas, feed posts, library items, and cards (for example
  a "Connect Google" card). It is a CRUD bridge into Iris's database, not a
  tool layer.
- **Connectors**: MCP servers you add in the app are passed to the CLI on
  every turn, through each CLI's own MCP configuration. Their secrets are
  encrypted at rest. If a connector's sign-in expires, Iris posts a
  "Reconnect" card.
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
   it, then restart Iris.
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
| `[heartbeat]` | disabled | Proactive check-in with active hours. |
| `[feed]` | enabled | The For you pipeline: checks hourly between 06:00 and 22:00, publishes one batch a day. |
| `[push]` | enabled | Web Push (VAPID); set `vapid_subject` to a `mailto:` URL. |

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
├── run/             # per-turn private files (prompt, MCP config)
├── engines/         # CLIs installed from the app (single-user mode)
├── vault.key        # 32-byte key for encrypted secrets, 0600
└── vapid.json       # Web Push key pair, 0600
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
- Signing in to **Gemini CLI** (it can only sign in from an interactive
  terminal: run `gemini` once) and adding providers to **OpenCode**
  (`opencode auth login`, or configuring Ollama / LM Studio). Iris shows
  these steps in the card when needed.
- The optional privilege split (`deploy/setup-engines.sh`, as root).

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
  short summaries for the posts that need them. Add or remove sources in
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
  codes lock pairing for 15 minutes and burn outstanding codes.
- Saved logins, wallet cards, and connector secrets are encrypted with
  XChaCha20-Poly1305 using a key in `vault.key` (0600). The API never returns
  them.
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
- **Using saved credentials**: logins and cards are stored encrypted and
  managed through the API, but nothing injects them into the agent's
  browsing yet. Planned design: the agent requests a credential by id
  through a tool and Iris fills it into a browser session without the
  model ever seeing the secret.
- **Live view of the agent's browser**, group chat UI.
- **Gemini CLI and OpenCode sign-in from the app**: both need one step on
  the server (see above).

## License

Dual-licensed under either of [Apache License 2.0](../LICENSE-APACHE) or
[MIT](../LICENSE-MIT), at your option.
