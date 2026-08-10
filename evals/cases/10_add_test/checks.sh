#!/usr/bin/env bash
set -euo pipefail
cargo test --quiet
grep -q '#\[test\]' src/lib.rs
grep -q 'is_even' src/lib.rs
