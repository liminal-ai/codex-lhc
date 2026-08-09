# Rollout ownership — problem space and where the thinking is

Working notes, not a spec. It records what the problem is, what was found in
the code, and the direction currently favoured, so the next person does not
have to rediscover it.

**As-built state** — what actually runs today — is in
[`../../FORK.md`](../../FORK.md)
(touchpoint inventory, laws, tripwire) and
[`../../codex-rs/lhc/CHUNK3-CERTIFICATION.md`](../../codex-rs/lhc/CHUNK3-CERTIFICATION.md)
(what is verified and what is not). Nothing described here is built yet; the
compact arm currently appends through `replace_compacted_history` and works.

The LHC-side schema request that came out of this analysis is
[`lhc-schema-ask.md`](lhc-schema-ask.md).

---

## The problem

Codex's rollout JSONL is append-only. Compaction appends a
`RolloutItem::Compacted` carrying the replacement history; nothing is ever
removed.

With LHC's compaction that appended body is a full banded view rather than a
short summary, so the file grows faster per compact than upstream's would.
And LHC exists precisely to make months-long threads viable, so the sessions
it enables are the ones that suffer most.

At resume the read cost depends on the thread's history mode, which is **not**
a single behaviour — see "Two history modes" below. For terminal Codex it is a
full read and full parse of the whole file before a session can start.

The more fundamental issue is not size. The same conversation lives in the
rollout and in LHC's per-thread SQLite, and while LHC is the source of truth
that makes the rollout a second one.

## What the code actually does

**Live session.** `replace_compacted_history` (`core/src/session/mod.rs`)
does two independent things: `state.replace_history(...)` swaps the in-memory
history, and `persist_rollout_items` appends a `Compacted` record. The
in-memory copy serves the next turn. Nothing reads the file back.

**Resume.** `rollout_reconstruction.rs` walks the loaded items **backward**,
stopping at the newest `Compacted` that carries `Some(replacement_history)`.
That becomes the base, and only items after it are replayed forward. The code
notes this directly at the forward loop's `Compacted` arm: reaching it
"should actually never happen, because the reverse loop above should stop
before any compaction that has Some replacement_history."

So everything before the last compaction is read and parsed, then discarded.

**Two readers, not one.** Alongside the `ResponseItem` stream that feeds model
history, the rollout carries a persisted `EventMsg` stream — the display
record. LHC capture hooks `send_raw_response_items` only, so it has the model
stream and none of the display stream. Exactly what that stream contains is
mode-dependent; the survey is below.

## Two history modes

`ThreadHistoryMode` is `Legacy` (default) or `Paginated`, and the difference
is larger than a flag.

**Legacy** loads the whole file via `load_rollout_items` and parses every
line.

**Paginated** never calls that. `thread-store/src/local/model_context.rs:132`
opens each segment with a `ReverseJsonlScanner` and walks **backward**,
stopping early. The stop condition is `rollout/src/model_context.rs:84`:

- a `Compacted` carrying **both** `replacement_history` and `window_number`
  → the scan may stop;
- a `Compacted` missing either → must scan to the start;
- a `ThreadRolledBack` → must scan to the start, commented "Paginated threads
  reject rollback."

So under Paginated the compaction record is the marker a backward reader stops
at, and `window_number` is half that condition. Anything that removes those
records makes resume *more* expensive, not less.

**Which mode applies.** `history_mode` arrives as a parameter on the
app-server thread-start request (`thread_processor.rs:966`), gated on
`supports_paginated_history_lists` (the local store returns true). Nothing in
the TUI, CLI, or exec paths sets it — the only occurrences are
`Default::default()` in tests. So terminal Codex is always Legacy; the desktop
app may opt in, which is Electron-side and not visible from this repo.

Upstream also has a multi-file mechanism: `RolloutLineage` follows
`SessionMeta.history_base` pointers across segments, each with a
`start_ordinal` and an end `HistoryPosition`. It exists for forks. It is
recorded here as **considered and set aside** — adopting it would mean Codex
maintaining a second history topology alongside LHC's, which cuts against the
governing principle below.

## Patterns reviewed

**pi-lhc** — LHC-native host. PI's session is
`SessionManager.inMemory(cwd)`: never written to disk. At launch the
launcher reads `getSessionThreadView(threadRef)` and seeds that in-memory
session, then disposes its read-only SDK instance. Thread identity comes from
LHC's registry via `--lhc-thread` / `--lhc-continue` / `--lhc-resume`, and
PI's own `--session`/`--resume`/`--continue` are rejected outright. One
durable store, no host session file at all.

**cc-lhc** — wrapper around a closed CLI, so it cannot inject context into a
running session. Capture watches the rollout read-only. On compact it
*rotates*: `writeRebuiltRollout` mints a new session id, builds JSONL lines
from the thread view's entries, writes them to a **new path** with fsync, and
injects `/resume <newSessionId>` so the CLI hot-swaps. The original file is
only ever read (for the envelope) and never modified; old files are not
deleted. No single file grows without bound, though total bytes still do.

Codex sits between these: unlike cc-lhc we can modify the host, and unlike
pi-lhc we are adapting an existing rollout/context system rather than
designing around LHC.

## The governing principle

LHC's SQLite is the source of truth. It follows that **any host-side file is a
projection of the thread** — derived, disposable, regenerable — not a parallel
record. This is not a preference to be traded off; it follows from what the
integration is. It can be broken, but only for a compelling reason, and
breaking it should be argued explicitly rather than arrived at.

Consequences worth stating, because they reclassify most of the open work:

- A fact that can only live in the rollout is a **capture gap**, not a design
  tradeoff. Reading such facts from the previous file is a stopgap with a
  known end state, not the architecture.
- A retained prior generation is a **backup**, never authoritative.
- The compaction record kept at the boundary is a **rendering detail** — it is
  where a backward reader stops, and records nothing on its own.
- The conformance test is: delete the rollout, regenerate it from the thread,
  resume, and get an identical session. Whatever fails that is still held only
  in the projection.

## Where the thinking is landing

Rather than appending on compact, rewrite the rollout so it holds only the
latest materialized thread-view plus turns after it, with full history living
only in LHC's SQLite. Post-compact turns append to the full-fidelity band and
go to LHC as intake, as they already do.

The original framing was "no `Compacted` records at all." That was written
before the Paginated reader was found, and is superseded: exactly one such
record stays, at the boundary. It carries no history the file does not already
contain — it is where a backward reader stops.

Construction would read from both sources: LHC's SQLite for history, and the
existing rollout for what LHC does not hold — the latter being a stopgap, per
the governing principle, not the end state. The new file is written, then
swapped in by rename, keeping one prior generation rather than deleting
outright. Generation management is needed; one generation is a reasonable
starting point.

Settled in discussion:

- **Same identity** — same session id and path. cc-lhc mints new ids because
  a PTY wrapper cannot do otherwise; that constraint does not apply here, and
  same-path preserves `codex resume <id>` and everything keyed on it.
- **Inline** — the rewrite runs in the compact path. A file write is
  negligible against a compact that does no inference at all, let alone
  against native compaction's model call.
- **A compaction record stays** at the boundary of the rewritten file. Under
  Paginated it is where a backward reader stops; under Legacy it costs
  nothing.
- **LHC declining to compact is a defect to fix**, not a path to design
  around. No second growth path is built for it.
- **No multi-file lineage.** See "Two history modes".

Points in favour, from the code rather than assumption:

- Reconstruction already uses only the tail, so a file containing the view
  plus subsequent turns carries everything resume consumes.
- With no `Compacted` records, reconstruction never enters that branch and
  replays the file linearly.
- The app-server's reads show empirically what shape the Codex app needs,
  without having to model the whole GUI.
- It does not require replacing the rollout/context-management system.

It is acknowledged as somewhat janky. That is accepted against the
alternative of a deeper rewrite.

## Display stream — survey

From `rollout/src/policy.rs:87`, persistence is mode-dependent.

**Both modes:** `TokenCount`, `TurnStarted`, `TurnComplete`, `TurnAborted`,
`ThreadSettingsApplied`, `ThreadGoalUpdated`, `ThreadRolledBack`.

**Legacy only** (so: terminal Codex): `UserMessage`, `AgentMessage`,
`AgentReasoning`, `AgentReasoningRawContent`, `EnteredReviewMode`,
`ExitedReviewMode`, `PatchApplyEnd`, `ContextCompacted`, `McpToolCallEnd`,
`WebSearchEnd`, `ImageGenerationEnd`, `SubAgentActivity`.

**Paginated only:** `ItemCompleted` carrying `TurnItem`s — the richer display
record. Paginated stops persisting the Legacy duplicates above precisely
because they are re-derivable from the model stream.

**Persisted by neither** (declared transient): `Error`, `GuardianAssessment`,
`GuardianWarning`, `ExecCommandEnd`, the collab events, realtime,
`SafetyBuffering`, `ModelReroute`.

Consumers:

- **thread-store** — `UserMessage` (7 sites), `SessionMeta` (6), `TokenCount`,
  `ItemCompleted`, turn lifecycle.
- **app-server** — turn lifecycle, `ItemStarted`/`ItemCompleted`,
  `TokenCount`, `ThreadSettingsApplied`, `ThreadGoalUpdated`.
- **guardian** — reads `TurnComplete`/`TurnAborted` and its own assessments,
  but assessments are transient and never persisted, so guardian is a
  live-stream consumer, **not** a projection consumer.
- **TUI** — no rollout-item reads at all. Renders from the live event stream.

So for terminal Codex the projection needs: the model stream, turn lifecycle
with timing and outcome, token usage, settings and goal, and the Legacy
message/reasoning duplicates. Of those, three are the schema ask already in
flight, settings and goal are session-scoped config rather than turn facts,
and the duplicates are re-derivable.

## Sequencing

**Not blocked on anything.** The rewrite construction itself: materializing a
rollout from a thread, the write/fsync/rename swap, reopening the recorder,
generation handling. Also the regenerate-and-resume drill, which can be built
against the current append-based arm and will keep working after the change.

**Blocked on lhc-rs.** Capture of provider token usage, turn outcome, and
wall-clock timing. Adding schema slots does not fill them — once the fields
exist, codex-lhc capture has to start supplying them
(`ResponseEvent::Completed` for usage; turn complete/abort for outcome and
timing). Sequence is TypeScript `lhc` first, then `lhc-rs`, then a vendor pin
bump here, then fork capture work.

**Note on the pin.** The vendored LHC submodule currently sits on a side
branch rather than the certified `lhc-rs-port` line, and the tripwire warns
about it every run. That wants reconciling before chasing new commits on that
line.

**Independent of both.** Rollback wiring — LHC has the primitives, the fork
does not call them. Can be done at any point, and should be, since a
projection cannot be authoritative about turns the archive still holds.

## Open questions

Tagged by who resolves them: **[research]** someone can go find out,
**[build]** whoever implements decides, **[Lee]** needs a call.

**Rollback. [Lee]** The mechanics are clear; what needs a call is whether
rollback maps onto LHC's delete/visibility primitives (so the archive forgets
too) or whether the projection is allowed to differ from the archive here.
`ThreadRolledBack` is a marker; reconstruction applies it by
dropping the newest N user turns. LHC never sees it, so its record still
holds those turns and a regenerated file would resurrect them. LHC has delete
/ visibility-boundary primitives in the messages domain, but they are not
wired to this. Note upstream reached the same tension from the other side:
Paginated threads reject rollback rather than support it under a bounded
read.

**Legacy display duplicates. [research]** Re-derivable in principle, since Paginated
drops them for that reason. Whether regenerating them is faithful enough for
thread-store's reads is unverified.

**World state. [research]** A compaction resets the baseline and a fresh full snapshot
is persisted immediately after, so a rebuilt file needs one correct full
snapshot rather than a patch history. How that snapshot is produced is open.

**Recorder handle. [build]** The rollout recorder holds an open append-mode file
handle (`rollout/src/recorder.rs:1591`, `:1709`). An open fd follows the
inode through a rename, so a swap without reopening the recorder would leave
it appending to the previous file — silently. There is no file locking on
rollouts; the mutexes in the recorder are internal state, not `flock`, so
single-writer safety rests on one session owning a thread. Keeping the same
path does not change that.

**Existing rollouts. [build]** Files already written contain `Compacted` records and
must keep resuming. The reader stays dual-format indefinitely; "no compact
records" would only ever describe files we write. Easy to design as though
the old shape is gone.

**TurnContext. [research]** Absent, reconstruction clears `reference_context_item` and
re-injects canonical context, which the code comments accept as an
out-of-distribution prompt shape. Degraded rather than broken.

## Possible LHC-side gaps

Noted while checking what could be reconstructed; raised as observations, not
requirements.

`event` carries `recorded_at`. `turns` carries `opened_at_event_order` /
`closed_at_event_order` — ordering integers, not times — so turn timing is
derivable via the referenced events, as capture time rather than Codex's
`started_at` / `completed_at`.

Two things appear absent rather than derivable:

- **Provider token usage.** `message.token_estimate` is LHC's own estimate
  for band sizing, not the input/output/cached counts Codex's `TokenCount`
  events carry.
- **Turn outcome.** `turns.status` is `open|closed`; an aborted turn is not
  distinguishable from a completed one, and abort reasons have no slot.

Both are host-observable facts about a turn that any host would need if LHC
were to serve display state.

---

## Files read

Line numbers are as of upstream base `61a44880a8`.

**Codex — write path**
- `codex-rs/core/src/session/mod.rs:3219` — `replace_compacted_history`:
  in-memory `state.replace_history` then `persist_rollout_items` at `:3254`.
- `codex-rs/core/src/session/mod.rs:3115` — inter-agent record path; reaches
  capture via `send_raw_response_items` with `RawItemProvenance::InterAgent`
  at `:3142`, so inter-agent traffic is **not** a capture gap.
- `codex-rs/core/src/session/mod.rs:3357` — `send_raw_response_items`, the
  capture fan-out.

**Codex — read path**
- `codex-rs/rollout/src/recorder.rs:982` — `load_rollout_items`: full read,
  per-line `serde_json` parse into a `Vec`.
- `codex-rs/rollout/src/recorder.rs:1047` — `get_rollout_history`.
- `codex-rs/rollout/src/recorder.rs:1591`, `:1709` — `OpenOptions::append(true)`,
  wrapped as a `tokio::fs::File`; the open-handle constraint.
- `codex-rs/core/src/session/rollout_reconstruction.rs:154` — reverse scan;
  `:181` sets the base and truncates the suffix; `:325` forward replay;
  `:343` the "should never happen" comment; `:389-422` world-state replay,
  where `:393` resets the baseline at a compaction; `:430` the
  `RolloutReconstruction` output fields.

**Codex — the display stream**
- `codex-rs/rollout/src/policy.rs:98-117` — which `EventMsg`s persist.
- `codex-rs/app-server/src/request_processors/token_usage_replay.rs:77` —
  `rposition` for the newest `TokenCount`.
- `codex-rs/app-server/src/bespoke_event_handling.rs:2191`,
  `request_processors/thread_goal_processor.rs:423`,
  `external_agent_migration/session_importer.rs:340` — other consumers.

**Codex — protocol shapes**
- `codex-rs/protocol/src/protocol.rs:3186` — `RolloutItem` variants;
  `:3263` `TurnContextItem`; `:3203` `WorldStateItem`; `:3148`
  `SessionMetaLine`.
- `codex-rs/core/src/context_manager/updates.rs:47` —
  `build_model_instructions_update_item`, the sole consumer of
  `previous_turn_settings`.
- `codex-rs/core/src/session/handlers.rs:508` — rollback appends the marker
  and re-runs reconstruction.
- `codex-rs/core/src/thread_rollout_truncation.rs:36` — how rollback markers
  are applied when indexing.

**Fork — current transform**
- `codex-rs/lhc/codex-lhc-host/src/compact_bridge.rs:253` —
  `llm_request_context_to_response_items`: view messages to
  `ResponseItem::Message`, flattened to role + concatenated text,
  `id: None`, `phase: None`. No custom rollout writer exists; the fork
  shapes items and hands them to the stock persist path.

**LHC**
- `docs/onboard/01-core-concepts.md` — record, derivations, bands, host modes,
  smart compact.
- `docs/onboard/04-host-pi-lhc.md` — the LHC-native host pattern.
- `docs/onboard/05-host-cc-lhc.md` — the wrapper pattern.
- `packages/cc-lhc/src/rollout/write-rebuilt.ts:54` — `writeRebuiltRollout`;
  `rebuild.ts:166` `buildRolloutLines`; `sessions-index.ts:71`
  `writeRolloutFileFsync` (`open(path,"w")` + `sync()`).
- `packages/pi-lhc/src/launcher/startup.ts:68` — `SessionManager.inMemory`.
- `packages/lhc-rs/src/threads/internal/create.rs:31` — `event` table
  (`recorded_at`); `:40` `turns` (`opened_at_event_order`,
  `closed_at_event_order`, `status`); `:48` `message`
  (`token_estimate`).

**Codex — history modes (added after the first pass)**
- `codex-rs/protocol/src/protocol.rs:691` — `ThreadHistoryMode`.
- `codex-rs/thread-store/src/local/model_context.rs:132` — reverse scan over
  lineage segments; `:61` selects paginated vs legacy loading.
- `codex-rs/rollout/src/model_context.rs:84` — `ModelContextScan::observe`,
  the bounded-cutoff condition.
- `codex-rs/rollout/src/policy.rs:87` — `should_persist_event_msg`, the
  mode-dependent display-stream policy.
- `codex-rs/app-server/src/request_processors/thread_processor.rs:966` —
  `history_mode` as a request parameter; `thread-store/src/local/mod.rs:454`
  the capability.
- `codex-rs/thread-store/src/local/rollout_lineage.rs:16` — segments and
  `history_base` pointers.
- `codex-rs/thread-store/src/local/read_thread.rs:316` — thread-store's full
  load path.
- `codex-rs/core/src/guardian/review_session.rs:888` — guardian fork load.
