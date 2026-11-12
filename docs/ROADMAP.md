# Developer workflow: status and what's next

The intended product is Apparition for building, installing, running and diagnosing apps.
The Apparition Store, built into the desktop app, supplies build jobs, artifacts and report
storage, and `apparition build` is the build executor. DebugTrace stays an independent SDK.
The AltStore feed stays compatible, and the job API exposes build progress, errors and
cancellation to Apparition.

Implementation order:

1. **Establish patching limits on hardware.** Optimized entry patches, stale-inline
   controls and compiled Swift scalar/String replacements now pass on the headset.
   Next: shared app types, calling-convention and metadata handling, thread coordination
   and representative workloads. Retain signed-dylib injection while investigating.
2. **Connect the build service.** Authenticated version 1 service access, configured
   projects/devices, explicit build jobs, logs and report browsing are implemented in
   desktop Apparition. The initial device Developer tab adds service sign-in, project
   and branch selection, build-only/install jobs, logs, cancellation and registered-build
   browsing. Service passwords can be remembered in this device's Keychain, separately
   per service, for automatic reconnection. Bearer sessions stay in memory and
   authenticated requests refuse redirects. Disconnect forgets the service password.
   Desktop Apparition can also register an existing Mac checkout and its targets.
   The Library matches installed debuggable apps against configured project bundle IDs.
   Matching cards prefer the service target configured for the paired device, and use
   `UIDeviceFamily` to identify iPhone/iPad apps running in compatibility mode. They show
   project/platform, installed and retained build identity, JIT launch and report counts
   for the installed `CFBundleVersion`, and open the project's Developer page, where
   build-only, separately labelled device-install actions, re-signing and reports live. A retained build can be downloaded into Apparition's normal
   install review and signing flow without rebuilding it. Per-app automatic update
   preparation watches retained artifacts while the service refreshes, downloads a newer
   build and opens that same review; installation still requires confirmation. Crash text
   and trace text files can be opened and closed on device.
   Apps without a project match stay in Other Apps. Next: finish installation/session
   relationships and round out device-side report navigation.
   A Mac with Xcode executes Apple builds; other desktop platforms can connect to it.
   Service credentials stay separate from the existing relay.
3. **Unify build identity.** Connect projects, immutable builds, artifact digests and
   binary UUIDs to signed installations, sessions, patch history and reports. Preserve
   original and installed bundle IDs. Public build IDs now join jobs, installed-build
   ledger entries, traces and immutable symbol sets; symbols are selected by the app's
   actual binary UUIDs and retained across updates, including builds not yet deployed.
   Immutable artifact digests, binary identities, before/after source/dependency snapshots
   and verified downloads are now implemented and tested through a real service build.
   Next: remaining installation/session relationships and report patch history. Distinguish
   dirty local iterations from published builds.
4. **Add Xcode handoff.** Retaining an existing Xcode product and its symbols without
   rebuilding or changing its signature, with an optional generated XcodeGen scheme that
   captures source context before building, is not part of 1.0. Keep debugger ownership
   explicit when it returns: Xcode/LLDB and Apparition must not compete for the same
   process. JIT preparation and Swift patching remain separately selectable capabilities.
5. **Close the reporting loop.** Add reviewed user submissions to Apparition, then
   connect other apps' DebugTrace reports to their exact builds. Add a portable report
   envelope for Rust/Tauri failures, build registration and scoped external submission
   credentials. Keep unverified imports separately classified. Report upload never
   grants live debug access; preserve device approvals for inspection and code changes.
6. **Consolidate packaging.** Done for 1.0: the builder is `apparition build`, the Store runs
   inside the desktop app (or `apparition store serve`), and configs and ledgers are read as
   they were.

Structural Swift changes need a separate approach: recreate a component with selected
state migration, or rebuild/relaunch with state restoration. Executable-memory access
alone cannot change the layout of instances that already exist. A debugger-assisted
backend must retain per-patch user approval before it becomes a product feature.
