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
| [PPSSPP](#ppsspp) (PSP) | Vision Pro, iPhone | JIT works with a build that asks for it the JIT26 way |
| [Kodi](#kodi) | Vision Pro | Works, from its `.deb` (Apparition 0.3.2 and later) |
| [Provenance](#provenance) | Apple TV | The alpha runs; the stable 3.3.0 crashes at launch |

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

PlayStation Portable emulator. An unmodified PPSSPP has no JIT on devices that enforce
[the iOS 26 rules](../README.md) (A15 and M2 and later, and Vision Pro): it falls back to its
interpreter. A build that asks the debugger for its code memory with the JIT26 request
(`brk #0xf00d`) works: we ran PPSSPP's own CPU test through the JIT after Apparition prepared
the memory and let go. We tested a build patched for that, on a Vision Pro and an iPhone 15
Pro Max; stock builds need that support upstream first.

## Kodi

Kodi's official iOS download is a `.deb` package (from
[mirrors.kodi.tv](https://mirrors.kodi.tv/releases/darwin/ios-arm64/)), not an IPA.
Apparition 0.3.2 and later unpack the app from it: choose the `.deb` in the desktop app (Advanced, An app
file), pass it to `apparition install`, or list it in a source. We installed and ran Kodi 21.3 on a Vision Pro.

Earlier releases fail with `Signing failed … __LINKEDIT isn't final Mach-O segment`: Kodi's
bundled Python ships an object file (`python.o`) that isn't code. See [issue #7](https://github.com/illixion/Apparition/issues/7).

## Provenance

Multi-system emulator, `github:Provenance-Emu/Provenance`. On Apple TV it signs in about 70
seconds, installs in two minutes or so, and refreshes. The **alpha** release (turn on
prereleases) is a different app (`org.provenance-emu.provenance.alpha`) and runs. The stable
**3.3.0** asks for iCloud (CloudKit), which a free account can't give, and crashes at launch
when signed with one.

---

*Tested means we installed it through Apparition and ran it on that device, with the
Apparition release of the time (0.1 to 0.3 for the apps above). The versions of the apps
change; if a newer one behaves differently, tell us.*
