# ApparitionHooks

MIT source for Apparition's own hook engine. This is the first implementation
stage of the ElleKit replacement. It is **not yet installed into tweaked apps**;
the shipping injector still bundles ElleKit.

Implemented:

- Objective-C method replacement, preserving the original implementation and
  isolating inherited methods to the requested subclass.
- Lookup of exported symbols through dyld, accepting C and Mach-O spellings.
- Rebinding resolved lazy and non-lazy import pointers by address, including
  pointers resolved by chained fixups in ordinary data sections. No executable
  memory is modified. Page protections are restored after each write, and
  failures are returned to the caller.
- Compilation on iOS and visionOS (devices and simulators), and executable
  fixture checks on macOS and the iOS simulator.

Run `scripts/check-hooks.sh`. To also execute on an already booted iPhone
simulator, use `scripts/check-hooks.sh --simulator <UDID>`. The checks build two
small libraries so calls actually cross an import slot; they check replacement,
calling the captured original, invalid arguments, symbol lookup, and inherited
Objective-C methods. Artifacts go to a newly created temporary directory.

## Limits and remaining implementation

This engine currently exposes an Apparition API, not the Substrate/libhooker ABI.
Do not replace the committed ElleKit binary with it yet. In particular:

- Rebinding imports does not intercept direct calls or unresolved lazy imports.
  Authenticated pointer sections are skipped. There is no pending-hook registry
  or dyld add-image callback yet. The caller must exclude concurrent unloading
  of images and keep replacements loaded. Calls into the Objective-C runtime by
  other swizzlers are not serialized by this library's lock.
- Symbol lookup does not yet inspect local symbols or the shared-cache symbol
  file. An underscore-prefixed name is tried literally before stripping `_`.
- Substrate/libhooker/blackjack export adapters and ABI conformance fixtures are
  still required, including correct per-hook error reporting.
- Full function-entry hooks still need a debugger capability handshake, original
  instruction relocation into debugger-prepared memory, and the existing 0x4151
  validated batch redirect protocol. Do not infer success from CS_DEBUGGED or
  probe for a debugger by issuing a breakpoint with no confirmed server.
- Stable original-call indirection and a pending-hook registry must precede late
  upgrades from import rebinding to entry hooks.
- The in-app JIT notice, enable-jit handoff, and attach-failure experience still
  need implementation. The install-time import warning is implemented in the
  Rust injector and uses its existing warning presentation across clients.
- DebugTrace's AnyView experiment and tvOS structural-change repro remain
  separate work. No DebugTrace files have been changed at this stage.
- Only after ABI coverage and device validation should a pinned-toolchain build,
  committed binary and hash replace `core/assets/ellekit` and its build script.

A simulator does not reproduce TXM policy. Device validation must distinguish
app-owned signed text from shared-cache text, and test both while attached and
after detaching. Neither class of debugger-served entry hook is implemented or
claimed by these checks.
