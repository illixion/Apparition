#!/bin/bash
# Development-only first-boot injection. Run on the host with sudo, VM stopped.
set -eu
[ "$(id -u)" = 0 ] || { echo 'Run with sudo; guest files must be root-owned.' >&2; exit 1; }
[ "$#" = 2 ] || { echo 'usage: sudo bash provision.sh <vm-bundle> <authorized-public-key>' >&2; exit 1; }
BUNDLE=$(cd "$1" && pwd)
SOURCE=$(cd "$(dirname "$0")" && pwd)
[ -f "$BUNDLE/disk.img" ] && [ -f "$2" ] || exit 1
if /usr/sbin/lsof -t "$BUNDLE/disk.img" >/dev/null 2>&1; then
  echo 'Disk is open; shut down the VM and detach it before provisioning.' >&2
  exit 1
fi
# A suspended VM's saved memory only matches the disk as it is now; writing to the disk would make resuming it corrupt it.
if [ -e "$BUNDLE/suspended.vzvmsave" ]; then
  echo 'The VM is suspended; resume it and shut it down first (vm.py shutdown does both).' >&2
  exit 1
fi
# Hold the same advisory lock as the native runner for the complete attachment.
if [ "${APPARITION_PROVISION_LOCKED:-}" != 1 ]; then
  exec /usr/bin/python3 - "$SOURCE/provision.sh" "$BUNDLE" "$2" <<'PYLOCK'
import fcntl,os,subprocess,sys
with open(os.path.join(sys.argv[2],'.lock'),'a') as lock:
    try: fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
    except BlockingIOError: sys.exit('Bundle is already in use')
    env=dict(os.environ,APPARITION_PROVISION_LOCKED='1')
    sys.exit(subprocess.call(['/bin/bash',*sys.argv[1:]],env=env))
PYLOCK
fi
umask 077
STAGE="$BUNDLE/provisioning"
mkdir -p "$STAGE/mounts"
if [ ! -f "$BUNDLE/credentials.txt" ]; then
  PASSWORD=$(/usr/bin/openssl rand -hex 12)
  printf 'VM account: apparition\nPassword: %s\n' "$PASSWORD" > "$BUNDLE/credentials.txt"
  unset PASSWORD
fi
# Existing credentials are reused; never silently rotate an existing VM password.
sed -n 's/^Password: //p' "$BUNDLE/credentials.txt" > "$STAGE/password"
[ -s "$STAGE/password" ] || { echo 'No password in credentials.txt' >&2; exit 1; }
chmod 600 "$BUNDLE/credentials.txt" "$STAGE/password"
chown "$(stat -f%u "$BUNDLE"):$(stat -f%g "$BUNDLE")" "$BUNDLE/credentials.txt"
hdiutil attach -owners on -nobrowse -mountroot "$STAGE/mounts" -plist "$BUNDLE/disk.img" > "$STAGE/attached.plist"
DEVICE=$(/usr/bin/python3 - "$STAGE/attached.plist" <<'PY'
import plistlib,sys
print(plistlib.load(open(sys.argv[1],'rb'))['system-entities'][0]['dev-entry'])
PY
)
# Detach, then remove the staged copy of the password and its folder (exact files, nothing recursive).
cleanup() {
  hdiutil detach "$DEVICE" >/dev/null
  rm -f "$STAGE/password" "$STAGE/attached.plist"
  rmdir "$STAGE/mounts" "$STAGE" 2>/dev/null || true
}
trap cleanup EXIT
DATA=$(/usr/bin/python3 - "$STAGE/attached.plist" <<'PY'
import pathlib,plistlib,sys
for e in plistlib.load(open(sys.argv[1],'rb'))['system-entities']:
    m=e.get('mount-point')
    if m and pathlib.Path(m,'private/var/db/dslocal/nodes/Default').is_dir():
        print(m);break
else: sys.exit('No installed Data volume found')
PY
)
DEST="$DATA/Library/Application Support/ApparitionBootstrap"
install -d -o root -g wheel -m 700 "$DEST"
install -o root -g wheel -m 600 "$STAGE/password" "$DEST/password"
install -o root -g wheel -m 600 "$2" "$DEST/authorized_keys"
install -o root -g wheel -m 700 "$SOURCE/first-boot.sh" "$DEST/first-boot.sh"
install -o root -g wheel -m 700 "$SOURCE/report-status.sh" "$DEST/report-status.sh"
install -o root -g wheel -m 755 "$SOURCE/../../build/macos-vm/guest-status" "$DEST/guest-status"
install -o root -g wheel -m 755 "$SOURCE/../../build/macos-vm/guest-bridge" "$DEST/guest-bridge"
# Trim the desktop services the appliance doesn't need (launchd override files, so it must happen before the
# account's first login). SSH stays enabled for this development flow.
APPARITION_KEEP_SSH=1 /bin/bash "$SOURCE/appliance-trim.sh" "$DATA"
# Set the marker before loginwindow starts, not concurrently with Setup Assistant.
touch "$DATA/private/var/db/.AppleSetupDone"
chown 0:0 "$DATA/private/var/db/.AppleSetupDone"
chmod 644 "$DATA/private/var/db/.AppleSetupDone"
mkdir -p "$DATA/Library/LaunchDaemons"
AGENT="$DATA/Library/LaunchDaemons/com.illixion.apparition.bootstrap.plist"
cat > "$AGENT" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.illixion.apparition.bootstrap</string>
<key>ProgramArguments</key><array><string>/bin/bash</string><string>/Library/Application Support/ApparitionBootstrap/first-boot.sh</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>
PLIST
chown 0:0 "$AGENT"
chmod 644 "$AGENT"
REPORTER="$DATA/Library/LaunchDaemons/com.illixion.apparition.vm-status.plist"
cat > "$REPORTER" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.illixion.apparition.vm-status</string>
<key>ProgramArguments</key><array><string>/bin/bash</string><string>/Library/Application Support/ApparitionBootstrap/report-status.sh</string></array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>StandardErrorPath</key><string>/var/log/apparition-vm-status.log</string>
</dict></plist>
PLIST
chown 0:0 "$REPORTER"
chmod 644 "$REPORTER"
# The runner's anisette front door (`vm serve`): guest-bridge carries vsock 6970 to the anisette agent on the guest's own
# loopback, and sets the clock when the host resumes a suspended VM (vsock 6971). Nothing listens on a network.
BRIDGE="$DATA/Library/LaunchDaemons/com.illixion.apparition.vm-bridge.plist"
cat > "$BRIDGE" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.illixion.apparition.vm-bridge</string>
<key>ProgramArguments</key><array><string>/Library/Application Support/ApparitionBootstrap/guest-bridge</string></array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>StandardErrorPath</key><string>/var/log/apparition-vm-bridge.log</string>
</dict></plist>
PLIST
# In the Aqua session, the only place the helper's attestation is valid. The companion itself is installed later, from
# recoveryOS (setup.sh); until then launchd retries this every 30 seconds.
ANISETTE="$DATA/Library/LaunchAgents/com.illixion.apparition.vm-anisette.plist"
mkdir -p "$DATA/Library/LaunchAgents"
cat > "$ANISETTE" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.illixion.apparition.vm-anisette</string>
<key>ProgramArguments</key><array><string>/usr/local/apparition/apparition</string><string>anisette</string><string>serve</string><string>--port</string><string>6970</string></array>
<key>LimitLoadToSessionType</key><string>Aqua</string>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ThrottleInterval</key><integer>30</integer>
<key>StandardOutPath</key><string>/tmp/apparition-vm-anisette.log</string>
<key>StandardErrorPath</key><string>/tmp/apparition-vm-anisette.log</string>
</dict></plist>
PLIST
chown 0:0 "$BRIDGE" "$ANISETTE"
chmod 644 "$BRIDGE" "$ANISETTE"
printf 'First boot prepared. Account: apparition\nLogin details: %s/credentials.txt\n' "$BUNDLE"
