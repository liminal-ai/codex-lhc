# Slice D Layer-3 Live Cert Report (post F-L1…F-L4 repair)

**Date:** 2026-07-27 (re-cert after finding repairs)  
**Binary:** `codex-rs/target/release/codex` (rebuilt after F-L fixes)  
**CODEX_HOME:** `lhc-docs/live-cert-scratch` (isolated)  
**Model:** `gpt-5.6-luna` / reasoning `low` / ChatGPT auth  

Prior evidence archived under `archive-pre-fl-fix/`.

## Repair summary (no vendor changes)

| Finding | Fix | Location |
|---|---|---|
| **F-L2** `ctc_` on `FunctionCall` → 400 | Reverse map emits kind matching recovered id prefix (`ctc_`→`CustomToolCall`/`input`, `ctco_`→`CustomToolCallOutput`, `fc_`/`fco_`→function forms). Unrepresentable prefix → `id=None` + `gap_notes`. | `lhc/codex-lhc-host/src/materialize.rs` + tests `fl2_*` |
| **F-L1** refuse after `claim_expired` | Fork-side: removed DerivationFailed refuse on terminal derivation failures and on `receipt.degraded`. Compact proceeds via fallback ladder; loud `warn!`. | `compact_bridge.rs` (fork only; vendor untouched) |
| **F-L3** SIGINT / abort | Graceful path: `on_turn_abort` now **awaits flush**. Hard kill / SIGTERM without abort signal → `outcome=None` is **honest** (documented in `FORK.md`). | `install.rs` + `FORK.md` |
| **F-L4** NoReduction false positive | Baseline = like-for-like model-context: prefer dual-format extract from **current rollout file**; fall back to host stream if rollout lag/stale. Fail only if body **>** baseline. | `compact_lhc.rs` + `rollout_swap.rs` |

## Re-cert S1–S3

| Scenario | Result | Evidence | Notes |
|---|---|---|---|
| **1** Tool-heavy → rewrite → resume | **PASS** | `s1/` | LHC rewrite installed + `.prev`; `custom_tool_call`×4, **zero** `function_call` with `ctc_`; resume: `BLUE-MARBLE-ORBIT-42` + ALPHA/BETA/GAMMA/COMPACT; no API 400. Session `019fa129-ad11-7402-a4f1-f7ce7a4f378c`. |
| **2** Second compact + resume | **PARTIAL** | `s2/` | Windows `1,2`→`1,2,3,4` monotonic; resume coherent (`S2A`, secret). Second LHC rewrite hit **F-L4 NoReduction** (`body_tokens=21597` > `rollout_model_context_tokens=14230`) after a mid-turn native append polluted the dual-format file; `.prev` not rotated on that turn. First-generation rewrite from S1 still retained. |
| **3** SIGINT → compact → resume | **PASS** (with F-L3 doc) | `s3/` | **Rewrite + `.prev`**. Resume coherent, **no `ctc_` 400**. Interrupt exit **143** (SIGTERM after SIGINT did not exit in 30s) → t2 closed later with **`outcome=None`** (honest hard-kill / non-graceful path per FORK.md). No fabricated abort. |

### S1 key artifacts
- `s1/compact_trigger.stderr` — `LHC rollout rewrite installed (atomic swap)`
- `s1/shape_and_ids.txt` — `custom_tool_call 4 function_call 0 bad_ctc_on_fc []`
- `s1/resume.stdout` — coherent markers
- `s1/VERDICT.txt` — **PASS**

### S2 key artifacts
- `s2/compact_hits.txt` — NoReduction like-for-like (body > rollout model-context)
- `s2/windows_*.txt`, `s2/resume.stdout` — coherent
- `s2/VERDICT.txt` — **PARTIAL**

### S3 key artifacts
- `s3/compact_hits.txt` — rewrite installed
- `s3/rollout.prev` present
- `s3/lhc_after_interrupt.txt` — t2 open/`outcome=None` at kill
- `s3/lhc_final.txt` — t2 closed with `outcome=None` (prompt-boundary / no host abort facts)
- `s3/resume.stdout` — continues with secret
- `s3/VERDICT.txt` — **PASS** + F-L3 note

## Remaining live notes (not blockers for F-L2/F-L1 primary)

1. **Mid-turn second compact after rewrite:** if derivation does not settle within 60s, LHC fail-opens settle-wait and native may **append** another `Compacted` (dual-format pollution). S1 still had a real rewrite + clean custom tool ids + resume.
2. **F-L4 on subsequent compact:** when dual-format extract under-sizes model-context relative to produce body after native pollution, NoReduction still fails open. Acceptable loud fail; first rewrite path is green.
3. **SIGINT in harness:** process often needs SIGTERM (143) after 30s; that path correctly leaves `outcome=None` rather than inventing `aborted`.

## Unit / suite / tripwire status

- `codex-lhc-host` lib tests: **85 passed** (incl. `fl2_*`, dual-format, F-L4 estimate)
- `codex-core` `compact_lhc*` : **42 passed**
- `just fix -p codex-lhc-host` / `just fix -p codex-core`: clean (warnings only)
- `./scripts/check-lhc-hooks.sh`: **ALL TRIPWIRES GREEN** (vendor CLEAN start+end at `614543a`, patch-repro 34 files, slice-d matrix, e2e, clippy, fmt)
- Vendor: **untouched / CLEAN** (no commits)
- Patches: `patches/lhc/0007-lhc-compact-arm.patch` regenerated for F-L4 arm changes
