#!/usr/bin/env bash
set -euo pipefail

# Puts the appstore build service inside a built Apparition.app (Contents/Resources/service),
# so the desktop app can run it as its child on a Mac (desktop/src/host.rs). Copies what the
# appstore checkout has committed at HEAD (never uncommitted edits, never its config or data):
# server.js, lib/, public/, bin/ and the license. Run before the app is signed.
#
# Usage: scripts/bundle-service.sh <path/to/Apparition.app>
#        APPARITION_SERVICE_DIR overrides the checkout (default: ../appstore beside this repo).

app="${1:?usage: bundle-service.sh <Apparition.app>}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_dir="${APPARITION_SERVICE_DIR:-$ROOT/../appstore}"
[[ -d "$app/Contents" ]] || { echo "✗ Not an app bundle: $app" >&2; exit 1; }
[[ -f "$source_dir/server.js" ]] || { echo "✗ No appstore checkout at $source_dir (set APPARITION_SERVICE_DIR)" >&2; exit 1; }

dest="$app/Contents/Resources/service"
rm -rf "$dest"
mkdir -p "$dest"
git -C "$source_dir" archive --format=tar HEAD server.js lib public bin LICENSE | tar -x -C "$dest"
git -C "$source_dir" rev-parse --short HEAD >"$dest/REVISION"
echo "▶ Bundled the build service (appstore $(cat "$dest/REVISION")) into $(basename "$app")"
