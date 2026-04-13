#!/usr/bin/env bash
# diffmind installer
# Usage:
#   curl -fsSL https://github.com/thinkgrid-labs/diffmind/releases/latest/download/install.sh | bash
#
# Options (env vars):
#   VERSION     — pin a specific release tag, e.g. VERSION=v0.6.0
#   INSTALL_DIR — override install location (default: /usr/local/bin)

set -euo pipefail

REPO="thinkgrid-labs/diffmind"
BIN="diffmind"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
VERSION="${VERSION:-latest}"

# ── Detect platform ──────────────────────────────────────────────────────────

OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Darwin)
    case "$ARCH" in
      x86_64)       TARGET="x86_64-apple-darwin" ;;
      arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
      *) echo "error: unsupported macOS architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  Linux)
    case "$ARCH" in
      x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
      aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
      *) echo "error: unsupported Linux architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "error: unsupported OS '$OS'." >&2
    echo "       For Windows, download the .zip from:" >&2
    echo "       https://github.com/$REPO/releases/latest" >&2
    exit 1
    ;;
esac

# ── Resolve download URL ─────────────────────────────────────────────────────

if [ "$VERSION" = "latest" ]; then
  BASE_URL="https://github.com/$REPO/releases/latest/download"
else
  BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
fi

ARCHIVE="${BIN}-${TARGET}.tar.gz"
URL="${BASE_URL}/${ARCHIVE}"

# ── Download & extract ───────────────────────────────────────────────────────

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "  Detected  $OS / $ARCH  →  $TARGET"
echo "  Fetching  $URL"
echo ""

if ! curl -fSL --progress-bar "$URL" -o "$TMP/$ARCHIVE"; then
  echo "" >&2
  echo "error: download failed. Check that version '$VERSION' exists at:" >&2
  echo "  https://github.com/$REPO/releases" >&2
  exit 1
fi

tar -xzf "$TMP/$ARCHIVE" -C "$TMP"

if [ ! -f "$TMP/$BIN" ]; then
  echo "error: binary '$BIN' not found in archive" >&2
  exit 1
fi

chmod +x "$TMP/$BIN"

# ── Install ──────────────────────────────────────────────────────────────────

if [ -w "$INSTALL_DIR" ]; then
  mv "$TMP/$BIN" "$INSTALL_DIR/$BIN"
else
  echo "  Installing to $INSTALL_DIR  (sudo required)"
  sudo mv "$TMP/$BIN" "$INSTALL_DIR/$BIN"
fi

# ── Done ─────────────────────────────────────────────────────────────────────

echo ""
echo "  ✓  diffmind installed → $INSTALL_DIR/$BIN"
echo ""
"$INSTALL_DIR/$BIN" --version
echo ""
echo "  Next steps:"
echo "    diffmind download          # download AI model (one-time setup)"
echo "    diffmind --branch main     # review your current branch"
echo ""
