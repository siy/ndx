#!/usr/bin/env bash
set -euo pipefail

# ndx installer — downloads a prebuilt binary from GitHub Releases.
# Falls back to building from source if no prebuilt is available.
#
# Usage: curl -fsSL https://raw.githubusercontent.com/siy/ndx/master/install.sh | bash

REPO="siy/ndx"
INSTALL_DIR="${HOME}/.local/bin"

info()  { printf '\033[1;34m%s\033[0m\n' "$*"; }
warn()  { printf '\033[1;33m%s\033[0m\n' "$*"; }
error() { printf '\033[1;31mError: %s\033[0m\n' "$*" >&2; exit 1; }

detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin) os="macos" ;;
        Linux)  os="linux" ;;
        *)      error "Unsupported OS: $os" ;;
    esac

    case "$arch" in
        x86_64|amd64)  arch="x86_64" ;;
        arm64|aarch64) arch="aarch64" ;;
        *)             error "Unsupported architecture: $arch" ;;
    esac

    echo "ndx-${os}-${arch}"
}

latest_tag() {
    # Use GitHub API to get latest release tag.
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
        | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
}

download_prebuilt() {
    local artifact="$1" tag="$2"
    local url="https://github.com/${REPO}/releases/download/${tag}/${artifact}"

    info "Downloading ${artifact} (${tag})..."
    if curl -fsSL -o "${INSTALL_DIR}/ndx" "$url" 2>/dev/null; then
        chmod +x "${INSTALL_DIR}/ndx"
        return 0
    fi
    return 1
}

build_from_source() {
    command -v cargo >/dev/null 2>&1 || error "No prebuilt binary and Rust toolchain not found. Install from https://rustup.rs/"
    command -v git   >/dev/null 2>&1 || error "git not found"

    local clone_dir="${TMPDIR:-/tmp}/ndx-install-$$"
    info "No prebuilt binary available — building from source..."
    git clone --depth 1 "https://github.com/${REPO}.git" "$clone_dir" 2>&1 | tail -1
    cd "$clone_dir"
    cargo build --release 2>&1 | tail -3
    cp target/release/ndx "${INSTALL_DIR}/ndx"
    chmod +x "${INSTALL_DIR}/ndx"
    rm -rf "$clone_dir"
}

# ── Main ──

info "ndx installer"
info "============="

mkdir -p "$INSTALL_DIR"
ARTIFACT="$(detect_platform)"
TAG="$(latest_tag)"

if [ -n "$TAG" ] && download_prebuilt "$ARTIFACT" "$TAG"; then
    info "Binary installed to ${INSTALL_DIR}/ndx"
else
    warn "Prebuilt download failed — falling back to source build"
    build_from_source
    info "Binary installed to ${INSTALL_DIR}/ndx"
fi

# Check PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
    info ""
    info "NOTE: ${INSTALL_DIR} is not in your PATH."
    info "Add to your shell profile:"
    info "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    info ""
fi

# Run ndx install (manifests + hook + skills)
info "Running ndx install (manifests + hook + skills)..."
"${INSTALL_DIR}/ndx" install 2>&1

info ""
info "Done! Restart Claude Code to activate the hook and skills."
info ""
info "Quick start in any project:"
info "  ndx init                      # install skills + update CLAUDE.md"
info "  ndx recall init               # create the recall palace"
info "  ndx recall mine --from-memory # seed from session history"
info "  /ndx-recall-classify          # let Claude organize your drawers"
