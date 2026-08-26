#!/usr/bin/env bash
set -euo pipefail

# Sign a release's checksums.txt with the Ed25519 secret key.
#
# Produces checksums.txt.sig alongside the checksums file. The installers
# verify this signature against the pinned public key before trusting the
# checksums, so a tampered checksums.txt (or binary) fails closed.

usage() {
    cat <<EOF
Usage: $0 CHECKSUMS_FILE SECRET_KEY

Sign CHECKSUMS_FILE with SECRET_KEY (Ed25519), writing CHECKSUMS_FILE.sig.

  CHECKSUMS_FILE  Path to checksums.txt
  SECRET_KEY      Path to the Ed25519 secret key (PEM)
EOF
}

if [[ $# -lt 2 ]]; then
    usage
    exit 1
fi

CHECKSUMS_FILE="$1"
SECRET_KEY="$2"

if ! command -v openssl &>/dev/null; then
    echo "Error: openssl is required to sign the release" >&2
    exit 1
fi

if [[ ! -f "$CHECKSUMS_FILE" ]]; then
    echo "Error: checksums file not found: $CHECKSUMS_FILE" >&2
    exit 1
fi
if [[ ! -f "$SECRET_KEY" ]]; then
    echo "Error: secret key not found: $SECRET_KEY" >&2
    exit 1
fi

SIG_FILE="${CHECKSUMS_FILE}.sig"

openssl pkeyutl -sign -rawin -in "$CHECKSUMS_FILE" -inkey "$SECRET_KEY" -out "$SIG_FILE"

echo "==> Signed $CHECKSUMS_FILE"
echo "==> Signature: $SIG_FILE"
