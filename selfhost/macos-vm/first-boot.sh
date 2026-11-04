#!/bin/bash
# Experimental macOS 26 ownership probe. Runs as root inside the guest, once.
set -eu
STATE=/Library/Application\ Support/ApparitionBootstrap
exec >> "$STATE/boot.log" 2>&1
[ ! -e "$STATE/provisioning-v4-finished" ] || exit 0
trap 'printf "failed\n" > "$STATE/result"' ERR
if [ ! -e "$STATE/finished" ]; then
    # Refuse to alter a guest already provisioned with a real account.
    if dscl . -list /Users UniqueID | awk '$2 >= 501 && $2 < 1000 {found=1} END {exit !found}'; then
        printf 'existing-account\n' > "$STATE/result"
        exit 1
    fi
    # Let running macOS assign the first user's initial password and token.
    /usr/sbin/sysadminctl -addUser apparition -fullName 'Apparition Companion' \
        -password "$(cat "$STATE/password")" -admin > /dev/null 2>&1
    /usr/bin/touch "$STATE/finished"
fi
id apparition > "$STATE/account"
/usr/sbin/sysadminctl -secureTokenStatus apparition > "$STATE/token-status" 2>&1
/usr/bin/dscl . -read /Users/apparition GeneratedUID > "$STATE/account-guid"
/usr/sbin/diskutil apfs listUsers /System/Volumes/Data > "$STATE/volume-users" 2>&1
# loginwindow's on-disk automatic-login settings. Password is ASCII hex generated
# by the host; never trace this section or log its inputs.
/usr/bin/defaults write /Library/Preferences/com.apple.loginwindow autoLoginUser -string apparition
PASSWORD=$(cat "$STATE/password")
key="125 137 82 35 210 188 221 234 163 185 31"
set -- $key
length=${#PASSWORD}
padded=$(( (length / 12 + 1) * 12 ))
: > /etc/kcpassword
i=0
while [ "$i" -lt "$padded" ]; do
    if [ "$i" -lt "$length" ]; then byte=$(printf '%d' "'${PASSWORD:$i:1}"); else byte=0; fi
    k=$(( i % 11 + 1 )); eval "kb=\${$k}"
    printf "\\$(printf '%03o' $(( byte ^ kb )))" >> /etc/kcpassword
    i=$((i + 1))
done
chmod 600 /etc/kcpassword
chown 0:0 /etc/kcpassword
unset PASSWORD
/usr/bin/fdesetup status > "$STATE/filevault-status" 2>&1
/usr/sbin/sysadminctl -autologin status > "$STATE/autologin-status" 2>&1
# SSH is a management path independent of Finder and Setup Assistant.
/usr/bin/install -d -o apparition -g staff -m 700 /Users/apparition/.ssh
if [ -f "$STATE/authorized_keys" ]; then
    /usr/bin/install -o apparition -g staff -m 600 "$STATE/authorized_keys" /Users/apparition/.ssh/authorized_keys
fi
/bin/launchctl load -w /System/Library/LaunchDaemons/ssh.plist
# Suppress per-user upgrade/setup panes as well as the system setup marker.
PREFS=/Users/apparition/Library/Preferences/com.apple.SetupAssistant
mkdir -p /Users/apparition/Library/Preferences
for flag in DidSeeCloudSetup DidSeeSiriSetup DidSeePrivacy DidSeeAppearanceSetup DidSeeScreenTime DidSeeAccessibility; do
    defaults write "$PREFS" "$flag" -bool true
done
defaults write "$PREFS" LastSeenCloudProductVersion -string "$(sw_vers -productVersion)"
defaults write "$PREFS" LastSeenBuddyBuildVersion -string "$(sw_vers -buildVersion)"
chown apparition:staff "$PREFS.plist"
chmod 600 "$PREFS.plist"
/usr/bin/touch /var/db/.AppleSetupDone
printf 'provisioned\n' > "$STATE/result"
/usr/bin/touch "$STATE/provisioning-v4-finished"
# Send readiness before shutting down, even if the periodic reporter has not run yet.
/bin/bash "$STATE/report-status.sh" --once || true
# loginwindow may have started before account creation; restart once after setup.
/sbin/shutdown -h now
