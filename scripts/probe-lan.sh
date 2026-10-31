#!/bin/bash
# probe-lan.sh <endpoint> [json-args]: call Apparition on the headset by LAN address,
# for when Tailscale is off (LocalDevVPN holds the VPN slot) and the apps MCP cannot
# reach http://avp. Uses Apple's curl: macOS Local Network privacy blocks Homebrew
# binaries and Xcode's python. Set APPARITION_HOST to the headset address.
# build-and-sign skips the session record when the app is unreachable at deploy time, so the
# token comes from the newest build in the key ledger (every dev build gets one there).
: "${APPARITION_HOST:?set APPARITION_HOST to the headset IP}"
token_file="" key=""
for f in $(ls -t "$HOME/.local/state/debugtrace/keys/"*.json); do
  [[ $(/usr/bin/plutil -extract bundleId raw -o - "$f" 2>/dev/null) == com.illixion.apparition ]] && { token_file=$f; key=commandToken; break; }
done
[[ -n $token_file ]] || { token_file="$HOME/.local/state/debugtrace/sessions/com.illixion.apparition@AVP.json"; key=token; }
hdr=$(mktemp); trap 'rm -f "$hdr"' EXIT
/usr/bin/plutil -extract "$key" raw -o - "$token_file" | sed 's/^/Authorization: Bearer /' > "$hdr"
case "$1" in probeState|_logs) m=GET;; *) m=POST;; esac
/usr/bin/curl -s -m 90 -X "$m" -H @"$hdr" -H 'Content-Type: application/json' -H 'X-DebugTrace-Client: claude-probe' \
  ${2:+--data "$2"} "http://$APPARITION_HOST:8644/$1"
