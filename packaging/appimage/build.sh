#!/bin/sh
# Build a hopchat AppImage (x86_64/aarch64 Linux).
# Usage: sh packaging/appimage/build.sh [--output DIR]
# Requires: cargo, appimagetool (or linuxdeploy) in PATH.
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/dist"
while [ $# -gt 0 ]; do
  case "$1" in
    --output)
      [ $# -ge 2 ] || { echo "usage: $0 [--output DIR]" >&2; exit 1; }
      OUT="$2"; shift 2 ;;
    --output=*) OUT="${1#--output=}"; shift ;;
    *) echo "usage: $0 [--output DIR]" >&2; exit 1 ;;
  esac
done

VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
ARCH="$(uname -m)"

echo "==> building hopchat v$VERSION (release) ..."
cargo build --release --locked --manifest-path "$ROOT/Cargo.toml"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT INT TERM
APPDIR="$WORK/HopChat.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/scalable/apps" \
  "$APPDIR/usr/share/man/man1"

cp "$ROOT/target/release/hopchat" "$APPDIR/usr/bin/hopchat"
cp "$ROOT/packaging/hopchat.desktop" "$APPDIR/hopchat.desktop"
cp "$ROOT/packaging/hopchat.desktop" "$APPDIR/usr/share/applications/hopchat.desktop"
cp "$ROOT/packaging/hopchat.svg" "$APPDIR/hopchat.svg"
cp "$ROOT/packaging/hopchat.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/hopchat.svg"
cp "$ROOT/packaging/hopchat.1" "$APPDIR/usr/share/man/man1/hopchat.1"

# AppRun: prefer the bundled binary, keep Arches' portable HOME semantics.
cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/hopchat" "$@"
EOF
chmod +x "$APPDIR/AppRun"

mkdir -p "$OUT"
ARCH_OUT="$ARCH"
APPIMAGE="$OUT/hopchat-$VERSION-linux-$ARCH_OUT.AppImage"
rm -f "$APPIMAGE"

if command -v appimagetool >/dev/null 2>&1; then
  echo "==> running appimagetool ..."
  ARCH="$ARCH_OUT" appimagetool "$APPDIR" "$APPIMAGE"
elif command -v linuxdeploy >/dev/null 2>&1; then
  echo "==> running linuxdeploy ..."
  linuxdeploy --appdir "$APPDIR" --output appimage
  mv HopChat*.AppImage "$APPIMAGE" 2>/dev/null || mv ./*.AppImage "$APPIMAGE"
else
  echo "error: need appimagetool or linuxdeploy in PATH" >&2
  echo "  download: https://github.com/AppImage/appimagetool/releases" >&2
  exit 1
fi

chmod +x "$APPIMAGE"
echo "Built: $APPIMAGE"
ls -lh "$APPIMAGE"
