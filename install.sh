#!/bin/sh
set -e

REPO="shivships/remux-cli"
INSTALL_DIR="${REMUX_INSTALL_DIR:-$HOME/.local/bin}"

main() {
    os=$(uname -s)
    arch=$(uname -m)

    case "$os" in
        Linux)  os_target="unknown-linux-gnu" ;;
        Darwin) os_target="apple-darwin" ;;
        *)      echo "Error: unsupported OS: $os (Linux and macOS only; on Windows use WSL)" >&2; exit 1 ;;
    esac

    case "$arch" in
        x86_64|amd64)  arch_target="x86_64" ;;
        aarch64|arm64) arch_target="aarch64" ;;
        *)             echo "Error: unsupported architecture: $arch" >&2; exit 1 ;;
    esac

    target="${arch_target}-${os_target}"
    artifact="remux-cli-${target}.tar.gz"
    url="https://github.com/${REPO}/releases/latest/download/${artifact}"

    echo "Installing remux-cli (${target})..."

    tmpdir=$(mktemp -d)
    trap 'rm -rf "$tmpdir"' EXIT

    echo "Downloading ${url}..."
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$url" -o "$tmpdir/$artifact"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$url" -O "$tmpdir/$artifact"
    else
        echo "Error: curl or wget is required" >&2
        exit 1
    fi

    tar xzf "$tmpdir/$artifact" -C "$tmpdir"

    mkdir -p "$INSTALL_DIR"
    mv "$tmpdir/remux-cli" "$INSTALL_DIR/remux-cli"
    chmod +x "$INSTALL_DIR/remux-cli"

    echo "Installed remux-cli to ${INSTALL_DIR}/remux-cli"

    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        echo ""
        echo "Add to your PATH if not already present:"
        echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi
}

main
