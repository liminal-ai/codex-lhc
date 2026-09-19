#!/bin/sh
# Emit a nextest -E filter for the comparison-8 residual list.
# Not the candidate sandbox-60 gate.
set -eu
ROOT=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
LIST="$ROOT/scripts/lhc-sandbox-comparison-8.tsv"
awk -F '\t' '
  /^#/ || NF == 0 { next }
  NF != 2 { next }
  {
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
    if (n != 8) {
      printf "expected 8 comparison tests, found %d\n", n > "/dev/stderr"
      exit 1
    }
    print out
  }
' "$LIST"
