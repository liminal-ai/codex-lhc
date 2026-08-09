# LHC schema ask — turn-scoped host facts

Three fields LHC has no slot for. Found while working out whether a Codex
rollout file could be regenerated from an LHC thread. They are not
Codex-specific: grok hits the same ones, and any host serving display state
would.

Written for whoever picks up the LHC side. Analysis first, ask at the end.

Fuller context for why this came up — the rollout-as-projection work it fell
out of — is in [`rollout-design-notes.md`](rollout-design-notes.md), including
the sequencing: these fields land in TypeScript `lhc` first, then `lhc-rs`,
then a vendor pin bump in the codex fork, then capture work here to supply
them.

---

## Where this came from

Codex persists its session to an append-only rollout JSONL. That file serves
**two** readers, not one:

- the **model** stream — `ResponseItem`s, folded into history by
  `core/src/session/rollout_reconstruction.rs`;
- a **display** stream — persisted `EventMsg`s (`rollout/src/policy.rs:98-117`):
  `TokenCount`, `TurnStarted` / `TurnComplete` / `TurnAborted`,
  `ThreadSettingsApplied`, `ThreadGoalUpdated`, `ThreadRolledBack`,
  `UserMessage`, `AgentMessage`, and others. Codex's app-server reads this
  stream — `request_processors/token_usage_replay.rs`,
  `bespoke_event_handling.rs`, `thread_goal_processor.rs`,
  `external_agent_migration/session_importer.rs` — and it is how the Codex
  desktop app renders a thread.

The codex-lhc capture hook rides `send_raw_response_items`, so LHC holds the
model stream and none of the display stream.

We are considering rewriting the rollout on compact instead of appending to
it, so that it holds only the current materialized view plus subsequent
turns. That makes the question concrete: what can be regenerated from an LHC
thread, and what cannot.

Most of the display stream is either host bookkeeping we would carry forward
from the previous file, or derivable. Three things are neither.

## What LHC has today

From `packages/lhc-rs/src/threads/internal/create.rs`:

```
event   (event_order, event_kind, idempotency_key, actor, harness,
         payload, recorded_at)
turns   (turn_id, turn_order, status CHECK(status IN ('open','closed')),
         opened_at_event_order, closed_at_event_order, deleted_at)
message (message_id, source_event_order, kind, token_estimate, actor,
         harness, turn_id, deleted_at)
```

## The three gaps

### 1. Provider token usage

`message.token_estimate` is LHC's own estimate, used for band sizing. It is
not what a provider reports and is not interchangeable with it.

Codex's `TokenUsage` (`protocol/src/protocol.rs:2056`) carries
`input_tokens`, `cached_input_tokens`, `cache_write_input_tokens`,
`output_tokens`, `reasoning_output_tokens`. It arrives on the wire at
`ResponseEvent::Completed { token_usage, .. }` (`core/src/client.rs:2002`),
so the host already holds it at the moment it is recording the assistant
turn's events — no new plumbing, nothing to reconstruct later.

Note the granularity: usage is **per model call**, and one turn may contain
several calls in a tool loop. Codex keeps both a running total and the last
call's figure (`TokenUsageInfo`). Whichever LHC stores, the distinction
should be explicit rather than implied.

Grok receives the same fact on its own completion path.

### 2. Turn outcome

`turns.status` is `open | closed`. A turn that the user interrupted is
indistinguishable from one that ran to completion.

Codex distinguishes `TurnComplete` from `TurnAborted`, and the abort carries
a reason. Grok's Replace path has the same interruption cases.

This is not only a display concern. A record that cannot tell a finished turn
from an abandoned one is weaker input for derivation, since an aborted turn's
content means something different.

### 3. Wall-clock turn timing

`turns` carries `opened_at_event_order` and `closed_at_event_order` —
ordering integers into the event table, not times. Timing is *derivable* by
joining to those events and reading `event.recorded_at`, but that is host
**capture** time, which approximates rather than equals Codex's
`TurnStartedEvent.started_at` / `TurnCompleteEvent.completed_at`.

Good enough for display; not for anything reconciled against the host's own
numbers.

## Not gaps

Recorded so they are not re-derived:

- **Inter-agent traffic.** Reaches codex-lhc capture through
  `send_raw_response_items` with `RawItemProvenance::InterAgent`
  (`core/src/session/mod.rs:3142`). Already in the record.
- **Model / thinking level.** Captured as `model_change` /
  `thinking_level_change`.
- **`git_head` / session metadata.** Present in both hosts (grok's
  `SummaryPatch`, Codex's `SessionMeta.git`) but session-scoped, not a turn
  fact. If it belongs anywhere it is thread metadata, and we are not asking
  for it.

## Related, but a host problem, not a schema one

Codex's `ThreadRolledBack` drops the newest N user turns. LHC never sees it,
so its record still holds those turns and a regenerated file would resurrect
content the user rolled back. LHC already has delete / visibility-boundary
primitives in the messages domain; this is a wiring gap on the Codex side,
not a missing field. Flagged only so it is not mistaken for one.

## The ask

Add turn-scoped slots for:

1. **provider token usage** — the provider's own counts, with the per-call vs
   per-turn distinction made explicit;
2. **turn outcome** — beyond `open|closed`, distinguishing completed from
   aborted, with room for a reason;
3. **wall-clock turn timing** — host-reported start/end, distinct from
   `event.recorded_at` capture time.

TypeScript `lhc` first, then `lhc-rs` — Lee's call, so the shape settles
against a real implementation before the Rust port has to match it. Both are
needed: pi-lhc and cc-lhc consume the TypeScript one, codex-lhc and grok-lhc
the Rust one, and divergence between them is its own cost.

Schema change, so it needs a migration; `shared_tech/thread_migrate.rs`
already exists for that.

All three are facts a host observes and currently discards. None require a
host to compute anything it does not already have.

## Open, not asked for

Whether LHC should eventually carry the display stream in full — enough that
a host GUI could be served from the thread rather than from a host file — is
a larger question and not part of this. These three are the ones that came up
as unavoidable.

---

**Source refs.** Codex paths are relative to the codex-lhc fork at upstream
base `61a44880a8`. Grok paths are relative to the grok-build fork.

- Codex display stream: `codex-rs/rollout/src/policy.rs:98-117`
- Codex reconstruction: `codex-rs/core/src/session/rollout_reconstruction.rs`
- Codex usage on the wire: `codex-rs/core/src/client.rs:2002`;
  `codex-rs/protocol/src/protocol.rs:2056`
- Codex capture hook: `codex-rs/core/src/session/mod.rs:3357`
- Grok persistence trait: `crates/codegen/xai-chat-state/src/persistence.rs:23`
  (note `replace_history`, and its file-rewriting implementation at
  `crates/codegen/xai-grok-shell/src/session/storage/jsonl/mod.rs:1834`)
- Grok session sidecar: `crates/codegen/xai-grok-shell/src/session/storage/summary_write.rs:77`
- LHC schema: `packages/lhc-rs/src/threads/internal/create.rs:31`
