#!/bin/sh
# mac-smart-fans (msf) installer — downloads a pre-built release binary, no build from source.
#
#   curl -fsSL https://raw.githubusercontent.com/marshallku/mac-smart-fans/master/install.sh | sh
#
# Environment overrides:
#   MSF_VERSION      tag to install (default: latest release, e.g. "v0.1.0")
#   MSF_INSTALL_DIR  install directory (default: $HOME/.local/bin)
#   MSF_NO_VERIFY    set to skip SHA256 checksum verification
set -eu

REPO="marshallku/mac-smart-fans"
INSTALL_DIR="${MSF_INSTALL_DIR:-$HOME/.local/bin}"

err() {
    printf 'msf-install: %s\n' "$1" >&2
    exit 1
}

have() {
    command -v "$1" >/dev/null 2>&1
}

# --- pick a downloader -------------------------------------------------------
if have curl; then
    dl() { curl -fsSL "$1" -o "$2"; }
    dl_stdout() { curl -fsSL "$1"; }
elif have wget; then
    dl() { wget -qO "$2" "$1"; }
    dl_stdout() { wget -qO- "$1"; }
else
    err "need curl or wget on PATH"
fi

# --- detect platform ---------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Darwin)
        case "$arch" in
            arm64 | aarch64) suffix="darwin-arm64" ;;
            *) err "unsupported macOS arch: $arch (msf is Apple Silicon only)" ;;
        esac
        ;;
    *)
        err "unsupported OS: $os (msf is macOS / Apple Silicon only)" ;;
esac

# --- resolve version ---------------------------------------------------------
version="${MSF_VERSION:-}"
if [ -z "$version" ]; then
    printf 'msf-install: resolving latest release...\n' >&2
    version="$(dl_stdout "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"tag_name"' \
        | head -n1 \
        | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
    [ -n "$version" ] || err "could not resolve latest release tag (set MSF_VERSION=vX.Y.Z)"
fi

asset="msf-${suffix}"
base="https://github.com/$REPO/releases/download/$version"
printf 'msf-install: installing %s %s (%s)\n' "$REPO" "$version" "$suffix" >&2

# --- download into a temp dir ------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

dl "$base/$asset" "$tmp/$asset" || err "download failed: $base/$asset"

# --- verify checksum ---------------------------------------------------------
if [ -z "${MSF_NO_VERIFY:-}" ]; then
    if dl "$base/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
        expected="$(grep " $asset\$" "$tmp/SHA256SUMS" | head -n1 | cut -d' ' -f1)"
        if [ -n "$expected" ]; then
            if have shasum; then
                actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
            elif have sha256sum; then
                actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
            else
                actual=""
                printf 'msf-install: no sha256 tool, skipping verification\n' >&2
            fi
            if [ -n "$actual" ]; then
                [ "$expected" = "$actual" ] || err "checksum mismatch for $asset (expected $expected, got $actual)"
                printf 'msf-install: checksum ok\n' >&2
            fi
        else
            printf 'msf-install: %s not listed in SHA256SUMS, skipping verification\n' "$asset" >&2
        fi
    else
        printf 'msf-install: SHA256SUMS not available, skipping verification\n' >&2
    fi
fi

# --- install -----------------------------------------------------------------
mkdir -p "$INSTALL_DIR"
chmod +x "$tmp/$asset"

# macOS: clear the quarantine flag so an unsigned binary runs without a prompt.
if have xattr; then
    xattr -dr com.apple.quarantine "$tmp/$asset" 2>/dev/null || true
fi

mv -f "$tmp/$asset" "$INSTALL_DIR/msf"
printf 'msf-install: installed to %s/msf\n' "$INSTALL_DIR" >&2

# --- PATH hint ---------------------------------------------------------------
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        printf 'msf-install: %s is not on PATH — add this to your shell profile:\n' "$INSTALL_DIR" >&2
        printf '    export PATH="%s:$PATH"\n' "$INSTALL_DIR" >&2
        ;;
esac

"$INSTALL_DIR/msf" --version 2>/dev/null || printf 'msf-install: done. Run: msf --help\n' >&2

# --- post-install note -------------------------------------------------------
cat >&2 <<'EOF'

msf-install: NOTE — msf set/run/install require root (SMC writes).
  - One-time auth: sudo msf --help
  - NOPASSWD setup: see https://github.com/marshallku/mac-smart-fans/blob/master/INSTALL.md
  - Persistent daemon: sudo msf install --curve /path/to/curve.toml
EOF
