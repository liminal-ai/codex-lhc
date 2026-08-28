#!/usr/bin/env bash
# LHC strict-routing ignore gate (LIM-142).
#
# Codex-LHC routes every compact entry point through the strict LHC arm
# (`compact_lhc::run_strict_lhc_compact`). Upstream's native TokenBudget /
# remote / local compaction code is kept for upstream parity, but the upstream
# *integration* tests that assert native routing have a false premise in this
# fork. Those — and only those — carry
#
#     #[ignore = "codex-lhc LIM-142 strict-lhc-routing: <precise reason>"]
#
# This gate pins the exact reviewed set. A new ignore, a removed ignore, or a
# renamed test fails the gate, so the exclusion cannot expand silently.
#
# Route-independent native helper/unit tests stay ACTIVE and are not listed
# here; so do the fork's own strict-LHC tests.
set -u
cd "$(dirname "$0")/.."

MARKER='codex-lhc LIM-142 strict-lhc-routing:'
ALLOWLIST=scripts/lhc-native-routing-ignored-tests.txt

if [ ! -f "$ALLOWLIST" ]; then
  echo "TRIPWIRE compact-ignores: missing $ALLOWLIST"
  exit 1
fi

scan_files() {
  find codex-rs -name '*.rs' -type f \
    -not -path 'codex-rs/lhc/*' \
    -not -path 'codex-rs/target/*' \
    | sort
}

# Emit "<file>::<test fn>" for every marked ignore. The attribute may be
# followed by further attributes (#[tokio::test], #[cfg_attr(...)], doc
# comments) before the function item.
actual=$(scan_files | xargs -r awk -v marker="$MARKER" '
  index($0, marker) > 0 { pending = 1; next }
  pending && match($0, /fn[ \t]+[A-Za-z0-9_]+/) {
    name = substr($0, RSTART + 3, RLENGTH - 3)
    gsub(/[ \t]/, "", name)
    print FILENAME "::" name
    pending = 0
  }
' | sort)

# Every marked attribute must have produced an entry (no dangling marker).
marked=$(scan_files | xargs -r grep -F -c "$MARKER" 2>/dev/null \
  | awk -F: '{ total += $NF } END { print total + 0 }')
resolved=$(printf '%s\n' "$actual" | grep -c . || true)
if [ "$marked" -ne "$resolved" ]; then
  echo "TRIPWIRE compact-ignores: $marked marked attribute(s) but $resolved resolved to a test fn"
  echo "  (an #[ignore] carrying the marker is not directly above a test function)"
  exit 1
fi

# Reasons must be precise: the marker plus a non-trivial explanation.
short=$(scan_files | xargs -r grep -hF "$MARKER" \
  | sed "s/.*${MARKER}[[:space:]]*//; s/\"\][[:space:]]*$//" \
  | awk 'length($0) < 30 { print }')
if [ -n "$short" ]; then
  echo "TRIPWIRE compact-ignores: ignore reason(s) too short to be precise:"
  printf '%s\n' "$short" | sed 's/^/    /'
  exit 1
fi

expected=$(grep -v '^[[:space:]]*#' "$ALLOWLIST" | grep -v '^[[:space:]]*$' | sort)
declared_count=$(sed -n 's/^# count:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$ALLOWLIST" | head -1)
expected_count=$(printf '%s\n' "$expected" | grep -c . || true)

if [ -z "$declared_count" ]; then
  echo "TRIPWIRE compact-ignores: $ALLOWLIST has no '# count: N' header"
  exit 1
fi
if [ "$declared_count" -ne "$expected_count" ]; then
  echo "TRIPWIRE compact-ignores: allowlist header says $declared_count entries, file lists $expected_count"
  exit 1
fi

if ! diff_out=$(diff <(printf '%s\n' "$expected") <(printf '%s\n' "$actual")); then
  echo "TRIPWIRE compact-ignores: ignored native-routing set drifted from the reviewed allowlist"
  echo "  ('<' = allowlisted but no longer ignored, '>' = newly ignored and unreviewed)"
  printf '%s\n' "$diff_out" | sed 's/^/    /'
  echo "  Update $ALLOWLIST (and its '# count:' header) in the SAME commit,"
  echo "  and only for a test whose upstream native-routing premise is false here."
  exit 1
fi

echo "ok compact-ignores: $expected_count reviewed native-routing ignores, no drift"
exit 0
