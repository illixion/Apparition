#!/usr/bin/env bash
# Check the locked Rust dependencies before building or blessing a release.
# Keep this shared by the release workflow's preflight and audit-release.sh.

set -euo pipefail

ROOT="$(cd "${1:?usage: audit-dependencies.sh <repository-root>}" && pwd -P)"
CARGO_AUDIT_VERSION=0.22.2

for manifest in Cargo.toml core/Cargo.toml; do
  cargo metadata -q --locked --format-version 1 --manifest-path "$ROOT/$manifest" >/dev/null \
    || { echo "AUDIT FAILED: cargo metadata failed for $manifest with --locked" >&2; exit 1; }
done

if ! python3 - "$ROOT/Cargo.lock" "$ROOT/core/Cargo.lock" <<'PY'
import re, sys

bad = []
for path in sys.argv[1:]:
    for source in re.findall(r'^source = "([^"]+)"$', open(path).read(), re.M):
        if source == "registry+https://github.com/rust-lang/crates.io-index":
            continue
        match = re.fullmatch(r"git\+https://[^?#]+\?rev=([0-9a-f]{40})#([0-9a-f]{40})", source)
        if not (match and match.group(1) == match.group(2)):
            bad.append(f"{path.rsplit('/src/', 1)[-1]}: {source}")
for line in bad:
    print(f"    not pinned: {line}")
sys.exit(1 if bad else 0)
PY
then
  echo "AUDIT FAILED: a dependency comes from somewhere unpinned" >&2
  exit 1
fi
echo "    both lock files current; every crate from crates.io or a pinned git commit"

audit_version="$(cargo audit --version 2>/dev/null || true)"
[[ "$audit_version" == *" $CARGO_AUDIT_VERSION" ]] || {
  echo "AUDIT FAILED: cargo-audit is '${audit_version:-missing}', not $CARGO_AUDIT_VERSION" >&2
  exit 1
}

ignores=()
while read -r id _; do
  [[ -z "$id" || "$id" == \#* ]] && continue
  ignores+=(--ignore "$id")
done < "$ROOT/scripts/audit-allow"

for lock in Cargo.lock core/Cargo.lock; do
  echo "==> cargo audit $lock"
  cargo audit -q --deny yanked ${ignores[@]+"${ignores[@]}"} --file "$ROOT/$lock"
done
echo "AUDIT PASSED: locked dependencies; $(( ${#ignores[@]} / 2 )) allowed RustSec advisories"
