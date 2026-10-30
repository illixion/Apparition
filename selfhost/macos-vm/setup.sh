#!/bin/bash
# setup.sh: turn a macOS virtual machine into Apparition's anisette server, the Mac's own
# anisette that Apple accepts (core/src/anisette/native.rs). See docs/ANISETTE.md for the full
# walkthrough. In short: install macOS in UTM (any version from 26), go through Setup Assistant,
# turn on automatic login, then boot the VM into recoveryOS and, in Utilities > Terminal:
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/illixion/Apparition/main/selfhost/macos-vm/setup.sh)"
#
# A VM's sign-in needs AuthKit's attestation from its host, which only the helper signed with
# AuthKit's private entitlements gets, and macOS runs that helper only with SIP off and
# amfi_get_out_of_my_way=1: this sets both (turning SIP off asks for the password of the account
# Setup Assistant made). That attestation is only valid inside a GUI login session, so `apparition
# anisette serve` is installed as a LaunchAgent (Aqua): with automatic login on, the VM serves
# anisette on port 6969 after a restart with no one at the keyboard. Only for a VM: SIP off is not
# something to do to the Mac you use. (A real Mac on macOS 26 or earlier needs no SIP change;
# run this there from Terminal with sudo and it stays on.)
#
# Uses only what recoveryOS ships (bash 3.2, curl, ditto, csrutil, nvram). Settings:
# APPARITION_ZIP (a release zip URL or local path), APPARITION_PORT, APPARITION_FORCE=1 (skip the
# SIP/NVRAM check in a normal boot, to install the rest).
set -u

REPO=https://github.com/illixion/Apparition
PORT=${APPARITION_PORT:-6969}
LABEL=com.illixion.apparition.anisette
BOOT_ARG=amfi_get_out_of_my_way=1

say() { printf '\n==> %s\n' "$*"; }
die() { printf '\nerror: %s\n' "$*" >&2; exit 1; }
# Prompts read the terminal, so `curl ... | bash` works as well as `bash -c "$(curl ...)"`.
ask() { local reply; printf '%s' "$1" > /dev/tty; IFS= read -r reply < /dev/tty; printf '%s' "$reply"; }

[ "$(id -u)" = 0 ] || die "run it as root (recoveryOS's Terminal is; in macOS, use sudo)"
[ "$(uname -m)" = arm64 ] || die "this is for a macOS VM on Apple silicon"
case "$(sysctl -n hw.model 2>/dev/null)" in
  VirtualMac*) VM=1 ;;
  *) VM=0 ;;
esac

# recoveryOS has no Finder; there the installed system is a mounted volume, otherwise it's /.
if [ -e /System/Library/CoreServices/Finder.app ]; then
  RECOVERY=0 DATA="" version=$(sw_vers -productVersion)
else
  RECOVERY=1 DATA="" version=""
  # /usr/local and /Library are on the read-write Data volume (it holds dslocal); the version
  # comes from a read-only System volume's SystemVersion.plist. Find each.
  find_volumes() {
    for volume in /Volumes/*; do
      [ -z "$DATA" ] && [ -d "$volume/private/var/db/dslocal/nodes/Default" ] && DATA=$volume
      plist="$volume/System/Library/CoreServices/SystemVersion.plist"
      [ -z "$version" ] && [ -f "$plist" ] && version=$(grep -A1 '<key>ProductVersion</key>' "$plist" | sed -n 's/.*<string>\(.*\)<\/string>.*/\1/p')
    done
  }
  find_volumes
  if [ -z "$DATA" ]; then
    # Not mounted yet (recoveryOS mounts it once something asks): mount each APFS volume.
    for device in $(diskutil list | awk '/APFS Volume/ {print $NF}'); do
      diskutil mount "$device" > /dev/null 2>&1
    done
    find_volumes
  fi
  [ -n "$DATA" ] || die "found no installed macOS to set up (is FileVault on? unlock it in Disk Utility first)"
fi
major=${version%%.*}
say "Setting up $([ "$RECOVERY" = 1 ] && echo "the macOS $version on $DATA" || echo "this Mac (macOS $version)")"

# ------------------------------------------------------------------ SIP and AMFI
if [ "$VM" = 0 ] && [ -n "$major" ] && [ "$major" -le 26 ]; then
  # A real Mac up to macOS 26 gives its anisette to any program (AOSKit), no SIP change needed.
  echo "macOS $version gives its anisette to any program: SIP and AMFI stay on."
elif [ "$RECOVERY" = 1 ]; then
  if csrutil status | grep -q 'disabled'; then
    echo "SIP is already off."
  else
    if [ "$VM" = 0 ] && [ "$(ask "This doesn't look like a virtual machine. Turn SIP off anyway? [y/N] ")" != y ]; then
      exit 1
    fi
    say "Turning SIP off (it asks for the password of the account Setup Assistant made)"
    csrutil disable < /dev/tty || die "csrutil disable failed. Did Setup Assistant finish? It makes the account this asks for."
  fi
  case "$(nvram boot-args 2>/dev/null)" in
    *"$BOOT_ARG"*) echo "AMFI is already off." ;;
    *) say "Turning AMFI off ($BOOT_ARG)"
       args=$(nvram boot-args 2>/dev/null | sed 's/^boot-args[[:space:]]*//')
       nvram boot-args="${args:+$args }$BOOT_ARG" || die "nvram failed" ;;
  esac
elif [ "${APPARITION_FORCE:-}" != 1 ]; then
  csrutil status | grep -q 'disabled' || die "SIP is on. Restart into recoveryOS and run this there."
  case "$(nvram boot-args 2>/dev/null)" in
    *"$BOOT_ARG"*) ;;
    *) die "$BOOT_ARG isn't set. Restart into recoveryOS and run this there." ;;
  esac
fi

# ------------------------------------------------------------------ Apparition
say "Installing apparition and its anisette helper"
if [ -z "${APPARITION_ZIP:-}" ]; then
  # The latest release: GitHub redirects /releases/latest to .../tag/vX.Y.Z.
  tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$REPO/releases/latest" | sed 's|.*/||')
  [ -n "$tag" ] || die "couldn't find the latest Apparition release"
  ZIP=$REPO/releases/download/$tag/Apparition-Installer-${tag#v}-macos-universal.zip
else
  ZIP=$APPARITION_ZIP
fi
dest="$DATA/usr/local/apparition"
mkdir -p "$dest"
work=$(mktemp -d /tmp/apparition.XXXXXX)
case "$ZIP" in
  http*) curl -fL --progress-bar -o "$work/apparition.zip" "$ZIP" || die "couldn't download $ZIP" ;;
  *) cp "$ZIP" "$work/apparition.zip" || die "no file $ZIP" ;;
esac
ditto -x -k "$work/apparition.zip" "$work/x" || die "couldn't unpack $ZIP"
for name in apparition apparition-anisette-helper; do
  file=$(find "$work/x" -maxdepth 2 -type f -name "$name" | head -1)
  [ -n "$file" ] || die "$name isn't in $ZIP"
  # ditto keeps the helper's signature, entitlements included; never re-sign it.
  ditto "$file" "$dest/$name"
  chown 0:0 "$dest/$name"
  chmod 755 "$dest/$name"
done
rm -rf "$work"

# A LaunchAgent, not a daemon: LimitLoadToSessionType Aqua runs it in the GUI login session, the
# only place the helper's attestation is valid (a root LaunchDaemon gets -3000076 from Apple). In
# /Library/LaunchAgents it loads for whichever account logs in, which automatic login makes the VM.
mkdir -p "$DATA/Library/LaunchAgents"
agent="$DATA/Library/LaunchAgents/$LABEL.plist"
cat > "$agent" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>$LABEL</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/local/apparition/apparition</string>
		<string>anisette</string>
		<string>serve</string>
		<string>--port</string>
		<string>$PORT</string>
	</array>
	<key>LimitLoadToSessionType</key><string>Aqua</string>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key><true/>
	<key>ThrottleInterval</key><integer>30</integer>
	<key>StandardOutPath</key><string>/tmp/apparition-anisette.log</string>
	<key>StandardErrorPath</key><string>/tmp/apparition-anisette.log</string>
</dict>
</plist>
EOF
chown 0:0 "$agent"
chmod 644 "$agent"

if [ "$RECOVERY" = 1 ]; then
  say "Done. With automatic login on, the VM serves anisette on port $PORT after it restarts."
  echo "Turn automatic login on in System Settings > Users & Groups if you haven't, then restart."
  echo "Point Apparition at http://<the VM's address>:$PORT (in the VM: ipconfig getifaddr en0)."
  [ "$(ask "Restart now? [Y/n] ")" = n ] || reboot
else
  owner=$(stat -f%u /dev/console)
  launchctl bootstrap "gui/$owner" "$agent" 2>/dev/null || launchctl kickstart -k "gui/$owner/$LABEL" 2>/dev/null
  say "Done. Anisette is on port $PORT; log: /tmp/apparition-anisette.log"
fi
