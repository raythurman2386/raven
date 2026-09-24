#!/usr/bin/env bash
set -euo pipefail
cargo test --quiet
# Correct implementations, no bugs.
grep -q 'sum / xs.len()' src/stats.rs
grep -q '(v\[mid - 1\] + v\[mid\]) / 2.0' src/stats.rs
grep -q 'w.to_lowercase()' src/strings.rs
grep -q 'principal \* r / denom' src/finance.rs
# Goal/todo state is session-scoped under .raven/sessions/<id>/state/
# (workspace-global .raven/state/ is a legacy read path only).
shopt -s nullglob
goals=(.raven/sessions/*/state/goal.json)
todos=(.raven/sessions/*/state/todos.json)
test "${#goals[@]}" -ge 1
test "${#todos[@]}" -ge 1
# The goal and todos must reflect the real task.
grep -q -iE 'test|fix' "${goals[0]}"
grep -q 'mean' "${todos[0]}"
