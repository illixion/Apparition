#!/bin/bash
# appliance-trim.sh [DATA]: make a macOS VM a headless anisette appliance by disabling launchd services it does
# not need. DATA is the root of the guest's Data volume as seen from where this runs: empty (the default) inside
# the running guest, or a mount point such as /Volumes/Macintosh HD - Data from recoveryOS or the host.
#
# It only writes launchd's override files, which launchd reads when a domain starts, so run it BEFORE the
# account's first login. Booting services out of a live session instead leaves loginwindow in a state that
# logs the session out (measured). Finder, Dock and SystemUIServer are kept on purpose: loginwindow waits for them.
#
# Needs bash 3.2 and nothing else, so recoveryOS can run it. appliance-disabled-*.txt sit beside this file
# (the host serves them next to the script); labels are matched exactly, so a service Apple adds later keeps running.
set -eu
[ "$(id -u)" = 0 ] || [ "${APPARITION_TRIM_TEST:-}" = 1 ] || { echo "run as root: launchd ignores override files owned by anyone else" >&2; exit 1; }
DATA=${1:-}
HERE=$(cd "$(dirname "$0")" && pwd)
LAUNCHD="$DATA/private/var/db/com.apple.xpc.launchd"
[ -z "$DATA" ] && LAUNCHD=/var/db/com.apple.xpc.launchd
USER_UID=${APPARITION_UID:-501}

# The labels this script disabled last time are remembered in STATE, so a label taken off a list later is put back
# to Apple's default on VMs that already have it (a list gets corrected: a disabled bluetoothd made coreaudiod leak).
STATE="$DATA/usr/local/apparition"
[ -z "$DATA" ] && STATE=/usr/local/apparition

plain_labels() { # list-file: the labels, without comments and blank lines
  while IFS= read -r label; do
    case "$label" in ''|'#'*) continue ;; esac
    case "$label" in *[!A-Za-z0-9._-]*) echo "skipping odd label: $label" >&2; continue ;; esac
    printf '%s\n' "$label"
  done < "$1"
}

# Merges into an existing override file (a person may have turned something on or off themselves);
# PlistBuddy ships with macOS, recoveryOS included.
write_overrides() { # list-file target keep-enabled-labels...
  local list=$1 target=$2; shift 2
  local commands remembered; commands=$(mktemp "${TMPDIR:-/tmp}/overrides.XXXXXX")
  remembered="$STATE/trim-applied-$(basename "$list")"
  {
    if [ -f "$remembered" ]; then
      while IFS= read -r label; do
        plain_labels "$list" | grep -qxF -- "$label" || printf 'Delete :%s\n' "$label"
      done < "$remembered"
    fi
    plain_labels "$list" | while IFS= read -r label; do printf 'Delete :%s\nAdd :%s bool true\n' "$label" "$label"; done
    # Labels that must stay enabled (e.g. sshd, which a development VM turned on).
    for label in "$@"; do printf 'Delete :%s\nAdd :%s bool false\n' "$label" "$label"; done
    printf 'Save\nExit\n'
  } > "$commands"
  mkdir -p "$LAUNCHD" "$STATE"
  # Deleting a key that is not there prints an error and carries on, which is what we want.
  /usr/libexec/PlistBuddy "$target" < "$commands" > /dev/null 2>&1 || true
  rm -f "$commands"
  [ -s "$target" ] || { echo "could not write $target" >&2; exit 1; }
  plain_labels "$list" > "$remembered"
  [ "${APPARITION_TRIM_TEST:-}" = 1 ] || chown 0:0 "$target" "$remembered"
  chmod 644 "$target" "$remembered"
}

write_overrides "$HERE/appliance-disabled-user.txt" "$LAUNCHD/disabled.$USER_UID.plist"
write_overrides "$HERE/appliance-disabled-system.txt" "$LAUNCHD/disabled.plist" ${APPARITION_KEEP_SSH:+com.openssh.sshd}
# Spotlight would index the whole system volume: keep it away from the Data volume as well.
if [ -z "$DATA" ]; then mdutil -a -i off > /dev/null 2>&1 || true; else touch "$DATA/.metadata_never_index"; fi
echo "appliance trim written to $LAUNCHD"
