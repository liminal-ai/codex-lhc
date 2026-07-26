# Chunk 3 — certification record (Phase 4, unit 22 of 22)

Date: 2026-07-26. Working tree `/srv/work/codex`, branch `lhc`,
base commit `3aa3a44d22` (Chunk 2). **Nothing here is committed or pushed.**

> **Superseded in part by round 11 (the drain correction — FORK.md §"The
> drain correction").** The SDK had been constructed in `SdkMode::Manual`;
> every compact-time drain number here (§3.5's 75 s `DRAIN_TIME_BUDGET`, the
> 102.9 s serial burst, the H ≈ 29k fail-open ceiling, §5.5's escalation, and
> the M1 idle-pump machinery in §3.1) measured that misconfiguration, not LHC.
> Round 11 opens the capture session in `SdkMode::Background`: derivation runs
> as intake commits, the compact-time drain loop and idle pump are deleted,
> and the arm only waits (bounded, cancellable) for the scheduler to settle.
> The per-call latency (§3.5), token-cost (§3.6/P1), KV (§3.4), and
> capture/recovery findings stand.

This document is meant to be trusted without rerunning anything. Every number
in it came out of a run on this tree; every claim that is *not* backed by a run
is in §7 (not exercised) or §8 (ceilings), named plainly. Where a measurement
contradicted an expectation — including one of mine — the measurement is what
is written down.

**Chunk 3 was split.** Phase A is everything that costs no model quota. Of
Phase B, **only run B3 was authorised and only B3 was run** — the real per-call
latency measurement that settles carried gap 2. Actual spend **63,838 tokens**
against a 69k authorisation and a 100k hard stop. B1, B2, B4, B5 and B6 remain
unrun and still costed in §6.

Phase A ran in two rounds. Round 8 certified and found defects; **round 9 fixed
three of them under instruction** — the patch series (N1), tripwire layer 13
(N2), and turn cancellation reaching the compact arm (N3). Sections marked
"was … now" carry both measurements deliberately: the before is the evidence
that the after is not vacuous.

---

## 1. Headline

Five rounds: 8 certified and found defects, 9 fixed three under instruction,
**B3** spent the one authorised live run, 10 acted on what B3 found, and **11
deleted most of what rounds 7-10 built** — the drain was ours to begin with.
This reflects the state after all five.

**Read §3.7 first.** It supersedes §3.1's and §3.5's conclusions.

| | |
|---|---|
| Tripwire | **ALL 13 GREEN.** Was 12/13 on arrival — layer 13 had been red since `3aa3a44d22`. §4.1 |
| History-reset recovery drill | **WORKS.** 26/26 fork-owned core files byte-identical; reconstructed tree compiles. §4.2 |
| Upstream sync drill, hooks live | **Ran, clean.** Zero conflicts — but the window was 3 commits / 9 h. §4.3 |
| Turn abort | **Correct, invariant revised (round 11).** No install, no marker, arm returns promptly. Background derivation continues by design — it is session work, not the turn's. §4.4, §3.7 |
| Carried gap 1 (M1 core-level measurement) | **Moot (round 11).** The idle pump it measured is deleted; LHC's own scheduler does this. §3.7 |
| Carried gap 2 (real per-call latency) | Measured at 1,660 ms/call (§3.5) — but **under a misconfiguration**, and before P1. An upper bound; no current bound exists. §3.7, §5.5 |
| Derivation architecture | **CORRECTED (round 11).** `SdkMode::Manual` → `Background`. Compact-time drain and idle pump deleted. Offline: **117 derivations in-session, 0 at compact**. §3.7 |
| KV / prefix-cache impact | **MEASURED**: a compact invalidates **100%** of the prefix. §3.4 |
| Resume / fork / abort | **Exercised offline**, all three. §3.3 |
| Phase B | **B3 run** (63,838 tokens actual vs 69k authorised). B1/B2/B4/B5/B6 **not run**. §6 |
| Per-call token cost | **Was 4.7x underestimated; now FIXED (P1).** Derivation shipped Codex's full 20,903-char agent prompt on every call. Removed — projected **7.2x** cheaper per call. §3.6 |

Would I use it for real work? §9. Round 8 said yes-with-caveats; B3 downgraded
that to "no for long threads"; **round 11 removes the cause of that downgrade**
— the H ≈ 29,000 ceiling was an artefact of the host draining LHC's queue at
compact time. What replaces it is not a better number but an honest absence of
one: the corrected configuration has never been measured live. §5.5.

---

## 2. What changed in this working tree

Round 8 was certification only. Round 9 changed production behaviour once (N3,
turn cancellation); round 10 once more (P1, agent prompt); **round 11 changed
the derivation architecture and deleted more than it added**. Sentinels
36 → 38 → **39**.

Round 11 net: **~500 lines deleted** — the compact-time drain loop and its three
constants, the M1 idle pump and its slot state, five tests whose subject no
longer exists, and the `SESSION_DERIVED_CAP_OVERRIDE` test global.

| File | Change |
|---|---|
| `codex-rs/lhc/codex-lhc-host/src/session.rs` | **R11**: capture path opens in `SdkMode::Background`; `close()` bounded |
| `codex-rs/lhc/codex-lhc-host/src/inference.rs` | **R11**: `LateBoundCallbacks` — capture-session callbacks that wait for host seeding rather than erroring |
| `codex-rs/lhc/codex-lhc-host/src/capture.rs` | **R11**: `CaptureHandle::drain_settled(timeout)` |
| `codex-rs/lhc/codex-lhc-host/src/compact_bridge.rs` | **R11**: drain loop deleted; L2 gate moved to LHC's typed derivation log |
| `codex-rs/lhc/codex-lhc-host/src/install.rs` | **R11**: idle pump deleted; cap override deleted |
| `codex-rs/core/src/compact_lhc_tests.rs` | `background_derivation_leaves_compact_with_no_inference_to_do` (R11, replaces the M1 test), `c1_resume_…`, `c1_fork_full_history_…`, `c1_kv_prefix_cache_…`, `c1_derivation_call_input_cost_profile_…`, `c1_abort_mid_compact_…` |
| `codex-rs/core/src/session/lhc_capture_e2e_tests.rs` | `e2e_rollout_reconstruction_does_not_re_ingest_into_capture` |
| `codex-rs/core/src/compact_lhc.rs` | **N3**: arm takes the turn's `CancellationToken`. **R11**: waits on `drain_settled` (bounded, cancellable) instead of draining |
| `codex-rs/core/src/tasks/compact.rs` | **N3**: binds `cancellation_token` (was `_cancellation_token`) and passes it |
| `codex-rs/core/src/session/turn.rs` | **N3**: `run_auto_compact` gains a `cancellation_token` parameter; 4 call sites |
| `codex-rs/core/src/session/lhc_band_shape_eval_tests.rs` | call-site update for the new arm signature |
| `codex-rs/core/src/lhc_inference_bridge.rs` | **P1**: `derivation_prompt()` sets every `Prompt` field explicitly; `base_instructions` empty, never defaulted. Two offline tests |
| `scripts/check-lhc-hooks.sh` | **N2**: layer 13 rewritten; `EXPECTED_HOOKS` 36 → 38 → 39 |
| `patches/lhc/0001..0007`, `patches/lhc/BASE`, `patches/lhc/README.md` | **N1**: whole series regenerated from one base |
| `FORK.md` | inventory rows 18-23; sync record; history-reset section rewritten |

Every new invariant was mutation-proven — broken, observed failing, restored
(§10).

---

## 3. C1 — what was exercised, with numbers

All C1 Phase A work runs on deterministic offline callbacks through production
entry points. Where the offline harness under-measures something relative to a
live run, that is stated in place rather than left for the reader to infer.

### 3.1 Carried gap 1 — the M1 core-level measurement

> **Superseded by §3.7.** The idle pump measured below was deleted in round 11:
> it was hand-rolling what `SdkMode::Background` does natively. The
> investigation stands as a record of how the cascade behaves, and the
> `remaining`-is-not-monotone finding remains true, but nothing in the fork
> reasons about `remaining` any more and the tests that measured it are gone.


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

### 3.2 Per-call cost, offline. **Superseded on the token axis by §3.5.**

> Read with §3.5. The offline profile below is correct about *content* size and
> call count, and it was **optimistic by 6.8x on total tokens**, because stub
> callbacks send no prompt and so could not see the ~4.4k base-instruction
> overhead every real call carries. The call-count and excerpt-size findings
> survive; the "total derivation input ≈ 1x history" finding does not.


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

Latency per call cannot be obtained offline. It was the primary purpose of run
B3 and is now measured — **§3.5**. The answer is that the bounds do not hold.

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

### 3.5 Carried gap 2 — real per-call latency. SETTLED. **The bounds do not hold.**

Run B3, the only authorised Phase B run. Real production path
(`try_run_lhc_compact_arm`), feature on, real `ModelClient`, ChatGPT auth
(`auth_mode=Chatgpt`), pinned model **`gpt-5.6-luna`**, resolved effort
**`low`** — luna advertises `low|medium|high|xhigh|max`, so
`resolve_lhc_derivation_effort` correctly takes the minimum supported rather
than `None`.

**Bounded by input size, verified before spending:** history grown turn by turn
and measured, `H = 40,568` tokens over 32 turns (target 40,000 ± 4,000). The
run refuses to start otherwise.

#### What it measured

| Kind | n | min | median | max |
|---|---|---|---|---|
| `compress_detailed_turn` | 6 | 1,534 ms | **2,132 ms** | 2,483 ms |
| `smooth_prompt` | 4 | 996 ms | **1,174 ms** | 1,329 ms |
| `summarize_chunk_brief` | 0 | — | — | — |
| `summarize_tool_result` | 0 | — | — | — |
| **all** | **10** | | **1,660 ms mean** | |

* **`max_inflight` = 1**, confirmed against the real client — question 4
  answered. Serial call time 16,597 ms against an arm wall clock of 17,468 ms:
  **95% of the arm's elapsed time is serialised inference.** Serialisation is
  what makes the deadline arithmetic bite, and it does.
* Tokens: input 59,019 (of which **cached 44,800 — 76%**), output 416.

#### Do the bounds hold? No.

Phase A measured 1 derivation call per 650 tokens of history, so `H = 40,568`
needs **~62 calls**. Serialised at the measured mean:

```
62 calls x 1.660 s = 102.9 s
```

* against `DRAIN_TIME_BUDGET` = **75 s** → **exceeded by 37%**;
* against `COMPACT_THREAD_TIMEOUT` = 120 s → under, but the drain budget bites
  first and fails open before the timeout is reached.

The conclusion is robust across the whole plausible range, which matters
because only two of four call kinds were sampled:

| Assumption for the ~52 unsampled calls | Total | vs 75 s |
|---|---|---|
| all at `smooth_prompt` median (1,174 ms) — optimistic | 77.6 s | still over |
| at the measured mean (1,660 ms) | 102.9 s | over by 37% |
| all at `compress_detailed_turn` median (2,132 ms) — pessimistic | 127.5 s | over 120 s too |

**Even the optimistic bound exceeds the 75 s drain budget.** Solving for the
largest history whose first compact fits: `75 s / 1.660 s = 45 calls`, i.e.
**H ≈ 29,000 tokens**. Above that, a first compact fails open to the native
ladder. Threads big enough to need LHC compaction are precisely the ones where
it declines — the exact failure the M1 idle pump was built to prevent.

Escalated in §5.5. Per instruction I did not touch the timeout or the pump.

#### Does the idle pump change the answer? Partly, and it costs more than assumed.

Phase A's "~1 idle tick per 1.5 turns" was a *count* derived with stub latency.
With real latency the binding constraint becomes wall clock. Of 294 work items
on a 60-turn thread, 117 were inference-bearing (40%), so a full 8-item tick is
~3.2 real calls ≈ **5.3 s of background inference per idle tick**.

The pump runs detached, so it never blocks a turn — but it is single-flight, so
a tick that is still running when the next idle fires is simply skipped. The
count stays right; the requirement it implies is new: **the user must be idle
for ~5 s at a time, roughly once per 1.5 turns.** In unhurried use that holds.
In fast interactive bursts it does not, derivation falls back to compact time,
and compact time is where the 75 s budget is already insufficient above
H ≈ 29k. The two findings compound rather than cancel.

#### The cost model in §6 was wrong by 4.7x — **cause removed in §3.6**

The pre-flight — one call, 55 characters of content — cost **4,392 input
tokens**. The bridge builds `Prompt { input, ..Default::default() }` and
`BaseInstructions::default()` is Codex's full base prompt: **20,903 characters,
~5.2k tokens, on every derivation call**, for a task that needs none of it.

§6 modelled derivation input as ~1x H (content only), which is what Phase A's
offline harness could see — the stubs never sent a prompt. Recomputed from
measurement, a *complete* H=40k run needs 62 calls x ~5,943 tokens ≈ **368k
tokens: 5.3x the 69k estimate and 3.7x the 100k hard stop.**

That is why this run is **truncated by design**. The budget guard refused
further calls at 59,435 tokens and LHC failed derivation
(`chunk_summary_brief: B3 budget guard`), the arm failed open, and what landed
is a real partial measurement inside budget. Latency per call does not depend on
H, so a truncated run still settles gap 2 — and the truncation itself produced
the token finding, which is arguably the more consequential of the two.

**76% of input was cached** (44,800 of 59,019), so the *billed* cost was far
below the raw token count — the base instructions prefix-cached after the first
call. The token ceiling was still what it was. Both numbers are reported because
they answer different questions.

Round 10 removed the cause (§3.6): the agent prompt is no longer sent, projected
**7.2x** cheaper per call. That also retires most of the caching benefit, since
the cached prefix *was* the agent prompt — a smaller uncached request is still
far cheaper than a large mostly-cached one, but the two effects should not be
added together.

#### Limits of this measurement

* 10 samples, two of four kinds. `summarize_chunk_brief` and
  `summarize_tool_result` were never sampled — the guard fired on the first
  chunk-brief item. The bounds conclusion is stated across the full range above
  precisely because of this.
* One run, one time of day, one network path. No variance across sessions.
* The extrapolation to 62 calls uses Phase A's calls-per-token ratio, measured
  offline; the live ratio was not independently confirmed.
* Measurement scaffolding (timing/token/concurrency instrumentation in
  `lhc_inference_bridge.rs`, plus a real-`AuthManager` session builder) was
  **removed after the run**. A stale budget guard left in the production
  derivation path could silently refuse calls, which is a worse defect than the
  one it measured. Re-running B3 means re-adding it; the method is described
  above in enough detail to reconstruct.

### 3.6 P1 — derivation no longer ships Codex's agent prompt

B3's most consequential finding was incidental to its purpose: a 55-character
derivation prompt cost **4,392 input tokens**. The bridge built
`Prompt { input, ..Default::default() }`, and `Prompt::default()` sets
`base_instructions: BaseInstructions::default()` =
`BASE_INSTRUCTIONS_DEFAULT` — **20,903 characters** of Codex's coding-agent
prompt: apply_patch conventions, sandbox and approval rules, tool protocol.
All of it, on every call, to ask a pinned model to compress a conversation turn.

**Fixed.** `derivation_prompt()` now sets every field explicitly and never
defaults `base_instructions`.

Audit of the other `Prompt::default()` fields, since the same route could have
carried more agent surface — three were already correct, one changed:

| Field | Default | For derivation | Action |
|---|---|---|---|
| `base_instructions` | `BASE_INSTRUCTIONS_DEFAULT` (20,903 ch) | wrong — agent prompt | **set empty** |
| `tools` | `Vec::new()` | correct — no tool surface | pinned explicitly |
| `parallel_tool_calls` | `false` | correct | pinned explicitly |
| `output_schema` | `None` | correct | pinned explicitly |
| `output_schema_strict` | **`true`** | inert with no schema, but "strict" defaulted on is not a default to inherit silently | set `false` |

**Empty rather than a short instruction string**, and this is provable rather
than hopeful: `gpt-5.6-luna` is a `use_responses_lite` model, and on that path
(`client.rs`) the request's `instructions` field is `String::new()` regardless,
while `base_instructions` rides as a *prepended developer message only when
non-empty*. Empty therefore deletes the message and adds nothing. Recorded in
the code: if derivation ever moves to a non-lite model, `instructions: ""`
would reach the wire directly and needs re-checking against the provider.

One residue remains and is not the fork's to remove: on the lite path
`client.rs` prepends an `AdditionalTools` developer item **unconditionally**,
even with an empty tool list. It is a few tokens; it is upstream's shape.

Verified offline, from the payload the bridge actually builds
(`derivation_prompt`), not a fixture: instruction text under 1,000 chars, free
of three distinctive `BASE_INSTRUCTIONS_DEFAULT` markers, no tools, and input
exactly equal to the content being derived. A second test asserts those markers
still occur in the real agent prompt, so the first cannot pass vacuously.
Mutation: restoring `..Default::default()` fails with
`got 20903 chars`.

#### Projected cost model — **arithmetic, not measurement**

No live call was made this round. Per-call input, using B3's measured 4,392
fixed overhead, Phase A's measured 649-token mean content, and a generous
50-token allowance for the residual lite-path envelope:

| | before | after | factor |
|---|---|---|---|
| input tokens per derivation call | 5,041 | **699** | **7.2x cheaper** |

Applied to §6's runs at their specified sizes:

| Run | before | after |
|---|---|---|
| B1 — 3 compacts, H=40k | 982,026 | **174,414** |
| B2 — auth lane, H=12k | 95,178 | **17,022** |
| B3 — latency, H=40k (complete) | 327,342 | **58,138** |
| B4 — KV billing, H=25k | 200,808 | **35,812** |
| B5 — band eval, H=30k | 242,986 | **43,254** |
| B6 — `model_change`, no derivation | 2,960 | 2,960 |
| **all six** | **1,851,300** | **331,600** |

Two things follow. **B2, B4 and B5 come back inside a ~50k envelope**, and B3
as originally specified would now fit its 69k estimate — the estimate was
right about everything except the prompt nobody had measured. B1 remains large
because it is three compacts of a 40k history; halving it to 2 compacts at
H=25k lands near 87k.

These are projections from two measured constants. The first live call after
this change replaces them with a number.

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

### 5.5 The bound measured a misconfiguration — **withdrawn** (round 11)

Round 8 escalated "the compact-time derivation budget is insufficient": 62
calls x 1.660 s = 102.9 s against a 75 s drain budget, ceiling H ≈ 29,000.
**That escalation is withdrawn. The numbers were real; what they measured was
our own misconfiguration, not LHC.**

`codex-lhc-host/src/session.rs` built the SDK with `SdkMode::Manual`, in which
the instance seam's `poke` and `touch` are no-op closures (`sdk.rs`) — the
scheduler never runs. The onboarding docs say so plainly
(`01-core-concepts.md` §Host mode, `02-domain-design.md` §scheduler), and the
reference host constructs "always in background mode, regardless of caller
config" (`04-host-pi-lhc.md`). One wrong constant produced everything
downstream:

* the original `drain_settled` was correct and did nothing, because the
  scheduler was inert;
* "derivation never runs" was the right diagnosis; "call `work.drain` at
  compact time" was the wrong fix — it made the host do LHC's job, serially,
  at the one moment a user is waiting;
* so 62 calls landed in a single burst against a deadline, and the H ≈ 29,000
  ceiling followed arithmetically;
* the M1 idle pump was hand-rolling what background mode does for free.

**What is true now (measured offline, round 11).** With the capture session in
`SdkMode::Background`, on the same 60-turn fixture:

| | round 8 (Manual + compact-time drain) | round 11 (Background) |
|---|---|---|
| derivations during the session | 0 | **117** |
| inference calls **at compact time** | **117** | **0** |
| what the arm does at compact | drains, serially, against 75 s | waits for `drain_settled`, bounded and cancellable |

The compact no longer performs inference at all, so the 102.9-s-against-75-s
arithmetic has no subject. `DRAIN_TIME_BUDGET`, `DRAIN_BATCH_ITEMS` and
`DRAIN_MAX_BATCHES` are deleted along with the loop they bounded.

**What is still unmeasured, and must not be claimed.** B3's 1,660 ms/call is
the only live latency number this project has, and it was taken under the old
design *and* before P1 removed the 4,392-token agent prompt from every request.
It remains an upper bound on per-call latency. Nobody has measured:

* per-call latency on the corrected configuration;
* whether background derivation keeps up with a fast interactive session — the
  question the idle-pump work was groping at. Background mode drains after each
  intake commit rather than on an idle tick, which is strictly better, but
  "strictly better" is not "measured".
* what a real session's compact costs end to end now.

Those need a live run and none was authorised this round. **The honest position
is that the old bound is void and no new bound exists.**

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
| ~~**B3**~~ | **DONE** — real per-call latency (gap 2). Truncated by its own guard; see §3.5 | 40.5 k | 1 | 32 | **10** | **59,019** (44,800 cached) | **416** | **59,435** |
| **B4** | KV/prefix-cache billing: `cached_input_tokens` on the two turns after a compact | 25 k | 1 | ~20 | ~48 | 34 k | 9 k | **43 k** |
| **B5** | Live band-shape eval (`lhc_band_shape_eval_*`, built in Chunk 2a, never run) | 30 k | 1 | ~24 | ~58 | 40 k | 11 k | **51 k** |
| **B6** | `model_change` on a real mid-thread `/model` switch (FORK.md checkpoint) | 8 k | 0 | ~6 | 0 | 8 k | 2 k | **10 k** |
| | | | | | **~436** | **314 k** | **85 k** | **~399 k** |

**B3 actual: 59,435 tokens for the run + 4,403 for a one-call pre-flight =
63,838 total**, against a 69k authorisation and a 100k hard stop. Under both.

But the estimate was right only by accident of truncation. §3.5 shows a
*complete* B3 would have cost ~368k — **5.3x its estimate**. The whole §6 table
below is built on the same broken assumption (derivation input ≈ 1x H) and is
therefore **low by roughly 5x across every remaining run**. Corrected rough
totals, using the measured ~5,943 tokens/call:

| Run | old estimate | corrected |
|---|---|---|
| B1 (3 compacts, H=40k each) | 206 k | **~1.1 M** |
| B2 (auth lane, H=12 k) | 20 k | **~110 k** |
| B4 (KV billing, H=25 k) | 43 k | **~230 k** |
| B5 (band-shape eval, H=30 k) | 51 k | **~275 k** |
| B6 (`model_change`, no compacts) | 10 k | ~10 k (unchanged — no derivation) |

**As of round 8 none of B1/B2/B4/B5 was affordable as specified.**

**Round 10 removed the cause.** P1 (§3.6) drops the agent prompt from every
derivation request, projected 7.2x cheaper per call. Recomputed — arithmetic on
two measured constants, not a new measurement:

| Run | round-8 corrected | **post-P1 projection** |
|---|---|---|
| B1 (3 compacts, H=40k) | ~1.1 M | **174 k** |
| B2 (auth lane, H=12k) | ~110 k | **17 k** |
| B4 (KV billing, H=25k) | ~230 k | **36 k** |
| B5 (band-shape eval, H=30k) | ~275 k | **43 k** |
| B6 (`model_change`) | ~10 k | ~3 k |

**B2, B4 and B5 are back inside a ~50k envelope.** B1 stays large because it is
three compacts of a 40k history; 2 compacts at H=25k lands near 87k. The first
live call after P1 replaces these projections with a number.

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

1. **Only run B3 was live.** 10 real calls, 63,838 tokens. Everything else in
   §3 is deterministic offline callbacks through production entry points. The
   ≥3 real compacts (B1), the live auth-lane confirmation in a real session
   (B2), KV billing (B4), band-shape eval (B5) and `model_change` (B6) were
   **not run** — and §6 now shows four of them are unaffordable as specified.
2. **B3 itself is partial.** 10 samples, two of four call kinds; no
   `summarize_chunk_brief` or `summarize_tool_result` latency. One run, one
   network path, no variance data. §3.5 states the bounds conclusion across the
   full plausible range for exactly this reason.
3. **No compact was ever observed to complete on the live lane.** B3 was
   truncated by budget, so the end-to-end "real compact installs a real body
   derived by gpt-5.6-luna" event has still never been witnessed. That is B1.
4. **P1 is unverified on the wire.** §3.6 asserts the `Prompt` the bridge
   builds, offline. That the resulting HTTP request is correspondingly smaller,
   that `gpt-5.6-luna` accepts a request with no base instructions, and that
   derivation output quality is unchanged without them, are all **unmeasured** —
   no live call was authorised this round. The lite-path reasoning in §3.6 is
   read from `client.rs`, not observed.
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

**Rounds 8-9: yes, with three things to watch. B3 changes that to: yes for
short threads, no for the long ones the feature exists for — until §5.5 is
ruled on.**

That is a real change of answer and it comes from one measurement, so it is
worth being precise about what did and did not change.

What still earns confidence, unchanged: the compact arm is not a summariser
wearing LHC's name. The body comes from `lhc.compact()` through the typed view;
law 1 holds as structural equality under mutation; law 2 holds as a next-turn
property through the production token-status path; both ladders are entered by
tests that fail when their hooks are removed. And every path that cannot
complete honestly fails open rather than installing something plausible — B3
is itself another instance of that: the run blew its budget, derivation failed,
and the arm declined cleanly instead of installing a half-derived body.

What changed: I assumed the fail-open was a rare path. **It is the common path
for any thread over ~29k tokens of history.** A 40.5k-token history needs ~103 s
of serialised derivation against a 75 s budget (§3.5). So on a long thread, LHC
captures everything faithfully, then declines to compact and hands over to
native compaction — which is exactly the outcome the fork exists to replace.
Nothing breaks, nothing is lost, and the archive stays complete and rebuildable;
the feature just does not deliver its main benefit where it matters most.

For a thread under ~29k tokens it works as designed, and the capture and
recovery machinery underneath is sound (§4.2).

What I would watch, revised:

1. **Whether a compact actually installs.** The arm logs
   `NoReduction` / `DerivationFailed` / `timed out` on fail-open. On a long
   thread, expect it. That log line is now the single most informative signal
   about whether the feature is doing anything.
2. **Idle-tick rate against turn rate**, now with wall clock attached: a tick
   costs ~5.3 s of real inference and is single-flight. Fast bursts starve the
   pump and push everything onto the insufficient compact-time budget.
3. **The first upstream sync that actually conflicts** — also the first test of
   the regenerate-and-move-`BASE` step the series now depends on.

The one thing I would fix before relying on it for long threads: **§5.5.**

Round 10 took the cheapest of its three levers under instruction — derivation no
longer ships Codex's agent prompt (§3.6), projected 7.2x cheaper per call, and
three of the five unrun Phase B runs come back inside budget. **That is a cost
fix, and I am not claiming it is a schedule fix.** Whether smaller requests also
run faster — and therefore whether the 102.9 s vs 75 s gap narrows at all — is
unmeasured and needs a second live run. The other two levers, reworking the
budget and making derivation concurrent, remain untouched and are Lee's call.

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

| 8 | P1: restore `..Default::default()` on the derivation `Prompt` | `p1_derivation_prompt_carries_no_agent_instructions` FAILED: "derivation instructions must stay tiny, got 20903 chars — `Prompt::default()` puts BASE_INSTRUCTIONS_DEFAULT (20903 chars) here" | ✓ |

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
| `cargo test -p codex-core --lib lhc_inference_bridge` | **8 passed**, 0 failed (incl. 2 new P1 tests) |
| `cargo test -p codex-lhc-host --lib` | **45 passed**, 0 failed |

B3's scaffolding was removed after the run (§3.5), so
`lhc_inference_bridge.rs` and `session/tests.rs` are byte-identical to `HEAD`
and the tripwire above is the round-9 tree, unchanged by Phase B.

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

## 10. Derivation quality on `gpt-5.6-luna` — untested, needs carve-out

The derivation model is pinned (`LHC_DERIVATION_MODEL`) rather than following
the session's model. That is deliberate and it is what keeps auth symmetric:
derivation rides the same provider as the CLI's own inference, so there is no
state where the agent can infer but derivation cannot. Per-host the pin
differs — claude models for cc-lhc, grok for grok-build, ChatGPT for this
fork.

**What has NOT been tested: whether the derivations luna produces are any
good.** B3 proved the path works — 10 real calls, non-degraded bands, correct
receipt — but nothing has judged the *content*.

Two specifics for whoever picks this up:

1. **Read the actual thread SQLite files** and assess derivation quality
   directly: are smoothed prompts faithful (intent, constraints, exact
   identifiers preserved)? Do turn compressions keep what a later turn would
   need? Are chunk briefs accurate rather than plausible?
2. **Model history, for context — not a concern.** Derivation previously
   used `5.4-mini` for most work and `5.4` for the broader brief-band
   derivation. All four callbacks now share `gpt-5.6-luna`, which is stronger
   than `5.4-mini` and at least comparable to `5.4`, at close to mini's
   rate-limit cost (~$6/M output vs ~$4.50/M). So no derivation type is on a
   weaker model than before; the small-op lane is straightforwardly upgraded.
   There is no band to watch more closely than any other on that account.

The open question is therefore absolute, not relative: is luna's output good
enough, anywhere? Reasonable prior: fine, possibly better than what preceded
it. Not a substitute for looking.
