#!/bin/sh
# Install the agent-browser prebuilt binary from GitHub Releases.
#   curl -fsSL https://raw.githubusercontent.com/maestrojeong/agent-browser-mcp/main/install.sh | sh
# Env: AB_VERSION (default: latest), AB_BIN_DIR (default: /usr/local/bin or ~/.local/bin)
set -e

REPO="maestrojeong/agent-browser-mcp"
VERSION="${AB_VERSION:-latest}"

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS-$ARCH" in
  Darwin-arm64)      ASSET="agent-browser-macos-arm64" ;;
  Linux-x86_64)      ASSET="agent-browser-linux-x64" ;;
  Linux-aarch64)     echo "No prebuilt Linux-arm64 yet."; NEED_SRC=1 ;;
  Darwin-x86_64)     echo "No prebuilt Intel-mac yet."; NEED_SRC=1 ;;
  *)                 echo "Unsupported: $OS-$ARCH"; NEED_SRC=1 ;;
esac
if [ "${NEED_SRC:-0}" = "1" ]; then
  echo "Build from source instead:"
  echo "  cargo install --git https://github.com/$REPO ab-mcp"
  exit 1
fi

if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/$REPO/releases/latest/download/$ASSET"
else
  URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"
fi

if [ -n "${AB_BIN_DIR:-}" ]; then
  DEST="$AB_BIN_DIR"
  mkdir -p "$DEST"
else
  DEST="/usr/local/bin"
  if ! ( [ -d "$DEST" ] && [ -w "$DEST" ] ); then
    DEST="$HOME/.local/bin"
    mkdir -p "$DEST"
  fi
fi

echo "Downloading $ASSET ($VERSION) -> $DEST/agent-browser"
curl -fsSL "$URL" -o "$DEST/agent-browser"
chmod +x "$DEST/agent-browser"
echo "Installed: $DEST/agent-browser"

case ":$PATH:" in
  *":$DEST:"*) echo "Run: agent-browser --help" ;;
  *) echo "Add to PATH:  export PATH=\"$DEST:\$PATH\"   then: agent-browser --help" ;;
esac
