#!/usr/bin/env bash
set -euo pipefail
cargo test --quiet
# Correct implementations, no bugs.
grep -q 'sum / xs.len()' src/stats.rs
grep -q '(v\[mid - 1\] + v\[mid\]) / 2.0' src/stats.rs
grep -q 'w.to_lowercase()' src/strings.rs
grep -q 'principal \* r / denom' src/finance.rs
# The agent should have set a goal and tracked todos (long-horizon discipline).
test -f .raven/state/goal.json
test -f .raven/state/todos.json
# The goal and todos must reflect the real task.
grep -q -iE 'test|fix' .raven/state/goal.json
grep -q 'mean' .raven/state/todos.json
