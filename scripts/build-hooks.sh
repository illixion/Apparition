#!/usr/bin/env bash
# Rebuild our MIT Substrate-compatible dylibs using the audited release toolchain.
# Default: compare to the committed files. --update: replace binaries and hashes.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
source <(command grep -E '^XCODE_BUILD=' "$root/scripts/release-toolchain")
developer=""
for app in /Applications/Xcode*.app; do
  if [[ "$(DEVELOPER_DIR="$app/Contents/Developer" xcodebuild -version 2>/dev/null | tail -1)" == "Build version $XCODE_BUILD" ]]; then
    developer="$app/Contents/Developer"
    break
  fi
done
[[ -n "$developer" ]] || { echo "error: Xcode build $XCODE_BUILD is required" >&2; exit 1; }
export DEVELOPER_DIR="$developer"
work="$(mktemp -d /private/tmp/apparition-hooks-build.XXXXXX)"
mkdir -p "$root/core/assets/hooks"
for platform in 'iphoneos arm64-apple-ios17.0 ios' 'xros arm64-apple-xros1.0 visionos'; do
  read -r sdk target name <<< "$platform"
  xcrun --sdk "$sdk" clang -target "$target" -isysroot "$(xcrun --sdk "$sdk" --show-sdk-path)" \
    -dynamiclib -O2 -fobjc-arc -fblocks -Wall -Wextra -Werror \
    -ffile-prefix-map="$root"=. -I "$root/hooks/include" \
    "$root"/hooks/src/*.c "$root/hooks/src/notice.m" "$root/hooks/src/originals.S" \
    -framework Foundation -framework UIKit -Wl,-no_uuid -Wl,-no_adhoc_codesign -Wl,-headerpad,0x400 \
    -Wl,-install_name,@rpath/CydiaSubstrate.framework/CydiaSubstrate \
    -Wl,-current_version,1.0 -Wl,-compatibility_version,1.0 \
    -o "$work/$name.dylib"
  python3 "$root/hooks/build-uuid.py" "$work/$name.dylib"
  xcrun nm -gjU "$work/$name.dylib" > "$work/$name.exports"
  while IFS= read -r symbol; do
    command grep -Fxq "_$symbol" "$work/$name.exports" || { echo "Missing export: $symbol" >&2; exit 1; }
  done < "$root/hooks/exports.txt"
  if [[ "${1:-}" == "--update" ]]; then
    cp "$work/$name.dylib" "$root/core/assets/hooks/$name.dylib"
  else
    cmp "$work/$name.dylib" "$root/core/assets/hooks/$name.dylib"
  fi
done
(cd "$work" && shasum -a 256 ios.dylib visionos.dylib) > "$work/SHA256SUMS"
if [[ "${1:-}" == "--update" ]]; then
  cp "$work/SHA256SUMS" "$root/core/assets/hooks/SHA256SUMS"
else
  cmp "$work/SHA256SUMS" "$root/core/assets/hooks/SHA256SUMS"
fi
cat "$work/SHA256SUMS"
echo "ApparitionHooks builds match. Artifacts: $work"
