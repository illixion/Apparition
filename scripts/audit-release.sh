#!/usr/bin/env bash
#
# Audits an Apparition release before its manifest is signed. scripts/publish.py add (in the release
# site's repo) runs it and signs nothing unless it passes, so a release signature says more than
# "GitHub built these":
#
#   scripts/audit-release.sh <version> <folder with the release files> [--commit SHA] [--deps-only] [--keep]
#                            [--report FILE]
#
# --report writes what passed as JSON (the commit, the files rebuilt, the advisories allowed), which
# publish.py lists with the release.
# A failure retains the audit evidence under build/release-audits. --keep retains successful runs too.
#
# 1. The commit. GitHub's attestation of every file in the folder (gh attestation verify, against
#    this repo and its release workflow) names the commit it was built from; they must all name
#    the same one, and --commit, when given, must be it.
# 2. The dependencies, as of that commit:
#    - both lock files (Cargo.lock, core/Cargo.lock) are current, so a build takes exactly the
#      versions and checksums they pin (cargo --locked);
#    - every crate comes from crates.io, or from a git repository at a full commit hash;
#    - no crate is yanked, and none has a RustSec advisory beyond those scripts/audit-allow lists
#      with a reason (cargo-audit at the version below, with the advisory database fetched now).
# 3. The macOS files (the macos-universal installer and the anisette VM's programs). Rebuilt here
#    from that commit, from nothing (no compiled dependency kept from before), with the toolchain in
#    scripts/release-toolchain, and compared with CI's (scripts/compare-release-zip.py). They must
#    match. The other platforms' files are only attested. --deps-only skips this.
#
# Needs: gh (signed in), cargo-audit $CARGO_AUDIT_VERSION, rustup, and for the rebuild the Xcode
# whose build scripts/release-toolchain names, installed anywhere as /Applications/Xcode*.app. A
# rebuild takes about 10 minutes and uses $FIXED (scripts/package-release.sh), so don't run two.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
REPO=illixion/Apparition
WORKFLOW="$REPO/.github/workflows/release.yml"

VERSION="${1:?usage: audit-release.sh <version> <folder> [--commit SHA] [--deps-only] [--keep]}"
FOLDER="${2:?usage: audit-release.sh <version> <folder> [--commit SHA] [--deps-only] [--keep]}"
shift 2
WANT_COMMIT=""
DEPS_ONLY=0
KEEP=0
REPORT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --commit) WANT_COMMIT="${2:?--commit needs a commit}"; shift ;;
    --deps-only) DEPS_ONLY=1 ;;
    --keep) KEEP=1 ;;
    --report) REPORT="${2:?--report needs a file}"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
AUDITS="$ROOT/build/release-audits"
mkdir -p "$AUDITS"
WORK="$(mktemp -d "$AUDITS/audit.XXXXXX")"
cleanup() {
  local status=$?
  if [[ "$status" != 0 || "$KEEP" == 1 ]]; then
    echo "    Audit evidence: $WORK" >&2
  elif ! python3 -c 'import shutil, sys; shutil.rmtree(sys.argv[1])' "$WORK"; then
    echo "    Audit evidence: $WORK" >&2
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
fail() { echo "AUDIT FAILED: $*" | tee -a "$WORK/failure.log" >&2; exit 1; }
mkdir "$WORK/attestations" "$WORK/ci"
# What a passed audit found, for --report: the commit, the files it rebuilt (the arguments), the
# toolchain, and the advisories it allowed.
report() {
  [[ -n "$REPORT" ]] || return 0
  python3 -c 'import json, sys
out, commit, audit, rust, xcode, allow, *rebuilt = sys.argv[1:]
allowed = [line.split()[0] for line in open(allow) if line.strip() and not line.startswith("#")]
json.dump({"commit": commit, "reproduced": rebuilt,
           "toolchain": {"rust": rust, "xcode": xcode} if rebuilt else None,
           "cargoAudit": audit, "advisoriesAllowed": allowed}, open(out, "w"), indent=2)' \
    "$REPORT" "$COMMIT" "$CARGO_AUDIT_VERSION" "$RUST_TOOLCHAIN" "$XCODE_BUILD" "$SRC/scripts/audit-allow" "$@"
}
FOLDER="$(cd "$FOLDER" && pwd -P)" || fail "can't open the release folder"
printf 'Version: %s\nRelease folder: %s\nStarted: %s\n' "$VERSION" "$FOLDER" "$(date -u '+%FT%TZ')" >"$WORK/context.txt"

# ------------------------------------------------------------------ 1. the commit
echo "==> 1. which commit built these (GitHub's attestations)"
files=()
for f in "$FOLDER"/Apparition-*; do [[ -f "$f" ]] && files+=("$f"); done
[[ ${#files[@]} -gt 0 ]] || fail "no Apparition-* files in $FOLDER"
COMMIT=""
for f in "${files[@]}"; do
  name="${f##*/}"
  # Compare the same macOS bytes that we attest, even if the draft assets are replaced later.
  if [[ "$DEPS_ONLY" == 0 ]]; then
    case "$name" in
      "Apparition-Installer-$VERSION-macos-universal.zip"|"Apparition-VMGuest-$VERSION-macos-arm64.zip")
        cp -p "$f" "$WORK/ci/$name"
        f="$WORK/ci/$name"
        ;;
    esac
  fi
  evidence="$WORK/attestations/$name"
  if ! gh attestation verify "$f" -R "$REPO" --signer-workflow "$WORKFLOW" --format json \
    >"$evidence.json" 2>"$evidence.log"; then
    fail "$name failed attestation verification"
  fi
  attested="$(python3 -c 'import json, sys
commits = {d["digest"]["gitCommit"] for a in json.load(sys.stdin)
           for d in a["verificationResult"]["statement"]["predicate"]["buildDefinition"]["resolvedDependencies"]
           if "gitCommit" in d.get("digest", {})}
print(commits.pop() if len(commits) == 1 else "")' <"$evidence.json" 2>>"$evidence.log")" || attested=""
  [[ -n "$attested" ]] || fail "$(basename "$f") has no attestation from $WORKFLOW naming one commit"
  [[ -z "$COMMIT" || "$COMMIT" == "$attested" ]] || fail "$(basename "$f") was built from $attested, the others from $COMMIT"
  COMMIT="$attested"
done
[[ -z "$WANT_COMMIT" || "$(git -C "$ROOT" rev-parse "$WANT_COMMIT^{commit}" 2>/dev/null)" == "$COMMIT" ]] \
  || fail "the files were built from $COMMIT, not $WANT_COMMIT"
echo "    ${#files[@]} files, all from $COMMIT"
printf 'Attested commit: %s\n' "$COMMIT" >>"$WORK/context.txt"
git -C "$ROOT" cat-file -e "$COMMIT^{commit}" 2>/dev/null || git -C "$ROOT" fetch -q origin "$COMMIT" \
  || fail "can't get commit $COMMIT"

SRC="$WORK/src"
git clone -q --no-checkout "$ROOT" "$SRC" >"$WORK/source.log" 2>&1 \
  && git -C "$SRC" checkout -q --detach "$COMMIT" >>"$WORK/source.log" 2>&1 \
  || fail "couldn't check out $COMMIT (see source.log)"
[[ -f "$SRC/scripts/release-toolchain" ]] || fail "$COMMIT predates reproducible macOS builds (no scripts/release-toolchain)"
# shellcheck source=release-toolchain
. "$SRC/scripts/release-toolchain"
export RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN"

# ------------------------------------------------------------------ 2. the dependencies
echo "==> 2. dependencies at ${COMMIT:0:8}"
if ! "$SRC/scripts/audit-dependencies.sh" "$SRC" >"$WORK/audit-dependencies.log" 2>&1; then
  sed 's/^/    /' "$WORK/audit-dependencies.log"
  fail "the Rust dependency audit failed"
fi
sed 's/^/    /' "$WORK/audit-dependencies.log"

# ------------------------------------------------------------------ 3. the macOS files
macos=()
for name in "Apparition-Installer-$VERSION-macos-universal.zip" "Apparition-VMGuest-$VERSION-macos-arm64.zip"; do
  [[ -f "$WORK/ci/$name" ]] && macos+=("$name")
done
if [[ "$DEPS_ONLY" == 1 || ${#macos[@]} -eq 0 ]]; then
  if [[ "$DEPS_ONLY" == 1 ]]; then echo "==> 3. skipped (--deps-only)"; else echo "==> 3. no macOS files to rebuild"; fi
  report
  echo "AUDIT PASSED: $VERSION, commit $COMMIT"
  exit 0
fi
echo "==> 3. rebuilding the macOS files from nothing (Rust $RUST_TOOLCHAIN, Xcode $XCODE_BUILD)"
DEVELOPER_DIR=""
for app in /Applications/Xcode*.app; do
  [[ -d "$app/Contents/Developer" ]] || continue
  build="$(DEVELOPER_DIR="$app/Contents/Developer" xcodebuild -version 2>/dev/null | sed -n 's/^Build version //p')"
  if [[ "$build" == "$XCODE_BUILD" ]]; then DEVELOPER_DIR="$app/Contents/Developer"; break; fi
done
[[ -n "$DEVELOPER_DIR" ]] || fail "no /Applications/Xcode*.app is build $XCODE_BUILD, which CI built with (scripts/release-toolchain)"
export DEVELOPER_DIR
{
  sw_vers
  xcodebuild -version
  xcrun --sdk macosx --show-sdk-path
  xcrun --sdk macosx clang --version
} >"$WORK/toolchain.log" 2>&1
rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal --target aarch64-apple-darwin,x86_64-apple-darwin >>"$WORK/toolchain.log" 2>&1 \
  || fail "Rust toolchain setup failed (see toolchain.log)"
# The pinned Tauri CLI, in a folder of its own, ahead of any other on PATH.
CARGO_HOME="$WORK/tools" bash "$SRC/scripts/install-tauri-cli.sh" >>"$WORK/toolchain.log" 2>&1 \
  || fail "Tauri CLI setup failed (see toolchain.log)"
export PATH="$WORK/tools/bin:$PATH"
"$SRC/scripts/package-release.sh" "$VERSION" --parts macos --into "$WORK/out" --clean >"$WORK/build.log" 2>&1 \
  || { tail -30 "$WORK/build.log" | sed 's/^/    /'; fail "the rebuild failed (the end of its log is above)"; }
echo "==> comparing with CI's"
mismatch=0
for name in "${macos[@]}"; do
  [[ -f "$WORK/out/$name" ]] || fail "the rebuild made no $name"
  python3 "$ROOT/scripts/compare-release-zip.py" "$WORK/ci/$name" "$WORK/out/$name" 2>&1 \
    | tee -a "$WORK/comparison.log" || mismatch=1
done
[[ "$mismatch" == 0 ]] || fail "CI's macOS files differ from the rebuild of $COMMIT"
report "${macos[@]}"
echo "AUDIT PASSED: $VERSION, commit $COMMIT; ${macos[*]} reproduced"
