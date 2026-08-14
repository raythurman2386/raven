#!/usr/bin/env bash
set -euo pipefail
cargo test --quiet
grep -q 'clamp(99, 0, 10), 10' src/lib.rs

# The harness contract: after an edit, tests must actually have run.
# EVAL_STDOUT / EVAL_STDERR are *paths* (see evals/run.py), not contents.
blob=""
if [[ -n "${EVAL_STDOUT:-}" && -f "$EVAL_STDOUT" ]]; then
  blob+=$(cat "$EVAL_STDOUT")
fi
if [[ -n "${EVAL_STDERR:-}" && -f "$EVAL_STDERR" ]]; then
  blob+=$(cat "$EVAL_STDERR")
fi
if ! printf '%s' "$blob" | grep -Eq '→[[:space:]]+(run_tests|run_shell|run_lint)\(|\[verify required|--- run_tests \(|cargo (test|clippy)'; then
  echo "FAIL: no run_tests / verification shell / verify-gate marker in raven output" >&2
  exit 1
fi
exit 0
