#!/usr/bin/env bash
# Builds the Rust core (core/: isideload + jkcoxson/idevice's C FFI) as one static
# library per Apple platform and stages it with its headers under build/core-out:
#
#   build/core-out/include/            idevice.h, plist.h, apparition.h (platform-independent)
#   build/core-out/lib/<PLATFORM_NAME>/libapparition_core.a
#
# <PLATFORM_NAME> is Xcode's name for the SDK (xros, xrsimulator, iphoneos, appletvos), so a target's
# LIBRARY_SEARCH_PATHS can be $(SRCROOT)/build/core-out/lib/$(PLATFORM_NAME) and pick the
# right one for whatever it is being built for.
#
#   scripts/build-core.sh                      # visionOS, iOS and tvOS devices (the default)
#   scripts/build-core.sh visionos-sim         # just the visionOS simulator
#   PLATFORMS="visionos visionos-sim" scripts/build-core.sh
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# Must match the idevice rev in core/Cargo.toml: the header comes from this checkout.
commit="${IDEVICE_COMMIT:-3854a5df4a5a6dee71ffce4d8befc2ea356a8065}"
src="$root/build/idevice"
out="$root/build/core-out"

if [[ $# -gt 0 ]]; then
  platforms=("$@")
else
  read -r -a platforms <<< "${PLATFORMS:-visionos ios tvos}"
fi

# platform -> rust target, Xcode SDK, deployment-target variable and value
spec() {
  case "$1" in
    visionos)     echo "aarch64-apple-visionos xros XROS_DEPLOYMENT_TARGET 26.0" ;;
    visionos-sim) echo "aarch64-apple-visionos-sim xrsimulator XROS_DEPLOYMENT_TARGET 26.0" ;;
    ios)          echo "aarch64-apple-ios iphoneos IPHONEOS_DEPLOYMENT_TARGET 17.0" ;;
    ios-sim)      echo "aarch64-apple-ios-sim iphonesimulator IPHONEOS_DEPLOYMENT_TARGET 17.0" ;;
    tvos)         echo "aarch64-apple-tvos appletvos TVOS_DEPLOYMENT_TARGET 17.0" ;;
    tvos-sim)     echo "aarch64-apple-tvos-sim appletvsimulator TVOS_DEPLOYMENT_TARGET 17.0" ;;
    *) echo "unknown platform: $1 (visionos, visionos-sim, ios, ios-sim, tvos, tvos-sim)" >&2; return 1 ;;
  esac
}

if [[ ! -d "$src/.git" ]]; then
  git clone https://github.com/jkcoxson/idevice.git "$src"
fi
git -C "$src" fetch --quiet origin
git -C "$src" checkout --quiet "$commit"

# idevice.h is written by the FFI crate's build script (cbindgen), which reads the source and
# nothing else: the header is the same for every target and feature set. So a host `cargo check`
# with the light TLS backend makes it, rather than a release build of all of idevice per platform,
# and a stamp skips even that while the checkout stays at this commit.
stamp="$src/ffi/.apparition-header-commit"
if [[ ! -f "$src/ffi/idevice.h" || "$(cat "$stamp" 2>/dev/null)" != "$commit" ]]; then
  rm -f "$stamp"
  (cd "$src/ffi" && cargo check --quiet --no-default-features --features full,ring)
  echo "$commit" > "$stamp"
fi

for platform in "${platforms[@]}"; do
  read -r target sdk_name deploy_var deploy_version <<< "$(spec "$platform")"
  sdk="$(xcrun --sdk "$sdk_name" --show-sdk-path)"
  if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
    echo "Rust target $target is not installed: rustup target add $target" >&2
    exit 1
  fi
  (cd "$root/core" && env BINDGEN_EXTRA_CLANG_ARGS="--sysroot=$sdk" "$deploy_var=$deploy_version" \
    cargo build --release --target "$target")
  mkdir -p "$out/lib/$sdk_name"
  cp "$root/core/target/$target/release/libapparition_core.a" "$out/lib/$sdk_name/"
  echo "Staged apparition-core for $platform in $out/lib/$sdk_name"
done

mkdir -p "$out/include"
cp "$src/ffi/idevice.h" "$src/ffi/plist.h" "$root/core/include/apparition.h" "$out/include/"
echo "Staged headers (idevice $commit) in $out/include"
