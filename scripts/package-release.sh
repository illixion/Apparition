#!/usr/bin/env bash
#
# Builds the files of an Apparition release into build/release/<version>/, named the way
# the release site (apparition.illixion.com, scripts/publish.py) expects:
#
#   Apparition-<version>-<visionOS|iOS|tvOS>.ipa            unsigned; signed on install
#   Apparition-Installer-<version>-macos-universal.zip      arm64 + x86_64: the desktop app
#                                                           and the command line
#   Apparition-Installer-<version>-windows-<arch>.zip       the same two for Windows: x86_64,
#                                                           and arm64 with WINDOWS_ARCHS=arm64
#                                                           on a Windows ARM PC (CI builds each
#                                                           on Windows: windows-2025 and
#                                                           windows-11-arm)
#   Apparition-Setup-<version>-windows-<arch>.exe           those files as a setup program
#                                                           (scripts/windows/setup.nsi): it makes
#                                                           the firewall's rules, and for a PC
#                                                           set up for everyone, the service
#   Apparition-Installer-<version>-linux-<arch>.tar.gz      with --linux, built in Docker: the
#                                                           static command line, and the
#                                                           desktop app as an AppImage and a .deb
#
# and the app's symbols into build/dsyms/<version>/, apart from those, because the release site
# takes exactly the files above (publish.py refuses any other name):
#
#   Apparition-<version>-<visionOS|iOS|tvOS>-dSYMs.zip      for reading crash reports from that
#                                                           IPA; published on the GitHub release
#
#   scripts/package-release.sh 0.2.0
#   scripts/package-release.sh 0.2.0 --linux        # also Linux x86_64 and arm64
#   LINUX_X86_64_HOST=tuxpc scripts/package-release.sh 0.2.0 --linux   # x86_64 natively there
#   scripts/package-release.sh 0.2.0 --only-app     # just the IPAs
#   scripts/package-release.sh 0.2.0 --parts app,macos,windows,linux   # any of these (CI builds
#                                                    # them on separate runners); LINUX_ARCHS
#                                                    # narrows linux to x86_64 or arm64, and
#                                                    # WINDOWS_ARCHS picks windows' (x86_64)
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
# signs that with a release key. Nothing here holds a key. Before it signs, scripts/audit-release.sh
# rebuilds the macOS files from the attested commit and requires the same contents, so those are
# built reproducibly: from one fixed path ($FIXED below), with the toolchain in
# scripts/release-toolchain, and zipped with fixed dates and order.
#
# Needs: Xcode with the visionOS, iOS and tvOS SDKs, xcodegen, rustup targets
# aarch64-apple-visionos, aarch64-apple-ios, aarch64-apple-tvos,
# aarch64-apple-darwin, x86_64-apple-darwin and x86_64-pc-windows-gnu (with mingw-w64 and
# makensis from Homebrew), the Tauri CLI for the desktop app (scripts/install-tauri-cli.sh), and
# Docker for --linux.
#
# The release's Windows files are built on Windows, with MSVC, in Git Bash:
# `WINDOWS_ARCHS=x86_64` (x86_64-pc-windows-msvc) on an x64 PC and `WINDOWS_ARCHS=arm64`
# (aarch64-pc-windows-msvc) on an ARM one, with the Visual Studio C++ tools and LLVM on PATH
# (ring and aws-lc compile their C and assembly with LLVM's clang on ARM64; its llvm-objdump reads
# the programs' imports, below). mingw has no ARM64 toolchain. The MSVC builds link the WebView2
# loader in, so their zips have no WebView2Loader.dll. On a Mac, x86_64 is cross-compiled with
# mingw-w64 instead (x86_64-pc-windows-gnu, which ships the DLL), for trying a change on a PC
# without a Windows build machine. Either way no program may need a compiler's runtime DLL
# (windows_runtime_check). Setup is made with NSIS (makensis on PATH, or where its own installer
# puts it); the setup program itself is x86 on every PC, which Windows on ARM runs.
#
# The Linux desktop app is built in an image made from scripts/desktop-linux.Dockerfile
# (Ubuntu 22.04 with WebKitGTK and the Tauri CLI), tagged per architecture and reused.

set -euo pipefail

# The physical path (pwd -P): the macOS files are built only from FIXED, below, and CI reaches it
# through a symlink.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
VERSION="${1:?usage: package-release.sh <version> [--linux] [--only-app] [--parts LIST] [--allow-dirty] [--into DIR] [--clean]}"
shift
LINUX=0
ONLY_APP=0
PARTS=""
ALLOW_DIRTY=0
INTO=""
CLEAN=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --linux) LINUX=1 ;;
    --only-app) ONLY_APP=1 ;;
    --parts) PARTS="${2:?--parts needs a list: app,macos,windows,linux}"; shift ;;
    # For trying the packaging itself; such a build isn't one to publish.
    --allow-dirty) ALLOW_DIRTY=1 ;;
    # Put the files into DIR, leaving what is already there (the macOS build from FIXED, below).
    --into) INTO="${2:?--into needs a folder}"; shift ;;
    # Build the macOS files from nothing: no compiled dependencies kept from an earlier build
    # (scripts/audit-release.sh).
    --clean) CLEAN=1 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

# The macOS release files are reproducible: the same commit gives the same bytes on any Mac with
# the toolchain in scripts/release-toolchain, which is how scripts/audit-release.sh checks what CI
# built before a release is signed. Cargo puts the source folder into every crate's symbol names,
# so they are always built from this one folder, here and on CI; a checkout elsewhere copies its
# commit there first (see macos_from_fixed).
FIXED=/private/tmp/apparition-build/Apparition

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

# NSIS, for Windows' setup program: said now rather than after the build it would follow.
MAKENSIS=""
if has windows; then
  for candidate in makensis "/c/Program Files (x86)/NSIS/makensis.exe" "/c/Program Files/NSIS/makensis.exe"; do
    if command -v "$candidate" >/dev/null; then MAKENSIS="$candidate"; break; fi
  done
  [[ -n "$MAKENSIS" ]] || { echo "error: the windows part needs NSIS's makensis (brew install makensis; on Windows, winget install NSIS.NSIS or choco install nsis)" >&2; exit 1; }
fi

cd "$ROOT"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: $VERSION isn't major.minor.patch (Cargo needs all three)" >&2; exit 1; }

app_version="$(sed -n 's/^ *MARKETING_VERSION: *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p' project.yml | head -1)"
installer_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' installer/Cargo.toml | head -1)"
desktop_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' desktop/Cargo.toml | head -1)"
{ ! has app || [[ "$app_version" == "$VERSION" ]]; } \
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
if [[ -n "$INTO" ]]; then
  mkdir -p "$INTO"
  OUT="$(cd "$INTO" && pwd -P)"
  rm -rf "$WORK"
else
  rm -rf "$OUT" "$WORK"
fi
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
$2 install --apple-id you@example.com
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
  mkdir -p "$src/scripts" "$out"
  rsync -a --exclude target "$ROOT/Cargo.toml" "$ROOT/Cargo.lock" "$ROOT/core" "$ROOT/installer" "$ROOT/desktop" "$ROOT/service" "$ROOT/release-signers" "$src/"
  cp "$ROOT/scripts/desktop-linux.Dockerfile" "$ROOT/scripts/install-tauri-cli.sh" "$src/scripts/"
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
  cp "$out/$app_name.AppImage" "$OUT/Apparition-Installer-$VERSION-linux-$arch.AppImage"
  cp "$out/$app_name.deb" "$OUT/Apparition-Installer-$VERSION-linux-$arch.deb"
  readme "$out" "./$bin" "Run $app_name.AppImage (chmod +x it first) for a guided install, or install $app_name.deb on Debian or Ubuntu; it then stays in the tray as the companion. Both need glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36 and later)."
  tar -C "$out" -czf "$OUT/Apparition-Installer-$VERSION-linux-$arch.tar.gz" "$bin" "$app_name.AppImage" "$app_name.deb" README.txt
}

LINUX_PIDS=()
LINUX_STARTED=()
# A failure here leaves no Linux build running on its own.
# (The ${…+…} form, because macOS's bash 3.2 calls an empty array unbound under set -u.)
trap 'for pid in ${LINUX_PIDS[@]+"${LINUX_PIDS[@]}"}; do kill "$pid" 2>/dev/null || true; done' EXIT
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
  SYMBOLS="$ROOT/build/dsyms/$VERSION"
  rm -rf "$SYMBOLS"
  mkdir -p "$SYMBOLS"
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
    # The symbols of this very binary. Signing on the device changes the signature, not the
    # binary's UUID, so they match every copy installed from this IPA, and a crash report from
    # any of them (MetricKit's binaryUUID, an .ips) symbolicates against them.
    products="$WORK/DerivedData/Build/Products/Release-$sdk"
    executable="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app/Info.plist")"
    dsym="$products/Apparition.app.dSYM"
    [[ -d "$dsym" ]] || { echo "error: no dSYM for the $name app at $dsym" >&2; exit 1; }
    app_uuids="$(xcrun dwarfdump --uuid "$app/$executable" | awk '{print $2}' | sort)"
    dsym_uuids="$(xcrun dwarfdump --uuid "$dsym" | awk '{print $2}' | sort)"
    [[ -n "$app_uuids" && "$app_uuids" == "$dsym_uuids" ]] \
      || { echo "error: the $name dSYM ($dsym_uuids) isn't the app's ($app_uuids)" >&2; exit 1; }
    # Every dSYM the build made (the app's, and any framework's), in one zip per platform.
    (cd "$products" && zip -qryX "$SYMBOLS/Apparition-$VERSION-$name-dSYMs.zip" ./*.dSYM)
    # With the dSYM made, the binary's debug map (stabs pointing at the build's object files,
    # 3.5 MB in 0.4.5) is dead weight, so it goes, as an Xcode archive would do. strip -S keeps
    # the UUID and every function symbol, which on-device backtraces and the profiler read.
    xcrun strip -S "$app/$executable"
    [[ "$(xcrun dwarfdump --uuid "$app/$executable" | awk '{print $2}' | sort)" == "$app_uuids" ]] \
      || { echo "error: stripping changed the $name app's UUID" >&2; exit 1; }
    rm -rf "$WORK/ipa"
    mkdir -p "$WORK/ipa/Payload"
    ditto "$app" "$WORK/ipa/Payload/Apparition.app"
    (cd "$WORK/ipa" && zip -qryX "$OUT/Apparition-$VERSION-$name.ipa" Payload)
  done
fi

# ------------------------------------------------------------------ the installers
# The command line and the desktop app share one workspace (Cargo.toml). Each target builds both
# in one cargo invocation: cargo unifies features only across what one invocation builds, so this
# is what lets them share one build of the core and its dependencies.
cd "$ROOT"
both=(-p apparition-installer -p apparition-desktop --features apparition-desktop/custom-protocol)

# A folder's entries zipped the same way every time: in name order, every file and folder dated to
# the commit, with no owner or extended attributes (-X), so only names, contents and modes vary.
STAMP="$(TZ=UTC git log -1 --format=%cd --date=format-local:%Y%m%d%H%M.%S)"
zip_tree() { # <folder> <zip> <entry>...  (each entry taken whole, relative to the folder)
  local folder="$1" zip="$2"
  shift 2
  (cd "$folder" && find "$@" -exec env TZ=UTC touch -h -t "$STAMP" {} + \
    && find "$@" | LC_ALL=C sort | TZ=UTC zip -qX -@ "$zip")
}

# This commit's macOS files, built in FIXED and put into OUT. One build at a time uses FIXED; its
# target/ stays between builds (unless --clean), so a later release rebuilds only what changed.
macos_from_fixed() {
  local base lock status=0
  base="$(dirname "$FIXED")"
  lock="$base/lock"
  mkdir -p "$base"
  if ! mkdir "$lock" 2>/dev/null; then
    echo "error: another build is using $FIXED (if none is running, remove $lock)" >&2
    return 1
  fi
  [[ -z "$(git status --porcelain)" ]] || echo "warning: the macOS files are built from the commit, without your uncommitted changes" >&2
  echo "==> macOS files from $FIXED (the same path on every machine, so the same bytes)"
  if (
    set -e
    mkdir -p "$FIXED"
    [[ -d "$FIXED/.git" ]] || git -C "$FIXED" init -q
    git -C "$FIXED" fetch -q --no-tags "$ROOT" HEAD
    git -C "$FIXED" checkout -q --force --detach FETCH_HEAD
    if [[ "$CLEAN" == 1 ]]; then git -C "$FIXED" clean -q -ffdx; else git -C "$FIXED" clean -q -ffdx -e /target; fi
    [[ "$(git -C "$FIXED" rev-parse HEAD)" == "$(git rev-parse HEAD)" ]]
    "$FIXED/scripts/package-release.sh" "$VERSION" --parts macos --into "$OUT"
  ); then :; else status=$?; fi
  rmdir "$lock"
  return "$status"
}

if has macos && [[ "$ROOT" != "$FIXED" ]]; then
  macos_from_fixed
elif has macos; then
  [[ "$CLEAN" == 1 ]] && rm -rf "$FIXED/target"
  # Files the build writes get the same modes on every machine.
  umask 022
  # The pinned toolchain (scripts/release-toolchain), checked rather than trusted: another Rust,
  # Xcode or Tauri CLI builds other bytes, and the audit of this release would fail.
  # shellcheck source=release-toolchain
  . "$ROOT/scripts/release-toolchain"
  export RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN"
  rustc_version="$(rustc --version)"
  [[ "$rustc_version" == "rustc $RUST_TOOLCHAIN "* ]] \
    || { echo "error: rustc is $rustc_version, not $RUST_TOOLCHAIN (rustup toolchain install $RUST_TOOLCHAIN)" >&2; exit 1; }
  xcode_build="$(xcodebuild -version | sed -n 's/^Build version //p')"
  if [[ "$xcode_build" != "$XCODE_BUILD" && -z "${DEVELOPER_DIR:-}" ]]; then
    # The selected Xcode isn't the pinned one: use an /Applications/Xcode*.app that is, as the audit does.
    for app in /Applications/Xcode*.app; do
      if [[ "$(DEVELOPER_DIR="$app/Contents/Developer" xcodebuild -version 2>/dev/null | sed -n 's/^Build version //p')" == "$XCODE_BUILD" ]]; then
        export DEVELOPER_DIR="$app/Contents/Developer"
        xcode_build="$XCODE_BUILD"
        echo "==> using $app (Xcode build $XCODE_BUILD, scripts/release-toolchain)"
        break
      fi
    done
  fi
  [[ "$xcode_build" == "$XCODE_BUILD" ]] || {
    echo "error: this is Xcode build $xcode_build; the macOS release files are built with $XCODE_BUILD (scripts/release-toolchain)." >&2
    echo "       Install that Xcode beside this one as /Applications/Xcode-<version>.app, or select it with DEVELOPER_DIR." >&2
    exit 1
  }
  tauri_cli="$(command -v cargo-tauri || true)"
  tauri_want="tauri-cli $(sed -n 's/^version=//p' "$ROOT/scripts/install-tauri-cli.sh")"
  [[ -n "$tauri_cli" && "$("$tauri_cli" tauri --version)" == "$tauri_want" ]] \
    || { echo "error: the Tauri CLI on PATH isn't $tauri_want (scripts/install-tauri-cli.sh)" >&2; exit 1; }
  # Paths that differ between machines, out of the binaries; everything else is under FIXED. This
  # replaces any RUSTFLAGS or CFLAGS from outside, which would change the bytes too.
  cargo_home="$(cd "${CARGO_HOME:-$HOME/.cargo}" && pwd -P)"
  rustup_home="$(cd "${RUSTUP_HOME:-$HOME/.rustup}" && pwd -P)"
  # `--cfg aes_armv8` is what .cargo/config.toml sets (see there): this RUSTFLAGS would replace it.
  export RUSTFLAGS="--remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$rustup_home=/rustup --cfg aes_armv8"
  export CFLAGS="-ffile-prefix-map=$cargo_home=/cargo -ffile-prefix-map=$rustup_home=/rustup"
  export MACOSX_DEPLOYMENT_TARGET=11.0
  # Tools that stamp a build date take this one instead (the vendored OpenSSL put "built on: <now>"
  # into the binaries). A constant, not the commit's date: CI restores compiled dependencies from a
  # cache an earlier commit built, and they must match what a build from nothing gives.
  export SOURCE_DATE_EPOCH=1767225600 # 2026-01-01

  echo "==> installer and desktop app for macOS (arm64 + x86_64; Rust $RUST_TOOLCHAIN, Xcode $XCODE_BUILD)"
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build -q --release --locked --target "$target" "${both[@]}"
  done
  mkdir -p "$WORK/macos"
  xcrun lipo -create -output "$WORK/macos/$bin" \
    target/aarch64-apple-darwin/release/$bin target/x86_64-apple-darwin/release/$bin
  helper=apparition-anisette-helper
  xcrun lipo -create -output "$WORK/macos/$helper" \
    target/aarch64-apple-darwin/release/$helper target/x86_64-apple-darwin/release/$helper
  codesign -f -s - --entitlements "$ROOT/installer/anisette-helper.entitlements" "$WORK/macos/$helper"
  echo "==> desktop app for macOS (universal)"
  app_name="Apparition"
  # Bundled from the binaries above, the universal one put where `tauri build` would put it:
  # `tauri build` would compile the desktop app again on its own, with other features.
  mkdir -p target/universal-apple-darwin/release
  # tauri bundle looks for the binary under tauri.conf's mainBinaryName, not the cargo name
  xcrun lipo -create -output "target/universal-apple-darwin/release/$app_name" \
    target/aarch64-apple-darwin/release/apparition-app target/x86_64-apple-darwin/release/apparition-app
  rm -rf target/universal-apple-darwin/release/bundle
  # The CLI checked above, called the way cargo calls it.
  (cd desktop && "$tauri_cli" tauri bundle --target universal-apple-darwin --bundles app)
  ditto "target/universal-apple-darwin/release/bundle/macos/$app_name.app" "$WORK/macos/$app_name.app"
  # The helper keeps its own signature; the app is sealed again around it (not --deep, which
  # would sign the helper over without its entitlements).
  cp "$WORK/macos/$helper" "$WORK/macos/$app_name.app/Contents/MacOS/$helper"
  # The command line rides inside the app too, so installing or updating the app updates it
  # (`apparition link` makes a symlink to it on PATH). Not named `apparition`: the app's own
  # executable is `Apparition`, which a case-insensitive volume would merge with it.
  cp "$WORK/macos/$bin" "$WORK/macos/$app_name.app/Contents/MacOS/$bin-cli"
  codesign -f -s - "$WORK/macos/$app_name.app/Contents/MacOS/$bin-cli"
  # The anisette VM (core/src/vm.rs): its runner and the two programs it puts in the guest, arm64 only, since
  # macOS runs in a VM only on Apple silicon. The runner keeps its virtualization entitlement.
  bash "$ROOT/selfhost/macos-vm/build.sh"
  cp "$ROOT/build/macos-vm/vm" "$WORK/macos/$app_name.app/Contents/MacOS/apparition-vm"
  cp "$ROOT/build/macos-vm/guest-status" "$WORK/macos/$app_name.app/Contents/MacOS/apparition-vm-guest-status"
  cp "$ROOT/build/macos-vm/guest-bridge" "$WORK/macos/$app_name.app/Contents/MacOS/apparition-vm-guest-bridge"
  codesign -f -s - --entitlements "$ROOT/selfhost/macos-vm/runner.entitlements" "$WORK/macos/$app_name.app/Contents/MacOS/apparition-vm"
  codesign -f -s - "$WORK/macos/$app_name.app"
  # The anisette VM's programs on their own (core/src/vm_update.rs): what the app hands a VM that is set up, which
  # checks this release's signature itself before it installs them. arm64 only, like the VM; the helper gets its
  # entitlements in its own signature, as in the app.
  mkdir -p "$WORK/vm-guest"
  cp "target/aarch64-apple-darwin/release/$bin" "$WORK/vm-guest/apparition"
  cp "target/aarch64-apple-darwin/release/$helper" "$WORK/vm-guest/$helper"
  codesign -f -s - --entitlements "$ROOT/installer/anisette-helper.entitlements" "$WORK/vm-guest/$helper"
  cp "$ROOT/build/macos-vm/guest-status" "$ROOT/build/macos-vm/guest-bridge" "$ROOT/selfhost/macos-vm/report-status.sh" "$WORK/vm-guest/"
  zip_tree "$WORK/vm-guest" "$OUT/Apparition-VMGuest-$VERSION-macos-arm64.zip" apparition "$helper" guest-status guest-bridge report-status.sh
  readme "$WORK/macos" "./$bin" "Open $app_name.app for a guided install; it then stays in the menu bar as the companion. It isn't notarized, so macOS blocks it until you clear the download quarantine. Move $app_name.app to Applications, then run in Terminal: xattr -dr com.apple.quarantine /Applications/$app_name.app (and for the command line, in this folder: xattr -d com.apple.quarantine $bin $helper). The app holds the command line too: run /Applications/$app_name.app/Contents/MacOS/$bin-cli link once to put $bin on your PATH. Or open it once and click Open Anyway in System Settings > Privacy & Security. Don't re-sign it with codesign: that strips the anisette helper's entitlements."
  zip_tree "$WORK/macos" "$OUT/Apparition-Installer-$VERSION-macos-universal.zip" "$bin" "$helper" "$app_name.app" README.txt
fi

# Every DLL a Windows program loads at start has to be on every Windows 10 and 11 PC, stripped-down
# ("debloated") ones included, or the program doesn't start: Windows' own, and WebView2Loader.dll,
# which the mingw build ships beside it. A compiler runtime is not one of those: VCRUNTIME140.dll
# and MSVCP140.dll come with the Visual C++ Redistributable, libgcc and libwinpthread with mingw.
windows_runtime_check() {
  local dump="" tool file dlls bad
  for tool in llvm-objdump x86_64-w64-mingw32-objdump objdump; do
    if command -v "$tool" >/dev/null && "$tool" -p "$1" 2>/dev/null | grep -q 'DLL Name'; then dump="$tool"; break; fi
  done
  if [[ -z "$dump" ]]; then echo "error: no objdump here reads Windows programs' imports (LLVM's llvm-objdump, or mingw-w64's)" >&2; exit 1; fi
  for file in "$@"; do
    dlls="$("$dump" -p "$file" | sed -n 's/^[[:space:]]*DLL Name:[[:space:]]*//p' | tr 'A-Z' 'a-z' | sort -u)"
    bad="$(printf '%s\n' "$dlls" | grep -E '^(vcruntime|msvcp|concrt|vccorlib|ucrtbased|libgcc|libwinpthread|libstdc|libssp)' || true)"
    if [[ -n "$bad" ]]; then
      echo "error: $(basename "$file") needs a DLL that Windows doesn't have: $(echo $bad)" >&2
      exit 1
    fi
  done
}

if has windows; then
  for arch in ${WINDOWS_ARCHS:-x86_64}; do
    case "$arch" in
      # MSVC when this runs on Windows (CI's windows-2025 runner), like ARM64: one toolchain for
      # both release builds, the WebView2 loader linked in and the VC runtime too (installer/build.rs).
      # On a Mac, mingw-w64's cross-compiler, for building and trying Windows files locally.
      x86_64) case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) target=x86_64-pc-windows-msvc ;; *) target=x86_64-pc-windows-gnu ;; esac ;;
      # MSVC's ARM64 compiler miscompiles OpenSSL under the /Gs0 its VC-WIN64-ARM config passes
      # (a __chkstk call before the link register is saved in tls_parse_all_extensions, so every
      # TLS server handshake crashes: openssl/openssl#26239). /Gs in CPPFLAGS lands after those
      # flags, and no other build reads it (cc-rs would hand a plain CFLAGS to ring's clang).
      # Written -Gs, not /Gs: Git Bash rewrites a value starting with / into a Windows path.
      arm64) target=aarch64-pc-windows-msvc; export CPPFLAGS="${CPPFLAGS:--Gs}" ;;
      *) echo "error: WINDOWS_ARCHS takes x86_64 and arm64, not $arch" >&2; exit 2 ;;
    esac
    echo "==> installer and desktop app for Windows ($arch)"
    cargo build -q --release --locked --target "$target" "${both[@]}"
    windows_runtime_check "target/$target/release/$bin.exe" "target/$target/release/apparition-app.exe"
    mkdir -p "$WORK/windows-$arch"
    # Windows is case-insensitive: apparition.exe collides with the desktop's Apparition.exe.
    cp "target/$target/release/$bin.exe" "$WORK/windows-$arch/$bin-cli.exe"
    cp "target/$target/release/apparition-app.exe" "$WORK/windows-$arch/$app_name.exe"
    entries=("$bin-cli.exe" "$app_name.exe")
    keep="The command line is $bin-cli.exe; its name must differ from the desktop app on Windows."
    # The mingw build loads WebView2 through this DLL; the MSVC one links the loader in.
    if [[ -f "target/$target/release/WebView2Loader.dll" ]]; then
      cp "target/$target/release/WebView2Loader.dll" "$WORK/windows-$arch/"
      entries+=(WebView2Loader.dll)
      keep="Keep WebView2Loader.dll beside it. $keep"
    elif [[ "$arch" == x86_64 ]]; then
      # The x64 zip carries it anyway, unused: up to 0.4.5 the x64 app's updater (mingw builds)
      # refuses a zip without it, and swaps it in with the rest. Microsoft's own copy, from the
      # webview2-com-sys release the lock file names.
      webview2_sys="$(sed -n '/^name = "webview2-com-sys"$/{n;s/^version = "\(.*\)"$/\1/p;}' Cargo.lock | head -1)"
      loader_dll="$(ls "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/"webview2-com-sys-$webview2_sys/x64/WebView2Loader.dll" 2>/dev/null | head -1)"
      [[ -n "$webview2_sys" && -f "$loader_dll" ]] || { echo "error: no WebView2Loader.dll from webview2-com-sys $webview2_sys for the x64 zip" >&2; exit 1; }
      cp "$loader_dll" "$WORK/windows-$arch/WebView2Loader.dll"
      entries+=(WebView2Loader.dll)
    fi
    readme "$WORK/windows-$arch" "$bin-cli.exe" "Open $app_name.exe for a guided install; it then stays in the tray as the companion (it uses Microsoft Edge WebView2, which Windows 10 and 11 normally have). $keep"
    entries+=(README.txt)
    zip_path="$OUT/Apparition-Installer-$VERSION-windows-$arch.zip"
    if command -v zip >/dev/null; then
      (cd "$WORK/windows-$arch" && zip -qX "$zip_path" "${entries[@]}")
    elif command -v 7z >/dev/null; then
      # Git Bash on Windows has no zip; the GitHub images have 7-Zip. Flat files either way.
      (cd "$WORK/windows-$arch" && 7z a -tzip -bso0 -bsp0 "$(cygpath -w "$zip_path")" "${entries[@]}")
    else
      # Windows' own, for a PC with neither.
      (cd "$WORK/windows-$arch" && powershell.exe -NoProfile -Command "Compress-Archive -Path $(printf "'%s'," "${entries[@]}" | sed 's/,$//') -DestinationPath '$(cygpath -w "$zip_path")'")
    fi
    # The same files as a setup program. makensis takes its options as -D on a Mac and on
    # Windows alike; in Git Bash the paths it is given have to be Windows' own.
    native() { if command -v cygpath >/dev/null; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
    loader=()
    [[ -f "$WORK/windows-$arch/WebView2Loader.dll" ]] && loader=(-DLOADER)
    "$MAKENSIS" -V2 -DVERSION="$VERSION" -DARCH="${arch/x86_64/x64}" \
      -DSOURCE="$(native "$WORK/windows-$arch")" -DOUTFILE="$(native "$OUT/Apparition-Setup-$VERSION-windows-$arch.exe")" \
      -DICON="$(native "$ROOT/desktop/icons/icon.ico")" ${loader[@]+"${loader[@]}"} "$(native "$ROOT/scripts/windows/setup.nsi")"
  done
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
if has app; then
  echo "==> symbols in $SYMBOLS (for the GitHub release, not the site):"
  (cd "$SYMBOLS" && ls -l | sed 's/^/    /')
fi
echo "    Upload them, then on the site: scripts/publish.py add $VERSION $OUT --url-base <where they are>"
