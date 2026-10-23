#!/bin/sh
# Build a hopchat AppImage (x86_64/aarch64 Linux).
# Usage: sh packaging/appimage/build.sh [--binary PATH] [--output DIR]
#   --binary PATH  package this prebuilt binary (default: cargo build --release)
#   --output DIR   where the AppImage goes (default: dist/)
#   VERSION=x.y.z  version for the file name (default: read from Cargo.toml)
# Requires appimagetool (or linuxdeploy) in PATH. Set APPIMAGE_EXTRACT_AND_RUN=1
# where FUSE is unavailable, e.g. in containers and CI.
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/dist"
BINARY=""
usage() { echo "usage: $0 [--binary PATH] [--output DIR]" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --output) [ $# -ge 2 ] || usage; OUT="$2"; shift 2 ;;
    --output=*) OUT="${1#--output=}"; shift ;;
    --binary) [ $# -ge 2 ] || usage; BINARY="$2"; shift 2 ;;
    --binary=*) BINARY="${1#--binary=}"; shift ;;
    *) usage ;;
  esac
done

# CI passes VERSION from `cargo metadata`; the grep is only for local runs.
VERSION="${VERSION:-$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)}"
ARCH="$(uname -m)"

if [ -z "$BINARY" ]; then
  echo "==> building hopchat v$VERSION (release) ..."
  cargo build --release --locked --manifest-path "$ROOT/Cargo.toml"
  BINARY="$ROOT/target/release/hopchat"
fi
[ -x "$BINARY" ] || { echo "error: no executable at $BINARY" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
trap 'exit 130' INT TERM
APPDIR="$WORK/HopChat.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/scalable/apps" \
  "$APPDIR/usr/share/man/man1"

cp "$BINARY" "$APPDIR/usr/bin/hopchat"
cp "$ROOT/packaging/hopchat.desktop" "$APPDIR/hopchat.desktop"
cp "$ROOT/packaging/hopchat.desktop" "$APPDIR/usr/share/applications/hopchat.desktop"
cp "$ROOT/packaging/hopchat.svg" "$APPDIR/hopchat.svg"
cp "$ROOT/packaging/hopchat.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/hopchat.svg"
cp "$ROOT/packaging/hopchat.1" "$APPDIR/usr/share/man/man1/hopchat.1"

cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/hopchat" "$@"
EOF
chmod +x "$APPDIR/AppRun"

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
APPIMAGE="$OUT/hopchat-$VERSION-linux-$ARCH.AppImage"
rm -f "$APPIMAGE"

if command -v appimagetool >/dev/null 2>&1; then
  echo "==> running appimagetool ..."
  ARCH="$ARCH" appimagetool "$APPDIR" "$APPIMAGE"
elif command -v linuxdeploy >/dev/null 2>&1; then
  echo "==> running linuxdeploy ..."
  (cd "$WORK" && linuxdeploy --appdir "$APPDIR" --output appimage)
  mv "$WORK"/*.AppImage "$APPIMAGE"
else
  echo "error: need appimagetool or linuxdeploy in PATH" >&2
  echo "  download: https://github.com/AppImage/appimagetool/releases" >&2
  exit 1
fi

chmod +x "$APPIMAGE"
echo "Built: $APPIMAGE"
ls -lh "$APPIMAGE"
