#!/usr/bin/env bash
# Verifier isolation (onboarding §Verifier isolation — MANDATORY).
# Copies the fork tree to a per-lane scratch dir so no two verifiers (or a
# verifier and the implementor) ever share a working tree. Prints the
# isolated dir; launch your verifier with that as cwd.
#   usage: scripts/verify-isolated.sh <lane-label>   e.g. r16-sol
set -eu
lane="${1:?usage: verify-isolated.sh <lane-label>}"
src="$(cd "$(dirname "$0")/.." && pwd)"
dest="$(dirname "$src")/$(basename "$src")-verif-$lane"
rsync -a --delete --exclude=".git/" --exclude="target/" "$src/" "$dest/"
echo "$dest"
