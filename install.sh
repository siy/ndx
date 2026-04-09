#!/usr/bin/env bash
set -euo pipefail

# ndx installer — builds from source and sets up globally.
# Usage: curl -fsSL https://raw.githubusercontent.com/<repo>/master/install.sh | bash
#
# Requires: Rust toolchain (https://rustup.rs/)

REPO_URL="https://github.com/siy/ndx.git"
INSTALL_DIR="${HOME}/.local/bin"
CLONE_DIR="${TMPDIR:-/tmp}/ndx-install-$$"

info()  { printf '\033[1;34m%s\033[0m\n' "$*"; }
error() { printf '\033[1;31mError: %s\033[0m\n' "$*" >&2; exit 1; }

# Check prerequisites
command -v cargo >/dev/null 2>&1 || error "Rust toolchain not found. Install from https://rustup.rs/"
command -v git   >/dev/null 2>&1 || error "git not found"

info "ndx installer"
info "============="

# Clone
info "Cloning ndx..."
git clone --depth 1 "$REPO_URL" "$CLONE_DIR" 2>&1 | tail -1
cd "$CLONE_DIR"

# Build
info "Building release binary (this may take a few minutes on first build)..."
cargo build --release 2>&1 | tail -3

# Install binary
mkdir -p "$INSTALL_DIR"
cp target/release/ndx "$INSTALL_DIR/ndx"
chmod +x "$INSTALL_DIR/ndx"
info "Binary installed to $INSTALL_DIR/ndx"

# Check PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
    info ""
    info "NOTE: $INSTALL_DIR is not in your PATH."
    info "Add to your shell profile:"
    info "  export PATH=\"$INSTALL_DIR:\$PATH\""
    info ""
fi

# Run ndx install (downloads manifests, registers hook + skills)
info "Running ndx install (manifests + hook + skills)..."
"$INSTALL_DIR/ndx" install 2>&1

# Cleanup
rm -rf "$CLONE_DIR"

info ""
info "Done! Restart Claude Code to activate the hook and skills."
info ""
info "Quick start in any project:"
info "  ndx init                      # install skills into this project"
info "  ndx recall init               # create the recall palace"
info "  ndx recall mine --from-memory # seed from session history"
info "  /ndx-recall-classify          # let Claude organize your drawers"
