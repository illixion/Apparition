#!/bin/bash
# Builds the anisette VM's runner and the two programs it puts in the guest, into build/macos-vm (vm, guest-status,
# guest-bridge). scripts/install-desktop.sh and package-release.sh put them in Apparition.app as apparition-vm,
# apparition-vm-guest-status and apparition-vm-guest-bridge. Signs the runner ad hoc with the virtualization
# entitlement (the app's build signs it again with its identity); never touches the anisette helper.
set -eu
cd "$(dirname "$0")/../.."
mkdir -p build/macos-vm
# The guest is macOS 26 (core/src/vm.rs pins it).
xcrun clang -Wall -Wextra -Werror -arch arm64 -mmacosx-version-min=26.0 selfhost/macos-vm/guest-status.c -o build/macos-vm/guest-status
codesign --force --sign - build/macos-vm/guest-status
xcrun clang -Wall -Wextra -Werror -arch arm64 -mmacosx-version-min=26.0 selfhost/macos-vm/guest-bridge.c -o build/macos-vm/guest-bridge
codesign --force --sign - build/macos-vm/guest-bridge
# Saving and restoring a VM (suspend when idle) needs macOS 14 on the host; macOS guests need Apple silicon.
xcrun swiftc -O -target arm64-apple-macos14.0 selfhost/macos-vm/main.swift -o build/macos-vm/vm
codesign --force --sign - --entitlements selfhost/macos-vm/runner.entitlements build/macos-vm/vm
