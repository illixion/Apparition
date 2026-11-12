# Using Apparition day to day

## About Apple IDs

Use a spare Apple ID for sideloading if you can. Apparition never stores
the password unless you turn on Remember password on the device, where it stays in the Keychain,
and it sends it only to Apple. The first sign-in from a computer or device Apple hasn't seen asks
for the code Apple sends to your other devices. Since 12 October 2026 Apple checks the
"anisette" data that comes with a sign-in more strictly; see [Anisette](ANISETTE.md#how-anisette-works) for what that
means and how Apparition handles it.

A free Apple ID has two development certificates. Every Apparition can sign with the same one
once it has that one's key, so the way to stay under two is to share it rather than revoke
(see [Sharing one certificate](PAIRING_AND_SIGNING.md#sharing-one-certificate)). Signing doesn't make a second
certificate while Apparition elsewhere has one: it stops and offers to get that one's key, or
to make one anyway. When both slots are taken, Apparition offers to receive another
Apparition's key first, then lists the certificates and revokes one only after you confirm.
Apple doesn't always free the slot right away: after a revoke it can keep refusing a new
certificate ("Maximum number of certificates reached") with only one listed, possibly because it
counts a certificate that free accounts can't list. If so, try again later, or share the
certificate you already have from another device.

## Installing, signing and pairing in detail

**Install apps** from AltStore and SideStore sources, from a GitHub project's releases
(`github:owner/repo`), or from an IPA you have. Apps published as a `.deb` package, as Kodi's
are, install the same way: Apparition unpacks the app from the package. On the device itself,
download the `.ipa` or `.deb` in Safari and share it to Apparition (or open it from Files,
or use Library > Install from a File): it names the app and asks before installing. A link like
`apparition://source?url=https%3A%2F%2Fexample.com%2Fsource.json` opens Add Source with the
source filled in, ready to add, for a source's own "Add to Apparition" button.

**Sign** with a free Apple ID, which Apple limits to apps that last 7 days and two
certificates, or with a paid account's certificate and profile, which last a year and need no
sign-in. An app that asks for the increased memory limit (UTM, emulators) gets it, with a
free account too; extended virtual addressing needs a paid account.

**Pair once, then work over Wi-Fi.** Vision Pro and Apple TV pair over Wi-Fi with the code
they show in Settings, no cable or Developer Strap. An iPhone or iPad pairs over a USB cable
the first time (it has no pairing code); after that it's reached over Wi-Fi. Apparition keeps
a copy of its pairing in the device's Keychain, so reinstalling it doesn't unpair it.

**Share one certificate** between your devices and computers, so a free account's two
certificates never run out one per device: nearby over the network, checked by a six-digit
code both screens show, or as a file sealed under five words. Signing stops and asks before
it would make a second.

## What refreshes when

Every re-sign is a round of requests to Apple, so Apparition only does one for an app that
needs it, and "needs it" is one number: 3 days or less left. The Library's **Refresh
Expiring** button, the background runs and Shortcuts' Refresh Apps (without "All Apps") all
use it; with nothing due the button is greyed out and Apparition contacts Apple for nothing.
Asking by name ignores it: an app's own Refresh button, the Developer tab's Install update
and "Automatically install updates", and Refresh Apps with "All Apps" on.

Which apps are due is read from the stored expiry dates, so the Library shows "N apps due for
refresh" and tints their pills orange without asking anyone.

Apparition contacts external servers only when you ask. Check for Update on its card asks the
release site, Reload Sources in Browse fetches the sources, and the Refresh Apps shortcut has a
**Check for Updates** option that does both for what is installed, at the same time as the
signing: the release site, and only the sources your installed apps came from. Off by default.
Nothing fetches any of them in the background, and the Library opens without a request.

When one of those checks finds an update, Apparition posts one quiet notification (no sound, in
the notification list) for it, and says nothing more about that update on later checks. Settings,
Notifications turns it off.

## Permission prompts

Apparition asks for three things, each when it first needs it, and none of them has a reason
beyond what its text says (the strings live in `project.yml`):

- **Local Network**, when it first looks for this device's developer services, a companion or
  a nearby device with a signing key, or connects to the build service you set up.
- **Location** (iOS only), the first time it becomes an app's JIT debugger. It is needed to keep
  Apparition running in the background while it serves JIT, and the updates are dropped unread,
  never stored or sent. visionOS keeps it running with silent audio and asks for nothing.
- **Notifications** (alerts only, never sound), at first launch, so the first update a Shortcuts
  run finds can be announced without a prompt nobody sees. Skipped if Settings, Notifications is
  off, and asked when you turn that on.

At first launch Apparition asks for Local Network and Notifications. It uses no camera,
microphone, photos, contacts or Face ID.

## Shortcuts automation

Background refresh is not reliable, so the dependable way to keep apps signed is a
Shortcuts automation. Apparition offers three actions: **Refresh Apps** (apps that expire
within 3 days, or all with "All Apps", and optionally checking for updates; returns how many it refreshed), **Launch with JIT**
(an installed app, through the companion or LocalDevVPN) and **Open Apparition**. Each
opens Apparition and runs there, because a refresh takes 40 to 150 s. Siri knows them too:
"Refresh apps in Apparition", "Launch UTM with JIT in Apparition".

To refresh every day: Shortcuts, Automation, New Automation, Time of Day, pick a time
you are usually wearing the headset, Daily, Run Immediately, then add Apparition's
Refresh Apps action. It needs "Remember password" on and the companion (or LocalDevVPN)
reachable. Apps refreshed are reported as "Refreshed UTM; expires Oct 18"; when nothing
is due it says when the next app expires; and when nothing could be refreshed the action
fails with the reason (not signed in, companion unreachable), so the automation shows it.

## Your own developer certificate

With a paid developer account, Apparition can sign without an Apple ID (Settings > Your
Certificates > Certificates & Profiles). That screen is separate from the Apple ID's and works
offline: it never signs in or contacts Apple, and the Apple ID's own Certificates & App IDs
screen (Settings > Apple ID) asks Apple only when you press Load from Apple. Import an Apple Development or Distribution certificate as a .p12, plus development, ad hoc or
enterprise profiles that list the headset. Apps then sign on the headset for as long as their
profile lasts, usually a year, with no sign-in, anisette server or free-account slots. A wildcard
profile keeps each app's own bundle ID, and development profiles keep Launch with JIT working.
The .p12 password is only used to open the file; the decrypted key is kept in Apparition's
container, like the free account's key.

### Both an Apple ID and a certificate

With an Apple ID and an imported certificate (a free account for trying apps out, a paid team
for keeping them), Apparition asks what signs a new app before its first install: Install in
Browse, or an .ipa you open. It also lists a copy of the app that another team signed, if the
headset has one. A copy signed by another team can't be replaced, so the choice has to come first.
The Apple ID adds its team ID to the bundle ID and a certificate keeps it, so the two kinds
install side by side with data of their own. Where a certificate from another team would need the
same bundle ID, Apparition says so before installing, rather than passing on installd's error.

Changing an app's signer later (its menu in the Library) changes its bundle ID too, as does an
Apple ID that signs for another team. The app comes back as a new copy; Apparition then copies
the old copy's files to it through the same tunnel (house_arrest), and removes the old copy only
once every file arrived. Turn it off in Settings > Certificates & Profiles. Caches are left behind, and so are
Keychain items and app group data, which belong to the signing team. Only a development-signed
app can have its data read, so a distribution-signed one keeps its data in the old copy.
