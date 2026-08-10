#!/usr/bin/env bash
set -euo pipefail
cargo test --quiet
# old public name should be gone
if grep -n 'pub fn greet' src/*.rs; then
  echo "FAIL: pub fn greet still present" >&2
  exit 1
fi
grep -q 'pub fn welcome' src/lib.rs
grep -q 'welcome' src/helper.rs
