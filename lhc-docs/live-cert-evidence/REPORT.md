# Slice D Layer-3 Live Cert Report (scenarios 1–5)

**Date:** 2026-07-27  
**Binary:** `/srv/work/codex/codex-rs/target/release/codex` (`codex-cli 0.0.0`, release)  
**CODEX_HOME (isolated):** `lhc-docs/live-cert-scratch`  
**CODEX_LHC_ROOT:** `lhc-docs/live-cert-scratch/lhc`  
**Model:** `gpt-5.6-luna`, `model_reasoning_effort=low` (ChatGPT auth)  
**Workspace:** `lhc-docs/live-cert-workspace`  
**No production code patches.** Script-only harness under `lhc-docs/live-cert-evidence/common/`.

## PASS/FAIL table

| Scenario | Result | Evidence | Notes |
|---|---|---|---|
| **1** Tool-heavy → compact → resume | **FAIL** (rewrite path) / coherent resume **PASS** | `s1/` | LHC arm fail-open (`NoReduction`, then terminal `claim_expired` derivation). Native compact **appended** 4–6 `Compacted` records (old shape). **No `.prev`.** Resume recalled `BLUE-MARBLE-ORBIT-42` + markers. |
| **2** Second compact + resume | **FAIL** (generation rotation) / windows+coherence **PASS** | `s2/` | Windows **1→6 monotonic** via native append. No `.prev` rotation. Resume coherent (`BLUE-MARBLE-ORBIT-42`, `S2A`). LHC still blocked by terminal derivation failure. |
| **3** SIGINT mid-inference → compact → resume | **PARTIAL** | `s3/` | **LHC rewrite succeeded** (log + single `Compacted` + `.prev`). SIGINT turn `t2` closed with `outcome=None` (not `aborted`). Later `t5` has `outcome=aborted`/`Other`. Post-rewrite resume **failed** `ctc_` vs `fc_` id prefix. |
| **4** Old-format dual-format resume | **PASS** | `s4/` | Dual-format fixture resumed; model recalled `OLD-FORMAT-ANCHOR-77` + `LIVE-CERT-OLD-FORMAT` + secret. Next compact **rewrote** to single `Compacted` + `.prev` (`window_number=3`). |
| **5** Two-counters | **PASS** | `s5/` | Rollout newest `TokenCount.total_tokens` **268453** = LHC `sum(message.provider_usage.total_tokens)` **268453** (14 calls). Exact match. |

## Critical findings (slices A–D live)

### F1 — LHC compact often fail-opens under `codex exec` multi-process use
- **`claim_expired`** on `detailed_turn_compression` when the exec process exits mid-derivation → terminal failure →  
  `"lhc derivation failed: N derivation(s) failed terminally in background; refusing to compact raw fallback content"`.
- **`NoReduction`**: body not smaller than host estimate → fail-open.
- Consequence: native ladder (local/remote) **appends** `Compacted` → dual-format old shape, **no rewrite**, **no `.prev`**.

Evidence: `s1/compact_log_hits.txt`, `s1/lhc_pre_compact.txt` / post queries, `s2/compact_hits.txt`.

### F2 — When LHC rewrite *does* run, materializer emits `function_call.id` with `ctc_` prefix
API rejects resume/follow-up:
```
Invalid 'input[N].id': 'ctc_…'. Expected an ID that begins with 'fc'.
```
Blocks post-rewrite coherent conversation when tool calls are in the native tail.

Evidence: `s3/resume.stdout`, exploratory `common/exploratory/resume_after_compact.err`, rewritten tails with `ctc_*` ids.

### F3 — SIGINT does not reliably set LHC `turns.outcome=aborted` on the interrupted turn
- Interrupted user turn closed with `outcome=None`.
- An `aborted`/`Other` later appeared on a different turn (compact/API-error path).
- Timing fields on aborted row also incomplete (`started_at`/`ended_at` null).

Evidence: `s3/lhc_after_interrupt.txt`, `s3/lhc_final.txt`.

### F4 — LHC rewrite *can* work live (positive)
- S3 compact-trigger and S4 first growth both logged  
  `LHC rollout rewrite installed (atomic swap)` and retained `.prev`.
- S4 proves old-shape → new-shape transition on next compact.

Evidence: `s3/after_compact_rollout.jsonl`, `s3/rollout.prev`, `s4/after_rewrite.jsonl`, `s4/after_rewrite.prev`.

### F5 — Two-counters match when both populated from host provider_usage
- Not a discrepancy; equality held on the long s1 thread (native TokenCount stream).
- Note: derivation-lane luna spend is separate and not in these counters.

Evidence: `s5/comparison.txt`, `s5/token_count_from_rollout.txt`.

## Per-scenario evidence map

### Scenario 1 — `s1/`
| Artifact | Path |
|---|---|
| Driver log | `s1/driver.log` |
| Grow / compact transcripts | `s1/grow_*.stdout/stderr`, `s1/compact_trigger*.stdout/stderr` |
| Before/after rollouts | `s1/before_compact_rollout.jsonl`, `s1/after-compact-rollout.jsonl`, `s1/final_rollout.jsonl` |
| LHC queries | `s1/lhc_pre_compact.txt`, `s1/lhc_post_compact.txt` |
| Resume | `s1/resume.stdout` (coherent markers+secret) |
| Verdict | `s1/VERDICT.txt` |
| Prior native-append attempt | `s1_native_append_attempt/` |

Session: `019fa0fe-22df-7342-8afb-4c99d8bdca3f`

### Scenario 2 — `s2/`
| Artifact | Path |
|---|---|
| Windows before/after | `s2/windows_before.txt` (`1,2,3,4`), `s2/windows_after.txt` (`1,2,3,4,5,6`) |
| Grow + resume | `s2/grow.*`, `s2/resume.*` |
| Compact hits | `s2/compact_hits.txt` (fail-open only) |
| Verdict | `s2/VERDICT.txt` |

### Scenario 3 — `s3/`
| Artifact | Path |
|---|---|
| Interrupt stdout/stderr | `s3/interrupted.*` |
| LHC after interrupt / final | `s3/lhc_after_interrupt.txt`, `s3/lhc_final.txt` |
| Rewrite proof | `s3/after_compact_rollout.jsonl`, `s3/rollout.prev`, `s3/compact_hits.txt` |
| Resume (ctc_ fail) | `s3/resume.stdout` |
| Verdict | `s3/VERDICT.txt` |

Session: `019fa104-b573-79a2-b1c6-aa81e79737ff`

### Scenario 4 — `s4/`
| Artifact | Path |
|---|---|
| Fixture generator | `common/generate_old_format_rollout.py` |
| Installed old shape | `s4/old_format_installed.jsonl` (2 Compacted) |
| Live resume | `s4/resume1.stdout` (markers + secret) |
| Rewrite | `s4/after_rewrite.jsonl` (1 Compacted), `s4/after_rewrite.prev` |
| Trace | `s4/compact_trace.txt` |
| Verdict | `s4/VERDICT.txt` |

Session: `9b0fc5ce-1305-4f38-9224-f5119260afe4`

### Scenario 5 — `s5/`
| Artifact | Path |
|---|---|
| Rollout summary | `s5/rollout_summary.txt` |
| TokenCount extract | `s5/token_count_from_rollout.txt` |
| LHC dump | `s5/lhc_db.txt` |
| Comparison | `s5/comparison.txt` (**MATCH 268453**) |

## Environment / harness notes

- Isolated scratch only; user `~/.codex/sessions` untouched. Auth copied into scratch for ChatGPT plan.
- Scripts: `common/env.sh`, `common/run_until_lhc_rewrite.sh`, `common/generate_old_format_rollout.py`.
- `remote_compaction_v2` disabled in cert config to unmask LHC fail-open (otherwise native remote appends dominate).
- Compact threshold strategy: grow at `200000`, trigger at `3000` after settle waits — still hit `claim_expired` on multi-process exec for long threads.
- One exploratory mid-session rewrite also captured under `common/exploratory/`.

## Suggested follow-ups (not done here; no prod patches)

1. Re-queue or re-drive `claim_expired` derivation on resume so terminal failures do not permanently block LHC compact.
2. Materializer: map code-mode/`ctc_*` tool call ids to API-legal `fc_*` (or preserve original provider ids) before rewrite install.
3. Ensure SIGINT/abort path always closes the active turn with `outcome=aborted` + reason + host timestamps (slice A contract).
4. Consider longer `SETTLE_WAIT` or in-process multi-turn driver for live rewrite cert of long tool sessions.
5. Re-run layer-3 after F1–F3 fixes; S4 shape is the green dual-format reference path.

## Bottom line

| What worked live | What failed live |
|---|---|
| Dual-format old resume + rewrite to new shape (S4) | Reliable LHC rewrite on long multi-exec tool sessions (S1/S2) |
| Exact two-counter match LHC vs TokenCount (S5) | Post-rewrite resume with tool ids (`ctc_` bug) (S3) |
| Window numbers monotonic (even under native append) | Interrupt → `outcome=aborted` on the interrupted turn (S3) |
| Coherent resume of conversation content when history is API-legal | Generation retention (`.prev`) when native append path wins |

**Layer-3 cert status: not fully green.** S4 and S5 pass; S1–S3 expose production issues in live rewrite reliability, materializer tool ids, and abort capture — all findings against slices A–D, not harness bugs.
