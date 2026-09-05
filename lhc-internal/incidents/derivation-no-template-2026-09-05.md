# Incident: fork derivations answered the prompt instead of transforming it

Date found: 2026-09-05. Thread: Alder (codex-lhc in t3code, `01a06ea2…`).
Status: record hand-repaired (115 rows). Code not yet fixed.

## What happened

Every model-produced derivation on the thread was a reply, not a transform.
Smoothed prompts read like the model answering Lee. Turn compressions read like
the model answering the turn. Chunk briefs the same. 46 of 51 smoothings, all 46
compressions, all 21 briefs.

## Which model

`gpt-5.6-luna`, pinned in `codex-rs/core/src/lhc_inference_bridge.rs`
(`LHC_DERIVATION_MODEL`), at the lowest accepted reasoning effort.
Only this model ran derivations on the thread. The config model
(`gpt-5.6-sol`) is the agent, not the deriver.

**Luna is not at fault.** It was never told to transform anything.

## Root cause

The fork bypasses the SDK's inference adapter. The SDK ships
`create_inference_callbacks(ResolvedInferenceConfig)` in
`lhc-rs/src/shared_tech/inference_adapter.rs`. It looks up the prompt template
by name (`smoothing-v1`, `detailed-turn-compression-v1`, `chunk-brief-v3`,
`tool-result-v2`), renders instructions plus content into messages, then calls
the host's `ModelCall`. cc-lhc and pi-lhc use it.

The fork instead builds its own `InferenceCallbacks` in
`callbacks_from_live_ctx` (`lhc_inference_bridge.rs`, since commit `3aa3a44d22`,
2026-07-26, "Chunk 2 — LHC compact bridge with real derivation"). Each callback
hands the raw input field straight to the model:

| callback | what is sent |
|---|---|
| `smooth_prompt` | `input.text` (Lee's cleaned prompt, verbatim) |
| `summarize_tool_result` | `input.content` |
| `compress_detailed_turn` | `input.dialogue_text` (the turn assembly) |
| `summarize_chunk_brief` | `input.text` |

`derivation_prompt()` wraps that as one `user` message with
`instructions: ""` and `base_instructions: ""` (deliberately, to avoid the
20k-char Codex agent prompt). No template is rendered anywhere on the path.
The size targets the SDK computed for compression (`target_min/aim/max`) are
dropped on the floor.

So the model receives a bare user message and does the only sensible thing:
it answers it.

Tool result summaries were unaffected. They came out as deterministic head
slices, not model output.

## Why it went unnoticed

- Every call "succeeded". Metadata says `inferenceSucceeded: true`.
- The smoothing guard only checks output-too-short. A long reply passes.
- Compression size checks flagged 34 of 43 as `under_min` and nobody read that
  signal.
- The fork's certification measured token cost (B3), not output shape.
- The agent read its own smoothed history as "Lee's" words for 40+ turns.

## The 7 unambiguous cases

These are the ones that cannot be mistaken for a paraphrase. All are Lee's
prompts; the "output" column is what was stored as the smoothed version.

| msg | turn | Lee said | model returned |
|---|---|---|---|
| m78 | t7 | links to three design docs | "Thanks for sharing… What would you like me to do with them?" |
| m282 | t18 | "Do a write-up… divide into slices" | "Sure. Please send the project context, goals…" |
| m704 | t22 | "dfce62fd68 verified… rerun the tripwire… merge" | "I can't rerun tests or merge without repository access." |
| m804 | t24 | "Ok, please continue." | "Sure—please share the text or context you'd like me to continue." |
| m1874 | t42 | roadmap; "run `claude-subagent`" | "I can't execute shell commands in this interface…" |
| m1904 | t43 | AGENTS.md eval, effort guidance, watchdog | "Please share the onboarding steps… for the LHC goal utility." |
| m1912 | t44 | "Would you like to choose a name?" | "I'd like to choose for myself: **Aster**." |

The last one is why Alder's record briefly contained a second name.

18 more are reply-shaped ("Understood.", "Agreed.", "Got it.", "Yes.").
21 more are silent rewrites: same topic, the model's framing and words.
Full list with verbatim input and output: `derivation-no-template-2026-09-05.repro.json`
in this directory. Source: the pre-repair backup
`~/.codex/lhc/repair-backups/01a06ea2-manual-smooth-20260905T143635Z.sqlite`.

## Reproduction

Send any case's `input_sent_verbatim` to `gpt-5.6-luna` as a single user
message, empty instructions, lowest effort. Expect an answer, not a rewrite.
Then send the same content through `smoothing-v1` rendered messages. Expect a
rewrite. That pair is the before/after test for the fix.

## Fix (2026-09-05, branch `fix/derivation-bridge-template`)

1. **Done.** `callbacks_from_live_ctx` now hands the SDK adapter a
   `ModelCall` and calls `create_inference_callbacks`. The four raw
   passthroughs are gone. Assignments take prompt names from the SDK's
   `DEFAULT_PROMPT_NAMES`; size ratios mirror the SDK's private defaults and
   are re-checked on a pin move. `derivation_prompt` maps rendered system
   messages to `base_instructions` (rides as a developer message on the lite
   lane) and user messages to the input. The agent prompt stays out.
   Stream failures now carry a typed `ModelCallFailureKind`, not a string.
2. **Done.** Offline goldens in the bridge tests: per lane, the outbound
   payload equals the rendered template, carries the input only wrapped,
   and contains no agent-prompt marker. The J2 wiremock test asserts the
   same on the actual HTTP body for all four lanes.
   **Live check (release build e23ef782, three `codex exec` turns, throwaway
   LHC root):** the derivation log shows the smoothing-v1 template on the wire
   (6,277-char instructions + wrapped input). Turn 2's prompt came back as a
   rewrite: "ok now reply with exactly BRIDGE_OK_2. dont explain anything, i
   just wanna see the reply come thru" -> "Okay, now reply with exactly
   BRIDGE_OK_2. Don't explain anything; I just want to see the reply come
   through." Turn 1's smoothing fell to the floor with `stream closed before
   response.completed`: `codex exec` exits at turn end and cuts in-flight
   derivations. Host lifecycle, not the bridge; app-server hosts keep the
   process alive. Installed into both 0.153.3 trees with the prior binary
   kept as `bin/codex.pre-bridge-fix-20260905`. Running app-servers keep the
   old binary until restarted.
   **t3code live check (after `systemctl --user restart t3code-3773.service`,
   16:51Z):** fresh seat thread on instance `codex`, model `gpt-6-astra`,
   three injected turns. App-server spawned from the installed 0.153.3 binary
   (not the deleted inode). Thread file opened at schema 13. Both smoothed
   prompts are rewrites of the source with `[from: lee]` kept. The multi-tool
   turn's compression is a third-person summary with provenance
   `detailed-turn-compression-v3`, `inferenceSucceeded: true`,
   `sizeDisposition: in_range`. Fix merged to `lhc` (459a17d3ce) and pushed.
3. Open: SDK-side reply-shape guard for all hosts.
4. Open: triage scan of live threads by source overlap.
5. Open: re-derive this thread by machine and diff against the hand repair.

## Open question

The same bypass has been in place since July. Any other codex-lhc thread that
compacted with live inference since `3aa3a44d22` has the same contamination.
Check those before adding them to the control plane.
