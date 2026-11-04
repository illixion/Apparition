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
CARGO_AUDIT_VERSION=0.22.2

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
fail() { echo "AUDIT FAILED: $*" >&2; exit 1; }
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
FOLDER="$(cd "$FOLDER" && pwd -P)"

# ------------------------------------------------------------------ 1. the commit
echo "==> 1. which commit built these (GitHub's attestations)"
files=()
for f in "$FOLDER"/Apparition-*; do [[ -f "$f" ]] && files+=("$f"); done
[[ ${#files[@]} -gt 0 ]] || fail "no Apparition-* files in $FOLDER"
COMMIT=""
for f in "${files[@]}"; do
  attested="$(gh attestation verify "$f" -R "$REPO" --signer-workflow "$WORKFLOW" --format json 2>/dev/null \
    | python3 -c 'import json, sys
commits = {d["digest"]["gitCommit"] for a in json.load(sys.stdin)
           for d in a["verificationResult"]["statement"]["predicate"]["buildDefinition"]["resolvedDependencies"]
           if "gitCommit" in d.get("digest", {})}
print(commits.pop() if len(commits) == 1 else "")')" || attested=""
  [[ -n "$attested" ]] || fail "$(basename "$f") has no attestation from $WORKFLOW naming one commit"
  [[ -z "$COMMIT" || "$COMMIT" == "$attested" ]] || fail "$(basename "$f") was built from $attested, the others from $COMMIT"
  COMMIT="$attested"
done
[[ -z "$WANT_COMMIT" || "$(git -C "$ROOT" rev-parse "$WANT_COMMIT^{commit}" 2>/dev/null)" == "$COMMIT" ]] \
  || fail "the files were built from $COMMIT, not $WANT_COMMIT"
echo "    ${#files[@]} files, all from $COMMIT"
git -C "$ROOT" cat-file -e "$COMMIT^{commit}" 2>/dev/null || git -C "$ROOT" fetch -q origin "$COMMIT" \
  || fail "can't get commit $COMMIT"

WORK="$(mktemp -d /private/tmp/apparition-audit.XXXXXX)"
if [[ "$KEEP" == 1 ]]; then
  trap 'echo "    (kept $WORK)"' EXIT
else
  trap 'rm -rf "$WORK"' EXIT
fi
SRC="$WORK/src"
git clone -q --no-checkout "$ROOT" "$SRC"
git -C "$SRC" checkout -q --detach "$COMMIT"
[[ -f "$SRC/scripts/release-toolchain" ]] || fail "$COMMIT predates reproducible macOS builds (no scripts/release-toolchain)"
# shellcheck source=release-toolchain
. "$SRC/scripts/release-toolchain"
export RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN"

# ------------------------------------------------------------------ 2. the dependencies
echo "==> 2. dependencies at ${COMMIT:0:8}"
for manifest in "$SRC/Cargo.toml" "$SRC/core/Cargo.toml"; do
  cargo metadata -q --locked --format-version 1 --manifest-path "$manifest" >/dev/null \
    || fail "${manifest#"$SRC/"}'s lock file isn't current: a build would take versions it doesn't pin"
done
python3 - "$SRC/Cargo.lock" "$SRC/core/Cargo.lock" <<'EOF' || fail "a dependency comes from somewhere unpinned"
import re, sys
bad = []
for path in sys.argv[1:]:
    for source in re.findall(r'^source = "([^"]+)"$', open(path).read(), re.M):
        if source == "registry+https://github.com/rust-lang/crates.io-index":
            continue
        # A git dependency must name a full commit, and be locked to that same commit.
        m = re.fullmatch(r"git\+https://[^?#]+\?rev=([0-9a-f]{40})#([0-9a-f]{40})", source)
        if not (m and m.group(1) == m.group(2)):
            bad.append(f"{path.rsplit('/src/', 1)[-1]}: {source}")
for line in bad:
    print(f"    not pinned: {line}")
sys.exit(1 if bad else 0)
EOF
echo "    both lock files current; every crate from crates.io or a git commit"
audit_version="$(cargo audit --version 2>/dev/null || true)"
[[ "$audit_version" == *" $CARGO_AUDIT_VERSION" ]] \
  || fail "cargo-audit is '${audit_version:-missing}', not $CARGO_AUDIT_VERSION (cargo install cargo-audit --version $CARGO_AUDIT_VERSION --locked)"
ignores=()
while read -r id _; do
  [[ -z "$id" || "$id" == \#* ]] && continue
  ignores+=(--ignore "$id")
done < "$SRC/scripts/audit-allow"
for lock in "$SRC/Cargo.lock" "$SRC/core/Cargo.lock"; do
  if ! cargo audit -q --deny yanked ${ignores[@]+"${ignores[@]}"} --file "$lock" >"$WORK/audit.log" 2>&1; then
    sed 's/^/    /' "$WORK/audit.log"
    fail "${lock#"$SRC/"} has a yanked crate or an advisory scripts/audit-allow doesn't list"
  fi
done
echo "    no yanked crates, no advisories beyond the $(( ${#ignores[@]} / 2 )) in scripts/audit-allow"

# ------------------------------------------------------------------ 3. the macOS files
macos=()
for name in "Apparition-Installer-$VERSION-macos-universal.zip" "Apparition-VMGuest-$VERSION-macos-arm64.zip"; do
  [[ -f "$FOLDER/$name" ]] && macos+=("$name")
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
rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal --target aarch64-apple-darwin,x86_64-apple-darwin >/dev/null 2>&1
# The pinned Tauri CLI, in a folder of its own, ahead of any other on PATH.
CARGO_HOME="$WORK/tools" bash "$SRC/scripts/install-tauri-cli.sh" >/dev/null
export PATH="$WORK/tools/bin:$PATH"
"$SRC/scripts/package-release.sh" "$VERSION" --parts macos --into "$WORK/out" --clean >"$WORK/build.log" 2>&1 \
  || { tail -30 "$WORK/build.log" | sed 's/^/    /'; fail "the rebuild failed (the end of its log is above)"; }
echo "==> comparing with CI's"
mismatch=0
for name in "${macos[@]}"; do
  [[ -f "$WORK/out/$name" ]] || fail "the rebuild made no $name"
  python3 "$SRC/scripts/compare-release-zip.py" "$FOLDER/$name" "$WORK/out/$name" || mismatch=1
done
[[ "$mismatch" == 0 ]] || fail "CI's macOS files aren't what commit $COMMIT builds"
report "${macos[@]}"
echo "AUDIT PASSED: $VERSION, commit $COMMIT; ${macos[*]} reproduced"
