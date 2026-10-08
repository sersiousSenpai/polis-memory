#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Polis Memory — one-line install:
#
#   curl -LsSf https://redline.dev/polis/install.sh | sh
#
# Downloads the polis binary for this machine from the GitHub release,
# verifies its SHA-256, installs it to ~/.local/bin, then runs `polis setup`,
# which shows its plan and asks before changing anything. Arguments after
# `sh -s --` go to setup (`... | sh -s -- --yes`, `--clients claude`).
#
# Environment: POLIS_VERSION (a tag; default latest), POLIS_BIN_DIR (default
# ~/.local/bin), POLIS_NO_SETUP=1 (install only). Undo: `polis uninstall`.
set -eu

REPO="${POLIS_INSTALL_REPO:-sersiousSenpai/polis-memory}"
VERSION="${POLIS_VERSION:-latest}"
BIN_DIR="${POLIS_BIN_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "polis-install: $*" >&2; }
die() { say "error: $*"; exit 1; }

download() {
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https,file' --tlsv1.2 -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$1" -O "$2"
  else
    die "needs curl or wget"
  fi
}

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Darwin)
    # An Intel shell on Apple silicon (Rosetta) still gets the native build.
    if [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then arch=arm64; fi
    case "$arch" in
      arm64|aarch64) target=aarch64-apple-darwin ;;
      x86_64) target=x86_64-apple-darwin ;;
      *) die "unsupported macOS architecture: $arch" ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64|amd64) target=x86_64-unknown-linux-gnu ;;
      aarch64|arm64) target=aarch64-unknown-linux-gnu ;;
      *) die "unsupported Linux architecture: $arch" ;;
    esac ;;
  *) die "unsupported OS: $os (on Windows: irm https://redline.dev/polis/install.ps1 | iex)" ;;
esac

if [ -n "${POLIS_INSTALL_BASE:-}" ]; then
  base="$POLIS_INSTALL_BASE"
elif [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi
archive="polis-memory-$target.tar.xz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $archive ($VERSION)"
download "$base/$archive" "$tmp/$archive" || die "could not download $base/$archive"
download "$base/$archive.sha256" "$tmp/$archive.sha256" || die "could not download the checksum"

want=$(awk 'NF { print $1; exit }' "$tmp/$archive.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$archive" | awk '{ print $1 }')
else
  got=$(shasum -a 256 "$tmp/$archive" | awk '{ print $1 }')
fi
[ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $archive (want $want, got $got) — nothing installed"

tar -xJf "$tmp/$archive" -C "$tmp" || die "could not unpack $archive (is xz installed?)"
src=$(find "$tmp" -type f -name polis | head -n 1)
[ -n "$src" ] || die "$archive has no polis binary"
mkdir -p "$BIN_DIR"
cp "$src" "$BIN_DIR/polis.new"
chmod 755 "$BIN_DIR/polis.new"
mv -f "$BIN_DIR/polis.new" "$BIN_DIR/polis"
say "installed $BIN_DIR/polis ($("$BIN_DIR/polis" --version 2>/dev/null || echo "version unknown"))"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "add $BIN_DIR to your PATH to run \`polis\` directly (e.g. in ~/.zshrc or ~/.bashrc: export PATH=\"$BIN_DIR:\$PATH\")" ;;
esac

if [ "${POLIS_NO_SETUP:-}" = 1 ]; then
  say "skipping setup (POLIS_NO_SETUP=1) — run \`$BIN_DIR/polis setup\` when ready"
  exit 0
fi
exec "$BIN_DIR/polis" setup --polis "$BIN_DIR/polis" "$@"
