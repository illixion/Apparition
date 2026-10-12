#!/usr/bin/env bash
#
# Builds the files of an Apparition release into build/release/<version>/, named the way
# the release site (apparition.illixion.com, scripts/publish.py) expects:
#
#   Apparition-<version>-visionOS.ipa                       unsigned; signed on install
#   Apparition-Installer-<version>-macos-universal.zip      arm64 + x86_64
#   Apparition-Installer-<version>-windows-x86_64.zip
#   Apparition-Installer-<version>-linux-<arch>.tar.gz      with --linux, static, built in Docker
#
#   scripts/package-release.sh 0.2.0
#   scripts/package-release.sh 0.2.0 --linux        # also Linux x86_64 and arm64
#   scripts/package-release.sh 0.2.0 --only-app     # just the IPA
#
# The version has to be committed first, in both places that carry it: MARKETING_VERSION
# in project.yml and `version` in installer/Cargo.toml. The build number is the commit
# count, so every release's is higher than the last. A dirty tree is refused, so a release
# is always a commit you can name.
#
# Don't edit this script while it runs: bash reads it as it goes, so the rest of a running
# copy shifts under it.
#
# Signing a release happens on the site: publish.py hashes these files into SHA256SUMS and
# signs that with a release key. Nothing here holds a key.
#
# Needs: Xcode with the visionOS SDK, xcodegen, rustup targets aarch64-apple-visionos,
# aarch64-apple-darwin, x86_64-apple-darwin and x86_64-pc-windows-gnu (with mingw-w64 from
# Homebrew), and Docker for --linux.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:?usage: package-release.sh <version> [--linux] [--only-app] [--allow-dirty]}"
shift
LINUX=0
ONLY_APP=0
ALLOW_DIRTY=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --linux) LINUX=1 ;;
    --only-app) ONLY_APP=1 ;;
    # For trying the packaging itself; such a build isn't one to publish.
    --allow-dirty) ALLOW_DIRTY=1 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

cd "$ROOT"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: $VERSION isn't major.minor.patch (Cargo needs all three)" >&2; exit 1; }

app_version="$(sed -n 's/^ *MARKETING_VERSION: *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p' project.yml | head -1)"
installer_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' installer/Cargo.toml | head -1)"
[[ "$app_version" == "$VERSION" ]] \
  || { echo "error: project.yml's MARKETING_VERSION is $app_version, not $VERSION; commit the bump first" >&2; exit 1; }
[[ "$ONLY_APP" == 1 || "$installer_version" == "$VERSION" ]] \
  || { echo "error: installer/Cargo.toml's version is $installer_version, not $VERSION; commit the bump first" >&2; exit 1; }
if [[ -n "$(git status --porcelain)" ]]; then
  if [[ "$ALLOW_DIRTY" == 1 ]]; then
    echo "warning: the tree has uncommitted changes; don't publish this build" >&2
  else
    echo "error: the tree has uncommitted changes; a release must be a commit" >&2
    exit 1
  fi
fi

BUILD="$(git rev-list --count HEAD)"
COMMIT="$(git rev-parse --short=8 HEAD)"
OUT="$ROOT/build/release/$VERSION"
WORK="$ROOT/build/release/work"
rm -rf "$OUT" "$WORK"
mkdir -p "$OUT" "$WORK"
echo "==> Apparition $VERSION, build $BUILD ($COMMIT)"

# ------------------------------------------------------------------ the app
echo "==> core for visionOS"
scripts/build-core.sh visionos
xcodegen generate --quiet
# APPARITION_RELEASE leaves out what only development builds carry (the experiment endpoints,
# the release-site override). build-and-sign also builds Release, so DEBUG can't tell them apart.
echo "==> app (Release, unsigned)"
xcodebuild -project Apparition.xcodeproj -scheme Apparition -configuration Release \
  -sdk xros -destination 'generic/platform=visionOS' -derivedDataPath "$WORK/DerivedData" \
  MARKETING_VERSION="$VERSION" CURRENT_PROJECT_VERSION="$BUILD" \
  SWIFT_ACTIVE_COMPILATION_CONDITIONS='$(inherited) APPARITION_RELEASE' \
  CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY="" \
  -quiet build
app="$WORK/DerivedData/Build/Products/Release-xros/Apparition.app"
[[ -d "$app" ]] || { echo "error: no Apparition.app at $app" >&2; exit 1; }
built_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Info.plist")"
[[ "$built_version" == "$VERSION" ]] || { echo "error: the app says $built_version" >&2; exit 1; }
# A free Apple ID pays an App ID for every extension, and Apparition is meant to have none.
if [[ -d "$app/PlugIns" || -d "$app/Extensions" ]]; then
  echo "error: Apparition.app embeds extensions; each costs a free account an App ID" >&2
  exit 1
fi
mkdir -p "$WORK/ipa/Payload"
ditto "$app" "$WORK/ipa/Payload/Apparition.app"
(cd "$WORK/ipa" && zip -qryX "$OUT/Apparition-$VERSION-visionOS.ipa" Payload)

if [[ "$ONLY_APP" == 0 ]]; then
  # ---------------------------------------------------------------- the installer
  cd "$ROOT/installer"
  bin=apparition-installer
  readme() {
    cat > "$1/README.txt" <<EOF
Apparition Installer $VERSION ($COMMIT)

Puts Apparition on your Apple Vision Pro: pair once, then install.

  $2 pair        (with Settings > General > Remote Devices open on the headset)
  $2 install --apple-id you@example.com --anisette <url>

Run it without arguments for every command. https://apparition.illixion.com
EOF
  }

  echo "==> installer for macOS (arm64 + x86_64)"
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    MACOSX_DEPLOYMENT_TARGET=11.0 cargo build -q --release --target "$target"
  done
  mkdir -p "$WORK/macos"
  xcrun lipo -create -output "$WORK/macos/$bin" \
    target/aarch64-apple-darwin/release/$bin target/x86_64-apple-darwin/release/$bin
  readme "$WORK/macos" "./$bin"
  (cd "$WORK/macos" && zip -qX "$OUT/Apparition-Installer-$VERSION-macos-universal.zip" "$bin" README.txt)

  echo "==> installer for Windows (x86_64)"
  cargo build -q --release --target x86_64-pc-windows-gnu
  mkdir -p "$WORK/windows"
  cp "target/x86_64-pc-windows-gnu/release/$bin.exe" "$WORK/windows/"
  readme "$WORK/windows" "$bin.exe"
  (cd "$WORK/windows" && zip -qX "$OUT/Apparition-Installer-$VERSION-windows-x86_64.zip" "$bin.exe" README.txt)

  if [[ "$LINUX" == 1 ]]; then
    # In Docker, from a copy of the repo (just core/, installer/ and release-signers), so the
    # container never writes into this checkout's target directories. Built on Alpine, so
    # it's linked statically against musl and runs on any distribution: a glibc build needs
    # the builder's glibc or newer (Debian trixie's 2.39 ruled out Ubuntu 22.04). The cargo
    # registry is kept in a volume per architecture between runs.
    for arch in x86_64 arm64; do
      echo "==> installer for Linux ($arch, static, in Docker)"
      platform=linux/amd64; [[ "$arch" == arm64 ]] && platform=linux/arm64
      src="$WORK/linux-src-$arch"
      mkdir -p "$src" "$WORK/linux-$arch"
      rsync -a --exclude target "$ROOT/core" "$ROOT/installer" "$ROOT/release-signers" "$src/"
      docker run --rm --platform "$platform" -v "apparition-cargo-$arch:/usr/local/cargo/registry" \
        -v "$src:/src" -w /src/installer -e CARGO_PROFILE_RELEASE_STRIP=true rust:1.97-alpine \
        sh -c 'apk add -q musl-dev && cargo build -q --release'
      file "$src/installer/target/release/$bin" | grep -qE "statically linked|static-pie linked" \
        || { echo "error: the Linux $arch installer isn't statically linked" >&2; exit 1; }
      cp "$src/installer/target/release/$bin" "$WORK/linux-$arch/"
      readme "$WORK/linux-$arch" "./$bin"
      tar -C "$WORK/linux-$arch" -czf "$OUT/Apparition-Installer-$VERSION-linux-$arch.tar.gz" "$bin" README.txt
    done
  fi
fi

rm -rf "$WORK"
echo "==> built into $OUT:"
(cd "$OUT" && ls -l | sed 's/^/    /')
echo "    Upload them, then on the site: scripts/publish.py add $VERSION $OUT --url-base <where they are>"
