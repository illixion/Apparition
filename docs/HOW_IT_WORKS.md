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

Windows Defender Firewall drops what your devices send until a rule allows Apparition. Windows
asks about that itself the first time Apparition listens: allow it, and it is remembered. That
question appears only on the desktop and only once, so two cases are left, where the companion
would be blocked without a word, and Apparition handles them by making the rule itself
(`core/src/firewall.rs`), with an administrator's approval when it doesn't already run as one. A
question that went unanswered (at login, say) leaves a block rule and isn't asked again: the
desktop app's Companion page then offers a button, and `apparition relay`, `apparition signing
receive` and the app's receiving of a signing key replace the rule before they listen. And over SSH
there is no desktop to ask on: the command line makes the rule there. The rule allows incoming connections to that one program, on the kinds of
network (private, public, domain) the PC is on at that moment. To make one by hand, or with another
firewall, allow inbound TCP 49160, 49161 and 49162 for the companion and its encrypted anisette
channel, e.g. from an elevated PowerShell: `New-NetFirewallRule -DisplayName "Apparition" -Direction
Inbound -Protocol TCP -LocalPort 49160,49161,49162 -Action Allow -Profile Private`. Port 6969 stays
on the Mac's loopback and needs no rule.
On Linux, open the same ports if a firewall runs (`sudo ufw allow 49160,49161,49162/tcp`, or firewalld's `--add-port`), and
UDP 5353 for mDNS. On macOS, Local Network
privacy blocks an unbundled binary's multicast, so `apparition relay` there can't advertise
itself: use the desktop app, or type the address in Apparition. `apparition browse` lists the
companions mDNS sees from a machine. (The relay protocol was tested 2026-10-11 from macOS over
Tailscale and from a Windows 11 VM on the LAN, then with the Swift and Rust companions this
app replaces.)
