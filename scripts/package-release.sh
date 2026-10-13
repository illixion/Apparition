#!/usr/bin/env bash
#
# Builds the files of an Apparition release into build/release/<version>/, named the way
# the release site (apparition.illixion.com, scripts/publish.py) expects:
#
#   Apparition-<version>-<visionOS|iOS|tvOS>.ipa            unsigned; signed on install
#   Apparition-Installer-<version>-macos-universal.zip      arm64 + x86_64: the desktop app
#                                                           and the command line
#   Apparition-Installer-<version>-windows-x86_64.zip       the same two for Windows
#   Apparition-Installer-<version>-linux-<arch>.tar.gz      with --linux, built in Docker: the
#                                                           static command line, and the
#                                                           desktop app as an AppImage and a .deb
#
#   scripts/package-release.sh 0.2.0
#   scripts/package-release.sh 0.2.0 --linux        # also Linux x86_64 and arm64
#   LINUX_X86_64_HOST=tuxpc scripts/package-release.sh 0.2.0 --linux   # x86_64 natively there
#   scripts/package-release.sh 0.2.0 --only-app     # just the IPAs
#   scripts/package-release.sh 0.2.0 --parts app,macos,windows,linux   # any of these (CI builds
#                                                    # them on separate runners); LINUX_ARCHS
#                                                    # narrows linux to x86_64 or arm64
#
# The version has to be committed first, in every place that carries it: MARKETING_VERSION
# in project.yml, and `version` in installer/Cargo.toml and desktop/Cargo.toml (the desktop
# app's tauri.conf.json takes it from there). The build number is the commit
# count, so every release's is higher than the last. A dirty tree is refused, so a release
# is always a commit you can name.
#
# Don't edit this script while it runs: bash reads it as it goes, so the rest of a running
# copy shifts under it.
#
# Signing a release happens on the site: publish.py hashes these files into SHA256SUMS and
# signs that with a release key. Nothing here holds a key.
#
# Needs: Xcode with the visionOS, iOS and tvOS SDKs, xcodegen, rustup targets
# aarch64-apple-visionos, aarch64-apple-ios, aarch64-apple-tvos,
# aarch64-apple-darwin, x86_64-apple-darwin and x86_64-pc-windows-gnu (with mingw-w64 from
# Homebrew), the Tauri CLI for the desktop app (cargo install tauri-cli --version '^2'
# --locked), and Docker for --linux.
#
# The Linux desktop app is built in an image made from scripts/desktop-linux.Dockerfile
# (Ubuntu 22.04 with WebKitGTK and the Tauri CLI), tagged per architecture and reused.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:?usage: package-release.sh <version> [--linux] [--only-app] [--parts LIST] [--allow-dirty]}"
shift
LINUX=0
ONLY_APP=0
PARTS=""
ALLOW_DIRTY=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --linux) LINUX=1 ;;
    --only-app) ONLY_APP=1 ;;
    --parts) PARTS="${2:?--parts needs a list: app,macos,windows,linux}"; shift ;;
    # For trying the packaging itself; such a build isn't one to publish.
    --allow-dirty) ALLOW_DIRTY=1 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

# What to build: the IPAs ("app"), the macOS and Windows installers, and with --linux both Linux
# ones, unless --parts names them.
if [[ -z "$PARTS" ]]; then
  PARTS=app,macos,windows
  [[ "$LINUX" == 1 ]] && PARTS+=,linux
  [[ "$ONLY_APP" == 1 ]] && PARTS=app
fi
for part in ${PARTS//,/ }; do
  case "$part" in app|macos|windows|linux) ;; *) echo "error: no part called $part (app, macos, windows, linux)" >&2; exit 2 ;; esac
done
has() { [[ ",$PARTS," == *",$1,"* ]]; }
installers() { has macos || has windows || has linux; }

cd "$ROOT"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: $VERSION isn't major.minor.patch (Cargo needs all three)" >&2; exit 1; }

app_version="$(sed -n 's/^ *MARKETING_VERSION: *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p' project.yml | head -1)"
installer_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' installer/Cargo.toml | head -1)"
desktop_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' desktop/Cargo.toml | head -1)"
[[ "$app_version" == "$VERSION" ]] \
  || { echo "error: project.yml's MARKETING_VERSION is $app_version, not $VERSION; commit the bump first" >&2; exit 1; }
{ ! installers || [[ "$installer_version" == "$VERSION" ]]; } \
  || { echo "error: installer/Cargo.toml's version is $installer_version, not $VERSION; commit the bump first" >&2; exit 1; }
{ ! installers || [[ "$desktop_version" == "$VERSION" ]]; } \
  || { echo "error: desktop/Cargo.toml's version is $desktop_version, not $VERSION; commit the bump first" >&2; exit 1; }
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

app_name="Apparition"
bin=apparition
readme() {
  cat > "$1/README.txt" <<EOF
Apparition $VERSION ($COMMIT)

Puts Apparition on your Apple devices (pair once, then install), and is the companion
Apparition on them uses: the relay back to their own device, and an anisette server.

$3

Or from a terminal:

$2 pair        (with Settings > General > Remote Devices open on the headset)
$2 install --apple-id you@example.com      (add --anisette <url> on Windows)
$2 relay       (the companion, until Ctrl-C)

Run it without arguments for every command. https://apparition.illixion.com
EOF
}

# ------------------------------------------------------------------ Linux, in the background
# Both architectures build in Docker while everything else builds here, each with its own log in
# build/release/. Built on Alpine, the command line is linked statically against musl and runs
# on any distribution (a glibc build needs the builder's glibc or newer); the desktop app is
# built on Ubuntu 22.04 (scripts/desktop-linux.Dockerfile), for glibc 2.35 and later. Each
# build works from a copy of the repo, so the container never writes into this checkout, and
# keeps its cargo registry and build output in Docker volumes per architecture, so a later
# release rebuilds only what changed.
#
# LINUX_X86_64_HOST=user@host builds x86_64 there instead, natively: here it runs under
# emulation, the slowest step. The host needs Docker (usable without sudo) and rsync; the same
# images and commands run there, so the files come out the same.
#
# For CI, where Docker volumes go with the machine: LINUX_CACHE=<folder> keeps the registries and
# build output in that folder instead (on Linux, where the mount takes linuxdeploy's files), for
# the workflow to cache, and LINUX_DESKTOP_IMAGE_BUILT=1 says the desktop image is already
# built (from the workflow's own layer cache).
LOGS="$ROOT/build/release"
SSH_OPTS=(-o ControlMaster=auto -o "ControlPath=$HOME/.ssh/apparition-release-%C" -o ControlPersist=900)

linux_build() {
  local arch="$1"
  local platform=linux/amd64 host="" src="$WORK/linux-src-$arch" out="$WORK/linux-$arch"
  [[ "$arch" == arm64 ]] && platform=linux/arm64
  [[ "$arch" == x86_64 ]] && host="${LINUX_X86_64_HOST:-}"
  mkdir -p "$src/selfhost" "$src/scripts" "$out"
  rsync -a --exclude target "$ROOT/core" "$ROOT/installer" "$ROOT/desktop" "$ROOT/release-signers" "$src/"
  rsync -a "$ROOT/selfhost/anisette" "$src/selfhost/"
  cp "$ROOT/scripts/desktop-linux.Dockerfile" "$src/scripts/"
  # Where the source is on the machine that builds, and how to run a command there.
  local dir="$src"
  on() { "$@"; }
  if [[ -n "$host" ]]; then
    dir="$(ssh "${SSH_OPTS[@]}" "$host" 'printf %s "$HOME"')/apparition-release/linux-src-$arch"
    # The containers write as root; clear what the last run left through Docker as well.
    ssh "${SSH_OPTS[@]}" "$host" "mkdir -p '$dir' && docker run --rm -v '$dir:/src' rust:1.97-alpine rm -rf /src/bundles"
    rsync -a --delete -e "ssh ${SSH_OPTS[*]}" "$src/" "$host:$dir/"
    on() { ssh "${SSH_OPTS[@]}" "$host" "$(printf '%q ' "$@")"; }
  fi

  # A Docker volume by this name, or with LINUX_CACHE (not for a remote host) a folder in it.
  store() {
    if [[ -n "${LINUX_CACHE:-}" && -z "$host" ]]; then
      mkdir -p "$LINUX_CACHE/$1" && printf %s "$LINUX_CACHE/$1"
    else
      printf %s "apparition-$1"
    fi
  }
  # The containers run as root; what they leave in the source and in the stores goes back to
  # the owner of the source, so it can be cached and removed without root.
  local give_back='{ chown -R "$(stat -c %u:%g /src)" /src /target /usr/local/cargo/registry || true; }'

  echo "==> installer for Linux ($arch${host:+, on $host})"
  on docker run --rm --platform "$platform" -v "$(store "cargo-$arch"):/usr/local/cargo/registry" \
    -v "$(store "installer-target-$arch"):/target" -e CARGO_TARGET_DIR=/target \
    -v "$dir:/src" -w /src/installer -e CARGO_PROFILE_RELEASE_STRIP=true rust:1.97-alpine \
    sh -c "apk add -q musl-dev && cargo build -q --release && mkdir -p /src/bundles && cp /target/release/apparition /src/bundles/ && $give_back"

  echo "==> desktop app for Linux ($arch${host:+, on $host})"
  if [[ -z "${LINUX_DESKTOP_IMAGE_BUILT:-}" ]]; then
    on docker build -q --platform "$platform" -t "apparition-desktop-linux:$arch" \
      -f "$dir/scripts/desktop-linux.Dockerfile" "$dir/scripts" >/dev/null
  fi
  # The target directory is a volume, not the mount: linuxdeploy copies files with their
  # permissions, which a macOS host's mount refuses. Only the two bundles come out.
  on docker run --rm --platform "$platform" -v "$(store "cargo-desktop-$arch"):/usr/local/cargo/registry" \
    -v "$(store "desktop-target-$arch"):/target" -e CARGO_TARGET_DIR=/target \
    -v "$dir:/src" -w /src/desktop "apparition-desktop-linux:$arch" sh -c "rm -rf /target/release/bundle &&
      { cargo tauri build --bundles deb,appimage >/tmp/log 2>&1 || { tail -30 /tmp/log; exit 1; }; } &&
      cp /target/release/bundle/appimage/*.AppImage /target/release/bundle/deb/*.deb /src/bundles/ &&
      $give_back"
  if [[ -n "$host" ]]; then
    rsync -a -e "ssh ${SSH_OPTS[*]}" "$host:$dir/bundles/" "$src/bundles/"
  fi

  file "$src/bundles/$bin" | grep -qE "statically linked|static-pie linked" \
    || { echo "error: the Linux $arch installer isn't statically linked" >&2; return 1; }
  cp "$src/bundles/$bin" "$out/"
  cp "$src"/bundles/*.AppImage "$out/$app_name.AppImage"
  cp "$src"/bundles/*.deb "$out/$app_name.deb"
  readme "$out" "./$bin" "Run $app_name.AppImage (chmod +x it first) for a guided install, or install $app_name.deb on Debian or Ubuntu; it then stays in the tray as the companion. Both need glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36 and later)."
  tar -C "$out" -czf "$OUT/Apparition-Installer-$VERSION-linux-$arch.tar.gz" "$bin" "$app_name.AppImage" "$app_name.deb" README.txt
}

LINUX_PIDS=()
LINUX_STARTED=()
# A failure here leaves no Linux build running on its own.
trap 'for pid in "${LINUX_PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done' EXIT
if has linux; then
  for arch in ${LINUX_ARCHS:-x86_64 arm64}; do
    ( set -e; linux_build "$arch" ) > "$LOGS/linux-$arch.log" 2>&1 &
    LINUX_PIDS+=("$!")
    LINUX_STARTED+=("$arch")
  done
  echo "==> Linux ${LINUX_ARCHS:-x86_64 arm64}${LINUX_X86_64_HOST:+ (x86_64 on $LINUX_X86_64_HOST)} building in the background (build/release/linux-<arch>.log)"
fi

# ------------------------------------------------------------------ the app
if has app; then
  xcodegen generate --quiet
  # One IPA per platform: <build-core.sh platform> <SDK> <name in the file name>.
  for spec in "visionos xros visionOS" "ios iphoneos iOS" "tvos appletvos tvOS"; do
    read -r core sdk name <<< "$spec"
    echo "==> core for $name"
    scripts/build-core.sh "$core"
    # APPARITION_RELEASE leaves out what only development builds carry (the experiment endpoints,
    # the release-site override). build-and-sign also builds Release, so DEBUG can't tell them apart.
    echo "==> app for $name (Release, unsigned)"
    xcodebuild -project Apparition.xcodeproj -scheme Apparition -configuration Release \
      -sdk "$sdk" -destination "generic/platform=$name" -derivedDataPath "$WORK/DerivedData" \
      MARKETING_VERSION="$VERSION" CURRENT_PROJECT_VERSION="$BUILD" \
      SWIFT_ACTIVE_COMPILATION_CONDITIONS='$(inherited) APPARITION_RELEASE' \
      CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY="" \
      -quiet build
    app="$WORK/DerivedData/Build/Products/Release-$sdk/Apparition.app"
    [[ -d "$app" ]] || { echo "error: no Apparition.app at $app" >&2; exit 1; }
    built_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Info.plist")"
    [[ "$built_version" == "$VERSION" ]] || { echo "error: the $name app says $built_version" >&2; exit 1; }
    # A free Apple ID pays an App ID for every extension, and Apparition is meant to have none.
    if [[ -d "$app/PlugIns" || -d "$app/Extensions" ]]; then
      echo "error: the $name Apparition.app embeds extensions; each costs a free account an App ID" >&2
      exit 1
    fi
    rm -rf "$WORK/ipa"
    mkdir -p "$WORK/ipa/Payload"
    ditto "$app" "$WORK/ipa/Payload/Apparition.app"
    (cd "$WORK/ipa" && zip -qryX "$OUT/Apparition-$VERSION-$name.ipa" Payload)
  done
fi

# ------------------------------------------------------------------ the installers
cd "$ROOT/installer"
if has macos; then

  echo "==> installer for macOS (arm64 + x86_64)"
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    MACOSX_DEPLOYMENT_TARGET=11.0 cargo build -q --release --target "$target"
  done
  mkdir -p "$WORK/macos"
  xcrun lipo -create -output "$WORK/macos/$bin" \
    target/aarch64-apple-darwin/release/$bin target/x86_64-apple-darwin/release/$bin
  # This Mac's anisette on macOS 27 (core/src/anisette/native.rs): AuthKit's private entitlements,
  # so macOS runs it only with SIP and AMFI off. It sits next to whatever uses it.
  helper=apparition-anisette-helper
  xcrun lipo -create -output "$WORK/macos/$helper" \
    target/aarch64-apple-darwin/release/$helper target/x86_64-apple-darwin/release/$helper
  codesign -f -s - --entitlements "$ROOT/installer/anisette-helper.entitlements" "$WORK/macos/$helper"
  echo "==> desktop app for macOS (universal)"
  app_name="Apparition"
  (cd "$ROOT/desktop" && MACOSX_DEPLOYMENT_TARGET=11.0 cargo tauri build --target universal-apple-darwin --bundles app)
  ditto "$ROOT/desktop/target/universal-apple-darwin/release/bundle/macos/$app_name.app" "$WORK/macos/$app_name.app"
  # The helper keeps its own signature; the app is sealed again around it (not --deep, which
  # would sign the helper over without its entitlements).
  cp "$WORK/macos/$helper" "$WORK/macos/$app_name.app/Contents/MacOS/$helper"
  codesign -f -s - "$WORK/macos/$app_name.app"
  readme "$WORK/macos" "./$bin" "Open $app_name.app for a guided install; it then stays in the menu bar as the companion. It isn't notarized, so macOS blocks it until you clear the download quarantine. Move $app_name.app to Applications, then run in Terminal: xattr -dr com.apple.quarantine /Applications/$app_name.app (and for the command line, in this folder: xattr -d com.apple.quarantine $bin $helper). Or open it once and click Open Anyway in System Settings > Privacy & Security. Don't re-sign it with codesign: that strips the anisette helper's entitlements."
  (cd "$WORK/macos" && zip -qryX "$OUT/Apparition-Installer-$VERSION-macos-universal.zip" "$bin" "$helper" "$app_name.app" README.txt)
fi

if has windows; then
  echo "==> installer for Windows (x86_64)"
  cargo build -q --release --target x86_64-pc-windows-gnu
  mkdir -p "$WORK/windows"
  cp "target/x86_64-pc-windows-gnu/release/$bin.exe" "$WORK/windows/"
  echo "==> desktop app for Windows (x86_64)"
  (cd "$ROOT/desktop" && cargo build -q --release --features custom-protocol --target x86_64-pc-windows-gnu)
  cp "$ROOT/desktop/target/x86_64-pc-windows-gnu/release/apparition-app.exe" "$WORK/windows/$app_name.exe"
  readme "$WORK/windows" "$bin.exe" "Open $app_name.exe for a guided install; it then stays in the tray as the companion (it uses Microsoft Edge WebView2, which Windows 10 and 11 normally have)."
  (cd "$WORK/windows" && zip -qX "$OUT/Apparition-Installer-$VERSION-windows-x86_64.zip" "$bin.exe" "$app_name.exe" README.txt)

fi

failed=0
for i in "${!LINUX_PIDS[@]}"; do
  if wait "${LINUX_PIDS[$i]}"; then
    echo "==> Linux ${LINUX_STARTED[$i]} done"
  else
    echo "error: the Linux ${LINUX_STARTED[$i]} build failed; the end of build/release/linux-${LINUX_STARTED[$i]}.log:" >&2
    tail -30 "$LOGS/linux-${LINUX_STARTED[$i]}.log" >&2
    failed=1
  fi
done
LINUX_PIDS=()
[[ "$failed" == 0 ]] || exit 1

rm -rf "$WORK"
echo "==> built into $OUT:"
(cd "$OUT" && ls -l | sed 's/^/    /')
echo "    Upload them, then on the site: scripts/publish.py add $VERSION $OUT --url-base <where they are>"
