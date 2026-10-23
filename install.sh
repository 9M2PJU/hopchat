#!/bin/sh
# hopchat one-liner installer
#
#   curl -fsSL https://raw.githubusercontent.com/bahdotsh/hopchat/main/install.sh | sh
#
# Downloads the latest GitHub release for your OS/arch, checks it against the
# release's SHA256SUMS, and installs the `hopchat` binary to ~/.local/bin
# (default) or /usr/local/bin with --system. Falls back to `cargo install` only
# when there is no release at all, or the release has no asset for this
# platform. A network error, a missing SHA256SUMS or a checksum mismatch is
# always fatal: it never quietly turns into a source build.
#
# Everything runs from main() on the last line, so a download cut short by the
# network executes nothing.
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
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

# fetch_to URL DEST: 0 when downloaded, 2 when the server says 404, 1 for any
# other failure (network, TLS, 5xx, rate limit). Only 2 may lead to a fallback.
if command -v curl >/dev/null 2>&1; then
  fetch_to() {
    code="$(curl -sSL --proto '=https' --tlsv1.2 -o "$2" -w '%{http_code}' "$1")" || return 1
    case "$code" in
      200) return 0 ;;
      404) return 2 ;;
      *) return 1 ;;
    esac
  }
  # Prints where github.com/$REPO/releases/latest redirects to.
  latest_url() {
    curl -sSIL --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' \
      "https://github.com/$REPO/releases/latest" || die "cannot reach github.com"
  }
elif command -v wget >/dev/null 2>&1; then
  fetch_to() {
    if wget -q -S -O "$2" "$1" 2>"$TMP/wget.log"; then return 0; fi
    status="$(awk '$1 ~ /^HTTP\// { s = $2 } END { print s }' "$TMP/wget.log")"
    [ "$status" = 404 ] && return 2
    return 1
  }
  # Busybox wget has no --max-redirect, so follow the redirects and take the
  # last Location header; both GNU and busybox wget print every hop with -S.
  latest_url() {
    hdrs="$(wget -q -S --spider "https://github.com/$REPO/releases/latest" 2>&1)" || true
    printf '%s\n' "$hdrs" | grep -q 'HTTP/' || die "cannot reach github.com"
    printf '%s\n' "$hdrs" | awk 'tolower($1) == "location:" { l = $2 } END { print l }' | tr -d '\r'
  }
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

# The latest release tag, or nothing when the repo has no release yet. Uses the
# web redirect, not api.github.com, which allows 60 requests an hour per IP and
# would make a shared network look like a repo with no releases.
resolve_latest_tag() {
  url="$(latest_url)" || exit 1
  case "$url" in
    */releases/tag/*) printf '%s\n' "${url##*/}" ;;
    *) ;;
  esac
}

# Puts a verified binary at $TMP/$BIN.
# Returns 1 only when the release exists but has no asset for this platform,
# so the caller may fall back to a source build. Every other failure exits.
download_release() {
  tag="$1"
  ver="${tag#v}"
  asset="$BIN-${ver}-${TARGET_OS}-${ARCH}.tar.gz"
  base="https://github.com/$REPO/releases/download/$tag"

  # SHA256SUMS first: every published release has one, so its absence means
  # the tag is not a release, and its contents say which platforms exist.
  rc=0
  fetch_to "$base/SHA256SUMS" "$TMP/SHA256SUMS" || rc=$?
  case "$rc" in
    0) ;;
    2) die "release $tag not found, or it has no SHA256SUMS; refusing to install unverified" ;;
    *) die "could not download $base/SHA256SUMS (network error or GitHub unavailable)" ;;
  esac
  want="$(awk -v f="$asset" '$2 == f || $2 == "*"f { print $1; exit }' "$TMP/SHA256SUMS")"
  [ -n "$want" ] || return 1

  echo "Downloading $base/$asset ..."
  fetch_to "$base/$asset" "$TMP/$asset" || die "could not download $base/$asset"
  got="$(sha256 "$TMP/$asset")"
  [ "$want" = "$got" ] || die "checksum mismatch for $asset (expected $want, got $got)"
  echo "Checksum OK."
  tar -xzf "$TMP/$asset" -C "$TMP" "$BIN" || die "release archive has no $BIN binary"
}

build_from_source() {
  command -v cargo >/dev/null 2>&1 || die "no prebuilt $BIN for $TARGET_OS/$ARCH and cargo is not installed; see https://github.com/$REPO#installation"
  echo "Building from source with cargo ..."
  if [ -n "$1" ]; then
    cargo install --git "https://github.com/$REPO" --tag "$1" --locked --root "$TMP/cargo-root"
  else
    cargo install --git "https://github.com/$REPO" --locked --root "$TMP/cargo-root"
  fi
  mv "$TMP/cargo-root/bin/$BIN" "$TMP/$BIN"
}

main() {
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

  need uname
  need mktemp
  need tar

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
  STAGED=""
  trap 'rm -rf "$TMP"; [ -z "$STAGED" ] || rm -f "$STAGED"' EXIT
  trap 'exit 130' INT TERM

  if [ -z "$TAG" ]; then
    TAG="$(resolve_latest_tag)" || exit 1
  fi
  # The tag ends up in a URL; keep it to the characters real tags use.
  case "$TAG" in
    *[!A-Za-z0-9._-]*) die "invalid tag: $TAG" ;;
  esac

  if [ -z "$TAG" ]; then
    echo "No published release yet."
    build_from_source ""
  else
    echo "Release: $TAG"
    if ! download_release "$TAG"; then
      echo "Release $TAG has no $TARGET_OS/$ARCH build."
      build_from_source "$TAG"
    fi
  fi

  # Stage next to the destination, not in $TMP: /tmp may be mounted noexec
  # (the check below would then blame glibc), and a rename within one
  # filesystem is atomic, so an interrupted install never leaves half a binary.
  STAGED="$INSTALL_DIR/.$BIN.new.$$"
  cp "$TMP/$BIN" "$STAGED"
  chmod 755 "$STAGED"

  # Prove it runs here (glibc, OpenSSL, architecture) before replacing anything.
  if ! "$STAGED" --help >/dev/null 2>&1; then
    echo "error: the downloaded $BIN does not run on this system." >&2
    echo "  Linux builds need glibc 2.35+ and OpenSSL 3 (Debian 12, Ubuntu 22.04 or newer)." >&2
    echo "  Otherwise install with: cargo install --git https://github.com/$REPO --locked" >&2
    exit 1
  fi

  mv -f "$STAGED" "$INSTALL_DIR/$BIN"
  STAGED=""
  echo ""
  echo "Installed $BIN to $INSTALL_DIR/$BIN"
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "Note: $INSTALL_DIR is not on your PATH. Add it, e.g.:"; echo "  export PATH=\"\$PATH:$INSTALL_DIR\"" ;;
  esac
  echo "Run it with:  $BIN --name alice"
}

main "$@"
