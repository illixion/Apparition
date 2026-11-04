#!/bin/bash
# setup.sh: turn a macOS virtual machine on your Mac into Apparition's companion with the Mac's
# own anisette that Apple accepts (core/src/anisette/native.rs). See selfhost/macos-vm/README.md for the full
# walkthrough. In short: install macOS in UTM (any version from 26), go through Setup Assistant,
# turn on automatic login, then boot the VM into recoveryOS and, in Utilities > Terminal:
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/illixion/Apparition/main/selfhost/macos-vm/setup.sh)"
#
# A VM's sign-in needs AuthKit's attestation from its host, which only the helper signed with
# AuthKit's private entitlements gets, and macOS runs that helper only with SIP off and
# amfi_get_out_of_my_way=1: this sets both (follow any authentication prompts from
# recoveryOS; macOS 26 requires an owner account too). That attestation is only valid inside a GUI login session, so `apparition
# relay` (the companion) is installed as a LaunchAgent (Aqua): with automatic login on, the VM
# offers its anisette to paired devices and computers on port 49160 (pairing 49161, encrypted
# channel 49162) after a restart with no one at the keyboard. Only for a VM: SIP off is not
# something to do to the Mac you use. (A real Mac on macOS 26 or earlier needs no SIP change;
# run this there from Terminal with sudo and it stays on.)
#
# From the Mac that hosts the VM, there is a shorter way that needs no internet: the Apparition
# app (Companion > Set up a VM) or `apparition vm-setup` on that Mac serves this script and its own
# build, and shows a line to type here, `curl -s <that Mac's address>:4949|bash`. The copy it serves
# has BASE filled in, and installs from there instead of from GitHub.
#
# Uses only what recoveryOS ships (bash 3.2, curl, ditto, csrutil, nvram). Settings:
# APPARITION_ZIP (a release zip URL or local path), APPARITION_PORT, APPARITION_FORCE=1 (skip the
# SIP/NVRAM check in a normal boot, to install the rest).
set -eu

REPO=https://github.com/illixion/Apparition
PORT=${APPARITION_PORT:-49160}
LABEL=com.illixion.apparition.anisette
# Filled in by the host Mac that serves this script (core/src/vm_setup.rs); empty otherwise.
BASE='@@BASE@@'
# 1: the host drives this recoveryOS (no questions; shut down when done so it can go on).
AUTOMATED='@@AUTOMATED@@'
# provision: the Apparition app's anisette VM, before its first boot (core/src/vm.rs); setup: everything else, which
# for the app's VM is also how a newer app puts what only recoveryOS can write into a VM that is set up
# (`apparition vm update` when core's GUEST_VERSION says so; a release's programs alone reach the running VM instead).
STAGE='@@STAGE@@'
# 1: the anisette VM the Apparition app runs. Its anisette agent is already in place (the provision stage), so the
# companion's relay, which a VM used on its own needs, isn't installed.
APPLIANCE='@@APPLIANCE@@'
# 1: Remote Login stays on, for development (`apparition vm setup --public-key`). Nobody else gets it.
KEEP_SSH='@@KEEP_SSH@@'
case "$AUTOMATED" in @@*) AUTOMATED=0 ;; esac
case "$STAGE" in @@*) STAGE=setup ;; esac
case "$APPLIANCE" in @@*) APPLIANCE=0 ;; esac
case "$KEEP_SSH" in @@*) KEEP_SSH=0 ;; esac
case "$BASE" in @@*) BASE="" ;; esac
BOOT_ARG=amfi_get_out_of_my_way=1
DEFER_AMFI=0

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
    # No match is normal while recovery is still mounting the installed volumes.
    return 0
  }
  find_volumes
  if [ -z "$DATA" ]; then
    # Not mounted yet (recoveryOS mounts it once something asks): mount each APFS volume.
    for device in $(diskutil list | awk '/APFS Volume/ {print $NF}'); do
      diskutil mount "$device" > /dev/null 2>&1 || true
    done
    find_volumes
  fi
  [ -n "$DATA" ] || die "found no installed macOS to set up (is FileVault on? unlock it in Disk Utility first)"
fi
major=${version%%.*}
say "Setting up $([ "$RECOVERY" = 1 ] && echo "the macOS $version on $DATA" || echo "this Mac (macOS $version)")"

# ------------------------------------------------------------------ the app's anisette VM: its own programs
bootstrap="/Library/Application Support/ApparitionBootstrap"
plist() { # path label keep-alive program [arguments...]: a root-owned launchd job
  local path=$1 label=$2 keep=$3; shift 3
  {
    printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>' \
      '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
      '<plist version="1.0"><dict>' "<key>Label</key><string>$label</string>" '<key>ProgramArguments</key><array>'
    for argument in "$@"; do printf '<string>%s</string>\n' "$argument"; done
    printf '%s\n' '</array>' '<key>RunAtLoad</key><true/>'
    if [ "$keep" = 1 ]; then printf '%s\n' '<key>KeepAlive</key><true/>'; fi
    case "$path" in
      */LaunchAgents/*)
        # In the Aqua session, the only place the helper's attestation is valid. The companion it runs comes in the
        # next recoveryOS stage; until then launchd retries every 30 seconds.
        printf '%s\n' '<key>LimitLoadToSessionType</key><string>Aqua</string>' '<key>ThrottleInterval</key><integer>30</integer>' \
          "<key>StandardOutPath</key><string>/tmp/$label.log</string>" "<key>StandardErrorPath</key><string>/tmp/$label.log</string>" ;;
      *) printf '%s\n' "<key>StandardErrorPath</key><string>/var/log/$label.log</string>" ;;
    esac
    printf '%s\n' '</dict></plist>'
  } > "$path"
  chown 0:0 "$path"
  chmod 644 "$path"
}
# The programs the host talks to over vsock and their launchd jobs, from the host. The provision stage puts them in,
# and every setup stage after it puts the host's current ones in again: `apparition vm update --recovery` runs one, so
# a VM that is set up takes what only recoveryOS can write from there rather than being made again. A release's programs
# alone reach the running guest over vsock 6972 instead, checked against the release signers compiled into its
# `apparition` (core/src/vm_update.rs). Everything is downloaded before
# anything is replaced, so a visit that stops partway leaves the programs that were there.
appliance_programs() { # name:mode...
  local state="$DATA$bootstrap" stage entry
  stage=$(mktemp -d /tmp/apparition-appliance.XXXXXX)
  for entry in "$@"; do
    curl -fsS -o "$stage/${entry%%:*}" "$BASE/${entry%%:*}" || die "couldn't download ${entry%%:*} from the host"
  done
  mkdir -p "$state" "$DATA/Library/LaunchDaemons" "$DATA/Library/LaunchAgents"
  chown 0:0 "$state"
  chmod 700 "$state"
  for entry in "$@"; do
    ditto "$stage/${entry%%:*}" "$state/${entry%%:*}"
    chown 0:0 "$state/${entry%%:*}"
    chmod "${entry##*:}" "$state/${entry%%:*}"
  done
  # Setup state for the host, over vsock: never credentials or anisette headers (report-status.sh).
  plist "$DATA/Library/LaunchDaemons/com.illixion.apparition.vm-status.plist" com.illixion.apparition.vm-status 1 \
    /bin/bash "$bootstrap/report-status.sh"
  # The host's anisette front door: vsock 6970 to the agent below on the guest's own loopback, and the clock (6971)
  # when the host resumes a suspended VM. Nothing listens on a network.
  plist "$DATA/Library/LaunchDaemons/com.illixion.apparition.vm-bridge.plist" com.illixion.apparition.vm-bridge 1 \
    "$bootstrap/guest-bridge"
  plist "$DATA/Library/LaunchAgents/com.illixion.apparition.vm-anisette.plist" com.illixion.apparition.vm-anisette 1 \
    /usr/local/apparition/apparition anisette serve --port 6970
}

# ------------------------------------------------------------------ first boot (the app's anisette VM)
# Run once, in recoveryOS, right after the install and before macOS has ever booted: what makes the first boot create
# the account (with a secure token, which csrutil needs later), log it in automatically and skip Setup Assistant, plus
# the daemons the host talks to over vsock. Recovery is the only place root can write the guest's disk without asking
# the person at the Mac for an administrator password. The password comes from the host, under the session's token.
if [ "$STAGE" = provision ]; then
  [ "$RECOVERY" = 1 ] && [ -n "$BASE" ] || die "this stage runs in recoveryOS, served by the Mac that runs the VM"
  say "Writing the first-boot setup"
  stage=$(mktemp -d /tmp/apparition-provision.XXXXXX)
  programs="password:600 first-boot.sh:700 report-status.sh:700 guest-status:755 guest-bridge:755"
  if [ "$KEEP_SSH" = 1 ]; then programs="$programs authorized_keys:600"; fi
  # shellcheck disable=SC2086 # one word per file
  appliance_programs $programs
  # The trim only writes launchd's override files, so it has to come before the account's first login.
  for name in appliance-trim.sh appliance-disabled-user.txt appliance-disabled-system.txt; do
    curl -fsS -o "$stage/$name" "$BASE/$name" || die "couldn't download $name from the host"
  done
  APPARITION_KEEP_SSH=$([ "$KEEP_SSH" = 1 ] && echo 1) bash "$stage/appliance-trim.sh" "$DATA"
  # Set before loginwindow starts, so Setup Assistant never runs.
  touch "$DATA/private/var/db/.AppleSetupDone"
  chown 0:0 "$DATA/private/var/db/.AppleSetupDone"
  chmod 644 "$DATA/private/var/db/.AppleSetupDone"
  # Makes the account on the first boot, then shuts the VM down (first-boot.sh).
  plist "$DATA/Library/LaunchDaemons/com.illixion.apparition.bootstrap.plist" com.illixion.apparition.bootstrap 0 \
    /bin/bash "$bootstrap/first-boot.sh"
  curl -fsS "$BASE/complete" > /dev/null || die "couldn't confirm the first-boot setup to the host"
  say "First boot prepared. Shutting down so the host can boot it."
  /sbin/shutdown -h now
  exit 0
fi

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
    say "Turning SIP off (follow any recoveryOS authentication prompts)"
    csrutil disable < /dev/tty || die "csrutil disable failed; see the recoveryOS error above."
  fi
  case "$(nvram boot-args 2>/dev/null)" in
    *"$BOOT_ARG"*) echo "AMFI is already off." ;;
    *) say "Turning AMFI off ($BOOT_ARG)"
       args=$(nvram boot-args 2>/dev/null | sed 's/^boot-args[[:space:]]*//')
       if ! nvram boot-args="${args:+$args }$BOOT_ARG"; then
         [ "$VM" = 1 ] || die "nvram failed"
         DEFER_AMFI=1
         echo "Recovery refused the boot argument. A root first-boot job will try it in macOS and restart once."
       fi ;;
  esac
elif [ "${APPARITION_FORCE:-}" != 1 ]; then
  csrutil status | grep -q 'disabled' || die "SIP is on. Restart into recoveryOS and run this there."
  case "$(nvram boot-args 2>/dev/null)" in
    *"$BOOT_ARG"*) ;;
    *) die "$BOOT_ARG isn't set. Restart into recoveryOS and run this there." ;;
  esac
fi

# ------------------------------------------------------------------ Apparition
if [ "$APPLIANCE" = 1 ] && [ -n "$BASE" ]; then
  say "Installing the programs the host talks to"
  appliance_programs report-status.sh:700 guest-status:755 guest-bridge:755
fi
say "Installing apparition and its anisette helper"
dest="$DATA/usr/local/apparition"
mkdir -p "$dest"
work=$(mktemp -d /tmp/apparition.XXXXXX)
if [ -n "$BASE" ]; then
  # The host Mac's own build, one file each, both downloaded before either is replaced. The helper is kept as
  # sent: its signature carries the entitlements, so never re-sign it.
  for name in apparition apparition-anisette-helper; do
    curl -fL --progress-bar -o "$work/$name" "$BASE/$name" || die "couldn't download $BASE/$name"
  done
  for name in apparition apparition-anisette-helper; do
    ditto "$work/$name" "$dest/$name"
    chown 0:0 "$dest/$name"
    chmod 755 "$dest/$name"
  done
else
  if [ -z "${APPARITION_ZIP:-}" ]; then
    # The latest release: GitHub redirects /releases/latest to .../tag/vX.Y.Z.
    tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$REPO/releases/latest" | sed 's|.*/||')
    [ -n "$tag" ] || die "couldn't find the latest Apparition release"
    ZIP=$REPO/releases/download/$tag/Apparition-Installer-${tag#v}-macos-universal.zip
  else
    ZIP=$APPARITION_ZIP
  fi
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
fi
# Leave the temporary directory for recovery diagnostics.

# A VM that only exists to run the companion doesn't need its desktop services, and a trimmed macOS 26 runs in
# 1.5 GiB without swapping (stock needs 2 GiB and swaps), which matters on an 8 GB Mac. APPARITION_TRIM=0 skips it.
if [ "$VM" = 1 ] && [ "${APPARITION_TRIM:-1}" != 0 ]; then
  say "Trimming the VM down to what the companion needs (APPARITION_TRIM=0 skips this)"
  mkdir -p "$work/trim"
  trim_ok=1
  for name in appliance-trim.sh appliance-disabled-user.txt appliance-disabled-system.txt; do
    if [ -n "$BASE" ]; then url="$BASE/$name"; else url="https://raw.githubusercontent.com/illixion/Apparition/main/selfhost/macos-vm/$name"; fi
    curl -fsSL -o "$work/trim/$name" "$url" || { trim_ok=0; break; }
  done
  # Only the automated development flow keeps SSH; nobody else should get Remote Login switched on.
  if [ "$trim_ok" = 1 ] && APPARITION_KEEP_SSH=$([ "$KEEP_SSH" = 1 ] && echo 1) bash "$work/trim/appliance-trim.sh" "$DATA"; then :
  else echo "The trim didn't apply; the VM works without it, just with more memory use."; fi
fi

# Some recoveryOS builds refuse boot-args writes even after SIP was disabled.
# Try from the installed macOS as root, then reboot before the helper is usable.
# This daemon only configures the boot argument; anisette stays in the Aqua agent.
if [ "$VM" = 1 ]; then
  {
    printf '%s\n' '#!/bin/bash' 'set -eu'
    if [ "$KEEP_SSH" = 1 ]; then
      # Development only: the automated flow keeps Remote Login on for diagnostics. Nobody else gets it.
      printf '%s\n' 'launchctl print system/com.openssh.sshd > /dev/null 2>&1 || launchctl load -w /System/Library/LaunchDaemons/ssh.plist'
    fi
    cat <<'AMFI'
csrutil status | grep -q disabled || exit 1
args=$(nvram boot-args 2>/dev/null | sed 's/^boot-args[[:space:]]*//')
case "$args" in *amfi_get_out_of_my_way=1*) exit 0 ;; esac
nvram boot-args="${args:+$args }amfi_get_out_of_my_way=1"
/sbin/shutdown -r now
AMFI
  } > "$dest/enable-amfi.sh"
  chown 0:0 "$dest/enable-amfi.sh"
  chmod 700 "$dest/enable-amfi.sh"
  mkdir -p "$DATA/Library/LaunchDaemons"
  daemon="$DATA/Library/LaunchDaemons/com.illixion.apparition.enable-amfi.plist"
  cat > "$daemon" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.illixion.apparition.enable-amfi</string>
<key>ProgramArguments</key><array><string>/bin/bash</string><string>/usr/local/apparition/enable-amfi.sh</string></array>
<key>RunAtLoad</key><true/>
<key>StandardOutPath</key><string>/var/log/apparition-enable-amfi.log</string>
<key>StandardErrorPath</key><string>/var/log/apparition-enable-amfi.log</string>
</dict></plist>
PLIST
  chown 0:0 "$daemon"
  chmod 644 "$daemon"
fi

# A LaunchAgent, not a daemon: LimitLoadToSessionType Aqua runs it in the GUI login session, the
# only place the helper's attestation is valid (a root LaunchDaemon gets -3000076 from Apple). In
# /Library/LaunchAgents it loads for whichever account logs in, which automatic login makes the VM.
# The app's anisette VM has its own agent (the provision stage) and no relay: devices reach it through the host.
mkdir -p "$DATA/Library/LaunchAgents"
agent="$DATA/Library/LaunchAgents/$LABEL.plist"
if [ "$APPLIANCE" = 1 ]; then
  rm -f "$agent"
else
cat > "$agent" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>$LABEL</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/local/apparition/apparition</string>
		<string>relay</string>
		<string>--port</string>
		<string>$PORT</string>
		<string>--no-prompt</string>
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
fi

# A download is not completion: notify the host only after the files and agent exist.
if [ -n "$BASE" ]; then
  curl -fsS "$BASE/complete" > /dev/null || die "couldn't confirm setup to the host"
fi

if [ "$RECOVERY" = 1 ]; then
  if [ "$AUTOMATED" = 1 ]; then
    say "Installed. Shutting down so the host can resume with networking disconnected."
    /sbin/shutdown -h now
    exit 0
  fi
  say "Done. With automatic login on, the VM runs Apparition's companion on port $PORT after it restarts."
  echo "Turn automatic login on in System Settings > Users & Groups if you haven't, then restart."
  echo "Then pair your devices and computers with it once (selfhost/macos-vm/README.md, step 6)."
  [ "$(ask "Restart now? [Y/n] ")" = n ] || reboot
else
  owner=$(stat -f%u /dev/console)
  launchctl bootstrap "gui/$owner" "$agent" 2>/dev/null || launchctl kickstart -k "gui/$owner/$LABEL" 2>/dev/null
  say "Done. The companion is on port $PORT; log: /tmp/apparition-anisette.log"
  echo "Pair your devices and computers with it once: see selfhost/macos-vm/README.md, step 6."
fi
