#!/usr/bin/env bash
set -euo pipefail

# Package a directory of built raven binaries into a release layout.
#
# Input:  a directory containing the raw release binaries, named
#         `raven-{version}-{triple}`.
# Output (written into the same directory):
#   - checksums.txt        SHA-256 of every raw binary, then of every archive
#   - raven-*.tar.gz       per-binary archive, containing a stable `raven`
#                          entry so consumers stay version-independent
#   - checksums.txt.sig    Ed25519 signature (only when a secret key is given)
#
# The raw binaries are kept alongside the archives (the installer downloads
# those directly). Archive checksums are appended to checksums.txt so the ACP
# registry's agent.json can pin them.

usage() {
    cat <<EOF
Usage: $0 BIN_DIR [SECRET_KEY]

Package the raven binaries in BIN_DIR into a release layout.

  BIN_DIR     Directory of built binaries (raven-{version}-{triple})
  SECRET_KEY  Optional path to the Ed25519 secret key; when given, the
              resulting checksums.txt is signed via scripts/sign-release.sh
EOF
}

if [[ $# -lt 1 ]]; then
    usage
    exit 1
fi

BIN_DIR="$1"
SECRET_KEY="${2:-}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ ! -d "$BIN_DIR" ]]; then
    echo "Error: BIN_DIR not found: $BIN_DIR" >&2
    exit 1
fi

if ! command -v sha256sum &>/dev/null; then
    echo "Error: sha256sum is required" >&2
    exit 1
fi

cd "$BIN_DIR"

# Integrity: checksum every raw binary. Two spaces between hash and name
# (matches the installer's parsing).
sha256sum raven-* > checksums.txt

# Build a per-binary archive with a stable inner name (`raven`).
for f in raven-*; do
    [[ "$f" == "checksums.txt" ]] && continue
    [[ "$f" == *.tar.gz ]] && continue
    mkdir -p stage
    cp "$f" stage/raven
    (cd stage && tar -czf "../${f}.tar.gz" raven)
    rm -rf stage
done

# Append archive checksums so the ACP registry can pin them.
sha256sum raven-*.tar.gz >> checksums.txt 2>/dev/null || true

echo "--- checksums.txt (raw + archives) ---"
cat checksums.txt

if [[ -n "$SECRET_KEY" ]]; then
    "$SCRIPT_DIR/sign-release.sh" checksums.txt "$SECRET_KEY"
fi
