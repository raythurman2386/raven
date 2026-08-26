#!/usr/bin/env bash
set -euo pipefail

# Generate an Ed25519 signing keypair for raven release signing.
#
# The secret key MUST be kept offline: never commit it, never put it in CI,
# never upload it to a release. Only the public key is committed and pinned
# in the installers (install.sh / install.ps1).
#
# The same Ed25519 primitive is used by minisign; OpenSSL is used here so the
# whole flow is testable locally with no external tooling.

OUT_DIR="${1:-$HOME/.raven/signing}"

usage() {
    cat <<EOF
Usage: $0 [OUT_DIR]

Generate an Ed25519 keypair for signing raven releases.

  OUT_DIR  Directory to write the keys (default: \$HOME/.raven/signing)

Writes:
  OUT_DIR/secret.pem   Secret key — KEEP OFFLINE, never commit
  OUT_DIR/public.pem   Public key — commit and pin in the installers

After generating, copy the contents of public.pem into the pinned public key
in install.sh and install.ps1 (or the raven-signing-key.pub file).
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi

if ! command -v openssl &>/dev/null; then
    echo "Error: openssl is required to generate the signing key" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"

if [[ -f "$OUT_DIR/secret.pem" ]]; then
    echo "Error: $OUT_DIR/secret.pem already exists. Refusing to overwrite." >&2
    echo "Move it somewhere safe first, or choose a different OUT_DIR." >&2
    exit 1
fi

openssl genpkey -algorithm ED25519 -out "$OUT_DIR/secret.pem"
openssl pkey -in "$OUT_DIR/secret.pem" -pubout -out "$OUT_DIR/public.pem"

chmod 600 "$OUT_DIR/secret.pem"

echo "==> Generated Ed25519 keypair in $OUT_DIR"
echo "==> Secret key: $OUT_DIR/secret.pem  (KEEP OFFLINE — never commit)"
echo "==> Public key: $OUT_DIR/public.pem  (commit + pin in installers)"
echo ""
echo "Public key contents:"
cat "$OUT_DIR/public.pem"
