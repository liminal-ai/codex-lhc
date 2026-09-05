# codex-lhc — LHC context management for Codex

Fork of [`openai/codex`](https://github.com/openai/codex) adding
[LHC](https://github.com/liminal-ai/long-horizon-context) (Long Horizon
Context): event-sourced capture of every session into a per-thread SQLite
record, with banded compaction replacing native auto-compact — durable
history with explicit capture/reconstruction limits. The record preserves what
the adapter captures; it cannot recover host structure discarded during capture
(see `codex-rs/lhc/codex-lhc-host/src/materialize.rs`, `CAPTURE_GAPS`).

- **[`lhc-docs/README.md`](lhc-docs/README.md)** — what this fork is and why, for
  humans or agents evaluating it. Start there; this file is the maintenance
  contract. Install and configuration: [`lhc-docs/INSTALL.md`](lhc-docs/INSTALL.md).
- Fork work lives on **`lhc`** (default branch). `main` tracks upstream.
- Never run any self-update path on this checkout — it is a git-tracked
  source build.
- Current law is this file, the touchpoint inventory below, and the enforced
  tripwire. The LHC repo's
  `docs/lhc-rs-port/phase4-codex-integration-brief.md` is retained as historical
  implementation context; it predates certified retrieval and is no longer a
  plan of record.

## Layout

- `codex-rs/lhc/vendor/long-horizon-context` — submodule, pinned to
  **certified commits only** (gate-green at the pin; the historical
  `lhc-rs-port` working branch was retired into `main` 2026-08-08).
  Current pin: **`5207952`** (`5207952b0d6d5dadb95f810c75a18e0867da6b72`) —
  LIM-133 bounded, non-copying shared-LHC opens + frontier/key-projection
  APIs, certified by the LHC-side director 2026-08-28 (chain
  `b408f89 -> 6ec5796 -> 49e1887 -> dc0153c -> 5207952`; TS 45/45, Rust
  23/23+15/15+10/10+4/4). **Certified-content:** the pin is an ancestor of the
  locally available
  `origin/main` ref (checked 2026-09-05). The tripwire refreshes and reports
  ancestry separately from certification; main ancestry alone is not certification.
  Prior pin `b408f89` —
  accepted final SDK identity delta (2026-08-26), directly over `13573a1`.
  The final delta changes only TypeScript scheduler files; its
  `packages/lhc-rs` tree is byte-identical to the parent. The inherited Rust
  turn-parts post-close-tail repair is directly over `713b38d`: schema v12
  host step index on messages, host metadata surface, `mid_turn_compact` entry
  with the four-fact seam assertion and per-thread mechanism exclusivity (typed
  `forced_boundary_thread` / `compact_continuation_parts_thread` refusals),
  walk split/settle/parts, newest-closed protection, and retention of a split
  closed turn's parts, compact point, and nonempty Full suffix while a newer
  turn remains below the Full budget. Once newer pressure fills that budget,
  the old turn settles whole before the newer active turn splits, preserving
  the one-unsettled-turn invariant. It descends `9d4d182` (bounded selector default;
  `LHC_COMPACT_ALGORITHM=legacy` still selects the legacy eager selector),
  `f4de85c` (compact-continuation UTC timestamps / CX-S5), `2cb04a5` (LIM-67
  contract 2.0.0 protected pending-tool escalation), `6232317` / `98826c1`
  (LIM-63 / 63A), and retrieval `7062814` / `dd251ec`. The tripwire requires a
  clean tree at the pin and full mid-turn / full-loop layers.
  A dirty submodule working tree fails the tripwire (F12) — layer 0 at
  start **and** end of `scripts/check-lhc-hooks.sh`, so fmt-churn or any
  mid-run dirt in the certified port cannot go green.
- `codex-rs/lhc/codex-lhc-host` — the adapter crate (fork-only). Owns capture, SDK access, mapping, and reconstruction primitives.
  Core owns Session-dependent compaction preparation, worker execution, and
  durable/in-memory installation coordination in `core/src/compact_lhc.rs`.
- `codex-rs/lhc/goldens/` — capture mapping goldens (Chunk 1). Byte-equality
  is enforced by `mapping_goldens_round_trip_and_match_fixtures` in
  certification (mapper shapes after a real `LhcSession` round-trip);
  the tripwire layer 3 only asserts presence (capture→rebuild diff is Chunk 2).
  Tripwire neutralizes `UPDATE_LHC_GOLDENS` via `env -u`.
- `patches/lhc/` — re-appliable patch per core touchpoint (see its README).
  **Note:** the repo-root `patches/` directory is upstream's third-party
  patch collection; the LHC series lives under `patches/lhc/`.
- `scripts/check-lhc-hooks.sh` — tripwire layers (see header for the
  exact list — it must stay truthful). Inventory:
  **0** vendor CLEAN (start); **0b** reporting control-path tests; **1** sentinel count; **1b** strict-routing
  ignore allowlist (`scripts/check-lhc-compact-ignores.sh`); **2a–2e2** compile /
  lib / certification / e2e / schema fixture / compact_bridge+arm / fmt /
  clippy; **3** goldens present; **4** history-reset patch-repro; **5**
  slice D certification suite (`slice_d_`, serial); **0'** vendor CLEAN
  (end). Each run prints and retains a unique temporary log directory. SDK
  ancestry is refreshed once: an unavailable remote is UNVERIFIED, and an
  off-main pin remains WARN. Either changes the final summary; neither is
  called fully green. Existing local failures still produce a nonzero exit.
- `scripts/check-lhc-compact-ignores.sh` +
  `scripts/lhc-native-routing-ignored-tests.txt` — reviewed native-routing
  ignore allowlist and drift gate (see §Compact test policy).
- `Cargo.lock` — regenerated by cargo when workspace deps change; not
  hand-edited. Inventory row below.

## Release qualification

Release qualification is separate from source synchronization. The manual
candidate workflow builds one immutable aggregate from one source identity on
hosted Linux x86-64/ARM64, Windows x86-64/ARM64, and Apple Silicon macOS
runners. Every archive uses the canonical Codex package layout plus exact LHC
SDK/schema provenance. Protected promotion consumes those exact bytes without
rebuilding through the audited maintainer token, then fails unless the public
tag, release, complete asset set, and downloaded hashes read back exactly.
`lhc-platform-readiness.yml` independently keeps all five native paths ready.

## LC Adaptive Service Tier

`lc_adaptive_service_tier` is an opt-in cost control for long-context agents.
It uses Fast service below a configured prepared-request threshold and Normal
service at or above it. After LHC compact reduces the request below the
threshold, the next request naturally returns to Fast. The resolver is pure:
it does not mutate config or persist a separate toggle state. When disabled,
manual `service_tier` behavior is unchanged.

```toml
# Cost control for long-context agents.
#
# Fast service improves latency but consumes rate-limit capacity faster.
# OpenAI also increases token cost after the long-context threshold.
# This feature avoids stacking both costs: it uses Fast below the threshold,
# downshifts to Normal above it, and returns to Fast after compaction.
#
# Enable this if you prefer Fast and contexts above 272K,
# but do not want to pay both multipliers at the same time.
[lc_adaptive_service_tier]
enabled = true
threshold = 272000
below = "fast"
at_or_above = "default"
```

The distributed GPT-5.6 defaults use a 370K working window and a 350K LHC
compact trigger. Their catalog capability ceiling remains 1.05M so operators
using an API key can opt into a larger window. Requests above 272K use
OpenAI's long-context pricing and consume rate limits faster.

## Per-session LHC compact bands

Band allocation is runtime configuration, not compiled policy. The fork default
is equal allocation, and a session can override it through `-c`:

```toml
[lhc_compact.percentages]
full = 25
smooth = 25
detailed = 25
brief = 25
```

All values must be non-negative and sum to 100. The selected mix applies to
manual, automatic, and mid-turn LHC compact paths for that session.

## Touchpoint inventory (core lines owned by the fork)

Every `LHC-HOOK` marker is an occurrence of the substring `LHC-HOOK` outside
`codex-rs/lhc/`. Count is the tripwire denominator, not a coverage proxy —
**F11's Session e2e test is the wiring gate**, not the marker arithmetic.

| # | File | Purpose | Patch |
|---|------|---------|-------|
| 1 | `codex-rs/Cargo.toml` | workspace member + path dep for `lhc/codex-lhc-host` | `0001-workspace-member` |
| 2 | `ext/extension-api` | additive `RawItemContributor` / `RawItemProvenance` + registry | `0002-raw-item-contributor` |
| 3 | `features/src/lib.rs` | `Feature::LhcCapture` (product default ON) | `0003-feature-flag` |
| 4 | `core/src/session/mod.rs` | provenance-carrying record path + `send_raw_response_items` fan-out + e2e module | `0004-session-raw-item-hook` |
| 4a | `ext/extension-api/src/contributors/turn_lifecycle.rs` | turn start/stop/abort inputs carry optional host `started_at`/`completed_at` (schema v5 timing) | (with 0002) |
| 4b | `core/src/tasks/lifecycle.rs` + `tasks/mod.rs` | pass host turn timestamps into turn lifecycle emitters; abort path returns timing from `handle_task_abort`; LIM-134 centralizes terminal contributor selection so exactly one of stop, abort, or error fires | (with 0007) |
| 5 | `app-server/Cargo.toml` | `codex-lhc-host` dependency | `0005-app-server-dep` |
| 6 | `app-server/src/extensions.rs` | `codex_lhc_host::install(...)` (cwd + host seam) | `0006-app-server-install` |
| 7 | `core/Cargo.toml` | `codex-lhc-host` runtime dep (compact arm) + dev e2e | `0007` |
| 8 | `Cargo.lock` | regenerated lockfile (not hand-edited) | n/a |
| 9 | `core/src/stream_events_utils.rs` | model-output path tags `RawItemProvenance::ModelOutput` | (with 0004) |
| 10 | `core/src/compact.rs` | compaction model-output tags `ModelOutput` | (with 0004) |
| 11 | `core/src/compact_lhc.rs` | LHC compact arm + write-back (real `lhc.compact` body) + slice C rewrite install + turn-parts MidTurn arm (Story 5: certified `mid_turn_compact` at the settled seam, exact active-turn identity, typed-only `ForcedBoundaryThread` route to the LIM-63B compact-continuation runtime; one-writer, no native fall-open); LIM-134 required PreTurn/Standalone compact awaits capture Ready/Failed/Stopped (bounded, cancellable) | `0007-lhc-compact-arm` |
| 12 | `core/src/tasks/compact.rs` | manual ladder: LHC arm above TokenBudget | (with 0007) |
| 13 | `core/src/session/turn.rs` | auto ladder: LHC arm above TokenBudget; MidTurn passes settled seam facts (response_id/usage/tool IDs + total continuation intent + input-queue epoch); LIM-134 compact over prior native history before recording the new prompt and records accepted input exactly once | (with 0007) |
| 13b | `core/src/session/turn.rs` | turn parts F2: begin the provider request/response cycle before each outer sampling request so raw-item capture stamps `stepIndex` on assistant_text/assistant_thinking/tool_call/tool_result | (with 0007) |
| 13a | `core/src/session/input_queue.rs` | monotonic pending-input epoch for MidTurn input-epoch gate (steer/mailbox enqueue) | (with 0007) |
| 14 | `core/src/lhc_inference_bridge.rs` | ModelClient → InferenceCallbacks (live, gated); `derivation_prompt` pins `base_instructions` empty — never `..Default::default()` (P1) | (with 0007) |
| 15 | `core/src/lib.rs` | `mod compact_lhc` + `mod lhc_inference_bridge` | (with 0007) |
| 16 | `core/src/compact.rs` | `#[derive(Clone)]` on `InitialContextInjection` (native arms still take it by value; production dispatch does not clone-and-fall-through) | (with 0004) |
| 17 | `core/src/tasks/lifecycle.rs` | seeds production derivation callbacks into the capture slot (what the capture session's background scheduler derives with) | (with 0007) |
| 18 | `core/src/tasks/compact.rs` | manual ladder binds `cancellation_token` (was `_cancellation_token`) and passes it to the arm — N3 | (with 0007) |
| 19 | `core/src/session/turn.rs` | `run_auto_compact` gains a `cancellation_token` parameter (fork-added; all four callers already had one in scope) — N3 | (with 0007) |
| 20 | `core/src/state/service.rs` | `lhc_test_inference` slot on `SessionServices` | (with 0007) |
| 21 | `core/src/session/session.rs` | initialises `lhc_test_inference` | (with 0007) |
| 22 | `core/src/session/tests.rs` | initialises `lhc_test_inference` (x2) | (with 0007) |
| 23 | `core/src/session/lhc_band_shape_eval_tests.rs` | band-shape eval harness (Chunk 2a); `session/mod.rs` declares the module | (with 0007) |
| 24 | `rollout/src/recorder.rs` | `RolloutRecorder::reopen_after_rewrite` after atomic swap (slice C) | `0007-lhc-compact-arm` |
| 25 | `thread-store/src/live_thread.rs` | `LiveThread::reopen_rollout_after_rewrite` escape hatch (slice C) | `0007-lhc-compact-arm` |
| 26 | `thread-store/src/local/{mod,live_writer}.rs` | local-store reopen implementation (slice C; no sentinel on impl) | `0007-lhc-compact-arm` |
| 27 | `core/src/session/mod.rs` | `install_compacted_history_memory` — in-memory install without append (slice C) | (with 0004) |
| 28 | `core/src/compact_lhc.rs` | startup reconciliation entry before history load (slice E); since 0.153.3 it materialises a compressed `.jsonl.zst` rollout to plain before classification | (with 0007) |
| 29 | `core/src/thread_manager.rs` | call reconcile before `initial_history_from_rollout_path` loads history (slice E) | (with 0007) |
| 30 | `app-server/.../thread_processor.rs` | call reconcile before resume history load (slice E) | (with 0007) |
| 31 | `code-mode-runtime/Cargo.toml` | local Linux build workaround: use the published non-sandbox V8 artifact | `0001-workspace-member` |
| 32 | `cli/Cargo.toml`, `cli/tests/version.rs` | inherits the mapped upstream workspace version reported by `codex --version`, with fork regression coverage | `0001-workspace-member` (test only; manifest restored to upstream) |
| 33 | `protocol/src/config_types.rs`, `config/src/config_toml.rs`, `core/src/{config/mod.rs,config/config_tests.rs,lc_adaptive_service_tier.rs,session/turn.rs,lib.rs}` | LC Adaptive Service Tier config, validation, prepared-request resolver, and request-seam selection | `0007-lhc-compact-arm` |
| 34 | `protocol/src/config_types.rs`, `config/src/config_toml.rs`, `core/src/{config/mod.rs,config/config_tests.rs,compact_lhc.rs}`, `lhc/codex-lhc-host/src/{compact_bridge.rs,compact_continuation.rs,lib.rs}` | Per-session LHC band percentages across manual, automatic, and mid-turn compact | `0007-lhc-compact-arm` |
| 35 | `history/src/lib.rs`, `rollout/src/lib.rs`, `state/{src/migrations.rs,src/migrations_tests.rs,src/sqlite.rs,thread_history_migrations/0007_rollout_generation_id.sql}`, `thread-store/{Cargo.toml,src/local/mod.rs,src/local/rollout_migration.rs,src/local/rollout_migration_tests.rs,src/local/thread_history.rs,src/local/thread_history_generation.rs,src/local/thread_history_materialization.rs,src/local/thread_history_materialization_tests.rs}`, `lhc/codex-lhc-host/{Cargo.toml,src/rollout_swap.rs,src/rollout_swap_tests.rs}` | Paginated projection self-heals after an LHC rollout generation swap using a persisted durable generation identity, including equal-boundary replacements and lifted subagent ordinals; the fork migration is version 7 (it shipped as 5 before upstream 0.150 added its own 5/6) and a legacy version-5 row is relabelled at open by `repair_legacy_rollout_generation_migration_version` | `0007-lhc-compact-arm` |
| 36 | `app-server-daemon/{README.md,src/lib.rs,src/managed_install.rs,src/managed_install_tests.rs,src/update_loop.rs,src/update_loop_tests.rs}` | managed CLI/app-server version parsing and same-version restart coherence use the workspace version; isolated remote-control homes seed fork bytes and never download stock Codex | `0001-workspace-member` |
| 37 | `core/src/compact_lhc_readiness_tests.rs` | LIM-134 required-compact readiness unit proofs (virtual-time wait, cancellation, Failed/Stopped/bound expiry); no sentinel | `0007-lhc-compact-arm` |
| 38 | `core/tests/suite/lhc_preturn_readiness.rs` | LIM-134 production-path PreTurn proofs (natural 350K resumed ready, pre/post-dispatch failure, no-tool one-request success); no sentinel | `0007-lhc-compact-arm` |
| 39 | `exec/src/lib.rs` | LIM-134 empty-result truth: a completed nonblank prompt with no nonblank AgentMessage or Plan fails in human and JSONL paths; no sentinel | `0007-lhc-compact-arm` |
| 40 | `exec/src/{lib_tests.rs,event_processor_with_human_output_tests.rs}` | LIM-134 exec human/JSONL/backfill empty-result proofs (processor-level and reclassify units); no sentinel | `0007-lhc-compact-arm` |
| 41 | `exec/tests/suite/{apply_patch.rs,auth_env.rs}` | LIM-134 F6: fixtures gain a minimal assistant message so completed turns satisfy empty-result truth; no sentinel | `0007-lhc-compact-arm` |
| 42 | `exec/tests/suite/resume.rs` | F7 ruling (Lee): 12 resume tests disabled-with-reasons — budgeted mount_sse_sequence mocks cannot absorb LHC background-derivation POSTs (harness artifact, prod unaffected) | `0007-lhc-compact-arm` |

Rows 20-23 carry **no `LHC-HOOK` sentinel** (they are struct fields, initialisers
and a test module, not seams). They were missing from every patch until Chunk 3
round 9 — see §History-reset recovery R3. Row 31 is likewise non-sentinel build
policy and is covered by 0001. Row 32 records the upstream-aligned runtime
version policy; its regression test is covered by 0001 while the CLI manifest
itself no longer carries a fork delta. Fork-owned and not sentinel-bearing is a
legitimate combination; fork-owned and *not in any patch* is not. Row 26 is the
same pattern (impl details under a sentinel-bearing LiveThread API). Rows 37-40
are the LIM-134 non-sentinel set (readiness/preturn proofs and exec empty-result
truth), row 41 its fixture repairs; LIM-134 added no `LHC-HOOK` marker. Host capture-lifecycle sources stay
under `codex-rs/lhc/` and are outside the patch series.

Expected markers: **53** (`EXPECTED_HOOKS` in the tripwire script).
Was 52 before turn parts Story 5 (+1 for the F2 provider-cycle begin seam in
`core/src/session/turn.rs`).
Was 54 before the 2026-08-06 upstream sync removed two marker sites while
preserving the raw-item contributor behavior (see the sync record below).
Was 51 before slice E (startup reconciliation); +3 for reconcile entry +
thread_manager history-load seam + app-server resume history-load seam.
Was 47 before slice C (rollout rewrite); +4 for recorder reopen, LiveThread
reopen, compact-arm rewrite install, and in-memory-only compact install.
Was 39 before slice A (schema v5 field capture); +8 for turn timing fields on
`TurnStart`/`TurnStop`/`TurnAbort` inputs and the three lifecycle emit sites that
thread host `started_at`/`completed_at` (no new raw-item hook sites — provider
usage rides the existing free `TokenUsageContributor` seam).

Rule: any commit that adds/changes an `LHC-HOOK` line updates, in the
SAME commit: `EXPECTED_HOOKS`, this inventory, and `patches/lhc/`.

Chunk 2b compact arm: **done** (offline, deterministic inference). Chunk 3
remains: live cert with real model (auth lane).

### Rollout rewrite generation scheme (slice C)

On LHC compact install the rollout is **rewritten**, not appended. For a live
path `P` (e.g. `…/sessions/YYYY/MM/DD/rollout-….jsonl`):

| Role | Path |
|------|------|
| Active generation | `P` |
| Prior generation (exactly one) | `P.prev` |
| In-progress rewrite | `P.rewrite-tmp` |

Steps: write full materialized sequence to `P.rewrite-tmp` → fsync file →
fsync directory → remove old `P.prev` if present → `rename(P → P.prev)` →
`rename(P.rewrite-tmp → P)` → **reopen** the append-mode recorder handle so
it points at the new inode. An unreopened fd silently follows the orphaned
prior generation.

Failure before the final rename leaves `P` untouched and authoritative —
loud `tracing::error`, session continues, next compact retries. **No append
fallback path.** In-memory history installed at compact equals
`Compacted.replacement_history` (bands) + post-boundary native
`ResponseItem`s — the same split resume rebuilds from the rewritten file.

Implementation: pure swap in `codex-lhc-host::rollout_swap`; materializer
wiring in `core/src/compact_lhc.rs`; reopen on `RolloutRecorder` +
`LiveThread`.

Every rewrite mints a UUIDv4 generation identity and persists it as a top-level
field on the SessionMeta rollout record. The paginated history projector stores
that exact identity with its frontier so equal-size, equal-boundary replacements
cannot be mistaken for append-only growth.

### Background derivation ownership

Only the long-lived capture session uses `SdkMode::Background`. One-shot SDK
opens must not start a scheduler whose runtime dies when that call returns.
Capture starts with `LateBoundCallbacks` and waits for production inference
callbacks; deterministic callbacks must never derive the production archive.

Compaction does not require derivation settlement: it uses fallback bands and
full-fidelity residue when summaries are unavailable. Any explicit settle/shutdown
wait must remain bounded. See §Compact vs derivation and the capture session's
close path. The earlier manual-scheduler incident and superseded compact-time
wait policy are preserved in the maintenance history.

### LIM-141 — v0.150.2 release-machinery pin update (2026-08-29)

The release workflows' hardcoded SDK identity advanced from the v0.149.2
pin `b408f89` to the LIM-135 certified pin `5207952` (9 occurrences in
`lhc-release.yml`, 1 assert in `lhc-release-promote.yml`). The
`test_verify_package_archive.py` fixture constant is self-consistent test
data, not a gate, and was left untouched. Note: `5207952` sits on the SDK's
`campaign/lhc-rust-open-repair` branch on origin, not yet on `main`; the
workflows' ancestor-of-main policy check is under separate disposition
(main-fold preferred, ruled exception fallback).

### LIM-135 — bounded scans + one ruled raw-SQL exception (2026-08-29)

Normal capture open, occurrence tracking, archive coverage, and reconcile
paths use constant-row / caller-bounded SDK projections (`thread_frontier`,
`event_key_prefix_counts`, `list_event_keys_by_prefix`); no normal startup
path calls `list_events` or parses historical payload JSON. Legacy ID-less
occurrences resolve lazily under a hard cap that refuses visibly.

**Ruled exception (manager, 2026-08-29):** exactly one host call site —
`codex-lhc-host/src/compact_bridge.rs` (`load_events_by_idempotency_keys`) —
reads the thread file with a direct exact-key `SELECT` to fetch walked
compact-marker rows for derived-provenance recovery, because the certified
pin has no bounded fetch-by-key operation. Bounds of the ruling: this single
call site; exact-key SELECT only; opens via the SDK's own
`open_thread_database`; schema authority is the vendored pin in this tree.
**Any second raw-SQL site is a new ruling, not an extension.** A bounded
`get_events_by_keys` SDK API is a live request with the LHC-side director;
**next pin uptake must check whether it landed and migrate this site if so.**
Marker-key/regenerated-boundary outputs are semantically equivalent, not
byte-identical (accepted under the criterion's semantic arm).

## Sync drill (merge-based, exact selected release)

1. Fetch upstream tags, verify the selected stable `rust-v…` tag and its peeled
   commit, and merge that exact commit onto the fork working branch. Record both
   identities. Do not substitute `upstream/main` for a selected stable release;
   main-line adoption is a separate deliberate decision.
2. Inspect upstream changes around `core/src/session/mod.rs` and the turn loop.
3. Watch item: compaction dispatch ladder in `core/src/tasks/compact.rs`.
4. **Advance the patch base.** `patches/lhc/BASE` names the upstream commit the
   whole series diffs from. A merge moves the tree past it, so upstream's own
   changed files then read as "fork-owned but in no patch" and `patch-repro`
   fails. Write the merged upstream tip into `BASE` and regenerate all seven
   patches against it (`patches/lhc/README.md`). This step is **part of the
   sync**, not cleanup after it.
5. `./scripts/check-lhc-hooks.sh` — all layers green before push.
6. Commit with tripwire output summarized in the body; push to origin only.

### Previous syncs and release work

Dated merge accounts, qualification evidence, and conflict resolutions are in
[the maintenance history](lhc-internal/history/fork-maintenance-through-2026-09-04.md).
Current upstream base: `patches/lhc/BASE` (`rust-v0.153.3` at this revision).

**Upstream experimental context management:** notes/history and `new_context`
remain unqualified with `features.context_management.experimental_mode` enabled.
The request currently reaches strict LHC through `run_auto_compact`; native reset
semantics must not be assumed. See maintenance slice S11 before enabling this mode.

## History-reset recovery — **works, verified** (Chunk 3 round 9, 2026-07-26)

The whole series is a diff from **one upstream base**, recorded in
`patches/lhc/BASE` (read that file for the current identity; see Sync drill
step 4). Each fork-owned file appears in **exactly one** patch. Tripwire
layer 4 runs this drill on every invocation and fails if it stops reproducing
the tree, so it cannot rot silently again.

### Procedure

1. Fresh clone of upstream → branch `lhc`.
2. Restore fork-owned files that are **not** core touchpoints:
   `FORK.md`, `.gitmodules`, `patches/`, `scripts/`, `codex-rs/lhc/`.
3. Restore the vendored submodule at the pin in §Layout:
   `git clone <lhc url> codex-rs/lhc/vendor/long-horizon-context && git -C … checkout <pin>`.
   **Not `git submodule update --init`** — the gitlink is a tree entry in fork
   *commits*, so on a clean upstream base there is nothing to init. That step
   was wrong in the previous text and would stop the drill dead.
4. `git apply patches/lhc/0*.patch` (in order).
5. `cargo` regenerates `codex-rs/Cargo.lock` — it is deliberately in no patch
   (inventory row 8).
6. `./scripts/check-lhc-hooks.sh` — all layers green.
7. Force-push with Lee's sign-off.

### Verification and historical evidence

Layer 4 of the tripwire proves recovery against the current recorded base.
Older drill results, the historical verification backlog, and the correction to
Chunk 1's independence claim are retained in
[the maintenance history](lhc-internal/history/fork-maintenance-through-2026-09-04.md).
They are evidence of those runs, not current qualification or outstanding-work status.

Any fork-owned core change regenerates its patch in the same commit. An upstream
sync regenerates the entire series against the new `patches/lhc/BASE`. See
[patch regeneration](patches/lhc/README.md).

## Laws (binding)

1. Write-back is the architecture. Native arms still append via
   `replace_compacted_history`; the LHC arm (slice C) rewrites the rollout
   and installs bands+tail in memory so live history equals resume-from-file.
2. LHC compact arm feeds native accounting (threshold-untrips).
3. Census fail-open / full-conversation consumers at Chunk 2 start.
4. Capture idempotent under write-back; also under resume/replay/retry via
   `ResponseItemId`+content-digest keys (F1/H5). Status-advancing items
   (e.g. ImageGenerationCall in_progress→completed) mint distinct keys.
5. Never put host structure in envelope `extra` (LHC rejects unknown envelope
   keys). Closed payload schemas carry host raw bytes in nested free-form
   fields (`arguments.__hostRaw`, full media URLs in `text`).
6. Classify on typed host signal (`RawItemProvenance`), never text prefixes.
7. Per-entry classification fails toward synthetic.
8. A test that cannot fail is not a test; fixtures must be host-reachable shapes.
9. **Rule zero:** every test that certifies what LHC *records* must submit
   through a real `LhcSession` and read the stored row back. Mapper-only
   assertions may supplement a round-trip, never stand alone.

## Capture degradation policy (F5/H6)

Bounded queue (`CAPTURE_QUEUE_CAP = 1024`, one slot reserved for the
truncation note). On full queue / closed channel / repeated submit failure:
**loud error + `degraded` latch + self-describing `runtime_note` in the
record + refuse further captures** until reopen. A silently truncated
prefix is worse for Chunk 2 write-back than a lossy one with a marker.

## Pre-open buffer (H2/F17)

`on_thread_start` still opens off the critical path, but items (and config
changes) that arrive before the handle is ready are **buffered** (same cap
as the capture queue) and flushed on open. Prefer a complete session
opening over silent loss of the first user prompt.

## `lhc_capture = false` is an invalid product state (Lee, 2026-08-29)

The product fork runs LHC because LHC is the product, not an optional build
flavor. **`Feature::LhcCapture = false` is an invalid state for this product
— product-owner ruling.** It is not a supported mode, not a fallback, and not
a user-facing kill switch; doc language framing it as one is wrong and dies
on touch (this section previously did). Its only legitimate uses are test
infrastructure and diagnostic attribution (e.g. the F7 resume-regression
discriminator, 2026-08-29).

Mechanically, with the flag off `on_thread_start` returns immediately: no
worker, no slot, no LHC I/O, no SQLite, no mapping; contributors stay
registered (one `Box::pin` ready-future per raw-item batch). That behavior
exists for the legitimate uses above only. Flag provenance trace and
disposition are tracked with the campaign manager.

## Compact test policy (LIM-142)

Codex-LHC is **strict LHC-only** for compaction. Every normal entry point —
manual `/compact` (`CompactTask`), pre-turn automatic (`run_pre_sampling_compact`
→ `run_auto_compact`), mid-turn rollover, model downshift, comp-hash change,
context-limit, and the first turn after resume — reaches
`compact_lhc::run_strict_lhc_compact`. Upstream's native TokenBudget / remote
v1 / remote v2 / local compaction implementation is **retained unchanged** for
upstream parity; it has no reachable caller from those entry points.

Reachability (source):

| Entry | Production route | Native fall-open |
|-------|------------------|------------------|
| `Op::Compact` / `CompactTask` | `run_strict_lhc_compact` | none |
| PreTurn context-limit | `run_pre_sampling_compact` → `run_auto_compact` → `run_strict_lhc_compact` | none |
| Model downshift / comp-hash | `maybe_run_previous_model_inline_compact` → `run_auto_compact` → `run_strict_lhc_compact` | none |
| MidTurn rollover | `run_auto_compact(MidTurn)` → `run_strict_lhc_compact` | none; typed `ForcedBoundaryThread` only reaches compact-continuation |
| Resume over-limit | same PreTurn auto route on the first post-resume turn | none |
| `lhc_capture = false` | hard `UnsupportedOperation`, history preserved | none |

Tests split three ways. The split is the policy, not the failure inventory:

1. **Active strict-LHC behavior.** `compact_lhc::{tests,strict_routing_tests,mid_turn_tests,canary_tests,slice_d_tests}`, `suite::compact_lhc_mid_turn_loops`, and the already-ported suite proofs `auto_compact_runs_after_token_limit_hit` / `manual_op_compact_routes_to_strict_lhc_not_native`. Class-1 invariants that used to ride native fixtures (PreCompact cannot veto, PostCompact after install, Compact session-start queue, window-id advance, resume/fork history, in-run steer) are owned here — not by ignoring the old test.
2. **Active route-independent native helper/unit coverage.** `compact::tests`, `compact_remote` metadata / v2 image-budget units, skip-path pre-sampling tests. These stay green.
3. **False-premise native-routing integration coverage.** Tests whose owned assertion is that a normal entry point issues native local summarization, remote `/responses/compact`, or TokenBudget compaction. Each carries `#[ignore = "codex-lhc LIM-142 strict-lhc-routing: …"]` naming that native artifact. When the old fixture also named a supported invariant, the reason points at the active LHC owner; the ignore is only for the native-request premise.

The class-3 set is exact: `scripts/lhc-native-routing-ignored-tests.txt` (`# count:`) plus `scripts/check-lhc-compact-ignores.sh` (tripwire layer 1b). Drift in either direction fails.

## Compact-continuation MidTurn (LIM-63B)

At `CompactionPhase::MidTurn` (post-sampling seam: provider response complete,
tools settled, async hooks drained, capture flushed, before next provider
request), **LHC is the single writer**. The certified SDK operation
`run_compact_continuation` owns boundary/marker/install residual:

| Continuation branch | Behavior |
|---------------------|----------|
| `pending_correlated_tool_result` | Contract 2.0.0: the branch carries the **complete sorted unique set** of response-scoped client tool-call IDs (`protectedToolCallIds`). Ordinary preserve first (same canonical turn, all pairs verbatim, no marker). When preserve cannot create safe runway, **one protected escalation** (LIM-67): forced `context_compact_continue` boundary, protected visibility-boundary prune of older unprotected result bodies only, one typed marker, atomic view+boundary install. |
| `active_non_tool` | Force LHC `context_compact_continue` boundary; compact; **one** typed marker; continue same Codex task. |
| `none` | No continuation compact path. |

### LIM-67 protected escalation + host full-body validation

- The host supplies the **real safe-runway threshold**: the Codex
  auto-compact scope limit (source `codex_auto_compact_scope_limit`) or the
  context-window limit — never a percentage, never the advisory LHC lower
  target.
- A protected-escalation core install leaves the durable SDK residual
  `hostValidationStatus = awaiting` with the next provider request blocked.
  The host then materializes the **exact next-request item sequence** through
  the existing rewrite materializer and validates it
  (`codex_lhc_host::validate_next_request_body`): full tool
  correlation/ordering, protected pairs byte-stable against the live
  pre-attempt history (host provenance keys `id` /
  `internal_chat_message_metadata_passthrough` normalized out), required
  encrypted reasoning byte-exact (the materializer's certified canonical text
  flattening is allowed), and complete-body size strictly below the runway
  (`codex_materialized_body_o200k_estimate`).
- `record_mid_turn_host_validation` records durable `ok`/`failed` (schema
  v11). `ok` proceeds to rollout rewrite + in-memory install; `failed` (or an
  unrecordable ack) leaves rollout and in-memory history on their **prior
  generation**, blocks the next provider request, and never rolls the core
  install back.
- **Reload gate:** `codex_lhc_host::host_validation_reload_block` — when the
  newest receipt records an installed view whose validation is
  awaiting/failed (and no later `ok` row exists), startup reconciliation
  refuses to regenerate the rollout from the installed surface
  (`ReconcileOutcome::Unchanged { reason: "host_validation_blocked" }`).
  A later safe attempt supersedes.
- Evidence: `mid_turn_protected_escalation_validates_installs_and_clears_reload_gate`,
  `mid_turn_host_validation_failed_blocks_send_and_gates_reload` (lib), and
  the legacy-runtime unit tests routed directly to
  `run_mid_turn_forced_boundary_continuation` (Story 5: the arm reaches this
  runtime only on the SDK's typed `ForcedBoundaryThread`; the suite loops now
  pin the parts path — see "Turn parts MidTurn").

Host obligations:

- Build truthful `CompactContinuationHostFacts` (provider usage without
  double-counting cache; post-measurement estimate from newly captured tail;
  upper trigger from Codex model/window policy; lower target from LHC compact
  profile; real attempt identity; input epoch at decision/apply).
- Obey `next_provider_request_allowed`. Refuse → stop before next request with
  a clear diagnostic; **do not** fall open to native compaction while
  `Feature::LhcCapture` is on.
- Transport retry and input-epoch change → stable skip, no mutation.
- Capture lag / incomplete flush → skip; prior serving view remains.
- After truthful `no_reduction` / `terminal_no_reduction` /
  `dry_relief_no_reduction` only, hysteresis arms and blocks re-attempt until
  measured pressure grows by the configured margin (default **10_000** tokens).
  Skip/refuse/capture-lag/transport-retry/input-epoch/invalid-install outcomes
  do **not** arm or suppress later recovery. Successful reduction clears.
- Input-epoch gate uses the monotonic `InputQueue` epoch (bumped on every
  steer/mailbox enqueue), snapshotted at rollover decision and re-read at apply
  — never `history_version`.
- Cancellation/timeout: the MidTurn operation future is timed out **inside**
  the worker runtime around `run_mid_turn_compact_continuation` so a hung
  future is dropped on that worker thread and the thread exits; the host always
  joins. Host rewrite is suppressed if the turn token cancelled during the
  section. No detached mutator.
- Continuation classification carries total follow-up intent and the complete
  sorted response-scoped tool call ID set from the completed sampling response
  (not a history-tail rescan as authority). Queued steering/mailbox alone is
  `active_non_tool`, not `none`.
- Attempt identity uses the completed provider `response_id`; provider usage is
  that response's `token_usage` (cache split exact).
- `lhc_capture = false` (invalid product state; test/diagnostic only) restores
  native MidTurn behavior (visible `Unavailable` residual).
- Schema v11 thread stores (writer claim, boundary, receipt, stage log,
  host-validation ack) are owned by the vendored pin; startup reconcile
  remains valid and additionally honors the LIM-67 host-validation reload
  gate.

Implementation: `codex-lhc-host::compact_continuation` +
`core/src/compact_lhc.rs` MidTurn arm; evidence in
`compact_lhc_mid_turn_tests.rs`.

## Turn parts MidTurn (Story 5, 2026-08-25)

On a **clean thread** the MidTurn arm no longer forces a boundary. At the
same settled post-sampling seam it invokes the certified SDK
`thread_view::mid_turn_compact` — the ordinary bounded prepare → install
compact behind the four-fact seam assertion (`modelResponseComplete`,
`requestedToolsSettled`, `captureFlushed`, `beforeNextProviderRequest`, all
asserted true and only when true) — which splits the active turn at step
edges into parts inside the **same Codex turn**. The existing atomic
materialize / rollout rewrite / in-memory install path then serves the view.
No synthetic continuation turn, no forced-boundary marker.

- **F2 step stamping** (`LhcStepIndex`, hook 13b): one zero-based cycle per
  outer sampling request; transport retries inside `run_sampling_request`
  never advance it; stamped only on `assistant_text` / `assistant_thinking` /
  `tool_call` / `tool_result`; NULL = unknown, never split.
- **Exact active-turn identity (AC-7.4 host side).** The SDK names durable
  turns itself (`t{order}`), so capture binds the host `LhcTurnId` to the
  durable turn the host turn's prompt opens (`BindTurn` at `on_turn_start`,
  ordered ahead of the prompt; the intake's `Opened` transition — or the
  adopted open turn when the prompt joins an empty one — completes the
  binding). The arm compares that bound id **exactly** with
  `host_metadata.active_turn.turn_id` before invoking compact. Missing
  identity, missing binding, or mismatch keeps the current body, invokes
  nothing, and retries at a later eligible seam. A forced-boundary
  continuation re-binds the host turn to the SDK-opened continuation turn so
  legacy threads keep reaching their typed classification.
- **In-run steer stays in the task turn (Flow 7).** A human prompt the
  host records after the current Codex turn has begun provider cycles (a
  pending `start_or_steer_turn` drained by `run_turn` inside the loop) is
  stamped `payload.steer=true` by the raw-item contributor from the
  turn-scoped `LhcStepIndex` fact alone — never from text, never on the
  opening prompt (recorded before cycle 0). The SDK keeps it a member of the
  open task turn (no close/open), and capture leaves the host→durable
  binding untouched (only a forced-boundary continuation re-binds).
  `full_loop_in_run_steer_stays_in_task_turn`.
- **Host apply after an SDK parts install retries later — after the swap
  state is reconciled against exact generation identities.** When the SDK
  has installed the parts view but the host materialize / rollout rewrite
  does not complete, the arm first reads the actual on-disk swap state under
  its own one-writer authority (`codex_lhc_host::reconcile_interrupted_swap`,
  synchronous, same arm) and establishes exactly one authoritative active
  generation before deciding. Both generations are identified exactly
  (`SwapGenerations`): the *prior* generation is the byte content of the
  authoritative active file captured after the final flush immediately
  before the swap (`.prev` is that inode renamed, so only a byte-exact match
  proves it; an unreadable file there means no swap is attempted and the
  seam retries); the *new* generation is the ordered wire content the swap
  wrote, proven by `strict_read_generation` + `proves_new_generation` —
  every row a complete rollout line, nothing skipped, exact row count/order,
  each row's item object equal to the expected item's wire form, and the
  envelope exactly as `write_rollout_jsonl` emits it: ordinals derived by
  the same `RolloutOrdinalState::for_rewrite` plan (none on legacy output,
  exact contiguous values on paginated output — missing, duplicate,
  shifted, reordered, malformed, or foreign ordinals fail), the exact
  `rollout_generation_id` this attempt wrote — generated by the arm via
  `new_rollout_generation_id`, handed to
  `atomic_rewrite_rollout_as_generation`, and retained in the arm across
  the attempt (never read back from disk) — on every `SessionMeta` row and
  on no other row (missing, foreign, non-generated, non-string, misplaced,
  split, or valid-but-different UUID v4 identities fail), and RFC 3339
  timestamps (value nondeterministic, contract not). The tolerant `parse_rollout_items`
  never establishes authority. Dispositions: old still active
  (`PostTempWrite`/`PostFsync` class) → prior body and rollout stand,
  `MidTurnBlocked { next_provider_request_allowed: true }`, retry at the
  next seam; old moved to `.prev` with the proven new generation at
  `.rewrite-tmp` (`PostOldRename` class) → the swap is finished
  (`tmp → active`, directory sync proven) and the host completes its
  in-memory / window install against it (`Installed`); new generation
  already active (`PostNewRenamePreReopen` / post-rename directory fsync) →
  the compact stands, never rolled back, host install completed
  (`Installed`); old moved and tmp unproven → `.prev` restored only when it
  proves the prior generation byte-exactly (directory sync proven), retry
  later; anything else — foreign or torn active, foreign/torn `.prev`,
  unproven tmp with no proven prior, or a repair rename whose directory sync
  failed — → `RolloutUnreconciled` denies further sampling with the exact
  state (history preserved; nothing promoted, restored, or guessed). Never
  native, never compact-continuation; cancellation/abort and
  `RolloutUnreconciled` are the only denies.
  `mid_turn_parts_host_apply_failure_retries_at_later_seam` (old active),
  `mid_turn_parts_host_apply_post_old_rename_finishes_swap_and_installs`,
  `mid_turn_parts_host_apply_post_new_rename_completes_install`,
  `mid_turn_parts_host_apply_unproven_repair_sync_denies_sampling`; host
  `reconcile_interrupted_swap_establishes_one_active_generation`,
  `reconcile_interrupted_swap_refuses_unproven_generations`,
  `strict_new_generation_proof_requires_exact_envelope`. Foreign/torn
  active or `.prev` cannot be produced through the arm under one-writer
  authority (the arm snapshots and swaps in one critical section; `.prev`
  is the renamed prior inode), so those refusals are proven at the exact
  reconciliation function the arm calls.
- **Truthful seam.** A capture flush that does not complete within its bound
  is not a settled seam: keep the body, retry later — never assert a false
  fact (supersedes the LIM-63B "warn and continue" behavior).
- **Failure split.** Only cancellation/abort denies the next provider
  request. Every other refusal, storage, worker, timeout, or panic failure
  preserves the current body and allows progress to a later seam. Nothing but
  the typed `ForcedBoundaryThread` reaches compact-continuation.
- **Coexistence / exclusivity (AC-7.3, typed-only).** A thread the SDK types
  `ForcedBoundaryThread` (any boundary row) keeps the LIM-63B runtime through
  the ordinary arm, exactly as before. Once `parts_activated_at` exists the
  legacy runtime never runs for that thread (the SDK refuses it typed even
  when invoked directly). Both directions:
  `mid_turn_forced_boundary_thread_keeps_legacy_runtime_through_arm`,
  `mid_turn_parts_thread_never_runs_legacy_runtime`.
- **Post-close Full-tail retention.** Closing a split turn does not collapse
  it while the next user turn is still small. A compact at the next settled
  seam keeps the prior parts and compact point byte-stable, keeps the prior
  nonempty verbatim suffix in Full before the new prompt, and serves no whole
  duplicate. Later pressure settles that old turn atomically before the new
  active turn becomes the sole split turn. Schema remains 12; no migration or
  host-source change is involved.
- **Evidence:** `compact_lhc::mid_turn_tests` (parts, identity mismatch,
  generic-failure retry, host-apply retry, cancel blocks, flush seam), suite
  `compact_lhc_mid_turn_loops` (same turn, no marker, in-run steer, sustained
  pressure splits under the production 120k bound, closes, preserves its exact
  late Full suffix through a real small next Codex turn, then settles before
  that newer turn splits), `lhc_capture_e2e` step stamps,
  certification `step_index_round_trips_on_four_kinds_and_null_elsewhere`,
  and tripwire layer 2a2 (`scripts/check-lhc-midturn-parts.py`, bare exec).
- Not in scope here: threshold/band tuning, Story 6.

## Turn abort / SIGINT capture (slice A + F-L3 live-cert)

**Graceful interrupt path (host emits abort):** `codex exec` listens for
`ctrl_c` / SIGINT and sends `turn/interrupt` → `Session::abort_all_tasks(
TurnAbortReason::Interrupted)` → `emit_turn_abort_lifecycle` → LHC
`on_turn_abort` with `outcome=aborted` + reason (`interrupted`) + host
timestamps. The contributor **awaits `flush()`** after `turn_end` so the row
is durable before process shutdown continues. Covered by
`e2e_v5_host_facts_abort_with_reason`.

**Hard process death (no host abort signal):** SIGKILL, or SIGTERM that does
not flow through the interrupt handler, or any kill that races past
`on_turn_abort`/`flush` leaves the open turn as-is. LHC does **not** invent
`outcome=aborted` — `outcome=None` (or a later prompt-boundary close with
unset host facts) is the honest record. Vendor store documents the same:
prompt-boundary closes leave outcome/timing NULL. Do not fabricate aborts.

**Next-open boundary:** when a new user turn starts while a prior turn is
still open in the LHC record (resume after hard death), the SDK/prompt-boundary
close path may close the prior turn without host facts — again `outcome=None`
is correct, not a capture bug.

## Compact vs derivation (F-L1 / doctrine)

LHC compact **must not refuse** because derivation failed or is unready
(`claim_expired`, terminal_failed, degraded bands). The selection walk uses
the fallback ladder (less-derived bands, full-fidelity residue). The fork arm
loud-logs terminal failures and installs via the ladder; derivations upgrade
later. A `NoReduction` loud-fail remains only when the **materialized body is
strictly larger** than the current rollout file's model-context size
(like-for-like; F-L4) **and** the file is a pure single-boundary projection.
When the rollout is **native-append-polluted** (more than one `Compacted`
record — native append after an LHC rewrite), a NORMALIZATION rewrite
proceeds regardless of the size comparison (slice E Part 1; loud `info!`).

## Startup reconciliation (slice E)

At resume / history load: classify the rollout vs the LHC thread compact
point as MISSING (file gone), CORRUPT (unparseable), or STALE (LHC compact
point advanced past the file boundary — crash window between LHC commit and
rename). In each case regenerate via materialize + atomic swap **before**
history is served; loud log names the trigger. If the thread is unavailable,
leave the file alone (fail-open, native behavior). Logic lives in
`codex-lhc-host::rollout_reconcile`; thin core/app-server hooks call it at
the history-load seams.

## Host obligations

- Canonical ISO timestamps only for work-queue APIs.
- No production SDK clock expecting event-stamp provenance.
- Certify at rollout/replacement-history level.
