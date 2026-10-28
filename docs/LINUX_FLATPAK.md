# Linux Flatpak prototype

This is a test distribution of the existing Linux desktop and static CLI in a
GNOME 49 runtime. It separates GTK, WebKitGTK and glibc from the host distribution.
It is not yet a published Flatpak repository or a Flathub submission. The existing
deb and AppImage releases remain available.

## Build

First build a Linux release archive with `scripts/package-release.sh`, or download
an existing one from GitHub Releases. On Linux with Flatpak, binutils and
squashfs-tools installed:

```sh
scripts/package-flatpak.sh build/release/0.3.5/Apparition-Installer-0.3.5-linux-arm64.tar.gz
```

Alternatively, package either architecture from macOS or Linux using Docker:

Allow at least 4 GiB of memory for the Docker VM; OSTree bundle compression can
exceed a 2 GiB limit.

```sh
docker build -t apparition-flatpak-packager packaging/flatpak
docker run --rm -v "$PWD:/src" apparition-flatpak-packager \
  build/release/0.3.5/Apparition-Installer-0.3.5-linux-arm64.tar.gz
docker run --rm -v "$PWD:/src" apparition-flatpak-packager \
  build/release/0.3.5/Apparition-Installer-0.3.5-linux-x86_64.tar.gz
```

The bundles land in `build/flatpak/`. Assembly does not run the payload, so it
does not require CPU emulation or a privileged Docker container. It repackages the
archive's desktop binary, resources, CLI and AppImage's tray libraries; it does not rebuild Rust against the
Flatpak SDK. Moving compilation into the SDK and adding an offline source manifest
are follow-up work before a Flathub submission.

## Install and run

```sh
flatpak remote-add --user --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
flatpak install --user ./Apparition-Installer-0.3.5-linux-arm64.flatpak
flatpak run com.illixion.apparition.desktop
```

Use the x86_64 bundle on x64 machines. Both use the `prototype` branch and the
existing desktop app ID. The runtime is installed separately from Flathub; the
bundle is not a fully offline installer. Updating to another prototype means
installing its bundle again.

The CLI is available inside the same sandbox and shares its settings and pairings:

```sh
flatpak run --command=apparition-cli com.illixion.apparition.desktop --version
flatpak run --command=apparition-cli com.illixion.apparition.desktop pair --usb
```

For a server without a desktop, continue using the release's standalone static
CLI. Flatpak is being evaluated for desktop distribution.

## NixOS and Fusion

Enable these NixOS services and apply the configuration:

```nix
services.flatpak.enable = true;
services.usbmuxd.enable = true;
networking.firewall.allowedTCPPorts = [ 49160 6969 ];
networking.firewall.allowedUDPPorts = [ 5353 ];
```

Merge the ports into existing lists. TCP 6969 is needed only when sharing builtin
anisette, TCP 49160 for the relay, and UDP 5353 for discovery. USB pairing still
needs the host's usbmuxd, regardless of the application's packaging.

1. Connect the iPhone/iPad to **the NixOS VM** using Fusion's USB device menu.
2. Unlock the device. Check `systemctl status usbmuxd` and `ls -l /run/usbmuxd`.
3. Open Apparition, choose USB pairing, and accept Trust on the device when asked.
4. Try builtin anisette, Apple ID sign-in, signing and installation.
5. Check discovery and companion connectivity from an Apple device. USB passthrough
   does not guarantee guest LAN multicast; test direct guest IP access separately
   if discovery fails with Fusion NAT.
6. Close and reopen the window, then quit from the app. Check relay availability
   while the window is closed. A desktop without a tray reopens the window by
   launching the app again.

Useful diagnostics, without signing in or changing a device:

```sh
flatpak info --show-permissions com.illixion.apparition.desktop
flatpak run --command=sh com.illixion.apparition.desktop -c \
  'ls -l /run/usbmuxd /var/run/usbmuxd; ldd /app/libexec/Apparition'
```

## Permissions and remaining work

The prototype grants network access (including incoming relay connections),
Wayland/X11, GPU rendering, the tray watcher D-Bus name, and `/run/usbmuxd`. It
does not grant the whole home directory, host command execution or raw USB access.
The file chooser should use the desktop portal. Data stays under Flatpak's
per-app XDG directories; existing native-install data is not migrated.

Known integration gaps to address before recommending this package:

- The native **open at login** checkbox is not Flatpak-aware. Do not rely on it;
  use a desktop startup entry running `flatpak run com.illixion.apparition.desktop --hidden`
  for manual testing. A Background portal implementation is the intended fix.
- The Docker anisette start/stop action cannot launch host Docker. Use builtin
  anisette, or start the external server on the host and select the local server.
- Tray icon export, file portals, graphics, builtin anisette and device workflows
  need runtime validation. Successful bundle creation proves packaging only.
- No automatic package updates, native-data migration, signed Flatpak remote or
  Flathub review yet. Keep these bundles as local test artifacts.

## Prototype validation (2026-10-28)

Both architecture bundles were exported successfully. On the ARM64 NixOS Fusion
VM with the niri desktop, the bundle installed, the window rendered, and the
companion remained running when the window was closed and reopened. The desktop
and bundled tray libraries resolved against the runtime without missing libraries.
The sandbox exposed `/run/usbmuxd` at the application's expected `/var/run/usbmuxd`
path. The relay listened on TCP 49160 and advertised over mDNS. The bundled CLI's
builtin anisette server fetched its libraries, started, and answered
`/v3/client_info`; Apple provisioning and sign-in were not exercised.

Signing/installation, file chooser portals, and connectivity from another device
still need testing. The x64 bundle has not
been run on an x64 desktop. Fusion's graphics driver emitted Mesa warnings, but
the ARM64 window rendered without a graphics workaround.

USB pairing was subsequently checked with an iPhone through Fusion passthrough.
It exposed a pairing identity collision: without `/etc/hostname` or an exported
`HOSTNAME`, the computer used the same name as the phone app, so the second
pairing invalidated the first. The Flatpak launchers now export the runtime's
hostname for both desktop and CLI. Using that distinct identity completed USB
pairing and reached the phone's authenticated network service. Signing and
installation remain untested in this VM.
