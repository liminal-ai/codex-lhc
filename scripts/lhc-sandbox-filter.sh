#!/bin/sh
# Emit a nextest -E filter for the reviewed 60-name sandbox list.
set -eu
ROOT=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
LIST="$ROOT/scripts/lhc-sandbox-tests.tsv"
awk -F '\t' '
  /^#/ || NF == 0 { next }
  NF != 2 { next }
  {
    gsub(/"/, "\\\"", $1)
    gsub(/"/, "\\\"", $2)
    term = "(binary(\"" $1 "\") & test(=\"" $2 "\"))"
    if (NR == 1 || out == "") out = term
    else out = out " + " term
    n++
  }
  END {
    if (n != 60) {
      printf "expected 60 sandbox tests, found %d\n", n > "/dev/stderr"
      exit 1
    }
    print out
  }
' "$LIST"
