#!/usr/bin/env bash
set -euo pipefail
cargo test --quiet
grep -q 'fn triple' src/lib.rs
