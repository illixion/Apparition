# Building your own apps with Apparition

Apparition can install other people's apps. With a Mac, it can also build yours: start a build of
your Xcode project from the headset, see it install, and read its crash logs on the same screen.
This guide explains how the pieces fit, how to set them up, and what each part of the Developer
screen does.

## The pieces

- **[appstore](https://github.com/illixion/appstore)** is an open-source build service that runs
  on your Mac. It builds your projects from their Git checkouts with its `build-and-sign` script,
  signs them with your developer certificate, and installs them on devices paired with the Mac
  (`xcrun devicectl`). It keeps every signed build with a manifest of exactly what went into it,
  pulls crash logs off your devices and symbolicates them, and receives debug traces from apps
  that link [DebugTrace](https://github.com/illixion/DebugTrace). It has its own web page too, but
  Apparition talks to its versioned service API.
- **Apparition on the Mac** (the desktop app) has a **Developer** tab that connects to appstore on
  the same Mac or another one. It can also register projects and send your signing certificates to
  a device.
- **Apparition on a device** (Apple Vision Pro, iPhone, iPad, Apple TV) has the same **Developer**
  tab, and its **Library** shows the apps your projects installed there, with their updates.

Apparition never builds anything itself. The Mac builds; Apparition asks it to and shows the
result. The device's own approvals still apply: connecting to the service doesn't grant debug
access or code changes.

## What you need

- A Mac with Xcode, Node 18 or newer and Homebrew's bash (`brew install bash`).
- A paid Apple Developer account, a development certificate exported as `.p12`, and a development
  provisioning profile listing your devices. A wildcard profile (App ID `*`) signs every app.
- Your devices paired with the Mac in Xcode (Window > Devices and Simulators).
- For a headset, iPhone or Apple TV away from the Mac: a way to reach the Mac over HTTPS. Tailscale
  Serve is the easy one (see "Away from home, over Tailscale" in the README).

## Setting up

1. **Install appstore.** Clone it and follow its README:

   ```sh
   git clone https://github.com/illixion/appstore
   cd appstore
   mkdir -p ~/.config/build-signing/{certs,profiles}
   cp examples/global.conf.example ~/.config/build-signing/global.conf   # then edit
   cp config.example.json config.json                                    # then edit
   ```

   Put your `.p12` in `~/.config/build-signing/certs` and your profiles in `profiles`. Each app's
   repository gets a gitignored `build-signing.conf` (from `examples/build-signing.conf.example`)
   naming its scheme and bundle ID.

2. **Set a service password.** Apparition needs one even if the web page runs without a login:

   ```sh
   node server.js set-password
   ```

3. **Start it.** `node server.js` serves `http://127.0.0.1:8484`. To keep it running, wrap it in a
   launch agent with `KeepAlive`.

4. **Connect.** In Apparition's Developer tab, enter the address and the password. On the Mac
   itself, `http://localhost:8484` works. A device needs the HTTPS address, for example
   `https://your-mac.your-tailnet.ts.net`. Apparition refuses plain HTTP to another host. The
   device can remember the password in its Keychain; the desktop app forgets the session when it
   quits.

5. **Add your projects.** On the Mac, Developer > **Register a project** takes the checkout's path,
   its branches, and one target per device it installs on: a platform alias (`visionOS`, `iOS`,
   `iPad`, `tvOS`), the device's name as Xcode shows it, and the bundle ID. You can also add them to
   appstore's `config.json` by hand.

## The Developer screen

Before you connect, it shows how to get started. Once connected it has four sections.

**Projects.** One card or row per project, showing its branch and commit and whether the checkout
has uncommitted changes. Each project lists its **targets**, one per device it installs on, with
what the Mac installed there last, whether that is still the checkout's commit, and whether the
device is reachable now. Each target can:

- **Install on _device_**: build and install it there. Under **Build options** you can choose the
  branch, launch it after installing, allow debug access (the device still asks), enable live
  patching, or build the Debug configuration.
- **Build only**: build and sign it without installing it. The Mac keeps the IPA, so you can
  download it, install it over the air, install it on this device through Apparition's own signing
  (on a device: **Install … on this headset**), or re-sign it later.
- **Re-sign…**: see below.

A project also lists its **kept builds** (every signed build the Mac made, exactly as signed) and
how many crash logs and traces its apps have. On a device, open a project to see all of this; the
Library's **Open Project** goes straight there.

**Activity.** The build queue: what is running or queued first, then recent builds, each with its
log and, while it runs, a Cancel. Builds of one project run one at a time.

**Reports.** Crash logs and DebugTrace traces, grouped by app. Crash logs open symbolicated
against the symbols the Mac kept for that exact build. On the Mac, this section also lists running
dev builds with a debug server; inspecting one asks for approval on the device.

**Devices & signing** (on the Mac) or **Service** (on a device). The devices the Mac can install to
(simulators are left out) and the connection. On the Mac, **Signing on devices** sends
build-and-sign's certificates and profiles into Apparition's Signing folder on a paired device, so
Apparition there can sign with your developer certificate instead of an Apple ID. Nothing is
reinstalled; enter each `.p12`'s password on the device once.

## Re-signing an app with another profile

**Re-sign…** signs an app that is already on a device with another of the Mac's provisioning
profiles (any `.mobileprovision` in `~/.config/build-signing/profiles`) and installs it over the
copy on the device. Use it to move an app onto a profile with a capability it now needs, onto
another team, or off a profile that is about to expire.

A device can't hand an installed app back, so the Mac needs the IPA:

- If it **kept the build that is installed**, it re-signs exactly that build.
- If it didn't (the app was installed before builds were kept, or the build was pruned), it first
  **builds the project target** from its current checkout without installing it, the same build
  Build only makes, and re-signs that. Activity shows the two jobs, the build first.

Then:

- A profile for the **same App ID** updates the app in place and keeps its data.
- A profile for a **different App ID** can't be installed over the old copy: the device refuses
  that update. The Mac copies the app's Documents and Library off the device into
  `~/.local/state/apparition/resign-backups/`, removes the app, installs the new signature and
  copies the data back, checking the file count. If the new signature won't install, it puts the
  previous one back with its data. Keychain items and app-group data don't move, and the backup is
  never deleted for you.
- **Check first** only tests that the app's data can be reached. It builds and changes nothing.
- Choosing to discard data skips the backup.

Re-signing needs the device reachable from the Mac. An over-the-air install can't change an
installed app's App ID.

## When something doesn't work

- **"Connect to the build service again."** The password changed or the session was revoked.
  Connect again.
- **A device is missing from a target.** It isn't paired with the Mac, or Xcode names it
  differently. The target's device name must match the name Xcode shows.
- **A build fails.** Read its log in Activity. The same build from a terminal is
  `build-and-sign` in the project's checkout, which prints the same output.
- **An app crashed.** Its crash log usually reaches Reports within a few minutes; the Mac pulls them
  from each device while it is reachable. If nothing arrives, appstore's own Traces page says why a
  device was skipped.

## Security

The service password protects everything, so use a long one and keep the service on localhost or
your tailnet. Apparition keeps the service session in memory (and, on a device, the password in its
Keychain if you allow it), sends only identifiers to the service and never file paths, and refuses
redirects. The service decides what is allowed: configured projects, targets, branches and
profiles only.
