# Chunk 3 — certification record (Phase 4, unit 22 of 22)

Date: 2026-07-26. Working tree `/srv/work/codex`, branch `lhc`,
base commit `3aa3a44d22` (Chunk 2). **Nothing here is committed or pushed.**

This document is meant to be trusted without rerunning anything. Every number
in it came out of a run on this tree; every claim that is *not* backed by a run
is in §7 (not exercised) or §8 (ceilings), named plainly. Where a measurement
contradicted an expectation — including one of mine — the measurement is what
is written down.

**Chunk 3 was split.** Phase A (this document) is everything that costs no
model quota. Phase B — every run that calls `gpt-5.6-luna` for real — was
**not run**; it is costed in §6 and awaits Lee's budget ruling.

Phase A ran in two rounds. Round 8 certified and found defects; **round 9 fixed
three of them under instruction** — the patch series (N1), tripwire layer 13
(N2), and turn cancellation reaching the compact arm (N3). Sections marked
"was … now" carry both measurements deliberately: the before is the evidence
that the after is not vacuous.

---

## 1. Headline

Phase A ran in two rounds. Round 8 found the defects; **round 9 fixed two of
them under instruction** (N1/N2: the patch series and tripwire layer 13; N3:
turn cancellation). This section reflects the state *after* round 9.

| | |
|---|---|
| Tripwire | **ALL 13 GREEN.** Was 12/13 on arrival — layer 13 had been red since `3aa3a44d22`. §4.1 |
| History-reset recovery drill | **WORKS.** 26/26 fork-owned core files byte-identical; reconstructed tree compiles. §4.2 |
| Upstream sync drill, hooks live | **Ran, clean.** Zero conflicts — but the window was 3 commits / 9 h. §4.3 |
| Turn abort stops derivation | **FIXED (N3).** Was 3 calls at abort → 12 by 500 ms, history rewritten, marker committed. Now 1 → 1, nothing installed. §4.4 |
| Carried gap 1 (M1 core-level measurement) | **SETTLED**, with numbers. §3.1 |
| Carried gap 2 (real per-call latency) | **Half settled offline** — per-call *input size* now measured; latency still needs Phase B. §3.2, §6 |
| KV / prefix-cache impact | **MEASURED**: a compact invalidates **100%** of the prefix. §3.4 |
| Resume / fork / abort | **Exercised offline**, all three. §3.3 |
| Real compacts on `gpt-5.6-luna` (≥3), live auth lane | **NOT RUN** — Phase B, blocked on budget ruling. §6, §7 |

Would I use it for real work? §9. Round 9 removed both of the two things I said
I would fix or watch most closely; what remains is the idle-tick-rate question
and an unrehearsed conflicting sync.

---

## 2. What changed in this working tree

Round 8 was certification only — six tests, no production change. Round 9
changed production behaviour once, under instruction: N3, propagating turn
cancellation into the compact arm. Sentinels 36 → **38**.

| File | Change |
|---|---|
| `codex-rs/lhc/codex-lhc-host/src/install.rs` | `m1_remaining_is_not_a_monotone_progress_metric` |
| `codex-rs/core/src/compact_lhc_tests.rs` | `m1_core_idle_pump_…`, `c1_resume_…`, `c1_fork_full_history_…`, `c1_kv_prefix_cache_…`, `c1_derivation_call_input_cost_profile_…`, `c1_abort_mid_compact_…` (N3) |
| `codex-rs/core/src/session/lhc_capture_e2e_tests.rs` | `e2e_rollout_reconstruction_does_not_re_ingest_into_capture` |
| `codex-rs/core/src/compact_lhc.rs` | **N3**: arm takes the turn's `CancellationToken`; races it against the worker and the timeout |
| `codex-rs/core/src/tasks/compact.rs` | **N3**: binds `cancellation_token` (was `_cancellation_token`) and passes it |
| `codex-rs/core/src/session/turn.rs` | **N3**: `run_auto_compact` gains a `cancellation_token` parameter; 4 call sites |
| `codex-rs/core/src/session/lhc_band_shape_eval_tests.rs` | call-site update for the new arm signature |
| `scripts/check-lhc-hooks.sh` | **N2**: layer 13 rewritten; `EXPECTED_HOOKS` 36 → 38 |
| `patches/lhc/0001..0007`, `patches/lhc/BASE`, `patches/lhc/README.md` | **N1**: whole series regenerated from one base |
| `FORK.md` | inventory rows 18-23; sync record; history-reset section rewritten |

Every new invariant was mutation-proven — broken, observed failing, restored
(§10).

---

## 3. C1 — what was exercised, with numbers

All C1 Phase A work runs on deterministic offline callbacks through production
entry points. Where the offline harness under-measures something relative to a
live run, that is stated in place rather than left for the reader to infer.

### 3.1 Carried gap 1 — the M1 core-level measurement. SETTLED.

Chunk 2 left this: "`remaining` grew under the core pump and fell under the host
pump; nobody accounted for it." The core test was deleted rather than left
meaningless.

**It is now reproduced deterministically, and the Chunk 2 observation was
correct — it was truncated, not wrong.** Instrumenting the pump on a 60-turn
core session (120 captured events, 294 derivation work items) shows two phases:

| Idle ticks | `ran`/tick | `remaining` | Inference calls |
|---|---|---|---|
| 1 – ~12 | 8 (the full `IDLE_PUMP_MAX_ITEMS`) | **climbs** 121 → ~137, +4/tick | **0** |
| ~13 – ~40 | 8 | falls to **0** | all 117 |
| 41 – 80 | 0 | 0 | — |

The first phase drains only *non-inference* work — ingest, placement,
projection. Each settled item enqueues more successors than it consumed, so
`remaining` rises while the inference counter sits at zero. Any experiment
shorter than ~13 ticks sees exactly the reported symptom: "the pump runs,
derives nothing, and the backlog grows." From ~tick 13 the cascade front reaches
inference-bearing kinds and the whole backlog clears.

Two conclusions, both now pinned by tests:

1. **`DrainReport::remaining` is the wrong instrument.** It is
   `count_live_items` — `SELECT COUNT(*) FROM work_item WHERE status IN
   ('queued','claimed')` — over a graph that cascades (`turns/internal/derive.rs`
   detailed-turn compression, `chunks.rs::enqueue_chunk_summaries` on chunk
   close, `messages/internal/cascade.rs` rebuild groups). It nets settled work
   against newly enqueued successors, so it under-reports progress and can rise
   during real work. Independently reproduced in the host crate on an 80-turn
   thread: over 8 production idle ticks `remaining` fell 159 → 147 while 32
   inference calls ran, and **two ticks settled work for zero net reduction**
   (`m1_remaining_is_not_a_monotone_progress_metric`, identical across 3 runs).

2. **Measured on the metric that matters, M1 works.** Compact-time inference
   calls, control vs pumped, identically seeded 60-turn threads:

   | Arm | Background calls | **Calls paid at compact time** |
   |---|---|---|
   | Control (no idle pump) | 0 | **117** |
   | Pumped (80 idle ticks) | 117 | **0** |

   Driven through `Session::emit_thread_idle_lifecycle_if_idle` — the production
   seam, not the pump function — so severing `tasks/lifecycle.rs` fails the test.

**The operational number that falls out of this, and the one worth
remembering:** at 8 items/tick the pump needs roughly **one idle tick per 1.5
conversation turns** to keep up. Below that rate the compact still pays the
balance, and M1's protection is partial rather than absent.

### 3.2 Carried gap 2 — per-call cost. Input measured; latency still open.

Chunk 2's timeout arithmetic multiplied an assumed latency by a call count.
The *other* factor is input tokens per call, and that is measurable offline
because the callback inputs are exactly what the live `ModelClient` bridge
sends. On a 60-turn, 76,090-token history:

| Kind | Calls | Total input tokens | Mean | Max |
|---|---|---|---|---|
| `compress_detailed_turn` | 59 | 75,038 | 1,271 | 1,272 |
| `summarize_chunk_brief` | 58 | 928 | 16 | 16 |
| `smooth_prompt` | 0 | — | — | — |
| `summarize_tool_result` | 0 | — | — | — |
| **total** | **117** | **75,966** | **649** | **1,272** |

Three things follow, and they are the basis of the Phase B estimate in §6:

* **1.95 derivation calls per turn** measured, against the "~3/turn" Chunk 2
  assumed. The assumption was conservative by ~1.5×.
* **Total derivation input ≈ the history size, once** (75,966 vs 76,090 tokens,
  0.998×). Derivation reads the conversation approximately once, sharded.
* **Derivation is fed excerpts, never the conversation.** Largest single call
  carried 1,272 tokens against a 76,090-token history — 1.7%. This is now an
  asserted invariant; if it ever trips, per-call cost starts scaling with thread
  length and the 120 s bound becomes unreachable.

**Where this under-measures a live run**, stated plainly:
`summarize_chunk_brief` consumes the *previous* stage's output, and the offline
deterministic stub is far shorter than real model output — so its 16-token mean
is an artefact and will be materially larger live. And this fixture has no tool
calls, so `summarize_tool_result` shows zero; a real session with tool use adds
that kind entirely. Both are reasons C1 asks for *real tool use*, and both are
priced conservatively in §6.

Latency per call remains **unmeasured**. It cannot be obtained offline, and it
is the factor that decides whether the 120 s bound holds. It is the primary
purpose of Phase B run B3.

### 3.3 Resume, fork, abort

**Resume (C1.2).** Split across the two seams it actually spans.

* *No re-ingest.* The load-bearing fact is that rollout reconstruction installs
  history via `state.replace_history` and does **not** fan out to
  `RawItemContributor` — so a resumed session does not feed the previous
  compact's served body back into the archive as source. This is invisible and
  fragile: the resumed session's slot has no derived provenance yet (I2's reseed
  runs at compact time, *after* reconstruction), so nothing else would catch it.
  Now guarded at the seam, driven through `apply_rollout_reconstruction` itself.
  Mutation: routing reconstruction through `send_raw_response_items` grew the
  archive 1 → 7 events and failed the test.

  Worth recording because it cost me a wrong conclusion: my first version
  replayed history through `record_conversation_items_with_provenance` and
  measured 160 → 175 source events. That is a real hazard but the wrong
  production analogue — it is the live-turn path, not the reconstruction path.

* *Durable provenance survives.* A second `Session` over the same archive starts
  with an empty process slot; after `seed_last_lhc_durable_from_rollout` (the
  production resume seam) recovers the record from a `RolloutItem::Compacted`
  and the arm runs `reseed_slot_from_durable_session`, the slot carries **31
  derived ids**. Mutation: deleting the reseed call fails the test. This is I2's
  durable path in anger.

**Fork — `SpawnAgentForkMode::FullHistory` (C1.3).** The census's highest-risk
consumer. A child on a fresh archive inheriting the parent's post-compact body:

* the child's history is **structurally equal** to the parent's 31-item body;
* the child's own compact returns `Unavailable(NoReduction: body_tokens=37,139
  host_tokens=37,079)` and **fails open to the native ladder**.

That is correct, not a miss: there is nothing left to compact, so LHC declines
rather than compacting a partial archive into a full replacement (Chunk 2
stopping rule 5). The child inherits a coherent body.

**Abort mid-compact (C1.4).** Driven through the production manual ladder using
`Session::handle_task_abort`'s exact sequence — cancel the token, wait the
100 ms `GRACEFULL_INTERRUPTION_TIMEOUT_MS`, hard-abort the handle. Result:

* history **160 → 160 items, unchanged**;
* **no marker committed**;
* task aborted, session intact.

All three C1 requirements hold. But *why* they hold is not what it looks like —
see escalation §5.2.

### 3.4 KV / prefix-cache impact — measured

The brief asks for this explicitly and no previous round produced a number.

A provider's prefix cache keys on the literal leading token sequence, so the
cost of a compact is exactly how much of the previous request's prefix survives
into the next one. On a real compact through the production arm (80 turns):

| | items | est. tokens |
|---|---|---|
| Pre-compact request | 160 | 101,455 |
| Post-compact request | 31 | 37,079 |
| **Shared leading prefix** | **0** | **0** |

**Reusable share of the next request: 0.0%.** A compact invalidates the prefix
cache completely — the first post-compact turn re-sends its entire 37,079-token
body as uncached input. This is inherent, not a defect: LHC's body opens with
derived band content, so there is no surviving prefix by construction.

Two framings of the same number, both worth having:

* Against the 101,455 tokens the compact *removed* from every subsequent turn,
  paying ~37 k uncached once is a good trade — it pays for itself on the first
  turn and compounds after.
* But it is a real, repeated cost: at three compacts in a long session, that is
  three full-body cache misses, not one.

This is the *invalidation* half and it is exact — it depends only on the two
histories, not on the provider. The *billing* half — confirming
`cached_input_tokens` collapses on the turn after a compact and recovers on the
turn after that — needs a live run and is Phase B run B4. The assertion is
written so that if LHC ever gains a cache-preserving prefix, the test tells the
reader this document is now wrong.

---

## 4. C2 — sync drill, recovery drill, and the abort fix

### 4.1 Tripwire: was red on arrival, now 13/13

Run on `3aa3a44d22` with a clean tree, first action of round 8:

```
ok sentinel: 36/36 … ok golden: 21
TRIPWIRE patch-repro: patches/lhc/0007-lhc-compact-arm.patch failed to apply on HEAD
TRIPWIRES FAILED
```

Layer 13 applied `0007` to a worktree at `HEAD`. `0007` was the diff *Chunk 2a →
Chunk 2*, so once Chunk 2 was committed `HEAD` already contained it and the
apply could only fail. The layer was meaningful **only** while the work was
uncommitted; Chunk 2's "13 layers green" was true when run and false thereafter.

**N2 rewrote the layer** to test the drill it exists to guard: apply the *whole*
series to a detached worktree at the recorded upstream base
(`patches/lhc/BASE`), require byte-identity with the live tree for every file
the series touches, and additionally require that **every** fork-owned file
under `codex-rs/` outside the adapter tree is covered by some patch. That
property holds before and after a commit.

After N1 + N2 + N3:

```
ok sentinel: 38/38 LHC-HOOK markers
ok vendor: clean at 3663839
ok check: codex-core + codex-app-server + codex-extension-api
ok lib-test: codex-lhc-host --lib
ok cert-test: codex-lhc-host certification
ok e2e: codex-core lhc_capture_e2e (real Session seam)
ok upstream-schema: core config_schema_matches_fixture
ok compact-bridge: codex-lhc-host produce + marker
ok compact-arm: law1 write-back + law2 prefill + fail-open
ok fmt: codex-lhc-host
ok clippy: codex-lhc-host --no-deps (-D unused -D dead_code)
ok golden: 21 fixture files (byte-checked by certification mapping_goldens)
ok patch-repro: drill at 322d5b96cf reproduces 26 files byte-identically
ALL TRIPWIRES GREEN
```

### 4.2 History-reset recovery drill — was broken, now works

**Round 8 finding.** Executed literally against a clean worktree at the fork's
merge-base: `0004` failed to apply; of 22 patch-covered files, **16 identical,
5 differ, 1 missing**. Four defects:

* **R1** — no single valid base. `0004` needed `session/mod.rs` at blob
  `d0a14d1223` (upstream `63fe5a6b71`, the Chunk 1 base) and produced
  `c880af3a15`; `0007` needed `2389a217df` (Chunk 2a). The upstream drift
  between them was in no patch, so the series applied in order at no base.
* **R2** — `0006-app-server-install`'s result blob (`a6e7467449`) matched no
  fork commit; it restored the Chunk-1-era `include_str!` registration test that
  a later round replaced — a test the standing bar forbids.
* **R3** — four fork-owned core files in no patch, all compile-critical:
  `core/src/state/service.rs`, `core/src/session/session.rs`,
  `core/src/session/tests.rs`, `core/src/session/lhc_band_shape_eval_tests.rs`
  (742 lines, whose `mod` declaration *is* inside `0007`).
* **R4** — no gate caught it. Layer 13 tested `0007` alone; clippy's
  `-D dead_code` runs only on `codex-lhc-host`.

**Round 9 fix (N1).** The series is a recovery mechanism, so every patch is now
a diff from **one** base, recorded in `patches/lhc/BASE` = `322d5b96cf` (the
last upstream commit before Chunk 0). Each fork-owned file appears in exactly
one patch — with one base, a file in two patches would double-apply. R3's four
files went into `0007` and into FORK.md's inventory as rows 20-23.

**Proof.** Worktree at `322d5b96cf`, fork-owned non-touchpoint trees restored,
series applied in order:

```
APPLIED 0001-workspace-member      APPLIED 0005-app-server-dep
APPLIED 0002-raw-item-contributor  APPLIED 0006-app-server-install
APPLIED 0003-feature-flag          APPLIED 0007-lhc-compact-arm
APPLIED 0004-session-raw-item-hook
```

Byte-identity of every fork-owned file under `codex-rs/` outside the adapter
tree — `identical=26  differs=0  missing=0`:

```
  codex-rs/Cargo.toml                                    identical
  codex-rs/Cargo.lock                                    SKIP (FORK.md row 8: cargo-regenerated)
  codex-rs/app-server/Cargo.toml                         identical
  codex-rs/app-server/src/extensions.rs                  identical
  codex-rs/core/Cargo.toml                               identical
  codex-rs/core/config.schema.json                       identical
  codex-rs/core/src/compact.rs                           identical
  codex-rs/core/src/compact_lhc.rs                       identical
  codex-rs/core/src/compact_lhc_tests.rs                 identical
  codex-rs/core/src/lhc_inference_bridge.rs              identical
  codex-rs/core/src/lib.rs                               identical
  codex-rs/core/src/session/lhc_band_shape_eval_tests.rs identical
  codex-rs/core/src/session/lhc_capture_e2e_tests.rs     identical
  codex-rs/core/src/session/mod.rs                       identical
  codex-rs/core/src/session/session.rs                   identical
  codex-rs/core/src/session/tests.rs                     identical
  codex-rs/core/src/session/turn.rs                      identical
  codex-rs/core/src/state/service.rs                     identical
  codex-rs/core/src/state/session.rs                     identical
  codex-rs/core/src/stream_events_utils.rs               identical
  codex-rs/core/src/tasks/compact.rs                     identical
  codex-rs/core/src/tasks/lifecycle.rs                   identical
  codex-rs/ext/extension-api/src/contributors.rs         identical
  codex-rs/ext/extension-api/src/contributors/raw_item.rs identical
  codex-rs/ext/extension-api/src/lib.rs                  identical
  codex-rs/ext/extension-api/src/registry.rs             identical
  codex-rs/features/src/lib.rs                           identical
```

And — R3's actual consequence, so worth testing directly rather than inferring —
the reconstructed tree **compiles**:
`cargo check -p codex-core -p codex-app-server -p codex-extension-api` exits 0.

**One more defect found while proving it.** FORK.md's step 3 said
`git submodule update --init` at the pinned commit. That cannot work: the
submodule gitlink is a tree entry in fork *commits*, and the drill starts from a
clean upstream base where no such entry exists. It fails with `cannot change to
'codex-rs/lhc/vendor/long-horizon-context': No such file or directory`. The step
is now an explicit clone-and-checkout at the pin recorded in FORK.md §Layout.
Nothing in the old text would have told you this — it was only visible by
running it.

### 4.3 Upstream sync drill — ran, clean, and thinner than it should be

`git fetch upstream` → `git merge upstream/main` on a branch off `lhc`, hooks
live.

* **3 upstream commits**, `322d5b96cf..61a44880a8`, spanning 9 hours.
* Merged by `ort`, **zero conflicts**, zero hook files touched.
* Tripwire on the merged tree: 12/13 green (layer 13 red, pre-N2).

**This exercised the procedure, not the conflict resolution**, and saying
otherwise would be pretending. What can be offered instead is the exposure,
measured on real upstream data over 30 days:

| File | upstream commits | commits touching the fork's own line ranges |
|---|---|---|
| `core/src/session/mod.rs` | **66** | **1** (of 13 hook sites) |
| `core/src/session/turn.rs` | 35 | 1 (of 2) |
| `core/src/tasks/compact.rs` | 2 | 1 (of 1) |
| `features/src/lib.rs` | 20 | 0 |
| `core/src/compact.rs` | 11 | 0 |
| `ext/extension-api/src/registry.rs` | 1 | 0 (of 7) |
| all other hooked files | ≤ 18 each | 0 |

66 commits a month on the top-risk file; **3 of 26 hook sites saw any churn at
all**. That is the "tiny hook footprint" mitigation measured rather than
asserted. It is not a substitute for a sync that actually conflicts — §7 lists
that as still not done.

Note for the next sync: the series is now pinned to `patches/lhc/BASE`. A merge
that changes a fork-owned file requires regenerating the series **and** moving
`BASE`, in the same commit. Layer 13 fails loudly if that is skipped, which is
the point.

### 4.4 N3 — turn abort now stops derivation

**Round 8 finding.** `CompactTask::run` bound its token as
`_cancellation_token` — upstream's own shape, and upstream's native compact
ignores it identically — and `run_auto_compact` had no token at all. The arm
built a private `AtomicBool` set only by its own `COMPACT_THREAD_TIMEOUT`.
Measured with derivation in flight, cancelling the token and *not* dropping the
future: the compact ran to completion, rewrote history **160 → 31 items**, and
committed its marker, all after the abort. Production was saved from that only
by the hard `handle.abort()` 100 ms later
(`GRACEFULL_INTERRUPTION_TIMEOUT_MS`) — and the detached derivation worker
survived even that: **3 calls at abort, 12 by 500 ms later**, still climbing
against a 75 s budget.

**Round 9 fix.** The turn's real `CancellationToken` now reaches the arm and is
raced (`tokio::select!`, biased) against both the worker and the timeout. On
cancellation the arm sets the same `AtomicBool` the M2 timeout path uses — which
the detached drain already checks between batches — and fails open: no partial
install, no marker, native ladder untouched. `run_auto_compact` gained a
`cancellation_token` parameter; all four production callers already had one in
scope. Both are marked with `LHC-HOOK` sentinels and are in the inventory
(rows 18-19) and in `0007`.

Same test, same sequence, without any hard abort — the token alone now suffices:

| | before N3 | after N3 |
|---|---|---|
| calls at cancel | 3 | 1 |
| calls 2 s later | 12 (climbing) | **1** |
| history | 160 → **31** | 160 → **160** |
| marker committed | **yes** | **no** |
| task returns on its own | no (needed hard abort) | yes |

## 5. Escalations

Round 8 raised three. Two were settled from the fork's own documents and became
round 9's instructions; one remains open.

### 5.1 Recovery drill / patch series — **SETTLED, fixed in round 9**

`patches/lhc/README.md` and FORK.md §History-reset already answered "what is the
series for": a recovery mechanism, every patch a diff from the upstream base.
Ruling: `322d5b96cf`. Fixed under N1/N2 — see §4.1, §4.2.

### 5.2 Turn cancellation — **SETTLED, fixed in round 9**

Ruled that "stop working when the user hits abort" is not a judgment call, and
that upstream paying this for one model call is not a precedent for the fork
paying it for ~2 calls per turn. Fixed under N3 — see §4.4.

### 5.3 Phase B budget — **still open with Lee**

No live run was made. §6.

### 5.4 Carried, not escalated

Two Chunk 2 gaps remain live-only and are not in the Phase B plan because they
need fault injection rather than spend: silent partial derivation-failure
demotion (§7.9), and the tag-fidelity ceiling (§8), which no live run lifts.

## 6. Phase B — costed plan

Bounded by **input tokens per call × calls**, not by turn count. The last
authorised run overran 12× because it was bounded by turns; this plan states the
arithmetic so the overrun mode is visible before spending.

**The cost model, from §3.2's measurements.** For a compact of an `H`-token
history:

* derivation calls ≈ `1.95 × turns`, and `turns ≈ H / 1,270` (measured mean
  `compress_detailed_turn` input) — so **calls ≈ H / 650**;
* total derivation **input** ≈ `1.0 × H` (measured 0.998×);
* total derivation **output** ≈ body size ≈ `0.37 × H` (measured 37,079 from
  101,455).

Two corrections applied on top, both from §3.2's named under-measurements:

* `summarize_chunk_brief` input is under-measured offline (canned stub shorter
  than real output). Priced at **+0.15 × H**.
* the offline fixture has no tool calls; a real tool-using session adds
  `summarize_tool_result`. Priced at **+0.20 × H** and **+0.25× on call count**.

**Working figure: input ≈ 1.35 × H, output ≈ 0.37 × H, calls ≈ 1.25 × H/650.**
Every run below states `H` and multiplies. These are estimates on a corrected
model, not measurements — B3 replaces them with measurements.

| Run | Purpose (C1 item) | `H` per compact | Compacts | Turns | Calls (est.) | **Input tok** | **Output tok** | **Total** |
|---|---|---|---|---|---|---|---|---|
| **B1** | ≥3 real compacts, one session, real tool use; per-compact source events / body items / tokens before-after / band composition | 40 k, 40 k, 40 k | 3 | ~95 | ~230 | 162 k | 44 k | **206 k** |
| **B2** | Auth-lane confirmation in a live session: derivation rides `gpt-5.6-luna`, not the turn model | 12 k | 1 | ~10 | ~23 | 16 k | 4 k | **20 k** |
| **B3** | **Real per-call latency (gap 2)** — wall-clock per call by kind, and whether 117 calls fit under 120 s / 75 s | 40 k | 1 | ~32 | ~77 | 54 k | 15 k | **69 k** |
| **B4** | KV/prefix-cache billing: `cached_input_tokens` on the two turns after a compact | 25 k | 1 | ~20 | ~48 | 34 k | 9 k | **43 k** |
| **B5** | Live band-shape eval (`lhc_band_shape_eval_*`, built in Chunk 2a, never run) | 30 k | 1 | ~24 | ~58 | 40 k | 11 k | **51 k** |
| **B6** | `model_change` on a real mid-thread `/model` switch (FORK.md checkpoint) | 8 k | 0 | ~6 | 0 | 8 k | 2 k | **10 k** |
| | | | | | **~436** | **314 k** | **85 k** | **~399 k** |

**Every run except B2 and B6 exceeds the ~50 k stop-and-ask bound, and the
chunk total is ~8× it. I am therefore not running any of them, and this table
is the ask rather than a plan I intend to execute.**

Notes for whoever rules on this:

* **B3 is the one that matters most.** It is the only run that converts gap 2
  from arithmetic into measurement, and its answer decides whether the 120 s
  bound and the idle pump hold in practice. If only one run is authorised,
  authorise B3.
* **B1 can be halved** to 2 compacts at `H` = 25 k (~85 k total) and still
  satisfy "multiple real compacts with real derivation", at the cost of not
  exercising the 3-compact marker-chain path that
  `production_three_compacts_do_not_reingest_body` covers offline.
* **B2 and B6 are cheap** (~30 k combined) and independently useful; they could
  be authorised alone.
* **B5 is deferrable.** The harness exists and is offline-tested; a live run is
  a quality check on model band-shape tolerance, not a correctness gate.
* **Overrun guard:** each run should be bounded by asserting `H` before it
  starts and aborting if the pre-compact history estimate exceeds the stated `H`
  by more than 25%. Bounding by turn count is what produced the 12× last time.

---

## 7. What was NOT exercised — named plainly

An honest gap list is worth more than a claim of completeness, and every prior
round of this project that concealed a gap cost a round to undo.

1. **No live model call was made.** Zero. All C1 results above are on
   deterministic offline callbacks through production entry points. The ≥3 real
   compacts, the live auth-lane confirmation, and the real latency measurement
   are Phase B and were not run.
2. **Real per-call latency (carried gap 2) remains unmeasured.** §3.2 measured
   the input-size factor; the latency factor is unobtainable offline. The 120 s
   bound and 75 s drain budget remain justified by arithmetic.
3. **The sync drill did not exercise conflict resolution.** 3 commits, 9 hours,
   zero hook files touched. §4.3's churn table is exposure measurement, not a
   substitute. A sync after a real gap — a week or more — has still never been
   rehearsed on this fork.
4. **No real tool use.** Every fixture is text turns. `summarize_tool_result`
   has never been invoked, live or offline, in any C1 measurement; its cost and
   its band behaviour are both unobserved.
5. **Resume was not driven through `Session` construction.** `InitialHistory::
   Resumed`/`Forked` are constructor paths not reachable from the LHC test
   modules; the two seams they call (`apply_rollout_reconstruction`,
   `seed_last_lhc_durable_from_rollout`) were each driven directly instead.
   Their composition inside `Session::new` is untested by the fork.
6. **Fork was simulated, not spawned.** No `SpawnAgentForkMode::FullHistory`
   call was made; the child was constructed with the parent's post-compact body
   and the rollout-carried durable record. The census's other fork consumers —
   `/btw`, `/side`, guardian, multi-agent v1 — were not touched at all.
7. **The KV measurement is invalidation, not billing.** 0% prefix reuse is
   exact and provider-independent; that this shows up as `cached_input_tokens`
   collapsing on a real request is inferred, not observed (Phase B run B4).
8. **`MODULE.bazel.lock` refresh** — still not run; no Bazel on this host.
   Carried from Chunk 2.
9. **Chunk 2's gap 3 (silent partial derivation-failure demotion)** is
   untouched. If one call kind fails (e.g. 429 on `compress_detailed_turn`), LHC
   demotes those turns `detailed → brief` and the arm cannot see it happened.
   Offline callbacks never 429, so this is a live-only observation and is not in
   the Phase B plan above — it would need fault injection, not just spend.
10. **Chunk 2's gap 4 (full-suite ordering artefact)** is unchanged and still
    not ours: `session::turn::tests::post_sampling_token_estimate_is_disabled_by_
    always_on_sinks` fails in a full `-p codex-core --lib` run, passes in
    isolation and within its module. Pre-existing upstream tracing interference.
11. **A conflicting sync is still unrehearsed**, and the series is now pinned
    to a `BASE` that a future sync will move. Layer 13 will catch a stale
    series, but nobody has yet performed the regenerate-and-move-BASE step for
    real. That is the next thing I would want exercised.

---

## 8. Known ceilings

These are properties of the design or the provider, not bugs to be fixed. They
should be read as "this is as good as it gets without a different architecture".

* **Encrypted reasoning is opaque.** Capture stores what the wire carries. When
  reasoning payloads are provider-encrypted, the archive holds the ciphertext
  envelope; LHC cannot band, summarise, or rebuild across it. Same class as the
  t3code narration-redaction ceiling: provider-side, not a fork bug. A
  reasoning-heavy thread is less compressible than its token count suggests.
* **Fork off an old compact point loses provenance.** `DERIVED_MARKER_CAP = 8`
  retains the most recent markers' derived ids/digests. History is linear — each
  write-back re-fingerprints the whole served body — so a fork taken from a point
  more than 8 compacts back cannot reseed its provenance and will refuse or fail
  open rather than mistake LHC's own output for user content. Correct, but it is
  a cap, and a long-lived thread forked from its own distant past hits it.
* **`ModelOutput` vs `HostContext` vs `InterAgent` tag fidelity is
  unverifiable by behaviour.** The mapper collapses them for every variant; only
  `UserPrompt` vs the rest is observable on user-role items. The tags are present
  and correct in code at the stream and compact call sites, but no test can
  distinguish them, so no test guards them. Making this durable needs a payload
  field, not a test. Unchanged from Chunk 2 — Chunk 3's live cert does not lift
  it either, because the distinction is invisible in the record regardless of
  where the items came from.
* **A compact always costs a full prefix-cache miss** (§3.4). Inherent to
  rewriting history; the trade is favourable but it is not free.
* **`codex-core` takes a non-optional runtime dep on `codex-lhc-host`**, so
  every core build compiles LHC and bundled SQLite regardless of
  `Feature::LhcCapture`. Documented in `patches/lhc/README.md` rather than
  contorted around; feature-gating it needs a modularisation.

---

## 9. Would I use this for real work?

**Yes, with the feature on** — and I would watch three things.

What earns that: the compact arm is not a summariser wearing LHC's name. The
body comes from `lhc.compact()` through the typed view; law 1 holds as
structural equality under mutation; law 2 holds as a next-turn property through
the production token-status path; both ladders are entered by tests that fail
when their hooks are removed; and every path that cannot complete honestly —
partial archive, zero reduction, derivation failure, timeout, cancel — fails
open to the native ladder rather than installing something plausible. That
last property is the one I would actually rely on day to day, and it is the one
this unit found the most evidence for: every C1 path I drove that *could not*
succeed (fork with nothing left to compact, resume with no history, abort
mid-flight) declined cleanly instead of installing a partial result.

Round 9 closed two of the three concerns round 8 raised — the recovery drill
(§4.2) and post-abort spend (§4.4) are both fixed and mutation-proven. What
remains to watch:

1. **Idle-tick rate against turn rate.** §3.1's finding is the practical one:
   the pump needs roughly one idle tick per 1.5 turns to keep up. A user who
   works in fast bursts with no idle gaps gets the Chunk-2 behaviour — the whole
   derivation backlog lands on the first compact, against a 120 s deadline whose
   sufficiency is still arithmetic (§7.2). The failure mode is a fail-open, not
   corruption, so it degrades to native compaction rather than breaking; but it
   degrades silently.
2. **The first upstream sync that actually conflicts.** §4.3 measured the
   exposure as low (3 of 26 hook sites saw churn in 30 days), but the drill has
   never resolved a conflict, and `session/mod.rs` takes 66 commits a month.
   That sync is also the first test of the regenerate-and-move-`BASE` step the
   series now depends on.
3. **The unmeasured latency (§3.2).** Every claim that the 120 s bound holds is
   still arithmetic. Phase B run B3 is the only thing that changes that, and it
   is the run I would authorise first.

Nothing on this list now needs fixing before use. Round 8's one
fix-before-relying-on-it — a recovery procedure that produced a non-compiling
tree carrying a forbidden test — is closed, verified by running it rather than
by reading the patches.

---

## 10. Mutation log — every new invariant, broken and restored

Per law 3: assertion of sensitivity is not evidence of sensitivity.

| # | Mutation | Result | Restored |
|---|---|---|---|
| 1 | `IDLE_PUMP_MAX_ITEMS` → `max_items: Some(0)` (pump does no work) | `m1_remaining_is_not_a_monotone_progress_metric` FAILED "fixture: derivation must actually run"; `m1_core_idle_pump_…` FAILED "idle ticks must run real derivation; got 0" | ✓ |
| 2 | delete `seed_lhc_idle_derivation_callbacks` call in `tasks/lifecycle.rs` | `m1_core_idle_pump_…` FAILED at tick 1: "idle tick 1 did not pump" | ✓ |
| 3 | `apply_rollout_reconstruction` fans replayed history into `send_raw_response_items` | `e2e_rollout_reconstruction_does_not_re_ingest_into_capture` FAILED: "archive grew 1 -> 7" | ✓ |
| 4 | delete `reseed_slot_from_durable_session(...)` in `compact_lhc.rs` | `c1_resume_…` FAILED: "the slot must carry derived provenance recovered from the durable record" | ✓ |

| 5 | N3: arm passes `&CancellationToken::new()` instead of the turn's token (pre-N3 behaviour) | `c1_abort_mid_compact_…` FAILED: "derivation must stop within one drain batch of the abort — fired 156 more calls"; history 160 → 31, marker committed | ✓ |
| 6 | N2/R3: drop `lhc_band_shape_eval_tests.rs` from `0007` | layer 13 FAILED: "fork-owned file(s) in NO patch — the drill would reconstruct upstream's version of these: codex-rs/core/src/session/lhc_band_shape_eval_tests.rs" | ✓ |
| 7 | N1: edit `tasks/lifecycle.rs` without regenerating the series | layer 13 FAILED: "codex-rs/core/src/tasks/lifecycle.rs differs after the drill" + unified diff | ✓ |

Mutations 6 and 7 are the two ways the recovery drill can rot — a fork-owned
file falling out of the series, and the tree drifting from it. Both were live
defects before round 9 and neither had a gate.

Mutation 2 also exposed a defect in my own test: with the seam severed it
originally failed only after 80 ticks × 60 s of timeouts. The per-tick assertion
was added so it fails in 20 s. A test that takes 80 minutes to report a failure
is not much better than one that cannot fail.

`git diff` after restoration confirms `tasks/lifecycle.rs`, `session/mod.rs` and
`compact_lhc.rs` are byte-identical to `HEAD`; the only modified files are the
three in §2.

---

## 11. Final state and reproducing this

Verification sweep on this working tree, after round 9:

| Suite | Result |
|---|---|
| `./scripts/check-lhc-hooks.sh` | **ALL 13 TRIPWIRES GREEN** (38/38 sentinels; drill reproduces 26 files) |
| `cargo test -p codex-core --lib compact_lhc` | **25 passed**, 0 failed |
| `cargo test -p codex-core --lib lhc_capture_e2e` | **8 passed**, 0 failed |
| `cargo test -p codex-core --lib lhc_band_shape` | **2 passed**, 0 failed, 1 ignored (live band eval — Phase B) |
| `cargo test -p codex-lhc-host --lib` | **45 passed**, 0 failed |

Nothing committed, nothing pushed. Drill worktrees left in place as evidence and
are disposable: `/tmp/lhc-reset-v2` (§4.2, includes the compiled reconstruction)
and `/tmp/lhc-sync-drill` on branch `lhc-sync-drill-c3` (§4.3).

```bash
./scripts/check-lhc-hooks.sh                       # 13/13, incl. the recovery drill
cargo test -p codex-lhc-host --lib m1_ -- --nocapture
cargo test -p codex-core --lib m1_core_idle_pump -- --nocapture
cargo test -p codex-core --lib c1_ -- --nocapture
cargo test -p codex-core --lib e2e_rollout_reconstruction -- --nocapture
```

The `--nocapture` runs print every number quoted in §3 and §4.4. The recovery
drill is now layer 13 of the tripwire, so §4.2 reproduces on every run; to do it
by hand, `git worktree add --detach <tmp> $(cat patches/lhc/BASE)`, restore the
fork-owned trees, and apply `patches/lhc/0*.patch` in order.
