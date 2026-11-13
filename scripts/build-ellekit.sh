#!/usr/bin/env bash
# Builds ElleKit's libellekit.dylib (the Substrate replacement the tweak injector adds to an app,
# core/src/tweaks.rs) from its v1.1.3 tag, with the Xcode build scripts/release-toolchain names, and
# checks it against core/assets/ellekit/libellekit.dylib byte for byte.
#
#   scripts/build-ellekit.sh           # build and compare; exits non-zero when they differ
#   scripts/build-ellekit.sh --update  # build and replace the committed copy (then update
#                                      # ELLEKIT_SHA256 in core/src/tweaks.rs)
#
# ElleKit is BSD-3-Clause (core/assets/ellekit/LICENSE, copied into every app that gets it). Its
# only official builds are jailbreak packages, so Apparition builds it itself: the same tag and
# Xcode give the same bytes, which is how anyone can check the copy in this repository.
#
# The only change from ElleKit's own build is the deployment target: its project asks for iOS 11,
# below what current Xcode builds for, so this asks for 15.0 (the lowest Xcode 27 supports).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
tag=v1.1.3
commit=24033cc3cfa78cfd64908fc9f4b63f1880adeb93
asset="$root/core/assets/ellekit/libellekit.dylib"

# shellcheck source=/dev/null
source <(grep -E '^XCODE_BUILD=' "$root/scripts/release-toolchain")
developer=""
for app in /Applications/Xcode*.app; do
  if [[ "$(DEVELOPER_DIR="$app/Contents/Developer" xcodebuild -version 2>/dev/null | tail -1)" == "Build version $XCODE_BUILD" ]]; then
    developer="$app/Contents/Developer"
    break
  fi
done
if [[ -z "$developer" ]]; then
  echo "error: no /Applications/Xcode*.app is build $XCODE_BUILD (scripts/release-toolchain)" >&2
  exit 1
fi

# Always the same folder: source paths end up in the binary (Swift's #file in its messages), so a
# different folder gives different bytes.
work=/private/tmp/apparition-ellekit-build
rm -rf "$work"
mkdir -p "$work"
trap 'rm -rf "$work"' EXIT
git clone --quiet --depth 1 --branch "$tag" https://github.com/tealbathingsuit/ellekit.git "$work/ellekit"
if [[ "$(git -C "$work/ellekit" rev-parse HEAD)" != "$commit" ]]; then
  echo "error: ElleKit's $tag tag no longer points at $commit" >&2
  exit 1
fi
(
  cd "$work/ellekit"
  DEVELOPER_DIR="$developer" xcodebuild -scheme ellekit -configuration Release -destination 'generic/platform=iOS' \
    -derivedDataPath "$work/derived" BUILD_DIR="$work/build/" \
    CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY="" IPHONEOS_DEPLOYMENT_TARGET=15.0 \
    > "$work/build.log" 2>&1 || { tail -20 "$work/build.log" >&2; exit 1; }
)
built="$work/build/Release-iphoneos/libellekit.dylib"
echo "built:     $(shasum -a 256 "$built" | cut -d' ' -f1)"
if [[ "${1:-}" == "--update" ]]; then
  cp "$built" "$asset"
  cp "$work/ellekit/LICENSE" "$root/core/assets/ellekit/LICENSE"
  echo "Replaced $asset; set ELLEKIT_SHA256 in core/src/tweaks.rs to the hash above."
  exit 0
fi
echo "committed: $(shasum -a 256 "$asset" | cut -d' ' -f1)"
cmp -s "$built" "$asset" && echo "They match." || { echo "error: they differ" >&2; exit 1; }
