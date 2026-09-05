# Fork maintenance history through 2026-09-04

Extracted from FORK.md at `b27db354c6`. These dated accounts and old status
claims are historical evidence, not current policy or qualification. Current
maintenance requirements live in [FORK.md](../../FORK.md). Source contents are
preserved, including historical paths, headings, and superseded observations.

### Sync run 2026-09-04 — exact stable `rust-v0.153.3` (codex-lhc 0.153.3)

Merged annotated tag `rust-v0.153.3` (peeled commit `b1a547b1f7`) onto the
v0.150.2 release head `de0317e2a4` (workspace `0.150.2`) with a real
two-parent `--no-ff` merge; merge-base `076f17c114`, 351 upstream commits.
Purpose: a 0.153 client for `gpt-6-astra` (OpenAI's backend refuses it on
0.150.2). Sync only — no new LHC features; vendored SDK pin unchanged at
`5207952`.

Nine conflicts, upstream as base with the fork's hooks re-applied on top:
`Cargo.toml` (take `0.153.3`); `core/src/session/session.rs` and
`session/tests.rs` x2 (upstream `executed_tool_calls: ….clone()` + fork
`lhc_test_inference` initialiser); `core/src/session/mod.rs`
(`replace_compacted_history`: upstream `let mut compacted_item` for the new
guardian checkpoint, fork keeps `metadata.message.clone()` because the I2
durable-record check reads the message afterwards);
`protocol/src/openai_models.rs` (fork `auto_compact_token_limit` policy and
its two tests kept; upstream's new
`model_context_window_limits_preserve_their_distinct_meanings` test kept with
the fork-policy expectation `250_000` instead of the 90% clamp `244_800`);
`core/src/session/turn.rs` (LIM-134 `Err(err) => return Err(err)` kept over
upstream's new in-loop `InvalidImageRequest` / generic emit+break arms, so
the finalizer stays the sole terminal contributor — upstream's friendly image
message is therefore not emitted in-loop; stream args moved to
`step_context.settings.*` with the LC Adaptive Service Tier resolver now fed
`step_context.settings.service_tier` and its decision still passed to
`stream`; `response_tool_call_ids` seam kept beside upstream's new
`reasoning_effort` tracing string; `record_observed_response_completed` takes
upstream's `(response_id, usage, usage_metadata)` form);
`core/src/tasks/mod.rs` (`handle_task_abort` gains upstream's
`turn_state: &Mutex<TurnState>` parameter and keeps the fork's
`(started_at, completed_at)` return; `run_turn_interrupt_hooks` gets the
turn state in the fork's `TurnTerminal::Abort` arm);
`core/tests/suite/compact.rs` (LIM-142 ignores re-applied: upstream
parameterised `summarize_context_three_requests_and_instructions` and renamed
`manual_compact_emits_api_and_local_token_usage_events` →
`manual_compact_records_durable_and_local_token_usage`, allowlist entry
renamed to match; upstream's new
`previous_model_compaction_resolves_selected_settings` inserted — see the
classification note below);
`thread-store/src/local/thread_history_materialization.rs` (fork
generation-swap projection kept; the rollout is now opened through upstream's
`open_rollout_seekable_reader` in `spawn_blocking` so compressed
`.jsonl.zst` rollouts project from their logical JSONL bytes, a new
`ProjectionRead::Missing` arm mirrors upstream's `NotFound && start_offset == 0`
early return, and the exists-check uses `existing_rollout_path`).

Hook inventory unchanged: 53/53 markers, identical per-file distribution to
v0.150.2 (no hook site moved). Adapter/fork absorptions of upstream API
changes, no semantic change: `ToolExecutor<ToolCall<'call>>` HRTB on the
retrieval tools and `ToolContributor::tools`; `CompactedItem` gained
`guardian_history`, `compaction_response_id`, `latest_token_usage_record`
(LHC boundary items carry `None` — the boundary has no guardian checkpoint or
compaction response, and token usage is re-observed live after the rewrite
install); `AgentMessageEvent.questions`, `ErrorEvent`/`TurnError.misalignment`,
`ThreadSettingsAppliedEvent.thread_id`; `RolloutItem::TokenUsageRecord` joined
the fork's rollout-head scan; `TurnContext.model_info` became a method (tests
mutate via `update_turn_settings_for_test`); `SessionSettingsUpdate` nests
`step_settings`; `enqueue_mailbox_communication` takes `TurnStartOptions`;
`ReasoningEffort::Persistent` ranks above `Ultra` in the derivation-effort
ladder; `Prompt.cyber_access_program = None` on the derivation prompt;
`install_compacted_history_memory` passes `HistoryReplacement::Compaction` so
the guardian review transcript is retained exactly as the native compaction
arm does (in-memory only — the raw-item hook is not on this path; no
double-record).

**Rollout compression (upstream 0.153).** A background worker compresses
rollouts idle for 7 days to `.jsonl.zst`; resume/append materialises them
back to plain before use. The fork's startup reconciliation classified on the
plain path, so a compressed-but-intact file would have read as MISSING and
been regenerated from the LHC record. `reconcile_rollout_before_history_load`
now materialises a compressed rollout (upstream's `materialize_rollout_for_reference`)
before classification, exactly what upstream's own resume path does one step
later. Live rollouts (the rewrite/swap path) are never compressed.

**`features.context_management.experimental_mode`** (token budget, history
notes, `new_context` tool; default off) is not wired into any fork seam; the
LHC arms sit above TokenBudget in both ladders as before. Behaviour with it ON
is a slice-2 live finding, not a merge-time change.

Release identity: workspace `0.153.3` (upstream), `lhc-release/VERSION` and
`ALIGNED_VERSION` in the release-helper test moved `0.150.2` → `0.153.3`.
`patches/lhc/BASE` advanced to `b1a547b1f7`; all seven patches regenerated.
`Cargo.lock` regenerated by cargo. Test counts and tripwire summary: see the
merge commit body and the campaign STATUS.md done bar.

### Drill run 2026-08-27 — exact stable `rust-v0.150.1` (LIM-132)

Merged annotated tag `rust-v0.150.1` (tag object `0eb410ad0d`, peeled commit
`9085439396`) onto product base `673df1bf08` (workspace `0.149.2`, public tag
`v0.149.2`) with a real two-parent `--no-ff` merge; the second parent is the
peeled commit. `rust-v0.149.1` is not an ancestor of `rust-v0.150.1`
(release branches); merge-base `2584e88cad`, 208 upstream commits.

Twelve conflicts. Content: `Cargo.toml` (take `0.150.1`, keep the LHC
workspace member); `features/src/lib.rs` (keep `Feature::LhcCapture`);
`models.json` (keep fork windows `370000/1050000/350000` on gpt-5.6-*, take
upstream `model_specialty`); `core/src/tasks/mod.rs` (upstream
`run_turn_interrupt_hooks` on interrupt + fork abort timing into
`emit_turn_abort_lifecycle`); `core/src/tasks/compact.rs` and
`core/src/session/turn.rs` (strict LHC manual/auto ladders kept; upstream
`Session::responses_metadata` taken with the F2 `begin_cycle` seam after it);
`core/src/session/mod.rs` (upstream memory-pollution check on unpaired
outputs + fork provenance fan-out); `thread_history_materialization.rs`
(upstream `is_inherited_subagent_history` computed from the fork's rollout
`head`); `state/src/migrations_tests.rs` (both tests kept). Add/add or content
conflicts where the 0.149.1 release-branch image-budget backport met upstream
main (`compact_remote_v2_images.rs`, `compact_remote_v2_image_budget_tests.rs`,
`core/tests/suite/compact_remote.rs`) were taken from upstream verbatim — the
fork side was byte-identical to the `rust-v0.149.1` tag.

**Thread-history migration renumber.** Upstream 0.150 added
`0005_thread_realtime_items.sql` and `0006_thread_turn_ends.sql`; the fork's
`0005_rollout_generation_id.sql` moved to version **7** (same SQL, same
checksum). Existing fork databases carry the generation-ID migration as
version 5, which would fail upstream's version-5 checksum comparison, so
`repair_legacy_rollout_generation_migration_version` (mirroring upstream's
version-38/39 recency repair) relabels that row to 7 before the thread-history
migrator runs and upstream 5/6 apply beneath it. Pinned by
`repairs_rollout_generation_migration_that_was_applied_as_version_5`.

Adapter absorbed upstream API changes without semantic change:
`ResponseItem::FunctionCallOutput.call_id` is now `Option<String>` with new
`name`/`namespace` fields (unpaired named outputs). Capture maps a `None`
call id to a synthetic id exactly as `ToolSearchCall` already does; the
closed `tool_result` payload has no slot for `name`/`namespace`, so they are
not carried (recorded gap — no in-process producer emits such items today) and
materialize emits `name: None, namespace: None`. `ToolOutput::log_preview`
became `log_output`; `ToolCall` gained `source` (test fixtures);
`RolloutItem::RealtimeItem` joined the fork's rollout-head scan.

Release identity: `lhc-release/VERSION` and the release-helper alignment
test moved `0.149.2` → `0.150.1` so `check_version_identity` stays aligned
with the merged workspace; release notes, workflow defaults, and any
`0.150.2` identity remain with the release story. `patches/lhc/BASE` advanced
to `9085439396`; all seven patches regenerated. `Cargo.lock` regenerated
(the auto-merge left it inconsistent). Vendored SDK pin unchanged at
`b408f89`.

### LIM-134 — PreTurn readiness, prompt preservation, lifecycle, and exec truth (2026-08-28)

Landed on the LIM-142 descendant of `rust-v0.150.1`. A resumed Turn that
requires PreTurn compact waits for capture `Ready`/`Failed`/`Stopped`
(bounded, cancellable); native prompt is recorded exactly once; terminal
contributor selection emits exactly one of stop, abort, or error; exec
human/JSONL paths fail a completed nonblank prompt with no nonblank
`AgentMessage`. Patch `0007` was regenerated at `BASE` `9085439396` to cover
four newly fork-owned non-host files (`core/src/compact_lhc_readiness_tests.rs`,
`core/tests/suite/lhc_preturn_readiness.rs`, `exec/src/lib.rs`,
`exec/src/lib_tests.rs`, `exec/src/event_processor_with_human_output_tests.rs`). Host
capture-lifecycle sources remain under
`codex-rs/lhc/` (not patched). No new `LHC-HOOK` sentinel; `EXPECTED_HOOKS`
stays 53. Patches 0001–0006 are byte-identical to the LIM-132 series.

Known delta (accepted 2026-08-28): post-dispatch sampling errors now return
`Err` so the task finalizer is the sole terminal contributor; the
`InvalidImageRequest` arm's custom "Invalid image in your last message..."
message + `BadRequest` info is replaced by the generic finalizer error event.
Restoring the custom message is future cosmetic scope, not LIM-134 rework.

### Drill run 2026-08-25 — exact stable `rust-v0.149.1` (turn parts, Story 5)

Merged annotated tag `rust-v0.149.1` (peeled `ff29a44391`) onto product base
`a25a81a8d7` (workspace `0.149.0`). Two content conflicts: `Cargo.toml` (take
`0.149.1`, keep the LHC workspace member) and `features/src/lib.rs` (keep
`Feature::LhcCapture` default-on, take upstream `CompactionImageBudget`).
`core/config.schema.json` and `core/src/session/tests.rs` auto-merged and were
re-verified (`just write-config-schema`). `patches/lhc/BASE` advanced to
`ff29a44391`; all seven patches regenerated (drift-only deltas). `Cargo.lock`
regenerated. Vendored SDK advanced `9d4d182` → **`713b38d`** (accepted Rust
turn-parts source, since published on LHC `origin/main`).
Adapter absorbed two SDK shape additions without semantic change:
`ViewCompactParams.newest_closed_protection` (left `None`, profile default) and
`MessageRecord.step_index` in materialize test fixtures. `just fmt` reformats
the vendored crate and two Python scripts; that churn was reverted (F12).

Slices 2–3 of the same run: F2 step stamping (hook 13b, markers 52 → 53) and
the turn-parts MidTurn arm with typed-only forced-boundary coexistence (see
"Turn parts MidTurn"); patch `0007` regenerated for the fork-owned core files
touched, tripwire layer 2a2 added.

### Release preparation 2026-08-26 — `v0.149.2`

The unpushed release candidate aligns the workspace and release identity at
`0.149.2` and pins the exact accepted SDK `b408f89712cbbb525dbfc2f7b2c51ab3133c4f45`.
The final SDK delta is TS-only, and its `packages/lhc-rs` tree is unchanged
from parent `13573a1`. It packages the inherited post-close Full-tail repair
described under "Turn parts MidTurn" without changing host behavior, release
machinery, archive layout, installer behavior, platform matrix, or thresholds.
Thread schema remains 12 with no new migration. Provider-pressure crossing
remains unestablished; no new provider crossing is claimed.

### Drill run 2026-08-23 — exact stable `rust-v0.149.0`

Merged tag `rust-v0.149.0` (`758ef40f50`) onto accepted local baseline
`7ebcfc7f6a` (workspace version `0.148.0-alpha.20`). 0.148-alpha.20 is **not**
an ancestor of 0.149.0; merge-base is `b3cc217378`. Content conflicts:
`Cargo.toml` (take `0.149.0`, keep LHC workspace member), `core/src/lib.rs`
(keep LHC modules + take 0.149 network/MCP re-exports), `models.json` (keep
fork compact windows `370000/1050000/350000` on gpt-5.6-*), `Cargo.lock`
(regenerate from 0.149 + LHC host). Pre-pin source SHA `95879adda9` kept SDK
pin **`f4de85c`**. Host adapter gained 0.149 `CompactedItem.mcp_resource_origins`
and `AgentMessageEvent.delivery` so materialize still compiles. Isolated
remote-control homes seed running fork bytes (bead 3i7); stock
`chatgpt.com/codex/install.sh` is not used. Platform docs record published
Linux/Windows/macOS artifacts (5v7.2). `8y8` left for release-prep.

Fable accepted `95879adda9` as the clean pre-Rust-pin Codex 0.149 candidate,
then authorized one integration commit: advance the vendored gitlink
`f4de85c` → accepted local Rust candidate **`9d4d182`**. Default Smart Compact
is bounded metadata-first; `LHC_COMPACT_ALGORITHM=legacy` remains selectable.
Patches/BASE stay at `758ef40f50` (vendor lives under `codex-rs/lhc/`, outside
the patch series). Do not push the LHC or Codex repos for this pin.

### Drill run 2026-07-26 (Chunk 3 / C2) — clean, and thinner than intended

`322d5b96cf..61a44880a8`: **3 commits, 9 hours**, merged by `ort` with **zero
conflicts**; no hook file touched. Tripwire on the merged tree: 12/13 green
(layer 13 red for the pre-existing reason below). Nothing broke, so nothing was
resolved — **this exercised the procedure, not the conflict resolution.** Run on
a branch off `lhc` in a separate worktree (Chunk 3 was not allowed to commit).

**That drill's green verdict did not cover the patch base, and could not
have.** It ran at 03:54; `patches/lhc/BASE` was added at 05:39 (`ed22b96375`),
1h45m later. The rehearsal therefore ran against a `patch-repro` with no
fixed-base concept, so base-drift was invisible to it by construction. When
the same upstream range was merged for real on `lhc`, `patch-repro` failed
exactly there. Step 4 above is that missing step. Recorded because the drill
looked like evidence the sync was clean end-to-end and was not — a rehearsal
only covers the checks that existed when it ran.

Real exposure, measured on upstream over 30 days rather than assumed:

| File | upstream commits | commits touching the fork's own line ranges |
|---|---|---|
| `core/src/session/mod.rs` | 66 | **1** (of 13 hook sites) |
| `core/src/session/turn.rs` | 35 | 1 (of 2) |
| `core/src/tasks/compact.rs` | 2 | 1 (of 1) |
| every other hooked file | ≤ 20 each | 0 |

3 of 26 hook sites saw any churn in a month. The tiny-footprint mitigation was
then tested by the ten-day sync below.

### Sync run 2026-08-06 — 350 upstream commits, six conflicts, green

Merged `61a44880a8..aac9f84247` after ten days of upstream work. Six files
conflicted: `Cargo.lock`, `core/src/lib.rs`, `core/src/session/{mod.rs,session.rs,tests.rs}`,
and `ext/extension-api/src/registry.rs`. The source conflicts were additive or
upstream refactors around LHC touchpoints; `Cargo.lock` was regenerated.

Upstream changes that required LHC work:

- `ExtensionRegistryBuilder` now owns a registry directly; the raw-item
  contributor moved into that shape. This removed two marker sites without
  removing behavior, so the sentinel count changed from 54 to 52.
- conversation-item preparation now returns image-preparation metadata;
  provenance capture preserves that new analytics path.
- `SessionTaskContext` was removed, so compact-arm tests now call the production
  task interface directly.
- lifecycle and usage inputs gained `extension_metrics` and
  `codex_rollout_budget_units` fields.
- function calls gained `encrypted_function_args`; LHC now stores the optional
  value in `arguments.__hostEncryptedFunctionArgs` and restores it, including
  the meaningful `Some(empty)` case.
- image-generation completion gained transparent-background metadata; legacy
  LHC materialization supplies `None` because that field is not in the LHC
  record.

The vendored Rust port advanced `614543a..a3deafd` (latest `lhc-rs` source
change: band-walk brief-fallback repair). All tripwire layers passed, including
98 adapter tests, the real Session seam, compact arm, certification, schema,
patch reproduction at the new base, and the slice-D matrix.

### Sync run 2026-08-12 — LIM-40 (~13 commits)

`16fbfe55..1ad43978` class: merged `upstream/main` into `lhc`. Conflict:
`features/src/lib.rs` — kept `Feature::LhcCapture` (fork default on) and
upstream `RetainClientDeveloperMessages`. `patches/lhc/BASE` advanced to
`1ad4397821…`; full series regenerated. Tripwire in following commit.

### Sync run 2026-08-12 — 108 upstream commits, two conflicts, green

Merged `3aae5d885b..16fbfe5574`. Conflicts were limited to `core/src/lib.rs`
(upstream removed `config_lock`; LHC retained its inference bridge module) and
`core/src/tasks/mod.rs` (upstream simplified task abort; LHC retained the
finalized timing tuple required by lifecycle capture).

Upstream moved persisted rollout records from `codex-protocol` into the new
`codex-history` crate and wrapped response items in metadata envelopes. The LHC
host, compaction write-back, resume/materialization paths, and their tests now
use that typed boundary. The final upstream tail also renamed rollout writer
deferred state; the LHC reopen hook follows the new `deferred_creation` field.

All tripwire layers passed at the new base, including default bare-exec
durability, host tests and certification, the real Session seam, compact arm,
38-file patch reproduction, and the slice-D matrix. The canonical SDK pin was
also corrected to the actual certified `7062814` gitlink.

### Verified 2026-07-26

Drill run at `322d5b96cf`: all 7 patches applied, **26 of 26 fork-owned core
files byte-identical** to the working tree (0 differ, 0 missing; `Cargo.lock`
excluded by policy), and the reconstructed tree **compiles**
(`cargo check -p codex-core -p codex-app-server -p codex-extension-api`).

### What was wrong before (round 9 fixed all four)

Recorded because the failure mode — a series regenerated piecemeal against
whatever `HEAD` happened to be — is easy to re-introduce.

- **R1** No single valid base: `0004` needed `session/mod.rs` at the Chunk 1
  upstream base, `0007` needed Chunk 2a's, and the upstream drift between them
  was in no patch. *Fixed:* whole series regenerated from `patches/lhc/BASE`.
- **R2** `0006-app-server-install` matched no fork commit and restored the
  Chunk-1-era `include_str!` registration test a later round had replaced.
  *Fixed by the same regeneration.*
- **R3** Four fork-owned core files were in no patch, all compile-critical:
  `core/src/state/service.rs`, `core/src/session/session.rs`,
  `core/src/session/tests.rs`, `core/src/session/lhc_band_shape_eval_tests.rs`
  (whose `mod` declaration *is* inside `0007`). *Fixed:* added to `0007` and to
  the inventory as rows 20-23; layer 4 now fails on any uncovered fork-owned
  file.
- **R4** No gate caught it: layer 4 tested `0007` alone, applied to `HEAD`, so
  it could only ever be green pre-commit — it had been red since `3aa3a44d22`.
  *Fixed:* layer 4 now runs the whole drill at `BASE` and checks coverage.

**Rule:** any change to a fork-owned core file regenerates the series in the
same commit — as does any upstream sync that moves `patches/lhc/BASE`.
Regenerate with `git diff $(cat patches/lhc/BASE) -- <files for that patch>`
after `git add -N` (so untracked fork-owned files appear), keeping the
one-file-one-patch partition.

## Scheduled verification

| Item | Checkpoint |
|------|-----------|
| Mapping goldens (byte-eq + LhcSession round-trip) | Chunk 1 — DONE (rule zero) |
| Capture→rebuild diff | Chunk 2b — tripwire runs `compact_bridge` + `compact_lhc` (marker + law1/2) |
| Rollout rewrite certification (slice D) | Drill + dual-format + display consumers + layer-2 matrix in `compact_lhc_slice_d_tests.rs`; tripwire layer 5 runs the drill |
| LHC compact arm + write-back | Chunk 2b — **done**: body from `lhc.compact` + view map; derived provenance is **assigned host ids** co-written on `CompactedItem` (`lhc_compact_durable`); model-visible LHC note is small/constant (no digests); NoReduction falls open |
| Derived provenance cap | Process slot capped (`SESSION_DERIVED_CAP=512`, drops logged); archive summary notes do not carry digests. **Fork limit:** durable record is last CompactedItem message; forks that drop that item lose reseed — refuse/fail-open. |
| Real-session item-shape vs hand fixtures | Chunk 3 — **still open**: no live model call was made; Phase B blocked on budget ruling (`CHUNK3-CERTIFICATION.md` §6) |
| M1 core-level measurement (Chunk 2 gap 1) | Chunk 3 — **SETTLED**: pump moves 117/117 derivation calls off compact time; `remaining` is a cascading counter, not a progress metric (§3.1) |
| Real per-call latency (Chunk 2 gap 2) | Chunk 3 — **half**: input size measured (1.95 calls/turn, mean 649 tok/call, total input ≈ 1× history). Latency still needs Phase B run B3 |
| KV / prefix-cache impact | Chunk 3 — **MEASURED**: a compact invalidates 100% of the prefix (0 of 37,079 body tokens reusable). Billing confirmation is Phase B (§3.4) |
| Turn cancellation reaching the LHC compact arm | **Open with Lee** — `CompactTask` binds `_cancellation_token` (upstream's shape); derivation keeps spending after abort (12 calls 500 ms later). §5.2 |
| Band-shape tolerance eval harness | Chunk 2a — **built, not live-run** (`lhc_band_shape_eval_*`); live needs Lee auth-lane |
| Conversation-consumer census (law 3) | Chunk 2a — `codex-rs/lhc/CHUNK2-CENSUS.md` |
| Auth-lane ruling | **Open with Lee** — ChatGPT plan is the only available lane; live band-eval spends quota |
| Upstream-PR candidacy of `RawItemContributor` | after Chunk 3 |
| Full data-URL policy (URLs live in `payload.text` / `__hostRaw`; TextPayload is closed) | Chunk 2 if size issues |
| `model_change` / `thinking_level_change` free-seam wiring (`ConfigContributor`) | Chunk 1 — DONE offline |
| `model_change` fires on a real mid-thread model switch | Chunk 3 live cert (TUI `/model` path not proven offline) |
| `MODULE.bazel.lock` refresh for the LHC dependency change (`AGENTS.md:37`) | Run on a Bazel-capable host before any upstream PR or CI run |
| Module split + change-set decomposition before upstream PR candidacy | Deferred (H16) — after Chunk 1 accepted |
| ModelOutput vs HostContext vs InterAgent tag fidelity | **Unverifiable by behaviour today** — mapping collapses them for every variant (only `UserPrompt` vs rest is observable on user-role). Stream/compact tags are present in code; Chunk 3 live cert or a future payload field if the distinction must be durable. |
| Compaction `OutputItemDone` provenance path | Same collapse as above; not separately e2e-driven (same mapper). |

### Correction to the Chunk 1 record (2026-07-25)

Chunk 1's commit body (`86e9873220`) describes its dual-verify rounds as
having produced *independent* confirmation. **That claim is too strong.** Both
lanes were launched concurrently with cwd `/srv/work/codex` — one working tree
— across both verify rounds and the confirmation round, which the onboarding
doc's §"Verifier isolation — MANDATORY" forbids precisely because verifier
mandates include mutation testing. It demonstrably interfered here: a
confirmer found a cached certification binary carrying the *other* lane's
probe symbol (`confirmation_probe_extra_reaches_stored_row`), present in no
source file.

Per that rule, those rounds are **"corroborated", not "independently
confirmed"**. The substance stands: findings were traced to source, and the
orchestrator independently re-derived the decisive ones (envelope `extra`
rejection against `intake_stream/internal/validate.rs:359-372`; hook-body
deletion failing both core e2e tests; `codex_lhc_host::install` removal
failing the registry test; the provenance call sites in
`stream_events_utils.rs` / `compact.rs`). Those are orchestrator
measurements, not lane measurements.

Not amended in place: rewriting `lhc` history is reserved for the
history-reset drill with Lee's sign-off. Corrected forward here instead.
From Chunk 2 on, lanes are isolated via `scripts/verify-isolated.sh`.


### The drain correction (Chunk 3, round 11)

**LHC drains its own work. We had it in the wrong mode.**

`codex-lhc-host/src/session.rs` constructed the SDK with `SdkMode::Manual`.
In `Manual` the instance seam's `poke` and `touch` are no-op closures
(`sdk.rs`), so the scheduler is inert and nothing is ever scheduled. The
onboarding docs are explicit — `01-core-concepts.md` §Host mode,
`02-domain-design.md` §scheduler — and the reference host, pi-lhc, constructs
"always in background mode, regardless of caller config" (`04-host-pi-lhc.md`).

One wrong constant produced the whole causal chain the previous rounds chased:

- the original `drain_settled` call was **correct**; it did nothing because the
  scheduler was inert;
- diagnosing that as "derivation never runs" was right, but the fix — call
  `work.drain` at compact time — made the host do LHC's job, serially, at the
  worst possible moment;
- hence 62-447 calls in one burst, 102.9 s against a 75 s budget, and the
  H ≈ 29,000 ceiling. **Those numbers measured our misconfiguration, not LHC.**
- the M1 idle pump was hand-rolling what background mode does for free.

**Now:** the capture session is `SdkMode::Background`; derivation runs as
intake commits. The compact-time drain loop, its three constants, and the idle
pump are all deleted. The arm waits, bounded and cancellably, on
`CaptureHandle::drain_settled` and fails open if it does not settle.

Two things this required, both load-bearing:

- **Only the capture session is Background.** The scheduler drains via
  `tokio::spawn`, so it needs a runtime that outlives the work; the capture
  worker's `block_on(worker_loop)` runtime is exactly that. Every other
  `LhcSession` is opened on a runtime built for one call, where a scheduler
  would spawn drains that die at runtime drop. `set_scheduler_poke` /
  `set_thread_touch` are `thread_local!`, so no cross-instance clobbering.
- **The capture session must not hold deterministic callbacks.** Background
  derivation writes to the durable record, so J1 applies there now, not at
  compact time. The capture session is opened with `LateBoundCallbacks`, which
  **wait** for the host to seed production callbacks rather than erroring —
  LHC's `work_item` table has no `attempts` column, so a returned `Err` is
  terminal, and erroring early would permanently un-derive a thread's first
  turns.

**Every settle-wait is bounded.** With a live scheduler, "wait for quiescence"
can genuinely never return (unseeded callbacks park derivation on
`LateBoundCallbacks::resolve`; a wedged model call hangs a handler). Three
bounds keep that from becoming a hang: the arm's `SETTLE_WAIT` (fail-open);
the capture worker bounds its own `DrainSettled` await, because an unbounded
one wedges the worker loop and starves every queued command including
`Shutdown`; and `LhcSession::close` waits at most `CLOSE_SETTLE_BOUND`
(shutdown skips the wait outright when callbacks were never seeded — that work
provably cannot settle). Abandoned in-flight work stays claimed in the durable
queue; the lease expires and first-touch catch-up re-drains it on next open.
Pinned by `unsettleable_drain_neither_hangs_caller_nor_wedges_worker` and
`shutdown_is_bounded_when_seeded_derivation_hangs` (certification).

**L2's gate moved with it.** It used to be the drain's `FailedTerminal`
disposition. `receipt.degraded` does *not* cover the same ground — measured:
with every derivation failing, compact returns `degraded: []` while serving raw
prompts marked `[fallback]` in the rendered bands. Reading that marker would be
parsing a render (law 1), so the arm asks LHC's typed derivation log
(`query_derivation_log`, `TerminalFailed`) instead and fails open on any
terminal failure.


### LIM-141 — v0.150.2 release-machinery pin update (2026-08-29)

The release workflows' hardcoded SDK identity advanced from the v0.149.2
pin `b408f89` to the LIM-135 certified pin `5207952` (9 occurrences in
`lhc-release.yml`, 1 assert in `lhc-release-promote.yml`). The
`test_verify_package_archive.py` fixture constant is self-consistent test
data, not a gate, and was left untouched. Note: `5207952` sits on the SDK's
`campaign/lhc-rust-open-repair` branch on origin, not yet on `main`; the
workflows' ancestor-of-main policy check is under separate disposition
(main-fold preferred, ruled exception fallback).

