# Away from home, over Tailscale

Apparition's companion works over [Tailscale](https://tailscale.com), so a computer left at
home keeps signing and refreshing apps, and launching them with JIT, wherever the device is.
Doing this alongside Tailscale is the new part: AltStore needs its computer on the same Wi-Fi,
and SideStore and StikDebug need LocalDevVPN, which takes the device's only VPN slot.

New to Tailscale? It puts your devices on one private network wherever they are, free for
personal use, with nothing to open on your router. The same network reaches a Mac's Screen
Sharing or a game streaming PC from anywhere, for example with
[Longwave](https://longwave.pro), a remote desktop app for Vision Pro, iPhone and iPad.

1. Install Tailscale on the computer that runs the desktop app, and on the device (the
   Tailscale app from the App Store), signed in to the same tailnet. Leave the desktop app
   running on that computer.
2. Set the companion up once at home with Find Companion (or by typing either of its addresses,
   e.g. its Tailscale one, `100.x.y.z:49160`, shown in Tailscale's menu on the computer). The
   companion tells the device all its addresses, LAN and Tailscale, and a stable id. Before each
   action Apparition then picks where to reach it: a LAN address when the device is on that
   network (where it can also wake a sleeping Mac, below), the Tailscale address anywhere else.
   Nothing to switch when you leave home or come back, and before it uses another address it
   checks that the same computer answers there.
3. Under Anisette, choose Use Companion's Anisette: Apparition reaches it at the same Tailscale
   address, so signing in works away from home too. Anisette for your devices comes from a Mac, from Apparition's own
   Android anisette or from a third-party server (see [Anisette](ANISETTE.md#how-anisette-works)): use your Mac (a Mac on macOS 26
   or earlier, Intel included, is best; a macOS VM gives SMS codes only), or Apparition's own (a Mac with 8 GB,
   or on macOS 27 with no VM, or a Linux PC), or a third-party server, as this companion, or pair the
   device with the Mac as a second companion for signing in.
4. Keep Tailscale connected on the device when a refresh or a Shortcuts automation runs.
   Tailscale takes the VPN slot LocalDevVPN would, and doesn't need to share it: the companion
   replaces LocalDevVPN.

**A sleeping Mac wakes up for the device.** With "Wake for network access" on (System Settings >
Battery > Options, or Energy on a desktop Mac; `pmset -g` shows `womp 1`), a sleeping Mac keeps
answering on its network and wakes when a connection arrives for a port it listens on. So when
the companion doesn't answer, Apparition on the device knocks: it connects to the companion's
port at the Mac's LAN address, again and again for up to 20 seconds, until the Mac is up
(`Probe/CompanionWake.swift`; a MacBook Pro asleep answered in 8.6 seconds). It knows the Mac's
LAN addresses from the companion's own reply, so a companion saved by its Tailscale address is
knocked at home too (`CompanionRoutes` in the same file picks the address, for the relay and the
paired anisette Mac alike; Apps MCP's `followCompanion` runs that choice and says the result). It works only on the same network: Tailscale's UDP never wakes a Mac, so away
from home leave it awake. Woken this way macOS sleeps again within a minute, so the companion
holds it awake while devices use it and for three minutes after (`core/src/stay_awake.rs`, the
assertion `caffeinate -i` takes; `pmset -g assertions` shows "Apparition companion serving a
device"). A Windows or Linux PC needs a Wake-on-LAN broadcast, which iOS lets an app send only
with an entitlement Apple grants on request, so a PC companion isn't woken.

Only encrypted traffic crosses the companion, and it only connects back to the device that
asked. If a Windows or Linux companion doesn't answer over Tailscale while it does at home, its
firewall is blocking the Tailscale network: allow TCP 49160, 49161 and 49162 there too (see [How it works](HOW_IT_WORKS.md)).
