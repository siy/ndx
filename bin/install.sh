#!/usr/bin/env bash
# ndx installer — downloads the ndx binary and command manifests
# Usage: curl -fsSL https://raw.githubusercontent.com/<repo>/main/bin/install.sh | bash
set -euo pipefail

NDX_DIR="$HOME/.ndx"
NDX_BIN="$NDX_DIR/ndx"
COMMANDS_DIR="$NDX_DIR/commands"

MANIFEST_INDEX_URL="https://raw.githubusercontent.com/Cantara/kcp-commands/main/commands/index.txt"
MANIFEST_BASE_URL="https://raw.githubusercontent.com/Cantara/kcp-commands/main/commands"

echo "ndx installer"
echo "============="
echo ""

# Detect platform
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$OS" in
  darwin) OS="macos" ;;
  linux)  OS="linux" ;;
  *)      echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  arm64|aarch64) ARCH="aarch64" ;;
  *)            echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

echo "Platform: $OS-$ARCH"

# Create directories
mkdir -p "$NDX_DIR" "$COMMANDS_DIR"

# Check if ndx binary already exists (built from source)
if command -v ndx &>/dev/null; then
  NDX_BIN="$(command -v ndx)"
  echo "Found ndx at: $NDX_BIN"
elif [ -f "$NDX_BIN" ]; then
  echo "Found ndx at: $NDX_BIN"
else
  echo ""
  echo "ndx binary not found."
  echo "Build from source:"
  echo "  git clone <repo>"
  echo "  cd ndx && cargo build --release"
  echo "  cp target/release/ndx ~/.ndx/ndx"
  echo ""
  echo "Then re-run this script."
  echo ""
  echo "Continuing with manifest download only..."
fi

# Download command manifests from kcp-commands
echo ""
echo "Downloading command manifests from kcp-commands..."

INDEX=$(curl -fsSL --connect-timeout 10 "$MANIFEST_INDEX_URL" 2>/dev/null || echo "")
if [ -z "$INDEX" ]; then
  echo "  Warning: could not fetch manifest index from:"
  echo "    $MANIFEST_INDEX_URL"
  echo "  Manifests not downloaded. You can download them manually."
else
  TOTAL=$(echo "$INDEX" | wc -l | tr -d ' ')
  COUNT=0
  ERRORS=0

  while IFS= read -r key; do
    key="$(echo "$key" | tr -d '[:space:]')"
    [ -z "$key" ] && continue

    if curl -fsSL --connect-timeout 5 -o "$COMMANDS_DIR/${key}.yaml" "$MANIFEST_BASE_URL/${key}.yaml" 2>/dev/null; then
      COUNT=$((COUNT + 1))
    else
      ERRORS=$((ERRORS + 1))
    fi

    # Progress every 50
    CURRENT=$((COUNT + ERRORS))
    if [ $((CURRENT % 50)) -eq 0 ] || [ "$CURRENT" -eq "$TOTAL" ]; then
      printf "\r  Progress: %d/%d" "$CURRENT" "$TOTAL"
    fi
  done <<< "$INDEX"

  echo ""
  echo "  Downloaded: $COUNT manifests to $COMMANDS_DIR"
  [ "$ERRORS" -gt 0 ] && echo "  Errors: $ERRORS"
fi

# Register in Claude Code settings (if binary is available)
if [ -x "$NDX_BIN" ]; then
  echo ""
  echo "Running ndx install to register with Claude Code..."
  "$NDX_BIN" install 2>&1 | grep -v "^Downloading" || true
else
  echo ""
  echo "To complete setup after building ndx:"
  echo "  ndx install"
fi

echo ""
echo "Acknowledgment: Command manifests are from"
echo "  https://github.com/Cantara/kcp-commands (Apache 2.0)"
echo ""
