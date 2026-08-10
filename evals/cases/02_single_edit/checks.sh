#!/usr/bin/env bash
set -euo pipefail
cargo test --quiet
grep -qE 'n \* 2|n\*2|2 \* n|2\*n' src/lib.rs
