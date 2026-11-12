#!/usr/bin/env bash
# Runs ApparitionTests on a visionOS simulator: what a problem report (Settings > Report a
# Problem) can carry, checked with seeded values. See CONTRIBUTING.md, "What a problem report can carry".
#
#   scripts/test-report-privacy.sh                 # the simulator named "Apple Vision Pro"
#   scripts/test-report-privacy.sh <name or UDID>  # another one (xcrun simctl list devices)
#   SKIP_CORE=1 scripts/test-report-privacy.sh     # don't rebuild the core for the simulator first
#
# The tests are hosted by the app, so this builds the app (Debug) for the simulator. The whole
# xcodebuild output goes to build/test-report-privacy.log; the test results are printed.
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root" || exit 1

simulator="${1:-Apple Vision Pro}"
if [[ "$simulator" =~ ^[0-9A-Fa-f]{8}-([0-9A-Fa-f]{4}-){3}[0-9A-Fa-f]{12}$ ]]; then
  destination="platform=visionOS Simulator,id=$simulator"
else
  destination="platform=visionOS Simulator,name=$simulator"
fi

if [[ "${SKIP_CORE:-}" != "1" ]]; then
  scripts/build-core.sh visionos-sim || exit 1
fi
xcodegen generate --quiet || exit 1

log="$root/build/test-report-privacy.log"
# -collect-test-diagnostics never: without it a passing run can sit for ten minutes collecting
# simulator diagnostics the visionOS runtime can't produce.
xcodebuild test -project Apparition.xcodeproj -scheme Apparition \
  -destination "$destination" -only-testing:ApparitionTests \
  -collect-test-diagnostics never -derivedDataPath "$root/build/DerivedData-tests" > "$log" 2>&1
code=$?

# Swift Testing's own lines: each test, what each recorded issue says, and the totals.
/usr/bin/grep -E '(✔|✘) (Test|Suite)|^↳ | error: |\*\* TEST' "$log" \
  | /usr/bin/grep -vE '^↳ (Testing Library Version|Target Platform)' || true
if [[ $code -ne 0 ]]; then
  echo "failed (xcodebuild exit $code); the whole output is in $log" >&2
fi
exit $code
