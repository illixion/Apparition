# An encrypted report, sealed by CryptoKit

For `tests/traces.rs`: proof that the Store's Rust HPKE opens what an app's DebugTrace (CryptoKit)
seals, not only what its own `reports::seal` makes.

- `report.zip`: a synthetic, unsigned trace (a public copy has no build key). Made by hand, not
  captured from an app or a device.
- `cryptokit.trace`: `report.zip` sealed with
  `DebugTrace/Tools/debugtrace-decrypt/debugtrace-decrypt encrypt report.zip <public key>`.
- `keys/a5d2ee9eadd96f4a.key`: the private half of the key it is sealed to, in the layout of
  `~/.local/state/debugtrace/report-keys`. **A throwaway made for this fixture.** It has never
  protected anything and must never be used as a real report key. Its public half is
  `8ul/S/w+sryPLhdtdU1iaTtjwLJdwslQlPuNPcnwklA=`.

To make them again: `DEBUGTRACE_REPORT_KEYS=<an empty folder> debugtrace-decrypt keygen`, then
`encrypt` as above, and update the key id in the test.
