#!/usr/bin/env bash
set -euo pipefail

# Puts the Apparition Store build service inside a built Apparition.app (Contents/Resources/service),
# so the desktop app can run it as its child on a Mac (desktop/src/host.rs). Copies what the
# Apparition Store (store/ in this repo) has committed at HEAD (never uncommitted edits, never its config or data):
# server.js, lib/, public/, bin/ and the license. Run before the app is signed.
#
# Usage: scripts/bundle-service.sh <path/to/Apparition.app>
#        APPARITION_STORE_DIR overrides the folder (default: store/ in this repo).

app="${1:?usage: bundle-service.sh <Apparition.app>}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_dir="${APPARITION_STORE_DIR:-$ROOT/store}"
[[ -d "$app/Contents" ]] || { echo "✗ Not an app bundle: $app" >&2; exit 1; }
[[ -f "$source_dir/server.js" ]] || { echo "✗ No Apparition Store checkout at $source_dir (set APPARITION_STORE_DIR)" >&2; exit 1; }

dest="$app/Contents/Resources/service"
rm -rf "$dest"
mkdir -p "$dest"
# From the repository root, so the paths are read inside the store's own tree (HEAD:store/).
top="$(git -C "$source_dir" rev-parse --show-toplevel)"
prefix="$(git -C "$source_dir" rev-parse --show-prefix)"
git -C "$top" archive --format=tar "HEAD:$prefix" server.js lib public bin LICENSE | tar -x -C "$dest"
git -C "$source_dir" rev-parse --short HEAD >"$dest/REVISION"
echo "▶ Bundled the build service (Apparition Store $(cat "$dest/REVISION")) into $(basename "$app")"
