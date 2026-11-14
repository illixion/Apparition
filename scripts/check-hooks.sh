#!/usr/bin/env bash
# Compile the engine for device/simulator platforms, then exercise it on macOS.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d /private/tmp/apparition-hooks.XXXXXX)"
for platform in 'iphoneos arm64-apple-ios17.0' 'iphonesimulator arm64-apple-ios17.0-simulator' 'xros arm64-apple-xros1.0' 'xrsimulator arm64-apple-xros1.0-simulator'; do
  read -r sdk target <<< "$platform"
  xcrun --sdk "$sdk" clang -target "$target" -isysroot "$(xcrun --sdk "$sdk" --show-sdk-path)" \
    -Wall -Wextra -Werror -I "$root/hooks/include" -c "$root/hooks/src/hooks.c" -o "$work/$sdk.o"
  for src in runtime.c compat.c symbols.c relocate.c; do
    xcrun --sdk "$sdk" clang -target "$target" -isysroot "$(xcrun --sdk "$sdk" --show-sdk-path)" -Wall -Wextra -Werror -I "$root/hooks/include" -c "$root/hooks/src/$src" -o "$work/$sdk-$src.o"
  done
  xcrun --sdk "$sdk" clang -target "$target" -isysroot "$(xcrun --sdk "$sdk" --show-sdk-path)" -Wall -Wextra -Werror -fobjc-arc -fblocks -I "$root/hooks/include" -c "$root/hooks/src/notice.m" -o "$work/$sdk-notice.o"
done
xcrun clang -dynamiclib "$root/hooks/checks/provider.c" -o "$work/libprovider.dylib" -Wl,-install_name,@rpath/libprovider.dylib
xcrun clang -dynamiclib "$root/hooks/checks/client.c" -L "$work" -lprovider -o "$work/libclient.dylib" -Wl,-install_name,@rpath/libclient.dylib
xcrun clang -dynamiclib "$root/hooks/checks/late.c" -L "$work" -lprovider -o "$work/liblate.dylib"
xcrun clang -g -fobjc-arc -fblocks -Wall -Wextra -Werror -I "$root/hooks/include" "$root"/hooks/src/*.c "$root/hooks/src/originals.S" "$root/hooks/src/notice.m" "$root/hooks/checks/relocation.S" "$root/hooks/checks/check.m" \
  -framework Foundation -L "$work" -lclient -lprovider -Wl,-rpath,"$work" -o "$work/check"
"$work/check"
echo "Build artifacts: $work"
if [[ "${1:-}" == "--debugger" ]]; then
  xcrun lldb -b -o "command script import $root/hooks/checks/debugger.py" -o "aphook-check $work/check" -o quit > "$work/debugger.log" 2>&1
  cat "$work/debugger.log"
  command grep -q 'PASS late attach' "$work/debugger.log"
fi
if [[ "${1:-}" == "--simulator" ]]; then
  device="${2:?usage: check-hooks.sh --simulator UDID}"
  sim="$work/simulator"
  mkdir -p "$sim"
  simflags=(-target arm64-apple-ios17.0-simulator -isysroot "$(xcrun --sdk iphonesimulator --show-sdk-path)")
  xcrun clang "${simflags[@]}" -dynamiclib "$root/hooks/checks/provider.c" -o "$sim/libprovider.dylib" -Wl,-install_name,@rpath/libprovider.dylib
  xcrun clang "${simflags[@]}" -dynamiclib "$root/hooks/checks/client.c" -L "$sim" -lprovider -o "$sim/libclient.dylib" -Wl,-install_name,@rpath/libclient.dylib
  xcrun clang "${simflags[@]}" -dynamiclib "$root/hooks/checks/late.c" -L "$sim" -lprovider -o "$sim/liblate.dylib"
  xcrun clang "${simflags[@]}" -fobjc-arc -fblocks -Wall -Wextra -Werror -I "$root/hooks/include" "$root"/hooks/src/*.c "$root/hooks/src/originals.S" "$root/hooks/src/notice.m" "$root/hooks/checks/relocation.S" "$root/hooks/checks/check.m" \
    -framework Foundation -framework UIKit -L "$sim" -lclient -lprovider -Wl,-rpath,"$sim" -o "$sim/check"
  xcrun simctl spawn "$device" "$sim/check"
fi
