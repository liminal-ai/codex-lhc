# Slice E Live Cert Report (startup reconciliation + S2 normalization)

**Date:** 2026-07-27 (Slice E finale)  
**Binary:** `codex-rs/target/release/codex` (rebuilt after Slice E)  
**CODEX_HOME:** `lhc-docs/live-cert-scratch` (isolated)  
**Model:** `gpt-5.6-luna` / reasoning `low` / ChatGPT auth  

Prior D3 re-cert remains under `s1/`–`s5/`. Slice E additions: `s2-slice-e/`, `s6/`, and updated S2 verdict.

## Slice E code (no vendor changes)

| Part | Change | Location |
|---|---|---|
| **1** NoReduction = pathology tripwire only | When rollout is **native-append-polluted** (`Compacted` count > 1), NORMALIZATION rewrite proceeds regardless of size comparison; loud `info!`. Pure single-boundary keeps F-L4 guard. | `compact_lhc.rs` + `rollout_reconcile::{is_native_append_polluted}` |
| **2** Startup reconciliation | MISSING / CORRUPT / STALE vs LHC compact point → materialize + atomic swap **before** history load; fail-open if thread unavailable. | `codex-lhc-host/src/rollout_reconcile.rs`; hooks in `compact_lhc::reconcile_rollout_before_history_load`, `thread_manager`, `app-server thread_processor` |
| **3** Live S2 + S6 | Re-run second compact (must land); delete rollout → resume → converse. | evidence `s2-slice-e/`, `s6/` |

## Live matrix (post Slice E)

| Scenario | Result | Evidence | Notes |
|---|---|---|---|
| **1** Tool-heavy → rewrite → resume | **PASS** (D3) | `s1/` | Unchanged; first rewrite green. |
| **2** Second compact + resume | **PASS** (Slice E) | `s2/` + `s2-slice-e/` | On polluted multi-`Compacted` s1 session: **NORMALIZATION rewrite** log + **atomic swap installed** (`items=109`). Resume coherent (`BLUE-MARBLE-ORBIT-42`). Mid-turn follow-up compact on clean single-boundary correctly hit F-L4 NoReduction (body slightly > baseline) — pathology guard retained. |
| **3** SIGINT → compact → resume | **PASS** (D3) | `s3/` | Unchanged. |
| **6** Delete rollout → resume → converse | **PASS** | `s6/` | File deleted → resume triggered **Missing** reconcile; log `regenerated rollout from thread` + `startup reconciliation completed before history load`. Regenerated shape: **single Compacted + native tail** (`SHAPE_OK`). Model recalled `BLUE-MARBLE-ORBIT-42`. |

### S2 key artifacts (Slice E re-run)
- `s2/compact_hits.txt` / `s2-slice-e/grow.stderr` — `NORMALIZATION rewrite` + `LHC rollout rewrite installed (atomic swap)`
- `s2-slice-e/resume.stdout` — coherent secret recall
- `s2/VERDICT.txt` — **PASS**

### S6 key artifacts
- `s6/reconcile_hits.txt` — `trigger=Missing`, `items=121`
- `s6/after_shape.txt` — `compacted_count=1` `SHAPE_OK`
- `s6/resume.stdout` — pre-compact secret recalled
- `s6/VERDICT.txt` — **PASS**

## Layer-2 (deterministic)

- `codex-lhc-host` `rollout_reconcile` tests: **12 passed**
  - classify MISSING / CORRUPT / STALE / ok / thread-unavailable
  - mutation demos: pollution detection; regenerate Missing/Corrupt/Stale
  - fail-open leaves file alone when thread unavailable
- `codex-core` `compact_lhc*` : **42 passed** (incl. slice D matrix)

## Unit / suite / tripwire status

- Vendor: **CLEAN** at `614543a` (untouched)
- Patches: regenerate `patches/lhc/0007-lhc-compact-arm.patch` for arm + reconcile hooks
- `EXPECTED_HOOKS=54` (+3 slice E history-load seams)
- FORK.md inventory rows 28–30 + Compact/NoReduction + Startup reconciliation sections updated
