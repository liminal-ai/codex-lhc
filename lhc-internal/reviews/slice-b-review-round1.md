# Slice B review round 1 — Opus findings (verified by Fable: C1/H2/H4/H5 confirmed in code)

All 11 tests pass locally (`cargo test -p codex-lhc-host --lib materialize`). Findings below, most severe first.

---

## CRITICAL

**C1. Carried-forward `ThreadRolledBack` is appended at the *end* of the rebuilt file → resume drops the newest N live turns.**
`materialize.rs:197` → `push_carry_forwards_rollback_and_ends` (`materialize.rs:961-975`) appends rollback markers after all post-boundary content. Reconstruction scans **newest-to-oldest** and treats a `ThreadRolledBack` as "skip the next N finalized user-turn segments" (`core/src/session/rollout_reconstruction.rs:188-190`, `finalize_active_segment` at `:73-78`). In the original file the marker sat mid-stream, so it dropped the turns *before* it. In the rebuilt file it is the newest item, so it drops the N *newest post-boundary* user turns — turns that were never rolled back. Their world-state replay and window metadata are skipped too (`:80-92`).
Failure: session with one rollback of 2 turns, compact, then two more user turns → rewrite → resume loses both new turns. This also inverts plan matrix item 4 ("dropped turns stay dropped"): the rolled-back content is *still present* (LHC never captured the rollback, gap #14), while live turns get dropped instead. No test asserts position or reconstruction semantics — `carry_forward_set_present_and_absent` (`materialize_tests.rs:664-672`) only asserts the event exists somewhere.

---

## HIGH

**H2. `TokenCount.total_token_usage` is set to the *per-call* usage → resumed token accounting collapses.**
`materialize.rs:886-896` sets `total_token_usage = last_token_usage = <this call's provider_usage>`. Two verified consumers read the newest `TokenCount` as the session/thread total: `core/src/session/mod.rs:1490-1495` (`last_token_info_from_rollout` → restored `TokenUsageInfo`) and `thread-store/src/thread_metadata_sync.rs:270-273` (`update.token_usage = info.total_token_usage`). After a rewrite, `/context` and thread metadata report the last model call's tokens instead of the session total. Not in `CAPTURE_GAPS` (which mentions only `rate_limits`/`model_context_window`, line 105) and not pinned: `banded_thread_with_tool_heavy_tail…` (`materialize_tests.rs:434-440`) asserts only `last_token_usage.total_tokens`. This is exactly the counter the plan's layer-3 item 5 intends to certify.

**H3. `image_generation` round-trips to *two* `ImageGenerationCall` items and two `ImageGenerationEnd` events.**
The tool-call arm emits a degraded `ImageGenerationCall{status:"unknown", result:""}` (`materialize.rs:697-720`), and the paired tool-result arm emits a second, complete `ImageGenerationCall` with the same id (`materialize.rs:822-850`). One forward `ResponseItem` (`mapping.rs:309-349`, which deliberately emits a call+result pair) becomes two duplicated items in the tail plus two display end-events. `CAPTURE_GAPS[5]` (`materialize.rs:100`) documents only the *unpaired* case, implying the paired case is clean. No test covers image generation at all. The comment block at `materialize.rs:698-707` is a stream-of-consciousness design deliberation left in the shipped source and does not describe what the code does.

**H4. The turn-close fallback fabricates `TurnComplete` for turns that are still open, contradicting the guard 10 lines above.**
`maybe_close_turn_if_last` deliberately refuses to close non-`Closed` turns (`materialize.rs:586-588`: "open turns stay open — no TurnComplete yet"), but the sweep at `materialize.rs:503-509` closes every `opened && !closed` turn with no `TurnStatus` check. An in-flight turn (the normal state when a compact fires mid-turn) gets a synthesized `TurnComplete`. Secondary effect: these are emitted at the very end in `turn_order` order, so an older turn's `TurnComplete` can land after a newer turn's `TurnStarted`, which mis-segments the reverse replay (`rollout_reconstruction.rs:193-201, 246-270`). Every fixture uses `TurnStatus::Closed` (`materialize_tests.rs:175`), so the path is untested.

**H5. Thread title / preview / `first_user_message` regress to the band summary text.**
Band entries get display twins (`materialize.rs:286-294` → `301-331`), so the first `UserMessage` event in the rebuilt file is `"[context · brief]\n…"`. `thread-store/src/thread_metadata_sync.rs:306-323` takes title, preview and `first_user_message` from the first `UserMessage` seen. Today's append arm keeps the real first prompt at the top of the file; the rewrite replaces it with a compaction artifact. Not in the disposition table, not in `CAPTURE_GAPS`, no test. (The plan's slice-D list names "thread-store user-message reads" as a consumer to certify — this is the regression it would catch.)

---

## MEDIUM

**M6. Runtime notes are replayed into the model stream as user turns, with the view's `[runtime note] ` prefix baked in.**
The LHC session view renders every `runtime_note` as a `SessionUserMessage` with a literal prefix (`vendor/.../session_view.rs:239-250`). `emit_tail` (`materialize.rs:457-463`) cannot distinguish those from real prompts, so host scaffolding (`HostContext` injections, `AgentMessage`, `AdditionalTools`, `Compaction` encrypted blobs — `mapping.rs:120-152, 350-404, 576-579`) comes back as a user-role `ResponseItem` **and** a visible `UserMessage` display event, with text that differs from the original by the prefix. `CAPTURE_GAPS[7]`/`[8]` (lines 102-103) cover the AgentMessage/AdditionalTools *shape* loss but not the prefix injection, the user-role/display promotion, or the fact that each such note now counts as a user-turn boundary in reconstruction (`rollout_reconstruction.rs:214-218`) — which interacts with C1's turn-dropping arithmetic.

**M7. Persisted rollout items with no reverse handling and no gap entry: `ItemCompleted(TurnItem::Plan | Extension(Sleep))` and `InterAgentCommunication{,Metadata}`.**
`rollout/src/policy.rs:88-96` persists plan/sleep `ItemCompleted` in Legacy mode, and `:11-13` persists inter-agent items unconditionally. The materializer neither regenerates nor carries them forward (`materialize.rs:949-975`), and neither appears in `CAPTURE_GAPS`. Plan items silently vanish on the first rewrite; inter-agent items lose content that reconstruction feeds back into history (`rollout_reconstruction.rs:276-279, 331-337`).

**M8. `FunctionCallOutput.success` silently degrades `Some(true)` → `None`.**
Forward: `success: Some(true)` → `is_error: Some(false)` (`mapping.rs:733`). The session view collapses anything non-error to `None` (`session_view.rs:145,150`). Reverse: `success = is_error.map(|e| !e)` → `None` (`materialize.rs:870-877`). Every successful tool output in the tail loses its explicit success flag. Not in `CAPTURE_GAPS`; the tool-heavy test (`materialize_tests.rs:404-403`) asserts only the body text.

**M9. `previous_turn_settings` is lost on every rewrite, beyond what the gap text admits.**
`CAPTURE_GAPS[15]` (`materialize.rs:110`) says only that `TurnContextItem` is not regenerated "(reconstruction clears reference_context at Compacted)". But `previous_turn_settings` (model, `comp_hash`, `realtime_active`) is sourced *exclusively* from `RolloutItem::TurnContext` (`rollout_reconstruction.rs:220-243`), so a rebuilt file always resumes with it `None`. Also disables the reverse-scan early-exit at `:286-294`.

**M10. Synthetic/defaulted ids leak into the tail.** When the original `call_id`/`id` was `None`, the forward map minted `synthetic:{digest}` (`mapping.rs:176-177, 211-212, 274-275, 295-297, 317-319`); the reverse writes that string back as a real `ResponseItemId`/`call_id` (`materialize.rs:751-786`). `CAPTURE_GAPS[4]` covers defaulted *status*, not fabricated ids.

**M11. `reverse_maps_local_shell_and_web_search_to_native_kinds` is vacuous on payload** (`materialize_tests.rs:717-731`). It asserts only `matches!(… LocalShellCall{..})` / `WebSearchCall{..}` / "some `WebSearchEnd` exists". A reverse map that hits the `unwrap_or` fallback and emits `LocalShellCall{command: vec![]}` (`materialize.rs:742-750`) — total loss of the command — passes. Same for a `WebSearchCall` with `action: None` and an empty query, and for the dropped `local_shell` tool result. This is the one arm where a silent deserialization fallback exists, and the test cannot see it.

**M12. `carry_forward_set_present_and_absent` passes with a copy-everything implementation** (`materialize_tests.rs:632-673`). The "absent" half uses `prior = &[]` (trivially true), and the "present" half asserts only that two events survive. Nothing pins that transient events (`Error`, `ExecCommandEnd`, collab/realtime — `policy.rs:120-170`) are dropped, that prior `ResponseItem`s/`Compacted`/`TokenCount` records are *not* copied (which would break the one-boundary invariant), or that `ThreadSettingsApplied` is carried. No test ever supplies a realistic prior generation — i.e. an actual parsed prior rollout — which is the only input this parameter ever receives in production.

**M13. `mutation_targets_boundary_fields_are_the_sharpest_invariants` (`materialize_tests.rs:797-829`) is redundant and self-referential** — it asserts `boundary_completeness_error` against hand-built `CompactedItem`s, duplicating `materialize_tests.rs:317-327`, and adds one materialize call already covered at `:292-313`. It tests the checker, not the producer. Related: `boundary_completeness_error` accepts `Some(vec![])` (`materialize.rs:996`), so a materializer that emits an *empty* replacement history passes both tests and both "mutation" assertions.

---

## LOW

- **L14.** `AgentReasoningRawContent` (a disposition-table row) is dead code: the twin fires only when `encrypted_content` is `Some` (`materialize.rs:349-353`), and the reverse map hardcodes `encrypted_content: None` (`materialize.rs:684`). Unreachable, untested.
- **L15.** `decode_percent` (`materialize.rs:928-945`) pushes raw bytes as `char`, mojibaking any non-ASCII id. More importantly `parse_host_id_from_key` is entirely untested — and the `key` parameter of the `assistant_text_tail` fixture (`materialize_tests.rs:57`) is never passed as `Some` by any test, so the whole id-recovery path (the mitigation for `CAPTURE_GAPS[2]`) is dead in the suite.
- **L16.** `map_abort_reason` (`materialize.rs:632-647`): the only test (`materialize_tests.rs:596-627`) uses `"interrupted by user"`, which maps to the same value as the `None`/unknown default. A constant `TurnAbortReason::Interrupted` passes; the `replac`/`review`/`budget` branches are unexercised.
- **L17.** Display twins are pushed *before* their `ResponseItem` (`materialize.rs:291-293`), inverting the live append order. Harmless for reconstruction, visible to any order-sensitive display consumer.
- **L18.** Pre-boundary turns get no `TurnStarted`/`TurnComplete` at all (`emit_tail` only) — defensible, but the module table row reads as unconditional.
- **L19.** `CAPTURE_GAPS` has **17** entries (`materialize.rs:95-111`), not 13.
- **L20.** Law 9 (rule zero): every fixture is hand-built rather than read back through a real `LhcSession`/`build_session_thread_view`. I spot-checked the shapes against `session_view.rs` and they are faithful (band = empty `source_messages`, one source per assistant part, `is_error` never `Some(false)`), so this is a process gap rather than a wrong-fixture bug — but it is why M8 and M6 went unnoticed.

---

## Q1 — disposition-table row → covering test

| Row | Covering test | Verdict |
|---|---|---|
| `TurnStarted`/`TurnComplete` | `banded_thread_with_tool_heavy_tail…` (`:423-432`), `pre_slice_a…` (`:572-581`) | covered |
| `TurnAborted` | `aborted_turn_regenerates…` (`:596`) | covered, reason-map branches not (L16) |
| `TokenCount` | `banded_thread…` (`:434-440`), absence in `pre_slice_a…` (`:582-587`) | present only; totals semantics unpinned (H2) |
| `UserMessage` / `AgentMessage` / `AgentReasoning` | `legacy_display_twins…` (`:763-778`) | covered |
| `AgentReasoningRawContent` | **NONE** — unreachable (L14) |
| `ThreadSettingsApplied` | **NONE** |
| `ThreadGoalUpdated` | `carry_forward…` (`:665-668`) | presence only |
| `ThreadRolledBack` | `carry_forward…` (`:669-672`) | presence only; position/semantics unpinned (C1) |
| `ContextCompacted` | `legacy_display_twins…` (`:779-783`) | covered |
| `WebSearchEnd` | `reverse_maps…` (`:727-731`) | presence only, no fields (M11) |
| `ImageGenerationEnd` | **NONE** (H3) |
| Review / patch / MCP / subagent ends (carry-forward) | **NONE** |
| Transient → dropped | **NONE** (M12) |

**CAPTURE_GAPS pinned by an assertion on the degraded behavior:** 1 of 17 — #17 pre-slice-A (`pre_slice_a_missing_host_facts_degrade_honestly`). Partial: #1 FunctionCall-always (implicit in the tool-heavy test), #4 reasoning-as-summary (shape asserted only as `Reasoning{..}`), #13 abort-reason (default branch only), #14 carry-forward (goal + rollback only). The other 12 are comments only.

## Q2 — reverse-map fidelity vs `mapping.rs`

Every forward `ResponseItem` arm has reverse handling or a gap entry, **except** the persisted non-`ResponseItem` rollout classes in M7. Silent divergences that are *not* documented gaps: C1 (rollback position), H2 (token totals), H3 (image-gen duplication), H5 (preview source), M6 (runtime-note prefix + user-role promotion), M8 (`success` flag), M9 (`previous_turn_settings`), M10 (synthetic ids).

## Q3 — vacuous tests

M11, M12, M13 are the three that would pass against a materially degenerate implementation. Weaker-but-not-vacuous: `structure_…:277` (`>= 1`), `banded_thread…:450` (`hist.len() == model_stream_response_item_count`, both derived from the same emission), `adversarial…` (name promises "boundary floats" but the float is never asserted anywhere in the output — `:472, :549`). `iso_to_unix_secs_round_trips_host_format` is a genuine round-trip against `mapping::unix_secs_to_iso`.

---

## What I did NOT check

- **Nothing was edited or run beyond `cargo test -p codex-lhc-host --lib materialize`.** I did not run clippy, the full crate suite, the tripwire (`scripts/check-lhc-hooks.sh`), or any mutation experiment — every "this test would still pass" claim above is from reading, not from breaking the code and watching.
- I did not verify the plan's own "verified" claims about `model_context.rs` Paginated stop conditions, atomic-rename/fsync semantics, or recorder-handle reopen — those are slices C/E and no code for them exists yet.
- I did not audit `compact_bridge.rs` (1645 LoC), `capture.rs`, `install.rs`, or `band_shape.rs`; only `mapping.rs`, `idempotency.rs`, the vendored `session_view.rs`, `policy.rs`, `rollout_reconstruction.rs`, `thread_metadata_sync.rs` and `token_usage_replay.rs` as needed for the three questions.
- I did not check whether resume re-captures rebuilt items back into LHC (which would determine whether the M6 prefix accumulates across generations); the record path at `core/src/session/mod.rs:2998` suggests it does not, but I did not trace the resume load path to be sure.
- No end-to-end or live verification: I did not materialize a real thread, write a file, or resume from one.
- `CAPTURE_GAPS` entries were checked for *test* coverage, not for factual accuracy against the LHC schema.