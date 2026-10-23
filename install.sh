#!/bin/sh
# hopchat one-liner installer
#
#   curl -fsSL https://raw.githubusercontent.com/bahdotsh/hopchat/main/install.sh | sh
#
# Downloads the latest GitHub release for your OS/arch and installs the
# `hopchat` binary to ~/.local/bin (default) or /usr/local/bin with --system.
# Falls back to `cargo install --git` when no release asset matches.
#
# Options:
#   --system        install to /usr/local/bin (needs root)
#   --dir PATH      install directory (default: ~/.local/bin)
#   --tag TAG       install a specific release tag instead of latest
#   --help          print this help
set -eu

REPO="bahdotsh/hopchat"
BIN="hopchat"
: "${HOME:?HOME must be set}"
DEFAULT_DIR="$HOME/.local/bin"
SYSTEM_DIR="/usr/local/bin"

INSTALL_DIR="$DEFAULT_DIR"
TAG=""
while [ $# -gt 0 ]; do
  case "$1" in
    --system) INSTALL_DIR="$SYSTEM_DIR"; shift ;;
    --dir)
      [ $# -ge 2 ] || { echo "error: --dir needs a value" >&2; exit 1; }
      INSTALL_DIR="$2"; shift 2 ;;
    --dir=*) INSTALL_DIR="${1#--dir=}"; shift ;;
    --tag)
      [ $# -ge 2 ] || { echo "error: --tag needs a value" >&2; exit 1; }
      TAG="$2"; shift 2 ;;
    --tag=*) TAG="${1#--tag=}"; shift ;;
    --help|-h) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 1 ;;
  esac
done

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: missing required tool: $1" >&2; exit 1; }; }
need uname
need mktemp
if command -v curl >/dev/null 2>&1; then FETCH="curl -fsSL"; elif command -v wget >/dev/null 2>&1; then FETCH="wget -qO-"; else echo "error: need curl or wget" >&2; exit 1; fi

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) echo "error: unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
case "$OS" in
  linux) TARGET_OS="linux" ;;
  darwin) TARGET_OS="macos" ;;
  *) echo "error: unsupported OS: $OS (Linux and macOS only)" >&2; exit 1 ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

github_api() { $FETCH "https://api.github.com/repos/$REPO/releases/${1:-latest}"; }

resolve_tag() {
  if [ -n "$TAG" ]; then echo "$TAG"; return; fi
  if command -v jq >/dev/null 2>&1; then
    github_api latest 2>/dev/null | jq -r '.tag_name // empty'
  else
    github_api latest 2>/dev/null | grep -m1 '"tag_name"' | cut -d'"' -f4
  fi
}

install_from_release() {
  tag="$1"
  ver="${tag#v}"
  # Asset naming from release.yml: hopchat-<ver>-<os>-<arch>.tar.gz
  # plus .deb / .rpm / .AppImage for Linux x86_64.
  asset="hopchat-${ver}-${TARGET_OS}-${ARCH}.tar.gz"
  url="https://github.com/$REPO/releases/download/$tag/$asset"
  echo "Downloading $url ..."
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$TMP/hopchat.tar.gz" "$url" || return 1
  else
    wget -qO "$TMP/hopchat.tar.gz" "$url" || return 1
  fi
  need tar
  tar -xzf "$TMP/hopchat.tar.gz" -C "$TMP"
  if [ ! -f "$TMP/$BIN" ]; then echo "error: release archive has no $BIN binary" >&2; exit 1; fi
  chmod +x "$TMP/$BIN"
  mkdir -p "$INSTALL_DIR"
  mv "$TMP/$BIN" "$INSTALL_DIR/$BIN"
}

install_from_source() {
  need cargo
  echo "No matching release asset; building from source with cargo ..."
  cargo install --git "https://github.com/$REPO" --locked --root "$TMP/cargo-root"
  mkdir -p "$INSTALL_DIR"
  mv "$TMP/cargo-root/bin/$BIN" "$INSTALL_DIR/$BIN"
}

TAG_RESOLVED="$(resolve_tag || true)"
if [ -n "${TAG_RESOLVED:-}" ] && [ "$TAG_RESOLVED" != "null" ]; then
  echo "Latest release: $TAG_RESOLVED"
  if ! install_from_release "$TAG_RESOLVED"; then
    echo "Release download failed, falling back to source build."
    install_from_source
  fi
else
  echo "Could not determine latest release (no releases yet?); building from source."
  install_from_source
fi

"$INSTALL_DIR/$BIN" --help >/dev/null 2>&1 || true
echo ""
echo "Installed $BIN to $INSTALL_DIR/$BIN"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Note: $INSTALL_DIR is not on your PATH. Add it, e.g.:"; echo "  export PATH=\"\$PATH:$INSTALL_DIR\"";;
esac
echo "Run it with:  $BIN --name alice"
