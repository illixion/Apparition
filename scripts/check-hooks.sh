#!/usr/bin/env bash
# Compile the engine for device/simulator platforms, then exercise it on macOS.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d /private/tmp/apparition-hooks.XXXXXX)"
for platform in 'iphoneos arm64-apple-ios17.0' 'iphonesimulator arm64-apple-ios17.0-simulator' 'xros arm64-apple-xros1.0' 'xrsimulator arm64-apple-xros1.0-simulator'; do
  read -r sdk target <<< "$platform"
  xcrun --sdk "$sdk" clang -target "$target" -isysroot "$(xcrun --sdk "$sdk" --show-sdk-path)" \
    -Wall -Wextra -Werror -I "$root/hooks/include" -c "$root/hooks/src/hooks.c" -o "$work/$sdk.o"
done
xcrun clang -dynamiclib "$root/hooks/checks/provider.c" -o "$work/libprovider.dylib" -Wl,-install_name,@rpath/libprovider.dylib
xcrun clang -dynamiclib "$root/hooks/checks/client.c" -L "$work" -lprovider -o "$work/libclient.dylib" -Wl,-install_name,@rpath/libclient.dylib
xcrun clang -Wall -Wextra -Werror -I "$root/hooks/include" "$root/hooks/src/hooks.c" "$root/hooks/checks/check.m" \
  -framework Foundation -L "$work" -lclient -lprovider -Wl,-rpath,"$work" -o "$work/check"
"$work/check"
echo "Build artifacts: $work"
if [[ "${1:-}" == "--simulator" ]]; then
  device="${2:?usage: check-hooks.sh --simulator UDID}"
  sim="$work/simulator"
  mkdir -p "$sim"
  simflags=(-target arm64-apple-ios17.0-simulator -isysroot "$(xcrun --sdk iphonesimulator --show-sdk-path)")
  xcrun clang "${simflags[@]}" -dynamiclib "$root/hooks/checks/provider.c" -o "$sim/libprovider.dylib" -Wl,-install_name,@rpath/libprovider.dylib
  xcrun clang "${simflags[@]}" -dynamiclib "$root/hooks/checks/client.c" -L "$sim" -lprovider -o "$sim/libclient.dylib" -Wl,-install_name,@rpath/libclient.dylib
  xcrun clang "${simflags[@]}" -Wall -Wextra -Werror -I "$root/hooks/include" "$root/hooks/src/hooks.c" "$root/hooks/checks/check.m" \
    -framework Foundation -L "$sim" -lclient -lprovider -Wl,-rpath,"$sim" -o "$sim/check"
  xcrun simctl spawn "$device" "$sim/check"
fi
