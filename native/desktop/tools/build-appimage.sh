#!/usr/bin/env bash
# Builds the AppImage to dist/: the release binary, its desktop entry and
# icon, in an AppDir packed by appimagetool. The binary needs only glibc,
# ALSA and fontconfig from the system (every desktop Linux has them), so
# nothing else is bundled.
#   APPIMAGETOOL=/path/to/appimagetool tools/build-appimage.sh
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOL="${APPIMAGETOOL:-appimagetool}"
command -v "$TOOL" >/dev/null || { echo "appimagetool is needed (set APPIMAGETOOL)" >&2; exit 1; }
cd "$HERE"
cargo build --release
VERSION="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"tawny-desktop","version":"\([^"]*\)".*/\1/p')"
APPDIR="$(mktemp -d)/Tawny.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/scalable/apps"
install -m755 target/release/tawny "$APPDIR/usr/bin/tawny"
install -m644 packaging/io.github.PressF4me.Tawny.desktop "$APPDIR/usr/share/applications/"
install -m644 packaging/io.github.PressF4me.Tawny.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/"
# appimagetool wants the entry and the icon at the top too.
cp packaging/io.github.PressF4me.Tawny.desktop packaging/io.github.PressF4me.Tawny.svg "$APPDIR/"
cat > "$APPDIR/AppRun" <<'RUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/tawny" "$@"
RUN
chmod 755 "$APPDIR/AppRun"
mkdir -p dist
ARCH=x86_64 "$TOOL" --no-appstream "$APPDIR" "dist/Tawny-${VERSION}-x86_64.AppImage"
echo "dist/Tawny-${VERSION}-x86_64.AppImage"
