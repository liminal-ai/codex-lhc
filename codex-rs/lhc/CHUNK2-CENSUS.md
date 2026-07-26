# Chunk 2a — Conversation-consumer census + fail-open inventory

**Status:** Chunk 2 pre-bridge deliverable (unit 21 of 22).  
**Date:** 2026-07-25.  
**Law 3:** enumerate every whole-conversation consumer outside the request
builder, and every fail-open path, *before* the LHC compact bridge lands.

Premise after write-back: LHC compact installs via
`Session::replace_compacted_history`, so **native host history becomes the LHC
body**. Consumers that ride post-write-back native state are fine. Consumers
that need pre-compact fidelity or a parallel LHC projection are design inputs.

---

## 1. Compact write path (shared by all arms)

| Site | Role |
|------|------|
| `session/mod.rs` `replace_compacted_history` | Assigns IDs → `state.replace_history` → persists `RolloutItem::Compacted{replacement_history}` (+ optional WorldState / TurnContext) → queues compact session-start hooks |
| `state/session.rs:114` `replace_history` | In-memory replace; **`auto_compact_window.clear_prefill()`** (law 2 observation) |
| `compact.rs:373` | Local arm install |
| `compact_remote.rs:284` | Remote arm install |
| `compact_remote_v2.rs:306` | Remote V2 arm install |
| `session/mod.rs` `start_new_context_window` (~3721) | TokenBudget / hard window reset install |

**Capture interaction (Chunk 1):** `replace_compacted_history` does **not** call
`record_conversation_items` / `send_raw_response_items`. Write-back is durable
in rollout as `Compacted`, not re-teed into the LHC raw-item stream. Chunk 2
bridge must define reconciliation (trust native = LHC after write-back, or
out-of-band map of the replacement).

---

## 2. Compact ladder (manual + auto)

### Manual — `tasks/compact.rs::run` (lines 27–78)

| Order | Guard | Arm |
|------:|-------|-----|
| 1 | `Feature::TokenBudget` | `compact_token_budget::run_manual_compact_task` → **early return** |
| 2 | remote provider + `RemoteCompactionV2` | `compact_remote_v2::run_remote_compact_task` |
| 3 | remote provider, no V2 | `compact_remote::run_remote_compact_task` |
| 4 | else | local `compact::run_compact_task` |

### Auto — `session/turn.rs::run_auto_compact` (~1012+)

Same arm order. Triggers: mid-turn token pressure, resume over limit, model
downshift / compaction-hash change.

**Placement implication for LHC arm (recommendation only — not implemented):**
see §5.

---

## 3. Whole-conversation consumers

### 3.1 Started list (verified)

| Consumer | Site | Reads | Native OK? | Notes |
|----------|------|-------|------------|-------|
| **Resume** | `thread_manager.rs` `InitialHistory::Resumed` (~874, 1122, 1164) | Persisted rollout → reconstruct | **Yes** post-writeback | Replays `Compacted.replacement_history` |
| **Fork — full history** | `tools/handlers/multi_agents_v2/spawn.rs` `SpawnAgentForkMode::FullHistory` (~66, 205); `agent/control/spawn.rs` | Parent model-context / rollout | **Yes**, highest risk | Child inherits compacted replacement as full history |
| **Fork — last N turns** | same, `fork_turns` numeric → `LastNTurns` | Rollout truncated by turn | **Yes** | `thread_rollout_truncation` |
| **Manual `/compact`** | `session/handlers.rs` → `CompactTask` | Ladder above | N/A (producer) | |
| **Auto-compact** | `session/turn.rs::run_auto_compact` | Ladder + token status | N/A (producer) | *Not* ~3672 in current tip; that area is `clone_history` / window helpers |
| **New / Cleared** | `InitialHistory::{New,Cleared}` | Empty history | **Yes** | No history |

### 3.2 Additional consumers audited this round

| Consumer | Site | Reads | Native OK? | Design input? |
|----------|------|-------|------------|---------------|
| **Rollout reconstruction** | `session/rollout_reconstruction.rs` `reconstruct_history_from_rollout` | Full rollout incl. `Compacted` | **Yes** (authority) | Band-shaped `replacement_history` must reconstruct byte-for-byte |
| **Apply reconstruction** | `session/mod.rs` `apply_rollout_reconstruction` + prefill estimate | Reconstructed native history | **Yes** | BodyAfterPrefix prefill from native |
| **Request builder (baseline)** | `session/turn.rs` sampling prepare / retry (~296, ~1206) | `clone_history().for_prompt` | **Yes** (desired) | Band shape must be model-tolerated |
| **Thread rollback** | `session/handlers.rs` ~440–541 | Live thread full history + re-reconstruct | **Yes** | Includes Compacted markers |
| **Code review** | `session/review.rs` `spawn_review_thread` | Does **not** fork history; review task runs on **parent Session** | **Yes** | Review model sees post-compact native history via normal turn path |
| **`/btw` / `/side`** | TUI `slash_dispatch` + `app/side.rs` → app-server fork | **Forked** parent history | **Yes** | Codex `/btw` is a **side-conversation fork**, not a side-channel recap; inherits compacted body |
| **Inter-agent replay** | reconstruction + `context_manager/history` | `InterAgentCommunication` → model items | **Yes** | Only post-compact items remain after write-back |
| **Guardian MCP review transcript** | `guardian/prompt.rs` ~115–119 | Parent `clone_history` | **Yes** | Judges compacted native transcript |
| **Guardian subagent fork** | `guardian/review_session.rs` | `load_rollout_items_for_fork` → `InitialHistory::Forked` | **Yes** | Snapshot of rollout-reconstructable history |
| **Multi-agent v1 spawn** | `tools/handlers/multi_agents/spawn.rs` | `fork_context` → FullHistory | **Yes** | Same risk class as v2 FullHistory |
| **Agent reload** | `agent/control/spawn.rs` `ensure_v2_agent_loaded` | Full model context → Resumed | **Yes** | |
| **Extension tools history** | `tools/handlers/extension_tools.rs` | `clone_history().into_raw_items()` | **Yes** | Extensions see compacted native |
| **Realtime context** | `realtime_context.rs` ~65–67 | `clone_history` (bounded ~1200 tokens) | **Yes** | Bounded by design |
| **Prompt debug dump** | `prompt_debug.rs` | `clone_history` | **Yes** | Debug only |
| **App-server resume / fork** | `app-server` thread_processor | Thread store / client history → InitialHistory | **Yes** | Paginated model context |
| **Memories stage-1** | `memories/write` prompts | Full **rollout file** text (truncated) | **Rollout** | Offline; sees Compacted lines, not live LHC view |
| **Token / auto-compact accounting** | `context_window.rs`, `recompute_token_usage` | Estimated tokens + prefill | **Yes** | Must untrip after write-back (law 2) |
| **LHC capture tee** | `send_raw_response_items` | Incremental raw items only | Parallel store | Not a full-history *reader* |

### 3.3 “Needs LHC view?” summary

Almost every consumer rides **native host state** after reconstruct/write-back.
True “needs LHC view” candidates for Chunk 2 design:

1. **Compact request serving** — the LHC compact arm itself (pre-write-back
   selection from LHC bands).
2. **Capture reconciliation** after write-back (parallel store vs native).
3. Speculative product choices: guardian / memories wanting pre-compact
   fidelity (today they use native/rollout — no change unless product says so).

Highest **shape-risk** consumers for band-shaped replacement (still native):

- Resume / reconstruction  
- FullHistory and LastNTurns forks (incl. `/btw`/`/side`)  
- Guardian transcript + guardian fork  
- Code review (same-session history)  
- Auto-compact re-trigger accounting  

---

## 4. Fail-open inventory (must fall back to a **bounded** body)

### 4.1 Chunk 1 capture (`codex-lhc-host`)

| Path | Fallback | Bounded? |
|------|----------|----------|
| Feature `LhcCapture` OFF | No open; raw-item no-ops | N/A — host-only |
| Lazy open race | Pre-open buffer then flush | Cap = `CAPTURE_QUEUE_CAP` (1024); overflow drops + warn |
| Queue full | Latch `degraded`, refuse further, try `runtime_note` | User slots 1023 + 1 note; **not** silent continuous loss after latch |
| Map panic | Drop item; worker continues | Per-item |
| Submit failures ×3 | `capture_disabled` + runtime_note | Thread-lifetime latch |
| Open / worker spawn fail | `open_failed` / degraded | No capture |
| Core contributor panic | `catch_unwind` in `send_raw_response_items` | Session continues |
| **Compact write-back (2b)** | Body **not** re-ingested; archive gets `lhc_compact_marker` runtime_note (`CompactReceipt`-shaped). Host history equals produced body (law 1). | Marker is one bounded note; body install hard-capped at 512 items |

Degraded latch: subsequent captures refused — loud truncated record (self-
describing note when possible). Fits law 3 for *session* path (never blocks);
LHC record is incomplete but marked.

### 4.2 Compaction (native + future LHC arm)

| Path | Fallback | Bounded? |
|------|----------|----------|
| Local compact context overflow | Drop oldest history item and retry while len > 1 | Shrinks until fit or fail |
| Local / remote stream errors | Provider backoff retries | Provider max retries |
| Remote model failure | Optional alternate model step context | One fallback if provided |
| Compact hooks Stopped | Abort; **no install** | Prior history unchanged (may still be over limit) |
| TokenBudget path | Hard reset to initial context via `start_new_context_window` | Fits window by construction |
| Legacy compact without `replacement_history` | Rebuild summary-shaped history from user messages | Best-effort; clears reference context |
| Install failure before write-back | Native history unchanged | Not bounded if already over limit → re-trigger |

**Law 3 for LHC arm (binding on 2b):** any LHC compact failure must either
install a body known to fit the window, or leave prior history and surface
loudly — never install an unbounded reconstruction of the full thread.

---

## 5. Ladder placement recommendation (do **not** implement in 2a)

**Recommendation: place the LHC arm *above* TokenBudget early-return, gated by
`Feature::LhcCapture` (and optionally “LHC compact ready” health), with
TokenBudget remaining as fail-open.**

```
if Feature::LhcCapture && lhc_compact_available(session) {
    run_lhc_compact(...);  // write-back via replace_compacted_history
    return;
}
if Feature::TokenBudget { ... early return }
// existing remote_v2 / remote / local
```

**Reasons:**

1. **TokenBudget early-return** (`tasks/compact.rs:36–38`) bypasses every arm
   below it. An LHC arm under TokenBudget would never run when that feature is
   on — the common “budget” product configuration would never exercise LHC.
2. LHC write-back is the same *kind* as TokenBudget’s
   `start_new_context_window` (replacement install) but preserves band-shaped
   content; TokenBudget wipes to scaffolding. Preferring LHC when capture is
   healthy better matches law 1 (write-back architecture with history).
3. **Fail-open:** if LHC open failed / degraded / list_events refuses, fall
   through to TokenBudget → remote → local — each of those already produces a
   bounded body or leaves history unchanged.
4. Do **not** place LHC only as a peer of local: remote_v2 would still win on
   ChatGPT providers and skip LHC.

**Needs Lee if:** product wants TokenBudget to always win over LHC (different
UX: hard reset vs band preserve). Default above assumes band preserve wins
when capture is healthy.

---

## 6. Design inputs that change the bridge (from this census)

1. **Capture does not re-ingest Compacted** — bridge must define store
   reconciliation after write-back.
2. **TokenBudget early-return** — LHC arm placement must sit above it or be
   unreachable under budget mode.
3. **Fork / `/btw` / FullHistory** inherit replacement history — band shape is
   load-bearing for multi-agent and side conversations, not only main-thread
   sampling.
4. **Reconstruction treats `replacement_history` as authoritative** — fixture
   shape for goldens must match what forks/resume install.
5. **Law 2 is coverage, not design risk** — prefill clear is already in
   `replace_history`; still need threshold-untrip test (native-path test
   possible without LHC arm; full “no re-trigger next turn” after LHC arm
   deferred to 2b if it needs the arm).

---

## 7. Hook / touchpoint delta this round

**No new core LHC-HOOK touchpoints.** Census + harness only.  
`EXPECTED_HOOKS` remains **30**. FORK.md inventory unchanged for hooks.

---

## 8. Related deliverables

| Artifact | Path |
|----------|------|
| Band-shape harness | `core/src/session/lhc_band_shape_eval_tests.rs` + host `band_shape.rs` |
| Runbook | § in this file below / harness module docs |

### Band-shape harness command (do not run live without Lee)

Dry-run (no model; always safe):

```bash
cargo test -p codex-core --lib lhc_band_shape_eval -- --nocapture
```

Live model eval (spends quota — **Lee sign-off required**):

```bash
CODEX_LHC_BAND_EVAL=1 CODEX_LHC_BAND_EVAL_OUT=/tmp/lhc-band-eval.jsonl \
  cargo test -p codex-core --lib lhc_band_shape_eval_live -- --nocapture --ignored
```

**Per-run cost (live):** default **2** continuation turns after install; tiny
band history (~4–8 messages, order of **2–4k tokens** total prompt per turn).
Approx **~5–10k tokens** total for the default run (history + 2 turns). Not
free on ChatGPT plan quota.
