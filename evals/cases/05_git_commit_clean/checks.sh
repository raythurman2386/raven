#!/usr/bin/env bash
set -euo pipefail
cargo test --quiet
grep -q 'fn square' src/lib.rs
# working tree clean. Ignore harness/build artifacts the grader or agent
# may create after the commit (`.raven/`, `target/`, `Cargo.lock`).
dirty=$(git status --porcelain \
  | grep -vE '\.raven|/target|Cargo.lock' \
  | grep -v '^$' || true)
if [[ -n "$dirty" ]]; then
  echo "FAIL: dirty tree:" >&2
  echo "$dirty" >&2
  exit 1
fi
# at least 2 commits (initial + agent)
test "$(git rev-list --count HEAD)" -ge 2
