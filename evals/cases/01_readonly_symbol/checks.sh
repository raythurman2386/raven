#!/usr/bin/env bash
# Read-only: answer should mention add and 5.
set -euo pipefail
out=$(cat "${EVAL_STDOUT:-/dev/null}")
echo "$out" | grep -qi 'add'
echo "$out" | grep -qE '\b5\b'
