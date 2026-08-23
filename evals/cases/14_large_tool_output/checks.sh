#!/usr/bin/env bash
set -euo pipefail
if [[ ! -f src/out.txt ]]; then
  echo "FAIL: src/out.txt was not written" >&2
  exit 1
fi
grep -q 'MARKER_HEAD_alpha' src/out.txt
grep -q 'MARKER_TAIL_omega' src/out.txt
