#!/usr/bin/env bash
# End-to-end check of the connector gateway with a real, signed-in engine:
#
#   scripts/e2e/connectors.sh [model]      (default: claude:haiku)
#
# Starts a throwaway Iris (temp data dir, random localhost port) with the
# fake messenger connector (scripts/e2e/fake_messenger_mcp.py, which never
# sends anything), pins the chat slot to <model>, and drives the app with
# Playwright's Chromium at phone size: approve, deny, trusted recipient,
# tripwire, approval timeout, Pause Iris, Disconnect everything. Costs a
# few small requests on that engine's subscription. Needs Node.js, Python
# 3, and Playwright (installed into a temp folder unless PLAYWRIGHT_DIR
# points at one). Screenshots go to $SHOTS (default: a temp folder).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
MODEL=${1:-claude:haiku}
WORK=$(mktemp -d)
SERVER=
trap 'kill $SERVER 2>/dev/null || true; [ -n "${KEEP:-}" ] || rm -rf "$WORK"' EXIT

cargo build --quiet --manifest-path "$ROOT/Cargo.toml"
IRIS="$ROOT/target/debug/iris"
CFG=(--config "$WORK/config.toml" --data-dir "$WORK/data")
"$IRIS" init "${CFG[@]}" >/dev/null
PORT=$((20000 + RANDOM % 20000))
sed -i -e "s/^bind = .*/bind = \"127.0.0.1:$PORT\"/" \
  -e 's/^approval_timeout_secs = .*/approval_timeout_secs = 30/' \
  -e 's/^max_sends_per_hour = .*/max_sends_per_hour = 3/' "$WORK/config.toml"
"$IRIS" serve "${CFG[@]}" >"$WORK/serve.log" 2>&1 &
SERVER=$!
for _ in $(seq 50); do curl -sf "http://127.0.0.1:$PORT/api/health" >/dev/null && break; sleep 0.2; done

pair() { "$IRIS" pair "${CFG[@]}" | sed -n 's/^Pairing code for .*: //p'; }
TOKEN=$(curl -sf "http://127.0.0.1:$PORT/api/pair" -H 'content-type: application/json' \
  -d "{\"code\":\"$(pair)\",\"device_name\":\"e2e\"}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
api() { curl -sf -X "$1" "http://127.0.0.1:$PORT/api$2" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' ${3:+-d "$3"}; }
LOG="$WORK/messenger.jsonl"
api POST /connectors "{\"name\":\"messenger\",\"description\":\"Test messenger\",\"transport\":\"stdio\",
  \"command\":\"python3\",\"args\":[\"$ROOT/scripts/e2e/fake_messenger_mcp.py\"],
  \"env\":{\"FAKE_MESSENGER_TOKEN\":\"fake-secret\",\"FAKE_MESSENGER_LOG\":\"$LOG\"}}" >/dev/null
api POST /harnesses/refresh >/dev/null
api PUT /models/slots/chat "{\"mode\":\"pinned\",\"model\":\"$MODEL\",\"effort\":\"low\"}" >/dev/null

PW=${PLAYWRIGHT_DIR:-}
if [ -z "$PW" ]; then
  PW="$WORK/pw"
  mkdir -p "$PW" && (cd "$PW" && npm init -y >/dev/null && npm i --silent playwright >/dev/null)
fi
SHOTS=${SHOTS:-$WORK/shots}
NODE_PATH="$PW/node_modules" node "$ROOT/scripts/e2e/approvals.mjs" \
  "http://127.0.0.1:$PORT" "$(pair)" "$LOG" "$SHOTS"
echo "screenshots: $SHOTS"
