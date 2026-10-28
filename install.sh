#!/bin/sh
# dependabatch installer.
#
#   curl -fsSL https://raw.githubusercontent.com/thiagobraga/dependabatch/main/install.sh | sh
#
# Optional environment overrides:
#   DEPENDABATCH_VERSION=v0.1.0   pin a release tag
#   DEPENDABATCH_INSTALL=/dir     install dir (default ~/.local/bin)
#   DEPENDABATCH_REPO=owner/repo  source repo (default thiagobraga/dependabatch)

set -eu

REPO="${DEPENDABATCH_REPO:-thiagobraga/dependabatch}"
BIN_NAME="dependabatch"
VERSION="${DEPENDABATCH_VERSION:-latest}"

say() {
    printf 'dependabatch installer: %s\n' "$*"
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

fetch() {
    # $1 = output file, $2 = url
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 --retry-connrefused -o "$1" "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$1" "$2"
    else
        fail "need curl or wget to download releases"
    fi
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux | Darwin) ;;
        *) fail "unsupported platform: $os (grab an asset manually from https://github.com/$REPO/releases)" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) fail "unsupported architecture: $arch" ;;
    esac
    case "$os/$arch" in
        Linux/x86_64) printf 'x86_64-unknown-linux-musl' ;;
        Linux/aarch64) printf 'aarch64-unknown-linux-gnu' ;;
        Darwin/x86_64) printf 'x86_64-apple-darwin' ;;
        Darwin/aarch64) printf 'aarch64-apple-darwin' ;;
        *) fail "no release target for $os/$arch" ;;
    esac
}

target=$(detect_target)
base="https://github.com/$REPO/releases"
archive="$BIN_NAME-$target.tar.gz"
if [ "$VERSION" = "latest" ]; then
    url="$base/latest/download/$archive"
else
    url="$base/download/$VERSION/$archive"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "detected target $target"
say "downloading $archive"
fetch "$tmp/$archive" "$url"

if fetch "$tmp/$archive.sha256" "$url.sha256" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
        expected=$(awk '{print $1}' "$tmp/$archive.sha256")
        actual=$(sha256sum "$tmp/$archive" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        expected=$(awk '{print $1}' "$tmp/$archive.sha256")
        actual=$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')
    else
        actual=$expected
    fi
    if [ "$actual" != "$expected" ]; then
        fail "checksum mismatch: $archive"
    fi
    say "checksum ok"
else
    say "no checksum published for this target, skipping verification"
fi

tar -xzf "$tmp/$archive" -C "$tmp"

bin="$tmp/$BIN_NAME"
if [ ! -f "$bin" ]; then
    fail "archive did not contain the $BIN_NAME binary"
fi

dest="${DEPENDABATCH_INSTALL:-$HOME/.local/bin}"
mkdir -p "$dest"
install -m 755 "$bin" "$dest/$BIN_NAME"

case ":$PATH:" in
    *":$dest:"*) ;;
    *) say "note: $dest is not on PATH; run: export PATH=\"$dest:\$PATH\"" ;;
esac

"$dest/$BIN_NAME" --version
