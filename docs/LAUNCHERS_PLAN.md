# Home Screen launchers (plan)

Status: plan, nothing built yet (2026-11-04). Branch `multi-icon-launchers`.

## Goal

Apparition installs small **launcher apps**. Each one is a Home Screen icon that opens one URL.
That gives Vision Pro something it lacks: neither Safari's Add to Home Screen nor Shortcuts
can put an icon on the Home View. Two consumers:

1. **JIT launchers**: one tap starts UTM or an emulator under Apparition's debugger
   (`apparition://launch?app=<library id>&jit=1`).
2. **Launchers requested by other apps**: Raven pins a web page
   (`raven://open?url=…`), and any app the user installed can ask for one.

## How it works

```
[Launcher icon]  ──openURL──▶  [Apparition]  ──JIT launch──▶  [UTM]
  stub app with                 launch?app=…
  one URL in Info.plist

[Launcher icon]  ──openURL──▶  [Raven]  raven://open?url=…   (Apparition not involved)
```

The launcher opens its target URL directly. It needs Apparition only when the URL is an
Apparition one (JIT). Apparition installs and refreshes launchers, and lists them in the Library.

## 1. The launcher stub

- A new target `ApparitionLauncher` holds one tiny Swift app, built for visionOS, iOS and
  tvOS. It has no app groups and no extensions. On launch it reads `ApparitionLauncherURL`
  from its own Info.plist, opens that URL and then gets out of the way: `exit(0)` once the
  open succeeds. If the target app is gone, it shows a single "isn't installed" view.
- **It ships as a template inside Apparition:** a zipped, unsigned `.app` per platform in
  Resources. It's zipped so that Xcode doesn't treat it as a nested app and sign or validate
  it, so Apparition stays a plain single app for a free first install from Xcode.
  `scripts/package-release.sh` and project.yml build the stub and copy it in.
- **Per launcher, Apparition edits only these things, never the binary:**
  - `CFBundleIdentifier`
  - `CFBundleDisplayName`
  - `ApparitionLauncherURL`
  - `ApparitionLauncherOwner`, the requesting app's bundle ID
  - the icon

  Then it signs and installs the result through the existing pipeline, the same as any IPA in
  the library.

### Icons: the main risk, so it's spike 1

visionOS takes icons from a compiled `Assets.car` (a layered `.solidimagestack`). Apparition
can't run `actool` on the device. Try these in order:

1. **Loose PNG icons** (`CFBundleIcons` › `CFBundleIconFiles`). iOS still honours them for
   sideloaded apps. Check whether the visionOS Home View does.
2. **A template `Assets.car`** with one placeholder icon, with the image data replaced at
   signing time. That needs a small CoreUI/BOM writer in core/, or a crate if a permissively
   licensed one exists.
3. **Fallback:** a fixed set of generic icons compiled into the template (one per colour or
   glyph), with only the name personalised. This is enough for v1 if 1 and 2 both fail.

tvOS needs a layered top-shelf image. Use the same template approach with a fixed image.

## 2. Slots and App IDs

- Each launcher is an app on the device and needs an App ID. On a **free profile** it takes
  one of the 3 app slots, Apparition included. The confirmation sheet says so up front, and
  the existing AppLimit card handles the error if it happens anyway.
- The **App ID reassigner** (install review, `LibraryModel` ~1690) picks the identity: reuse a
  registered App ID that nothing currently holds before registering a new one. Default new
  IDs to `com.illixion.apparition.launcher.<short hash>.<team>`. Launchers claim nothing for
  extensions, so the `reserved` and `owners` bookkeeping only gains a single target.
- A launcher has no data worth keeping, so the reassigner can always reclaim its ID without
  asking about data takeover (`discardDestinationData`).

## 3. Library model

- `InstalledApp` gains a field `launcher: Launcher?` holding `url`, `owner` (the requester's
  bundle ID or `apparition`), `iconFile` and `createdAt`. The IPA is generated, not
  downloaded, so `ipaFile` points at the patched stub, kept so refresh re-signs it as usual.
- Refresh treats launchers like any other app. Launchers sort after the owner they belong to,
  so when slots are short the app is refreshed before its icon.
- **Removal:** when a launcher's icon is deleted from the Home View, the next reconcile
  (`assignments.reconcile`) finds it gone, drops the record and frees its App ID for reuse.
  When the owning app is removed, Apparition offers to remove its launchers too.
- UI: the Library row of a JIT-capable app gets **Add to Home View** (JIT). A Launchers
  section lists every launcher with its owner, its URL and Remove.

## 4. Request API for other apps

A URL that any app can open:

```
apparition://add-launcher
  ?name=<display name>
  &url=<target URL, percent-encoded>
  &icon=<https URL of a PNG>          or  &icon-data=<base64url PNG, ≤ 64 KB, square>
  &callback=<URL to open when done>   (optional; gets &result=added|cancelled|failed&id=…)
```

- **Consent:** every request opens a sheet showing the icon, name, target URL and requester,
  and what it costs (for example "uses 1 of your 3 free app slots"). Nothing installs without
  a tap. Modelled on the existing enable-jit request window (`UI/JITRequestWindow.swift`).
- **Who's asking:** SwiftUI's `onOpenURL` doesn't say. Add a scene delegate
  `scene(_:openURLContexts:)` and read `UIOpenURLContext.options.sourceApplication`, which
  the system fills in for apps from the same team. Apps Apparition installed are signed with
  the user's team, so that covers them. With no source app, label the requester "Unknown app".
- **Allowed URLs:** the target URL's scheme must be one the requester declares
  (`CFBundleURLSchemes` in the IPA Apparition holds), or `https`. A request can't make a
  launcher that opens a third app, or an `apparition://` URL other than through Apparition's
  own UI.
- Later: `apparition://launchers?callback=…` for the requester to list its own launchers,
  and `remove-launcher?id=`. Not v1.
- A small Swift helper requesting apps can copy (it builds the URL and parses the callback)
  goes in this doc. It doesn't go in RAVESDK until a second app uses it; Raven is the first.

## 5. JIT launchers

- Add the `apparition://launch?app=<library id>&jit=1` route in `RootView`'s URL handling,
  backed by `ShortcutActions.launchJIT` (which already checks expiry and busy, and picks the
  companion or LocalDevVPN).
- Icon: the target app's own icon from its kept IPA, with a small bolt badge so it can't be
  confused with the app itself.
- After launching, Apparition can step back: on visionOS, dismiss its window once the JIT
  session is attached; the keep-alive session notice (`JITSessionNotice`) stays.

## 6. Raven (separate repo, after spike 1)

- Register the `raven` scheme and handle `raven://open?url=` by opening a tab or window.
- Add a **Add to Home View** menu item. It takes the page title, the best `apple-touch-icon`
  or manifest icon (sent as `icon=` when the URL is public, `icon-data=` otherwise), and
  `url=raven://open?url=<page>`, then opens Apparition's request URL.
- Raven installed through Apparition passes the same-team check. Raven built from Xcode under
  the same team does too.

## Spikes, in order

1. **Icons on visionOS:** hand-patch a stub (loose PNG, then a swapped `Assets.car`), sign
   and install it with bas, and look at the Home View. This decides how custom icons work.
2. **Stub behaviour on visionOS:** does open-then-exit feel instant, or does a window flash?
   Does `openURL` to Apparition bring Apparition's window forward?
3. **sourceApplication** is filled in on visionOS 26/27 for a same-team request.
4. Then build the stub target, the patch-and-sign path in core, the library model and UI,
   the request API, and Raven.

## Not doing

- Web Clip profiles (iOS only; they could come later as a no-slot option on iPhone).
- An injected auto-JIT library. It's still an option for apps that tolerate late JIT, but
  it's separate from this.
- Launchers that open a third app on another app's behalf.
