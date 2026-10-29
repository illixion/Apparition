#!/bin/bash
# setup.sh: turn a macOS virtual machine into Apparition's anisette server, the Mac's own
# anisette that Apple accepts (core/src/anisette/native.rs). Set the VM up as usual (UTM, any
# macOS from 26, through Setup Assistant), then restart it into recoveryOS (UTM: Start > Boot
# into recovery, or `utmctl start --recovery <vm>`) and, in Utilities > Terminal:
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/illixion/Apparition/main/selfhost/macos-vm/setup.sh)"
#
# A VM's sign-in needs AuthKit's attestation from its host, which only the helper signed with
# AuthKit's private entitlements gets, and macOS runs it only with SIP off and
# amfi_get_out_of_my_way=1: this sets both (turning SIP off takes the password of the user Setup
# Assistant made). That attestation is only valid inside a GUI login session, so `apparition
# anisette serve` is installed as a LaunchAgent (Aqua) and the Setup-Assistant user is set to log
# in automatically, so the VM serves anisette on port 6969 after a restart with no one at the
# keyboard. Only for a VM: SIP off is not something to do to the Mac you use. (A real Mac on
# macOS 26 or earlier needs no SIP change; run this there from Terminal with sudo.)
#
# Uses only what recoveryOS ships (bash 3.2, curl, ditto, csrutil, nvram, defaults). Settings:
# APPARITION_ZIP (a release zip URL or local path), APPARITION_PORT, APPARITION_USER (the account
# to log in automatically; otherwise the only one, or it asks), APPARITION_PASSWORD (answers the
# password prompt unattended), APPARITION_FORCE=1 (skip the SIP/NVRAM check in a normal boot).
set -u

VERSION=0.4.1
ZIP=${APPARITION_ZIP:-https://github.com/illixion/Apparition/releases/download/v$VERSION/Apparition-Installer-$VERSION-macos-universal.zip}
PORT=${APPARITION_PORT:-6969}
LABEL=com.illixion.apparition.anisette
BOOT_ARG=amfi_get_out_of_my_way=1

say() { printf '\n==> %s\n' "$*"; }
die() { printf '\nerror: %s\n' "$*" >&2; exit 1; }
# Prompts read the terminal, so `curl ... | bash` works as well as `bash -c "$(curl ...)"`.
ask() { local reply; printf '%s' "$1" > /dev/tty; IFS= read -r reply < /dev/tty; printf '%s' "$reply"; }
ask_secret() {
  [ -n "${APPARITION_PASSWORD:-}" ] && { printf '%s' "$APPARITION_PASSWORD"; return; }
  local reply; printf '%s' "$1" > /dev/tty
  stty -echo < /dev/tty; IFS= read -r reply < /dev/tty; stty echo < /dev/tty; printf '\n' > /dev/tty
  printf '%s' "$reply"
}

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
  find_data() {
    for volume in /Volumes/*; do
      [ -d "$volume/private/var/db/dslocal/nodes/Default" ] && { DATA=$volume; return; }
    done
  }
  find_data
  if [ -z "$DATA" ]; then
    # Not mounted yet (recoveryOS mounts it once something asks): mount each Data volume.
    for device in $(diskutil list | awk '/APFS Volume .*Data/ {print $NF}'); do
      diskutil mount "$device" > /dev/null 2>&1
    done
    find_data
  fi
  [ -n "$DATA" ] || die "found no macOS Data volume to set up (is FileVault on? unlock it in Disk Utility first)"
  for volume in /Volumes/*; do
    plist="$volume/System/Library/CoreServices/SystemVersion.plist"
    [ -f "$plist" ] && { version=$(grep -A1 '<key>ProductVersion</key>' "$plist" | sed -n 's/.*<string>\(.*\)<\/string>.*/\1/p'); break; }
  done
fi

# The directory: offline from its files in recoveryOS, through opendirectoryd otherwise.
if [ "$RECOVERY" = 1 ]; then
  ds() { dscl -f "$DATA/private/var/db/dslocal/nodes/Default" localonly "$@"; }
  DS=/Local/Default
else
  ds() { dscl . "$@"; }
  DS=""
fi
major=${version%%.*}
say "Setting up $([ "$RECOVERY" = 1 ] && echo "the macOS $version on $DATA" || echo "this Mac (macOS $version)")"

# ------------------------------------------------------------------ SIP and AMFI
if [ "$VM" = 0 ] && [ -n "$major" ] && [ "$major" -le 26 ]; then
  # A real Mac up to macOS 26 gives its anisette to any program (AOSKit).
  echo "macOS $version gives its anisette to any program: SIP and AMFI stay on."
elif [ "$RECOVERY" = 1 ]; then
  if csrutil status | grep -q 'disabled'; then
    echo "SIP is already off."
  else
    if [ "$VM" = 0 ] && [ "$(ask "This doesn't look like a virtual machine. Turn SIP off anyway? [y/N] ")" != y ]; then
      exit 1
    fi
    say "Turning SIP off (it asks for the password of the user Setup Assistant made)"
    csrutil disable < /dev/tty || die "csrutil disable failed. Did Setup Assistant finish? It makes the user this needs."
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

# ------------------------------------------------------------------ the user and automatic login
# The helper's attestation (x-apple-baa-*) is only valid inside a GUI (Aqua) login session: a
# LaunchDaemon running as root gets -3000076 from Apple, a LaunchAgent in a logged-in session gets
# through. So the anisette must run as a login item, and the VM must log a user in on its own.
users=$(ds -list $DS/Users UniqueID 2>/dev/null | awk '$2 >= 501 && $2 < 1000 {print $1}')
if [ -n "${APPARITION_USER:-}" ]; then
  USER_NAME=$APPARITION_USER
elif [ -z "$users" ]; then
  die "no user account yet. Finish Setup Assistant (create an account; skip Apple Account and the rest), then run this."
elif [ $(printf '%s\n' $users | wc -l) = 1 ]; then
  USER_NAME=$(printf '%s' $users)
else
  echo "Users: $users"
  USER_NAME=$(ask "Which one should log in automatically and run the anisette server? ")
fi
printf '%s\n' $users | grep -qx "$USER_NAME" || die "no user $USER_NAME on this macOS"
owner=$(ds -read "$DS/Users/$USER_NAME" UniqueID | awk '{print $2}')

say "Logging $USER_NAME in automatically (its password, for the login window)"
PASSWORD=$(ask_secret "$USER_NAME's password: ")
# loginwindow's autoLoginUser plus the password, obscured as /etc/kcpassword (XOR with
# loginwindow's fixed key, padded to a multiple of 12 bytes).
mkdir -p "$DATA/Library/Preferences"
loginwindow="$DATA/Library/Preferences/com.apple.loginwindow"
defaults write "$loginwindow" autoLoginUser -string "$USER_NAME" || die "couldn't set automatic login"
plutil -convert xml1 "$loginwindow.plist" 2>/dev/null
key="125 137 82 35 210 188 221 234 163 185 31"
set -- $key
length=${#PASSWORD}
padded=$(( (length / 12 + 1) * 12 ))
kcpassword="$DATA/private/etc/kcpassword"
: > "$kcpassword"
i=0
while [ $i -lt $padded ]; do
  if [ $i -lt "$length" ]; then byte=$(printf '%d' "'${PASSWORD:$i:1}"); else byte=0; fi
  [ "$byte" -lt 128 ] || die "the password must be plain ASCII for automatic login"
  k=$(( i % 11 + 1 )); eval "kb=\${$k}"
  printf "\\$(printf '%03o' $(( byte ^ kb )))" >> "$kcpassword"
  i=$((i + 1))
done
chmod 600 "$kcpassword"; chown 0:0 "$kcpassword"
PASSWORD=""

# ------------------------------------------------------------------ Apparition
say "Installing apparition and its anisette helper"
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
# only place the helper's attestation is valid.
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
  say "Done. When the VM restarts it logs $USER_NAME in and serves anisette on port $PORT."
  echo "Point Apparition at http://<the VM's address>:$PORT (in the VM: ipconfig getifaddr en0)."
  [ "$(ask "Restart now? [Y/n] ")" = n ] || reboot
else
  launchctl bootstrap "gui/$owner" "$agent" 2>/dev/null || launchctl kickstart -k "gui/$owner/$LABEL" 2>/dev/null
  say "Done. Anisette is on port $PORT; log: /tmp/apparition-anisette.log"
fi
