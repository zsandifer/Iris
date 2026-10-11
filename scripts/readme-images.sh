#!/usr/bin/env bash
# Regenerate the README screenshots in docs/images from fake demo data.
#
#   scripts/readme-images.sh
#
# Builds iris, seeds a temporary data directory with `iris demo-seed`, serves
# it on a random localhost port, and drives the app with Playwright's
# Chromium at iPhone size (light and dark). Needs Node.js and Python 3 with
# Pillow (to shrink the PNGs). Playwright is installed into a temp folder
# unless PLAYWRIGHT_DIR points at a folder that has it; set CHROME to use a
# specific Chromium binary. The live status phrase and the sign-in card in
# the shots are injected into the page's WebSocket; everything else is real
# app output.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
trap 'kill $SERVER 2>/dev/null || true; rm -rf "$WORK"' EXIT
SERVER=

cargo build --quiet --manifest-path "$ROOT/Cargo.toml"
IRIS="$ROOT/target/debug/iris"
CFG=(--config "$WORK/config.toml" --data-dir "$WORK/data")
"$IRIS" init "${CFG[@]}" >/dev/null
"$IRIS" demo-seed "${CFG[@]}" --assets "$ROOT/scripts/demo/images" >/dev/null
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
mkdir -p "$ROOT/docs/images"
NODE_PATH="$PW/node_modules" node "$ROOT/scripts/screenshots/shoot.mjs" \
  "http://127.0.0.1:$PORT" "$CODE" "$WORK/shots" "$ROOT/docs/images"
python3 "$ROOT/scripts/screenshots/shrink.py" "$ROOT"/docs/images/*.png
ls -l "$ROOT/docs/images"
