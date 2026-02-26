#!/bin/bash
set -e

REPO="longzhi/clawhive"
INSTALL_DIR="/usr/local/bin"

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
    x86_64) TARGET="x86_64-apple-darwin" ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

# Get latest version
VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | cut -d'"' -f4)
if [ -z "$VERSION" ]; then
    echo "Failed to fetch latest version"
    exit 1
fi

echo "Installing clawhive ${VERSION} for ${TARGET}..."

# Download and extract
TARBALL="clawhive-${VERSION}-${TARGET}.tar.gz"
curl -fsSL "https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}" -o "/tmp/${TARBALL}"
tar -xzf "/tmp/${TARBALL}" -C /tmp

# Install
sudo mv /tmp/clawhive "${INSTALL_DIR}/clawhive"
sudo chmod +x "${INSTALL_DIR}/clawhive"

# Cleanup
rm -f "/tmp/${TARBALL}"

echo "✅ clawhive ${VERSION} installed to ${INSTALL_DIR}/clawhive"
clawhive --version 2>/dev/null || true
