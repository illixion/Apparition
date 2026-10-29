# Apps we've tested

Apparition signs any IPA, and any `.deb` package that holds an app. Whether an app then runs
depends on the app: what it asks Apple's frameworks for, what a free account can give it, and
whether it needs JIT. This is the list of apps we ran ourselves, and what we found. An app
that isn't here may well work; it just isn't something we've checked.

**Doesn't work, or works differently than you expect?** [Open an *App support*
issue](https://github.com/illixion/Apparition/issues/new?template=app_support.yml). Apps we
get working end up here.

| App | Tested on | Result |
| --- | --- | --- |
| [UTM](#utm) | Vision Pro | Works, with JIT |
| [ARMSX2](#armsx2) (PS2) | Vision Pro | Works, with JIT |
| [Madeira](#madeira) (Windows apps) | Vision Pro | Works; its Enable JIT button finds Apparition |
| [PPSSPP](#ppsspp) (PSP) | Vision Pro, iPhone | Runs without JIT. JIT only worked in a patched build we made; there's no published one |
| [Kodi](#kodi) | Vision Pro | Works, from its `.deb` (Apparition 0.3.2 and later) |
| [Provenance](#provenance) | Apple TV | The alpha runs; the stable 3.3.0 crashes at launch |
| [LiveContainer](#livecontainer) | iPhone, Vision Pro | Runs apps on iOS 27.0.1 (Delta); can't start them on visionOS 27.2 |

"JIT" here is Apparition's **Launch with JIT**, which stays attached as the debugger and
prepares the memory the app asks for. It works on current chips, where the old
"attach once" trick doesn't (see the [README](../README.md)).

## UTM

- **Source:** `github:utmapp/UTM`, asset `UTM-visionOS.ipa`, with prereleases on (the
  visionOS build is only published as a release asset). Apparition's Add Source has it as a
  suggestion.
- **Works:** installs, refreshes, and runs a Linux VM with JIT, launched from Apparition.
  It asks for the increased memory limit, which a free account gets.
- **Caveat:** UTM's ExtensionKit helper (`Extensions/`) is rejected by the installer once the app
  is signed under a different bundle ID, so Apparition removes it before signing. It only
  runs a few disk-image tools.

## ARMSX2

PlayStation 2 emulator. Needs JIT; with **Launch with JIT** its 305 MiB of code memory is
prepared in about two seconds. We ran GTA San Andreas on a Vision Pro
([video](https://youtu.be/mNR0ik2QfEY)).

## Madeira

Runs Windows apps with Wine and FEX. Its **Enable JIT** button opens a `stikdebug://` link,
which Apparition answers: it asks, then stays the app's debugger until the app closes. On
Vision Pro the prompt opens in its own window (0.3.1 and later).

## PPSSPP

PlayStation Portable emulator. A stock PPSSPP runs, but without JIT on devices that enforce
[the iOS 26 rules](../README.md) (A15 and M2 and later, and Vision Pro): it falls back to its
interpreter, which is slow.

JIT needs PPSSPP to ask the debugger for its code memory with the JIT26 request
(`brk #0xf00d`), which it doesn't. We got it working in a build we patched ourselves: Apparition
prepared the memory and let go, and PPSSPP's own CPU test then ran through the JIT and matched
its expected output, on a Vision Pro and an iPhone 15 Pro Max. That patch isn't in PPSSPP and
nobody publishes a build with it, so for now you can't get PPSSPP with JIT from Apparition or
anywhere else. Other emulators that already speak the request (ARMSX2, UTM) don't have this problem.

## Kodi

Kodi's official iOS download is a `.deb` package (from
[mirrors.kodi.tv](https://mirrors.kodi.tv/releases/darwin/ios-arm64/)), not an IPA.
Apparition 0.3.2 and later unpack the app from it. On the device, download the `.deb` in Safari and
share it to Apparition (or open it from Files, or use Library > Install from a File): Apparition names
the app and asks before installing. The desktop app takes it too (Advanced, An app file), as does
`apparition install`, or list it in a source. We installed and ran Kodi 21.3 on a Vision Pro.

Earlier releases fail with `Signing failed … __LINKEDIT isn't final Mach-O segment`: Kodi's
bundled Python ships an object file (`python.o`) that isn't code. See [issue #7](https://github.com/illixion/Apparition/issues/7).

## Provenance

Multi-system emulator, `github:Provenance-Emu/Provenance`. On Apple TV it signs in about 70
seconds, installs in two minutes or so, and refreshes. The **alpha** release (turn on
prereleases) is a different app (`org.provenance-emu.provenance.alpha`) and runs. The stable
**3.3.0** asks for iCloud (CloudKit), which a free account can't give, and crashes at launch
when signed with one.

## LiveContainer

Runs other apps inside itself, so a whole library of apps takes one of a free account's ten App
IDs a week. It needs Apparition 0.3.5 or later, which can export the certificate it signs with.
We ran Delta in it on an iPhone 15 Pro Max with iOS 27.0.1.

1. **Install LiveContainer.** In Browse > Add Source, add `github:LiveContainer/LiveContainer`
   with **Asset name contains** set to `LiveContainer.ipa`, then install it. That picks the
   standalone build; `LiveContainer+SideStore.ipa` bundles SideStore, which you don't need with
   Apparition.
2. **Export the certificate.** In Apparition, open Settings > Share Signing with Another Device,
   and under For Other Apps tap Export Certificate as .p12. It signs in if it has to, and makes no
   new certificate: it is the one Apparition already signs with. Tap Copy Password (the six
   words stay on the clipboard for two minutes), then Share Certificate File… and save it to
   Files. Apparition deletes its copy when you close the screen.
3. **Import it in LiveContainer.** Settings > Import Certificate, choose the `.p12`, and type the
   password. On iOS 26 and later LiveContainer won't run an app without it ("JITLess mode is
   required since iOS 26").
4. **Add an app** to LiveContainer with its own + button and an IPA, then tap Run. LiveContainer
   closes and reopens into the app; that is how it starts one, not a crash.

**JIT for an app inside LiveContainer:** set LiveContainer's JIT enabler to StikDebug (Settings),
then turn on Launch with JIT in the app's settings. Apparition answers LiveContainer's link:
it launches LiveContainer again under the debugger, which then starts the app. An app that asks
for JIT itself (the JIT26 request) is served as it would be outside LiveContainer, with no
script. An app made for a StikDebug JIT script of its own (Geode's, for one) needs that script
added to it in LiveContainer. Apparition then runs the script as the app's debugger, and asks
you first each time, because a script can read and change everything in the app. Requires
Apparition 0.4.1 or later. Not yet tested on a device.

Refresh LiveContainer in Apparition like any other app; the apps inside it keep their data. If
Apparition's certificate changes (one revoked, or a new one made), export and import it again.
Delete the `.p12` from Files once LiveContainer has it: with its password, anyone can sign apps as
your Apple ID.

**Vision Pro:** LiveContainer 3.8.0 installs and imports the certificate on visionOS 27.2
(release candidate), but can't start an app there. It stops with `Assertion failed:
(mainBundleAddr != NULL)`: to make the app think it's running as itself, it searches
CoreFoundation's code for a pattern that visionOS 27.2 doesn't have. Its app list also crashes
while drawing an icon for an app that has none cached. Both are LiveContainer bugs, not signing
problems, and LiveContainer doesn't target visionOS.

---

*Tested means we installed it through Apparition and ran it on that device, with the
Apparition release of the time (0.1 to 0.3 for the apps above). The versions of the apps
change; if a newer one behaves differently, tell us.*
