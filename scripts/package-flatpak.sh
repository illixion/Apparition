#!/usr/bin/env bash
# Prototype: wrap a package-release.sh Linux archive in a GNOME runtime.
# No host execution of the payload, so either architecture can be packaged on Linux.
# This deliberately does not publish a Flatpak remote or change existing release assets.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ $# == 1 ]] || { echo "usage: $0 <Apparition-Installer-VERSION-linux-ARCH.tar.gz>" >&2; exit 2; }
archive="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
name="$(basename "$archive")"
if [[ "$name" =~ ^Apparition-Installer-([0-9]+\.[0-9]+\.[0-9]+)-linux-(arm64|x86_64)\.tar\.gz$ ]]; then
  version="${BASH_REMATCH[1]}"
  arch="${BASH_REMATCH[2]}"
else
  echo "error: expected a versioned Linux release archive" >&2; exit 2
fi
flatpak_arch="$arch"
[[ "$arch" != arm64 ]] || flatpak_arch=aarch64
for tool in flatpak ar tar unsquashfs; do
  command -v "$tool" >/dev/null || { echo "error: install $tool first" >&2; exit 1; }
done

id=com.illixion.apparition.desktop
# Keep this runtime explicit: upgrading it needs the same pairing/signing/relay checks.
runtime=49
branch=prototype
out="$ROOT/build/flatpak"
mkdir -p "$out"
work="$(mktemp -d "$out/work-$arch.XXXXXX")"
app="$work/app"
mkdir -p "$work/input" "$work/deb" "$app/files/bin" "$app/files/libexec" "$app/files/share/applications" "$app/files/share/metainfo"
tar -xzf "$archive" -C "$work/input" Apparition.deb Apparition.AppImage apparition
(cd "$work/deb" && ar x "$work/input/Apparition.deb")
tar -xf "$work/deb/data.tar.gz" -C "$work/deb"
install -m755 "${FLATPAK_DESKTOP_BINARY:-$work/deb/usr/bin/Apparition}" "$app/files/libexec/Apparition"
# Distinct beyond case: macOS Docker bind mounts may be case-insensitive.
install -m755 "$work/input/apparition" "$app/files/libexec/apparition-cli"
install -m755 "$ROOT/packaging/flatpak/launch.sh" "$app/files/bin/Apparition"
install -m755 "$ROOT/packaging/flatpak/launch.sh" "$app/files/bin/apparition-cli"
# Tauri's resources, if a future build adds any beyond the embedded frontend.
if [[ -d "$work/deb/usr/lib" ]]; then
  cp -R "$work/deb/usr/lib" "$app/files/"
fi
# GNOME supplies GTK/WebKit, but not the Ayatana tray library Tauri dlopens.
# Reuse the tray libraries already bundled by our AppImage, including their
# dbusmenu dependencies. Extract SquashFS without executing either architecture.
offset=""
# The runtime itself can contain the magic bytes; validate each candidate.
candidates="$(LC_ALL=C grep -abo hsqs "$work/input/Apparition.AppImage" | cut -d: -f1)"
for candidate in $candidates; do
  if unsquashfs -s -o "$candidate" "$work/input/Apparition.AppImage" >/dev/null 2>&1; then
    offset="$candidate"
    break
  fi
done
[[ -n "$offset" ]] || { echo "error: no AppImage SquashFS payload" >&2; exit 1; }
unsquashfs -no-progress -o "$offset" -d "$work/appimage" "$work/input/Apparition.AppImage" >/dev/null
mkdir -p "$app/files/lib"
find "$work/appimage/usr/lib" \( -name 'libayatana*.so*' -o -name 'libdbusmenu*.so*' \) \
  -exec cp -P '{}' "$app/files/lib/" \;
[[ -f "$app/files/lib/libayatana-appindicator3.so.1" ]] \
  || { echo "error: AppImage does not contain the Ayatana tray library" >&2; exit 1; }
for size in 32x32 128x128 256x256; do
  icon="$work/deb/usr/share/icons/hicolor/$size/apps/Apparition.png"
  if [[ -f "$icon" ]]; then
    mkdir -p "$app/files/share/icons/hicolor/$size/apps"
    install -m644 "$icon" "$app/files/share/icons/hicolor/$size/apps/$id.png"
  fi
done
install -m644 "$ROOT/packaging/flatpak/$id.desktop" "$app/files/share/applications/"
install -m644 "$ROOT/packaging/flatpak/$id.metainfo.xml" "$app/files/share/metainfo/"
# usbmuxd remains a host service. /var/run in the sandbox resolves to /run; expose
# only its socket, not host devices, the whole home directory, or the host filesystem.
cat > "$app/metadata" <<EOF
[Application]
name=$id
runtime=org.gnome.Platform/$flatpak_arch/$runtime
sdk=org.gnome.Sdk/$flatpak_arch/$runtime
command=Apparition

[Context]
shared=network;ipc;
sockets=wayland;fallback-x11;
devices=dri;
filesystems=/run/usbmuxd;

[Session Bus Policy]
org.kde.StatusNotifierWatcher=talk
EOF
repo="$work/repo"
flatpak build-finish "$app"
# The icons are our own release assets. Validate them without nested bubblewrap,
# which is unavailable in an ordinary Docker container. This does not change the
# installed application's sandbox.
flatpak build-export --disable-sandbox --arch="$flatpak_arch" "$repo" "$app" "$branch"
bundle="$out/Apparition-Installer-$version-linux-$arch.flatpak"
flatpak build-bundle --arch="$flatpak_arch" \
  --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo \
  "$repo" "$bundle" "$id" "$branch"
echo "Built $bundle (GNOME $runtime, $branch branch)"
