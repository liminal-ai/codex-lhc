#!/bin/sh
# Emit a nextest -E filter for the reviewed 60-name sandbox list.
set -eu
ROOT=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
LIST="$ROOT/scripts/lhc-sandbox-tests.tsv"
awk -F '\t' '
  /^#/ || NF == 0 { next }
  NF != 2 { next }
  {
    # nextest binary() matches binary-name only and rejects package::binary
    # IDs. binary_id(=) / test(=) are exact and must stay unquoted: quotes are
    # part of the matcher text and would miss every identity.
    if ($1 ~ /[()&|+" \t]/ || $2 ~ /[()&|+" \t]/) {
      printf "filter-unsafe identity: %s\t%s\n", $1, $2 > "/dev/stderr"
      bad=1
      next
    }
    term = "(binary_id(=" $1 ") & test(=" $2 "))"
    if (NR == 1 || out == "") out = term
    else out = out " + " term
    n++
  }
  END {
    if (bad) exit 1
    if (n != 60) {
      printf "expected 60 sandbox tests, found %d\n", n > "/dev/stderr"
      exit 1
    }
    print out
  }
' "$LIST"
