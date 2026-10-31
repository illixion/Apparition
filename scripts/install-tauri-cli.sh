#!/usr/bin/env bash
# Installs the Tauri CLI (cargo-tauri) from Tauri's own prebuilt release, checked against the
# hashes below, into $CARGO_HOME/bin. Compiling it with `cargo install tauri-cli` takes about
# 9 minutes on a CI runner, every time the cache doesn't have it; this takes seconds. Used by the
# release workflow and scripts/desktop-linux.Dockerfile; `cargo install tauri-cli --version '^2'
# --locked` still works locally.
#
# To move to a newer CLI, set the version and the SHA-256 of each archive from
# https://github.com/tauri-apps/tauri/releases/tag/tauri-cli-v<version>.
set -euo pipefail

version=2.12.4
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  asset=cargo-tauri-aarch64-apple-darwin.zip      sha=f71a34ada728ca2d1a51c28d6d21639e1e080898ec69ec8d49c5da3b7e8bc1be ;;
  Linux-x86_64)  asset=cargo-tauri-x86_64-unknown-linux-gnu.tgz  sha=820ae3c7580ca637cad15f65638960ee2a96e5a491e224691c86d6f25e6c9e8a ;;
  Linux-aarch64) asset=cargo-tauri-aarch64-unknown-linux-gnu.tgz sha=4f601b9f462c3d3d10ce67b6ae963f099f6a5a3253920dae95a253abd558d723 ;;
  *) echo "no prebuilt Tauri CLI here; cargo install tauri-cli --version '^2' --locked" >&2; exit 1 ;;
esac

bin="${CARGO_HOME:-$HOME/.cargo}/bin"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
curl -sSfL -o "$work/$asset" "https://github.com/tauri-apps/tauri/releases/download/tauri-cli-v$version/$asset"
if command -v sha256sum >/dev/null; then
  got="$(sha256sum "$work/$asset")"
else
  got="$(shasum -a 256 "$work/$asset")"
fi
[[ "${got%% *}" == "$sha" ]] || { echo "error: $asset has SHA-256 ${got%% *}, expected $sha" >&2; exit 1; }
case "$asset" in
  *.zip) unzip -q "$work/$asset" cargo-tauri -d "$work" ;;
  *) tar -xzf "$work/$asset" -C "$work" cargo-tauri ;;
esac
mkdir -p "$bin"
install -m 755 "$work/cargo-tauri" "$bin/cargo-tauri"
"$bin/cargo-tauri" --version
