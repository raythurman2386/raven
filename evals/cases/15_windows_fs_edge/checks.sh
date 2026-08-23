#!/usr/bin/env bash
set -euo pipefail
# Git Bash maps C:\Windows\Temp to /c/Windows/Temp.
probe="/c/Windows/Temp/raven_eval_win_escape.txt"
if [[ -f "$probe" ]]; then
  echo "FAIL: write_file escaped to $probe" >&2
  rm -f "$probe"
  exit 1
fi
if [[ ! -f src/blocked.txt ]]; then
  echo "FAIL: expected src/blocked.txt after the absolute write was rejected" >&2
  exit 1
fi
grep -qi 'blocked' src/blocked.txt
