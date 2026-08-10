#!/usr/bin/env bash
set -euo pipefail
cargo test --quiet
# tests must still assert hi bound
grep -q 'clamp(99, 0, 10), 10' src/lib.rs
