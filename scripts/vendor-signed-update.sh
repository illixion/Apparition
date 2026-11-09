#!/usr/bin/env bash
#
# Copies the signed-update crate (~/Projects/signed-update, or $SIGNED_UPDATE) into
# core/vendor/signed-update, which the desktop app and the core build against by path.
#
# Vendored, not a sibling path, because every release build works from a copy of this repo alone:
# the Docker builds for Linux rsync only these folders, the macOS files are built from a git copy
# at a fixed path (scripts/package-release.sh), and CI checks out one repository. A path outside it
# would break all three. Once signed-update is on GitHub, a `git = "…", rev = "…"` dependency does the
# same job without a copy; change the two Cargo.toml lines and delete core/vendor/signed-update.
#
#   scripts/vendor-signed-update.sh        # then build, test and commit the result
#
# core/vendor/signed-update/VENDORED_FROM names the commit the copy was taken from.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
SOURCE="${SIGNED_UPDATE:-$HOME/Projects/signed-update}"
DEST="$ROOT/core/vendor/signed-update"

[[ -d "$SOURCE/.git" ]] || { echo "error: no signed-update checkout at $SOURCE (set SIGNED_UPDATE)" >&2; exit 1; }
[[ -z "$(git -C "$SOURCE" status --porcelain)" ]] || { echo "error: $SOURCE has uncommitted changes; commit them first" >&2; exit 1; }

mkdir -p "$DEST"
# Only what a build and its doc example need; tests, docs and scripts stay in the source repo.
rsync -a --delete --exclude target --exclude .git --exclude tests --exclude docs --exclude scripts \
  --exclude .gitignore --exclude README.md --exclude Cargo.lock "$SOURCE/" "$DEST/"
printf '%s %s\n' "$(git -C "$SOURCE" rev-parse HEAD)" "$(git -C "$SOURCE" log -1 --format=%s)" > "$DEST/VENDORED_FROM"
echo "==> vendored signed-update $(git -C "$SOURCE" rev-parse --short HEAD) into core/vendor/signed-update"
