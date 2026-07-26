#!/usr/bin/env bash
# Verifier isolation (onboarding §Verifier isolation — MANDATORY).
# Copies the fork tree to a per-lane scratch dir so no two verifiers (or a
# verifier and the implementor) ever share a working tree. Prints the
# isolated dir; launch your verifier with that as cwd.
#   usage: scripts/verify-isolated.sh <lane-label>   e.g. r16-sol
#
# Two rules learned the hard way (Phase 4 Chunk 2):
#
#  1. **.git is COPIED, not excluded.** Without it the verifier cannot run the
#     tripwire's vendor-pin layer (`git status` on the submodule) or the
#     clean-checkout patch drill — both silently degrade to "cannot run here",
#     which reads like a pass. Costs ~845M per lane; disk is cheaper than a
#     blind gate. `target/` stays excluded (76G, and a cold rebuild is fine).
#
#  2. **Do NOT delete a lane's tree between rounds.** claude-subagent sessions
#     are keyed to their working directory, so removing the tree makes
#     `--resume` fail with "No conversation found" — which breaks the
#     verifier-session-continuity rule (onboarding §Verifier session
#     continuity). One stable tree per lane, for the life of the chunk.
#     Isolation and continuity are both satisfied: lanes never share a tree,
#     and each lane's session survives. Delete only once the chunk is accepted.
set -eu
lane="${1:?usage: verify-isolated.sh <lane-label>}"
src="$(cd "$(dirname "$0")/.." && pwd)"
dest="$(dirname "$src")/$(basename "$src")-verif-$lane"
rsync -a --delete --exclude="target/" "$src/" "$dest/"
{
  echo "isolated verifier tree"
  echo "lane:    $lane"
  echo "source:  $src"
  echo "git:     $(git -C "$src" rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "dirty:   $(git -C "$src" status --short 2>/dev/null | wc -l) modified paths"
} > "$dest/ISOLATED-TREE.txt"
echo "$dest"
