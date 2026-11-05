#!/usr/bin/env bash
set -euo pipefail

# Build the macOS desktop app (the Tauri app in desktop/), install it to /Applications and
# restart it: quits the running copy first, then opens the freshly installed one. For a fast
# local rebuild-test loop. The app is also the companion devices talk to, and macOS keeps its
# Local Network and Automation grants with the copy in /Applications, so that copy is the one
# to test, not a build under target/.
#
# It builds for this Mac's architecture only (scripts/package-release.sh makes the universal
# release) and bundles the same pieces a release has: the app, the `apparition-cli` command
# line next to it, the anisette helper and, on Apple silicon, the anisette VM's runner and the
# two programs it puts in the VM (selfhost/macos-vm/build.sh).
#
# Usage:   [VERBOSE=1] scripts/install-desktop.sh
#          Build output goes to build/install-desktop.log; VERBOSE=1 streams it.
#
# Why signing matters: macOS ties privacy grants to the app's code-signing identity. An ad-hoc
# signature ("-") gets a new cdhash on every build, so each rebuild looks like a new app and
# the grants are asked for again. Signing the app with your stable Apple Development identity
# keeps them. By default this finds one in the login keychain and signs with it; with none it
# falls back to ad-hoc and says so. The anisette helper is always ad-hoc: its private
# AuthKit entitlements only work that way (installer/anisette-helper.entitlements), and
# signing it over would strip them.
#
# Options (env):
#   SIGN_IDENTITY="…"   codesign identity for the app and the command line. Default: the first
#                       Apple Development identity found, else "-" (ad-hoc). A name or a
#                       40-character SHA-1 hash pins one.
#   DEST_DIR=…          where the app goes (default /Applications)
#   LAUNCH=0            install without opening the app
#   LINK=0              leave `apparition` on PATH alone. By default `apparition-cli link` points
#                       ~/.local/bin/apparition at the installed app's command line (a symlink, so
#                       it follows every reinstall); LINK_DIR=… picks another directory. A real file
#                       there is someone's own install and is left alone, with a warning.
#   VERBOSE=1           stream the build instead of logging it

APP_NAME="Apparition"
BUNDLE_ID="com.illixion.apparition.desktop"
CLI=apparition
HELPER=apparition-anisette-helper

if [[ "${1:-}" == -h || "${1:-}" == --help ]]; then
  sed -n '4,/^$/p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
fi
[[ "$(uname -s)" == Darwin ]] || { echo "✗ This installs the macOS app; run it on a Mac." >&2; exit 1; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST_DIR="${DEST_DIR:-/Applications}"
DEST="$DEST_DIR/$APP_NAME.app"
BUILD_LOG="$ROOT/build/install-desktop.log"
cd "$ROOT"

# Resolve the signing identity, as above.
if [[ -z "${SIGN_IDENTITY:-}" ]]; then
  SIGN_IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
    | awk -F'"' '/Apple Development/ { print $2; exit }')"
  if [[ -n "$SIGN_IDENTITY" ]]; then
    echo "▶ Auto-detected signing identity: $SIGN_IDENTITY"
  else
    SIGN_IDENTITY="-"
    echo "⚠ No 'Apple Development' identity found: signing ad-hoc (\"-\")." >&2
    echo "  macOS will ask for the app's permissions again after every rebuild. Open Xcode" >&2
    echo "  once to provision an identity, or pass SIGN_IDENTITY." >&2
  fi
fi

command -v cargo >/dev/null || { echo "✗ cargo not found; install Rust (rustup.rs)." >&2; exit 1; }
if ! cargo tauri --version >/dev/null 2>&1; then
  echo "▶ Installing the Tauri CLI…"
  scripts/install-tauri-cli.sh
fi

mkdir -p "$(dirname "$BUILD_LOG")"
# Cargo and the bundler print every crate they compile; keep that in a log and show the errors
# on failure. One cargo invocation for both packages, as the release does: features unify only
# within one, so they share one build of the core.
run() {
  if [[ "${VERBOSE:-0}" == 1 ]]; then
    "$@"
  else
    "$@" >>"$BUILD_LOG" 2>&1
  fi
}
fail() {
  if [[ "${VERBOSE:-0}" != 1 ]]; then
    grep -E '^(error|warning: unused)|error\[|error:' "$BUILD_LOG" | sort -u | head -30 >&2 || true
  fi
  echo "✗ Build failed. Full log: $BUILD_LOG" >&2
  exit 1
}
: >"$BUILD_LOG"

echo "▶ Building the app and command line (release, $(uname -m))…"
run cargo build --release -p apparition-installer -p apparition-desktop \
  --features apparition-desktop/custom-protocol || fail

# tauri bundle looks for the binary under tauri.conf.json's mainBinaryName, not the cargo name. On a
# case-insensitive volume (the default) target/release/Apparition IS target/release/apparition, the command
# line: set the command line aside first, and remove the shared name after bundling, so the next cargo build
# links the command line again instead of keeping the app's binary under its name.
CLI_COPY="$ROOT/build/install-desktop-cli"
rm -rf "$CLI_COPY"
mkdir -p "$CLI_COPY"
cp "target/release/$CLI" "$CLI_COPY/$CLI"
cp target/release/apparition-app "target/release/$APP_NAME"
rm -rf target/release/bundle/macos
echo "▶ Bundling $APP_NAME.app…"
(cd desktop && run cargo tauri bundle --bundles app) || fail
rm -f "target/release/$APP_NAME"

BUNDLED="$ROOT/target/release/bundle/macos/$APP_NAME.app"
[[ -d "$BUNDLED" ]] || { echo "✗ Bundled app not found at: $BUNDLED" >&2; exit 1; }

# Stage a copy to put the pieces in and sign, so a failure leaves the installed app alone.
STAGE="$ROOT/build/install-desktop-stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"
ditto "$BUNDLED" "$STAGE/$APP_NAME.app"
MACOS_DIR="$STAGE/$APP_NAME.app/Contents/MacOS"
cp "target/release/$HELPER" "$MACOS_DIR/$HELPER"
cp "$CLI_COPY/$CLI" "$MACOS_DIR/$CLI-cli"
# It must be the command line, not the app: `apparition-cli link` puts it on PATH, and the anisette VM runs it.
if cmp -s "$MACOS_DIR/$CLI-cli" "$MACOS_DIR/$APP_NAME"; then
  echo "✗ $CLI-cli is the app's own binary (a case clash in target/release); run cargo build -p apparition-installer --release and try again." >&2
  exit 1
fi
# The anisette VM (core/src/vm.rs): macOS runs in a VM only on Apple silicon.
VM=0
if [[ "$(uname -m)" == arm64 ]]; then
  echo "▶ Building the anisette VM's runner…"
  run bash selfhost/macos-vm/build.sh || fail
  cp build/macos-vm/vm "$MACOS_DIR/apparition-vm"
  cp build/macos-vm/guest-status "$MACOS_DIR/apparition-vm-guest-status"
  cp build/macos-vm/guest-bridge "$MACOS_DIR/apparition-vm-guest-bridge"
  VM=1
fi

echo "▶ Signing (app and command line: $SIGN_IDENTITY; helper: ad-hoc with its entitlements)…"
codesign -f -s - --entitlements installer/anisette-helper.entitlements "$MACOS_DIR/$HELPER"
codesign -f -s "$SIGN_IDENTITY" "$MACOS_DIR/$CLI-cli"
# The runner needs the virtualization entitlement; the guest's two programs keep build.sh's signatures.
[[ "$VM" == 1 ]] && codesign -f -s "$SIGN_IDENTITY" --entitlements selfhost/macos-vm/runner.entitlements "$MACOS_DIR/apparition-vm"
# Sealed around the helper, not --deep: that would sign the helper over without its entitlements.
codesign -f -s "$SIGN_IDENTITY" --entitlements desktop/Entitlements.plist "$STAGE/$APP_NAME.app"
codesign --verify --strict "$STAGE/$APP_NAME.app" \
  || { echo "✗ The signed app doesn't verify." >&2; exit 1; }

echo "▶ Quitting the running app (if any)…"
# Graceful quit by bundle id, only when it is running, so AppleScript doesn't launch it to
# quit it. Nothing is killed by name: if it ignores the quit, say so.
# Matched by the installed binary's path: a bare name also matches the iOS Apparition app in the Simulator.
running() { pgrep -f "^$DEST/Contents/MacOS/$APP_NAME( |\$)" >/dev/null 2>&1; }
if running; then
  osascript -e "tell application id \"$BUNDLE_ID\" to quit" 2>/dev/null || true
  for _ in $(seq 1 20); do
    running || break
    sleep 0.3
  done
  if running; then
    echo "✗ $APP_NAME is still running after a quit request. Quit it from its menu-bar icon and run this again." >&2
    exit 1
  fi
fi

echo "▶ Installing to ${DEST}…"
# Swapped in, never removed first: `apparition` on PATH is a link into this bundle, and build-and-sign and
# every other agent's deploys run through it. A `rm -rf` then `ditto` leaves the link dangling for the whole
# copy (and for good if the copy fails). Copy beside it, then exchange the two directories in one step.
NEW="$DEST.new"
rm -rf -- "$NEW"
ditto "$STAGE/$APP_NAME.app" "$NEW"
if [[ -e "$DEST" ]]; then
  # renamex_np(RENAME_SWAP) exchanges both at once; where it isn't there, fall back to two renames.
  if /usr/bin/python3 -c 'import ctypes,sys; l=ctypes.CDLL(None, use_errno=True); sys.exit(0 if l.renamex_np(sys.argv[1].encode(), sys.argv[2].encode(), 2)==0 else 1)' "$NEW" "$DEST" 2>/dev/null; then
    rm -rf -- "$NEW"   # now holds the previous app
  else
    mv -- "$DEST" "$DEST.old" && mv -- "$NEW" "$DEST" && rm -rf -- "$DEST.old"
  fi
else
  mv -- "$NEW" "$DEST"
fi

# The command line on PATH is what build-and-sign, agents' `inject` and the Store's builds run, so
# keep it the one just installed rather than whatever older copy the link pointed at.
if [[ "${LINK:-1}" != 0 ]]; then
  echo "▶ Linking the command line onto PATH…"
  # No empty array: bash 3.2 under `set -u` calls it unbound.
  if ! "$DEST/Contents/MacOS/$CLI-cli" link ${LINK_DIR:+--dir "$LINK_DIR"}; then
    echo "⚠ Couldn't link \`$CLI\` onto PATH (above); the app is installed. Run \`$DEST/Contents/MacOS/$CLI-cli link\` once it's sorted." >&2
  fi
fi

if [[ "${LAUNCH:-1}" == 0 ]]; then
  echo "✓ $APP_NAME installed to $DEST_DIR (not launched)."
else
  echo "▶ Launching…"
  open "$DEST"
  echo "✓ $APP_NAME installed to $DEST_DIR and relaunched."
fi
