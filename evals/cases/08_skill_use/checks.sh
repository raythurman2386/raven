#!/usr/bin/env bash
set -euo pipefail
test -f src/hello.txt
# exact contents (with or without trailing newline)
got=$(cat src/hello.txt | tr -d '\r')
got_stripped=$(printf '%s' "$got" | sed 's/\n$//')
if [[ "$got" != "HELLO_FROM_SKILL_V1" && "$got" != $'HELLO_FROM_SKILL_V1\n' ]]; then
  echo "FAIL: unexpected hello.txt contents: $(cat -A src/hello.txt)" >&2
  exit 1
fi
