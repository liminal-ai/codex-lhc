# LHC hook patch series

Each core touchpoint is maintained BOTH as a normal commit on `lhc` AND as
a re-appliable patch file here. Normal merge-based syncs never need these;
they are uniformity with grok-build-lhc (one maintainer drill across
forks) and insurance against upstream history surprises.

Regenerate after any hook change (same commit — FORK.md rule):
  git diff upstream/main..lhc -- <core files with LHC-HOOK lines> \
    > patches/NNNN-<name>.patch

Recovery drill: FORK.md "History-reset recovery".

No patches yet — Chunk 0 lands zero core touches. Patch 0001 will be the
root codex-rs/Cargo.toml workspace-members entry (Chunk 1).
