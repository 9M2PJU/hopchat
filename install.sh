#!/bin/sh
# hopchat one-liner installer
#
#   curl -fsSL https://raw.githubusercontent.com/bahdotsh/hopchat/main/install.sh | sh
#
# Downloads the latest GitHub release for your OS/arch, checks it against the
# release's SHA256SUMS, and installs the `hopchat` binary to ~/.local/bin
# (default) or /usr/local/bin with --system. Falls back to `cargo install` only
# when the release has no asset for this platform; a checksum mismatch is
# always fatal.
set -eu

REPO="bahdotsh/hopchat"
BIN="hopchat"

usage() {
  cat <<'EOF'
usage: install.sh [--system | --dir PATH] [--tag TAG]

  --system        install to /usr/local/bin (needs root, e.g. via sudo)
  --dir PATH      install directory (default: ~/.local/bin)
  --tag TAG       install a specific release tag (e.g. v0.1.0) instead of latest
  --help          print this help

Piped form:  curl -fsSL .../install.sh | sh -s -- --tag v0.1.0
EOF
}

die() { echo "error: $*" >&2; exit 1; }

INSTALL_DIR=""
SYSTEM=0
TAG=""
while [ $# -gt 0 ]; do
  case "$1" in
    --system) SYSTEM=1; shift ;;
    --dir)
      [ $# -ge 2 ] || die "--dir needs a value"
      INSTALL_DIR="$2"; shift 2 ;;
    --dir=*) INSTALL_DIR="${1#--dir=}"; shift ;;
    --tag)
      [ $# -ge 2 ] || die "--tag needs a value"
      TAG="$2"; shift 2 ;;
    --tag=*) TAG="${1#--tag=}"; shift ;;
    --help|-h) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

if [ "$SYSTEM" -eq 1 ]; then
  [ -z "$INSTALL_DIR" ] || die "--system and --dir are mutually exclusive"
  INSTALL_DIR="/usr/local/bin"
elif [ -z "$INSTALL_DIR" ]; then
  : "${HOME:?HOME must be set}"
  INSTALL_DIR="$HOME/.local/bin"
fi

# The tag ends up in a URL; keep it to the characters real tags use.
case "$TAG" in
  *[!A-Za-z0-9._-]*) die "invalid tag: $TAG" ;;
esac

need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }
need uname
need mktemp
need tar

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1"; }
  fetch_to() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO- "$1"; }
  fetch_to() { wget -qO "$2" "$1"; }
else
  die "need curl or wget"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "need sha256sum or shasum to verify the download"
fi

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) die "unsupported architecture: $ARCH" ;;
esac
case "$OS" in
  linux) TARGET_OS="linux" ;;
  darwin) TARGET_OS="macos" ;;
  *) die "unsupported OS: $OS (Linux and macOS only)" ;;
esac

# Fail before downloading anything if we cannot write the destination.
mkdir -p "$INSTALL_DIR" 2>/dev/null || true
if [ ! -d "$INSTALL_DIR" ] || [ ! -w "$INSTALL_DIR" ]; then
  if [ "$SYSTEM" -eq 1 ]; then
    die "$INSTALL_DIR is not writable; rerun with sudo, e.g. curl -fsSL .../install.sh | sudo sh -s -- --system"
  fi
  die "$INSTALL_DIR is not writable"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
trap 'exit 130' INT TERM

resolve_tag() {
  if [ -n "$TAG" ]; then echo "$TAG"; return; fi
  json="$(fetch "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null)" || return 0
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$json" | jq -r '.tag_name // empty'
  else
    printf '%s\n' "$json" | grep -m1 '"tag_name"' | cut -d'"' -f4
  fi
}

# Puts a verified, runnable binary at $TMP/$BIN.
# Returns 1 only when this release has no asset for this platform, so the
# caller may fall back to a source build. Every other failure exits.
download_release() {
  tag="$1"
  ver="${tag#v}"
  asset="hopchat-${ver}-${TARGET_OS}-${ARCH}.tar.gz"
  base="https://github.com/$REPO/releases/download/$tag"
  echo "Downloading $base/$asset ..."
  fetch_to "$base/$asset" "$TMP/$asset" || return 1
  fetch_to "$base/SHA256SUMS" "$TMP/SHA256SUMS" || die "release $tag has no SHA256SUMS; refusing to install unverified"
  want="$(awk -v f="$asset" '$2 == f || $2 == "*"f { print $1; exit }' "$TMP/SHA256SUMS")"
  [ -n "$want" ] || die "SHA256SUMS for $tag does not list $asset"
  got="$(sha256 "$TMP/$asset")"
  [ "$want" = "$got" ] || die "checksum mismatch for $asset (expected $want, got $got)"
  echo "Checksum OK."
  tar -xzf "$TMP/$asset" -C "$TMP" "$BIN" || die "release archive has no $BIN binary"
  chmod +x "$TMP/$BIN"
}

build_from_source() {
  command -v cargo >/dev/null 2>&1 || die "no release asset for $TARGET_OS/$ARCH and cargo is not installed; see https://github.com/$REPO#installation"
  echo "Building from source with cargo ..."
  if [ -n "$TAG" ]; then
    cargo install --git "https://github.com/$REPO" --tag "$TAG" --locked --root "$TMP/cargo-root"
  else
    cargo install --git "https://github.com/$REPO" --locked --root "$TMP/cargo-root"
  fi
  mv "$TMP/cargo-root/bin/$BIN" "$TMP/$BIN"
}

TAG_RESOLVED="$(resolve_tag)"
if [ -n "$TAG_RESOLVED" ] && [ "$TAG_RESOLVED" != "null" ]; then
  echo "Release: $TAG_RESOLVED"
  if ! download_release "$TAG_RESOLVED"; then
    echo "No $TARGET_OS/$ARCH asset in $TAG_RESOLVED."
    build_from_source
  fi
elif [ -n "$TAG" ]; then
  die "release $TAG not found"
else
  echo "Could not find a published release."
  build_from_source
fi

# Prove it runs here (glibc, OpenSSL, architecture) before replacing anything.
if ! "$TMP/$BIN" --help >/dev/null 2>&1; then
  echo "error: the downloaded $BIN does not run on this system." >&2
  echo "  Linux builds need glibc 2.35+ and OpenSSL 3 (Debian 12, Ubuntu 22.04 or newer)." >&2
  echo "  Otherwise install with: cargo install --git https://github.com/$REPO --locked" >&2
  exit 1
fi

mv -f "$TMP/$BIN" "$INSTALL_DIR/$BIN"
echo ""
echo "Installed $BIN to $INSTALL_DIR/$BIN"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Note: $INSTALL_DIR is not on your PATH. Add it, e.g.:"; echo "  export PATH=\"\$PATH:$INSTALL_DIR\"" ;;
esac
echo "Run it with:  $BIN --name alice"
