#!/bin/bash
set -e

REPO="longzhi/clawhive"
INSTALL_DIR="$HOME/.clawhive/bin"

# Detect OS and architecture
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS-$ARCH" in
    Darwin-arm64|Darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
    Darwin-x86_64)               TARGET="x86_64-apple-darwin" ;;
    Linux-x86_64)                TARGET="x86_64-unknown-linux-musl" ;;
    Linux-aarch64)               TARGET="aarch64-unknown-linux-musl" ;;
    *) echo "Unsupported platform: $OS $ARCH"; exit 1 ;;
esac

# Get latest version
VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | cut -d'"' -f4)
if [ -z "$VERSION" ]; then
    echo "Failed to fetch latest version"
    exit 1
fi

echo "Installing clawhive ${VERSION} for ${TARGET}..."

# Create install directory
mkdir -p "$INSTALL_DIR"

# Download and extract
TARBALL="clawhive-${VERSION}-${TARGET}.tar.gz"
curl -fsSL "https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}" -o "/tmp/${TARBALL}"
TMPDIR=$(mktemp -d)
tar -xzf "/tmp/${TARBALL}" -C "$TMPDIR"

# Install binary
mv "$TMPDIR/clawhive" "$INSTALL_DIR/clawhive"
chmod +x "$INSTALL_DIR/clawhive"

# Install skills (skip if already exists to preserve customizations)
CLAWHIVE_HOME="$HOME/.clawhive"
if [ ! -d "$CLAWHIVE_HOME/skills" ]; then
    cp -r "$TMPDIR/skills" "$CLAWHIVE_HOME/skills"
    echo "Installed skills to $CLAWHIVE_HOME/skills"
else
    echo "Skills already exists, skipping (use --force to overwrite)"
fi

# Cleanup
rm -rf "$TMPDIR" "/tmp/${TARBALL}"

# Add to PATH if not already present
add_to_path() {
    local rc_file="$1"
    if [ -f "$rc_file" ] && grep -q '\.clawhive/bin' "$rc_file" 2>/dev/null; then
        return
    fi
    if [ -f "$rc_file" ] || [ "$rc_file" = "$HOME/.profile" ]; then
        echo '' >> "$rc_file"
        echo '# clawhive' >> "$rc_file"
        echo 'export PATH="$HOME/.clawhive/bin:$PATH"' >> "$rc_file"
        echo "Added to PATH in $rc_file"
    fi
}

if ! echo "$PATH" | grep -q '\.clawhive/bin'; then
    SHELL_NAME=$(basename "$SHELL")
    case "$SHELL_NAME" in
        zsh)  add_to_path "$HOME/.zshrc" ;;
        bash)
            if [ -f "$HOME/.bashrc" ]; then
                add_to_path "$HOME/.bashrc"
            else
                add_to_path "$HOME/.profile"
            fi
            ;;
        *)    add_to_path "$HOME/.profile" ;;
    esac
    export PATH="$INSTALL_DIR:$PATH"
fi

echo "clawhive ${VERSION} installed to ${INSTALL_DIR}/clawhive"
clawhive --version 2>/dev/null || true
echo ""
echo "Run 'clawhive setup' to configure providers, agents, and channels."
