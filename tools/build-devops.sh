#!/usr/bin/env bash
# Tawny DevOps desktop build: the AppImage, renamed and signalling over Nostr
# only, through the relay + tap in Tawny DevOps/nostr-signaling-demo.
#   tools/build-devops.sh   (run on the build machine, after tools/sync-web.sh)
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export TAWNY_APP_ID="io.github.PressF4me.Tawny.DevOps"
export TAWNY_APP_NAME="Tawny DevOps"
export TAWNY_NOSTR_RELAYS="${TAWNY_NOSTR_RELAYS:-ws://127.0.0.1:7447/relay}"
export TAWNY_NOSTR_MODE=only
export TAWNY_NOSTR_MIRROR="${TAWNY_NOSTR_MIRROR:-http://127.0.0.1:7447/mirror}"
export TAWNY_TAURI_CONFIG="$HERE/src-tauri/tauri.devops.conf.json"
exec "$HERE/tools/build-appimage.sh"
