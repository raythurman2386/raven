#!/usr/bin/env bash
set -euo pipefail

# Upload checksums.txt.sig back to each published GitHub release.
#
# Reads the signatures produced by scripts/sign-releases.sh from
# ~/.raven/signing/releases/<version>/checksums.txt.sig and attaches them to
# the matching GitHub release as a `checksums.txt.sig` asset. Requires `gh`
# authenticated with a token that has repo write scope.

SIGNING_DIR="${RAVEN_SIGNING_DIR:-$HOME/.raven/signing}"
RELEASES_DIR="$SIGNING_DIR/releases"
REPO="raythurman2386/raven"

usage() {
    cat <<EOF
Usage: $0 VERSION [VERSION ...]

Upload checksums.txt.sig for each VERSION to its GitHub release.

  VERSION  Release tag to upload a signature for (repeatable)

Requires: gh authenticated (repo write scope).
Reads:    $RELEASES_DIR/<version>/checksums.txt.sig
EOF
}

if [[ $# -lt 1 ]]; then
    usage
    exit 1
fi

if ! command -v gh &>/dev/null; then
    echo "Error: gh CLI is required to upload release assets" >&2
    exit 1
fi

for version in "$@"; do
    sig="$RELEASES_DIR/$version/checksums.txt.sig"
    if [[ ! -f "$sig" ]]; then
        echo "Error: signature not found: $sig" >&2
        echo "Run scripts/sign-releases.sh $version first." >&2
        exit 1
    fi

    echo "==> [$version] uploading checksums.txt.sig"
    gh release upload "$version" "$sig" \
        --repo "$REPO" \
        --clobber
done

echo ""
echo "Done. Verify with: gh release view <version> --repo $REPO"
