# LHC hook patch series

Each core touchpoint is maintained BOTH as a normal commit on `main` (the
product branch) AND as a re-appliable patch under `patches/lhc/`. Normal
merge-based syncs never need these; they are uniformity with grok-build-lhc
(one maintainer drill across forks) and insurance against upstream history
surprises. The rest of this directory is upstream's third-party
Bazel/Windows patch collection.

Regenerate after any hook change (same commit — FORK.md rule) from the ONE
recorded upstream base, never from a branch or the moving upstream tip:
  sh patches/lhc/regenerate.sh      # diffs patches/lhc/BASE → working tree
See `patches/lhc/README.md` for the series, ownership groups, and when BASE
advances (FORK.md "Sync drill" step 4).

Recovery drill: FORK.md "History-reset recovery".
