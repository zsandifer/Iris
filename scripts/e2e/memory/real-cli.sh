#!/usr/bin/env bash
# Optional subscription-backed memory check; all app data lives in temp dirs.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
export IRIS_MEMORY_EVAL_CLI="${IRIS_MEMORY_EVAL_CLI:-claude}"
command -v "$IRIS_MEMORY_EVAL_CLI" >/dev/null
cargo test real_cli_memory_eval -- --ignored --nocapture
