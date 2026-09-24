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
# Select a matching session deliberately — do not rely on glob order.
shopt -s nullglob
goal_file=""
todo_file=""
for g in .raven/sessions/*/state/goal.json; do
  t="$(dirname "$g")/todos.json"
  [[ -f "$t" ]] || continue
  if grep -qiE 'test|fix' "$g" && grep -q 'mean' "$t"; then
    goal_file=$g
    todo_file=$t
    break
  fi
done
# Fall back to the newest paired goal/todos by mtime when keywords differ.
if [[ -z "$goal_file" ]]; then
  newest=0
  for g in .raven/sessions/*/state/goal.json; do
    t="$(dirname "$g")/todos.json"
    [[ -f "$t" ]] || continue
    m=$(stat -c %Y "$g" 2>/dev/null || stat -f %m "$g")
    if (( m >= newest )); then
      newest=$m
      goal_file=$g
      todo_file=$t
    fi
  done
fi
test -n "$goal_file"
test -f "$todo_file"
# The goal and todos must reflect the real task.
grep -q -iE 'test|fix' "$goal_file"
grep -q 'mean' "$todo_file"
