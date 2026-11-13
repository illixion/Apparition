# The command line

`installer/` is `apparition`, the command line for Windows, Linux and macOS: it puts
Apparition (or any IPA) on the headset without Xcode, using the same Rust core, and runs the
companion (`apparition relay`) and pairs with a Mac for anisette (`apparition anisette pair`):

    apparition pair       # with Settings > General > Remote Devices open; type the PIN
    apparition pair --usb # an iPhone or iPad: a cable until it trusts this computer, then Wi-Fi works too
    apparition pairing hand-off   # give an Apparition already installed there its pairing
    apparition apps --device ATV --json   # what it has installed, with versions; never wakes a sleeping Apple TV
    apparition push App-signed.ipa --device ATV   # install an IPA already signed for the device, as it is
    apparition container pull com.example.app --device ATV backup/   # its Documents and Library; `push` puts them back, `count` counts
    apparition uninstall com.example.app --device ATV
    apparition launch com.example.app --device ATV --console   # launch, streaming its stdout/stderr here
    apparition launch com.example.emu --device AVP --jit --console   # launch suspended and serve its JIT requests
    apparition anisette pair my-mac.local   # Windows, Linux, macOS 27: once, for a Mac's anisette
    apparition install --apple-id you@example.com   # this Mac's anisette, or the paired Mac's
    apparition install --p12 development.p12 --profile wildcard.mobileprovision
    apparition install Other.ipa --apple-id you@example.com --anisette https://anisette.example
    apparition install kodi.deb --apple-id you@example.com   # a .deb package: the app inside is installed
    apparition sign UTM.ipa --apple-id you@example.com   # sign only, for a paired device; nothing installed
    apparition relay      # the companion, with this Mac's anisette on a Mac that gives it; until Ctrl-C

## Getting `apparition` on your PATH

On a Mac the command line is inside the app (from 0.3.3), so updating the app updates
it. Run it once to put `apparition` where every terminal finds it:

    /Applications/Apparition.app/Contents/MacOS/apparition-cli link

That makes `~/.local/bin/apparition`, a symlink into the app, and says what to add to your
`PATH` if that folder isn't on it (`export PATH="$HOME/.local/bin:$PATH"` in `~/.zshrc`).
`--dir /usr/local/bin` links somewhere else (needs `sudo`) and `--remove` undoes it. The link
points at the app's location, so run `link` again if you move the app. The command inside the
app is called `apparition-cli` because a Mac treats `apparition` and the app's own `Apparition`
as one name. On Linux, `link` does the same for wherever you unpacked the tarball. On Windows, setup offers
to make the command line available in every terminal, and `apparition-cli link` does it for any
copy: it puts that copy's folder on your `PATH` (`--system`, as an administrator, on everyone's;
`--remove` takes it off), for terminals opened afterwards.
The Windows command line needs a distinct name too: `apparition.exe` collides with the
desktop's `Apparition.exe` on a case-insensitive filesystem.

## Every command

`apparition` with no arguments (or `--help`) prints the full text; this is the shape of it.

| Command | What it does |
| --- | --- |
| `devices` | Lists the devices on this network that are paired or waiting to pair. |
| `pair [<address>[:<port>]]` | Pairs with a headset or Apple TV showing its pairing screen; finds it over mDNS without an address. |
| `pair --usb [<UDID>]` | Pairs an iPhone or iPad. Despite the name, the cable is only needed until it trusts this computer; after that this works over Wi-Fi. Also makes the pairing for Apparition on the device. |
| `pairing hand-off` | Gives Apparition, already installed on an iPhone, iPad or Apple TV, its pairing with that device, without reinstalling it. |
| `install [<app.ipa or .deb>]` | Signs and installs. With no file, installs the latest Apparition, checked against the release signature. Signs with `--apple-id <email>` (anisette from this Mac or the paired Mac, or `--anisette <url>` for a server you run) or `--p12 <file> --profile <file>`. `--no-watch` leaves the app's watch app out. |
| `sign <app.ipa or .deb>` | Signs for a paired device and installs nothing; says which capabilities the app got. |
| `watches [--device <name>] [--install <signed watch .app>]` | Lists the Apple Watches paired with an iPhone, and can put an already signed watch app on them through the phone. |
| `jit <bundle ID>` | Is the app's debugger from this computer so it can use JIT: launches it, or with `--attach` takes the running process, and serves its JIT requests. |
| `certificates`, `app-ids` | Show what uses up a free account's two certificates and ten App IDs, and free one you choose after you confirm. |
| `signing receive`, `send`, `hand-off`, `share`, `import` | Move the Apple ID's certificate key between your devices and computers; see [Sharing one certificate](PAIRING_AND_SIGNING.md#sharing-one-certificate). |
| `forget <name or UDID>` | Forgets a paired device. |
| `apps [--device <name or UDID>] [--all] [--json]` | What a paired device has installed, with versions, over this pairing's own tunnel. A sleeping Apple TV stays asleep. |
| `push <signed.ipa>` | Installs an IPA that is already signed for the device, as it is, with no re-signing. |
| `uninstall <bundle ID>` | Removes an app and its data container. |
| `container pull`, `push`, `count <bundle ID>` | Copies an installed, development-signed app's Documents and Library out, back in, or counts them: for backing data up across a reinstall. |
| `launch <bundle ID> [--console] [--jit]` | Launches an app (ending a running copy); `--console` streams its output here, `--jit` starts it suspended and serves its JIT requests. |
| `inject [<app>]` | On a Mac: patches the Swift sources edited since the build into a running app. |
| `build [--run] [--log] [--mcp] [--jit] [--inject]` | On a Mac, in a repo with a `build-signing.conf`: builds with Xcode, signs, keeps the build and installs it over this pairing; `--run` launches it. `apparition build --help` lists the rest. |
| `updates [--check] [--run]` | Which apps on your devices are behind their branch, from the Mac's Store; `--run` builds and installs the updates. |
| `store serve`, `status`, `projects`, `build`, … | Everything the Apparition Store does, without the app. `apparition store help` lists them. |
| `crash-reports list`, `show`, `done`, `add`, `import-dsyms` | What the Store holds about crashes and uploaded traces; `add` opens an encrypted problem report from a device. |
| `vm setup`, `update`, `status`, `serve`, `quit`, `check`, `remove` | The anisette VM for a Mac on macOS 27 (Apple silicon). `apparition vm help` lists the rest. |
| `vm-setup` | Sets up a macOS VM on this Mac as an anisette companion with no internet. |
| `relay` | Is the companion for Apparition on your devices until Ctrl-C (the same as the desktop app's Settings), offering this Mac's anisette to paired devices and computers on a Mac that gives it. |
| `anisette pair <address>`, `status`, `check`, `forget` | Pair this computer with a Mac running the companion, for its anisette; say which anisette sign-ins use; forget the pairing. See [Anisette](ANISETTE.md#how-anisette-works). |
| `anisette import [--accept-unverified] <file>...`, `import --reset` | Give the Android anisette Apple's libraries from the Apple Music APK or its `.so` files, when Apple's CDN can't be reached; checked against the tested version's SHA-256. See [Anisette](ANISETTE.md#when-apparition-cant-fetch-apples-libraries). |
| `anisette serve` | Serve this Mac's own anisette on loopback, for other programs on this Mac. |
| `mac pair <address>`, `status`, `forget` | Pair this computer with your build Mac: developer access to its Apparition Store and, where it offers it, its anisette, with one approval there. See [docs/REMOTE_DEVELOPER.md](REMOTE_DEVELOPER.md). |
| `deploy <app.ipa> [--device <name>] [--keep]` | Send an app built here (xtool, Xcode) to the paired Mac. With a device named, the Mac installs it itself when it can (paired with the device, certificate imported); otherwise the device's Apparition installs it. |
| `deploy-signing import <cert.p12> --profile <file>… \| list` | On the Mac: give its Store a certificate and profiles so it can sign and install what other computers send. |
| `mcp` | An MCP server on stdio for agents here: the debug servers of the apps the Mac sees, plus `deploy`. |
| `browse` | Lists the companions mDNS finds, for 10 seconds: a check that multicast works. |
| `link` | Puts `apparition` on your `PATH` (above). |
| `firewall allow`, `remove`, `status` | Windows: Windows Defender Firewall's rules that let your devices reach Apparition. Setup makes them; a copy from the zip, or one whose folder moved, can ask for them here. See [How it works](HOW_IT_WORKS.md). |
| `service install`, `uninstall`, `start`, `stop`, `status` | Windows: the companion as a background service, for a PC that serves your devices with nobody signed in. Setting Apparition up for everyone installs it. See [How it works](HOW_IT_WORKS.md). |

**Apple Watch apps.** An iPhone app that carries a watch app (`Watch/` inside the IPA) installs
on the paired Apple Watch too: Apparition asks the iPhone which Watch it is paired with, registers
the Watch on the team, signs the watch app with its own App ID and profile, installs the iPhone
app, and then puts the watch app on the Watch directly through the phone. iOS itself only copies a
watch app over for App Store and paid-team profiles, so a free account's watch app would never
arrive without this step. The Watch asks to trust the computer the first time (tap Trust on the
Watch); the record is kept. The watch app is one of the items in Install Options (`--no-watch`
on the command line), so it can be left off to save an App ID. Nothing on the Watch itself needs
setting up: no Wi-Fi, and the Watch stays on Bluetooth. Developer Mode was already on for the
Watch this was tested with, so whether a Watch without it accepts the install is not known.

A bare watchOS IPA, whose main app is the Watch app itself (the `-watchOS.ipa` the Apparition Store
builds), goes only to the Watch, through the iPhone, and nothing is installed on the phone. With
your own certificate, the watch app's profile has to list the Watch's UDID as well as the
iPhone's, or the Watch refuses it. Apparition on the iPhone itself does the same for an app with
a watch app; it keeps which Watch got it.

Removing an app from the native Library (Remove App) deletes it and its data from the device and
its watch app from the Watch it was put on, then drops the library entry whatever those steps
did: a device that can't be reached never leaves an app stuck in the list, and what could not be
removed is listed. Apps installed before this release have no record of their Watch copy, so
that one is left for you to delete. Data takeover between App IDs moves the iPhone's container
only; a watch app's data is not moved.

Anything that signs in asks for the Apple ID's password, and for the 2FA code when Apple sends
one, in the terminal; neither is kept unless you turn on Remember password on the device.
`APPARITION_LOG=<file>` writes the sign-in's progress (no secrets) to a file, for working out
why a sign-in failed. `apparition --verbose <command>` (or `APPARITION_VERBOSE=1`) also records each
request the sign-in sends to Apple and the answer, as structure and sizes (no passwords, tokens or
anisette values), in `apparition-verbose.log` beside that log, and says where it is
([what it holds](ANISETTE.md#seeing-what-a-sign-in-sent-to-apple-and-what-came-back)).

On Linux, `pair --usb` needs usbmuxd running. Nothing else there makes the device trust the
computer (Finder and Apple Devices do on macOS and Windows), so Apparition asks it itself and
saves the record to usbmuxd; there's no need for `idevicepair`.

Without an IPA it installs the latest signed release of Apparition (see [Releases](RELEASES.md)).

It pairs over the network once (the pairing is kept), signs with your own certificate or an
Apple ID, and installs. Installing Apparition also hands it what signed it, through its Files
inbox: the .p12 and profiles (it asks for the .p12's password once), or the Apple ID's
certificate key as a `.apparitionsigning` file. With the key, Apparition signs with the same
certificate as the computer once signed in to the same Apple ID, so the two use one of a free
account's two certificates instead of one each. For an Apparition installed some other way,
`apparition signing hand-off --apple-id <email> [--device <name>]` puts the key in its inbox on
its own. `apparition signing hand-off --build-signing [<dir>]` does the same for the signing
`apparition build` uses: every `.p12` under `~/.config/build-signing/certs` and `.mobileprovision` under `profiles`,
sent into Documents/Signing over the device's pairing, so the headset can sign what the Apparition Store
builds without a reinstall (`--p12 <file>... --profile <file>...` sends files you name). The
Developer view's **Signing on devices** does it with a button. Apparition on the device asks for
each .p12's password once; the passwords are never sent.

## Building it

    cd installer && cargo build --release          # this machine, into target/ at the root (one workspace with desktop/)
    cargo build --release --target x86_64-pc-windows-gnu                    # Windows, from macOS with mingw-w64
