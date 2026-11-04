#!/bin/bash
# Root guest telemetry; shares only setup state, never credentials or anisette headers.
set -u
STATE=/Library/Application\ Support/ApparitionBootstrap
REPORT="$STATE/report-$$.json"
while :; do
  PROVISIONED=false
  [ -e "$STATE/provisioning-v4-finished" ] && PROVISIONED=true
  TOKEN=false OWNER=false SIP=false AMFI=false SSH=false RELAY=false ANISETTE=false SETUP=false
  sysadminctl -secureTokenStatus apparition 2>&1 | grep -q 'ENABLED' && TOKEN=true
  GUID=$(dscl . -read /Users/apparition GeneratedUID 2>/dev/null | awk '{print $2}')
  if [ -n "$GUID" ]; then
    diskutil apfs listUsers /System/Volumes/Data 2>/dev/null | grep -A4 "$GUID" | grep -q 'Volume Owner: Yes' && OWNER=true
  fi
  csrutil status 2>/dev/null | grep -q disabled && SIP=true
  case "$(sysctl -n kern.bootargs)" in *amfi_get_out_of_my_way=1*) AMFI=true ;; esac
  launchctl print system/com.openssh.sshd >/dev/null 2>&1 && SSH=true
  lsof -nP -iTCP:49160 -sTCP:LISTEN >/dev/null 2>&1 && RELAY=true
  lsof -nP -iTCP:6970 -sTCP:LISTEN >/dev/null 2>&1 && ANISETTE=true
  pgrep -x 'Setup Assistant' >/dev/null 2>&1 && SETUP=true
  CONSOLE=$(stat -f%u /dev/console)
  printf '{"provisioned":%s,"token":%s,"owner":%s,"sipDisabled":%s,"amfiActive":%s,"ssh":%s,"relay":%s,"anisette":%s,"setupAssistant":%s,"consoleUID":%s}\n' \
    "$PROVISIONED" "$TOKEN" "$OWNER" "$SIP" "$AMFI" "$SSH" "$RELAY" "$ANISETTE" "$SETUP" "$CONSOLE" > "$REPORT"
  ACTION=$("$STATE/guest-status" < "$REPORT")
  case "$ACTION" in
    shutdown) /sbin/shutdown -h now; exit 0 ;;
    reboot) /sbin/shutdown -r now; exit 0 ;;
  esac
  [ "${1:-}" != --once ] || exit 0
  sleep 3
 done
