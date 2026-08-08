#!/usr/bin/env bash
set -euo pipefail

REPO="raythurman2386/raven"
BINARY="raven"
DEFAULT_INSTALL_DIR="$HOME/.cargo/bin"
VERSION=""
INSTALL_DIR=""
FORCE=false

usage() {
    cat <<EOF
Usage: $0 [OPTIONS]

Install raven from a prebuilt GitHub Release binary.

Options:
  --version VERSION  Install a specific version (default: latest)
  --to DIR           Install to DIR (default: \$HOME/.cargo/bin)
  --force            Overwrite existing binary without prompting
  -h, --help         Show this help message

Examples:
  curl -fsSL https://raw.githubusercontent.com/$REPO/master/install.sh | sh
  $0 --version 0.1.0
  $0 --to /usr/local/bin
EOF
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)
            VERSION="$2"
            shift 2
            ;;
        --to)
            INSTALL_DIR="$2"
            shift 2
            ;;
        --force)
            FORCE=true
            shift
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "Unknown option: $1"
            usage
            ;;
    esac
done

INSTALL_DIR="${INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"

detect_platform() {
    local os arch triple

    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)  os="unknown-linux-gnu" ;;
        Darwin) os="apple-darwin" ;;
        *)
            echo "Error: unsupported OS: $os" >&2
            exit 1
            ;;
    esac

    case "$arch" in
        x86_64|amd64)
            arch="x86_64"
            ;;
        aarch64|arm64)
            arch="aarch64"
            ;;
        armv7l|armv6l)
            arch="armv7"
            os="unknown-linux-gnueabihf"
            ;;
        *)
            echo "Error: unsupported architecture: $arch" >&2
            exit 1
            ;;
    esac

    triple="${arch}-${os}"
    echo "$triple"
}

get_latest_version() {
    local tag
    tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
        | grep '"tag_name":' \
        | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    if [[ -z "$tag" ]]; then
        echo "Error: could not determine latest version from GitHub API" >&2
        exit 1
    fi
    echo "$tag"
}

main() {
    local triple version_tag download_url checksum_url tmp_dir

    triple="$(detect_platform)"

    if [[ -z "$VERSION" ]]; then
        version_tag="$(get_latest_version)"
    else
        version_tag="$VERSION"
        if [[ "$version_tag" != v* ]]; then
            version_tag="v$version_tag"
        fi
    fi

    local version_no_v="${version_tag#v}"
    local artifact="${BINARY}-${version_no_v}-${triple}"

    if [[ "$triple" == *"windows"* ]]; then
        artifact="${artifact}.exe"
    fi

    download_url="https://github.com/$REPO/releases/download/${version_tag}/${artifact}"
    checksum_url="https://github.com/$REPO/releases/download/${version_tag}/checksums.txt"

    echo "==> Platform:  $triple"
    echo "==> Version:   $version_tag"
    echo "==> Artifact:  $artifact"
    echo "==> Install:   $INSTALL_DIR"

    if [[ -f "$INSTALL_DIR/$BINARY" ]] && [[ "$FORCE" != true ]]; then
        echo "==> $BINARY already exists at $INSTALL_DIR/$BINARY"
        echo "    Use --force to overwrite."
        exit 0
    fi

    tmp_dir="$(mktemp -d)"
    trap 'rm -rf "$tmp_dir"' EXIT

    echo "==> Downloading $download_url ..."
    if ! curl -fsSL --retry 3 --retry-delay 2 -o "$tmp_dir/$artifact" "$download_url"; then
        echo "Error: failed to download $download_url" >&2
        echo "Check that the release exists and the artifact name is correct." >&2
        exit 1
    fi

    # Fail closed on integrity: the checksum file and matching entry are
    # required. If they're missing, refuse to install rather than silently
    # shipping an unverified binary.
    if ! curl -fsSL --retry 3 --retry-delay 2 -o "$tmp_dir/checksums.txt" "$checksum_url" 2>/dev/null; then
        echo "Error: failed to download checksums.txt from $checksum_url" >&2
        echo "Refusing to install without a checksum. Verify the release is complete." >&2
        exit 1
    fi

    local expected
    expected="$(grep "$artifact" "$tmp_dir/checksums.txt" | awk '{print $1}')"
    if [[ -z "$expected" ]]; then
        echo "Error: no checksum entry found for $artifact in checksums.txt" >&2
        echo "Refusing to install an unverified binary." >&2
        exit 1
    fi

    local actual
    if command -v sha256sum &>/dev/null; then
        actual="$(sha256sum "$tmp_dir/$artifact" | awk '{print $1}')"
    elif command -v shasum &>/dev/null; then
        actual="$(shasum -a 256 "$tmp_dir/$artifact" | awk '{print $1}')"
    else
        echo "Error: neither sha256sum nor shasum is available to verify the download" >&2
        exit 1
    fi

    if [[ "$actual" != "$expected" ]]; then
        echo "Error: checksum mismatch!" >&2
        echo "  expected: $expected" >&2
        echo "  actual:   $actual" >&2
        exit 1
    fi
    echo "==> Checksum OK"

    mkdir -p "$INSTALL_DIR"
    mv "$tmp_dir/$artifact" "$INSTALL_DIR/$BINARY"
    chmod +x "$INSTALL_DIR/$BINARY"

    echo "==> Installed $BINARY $version_tag to $INSTALL_DIR/$BINARY"

    if command -v "$BINARY" &>/dev/null; then
        echo "==> Version: $("$BINARY" --version 2>/dev/null || echo "unknown")"
    fi

    if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
        echo ""
        echo "Note: $INSTALL_DIR is not in your PATH."
        echo "Add it to your shell profile:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
    fi
}

main
