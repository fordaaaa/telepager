#!/bin/sh
# curl -fsSL https://raw.githubusercontent.com/fordaaaa/telepager/main/install.sh | sh
set -eu

REPO="fordaaaa/telepager"
BIN_DIR="${TELEPAGER_BIN_DIR:-$HOME/.local/bin}"
VERSION="${TELEPAGER_VERSION:-}"

say() { printf '%s\n' "$*"; }
die() { printf '\ntelepager: %s\n\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1; }

if need curl; then
  fetch() { curl -fsSL "$1"; }
elif need wget; then
  fetch() { wget -qO- "$1"; }
else
  die "needs curl or wget"
fi

if need sha256sum; then
  sum() { sha256sum "$1" | cut -d' ' -f1; }
elif need shasum; then
  sum() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "needs sha256sum or shasum"
fi

case "$(uname -s)" in
  Linux) os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *) die "unsupported os $(uname -s). this script only covers linux and macos.
  on windows, install the command with npm or cargo instead:
    npm install -g telepager
    cargo install telepager" ;;
esac

case "$(uname -m)" in
  x86_64|amd64) arch="x86_64" ;;
  arm64|aarch64) arch="aarch64" ;;
  *) die "unsupported cpu $(uname -m)" ;;
esac

target="$arch-$os"

if [ -z "$VERSION" ]; then
  VERSION=$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -1)
  [ -n "$VERSION" ] || die "could not work out the latest version, set TELEPAGER_VERSION"
fi

url="https://github.com/$REPO/releases/download/v$VERSION/telepager-$target"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "telepager $VERSION ($target)"
fetch "$url" > "$tmp/telepager" || die "no binary for $target at v$VERSION"
fetch "$url.sha256" > "$tmp/expected" || die "no checksum published for $target"

want=$(cut -d' ' -f1 < "$tmp/expected")
got=$(sum "$tmp/telepager")
[ "$want" = "$got" ] || die "checksum mismatch, refusing to install
  expected $want
  got      $got"

mkdir -p "$BIN_DIR"
chmod 755 "$tmp/telepager"
mv "$tmp/telepager" "$BIN_DIR/telepager"

say "installed to $BIN_DIR/telepager"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say ""; say "  $BIN_DIR isn't on your PATH — add it to your shell profile" ;;
esac

say ""
say "  next: telepager       (runs it here, ctrl-c ends it)"
say "        claude mcp add --scope user telepager -- telepager mcp"
