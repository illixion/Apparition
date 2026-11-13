# Releases and how they are verified

Releases are listed on [apparition.illixion.com](https://apparition.illixion.com) (repo
`apparition.illixion.com`, whose README has the steps). GitHub Actions builds each release's
files (`.github/workflows/release.yml`, running `scripts/package-release.sh` on macOS for the Apple
parts, and natively on Linux and Windows runners, x64 and ARM64 each, the Windows ones with MSVC) and attests their provenance: `gh attestation verify <file> -R
illixion/Apparition` checks a download came from this repository's workflow. The site then
signs the files' SHA256SUMS with a key in `release-signers`, which never goes near CI, and only
after `scripts/audit-release.sh` passes: it takes the commit from the files' attestations, checks
that commit's dependencies (lock files current, every crate from crates.io or a pinned git
commit, none yanked, no RustSec advisory beyond `scripts/audit-allow`), and rebuilds the macOS
files from nothing with the toolchain in `scripts/release-toolchain`, which must give CI's
bytes. The Linux and Windows files are attested and listed in the signed SHA256SUMS but not rebuilt by that audit,
and the Windows setup and app carry no Authenticode signature, so SmartScreen warns on first run.
A part whose runner fails or never comes holds up only itself: the others finish, and
"Re-run failed jobs" builds just that one, while every build job also caches its finished files
for that commit and version, so a new run on the same ref takes them instead of building again.
`core/src/release.rs` checks that signature against the keys compiled in,
then every download's hash, so the files can be hosted anywhere. `apparition install`
with no IPA installs the latest release this way.
