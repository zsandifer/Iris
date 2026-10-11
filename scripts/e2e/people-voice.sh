#!/usr/bin/env bash
# End-to-end check of People, Together chats, voice replies, and voice
# input in the real app (headless Chromium) against a real `iris serve`.
#
#   scripts/e2e/people-voice.sh [out-dir]
#
# Needs: the `claude` and/or `codex` CLIs installed and signed in (a few
# tiny requests), Node.js, network access (the voice models, ~330 MB, are
# downloaded through the app's own installer), `tar` and `bzip2`. Uses a
# temporary data directory and a random localhost port; never touches your
# real Iris data. Playwright is installed into the work folder unless
# PLAYWRIGHT_DIR points at a folder that has it; set CHROME to use a
# specific Chromium binary. Screenshots land in out-dir (default: a
# folder inside the work dir, printed at the end).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
WORK=$(mktemp -d)
OUT=${1:-$WORK/shots}
SERVER=
trap 'kill $SERVER 2>/dev/null || true' EXIT

cargo build --quiet --manifest-path "$ROOT/Cargo.toml"
IRIS="$ROOT/target/debug/iris"
CFG=(--config "$WORK/config.toml" --data-dir "$WORK/data")
"$IRIS" init "${CFG[@]}" >/dev/null
PORT=$((20000 + RANDOM % 20000))
"$IRIS" serve "${CFG[@]}" --bind "127.0.0.1:$PORT" >"$WORK/serve.log" 2>&1 &
SERVER=$!
for _ in $(seq 50); do curl -sf "http://127.0.0.1:$PORT/api/health" >/dev/null && break; sleep 0.2; done
CODE=$("$IRIS" pair "${CFG[@]}" | sed -n 's/^Pairing code for .*: //p')

PW=${PLAYWRIGHT_DIR:-}
if [ -z "$PW" ]; then
  PW="$WORK/pw"
  mkdir -p "$PW" && (cd "$PW" && npm init -y >/dev/null && npm i --silent playwright >/dev/null)
fi
status=0
SERVE_LOG="$WORK/serve.log" AVATAR_IMAGE="$ROOT/scripts/demo/images/ai.jpg" NODE_PATH="$PW/node_modules" \
  node "$ROOT/scripts/e2e/people-voice.mjs" "http://127.0.0.1:$PORT" "$CODE" "$WORK/data" "$OUT" || status=$?
echo "Screenshots: $OUT"
echo "Server log:  $WORK/serve.log"
exit $status
