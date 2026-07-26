# Chunk 2 (Phase 4, unit 21 of 22) — round log and lane sessions

Kept per the onboarding doc's §"Verifier session continuity — MANDATORY":
track the two lane session ids next to the round log, and resume them for
every re-verification within this chunk.

## Lane sessions — RESUME these, do not start fresh

| Lane | CLI | First run | session_id |
|------|-----|-----------|------------|
| Sol | `codex-subagent` | `20260725-204224-0c2b27` | `019f9b03-c9a6-7d82-9aa8-90afe6fd33ed` |
| Opus | `claude-subagent` | `20260725-213601-db5a0b` | `fa13a94c-4230-4d0f-8f96-046eb48e4930` |
| Implementor | `grok-subagent` | continuous since Chunk 1 | `019f9a59-7c56-7d83-909a-5fc5afeb0f43` |

Note: the ids listed in the onboarding doc's continuity section
(`20260725-201551-e785a1` / `20260725-202050-b4143c`) belong to **Phase 3's**
grok-build Chunk 2, a different orchestration. Phase 4 uses the table above.

Opus's original session (`17906899-…`) was destroyed when its tree was
deleted; the id above is its replacement, seeded with the prior findings list.

Isolation is orthogonal and still applies: resume the session, isolate the
filesystem. Each lane keeps ONE **stable** tree for the whole chunk —
refreshed in place by `scripts/verify-isolated.sh`, never deleted between
rounds (deleting it destroys the resumable session). Sol takes the canonical
tree because `codex-subagent` has failed in rsync copies (onboarding
§launch quirk).

## Rounds

| # | Round | Lanes | Outcome |
|---|-------|-------|---------|
| 1 | 2a — census + fail-open inventory + band-shape harness | none (orchestrator pass only) | Accepted, committed `b0319a5fe8`. Analysis + harness + one native test; no production behaviour change, so dual verify folded into round 2. |
| 2 | 2b — compact bridge, first build | Sol + Opus, **fresh** (first full verification of new scope — correct per policy), isolated trees | **Rejected.** 12 CONFIRMED blockers, both lanes converging: served body built by a host heuristic (`render_served_body`) rather than LHC's `compact()`, with the receipt stapled on; `ModelClient` bridge absent; resume/fork history never imported into the archive; law 1 and law 2 tests vacuous under mutation; neither ladder driven by any test; patch 0007 omits `compact_lhc.rs`. |
| 3 | 2b — REDO around `lhc.compact()` → `LlmRequestContext` → `replace_compacted_history` | (in flight) | — |
| 4 | post-redo confirmation | Sol resumed; Opus fresh+seeded (session lost, see traps) | **Not sound.** Both lanes converged: served body re-ingested into the archive as source events (incl. LHC's own `[context · smooth]` summary and the marker itself); law 2 vacuous (pass-through mutation left all 8 green); zero-reduction compact reported `Installed` and shadowed the native ladder; marker key over-collapsed distinct compacts; timeout did not bound the caller. Mechanism accepted — body verified genuinely LHC-produced at 300-event scale (5x reduction, real bands). |
| 5 | fix round 1 (F1-F6) | orchestrator mutation pass | All six landed. Orchestrator re-verified independently: F1 (disable derived-digest exclusion -> source events 8->16, test fails correctly) and F2 (pass-through mutation -> law 2 AND law 1 both fail; previously left all 8 green). Baseline restored green: core 9, host 10, tripwire ALL GREEN 35/35. |
| 6 | confirmation round 2 | Sol + Opus, **both resumed**, separate trees, `.git` present | in flight |

## Orchestrator stopping rule for this chunk — stated BEFORE dispatching round 4

Per the onboarding corollary ("state what would make you stop"; "blocking
means the product is wrong, not that a table row is mislabeled"):

**I accept Chunk 2 when all of the following hold:**

1. The served body is produced by LHC's own compaction
   (`lhc.compact()` → LHC's typed view), not by host-side summarisation.
2. Law 1 holds as **equality** between installed host items and the mapped
   LHC body, on a body containing more than plain text, proven by mutation.
3. Law 2 holds as the **next-turn** property (compact once → count drops →
   no re-trigger), measured through the production token-status path,
   proven by mutation.
4. Both compaction ladders (manual and auto) are entered by a test that
   fails when its hook is removed.
5. Resume/fork either import inherited history into the archive or fail open
   to the native ladder — never compact a partial archive into a full
   replacement.
6. The inference gate fails **closed** (live-without-config errors rather
   than returning deterministic text).
7. Fail-open paths are bounded against the context window, not by item count.
8. Tripwire green on a clean rebuild; patches apply to a clean checkout;
   sentinels / inventory / patches in lockstep.

**I stop and accept — carrying residuals as named FORK.md checkpoints —
when:** both lanes agree the component is functionally sound and the round's
findings are documentation, naming, test-metadata, or scope/size
observations. Those are not blocking. Explicitly *not* grounds to keep
looping: a stale FORK.md line, a mislabelled inventory row, module length,
or a warning.

**Known residuals already destined for checkpoints, not rounds:** live
band-shape eval and live `ModelClient` firing (both blocked on Lee's auth
lane); `MODULE.bazel.lock` refresh (no bazel on this host); module split
before upstream PR candidacy; ModelOutput-vs-HostContext tag fidelity
(unobservable by behaviour today).

## Resume mechanics — three traps, each cost a failed launch (2026-07-25)

Recording these because none is obvious and all three produce a run that
*looks* fine.

1. **`codex exec resume` rejects `--sandbox`.** Usage is
   `codex exec resume --json --model <M> --config <k=v> <SESSION_ID> [PROMPT]`.
   Pass the sandbox as `-c sandbox_mode=danger-full-access` instead. Dropping
   it entirely falls back to bubblewrap, which is broken on this box
   (`bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted`).
2. **`running:false` + envelope ≠ success — and neither does `status: ok`.**
   The bubblewrap failure above returned `status: "ok"`, `exit_code: 0`, with
   a result body explaining it could not execute a single command and no
   verdict was possible. Read the `result` text, not just the status field.
   A naive watcher records "Sol confirms" from a run that inspected nothing.
3. **Verifier isolation and verifier continuity conflict.** `claude-subagent`
   sessions are keyed to their working directory, so a fresh isolated tree
   per round makes `--resume` fail with "No conversation found with session
   ID". Deleting a lane's tree between rounds destroys its session.

   **Resolution: a stable per-lane tree for the chunk's duration.** Lanes
   still never share a tree (isolation holds); each lane's session survives
   to be resumed (continuity holds). Do not delete a lane tree until the
   chunk is accepted.

   Phase 4 Chunk 2 lane trees:
   - Sol → `/srv/work/codex` (canonical; `codex-subagent` fails in rsync
     copies per onboarding §launch quirk)
   - Opus → `/srv/work/codex-verif-c2r2-opus` (stable, do not delete)

## Round 4 — post-redo confirmation

- **Sol:** resumed session `019f9b03-…` (run `20260725-213816-a203f7`).
- **Opus:** session lost to trap 3 above (its round-2 tree was deleted).
  Per the onboarding policy, restarted **fresh** seeded with its 19 prior
  finding *titles* (not report prose) — brief
  `phase4-confirm-chunk2-opus-fresh.md`. Break noted here as required.
  New session runs in the stable tree above and is resumable from now on.

## Orchestrator-run mutations (round 5) — do not re-derive

Run on the canonical tree, restored and re-verified green afterwards:

| Mutation | Result |
|----------|--------|
| `if false && derived.contains(&content_d)` (disable F1 derived exclusion) | `three_compacts_do_not_reingest_body` FAILED: "round 1: source events must stay at 8, got 16 (total 18)" |
| `new_history = host_items.clone()` (Opus's pass-through) | `law2_token_count_drops_and_threshold_does_not_retrigger` FAILED **and** `law1_installed_items_equal_lhc_body_structurally` FAILED |

## Open probe carried into round 6

Under the pass-through mutation, `production_manual_ladder_invokes_lhc_arm`
**overflowed its stack**. Mutation-only, so not a baseline failure — but F3
made fail-open-to-native a common production path (every sub-threshold
compact now takes it). If that path can recurse when entered from inside the
LHC arm it is a production hang. Both lanes were given this as a targeted
probe.

## Isolation script fix (round 5)

`scripts/verify-isolated.sh` previously excluded `.git`, which silently
disabled the tripwire's vendor-pin layer and the clean-checkout patch drill
inside verifier trees — both degrading to "cannot run here", which reads like
a pass. `.git` is now copied (~845M/lane). The script header also records
that lane trees must NOT be deleted between rounds, since that is what
destroyed Opus's resumable session.

## Rounds 7–8 (derivation actually running)

| # | Round | Outcome |
|---|-------|---------|
| 7 | L1/L2/L3 | **The chunk's worst defect.** Both lanes independently found that LHC derivation was *never triggered*: the adapter called `drain_settled` (waits for idle) and never `work.drain` (runs work). Every band was a degraded excerpt and the whole inference path was dead code — so J1/J2's "real inference" was plumbed but never executed. Fixed: `work.drain` before `compact()`; `DerivationFailed` fail-open at four sites; current-body provenance pinned against cap eviction. Implementor lane died mid-report (grok billing exhausted); work had landed and was verified by the orchestrator. |
| 8 | M1/M2 | Round 7 exposed that *all* derivation was deferred to compact time: ~3 model calls/turn, strictly sequential (`max_inflight=1`), so 100 turns = 297 serialized calls vs a 120s timeout. Root cause: the brief specified "background drain pumped from `on_thread_idle`" and it was never implemented. Fixed: bounded idle pump (8 items/tick, single-flight, panic-guarded, off after thread stop) + batched compact-time drain with time budget, batch cap, and cancellation checked between batches. |

## Orchestrator mutations (rounds 7–8) — all restored, baseline re-verified

| Mutation | Result |
|----------|--------|
| `work.drain(max_items: Some(0))` (drain performs no work) | `l1_derivation_runs_callbacks_and_bands_are_not_degraded` FAILED, plus `produce_uses_lhc_compact_receipt_not_heuristic` |
| `spawn_idle_derivation_pump` disabled | `m1_idle_tick_derives_in_background_and_shrinks_backlog` and `m1_no_pump_after_thread_stop` both FAILED |
| `check_cancel` removed from drain loop | `m2_compact_timeout_cancels_in_flight_derivation` FAILED |

## Named gaps carried into Chunk 3 (not concealed)

1. **M1 core-level end-to-end measurement.** The idle pump is proven in the
   host crate through the real extension registry (`install.rs::tests::m1_*`,
   mutation-verified above). A core-level test measuring compact-time call
   reduction was attempted, failed for reasons the implementor could not
   explain, and was **deleted** rather than left passing-but-meaningless. The
   discrepancy (`remaining` growing under the core pump, falling under the
   host pump) is unexplained. Recorded in-source at
   `compact_lhc_tests.rs:1288`. Chunk 3 should settle it.
2. **Real per-call latency is unmeasured.** The timeout arithmetic
   (297 calls x 300ms–1s) is arithmetic on assumed latency, not measurement.
   Chunk 3's live cert is where this gets a real number — it determines
   whether the 120s bound and the idle pump are sufficient in practice.
3. **Partial derivation-failure demotion is silent.** If one call *kind*
   fails (e.g. 429 on `compress_detailed_turn`), LHC demotes those turns
   `detailed -> brief` and reports nothing degraded. Content is preserved and
   the body still reduces, so no degraded body is installed — but the arm
   cannot see that a demotion happened.
4. **Full-suite ordering artefact (NOT ours).**
   `session::turn::tests::post_sampling_token_estimate_is_disabled_by_always_on_sinks`
   fails in a full `cargo test -p codex-core --lib` run and passes both in
   isolation and within its own module. The only global-subscriber installer
   is upstream's `session/tests.rs` (`traced_test`); no LHC test installs one.
   Pre-existing cross-module tracing interference.
