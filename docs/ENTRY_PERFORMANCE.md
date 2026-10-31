# Native entry backend: performance and receiver validation

The entry backend removes implicit-dynamic instrumentation from the whole module.
Each explicitly declared patch boundary still uses `@inline(never)` and
`@_optimize(none)` to prevent stale optimized copies. Keep these boundaries small;
put hot implementation code outside them.

## Reproducible host comparison

Run on the Mac with Xcode:

```sh
python3 scripts/benchmark-entry-backends.py
python3 scripts/benchmark-entry-backends.py --inner 1 --rounds 2000000 \
  --output build/entry-performance/call-heavy.json
```

The script builds three isolated executables with `-O`: a normal build, an
implicit-dynamic build, and an Apparition build with one explicit entry boundary.
It checks matching observable checksums, excludes one warm-up repetition and
shuffles execution order for each of nine measured repetitions. Reports preserve
every sample, compiler flags, toolchain, OS and workload parameters. No app is
installed and no debugger is attached.

Measured on arm64 macOS 27.0.1, Apple Swift 6.4 (swiftlang-6.4.0.34.1):

| Workload | Normal | Dynamic | Apparition entry |
|---|---:|---:|---:|
| Compute-heavy: 10,000 calls × 1,000 mixing steps | 3.886 ns/step | 3.971 ns/step (1.022×) | 3.878 ns/step (0.998×) |
| Call-heavy: 2,000,000 calls × 1 mixing step | 0.990 ns/step | 7.031 ns/step (7.100×) | 1.845 ns/step (1.863×) |

These are host microbenchmarks, not application/frame-time measurements. They do
not include patch redirection, debugger attachment, code publication or device
thermal behavior. The compute-heavy difference between normal and entry is within
sample variation; it establishes no speedup. The call-heavy result shows that the
selected boundary still costs time even when other code stays optimized. It does
not predict how much an app will improve. Raw reports are generated under
`build/entry-performance/` and are not checked in.

## Struct receiver validation

`ApparitionLab` now has explicit instance, mutating and static entry boundaries on
`EntryWorkflowValue`, a non-generic struct. `entryProbe` calls all three and retains
one receiver across requests. The compiler-backed rewriter tests also compile and
link replacement extensions for all three receiver conventions without implicit
dynamic flags.

On physical Vision Pro (native visionOS 27.2), one CLI injection replaced all three
entries in a single all-stop transaction. The instance and static calls observed
the new code; the mutating receiver advanced from 3 to 9 instead of being recreated.
Restoring the bodies restored the static result to 20 and advanced the same mutating receiver from 9 to 10; the instance call read that retained state. Class receivers, generic
receivers, protocol dispatch, async and throwing methods remain refused. This is
additional ABI coverage, not a general Swift method-patching guarantee.
