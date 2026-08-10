#!/usr/bin/env bash
set -euo pipefail
out=$(cat "${EVAL_STDOUT:-/dev/null}")
echo "$out" | grep -q 'cargo test --workspace'
