#!/usr/bin/env bash
# A separate TEST Iris for the browser test loop (see README.md here).
#
#   instance.sh up        build-free start (copies $IRIS_BIN), pair, pin model
#   instance.sh down      stop it
#   instance.sh restart   down + up (picks up a rebuilt binary)
#   instance.sh status    health, browser, engines, model slots
#   instance.sh pair      print a pairing code for a phone or laptop
#   instance.sh logs      follow the server log
#
# Environment (all optional):
#   IRIS_TEST_DIR   state dir (default ~/.local/share/iris-test): config.toml,
#                   data/, the harness's device token, serve.log, runs/
#   IRIS_TEST_BIND  listen address (default 127.0.0.1:7999). Keep it on
#                   loopback or a private (tailnet) address, never public.
#   IRIS_TEST_PUBLIC_URL  URL `iris pair` prints (for example a tailnet name)
#   IRIS_TEST_MODEL chat model to pin (default claude:haiku)
#   IRIS_TEST_ENGINES  engines dir to borrow; must not be writable by this
#                   account (default /opt/iris/engines when it exists, else
#                   the TEST instance's own <dir>/data/engines)
#   IRIS_BIN        iris binary to run (default target/release/iris, else
#                   target/debug/iris of this checkout)
#
# Safety: the test instance never changes a borrowed engines directory:
# engines.auto_update = false, and `up` refuses to start if the config says
# otherwise or if this account could write that directory. It runs in single-user mode as the
# current account, with the heartbeat, feed, and push off.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
DIR=${IRIS_TEST_DIR:-$HOME/.local/share/iris-test}
BIND=${IRIS_TEST_BIND:-127.0.0.1:7999}
MODEL=${IRIS_TEST_MODEL:-claude:haiku}
# Engines (the CLIs and the browser) are borrowed from an existing Iris's
# privilege-split directory, which this account cannot write. Empty: the
# TEST instance's own (<dir>/data/engines), then PATH.
ENGINES=${IRIS_TEST_ENGINES:-}
[ -n "$ENGINES" ] || { [ -d /opt/iris/engines ] && ENGINES=/opt/iris/engines; } || true
CFG=(--config "$DIR/config.toml" --data-dir "$DIR/data")
BIN="$DIR/bin/iris"
BASE="http://$BIND"

die() { echo "instance.sh: $*" >&2; exit 1; }

pick_bin() {
  if [ -n "${IRIS_BIN:-}" ]; then echo "$IRIS_BIN"
  elif [ -x "$ROOT/target/release/iris" ]; then echo "$ROOT/target/release/iris"
  elif [ -x "$ROOT/target/debug/iris" ]; then echo "$ROOT/target/debug/iris"
  else die "no iris binary: run cargo build --release (or set IRIS_BIN)"; fi
}

running() { [ -f "$DIR/serve.pid" ] && kill -0 "$(cat "$DIR/serve.pid")" 2>/dev/null; }

# Section-aware edits of the TOML Iris wrote (keys repeat across sections).
configure() {
  python3 - "$DIR/config.toml" "$BIND" "${IRIS_TEST_PUBLIC_URL:-}" "$ENGINES" <<'PY'
import json, sys
path, bind, public_url, engines = sys.argv[1:]
want = {
    "": {"bind": json.dumps(bind), "public_url": json.dumps(public_url),
         "max_concurrent_turns": "1", "turn_timeout_secs": "600"},
    "heartbeat": {"enabled": "false"},
    "feed": {"enabled": "false"},
    "push": {"enabled": "false", "notify_replies_when_offline": "false"},
    "engines": {"helper": "[]", "auto_update": "false"},
    "agent": {"user": '""'},
    "harness.gemini": {"enabled": "false"},
    "harness.opencode": {"enabled": "false"},
}
if engines:
    want["engines"]["dir"] = json.dumps(engines)
section, out, seen = "", [], set()
for line in open(path).read().splitlines():
    s = line.strip()
    if s.startswith("[") and s.endswith("]"):
        section = s[1:-1]
    elif "=" in s and not s.startswith("#"):
        key = s.split("=", 1)[0].strip()
        if key in want.get(section, {}):
            line = f"{key} = {want[section][key]}"
            seen.add((section, key))
    out.append(line)
missing = [f"{s}.{k}" for s, keys in want.items() for k in keys if (s, k) not in seen]
if missing:
    sys.exit(f"config keys not found: {missing}")
open(path, "w").write("\n".join(out) + "\n")
PY
}

# Refuse to run anything that could install or update an engine.
check_safe() {
  python3 - "$DIR/config.toml" <<'PY'
import sys, tomllib
c = tomllib.load(open(sys.argv[1], "rb"))
e = c.get("engines", {})
bad = []
if e.get("auto_update", True): bad.append("engines.auto_update is not false")
if e.get("helper"): bad.append("engines.helper is set")
if c.get("agent", {}).get("user"): bad.append("agent.user is set (split mode)")
host = c.get("bind", "").rsplit(":", 1)[0]
if host in ("0.0.0.0", "::", "[::]", ""): bad.append(f"bind {c.get('bind')!r} is public")
if bad: sys.exit("unsafe test config: " + "; ".join(bad))
PY
}

api() { curl -sf -X "$1" "$BASE/api$2" -H "authorization: Bearer $(cat "$DIR/token")" \
  -H 'content-type: application/json' ${3:+-d "$3"}; }

pair_code() { "$BIN" pair "${CFG[@]}" | sed -n 's/^Pairing code for .*: //p'; }

up() {
  running && { echo "already running (pid $(cat "$DIR/serve.pid")) at $BASE"; return; }
  mkdir -p "$DIR/bin" "$DIR/runs"
  chmod 700 "$DIR"
  install -m 755 "$(pick_bin)" "$BIN"
  [ -f "$DIR/config.toml" ] || "$BIN" init "${CFG[@]}" >/dev/null
  configure
  check_safe
  # An empty helper means the TEST app could install, update or remove
  # engines in-process: fail closed unless this account cannot write there.
  if [ -n "$ENGINES" ] && [ -w "$ENGINES" ]; then
    die "$ENGINES is writable by $(id -un), so the TEST app could change it. Use a directory this account cannot write (an Iris privilege-split engines dir), or leave IRIS_TEST_ENGINES empty for the TEST instance's own."
  fi
  if [ -z "$ENGINES" ] && ! command -v playwright-mcp >/dev/null; then
    echo "note: no browser yet; install it from the TEST app (Iris's computer), which puts it in $DIR/data/engines" >&2
  fi
  # Keys that would bill API usage instead of the signed-in subscriptions.
  env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u CODEX_API_KEY \
    setsid nohup "$BIN" serve "${CFG[@]}" >>"$DIR/serve.log" 2>&1 </dev/null &
  echo $! >"$DIR/serve.pid"
  for _ in $(seq 100); do curl -sf "$BASE/api/health" >/dev/null && break; sleep 0.2; done
  curl -sf "$BASE/api/health" >/dev/null || { tail -20 "$DIR/serve.log"; die "server did not start"; }
  echo "$BASE" >"$DIR/url"
  if ! { [ -f "$DIR/token" ] && api GET /me >/dev/null; }; then
    (umask 077; curl -sf "$BASE/api/pair" -H 'content-type: application/json' \
      -d "{\"code\":\"$(pair_code)\",\"device_name\":\"browser-loop\"}" |
      python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])' >"$DIR/token")
  fi
  api POST /harnesses/refresh >/dev/null || true
  for slot in chat chat_strong; do
    api PUT "/models/slots/$slot" "{\"mode\":\"pinned\",\"model\":\"$MODEL\"}" >/dev/null ||
      die "could not pin $slot to $MODEL (is that engine installed and signed in?)"
  done
  echo "test Iris running at $BASE (pid $(cat "$DIR/serve.pid")), chat pinned to $MODEL"
}

down() {
  if running; then
    kill "$(cat "$DIR/serve.pid")"
    for _ in $(seq 50); do running || break; sleep 0.2; done
    running && kill -9 "$(cat "$DIR/serve.pid")" || true
  fi
  rm -f "$DIR/serve.pid"
}

status() {
  running || die "not running"
  echo "url: $BASE  pid: $(cat "$DIR/serve.pid")"
  { api GET /engines; echo; api GET /browser; echo; api GET /models/slots; echo; } | python3 -c '
import json, sys
eng, br, slots = (json.loads(l) for l in sys.stdin.read().split("\n") if l.strip())
print("engines dir: %s  auto_update: %s  helper: %s" % (eng["dir"], eng["auto_update"], eng["privileged_helper"]))
for e in eng["engines"]:
    print("  %s: installed=%s auth=%s version=%s" % (e["name"], e["installed"], e.get("auth"), e.get("version")))
b = br["browser"]
print("browser:", {k: b.get(k) for k in ("available", "running", "sandbox", "controller", "url", "error")})
for s in slots["slots"]:
    print("  slot %s: %s -> %s" % (s["slot"], s["selection"]["mode"], s.get("effective")))
'
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  restart) down; up ;;
  status) status ;;
  pair) running || die "not running"; "$BIN" pair "${CFG[@]}" ;;
  logs) tail -f "$DIR/serve.log" ;;
  *) sed -n '2,25p' "$0"; exit 2 ;;
esac
