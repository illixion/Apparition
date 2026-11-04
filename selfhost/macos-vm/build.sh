#!/bin/bash
# Development tool only; signs the VZ runner, never the anisette helper.
set -eu
cd "$(dirname "$0")/../.."
mkdir -p build/macos-vm
xcrun clang -Wall -Wextra -Werror -arch arm64 -mmacosx-version-min=26.0 selfhost/macos-vm/guest-status.c -o build/macos-vm/guest-status
codesign --force --sign - build/macos-vm/guest-status
xcrun swiftc -O selfhost/macos-vm/main.swift -o build/macos-vm/vm
codesign --force --sign - --entitlements selfhost/macos-vm/runner.entitlements build/macos-vm/vm
