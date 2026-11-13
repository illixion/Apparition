# How refreshing, the companion and the network fit together

Sources are AltStore/SideStore JSON, or `github:owner/repo` for apps published only as
release assets (UTM's visionOS build).
Installed apps keep their IPA in Documents/apps, so a refresh re-signs and reinstalls
without the network, and their data survives. With "Remember password" on, the password
stays in this device's Keychain, and a refresh can sign in with nobody present (Apple
skips 2FA for a device it already trusts). Background tasks ask to run this about every
12 hours: a short app-refresh task, and a processing task for refreshes that outlast its
~30 s (uploading UTM takes 40 to 150 s). They need the companion or LocalDevVPN, and their
firing is not verified yet (`scheduleBackgroundRefresh` is the test hook).

The companion replaces LocalDevVPN: the headset resets connections it makes to its own
RemotePairing service, so Apparition asks a companion on a computer (TCP 49160) to connect
back to it. The headset then sees the connection coming from that computer. The companion
only ever connects back to the address that asked, and it carries only ciphertext
(pair-verify, then TLS-PSK) with no keys. With the headset on Tailscale, which LocalDevVPN no
longer occupies, a companion at home keeps signing and JIT working from anywhere. Tested
2026-10-11 with LocalDevVPN off: tunnel, refresh (109 s over the LAN, 150 s over Tailscale)
and JIT launch. On the LAN, Apparition finds the companion over mDNS ("Find companion"); over
Tailscale, type its address. The companion announces itself only at IPv4 addresses a device
can use (no loopback, IPv6 or self-assigned 169.254 ones), under a host name of its own
(`apparition-<name>.local`). A device saves the companion's name with its address: when the
saved address stops answering (a new DHCP lease, another network), it finds the companion again
by name. The desktop app's "Let devices on this network find this computer" turns the
announcement off for networks you don't trust; devices then need the address typed, and ones
that already have it keep connecting. `apparition relay --no-mdns` is the same. The companion is
the desktop app, which keeps it running from
the menu bar or tray, or `apparition relay` on the command line (see [the command line](COMMAND_LINE.md)); both run
`core/src/companion.rs`.

Windows Defender Firewall drops what your devices send until a rule allows Apparition, and a rule
names a program by where its file is. Apparition's setup for Windows (`scripts/windows/setup.nsi`)
puts the files where they stay and makes the rules for the app and the command line, so an
installed copy is never asked; removing it takes the rules away. It does that through the command
line (`apparition-cli firewall allow`, `core/src/firewall.rs`), which a copy from the zip can run
too. A rule allows incoming connections to that one program on private and domain networks, and on
a public one if the PC is on one at that moment.

A copy unpacked from the zip has no rule yet. Windows
asks about that itself the first time Apparition listens: allow it, and it is remembered. That
question appears only on the desktop and only once, so two cases are left, where the companion
would be blocked without a word, and Apparition handles them by making the rule itself, with an
administrator's approval when it doesn't already run as one. A
question that went unanswered (at login, say) leaves a block rule and isn't asked again: the
desktop app's Settings then offers a button, and `apparition relay`, `apparition signing
receive` and the app's receiving of a signing key replace the rule before they listen. And over SSH
there is no desktop to ask on: the command line makes the rule there. To make one by hand, or with another
firewall, allow inbound TCP 49160, 49161 and 49162 for the companion and its encrypted anisette
channel, e.g. from an elevated PowerShell: `New-NetFirewallRule -DisplayName "Apparition" -Direction
Inbound -Protocol TCP -LocalPort 49160,49161,49162 -Action Allow -Profile Private`. Port 6969 stays
on the Mac's loopback and needs no rule.

**With nobody signed in (Windows).** Setting Apparition up for everyone on a PC also installs the
companion as a Windows service, `Apparition`, which starts with Windows: the relay, and the anisette
the app's Settings is set to offer (Apparition's own, or a server's address). It is
`apparition-cli service run`, and `apparition-cli service install` makes it from a copy unpacked
from the zip. Three things differ from the app's companion:

- It runs as `NT AUTHORITY\LocalService` with no privilege but the one every program has, not as
  the system, since it takes connections from the network and runs the Android anisette's libraries
  in its own process. Its own identity (`NT SERVICE\Apparition`) is given the companion's folder
  (`%APPDATA%\Apparition Installer\anisette` of whoever ran setup: the paired devices and the
  anisette's state) and nothing else of that person's, so not their pairings or signing keys.
- Nobody can answer it, so it declines a device that asks to pair. Pair in the app.
- It steps aside for an Apparition someone is using. The app, and `apparition relay` in a terminal,
  hold a claim on the ports while they run (`companion::Front`, an open file in the companion's
  folder); the service lets go within a second or two and takes the ports back when that program
  ends, however it ends. The app keeps a copy of its companion settings in that folder for the
  service, which starts again with them when they change.

`apparition-cli service status` says whether it runs and where its log is.

On Linux, open the same ports if a firewall runs (`sudo ufw allow 49160,49161,49162/tcp`, or firewalld's `--add-port`), and
UDP 5353 for mDNS. On macOS, Local Network
privacy blocks an unbundled binary's multicast, so `apparition relay` there can't advertise
itself: use the desktop app, or type the address in Apparition. `apparition browse` lists the
companions mDNS sees from a machine. (The relay protocol was tested 2026-10-11 from macOS over
Tailscale and from a Windows 11 VM on the LAN, then with the Swift and Rust companions this
app replaces.)
