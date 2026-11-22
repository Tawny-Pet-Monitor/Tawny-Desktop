#!/usr/bin/env bash
# Build the Flatpak and export a single-file bundle to dist/. Run it on the
# build machine, from desktop-native/ with core/ beside it.
#
#   tools/build-flatpak.sh            build and bundle
#   tools/build-flatpak.sh --install  also install it for this user
#
# Needs: flatpak, the Flathub remote, and org.flatpak.Builder
#   flatpak install --user flathub org.flatpak.Builder

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP=io.github.PressF4me.Tawny
MANIFEST="$HERE/packaging/flatpak/$APP.yml"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$HERE/Cargo.toml" | head -1)"

INSTALL=()
[ "${1:-}" = "--install" ] && INSTALL=(--user --install)

cd "$HERE"
flatpak run org.flatpak.Builder --force-clean --install-deps-from=flathub --user \
  --repo=repo "${INSTALL[@]}" build-dir "$MANIFEST"

mkdir -p dist
flatpak build-bundle repo "dist/Tawny-$VERSION-x86_64.flatpak" "$APP" \
  --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo
echo "dist/Tawny-$VERSION-x86_64.flatpak"
