# Slice B confirm pass — Opus round 2

Read `materialize.rs`, `materialize_tests.rs` (22 tests), and re-checked the consumer side (`rollout_reconstruction.rs`, `context_manager/history.rs`, `rollout/src/policy.rs`, `event_mapping.rs`). Ran `cargo test -p codex-lhc-host --lib materialize`: **22 passed, 0 failed**.

# Verdicts

| # | Verdict | Evidence |
|---|---|---|
| **C1** | **PARTIAL** | Marker no longer carried (`materialize.rs:226,1358-1359`); exclusion path + 3 tests (`materialize_tests.rs:476,699,830`). Residual: the segmenter re-implements reconstruction's arithmetic and diverges — see below. |
| **H2** | **RESOLVED** | Cumulative index `materialize.rs:251-270`; test asserts last=100/total=100 then last=50/**total=150** (`materialize_tests.rs:992-1000`). |
| **H3** | **RESOLVED** | Deferred pending-image + single completion (`materialize.rs:1058-1070,782-794`); test pins `ig_count == 1` and `end_count == 1` (`:1053,1063`). |
| **H4** | **PARTIAL** | Sweep now filters `t.status == TurnStatus::Closed` (`materialize.rs:836`); open-turn test at `:1099`. Ordering secondary not fully closed — see below. |
| **H5** | **RESOLVED** | `first_user_prompt_text` at `materialize.rs:176-178`; band emits ResponseItem only, no twin (`:510-524`); test asserts first UM == real prompt **and** zero `[context ·` twins (`:1183-1199`). |
| **M6** | **RESOLVED** | Classified by `MessageKind::RuntimeNote`, stored text, `with_twins=false` (`materialize.rs:705-733`); test asserts no-prefix text and no UM twin (`:1333-1354`). |
| **M7** | **RESOLVED (code), UNTESTED** | `materialize.rs:1367-1376` + gap #17 (`:119`). Grep for `ItemCompleted|InterAgentCommunication|ThreadSettingsApplied|…` in the test file: **0 hits**. |
| **M8** | **RESOLVED** | `is_error` from stored block → `success = !is_error` (`materialize.rs:796,1237-1238`); test asserts `output.success == Some(true)` (`:1273`). |
| **M9** | **RESOLVED** (slice-B scope) | Optional `turn_context` input + emission (`materialize.rs:147-149,211-213`), gap #18 rewritten (`:120`), test `:1531`. Production value depends on slice C supplying it. |
| **M10** | **RESOLVED** | `sanitize_id` rejects `synthetic:` (`materialize.rs:1255-1261`), call_id retention documented as gap #21 (`:123`); tests `:1555`, `:1774`. |
| **M11** | **RESOLVED** | Now asserts `command == ["echo","hi"]`, `call_id == Some("shell-1")`, `Search{query:"rust async"}` (`materialize_tests.rs:1408-1425`) — the `unwrap_or` empty-command fallback (`materialize.rs:1143-1151`) would fail it. |
| **M12** | **RESOLVED** | 5 biting assertions: goal carried, `Error` dropped, prior ResponseItem **not** copied, exactly one `Compacted`, prior `TokenCount(999)` **not** copied (`materialize_tests.rs:1498-1525`). Copy-everything now fails. |
| **M13** | **RESOLVED** | Self-referential test gone; `boundary_completeness_error` rejects `Some(vec![])` (`materialize.rs:1403-1405`) and the producer is pinned separately (`materialize_tests.rs:1789-1808`). |
| **L14** | **RESOLVED** (doc) | Table row now "Not emitted" (`materialize.rs:40`), gap #4 (`:106`), comment `:592`. |
| **L15** | **PARTIAL** | `decode_percent` collects contiguous bytes + UTF-8 decode (`materialize.rs:1305-1339`, bounds `i+2 < len` correct); `parse_host_id_from_key` tested (`:1774`). Positive end-to-end path untested — `assistant_text_tail`'s `key` is still never `Some` (`materialize_tests.rs:61`); only the synthetic case is wired inline (`:1573`). |
| **L16** | **RESOLVED** | All four branches + `None` (`materialize_tests.rs:1602-1620`). |
| **L17** | **RESOLVED** (documented as deliberate) | `materialize.rs:564` + gap #22 (`:124`). |
| **L18** | **RESOLVED** | Table row scoped "post-boundary closed turns only; open turns emit nothing" (`materialize.rs:36`), gap #12 (`:114`). |
| **L19** | **RESOLVED** | No count claimed (`materialize.rs:51`); list is 22 entries (`:103-124`). |
| **L20** | **UNRESOLVED** | All 22 tests still hand-build `SessionThreadView`/`MessageRecord`; nothing routes through `LhcSession`/`build_session_thread_view`. |

# (1) C1 — where positional alignment still mis-excludes

The prior-file scan (`prior_user_segments_with_drop`, `materialize.rs:323-391`) is an independent re-implementation of `finalize_active_segment` / the reverse loop. Interleaved rollbacks are correct (I traced `t1,t2,RB{1},t3,RB{1},t4` → drops t2,t3, matching `rollout_reconstruction.rs:73-78`). Four divergences remain:

1. **Contextual user messages inflate the segment count.** `materialize.rs:371-379` counts *any* `ResponseItem::Message{role:"user"}`; reconstruction uses `is_user_turn_boundary` (`context_manager/history.rs:780-790`), which excludes contextual fragments. Several such fragments are role `"user"` and are injected mid-session — `context/turn_aborted.rs:21`, `context/subagent_notification.rs:22`, `context/user_shell_command.rs:31`, `context/world_state/environment.rs:155`. Two effects: (a) inside a turn, the last reverse write wins (`:377`), so `segment_text` becomes the *fragment* text, not the prompt → text guard trips → under-exclusion + gap note on essentially every real file with an injection; (b) outside a turn (nothing after it, no `TurnStarted`), the trailing `finalize` at `:383` emits a **phantom segment**, shifting `offset = lhc.len() - segments.len()` (`:458`) by one.
2. **The text guard doesn't catch the shift when texts repeat.** This is the live mis-exclusion: with repeated identical prompts (`"continue"` ×N), a phantom or missing segment shifts the zip, every text comparison still passes at `:461-469`, and the `dropped` flags land on the **wrong `turn_id`s** — a live turn is excluded and a rolled-back one is kept. Silent: no gap note, because the mismatch branch never fires.
3. **`TurnStarted` finalization ignores turn-id compatibility.** `materialize.rs:363-370` finalizes on any `TurnStarted`; reconstruction only when `turn_ids_are_compatible` (`rollout_reconstruction.rs:251-258`). Any nested or misordered `TurnStarted` (including the H4 residual below) splits a segment here that reconstruction would merge.
4. **`InterAgentCommunication` isn't counted.** `rollout_reconstruction.rs:276-279` sets `counts_as_user_turn = true`; the materializer's scan falls through `_ => {}` (`materialize.rs:380`). And carry-forward appends these at the file end (`:1373-1375`), so next generation they attach to the newest segment.

Two smaller ones: leftover `pending > 0` after the scan (rollback deeper than the post-boundary region) is discarded with **no gap note** (`:383-390`); and `entry_belongs_to_rolled_back` requires `ids.iter().all(...)` (`:862-866`) while the User and ToolResult arms only re-check `.first()` (`:741,776`), so a view entry sourcing messages from both a rolled-back and a live turn survives whole, carrying rolled-back content into the tail.

**Empty-text prompts specifically are benign**: an empty user ResponseItem sets `is_user` without overwriting `segment_text` (`:376-378`), the segment falls back to `""` (`:343`), and LHC `stored_text` of a text-less prompt is also `""` — both sides degrade identically. Its only effect is contributing to the phantom-segment count in (1b).

Net: the reported failure (marker appended at end → newest live turns dropped) is gone and the fallback is under-exclusion, but the arithmetic is not provably the same arithmetic, and case 2 is a silent live-turn loss.

# (2) H5 — confirmed

No band display twins remain anywhere: `emit_band_entry` pushes only `RolloutItem::ResponseItem` (`materialize.rs:522-523`), and `push_response_with_twins` is called exclusively from `emit_tail`. The only EventMsg pushes before the first `UserMessage` are `ThreadSettingsApplied`/`ThreadGoalUpdated` (`:1343-1353`), so the first `UserMessage` in file order is necessarily the true first prompt — which is what `thread_metadata_sync.rs:306-323` reads. Two untested low residuals: with no `MessageKind::UserPrompt` row, **no** `UserMessage` is emitted at all (title/preview/`first_user_message` go empty rather than wrong), and `stored_text` joins multi-block prompts with `\n` (`:280-286`), so the preview can differ from the live one.

# (3) Vacuous-test replacements — all three bite

Read the assertions, not the names: M11 checks the decoded `command` vector and `call_id`, so the `unwrap_or(command: vec![])` fallback fails it. M12 has four assertions a copy-everything implementation fails (prior ResponseItem absent, one `Compacted`, no `TokenCount(999)`, no `Error`). M13's replacement pins the *producer* (`:1789-1808`) and `boundary_completeness_error` now rejects `Some(vec![])`.

Remaining coverage holes: the `local_shell` tool **result** is in the M11 fixture (`:1379`) but unasserted; `tool_search` call/output and the `unwrap_or_default()` empty-tools fallback (`materialize.rs:1221`) are wholly untested; and M12 covers only the goal row — `ThreadSettingsApplied`, review/patch/MCP/subagent, plan/sleep and inter-agent carry-forwards have zero assertions.

# New issues introduced by the fix rounds

- **`looks_like_image_result` misroutes non-image results.** `materialize.rs:1250-1252,785` — any tool result whose text contains both `"revisedPrompt"` and `"result"` becomes an `ImageGenerationCall`, dropping the `FunctionCallOutput` and orphaning the `FunctionCall`. Checked before the `tool_search` branch.
- **Unpaired image lands outside its turn.** The pending flush at `:819-828` runs after the entry loop, so the degraded `ImageGenerationCall` is emitted *after* the in-loop `TurnComplete` for its own turn (`:808-815`). Not asserted by the H3 test.
- **H4 ordering, residual.** A `Closed` turn whose last `member_message_ids` entry has no view entry never closes in-loop (`:934-940`) and falls to the end sweep (`:833-846`), placing its `TurnComplete` after newer `TurnStarted`s. Reconstruction then refuses to finalize at those `TurnStarted`s (`rollout_reconstruction.rs:251-258`) and merges segments — feeding divergence (3) above. Untested.
- **H2, residual.** `TokenCount` is emitted only on assistant **Text** parts (`materialize.rs:1021-1034`); usage carried on a tool-call or thinking message never produces an event, so if the newest usage-bearing message isn't a text message the newest `TokenCount` undercounts the session total. Separately, `cumulative_usage_index` (`:251-270`) sums rolled-back turns' usage even though those turns are excluded from the tail. Neither is in `CAPTURE_GAPS` (#11 covers only the pre-slice-A undercount).