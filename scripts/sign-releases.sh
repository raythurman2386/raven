#!/usr/bin/env bash
set -euo pipefail

# Batch-sign the checksums.txt of one or more published GitHub releases.
#
# For each VERSION (a tag like v0.5.10), downloads the release's checksums.txt
# from GitHub, signs it with the Ed25519 secret key, and writes the result to
# ~/.raven/signing/releases/<version>/checksums.txt.sig. The checksums.txt is
# kept alongside so each version's signing inputs and outputs live in one
# folder.
#
# Uploading the .sig back to the release is a separate step (see
# scripts/upload-release-sigs.sh) so signing stays offline and the secret key
# never touches a network call.

SIGNING_DIR="${RAVEN_SIGNING_DIR:-$HOME/.raven/signing}"
SECRET_KEY="${RAVEN_SIGNING_KEY:-$SIGNING_DIR/secret.pem}"
RELEASES_DIR="$SIGNING_DIR/releases"
REPO="raythurman2386/raven"
BASE_URL="https://github.com/$REPO/releases/download"

usage() {
    cat <<EOF
Usage: $0 VERSION [VERSION ...]

Sign the checksums.txt of each published release VERSION (e.g. v0.5.10).

  VERSION  Release tag to sign (repeatable)

Writes, per version:
  $RELEASES_DIR/<version>/checksums.txt
  $RELEASES_DIR/<version>/checksums.txt.sig

Environment:
  RAVEN_SIGNING_DIR   Override the signing directory (default: ~/.raven/signing)
  RAVEN_SIGNING_KEY   Override the secret key path
EOF
}

if [[ $# -lt 1 ]]; then
    usage
    exit 1
fi

if [[ ! -f "$SECRET_KEY" ]]; then
    echo "Error: secret key not found: $SECRET_KEY" >&2
    exit 1
fi

for version in "$@"; do
    out_dir="$RELEASES_DIR/$version"
    mkdir -p "$out_dir"
    checksums="$out_dir/checksums.txt"

    echo "==> [$version] downloading checksums.txt"
    curl -fsSL --retry 3 --retry-delay 2 \
        -o "$checksums" \
        "$BASE_URL/$version/checksums.txt"

    echo "==> [$version] signing"
    openssl pkeyutl -sign -rawin \
        -in "$checksums" \
        -inkey "$SECRET_KEY" \
        -out "$checksums.sig"

    echo "==> [$version] wrote $checksums.sig"
done

echo ""
echo "Done. Signatures are in $RELEASES_DIR/<version>/."
echo "Upload them with scripts/upload-release-sigs.sh."
