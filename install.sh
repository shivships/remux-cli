#!/bin/sh
set -e

REPO="shivships/remux-cli"
INSTALL_DIR="${REMUX_INSTALL_DIR:-$HOME/.remux/bin}"

# --- Colors (only when outputting to a terminal) ---

if [ -t 1 ]; then
    BOLD='\033[1m'
    DIM='\033[2m'
    GREEN='\033[32m'
    RED='\033[31m'
    RESET='\033[0m'
else
    BOLD='' DIM='' GREEN='' RED='' RESET=''
fi

info()  { printf "${DIM}%s${RESET}\n" "$1"; }
success() { printf "${GREEN}%s${RESET}\n" "$1"; }
error() { printf "${RED}error${RESET}: %s\n" "$1" >&2; exit 1; }

tildify() {
    case "$1" in
        $HOME/*) printf "~/%s" "${1#$HOME/}" ;;
        *)       printf "%s" "$1" ;;
    esac
}

# --- Platform detection ---

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)

    case "$os" in
        Linux)  os_target="unknown-linux-gnu" ;;
        Darwin) os_target="apple-darwin" ;;
        *)      error "unsupported OS: $os (Linux and macOS only; on Windows use WSL)" ;;
    esac

    case "$arch" in
        x86_64|amd64)  arch_target="x86_64" ;;
        aarch64|arm64) arch_target="aarch64" ;;
        *)             error "unsupported architecture: $arch" ;;
    esac

    printf "%s-%s" "$arch_target" "$os_target"
}

# --- Download ---

download() {
    url="$1"
    output="$2"

    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --progress-bar "$url" -o "$output"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --show-progress "$url" -O "$output"
    else
        error "curl or wget is required to download remux"
    fi
}

# --- PATH setup ---

setup_path() {
    # Already in PATH — nothing to do
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) return 0 ;;
    esac

    line="export PATH=\"$INSTALL_DIR:\$PATH\""
    shell_name=$(basename "${SHELL:-sh}")

    case "$shell_name" in
        zsh)
            rc="$HOME/.zshrc"
            ;;
        bash)
            # Prefer .bashrc, fall back to .bash_profile
            if [ -f "$HOME/.bashrc" ]; then
                rc="$HOME/.bashrc"
            else
                rc="$HOME/.bash_profile"
            fi
            ;;
        fish)
            # fish uses a different syntax
            fish_conf="$HOME/.config/fish/config.fish"
            if ! grep -qF "$INSTALL_DIR" "$fish_conf" 2>/dev/null; then
                mkdir -p "$(dirname "$fish_conf")"
                printf '\n# remux\nset -gx PATH "%s" $PATH\n' "$INSTALL_DIR" >> "$fish_conf"
            fi
            return 0
            ;;
        *)
            rc="$HOME/.profile"
            ;;
    esac

    # Don't duplicate if already present
    if grep -qF "$INSTALL_DIR" "$rc" 2>/dev/null; then
        return 0
    fi

    printf '\n# remux\n%s\n' "$line" >> "$rc"
}

# --- Main ---

main() {
    target=$(detect_target)
    artifact="remux-cli-${target}.tar.gz"
    url="https://github.com/${REPO}/releases/latest/download/${artifact}"

    printf "\n"
    info "Installing remux (${target})"
    printf "\n"

    tmpdir=$(mktemp -d)
    trap 'rm -rf "$tmpdir"' EXIT

    download "$url" "$tmpdir/$artifact"

    tar xzf "$tmpdir/$artifact" -C "$tmpdir"

    mkdir -p "$INSTALL_DIR"
    mv "$tmpdir/remux-cli" "$INSTALL_DIR/remux"
    chmod +x "$INSTALL_DIR/remux"

    setup_path

    printf "\n"
    success "remux was installed successfully to $(tildify "$INSTALL_DIR/remux")"

    # If not yet on PATH, tell the user how to activate now
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) ;;
        *)
            info "To get started, run:"
            printf "\n"
            printf "  ${BOLD}exec $SHELL${RESET}\n"
            printf "\n"
            ;;
    esac
}

main
