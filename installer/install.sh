#!/bin/sh
# Bugsee CLI installer (macOS / Linux).
#
#   curl --proto '=https' --tlsv1.2 -sSfL https://download.bugsee.com/cli/install.sh | sh
#
# Downloads the published bugsee-cli binary for this host from
# download.bugsee.com, SHA-256-verifies it against the published checksum, and
# installs it. No GitHub dependency; the same bytes the Gradle plugin and the
# iOS BugseeAgents download.
#
# Environment overrides:
#   BUGSEE_CLI_VERSION       pin an exact X.Y.Z (default: the latest release)
#   BUGSEE_CLI_INSTALL_DIR   install directory (default: /usr/local/bin if
#                            writable, else ~/.local/bin)
#   BUGSEE_CLI_BASE_URL      download root (default: https://download.bugsee.com/cli)
set -eu

BASE="${BUGSEE_CLI_BASE_URL:-https://download.bugsee.com/cli}"
VERSION="${BUGSEE_CLI_VERSION:-}"
INSTALL_DIR="${BUGSEE_CLI_INSTALL_DIR:-}"

say() { printf '%s\n' "$*" >&2; }
err() { printf 'bugsee-cli install error: %s\n' "$*" >&2; exit 1; }

command -v uname >/dev/null 2>&1 || err "required tool not found: uname"
command -v tar >/dev/null 2>&1 || err "required tool not found: tar"
command -v mktemp >/dev/null 2>&1 || err "required tool not found: mktemp"

# Pick a downloader: 'dl_to URL FILE' downloads to a file; 'dl URL' to stdout.
if command -v curl >/dev/null 2>&1; then
    dl_to() { curl --proto '=https' --tlsv1.2 -fsSL -o "$2" "$1"; }
    dl() { curl --proto '=https' --tlsv1.2 -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
    dl_to() { wget -qO "$2" "$1"; }
    dl() { wget -qO- "$1"; }
else
    err "need curl or wget to download"
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux)
        # The published Linux build links glibc (unknown-linux-gnu) and will
        # not run on musl (e.g. Alpine). Detect musl and bail with guidance.
        if (ldd --version 2>&1 || true) | grep -qi musl; then
            err "musl libc detected (e.g. Alpine) — no musl build is published; build from source instead"
        fi
        os_part="unknown-linux-gnu"
        ;;
    *) err "unsupported OS '$os' (supported: macOS, Linux; on Windows use install.ps1)" ;;
esac
case "$arch" in
    arm64 | aarch64) arch_part="aarch64" ;;
    x86_64 | amd64) arch_part="x86_64" ;;
    *) err "unsupported architecture '$arch'" ;;
esac
triple="${arch_part}-${os_part}"

if [ -z "$VERSION" ]; then
    VERSION="$(dl "$BASE/latest/version.txt" | tr -d '[:space:]')" || err "could not fetch the latest version"
fi
[ -n "$VERSION" ] || err "could not determine a version to install"
# Reject anything that isn't a clean version token (path-traversal guard — the
# value is interpolated into the download URL).
case "$VERSION" in
    *[!0-9A-Za-z.+-]*) err "unexpected version string: $VERSION" ;;
esac

art="bugsee-cli-${triple}.tar.xz"
url="$BASE/v${VERSION}/${art}"

tmp="$(mktemp -d 2>/dev/null || mktemp -d -t bugsee-cli)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading bugsee-cli ${VERSION} (${triple})…"
dl_to "$url" "$tmp/$art" || err "download failed: $url"

expected="$(dl "${url}.sha256" | awk '{print $1}')" || err "could not fetch checksum for $art"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$art" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/$art" | awk '{print $1}')"
else
    err "no SHA-256 tool found (need sha256sum or shasum)"
fi
[ "$expected" = "$actual" ] || err "checksum mismatch for $art (expected $expected, got $actual)"

tar -xf "$tmp/$art" -C "$tmp" --strip-components=1 || err "could not extract $art"
[ -f "$tmp/bugsee-cli" ] || err "bugsee-cli binary not found after extraction"
chmod +x "$tmp/bugsee-cli"

if [ -z "$INSTALL_DIR" ]; then
    if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
        INSTALL_DIR="/usr/local/bin"
    else
        INSTALL_DIR="$HOME/.local/bin"
    fi
fi
mkdir -p "$INSTALL_DIR" || err "could not create install directory $INSTALL_DIR"
mv -f "$tmp/bugsee-cli" "$INSTALL_DIR/bugsee-cli" || err "could not install into $INSTALL_DIR (try BUGSEE_CLI_INSTALL_DIR=...)"

say "Installed bugsee-cli ${VERSION} → ${INSTALL_DIR}/bugsee-cli"
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        say ""
        say "  ${INSTALL_DIR} is not on your PATH — add it, e.g.:"
        say "    export PATH=\"${INSTALL_DIR}:\$PATH\""
        ;;
esac
"${INSTALL_DIR}/bugsee-cli" --version >&2 2>/dev/null || true
