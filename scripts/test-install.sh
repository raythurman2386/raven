#!/usr/bin/env bash
set -euo pipefail

# Local test harness for the raven installer's integrity + authenticity checks.
#
# Builds a fake release (binary + checksums.txt + checksums.txt.sig) in a local
# directory and runs install.sh against it using the local-mirror mode
# (RAVEN_RELEASE_BASE_URL pointing at a filesystem path). Verifies that:
#   1. a clean, correctly-signed release installs successfully;
#   2. a tampered binary is refused (checksum mismatch);
#   3. a tampered checksums.txt is refused (signature verification fails);
#   4. a missing signature is refused (fail closed).
#
# No network access is required. Requires: bash, openssl, sha256sum.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SH="$ROOT/install.sh"
SIGN_SH="$ROOT/scripts/sign-release.sh"

VERSION="0.5.1"
VERSION_TAG="v$VERSION"

# Detect the triple the same way install.sh does (Linux-only here).
arch="$(uname -m)"
case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) echo "unsupported arch: $arch" >&2; exit 1 ;;
esac
TRIPLE="${arch}-unknown-linux-gnu"
ARTIFACT="raven-${VERSION}-${TRIPLE}"

WORK="$(mktemp -d)"
RELEASE_ROOT="$WORK/release"
RELEASE_DIR="$RELEASE_ROOT/$VERSION_TAG"
INSTALL_DIR="$WORK/install"
KEYS="$WORK/keys"

cleanup() {
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

pass() {
    echo "PASS: $1"
}

# --- helpers ---------------------------------------------------------------

make_fake_binary() {
    # A tiny executable that reports a version, standing in for the real raven.
    cat > "$1" <<'EOF'
#!/usr/bin/env bash
echo "raven 0.5.1 (fake)"
EOF
    chmod +x "$1"
}

write_checksums() {
    # checksums.txt uses two spaces between hash and name (matches install.sh).
    local hash
    hash="$(sha256sum "$RELEASE_DIR/$ARTIFACT" | awk '{print $1}')"
    printf '%s  %s\n' "$hash" "$ARTIFACT" > "$RELEASE_DIR/checksums.txt"
}

run_install() {
    # Run install.sh against the local release directory. Returns its exit code.
    RAVEN_RELEASE_BASE_URL="$RELEASE_ROOT" \
        bash "$INSTALL_SH_PATCHED" --version "$VERSION" --to "$INSTALL_DIR" --force \
        >"$WORK/out.log" 2>&1
}

# --- setup: generate a keypair and patch install.sh's pinned key -----------

mkdir -p "$RELEASE_DIR" "$INSTALL_DIR" "$KEYS"

# Generate a fresh keypair. The harness patches a temp copy of install.sh to
# pin this generated public key, so the test is self-contained (it does not
# depend on the committed public key matching a secret we can produce here).
openssl genpkey -algorithm ED25519 -out "$KEYS/secret.pem" 2>/dev/null
openssl pkey -in "$KEYS/secret.pem" -pubout -out "$KEYS/public.pem" 2>/dev/null

INSTALL_SH_PATCHED="$WORK/install.sh"
python3 - "$INSTALL_SH" "$KEYS/public.pem" "$INSTALL_SH_PATCHED" <<'PY'
import sys, re
src, pub, dst = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(src).read()
pubkey = open(pub).read().strip()
patched = re.sub(
    r"-----BEGIN PUBLIC KEY-----\n.*?\n-----END PUBLIC KEY-----",
    pubkey,
    text,
    count=1,
    flags=re.S,
)
open(dst, "w").write(patched)
PY

# --- case 1: clean install succeeds ---------------------------------------

make_fake_binary "$RELEASE_DIR/$ARTIFACT"
write_checksums
bash "$SIGN_SH" "$RELEASE_DIR/checksums.txt" "$KEYS/secret.pem" >/dev/null

run_install
rc=0
run_install || rc=$?
if [[ $rc -ne 0 ]]; then
    cat "$WORK/out.log" >&2
    fail "clean install should succeed"
fi
[[ -x "$INSTALL_DIR/raven" ]] || fail "raven binary not installed"
grep -q "Signature OK" "$WORK/out.log" || fail "expected 'Signature OK' in output"
grep -q "Checksum OK" "$WORK/out.log" || fail "expected 'Checksum OK' in output"
pass "case 1: clean signed release installs"

# --- case 2: tampered binary is refused ------------------------------------

rm -rf "$RELEASE_DIR" "$INSTALL_DIR"
mkdir -p "$RELEASE_DIR" "$INSTALL_DIR"
make_fake_binary "$RELEASE_DIR/$ARTIFACT"
write_checksums
bash "$SIGN_SH" "$RELEASE_DIR/checksums.txt" "$KEYS/secret.pem" >/dev/null
# Tamper with the binary AFTER checksums were computed.
printf 'evil payload\n' >> "$RELEASE_DIR/$ARTIFACT"

rc=0
run_install || rc=$?
if [[ $rc -eq 0 ]]; then
    fail "tampered binary should be refused"
fi
grep -q "checksum mismatch" "$WORK/out.log" || fail "expected checksum mismatch message"
[[ ! -e "$INSTALL_DIR/raven" ]] || fail "tampered binary must not be installed"
pass "case 2: tampered binary refused"

# --- case 3: tampered checksums.txt is refused -----------------------------

rm -rf "$RELEASE_DIR" "$INSTALL_DIR"
mkdir -p "$RELEASE_DIR" "$INSTALL_DIR"
make_fake_binary "$RELEASE_DIR/$ARTIFACT"
write_checksums
bash "$SIGN_SH" "$RELEASE_DIR/checksums.txt" "$KEYS/secret.pem" >/dev/null
# Tamper with checksums.txt AFTER signing (signature will no longer verify).
printf 'deadbeef  %s\n' "$ARTIFACT" >> "$RELEASE_DIR/checksums.txt"

rc=0
run_install || rc=$?
if [[ $rc -eq 0 ]]; then
    fail "tampered checksums.txt should be refused"
fi
grep -q "signature verification FAILED" "$WORK/out.log" || fail "expected signature failure message"
[[ ! -e "$INSTALL_DIR/raven" ]] || fail "binary must not be installed on bad signature"
pass "case 3: tampered checksums.txt refused"

# --- case 4: missing signature is refused (fail closed) --------------------

rm -rf "$RELEASE_DIR" "$INSTALL_DIR"
mkdir -p "$RELEASE_DIR" "$INSTALL_DIR"
make_fake_binary "$RELEASE_DIR/$ARTIFACT"
write_checksums
# Intentionally do NOT sign — no checksums.txt.sig present.

rc=0
run_install || rc=$?
if [[ $rc -eq 0 ]]; then
    fail "missing signature should be refused"
fi
grep -q "without a release signature" "$WORK/out.log" || fail "expected missing-signature message"
[[ ! -e "$INSTALL_DIR/raven" ]] || fail "binary must not be installed without a signature"
pass "case 4: missing signature refused (fail closed)"

echo ""
echo "All installer integrity/authenticity tests passed."
