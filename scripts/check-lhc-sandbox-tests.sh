#!/bin/sh
# Verify the reviewed sandbox gate list: exactly 60 unique binary/name pairs,
# and (when cargo nextest can list) that the selected set matches with no
# missing or extra tests.
set -eu
ROOT=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
LIST="$ROOT/scripts/lhc-sandbox-tests.tsv"
EXPECTED=60

if [ ! -f "$LIST" ]; then
  echo "missing $LIST" >&2
  exit 1
fi

data=$(awk -F '\t' '
  /^#/ || NF == 0 { next }
  NF != 2 {
    printf "malformed line %d: %s\n", NR, $0 > "/dev/stderr"
    bad=1
    next
  }
  {
    key=$1 "\t" $2
    if (seen[key]++) {
      printf "duplicate: %s\n", key > "/dev/stderr"
      bad=1
    }
    print key
  }
  END { if (bad) exit 1 }
' "$LIST")

count=$(printf '%s\n' "$data" | grep -c . || true)
if [ "$count" != "$EXPECTED" ]; then
  echo "expected $EXPECTED sandbox tests, found $count in $LIST" >&2
  exit 1
fi

header_count=$(awk '/^# count:/{print $3; exit}' "$LIST")
if [ "$header_count" != "$EXPECTED" ]; then
  echo "header count $header_count does not match $EXPECTED" >&2
  exit 1
fi

echo "ok sandbox-list: $EXPECTED reviewed tests"

if [ "${LHC_SANDBOX_LIST_ONLY:-}" = "1" ]; then
  exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found; list-only check passed" >&2
  exit 0
fi

filter=$(printf '%s\n' "$data" | awk -F '\t' '
  {
    gsub(/"/, "\\\"", $1)
    gsub(/"/, "\\\"", $2)
    term = "(binary(\"" $1 "\") & test(=\"" $2 "\"))"
    if (NR == 1) out = term
    else out = out " + " term
  }
  END { print out }
')

cd "$ROOT/codex-rs"
listed=$(
  cargo nextest list \
    -p codex-core \
    -p codex-app-server \
    -p codex-cli \
    --retries 0 \
    -E "$filter"
)

selected=$(printf '%s\n' "$listed" | awk '
  /^[^[:space:]].*:$/ { next }
  /^[[:space:]]+/ {
    sub(/^[[:space:]]+/, "")
    if ($0 != "") print
  }
' | sort)

expected_names=$(printf '%s\n' "$data" | awk -F '\t' '{print $2}' | sort)
selected_count=$(printf '%s\n' "$selected" | grep -c . || true)
if [ "$selected_count" != "$EXPECTED" ]; then
  echo "nextest selected $selected_count tests, expected $EXPECTED" >&2
  echo "--- expected ---" >&2
  printf '%s\n' "$expected_names" >&2
  echo "--- selected ---" >&2
  printf '%s\n' "$selected" >&2
  exit 1
fi

exp_tmp=$(mktemp)
got_tmp=$(mktemp)
trap 'rm -f "$exp_tmp" "$got_tmp"' EXIT
printf '%s\n' "$expected_names" >"$exp_tmp"
printf '%s\n' "$selected" >"$got_tmp"
missing=$(comm -23 "$exp_tmp" "$got_tmp" || true)
extra=$(comm -13 "$exp_tmp" "$got_tmp" || true)
if [ -n "$missing" ] || [ -n "$extra" ]; then
  echo "sandbox filter set mismatch" >&2
  [ -n "$missing" ] && printf 'missing:\n%s\n' "$missing" >&2
  [ -n "$extra" ] && printf 'extra:\n%s\n' "$extra" >&2
  exit 1
fi

echo "ok sandbox-filter: $EXPECTED nextest identities match"
