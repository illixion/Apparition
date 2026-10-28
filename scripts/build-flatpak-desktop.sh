#!/usr/bin/env bash
# Native Linux desktop build against the same SDK as the Flatpak runtime.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
id=com.illixion.apparition.desktop
work="$ROOT/build/flatpak/sdk-build"
cache="$ROOT/build/flatpak/sdk-cache"
mkdir -p "$cache/cargo" "$cache/target"
if [[ ! -f "$work/metadata" ]]; then
  flatpak build-init --sdk-extension=org.freedesktop.Sdk.Extension.rust-stable \
    "$work" "$id" org.gnome.Sdk org.gnome.Platform 49
fi
# Tauri generates desktop/gen schemas while building, so the source checkout is writable.
flatpak build --share=network --filesystem="$ROOT" --filesystem="$cache" \
  --env=PATH=/usr/lib/sdk/rust-stable/bin:/usr/bin:/app/bin \
  --env=CARGO_HOME="$cache/cargo" --env=CARGO_TARGET_DIR="$cache/target" \
  --env=CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" \
  "$work" cargo build --locked --release --features custom-protocol \
    --manifest-path "$ROOT/desktop/Cargo.toml"
echo "Desktop binary: $cache/target/release/apparition-app"
