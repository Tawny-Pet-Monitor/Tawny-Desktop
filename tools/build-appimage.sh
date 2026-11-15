#!/usr/bin/env bash
# Build the AppImage to dist/. Needs Rust and the Tauri CLI that matches the
# pinned crates:
#   cargo install tauri-cli --version '=3.0.0-alpha.1' --locked

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Tauri's AppImage bundler runs quick-sharun, which fetches anylinux.c from a
# path upstream removed on 2026-09-10 (pkgforge-dev/Anylinux-AppImages#866).
# Pin it to the last commit that has it until the bundler catches up.
export ANYLINUX_LIB_SOURCE="${ANYLINUX_LIB_SOURCE:-https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/df9cd3246ccbcf61eb22fc321a52277351047b4b/useful-tools/lib/anylinux.c}"

command -v patchelf >/dev/null || { echo "patchelf is needed (pacman -S patchelf / apt install patchelf)" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is needed (pacman -S jq / apt install jq)" >&2; exit 1; }

# quick-sharun's DEPLOY_CHROMIUM path bundles part of NSS (libnss3, libnssutil3,
# libsmime3, libnspr4, libplc4, libplds4) but not the modules NSS loads at
# runtime with dlopen(): libsoftokn3.so and libfreebl3.so / libfreeblpriv3.so.
# ldd-based discovery never sees a dlopen()ed library, so those get left out,
# and the bundled libnssutil3 then gets loaded against the *host's*
# libsoftokn3 at runtime. On any host with a newer NSS than the partial copy
# baked in, that fails a symbol-version check and CEF aborts on startup
# (nss_util.cc FATAL, NSSUTIL_3.10x not found). See
# ~/send to cachy/Tawny-AppImage-NSS-fix-PR.md for the full writeup.
#
# Also missing for the same reason: libnssckbi.so, NSS's dlopen()ed built-in
# root-CA-certs module. Without it, "After loading Root Certs, loaded==false"
# gets logged on hosts that don't happen to have it either (non-fatal, but
# means cert-dependent features have no bundled trust store to fall back on).
#
# Fix: find the dlopen-only modules on the build host and hand them to
# quick-sharun explicitly via tauri's `bundle.linux.appimage.files` (any
# target under /usr/lib is added to quick-sharun's explicit deploy list, per
# tauri-bundler's sharun_cef.rs). Paths differ by distro (Debian/Ubuntu:
# /usr/lib/x86_64-linux-gnu/, Arch: /usr/lib/, Fedora: /usr/lib64/), so this
# is discovered at build time rather than hardcoded in tauri.conf.json.
#
# libpulse.so.0 is in the same position. Chromium dlopen()s PulseAudio for the
# microphone, so nothing discovers it and the AppImage used to load the HOST's
# copy, against the bundled (older) glibc. On any system newer than the build
# machine that chain fails (CachyOS 2026-11: libmp3lame needs GLIBC_2.43), the
# microphone cannot start, and getUserMedia({audio, video}) rejects the whole
# request with NotReadableError, so the Monitor could not open the camera
# either. Bundling the build machine's libpulse (quick-sharun pulls in its
# dependencies) talks to any PulseAudio or PipeWire server: the protocol is
# stable, the library is what has to match.
#
# The distro's own library folders are searched first. On Debian/Ubuntu,
# plain /usr/lib also holds Firefox's and Thunderbird's private NSS copies
# (/usr/lib/firefox/...), which link libmozsqlite3 and a newer NSSUTIL than the
# system has; picking one made quick-sharun abort with "libsoftokn3.so is
# missing libraries".
nss_search_dirs=()
for d in /usr/lib/x86_64-linux-gnu /usr/lib/aarch64-linux-gnu /usr/lib64 /usr/lib; do
  [ -d "$d" ] && nss_search_dirs+=("$d")
done

nss_extra_config=""
nss_config_entries=()
for lib in libsoftokn3.so libfreebl3.so libfreeblpriv3.so libnssckbi.so libpulse.so.0; do
  path="$(find "${nss_search_dirs[@]}" -maxdepth 2 -name "$lib" \
    -not -path '*/firefox*/*' -not -path '*/thunderbird*/*' 2>/dev/null | head -1)"
  if [ -n "$path" ]; then
    nss_config_entries+=("--arg" "/usr/lib/$lib" "$path")
    # Bundle the matching .chk (FIPS checksum) file too, if the host has one.
    chk="${path%.so}.chk"
    if [ -f "$chk" ]; then
      nss_config_entries+=("--arg" "/usr/lib/$(basename "$chk")" "$chk")
    fi
  elif [ "$lib" = libpulse.so.0 ]; then
    # Not a warning: without it the AppImage cannot record sound on newer distros.
    echo "error: libpulse.so.0 not found on this host (install libpulse0); the AppImage would have no microphone on newer distros" >&2
    exit 1
  else
    echo "warning: $lib not found on this host; AppImage may crash on hosts with a newer NSS than whatever gets partially bundled" >&2
  fi
done

if [ "${#nss_config_entries[@]}" -gt 0 ]; then
  files_json="$(jq -n "${nss_config_entries[@]}" '$ARGS.named')"
  nss_extra_config="$(jq -n --argjson files "$files_json" '{bundle:{linux:{appimage:{files:$files}}}}')"
fi

cd "$HERE/src-tauri"
# TAWNY_TAURI_CONFIG: an extra config merged on top (Tawny DevOps uses
# tauri.devops.conf.json for its own name, binary and identifier).
extra=()
[ -n "${TAWNY_TAURI_CONFIG:-}" ] && extra=(--config "$TAWNY_TAURI_CONFIG")
if [ -n "$nss_extra_config" ]; then
  cargo tauri build --bundles appimage --config "$nss_extra_config" "${extra[@]}"
else
  cargo tauri build --bundles appimage "${extra[@]}"
fi

mkdir -p "$HERE/dist"
cp target/release/bundle/appimage/*.AppImage "$HERE/dist/"
ls -1 "$HERE"/dist/*.AppImage
