# Browser test loop

An end-to-end test suite for Iris's browser (Iris's computer). It starts a
separate TEST Iris, gives the agent real browsing tasks over the REST API,
and has a cheap model judge each result. It is meant to be run again and
again while fixing browser bugs, so nobody has to test by hand.

Costs: each scenario is one to three real turns on the TEST instance's chat
model (default `claude:haiku`) plus one small judge call per turn (default
Codex `gpt-6-luna`).

## Quick start

```sh
cargo build --release
scripts/e2e/browser-loop/instance.sh up        # TEST Iris on 127.0.0.1:7999
node scripts/e2e/browser-loop/run.mjs          # public + practice suites
node scripts/e2e/browser-loop/run.mjs --only expand-tables-due,login-the-internet
node scripts/e2e/browser-loop/run.mjs --rerun ~/.local/share/iris-test/runs/<ts>/report.json --repeat 3
scripts/e2e/browser-loop/instance.sh down
```

Needs Node.js 22 or newer (built-in `fetch` and `WebSocket`), Python 3.11+,
an engine signed in on this machine for the chat model, and the judge's CLI
(`codex` or `claude`) signed in.

## The TEST instance (`instance.sh`)

- Lives in `~/.local/share/iris-test` (`IRIS_TEST_DIR`): its own config,
  data, browser profile, vault, and the harness's device token (mode 600).
  It never touches the main Iris's data.
- Listens on `127.0.0.1:7999` (`IRIS_TEST_BIND`). Keep it on loopback or a
  private tailnet address.
- Borrows an engines directory (`IRIS_TEST_ENGINES`, default
  `/opt/iris/engines` from the privilege split) with `engines.auto_update =
  false`. `up` refuses to start if the config would let it update engines
  or if this account can write that directory (the TEST app could then
  change it). Without one, the TEST instance uses its own
  `data/engines` (install the browser there from its app) and `PATH`.
- Single-user mode (no agent account), heartbeat, feed, and push off, one
  turn at a time.
- Pins the chat slots to `IRIS_TEST_MODEL` (default `claude:haiku`).
- `up` copies the binary into the test dir, so rebuilding does not disturb
  a running suite; `restart` picks up a new build.

To use the TEST app yourself (for example to watch a run), run
`instance.sh pair` and open the printed URL. To reach it from a phone, bind
it to a tailnet address or put it behind `tailscale serve` (tailnet only).

## Scenarios (`scenarios.json`)

```json
{
  "defaults": {"safety_note": "..."},
  "credentials": [{"label": "...", "site": "https://...", "username": "...", "password": "..."}],
  "scenarios": [{
    "id": "kebab-id", "suite": "public | practice | real", "site": "host",
    "kinds": ["table", "paginate", "login", "..."],
    "task": "What the user types (always asks to use the browser).",
    "success": "What a correct reply contains, for the judge.",
    "checks": {"reply_all": [], "reply_any": [], "reply_none": [], "tools_all": [], "tools_any": [], "tools_none": []},
    "setup_urls": ["optional pages opened (as the owner) before the task"],
    "requires_login": "optional host: skipped unless the TEST vault has a login for it",
    "needs_picture": "optional true: the answer is a live value only the page's picture can confirm; no picture, no pass"
  }]
}
```

- `public`: no sign-in. `practice`: sites built for automation practice,
  signed in with their publicly posted test credentials (in `credentials`;
  the harness saves them into the TEST vault through the credentials API,
  the agent fills them with `fill_saved_login`, and they never appear in
  chat). `real`: skip stubs for sites that need a real account; they run
  only when a login for that site was saved in the TEST app by hand.
- `checks` are case-insensitive substring checks on the final reply and on
  the tool names the turn used. The browser must have been used.
- A scenario passes when the checks pass and the judge says pass.

## Judge

The judge (`--judge codex:gpt-6-luna`, `claude:haiku`, or `none`) gets the
task, the success criteria, every message, the tools each turn called, the
browser's final page, check results, warning lines from Iris's log, the
tool errors and the last page snapshot the agent read (with a Claude chat
model), and the live view's picture at the end of each turn (pictures and
snapshots not for the `real` suite, whose pages stay on this machine), so a
reply that contradicts the page fails. It
answers `pass`, `fail`, or `nudge` (a short follow-up a user would send when
the agent stopped early; at most `--max-nudges`, default 1), and for
failures a root cause: `site_blocking`, `iris_bug`, `model_behavior`,
`task_or_check`, or `unknown`. It runs without tools (Codex with the same
switches Iris uses for tool-less calls, no user config or project
instructions) in an empty temporary directory.

A scenario passes only when its last reply completed normally, the
required tools succeeded, the checks pass, and the judge says pass.
`--rejudge <run>/run1/<id>/result.json` grades a saved scenario again
without new turns (for tuning the judge or a scenario), and `--self-test`
checks the pass/fail rules on synthetic turns.

## Safety

- Any approval card (a payment, a send) is denied at once and the scenario
  stops as `stopped`. No payment card is ever saved in the TEST vault.
- Every task carries the `safety_note` (no buying, posting, sending, or
  account changes); tasks only fill forms on practice sandboxes.
- Sites are rate limited (`--host-gap`, default 15 s between scenarios on
  one site; scenarios of one site are spread apart).
- At most `--max-turns` messages per scenario; a reply that takes longer
  than `--timeout` is cancelled.

## Output

`<test dir>/runs/<timestamp>/`: `report.md` (table per run), `report.json`
(everything), and per scenario `result.json` (the transcript, tools,
judge verdicts), `iris-t<n>.log` (the server log during that turn),
`tools-t<n>.log` (with a Claude chat model: every tool call and what it
answered, from Claude Code's own session file; tool errors also go to the
judge), and
`frames/` (live-view pictures every few seconds and at the end). One line per
run is appended to `<test dir>/runs/index.tsv`.
