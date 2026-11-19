# The lab: JIT and live-patching experiments

`Lab/` is a test app for JIT experiments, launched through the probe's `launchJIT`. These are the experiments
behind the live-patching backend; they establish what is possible on hardware, not product behaviour.

Its first experiment (2026-10-11, visionOS 27.2): bytes the debugger writes into the
app's own signed `__TEXT` keep running after it detaches, including code first run
only after the detach and a site rewritten to branch to a trampoline. Apparition serves
this through an experimental JIT26 command, `x16 = 0x4150` (copy x2 bytes from x1 to x0).

## Optimized entry-patch experiment

The lab now runs a second experiment **before** the original experiment detaches. It
branches from an optimized compiler-generated C function to a replacement already linked
in the app, then restores the original instruction. A caller in the target's translation
unit deliberately inlines the original function. The harness is in a separate translation
unit, and the Lab target explicitly enables optimization and disables LTO.

Build and install the isolated lab, then launch it through Apparition's debugger:

```sh
xcodegen
SCHEME_NAME=ApparitionLab TARGET_NAME=ApparitionLab \
  BUILD_BUNDLE_ID=com.illixion.apparition.lab \
  apparition build --platform iOS --device AVP
apparition jit com.illixion.apparition.lab --device AVP --wait 60
xcrun devicectl device copy from --device AVP \
  --domain-type appDataContainer --domain-identifier com.illixion.apparition.lab \
  --source Documents/optimized-patch-report.json \
  --destination build/optimized-patch-report.json
```

The report records baseline/patched/restored return values, loop checksums, write results,
and five timings per phase in nanoseconds per call. Breadcrumbs are synced before risky
steps to `Documents/optimized-patch-steps.txt`. A normal launch reports `not-traced` and
does not issue debugger requests. Use a fresh launch for each experiment.

`[11,17,11]`, and the inlined caller returned `[11,11,11]`; loop checksums matched all three
phases. Tiny-function timings varied substantially, including after restoration, so they
establish no application-level performance improvement. This proves optimized entry
redirection and the stale-inline limitation. It does **not** yet demonstrate Swift-native
patching, dynamically generated replacement code, arbitrary type changes, or an alternative
to DebugTrace's current injection backend. Ordinary-launch controls also remain unpatched.

## Native Swift entry-patch experiment

The lab also tests optimized native Swift entries with a separately compiled, development-
signed dylib delivered into its Documents directory after the app build. It runs before the
C experiments detach the debugger. Only the dylib's address getters have a C ABI; patched
functions use Swift's native calling convention, without `-enable-implicit-dynamic` or
`@_dynamicReplacement`.

```sh
python3 scripts/run-swift-entry-lab.py --platform visionOS --device AVP
# For a paired iPhone instead:
python3 scripts/run-swift-entry-lab.py --platform iOS --device ixPhone
```

The runner builds and installs only `com.illixion.apparition.lab`, compiles and signs the
replacement, copies it to the device, launches through Apparition JIT, and validates both
the patched run and an ordinary-launch control. `--skip-build` reuses an already installed
lab of the selected platform. It overwrites the device report with a pending marker before
each launch, so a crash cannot reuse an old success. Reports stay under ignored
`build/swift-entry-lab/<platform>/`. Runtime checkpoints are in
`Documents/swift-entry-steps.txt`.


- A native Swift integer entry returned `[11,17,11]` before patching, after patching and
  after restoration. Both a Swift function reference and a normal compiled Swift call
  observed the replacement. The latter reads an opaque runtime input on every call to
  prevent constant specialization and reuse of a pure function's earlier result.
- A native `String -> String` entry returned `original:hello`, `replacement:hello`, then
  `original:hello`. All 1,000 heap-backed String calls matched the replacement result.
- All entry/island writes and restorations succeeded. The native visionOS replacement
  was 155,582,456 bytes away, beyond a direct ARM64 branch's range: a reserved nearby
  island successfully redirected the entry through an absolute jump.
- An ordinary launch returned original results without sending any debugger requests.

This is a single-threaded, ARM64 lab proof using signed dylib loading. It has no general
function discovery or dependency tracking, does not allocate anonymous JIT code, and does
not test custom app types, generics, async/throwing methods, pointer authentication, active
stack frames or concurrent patch publication. Existing inlined or constant-folded results
can still retain old behavior. Keep the dylib loaded for the process lifetime. The experimental Developer/CLI backend now uses explicit non-optimized boundaries,
all-stop publication, image/instruction validation, and the same device approval as
DebugTrace injection; see [the developer workflow](DEVELOPER.md#live-patches).
General optimized functions, caller dependency tracking and automatic type migration remain
unsupported. No application performance claim follows from these correctness tests.
