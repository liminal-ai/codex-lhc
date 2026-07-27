# LHC core-touchpoint patches

This directory holds the re-appliable patch series for codex-lhc core
touchpoints. It is **not** the repo-root `patches/` directory (that is
upstream's third-party Bazel/Windows patch collection).

Regenerate after any hook change (same commit as the hook):

```bash
# Every patch is a diff from the ONE base recorded in patches/lhc/BASE.
# Regenerating against whatever HEAD happens to be is what broke this series
# through Chunk 2 (FORK.md §History-reset R1/R2). `git add -N` first so
# untracked fork-owned files appear in the diff.

BASE=$(cat patches/lhc/BASE)
git add -N .
git diff "$BASE" -- codex-rs/Cargo.toml > patches/lhc/0001-workspace-member.patch
# ... one patch per group below; each fork-owned file in EXACTLY one patch.
```

Tripwire layer 4 applies the whole series at `BASE` and requires byte-identity
with the working tree, plus that every fork-owned file under `codex-rs/`
(outside `codex-rs/lhc/`, except the cargo-regenerated `Cargo.lock`) is covered.

See FORK.md's touchpoint inventory for the authoritative file list, and
FORK.md §History-reset recovery for the drill this series exists to serve.

## Series (regenerated 2026-07-26 against `BASE` = `61a44880a8`)

`BASE` advances on every upstream sync — see FORK.md "Sync drill" step 4.
Regenerate the *whole* series against the new base in the same commit as the
merge; a series left at the old base makes upstream's own changed files look
fork-owned-but-uncovered, and `patch-repro` fails.

One base for all seven; each fork-owned file in exactly one patch.

| Patch | Files |
|-------|-------|
| 0001-workspace-member | `codex-rs/Cargo.toml` |
| 0002-raw-item-contributor | `ext/extension-api/{contributors.rs, contributors/raw_item.rs, contributors/turn_lifecycle.rs, lib.rs, registry.rs}`, `ext/goal/tests/goal_extension_backend.rs` (turn timing fields on lifecycle inputs) |
| 0003-feature-flag | `features/src/lib.rs`, `core/config.schema.json` |
| 0004-session-raw-item-hook | `core/src/session/mod.rs`, `core/src/session/lhc_capture_e2e_tests.rs`, `core/src/stream_events_utils.rs`, `core/src/compact.rs` |
| 0005-app-server-dep | `app-server/Cargo.toml` |
| 0006-app-server-install | `app-server/src/extensions.rs` |
| 0007-lhc-compact-arm | `core/Cargo.toml`, `core/src/{compact_lhc.rs, compact_lhc_tests.rs, compact_lhc_slice_d_tests.rs, lhc_inference_bridge.rs, lib.rs, thread_manager.rs}`, `core/src/session/{turn.rs, session.rs, tests.rs, lhc_band_shape_eval_tests.rs}`, `core/src/state/{session.rs, service.rs}`, `core/src/tasks/{compact.rs, lifecycle.rs, mod.rs}`, `rollout/src/recorder.rs`, `thread-store/src/live_thread.rs`, `thread-store/src/local/{mod.rs,live_writer.rs}`, `app-server/src/request_processors/thread_processor.rs` (slice E reconcile) |

`core/Cargo.toml` and `core/src/compact.rs` each carry both their Chunk 1 and
Chunk 2b deltas, because one base means one patch per file. `core/Cargo.toml`
sits in 0007 (the runtime dep is the compact arm's); `compact.rs` sits in 0004.

`codex-rs/Cargo.lock` is deliberately in no patch — cargo regenerates it
(FORK.md inventory row 8), and layer 4 excludes it from the coverage check.

Chunk 2b: body from real `lhc.compact` + view map; NoReduction fail-open;
marker after write-back with derived digests on slot; timeout detaches.
Chunk 3 (N3): the turn's `CancellationToken` reaches the arm, so an aborted
turn stops derivation instead of running it out — `tasks/compact.rs` binds the
token and `run_auto_compact` gained one.

**Runtime cost note:** `codex-core` takes a runtime dep on `codex-lhc-host`
(not dev-only), so every core build compiles LHC + bundled SQLite regardless
of `Feature::LhcCapture`. No dependency cycle. Feature-gating the crate dep
would require a larger modularization; document rather than contort.

Rule zero (Chunk 1 fix round 3): storage invariants must round-trip through
`LhcSession`; host/core seams must exercise production registration paths.
