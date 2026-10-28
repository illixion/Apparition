#!/bin/sh
set -eu
# Flatpak hides /etc/hostname and does not export HOSTNAME. Without this the
# core falls back to "Apparition", also the phone app's pairing identity: its
# second pairing replaces the computer's first one on iOS.
if [ -z "${HOSTNAME:-}" ]; then
  HOSTNAME=$(hostname)
fi
export HOSTNAME
case "$0" in
  */apparition-cli) exec /app/libexec/apparition-cli "$@" ;;
  *) exec /app/libexec/Apparition "$@" ;;
esac
