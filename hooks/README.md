# ApparitionHooks

Apparition's MIT replacement for ElleKit. The injector embeds this library as
`Frameworks/CydiaSubstrate.framework/CydiaSubstrate`, retaining the install name
existing tweaks link. No ElleKit binary or source is shipped. `exports.txt` lists
the Substrate, libhooker and blackjack compatibility exports checked at build.

## Hook behavior

Objective-C hooks use the runtime and preserve inherited implementations without
changing the superclass. Symbol lookup accepts Mach-O headers returned by the
image APIs, exported names with or without a leading underscore, and local
symbols present in a loaded image's symbol table. Image handles are retained;
closing a compatibility image handle is a no-op.

Without JIT, function hooks rebind import pointers. This includes ordinary
chained-fixup pointers and classic lazy imports using their symbol table and
two-level library ordinal. Add-image callbacks apply hooks before a newly loaded
image's initializers. Calls within a library retain the original entry behavior.

With Apparition's debugger, the library relocates the original entry into memory
allocated and written by the debugger, then submits a validated entry redirect
through protocol 0x4151. That server stops all threads, refuses occupied entries
and stale expected bytes, verifies each write, and rolls back a failed transaction.
An entry that is busy is retried. The app never makes its own code pages writable.

The first original pointer handed to a tweak is a precompiled thunk loading an
atomic data cell. Before attachment it calls the original entry; after publication
it calls relocated code. A later hook of the same entry chains through the prior
replacement. Re-registering the identical replacement does not create recursion.
Original pointers and successful redirects remain usable after debugger detach.

A dedicated `__DATA,__aphooks` control section is the capability handshake.
Apparition discovers it while the process is stopped and publishes support for
version 1. Only then may the library issue `brk #0xf00d` requests. It never guesses
from CS_DEBUGGED and never probes an unknown debugger with a breakpoint. Support
is revoked before detach. A live P_TRACED check rejects stale grants after
an abrupt disconnect; CS_DEBUGGED is never used for that check. Explicit hook-only sessions (`hooks-only=1` in the URL, `--hooks-only` in the CLI)
detach after pending work completes;
if the app has also made its own JIT preparation requests, the server stays.
Ordinary JIT attachment always stays available for future requests.

The library offers **Turn On**, **Not Now** and **Don't Ask Again** when hooks need
JIT. Turn On opens the existing enable-jit URL with this app's bundle ID and pid,
so attachment upgrades pending hooks without restarting it. Without Apparition
on the device the notice explains the computer-launch route instead. The injector
adds `apparition` to LSApplicationQueriesSchemes while preserving existing entries.

## Honest compatibility limits

- Short entries, unsupported instructions and authenticated import sections are
  refused, never overwritten speculatively. The relocator supports common ARM64
  prologues, ADR/ADRP, literal loads, conditional branches, BL and terminal B.
  It conservatively rejects entries whose IP0/IP1 values would be clobbered by
  the fixed redirect. Import fallback remains available for those functions.
- Private symbols stripped from an image and unavailable in its mapped symbol
  table cannot be invented. Lookup returns null. Shared-cache symbol-file access
  depends on OS/sandbox availability; there is no entitlement bypass.
- Cross-process injection is unavailable to a jailed library. MSHookProcess can
  load into the current process and refuses other pids with ENOTSUP.
- Executable LHPatchMemory and LHExecMemory requests report failure without the
  debugger; writable data patches work directly. The void
  MSHookMemory API copies and queues a bounded patch for later attachment. Memory
  patches are limited to 4 KiB and executable allocations to 1 MiB per call.
- Original storage and executable mappings live until process exit. There are
  1,024 independently hooked entries. Keep replacements loaded and exclude
  concurrent image unloading when using the low-level APHookImports API.
- Unknown libhooker options are refused. Error strings have static lifetime.

These are jailed-process limits, not a promise of jailbreak behavior. A tweak
that depends on other processes, private entitlements or filesystem paths outside
its app still needs adaptation.

## Build and checks

`scripts/build-hooks.sh` uses the Xcode build pinned in
`scripts/release-toolchain`, checks every compatibility export and compares both
the iOS and visionOS dylibs and SHA256SUMS to committed assets. Use `--update`
only when intentionally replacing them. No network or third-party sources are
needed. The build uses a content-derived UUID and has no embedded absolute source paths.

- `scripts/check-hooks.sh`: cross-compile all sources for iOS/visionOS devices and
  simulators; run compatibility, import, original, Objective-C and new-image
  initializer fixtures on macOS.
- `scripts/check-hooks.sh --simulator <booted-iPhone-UDID>`: also run on iOS.
- `scripts/check-hooks.sh --debugger`: a real macOS debugserver executes late
  upgrade and relocated original calls, including ADR, ADRP, literal loads,
  conditional branches and BL. This is a debugger-written-code test, not an
  in-process writable-code substitute for TXM testing.
- Rust tests verify the committed platform/hash pairs, actual import warnings,
  mailbox discovery/revocation and redirect rollback/stale/busy handling.
- Apparition Lab's `hookPrepare` and `hookProbe` endpoints exercise app text and
  a behavior-preserving shared-cache `atoi` hook before and after attachment and
  detach. Build/launch with `SCHEME_NAME=ApparitionLab TARGET_NAME=ApparitionLab
  BUILD_BUNDLE_ID=com.illixion.apparition.lab build-and-sign --run --mcp`, prepare
  the fixtures, then use the freshly built CLI's `jit ... --attach --hooks-only`.

The planned DebugTrace SwiftUI erasure experiment is separate from replacing the
Substrate runtime; this change does not relax DebugTrace's structural patch checks.

The signed injected-IPA and shared-cache-after-detach results on Vision Pro are
recorded in [docs/LAB.md](../docs/LAB.md#apparitionhooks-replacement-2026-11-14).
Debugger errors now attempt a confirmed detach. An interrupted entry publication
that cannot be restored closes that exact app instead of abandoning it paused.
