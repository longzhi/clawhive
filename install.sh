#!/usr/bin/env bash
#
# clawhive installer
# Usage: curl -fsSL https://raw.githubusercontent.com/longzhi/clawhive/main/install.sh | bash
#
# Environment variables:
#   CLAWHIVE_VERSION  - specific version to install (default: latest)
#   CLAWHIVE_INSTALL  - installation directory (default: ~/.local/bin)
#

set -euo pipefail

# --- Configuration ---
REPO="longzhi/clawhive"
BINARY_NAME="clawhive"
DEFAULT_INSTALL_DIR="$HOME/.local/bin"

# --- Colors ---
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# --- Helper functions ---
info() { echo -e "${BLUE}==>${NC} $1"; }
success() { echo -e "${GREEN}==>${NC} $1"; }
warn() { echo -e "${YELLOW}warning:${NC} $1"; }
error() { echo -e "${RED}error:${NC} $1" >&2; exit 1; }

# --- Detect platform ---
detect_platform() {
    local os arch

    case "$(uname -s)" in
        Linux*)  os="unknown-linux-gnu" ;;
        Darwin*) os="apple-darwin" ;;
        MINGW*|MSYS*|CYGWIN*) os="pc-windows-msvc" ;;
        *) error "Unsupported operating system: $(uname -s)" ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64)  arch="x86_64" ;;
        arm64|aarch64) arch="aarch64" ;;
        *) error "Unsupported architecture: $(uname -m)" ;;
    esac

    echo "${arch}-${os}"
}

# --- Get latest version ---
get_latest_version() {
    local api_url="https://api.github.com/repos/${REPO}/releases/latest"
    
    if command -v curl &> /dev/null; then
        curl -fsSL "$api_url" | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/'
    elif command -v wget &> /dev/null; then
        wget -qO- "$api_url" | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/'
    else
        error "Neither curl nor wget found. Please install one of them."
    fi
}

# --- Download file ---
download() {
    local url="$1"
    local output="$2"

    info "Downloading from $url"

    if command -v curl &> /dev/null; then
        curl -fsSL "$url" -o "$output"
    elif command -v wget &> /dev/null; then
        wget -q "$url" -O "$output"
    else
        error "Neither curl nor wget found."
    fi
}

# --- Main ---
main() {
    info "Installing ${BINARY_NAME}..."

    # Detect platform
    local platform
    platform=$(detect_platform)
    info "Detected platform: $platform"

    # Get version
    local version="${CLAWHIVE_VERSION:-}"
    if [[ -z "$version" ]]; then
        info "Fetching latest version..."
        version=$(get_latest_version)
        if [[ -z "$version" ]]; then
            error "Could not determine latest version. Please set CLAWHIVE_VERSION."
        fi
    fi
    info "Version: $version"

    # Prepare paths
    local install_dir="${CLAWHIVE_INSTALL:-$DEFAULT_INSTALL_DIR}"
    local archive_name="${BINARY_NAME}-${version}-${platform}.tar.gz"
    local download_url="https://github.com/${REPO}/releases/download/${version}/${archive_name}"
    local tmp_dir
    tmp_dir=$(mktemp -d)
    trap "rm -rf '$tmp_dir'" EXIT

    # Download
    local archive_path="${tmp_dir}/${archive_name}"
    download "$download_url" "$archive_path"

    # Extract
    info "Extracting..."
    tar -xzf "$archive_path" -C "$tmp_dir"

    # Find binary (handle different archive structures)
    local binary_path
    if [[ -f "${tmp_dir}/${BINARY_NAME}" ]]; then
        binary_path="${tmp_dir}/${BINARY_NAME}"
    elif [[ -f "${tmp_dir}/${BINARY_NAME}-${version}-${platform}/${BINARY_NAME}" ]]; then
        binary_path="${tmp_dir}/${BINARY_NAME}-${version}-${platform}/${BINARY_NAME}"
    else
        # Search for it
        binary_path=$(find "$tmp_dir" -name "$BINARY_NAME" -type f | head -1)
        if [[ -z "$binary_path" ]]; then
            error "Could not find ${BINARY_NAME} binary in archive"
        fi
    fi

    # Install
    mkdir -p "$install_dir"
    chmod +x "$binary_path"
    mv "$binary_path" "${install_dir}/${BINARY_NAME}"
    success "Installed ${BINARY_NAME} to ${install_dir}/${BINARY_NAME}"

    # Check PATH
    if [[ ":$PATH:" != *":${install_dir}:"* ]]; then
        warn "${install_dir} is not in your PATH"
        echo ""
        echo "Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
        echo ""
        echo "    export PATH=\"${install_dir}:\$PATH\""
        echo ""
    fi

    # Verify installation
    if command -v "$BINARY_NAME" &> /dev/null; then
        success "Installation complete!"
        echo ""
        "$BINARY_NAME" --version 2>/dev/null || true
    else
        success "Installation complete!"
        echo ""
        echo "Run '${BINARY_NAME} --help' to get started."
    fi

    # Quick start hint
    echo ""
    info "Quick start:"
    echo "    ${BINARY_NAME} setup          # Interactive setup"
    echo "    ${BINARY_NAME} chat           # Local chat mode"
    echo "    ${BINARY_NAME} start          # Start the daemon"
    echo ""
}

main "$@"
