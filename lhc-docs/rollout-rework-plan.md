# Rollout rework — design and execution plan

Successor to [`rollout-design-notes.md`](rollout-design-notes.md) (the
problem space and discussion log — read it first; nothing there is
repeated here). This document is the settled design plus the execution
and test plan. Claims below marked "verified" were re-checked against
code by the reviewer on 2026-07-26, upstream base `61a44880a8`.

Foundation reading for anyone implementing: the LHC operating model
(`docs/onboard/01-core-concepts.md`, `02-domain-design.md` in the
long-horizon-context repo), `FORK.md` (laws, touchpoints, tripwires),
and the design-notes doc above.

## Design

**Principle (from the notes, unchanged):** LHC's SQLite is the source
of truth; the rollout is a derived, disposable, regenerable projection.
The conformance test is: delete the rollout, regenerate it from the
thread, resume, get an identical session.

**On compact, rewrite instead of append.** The new file holds: session
meta → the materialized thread view as the model stream → exactly one
boundary `Compacted` record → one full world-state snapshot → items and
events for turns after the boundary. Same session id, same path;
write to temp, fsync, rename over, reopen the recorder handle
(verified: append-mode fd, no locking — an unreopened handle would
silently follow the old inode), retain one prior generation.

**The boundary record** carries `replacement_history` AND
`window_number` — both, because the Paginated reader's stop condition
requires both or it degrades to scan-to-start (verified,
`rollout/src/model_context.rs` `observe`). `window_number` and the
`first_window_id`/`previous_window_id` chain stay monotonic across
rewrites, sourced from the same `CompactedHistoryMetadata` the current
arm already receives (verified, `replace_compacted_history`).

**World state:** the current arm already persists a full snapshot
immediately after the replacement history (verified). The rewrite emits
that same snapshot after the boundary record; reconstruction resets its
baseline at a `Compacted` and requires the next full snapshot
(verified, `rollout_reconstruction.rs` world-state replay).

**Display stream of the rebuilt file** (terminal Codex = Legacy mode,
verified: `history_mode` is set only by app-server request params;
TUI/CLI/exec occurrences are test fixtures; `Default` = `Legacy`):

| Event class | Source in rebuilt file |
|---|---|
| `TurnStarted` / `TurnComplete` / `TurnAborted` | **Regenerated from LHC** `turns` v5 fields: outcome, outcome_reason, started_at/ended_at |
| `TokenCount` | **Regenerated from LHC** `message.provider_usage` (per-call, verbatim provider counts) |
| `UserMessage` / `AgentMessage` / `AgentReasoning`(+raw) | **Regenerated from the model stream** (Paginated drops these as re-derivable — precedent verified in `policy.rs`) |
| `ThreadSettingsApplied` / `ThreadGoalUpdated` | **Carried forward** from prior generation — capture gap, documented stopgap |
| `ThreadRolledBack` | **Carried forward** — stopgap until the LHC rollback-capture batch lands (`docs/rollback-capture-outline.md` in the LHC repo) |
| Review-mode / patch / MCP / web-search / image / subagent end-events | **Regenerated from the model stream where derivable, else carried forward**; per-event disposition decided at build time and recorded in the materializer's module doc |
| Transient (never persisted — verified `policy.rs` list) | Dropped |

**Failure semantics:** a failed rewrite (disk full, IO error) leaves
the old file untouched and authoritative, logs loudly, and the session
continues — the append behavior is NOT retained as a fallback write
path; the next compact retries the rewrite. A torn swap is impossible
by construction (rename is atomic; every pre-rename crash leaves the
old generation intact) — crash-injection tests prove it.

**Old files:** the reader stays dual-format indefinitely (verified:
reverse scan already handles appended `Compacted` records). A
pre-rework session gets its first rewrite at its next compact; until
then it resumes exactly as today.

**Explicitly out of scope:** the LHC rollback mechanism (parked,
outlined in the LHC repo), pi-lhc/cc-lhc wiring, grok-build wiring
(inherits slices A–B's shape later), multi-file lineage, Paginated
live verification (test-level only; terminal is Legacy).

## Execution — five slices

Loop pattern per slice: grok-4.5 implements against this doc + the
foundation reading; Fable verifies (full diff read, independent check
runs, mutation of new invariants); nothing commits unverified. FORK.md
touchpoint inventory, sentinels, and patches/ regenerate in the same
commit as any hook change.

**A — field capture wiring.** Fork sends the v5 facts: outcome/reason/
timing at turn complete/abort (the host's own `started_at`/
`completed_at`, not capture time), per-call provider usage at
`ResponseEvent::Completed`. Vendor pin already carries schema v5
(`614543a`). No rollout changes. Verify: e2e capture test asserting all
three land in the record; abort path asserts `aborted` + reason.

**B — materializer (biggest).** Pure function: LHC thread (+ prior
generation for carry-forwards) → complete rollout line sequence per the
table above. No IO beyond reads. Verify: golden tests from fixture
threads including the v5 adversarial corpus; every display-event
disposition exercised; boundary record field-completeness pinned.

**C — the swap.** Materializer wired into the compact arm replacing the
append: temp-write/fsync/rename/reopen, generation retention (1),
window-number continuity, failure semantics above. Verify: crash
injection between every step (kill points: post-write, post-fsync,
post-rename, pre-reopen) — each leaves old or new generation intact,
never torn; rewrite-failure test (read-only dir) leaves old file
authoritative; recorder-reopen pinned by a test that appends post-swap
and asserts it lands in the NEW file.

**D — certification.** The regenerate-and-resume drill as a committed
test; dual-format resume (old appended-shape file); display consumers
against a rebuilt file (token-usage replay `rposition` on newest
TokenCount — verified consumer; thread-store user-message reads);
tripwire layer additions; FORK.md/patches updated. Layer-2 and layer-3
matrices (below) run here.

**E — startup reconciliation + final live cert (last, per Lee).** At
session open: rollout missing/corrupt/stale relative to LHC's compact
point → regenerate from thread. Then the live finale: delete a real
session's rollout, regenerate, resume, converse.

## Test plan — three layers

**Layer 1, mechanical (per slice, in-loop):** unit + golden tests as
listed per slice. Every new invariant mutation-tested: break the code,
watch the named test fail, restore (law 3).

**Layer 2, full-stack deterministic (slice D, repeatable, zero
network):** real compacts via `create_deterministic_inference_callbacks`
— real selection walk, real bands, real write-back, real rewrite.
Matrix:

1. First rewrite on a file with pre-existing appended `Compacted`
   records (the transition every existing session hits once).
2. Double compact / band-roll, rewrite each time; window numbers
   monotonic; generation rotation correct.
3. Mid-turn abort → outcome captured, subsequent rewrite well-formed.
4. Pre-existing `ThreadRolledBack` markers → carried forward; dropped
   turns stay dropped through regeneration.
5. Adversarial content (v5 conformance corpus: astral unicode,
   boundary floats, oversized tool results) through materialize +
   resume round-trip.
6. Crash injection at every swap step (also in slice C's own tests;
   rerun here against full-stack state).
7. Rewrite failure (read-only dir / injected IO error) → old file
   authoritative, session continues.
8. Empty-ish edges: compact with zero post-boundary turns; earliest
   legal compact; thread whose first compact happens before any tool
   call.

**Layer 3, live inference (slice D/E; `codex exec` scripted in tmux +
interactive resume checks; ChatGPT auth, gpt-5.6-luna, per standing
ruling):**

1. Tool-heavy scripted session → real compact → rewrite →
   `codex resume <id>` → model continues coherently; `/context` sane.
2. Second compact on the resumed session; resume again (rotation under
   real use).
3. Real mid-inference interrupt → outcome/timing/usage verified in the
   record → compact → resume.
4. A pre-rework session resumes unchanged (dual-format, live).
5. Two-counters check: provider usage totals in LHC vs the host's own
   token accounting.
6. (Slice E) Delete rollout, regenerate, resume, converse — the
   conformance drill performed live.

Estimated layer-3 spend: 50–80k tokens on plan quota across the matrix
(band-eval precedent: ~5–10k per run). **Authorization for this spend
is the one open ask before slice D.**

## Sequencing and status

A → B → C → D → E, strictly. A is unblocked now. Slices B–C touch the
compact arm; the fork is otherwise idle (both prior orchestration
efforts closed out). After E: grok-build's equivalent wiring, then the
rollback batch when scheduled.
