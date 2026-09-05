# codex-lhc — what this fork is

**Codex + LHC** is a maintained fork of
[`openai/codex`](https://github.com/openai/codex) that integrates Long Horizon
Context (LHC).

It keeps the transcript captured by the host adapter and serves **long-horizon
views**. Capture and reconstruction have documented limits, including some
multimodal content and host metadata (see the fidelity note below). Recent work stays verbatim. Older work is progressively compressed.
The full record remains available underneath each view.

Every compressed span remains addressable. Stable turn and message IDs let
Codex retrieve the exact source with `get_turns` and `get_messages` when a
compressed view does not contain enough detail.

The shared engine is
[**LHC (Long Horizon Context)**](https://github.com/liminal-ai/long-horizon-context).
This repository is its Codex host.

This page explains the problem, the LHC concepts used by the fork, and how LHC
is integrated into Codex. For the maintenance contract — touchpoint inventory,
tripwire, sync, and recovery drills — see [`FORK.md`](../FORK.md).

Install: [Install & use](INSTALL.md).

---

## Why it exists

An Agent's context window is finite. Long sessions must eventually reduce the
history sent to the model. A conventional compact replaces older history with
one summary and removes the original content from the working context.
Repeated compaction can reduce detail further because summaries become inputs
to later summaries.

LHC instead keeps the canonical record, serves older material at several
fidelity levels, and leaves stable addresses that can retrieve exact history.
Short sessions may not need this additional storage and context machinery.

## What LHC does instead

LHC keeps the **full record** — every event, durably, in a per-thread SQLite
file — and treats what the model sees as a *rendering* of that record rather
than a replacement for it. Because the record is never destroyed, the
rendering can be rebuilt at any fidelity, at any time.

The rendering is a **ramp**, not a cliff. A thread view is assembled from
fidelity tiers called **bands**, oldest to newest:

| Band | Content | Fidelity |
|---|---|---|
| **brief** | shortest chunk summaries — outcomes only | lowest |
| **detailed** | fuller chunk summaries | medium |
| **smooth** | turn renderings at full texture | high |
| *(live tail)* | everything since the compact point, verbatim | full |

Placement depends on the context budget and the size of the stored material,
not on wall-clock age. The live tail stays verbatim. Newer closed Turns use
higher-fidelity representations when they fit. Older chunks move through the
detailed and brief bands as space becomes tighter.

Each band is derived from the canonical record and can be rebuilt. Compact
changes the served view. It does not replace the stored transcript.

---

## Pull exact history when the view is too thin

A reversible memory system needs more than retained bytes. It has to leave
addresses in the working context and make following them cheap.

LHC labels archived turns and messages with stable IDs such as `t37` and
`m5232`. Codex gets two direct, bounded tools:

- **`get_turns`** returns one or more complete historical turns, including
  their message IDs and roles.
- **`get_messages`** returns the exact original content of specific messages.
  Oversized results use an explicit continuation offset instead of silently
  dropping the rest.

Results are wrapped as historical material, so old prompts are evidence under
discussion rather than fresh instructions. IDs survive compaction because
they belong to the durable record, not to a particular rendered view.

After compaction, Codex can retrieve a complete historical Turn by ID and then
retrieve one exact message if the Turn-level representation is not sufficient.
The user does not need to restate content that remains in the canonical record.

## What you get in practice

| Capability | What it means |
|---|---|
| Full transcript | The durable record remains underneath every working view |
| Fidelity ramp | Oldest material is brief; recent work keeps texture; the live tail is verbatim |
| Pull by ID | `get_turns` and `get_messages` recover exact evidence from compressed spans |
| Resume continuity | The LHC view is written back through Codex's native rollout and resume paths |
| Failure behavior | Manual, pre-turn, and mid-turn compaction use strict LHC routing; native compaction is not a fallback. MidTurn uses LHC as the single writer: safe transient failures keep the current body for a later seam; cancellation or an unproven rollout state can stop the next request; there is no silent native fallback |
| Current default | Capture on; the diagnostic kill switch disables capture without restoring native compaction |

## What this fork is not

- It is not an official OpenAI release channel.
- It is not a second cloud memory service or a vector-search layer that
  replaces the transcript; the event record remains the source of truth.
- It is not a promise that every short session improves. Stock Codex remains
  the clean comparison when long-horizon continuity is irrelevant.
- The host adapter owns SDK capture and reconstruction; Codex core also carries
  Session-dependent compaction and recovery integration. Regular upstream merges
  must preserve those seams.

## Fidelity boundary

The durable archive preserves what the Codex adapter records. It does not recover
host structure omitted during capture. At the current SDK pin, image/audio input
can be flattened into text, inter-agent metadata is not fully reconstructable,
and rolled-back content can remain in older compressed bands. The maintained
inventory is `CAPTURE_GAPS` in
[`materialize.rs`](../codex-rs/lhc/codex-lhc-host/src/materialize.rs).
“Exact retrieval” refers to the recorded representation, not a guarantee that
all original provider or UI metadata survived capture.

## Branches and releases

| Branch or channel | Role |
|---|---|
| **`lhc`** (default) | Product: Codex + LHC |
| **`main`** | Upstream mirror only |
| **Fork releases** | SemVer releases; see [GitHub Releases](https://github.com/liminal-ai/codex-lhc/releases/latest) for the current version |

## Where to go next

| You want… | Go to |
|---|---|
| Build, run, and verify | [Install & use](INSTALL.md) |
| Understand the engine | [LHC project](https://github.com/liminal-ai/long-horizon-context) and its [onboard docs](https://github.com/liminal-ai/long-horizon-context/tree/main/docs/onboard) |
| Maintain or sync the fork | [`FORK.md`](../FORK.md) |
| Use stock Codex | The upstream README below the [fork banner](../README.md), or [`openai/codex`](https://github.com/openai/codex) |

---

## LHC concepts worth knowing

Enough to read the integration. Full treatment in the LHC repo's
`docs/onboard/`.

**Record.** The durable, append-only source of truth: events as they
arrived. Everything else is derived from it and can be rebuilt from it.
Edits change what readers see; the record keeps the originals.

**Thread.** The container for one conversation, one SQLite file, plus a
registry tracking which threads exist and where. Restart-safe, including
queued background work.

**Intake stream.** The ordered event feed a harness produces — prompts,
assistant text and thinking, tool calls and results, model changes, turn
markers. LHC records these into the thread.

**Turns and chunks.** A Turn is one unit of Agent work: its opening prompt,
provider request/response cycles, tool activity, in-run steering, and terminal
response. Chunks are groups of closed Turns used by the summary bands.

**Steps and turn parts.** A step is one completed provider request/response
cycle. LHC records step edges for assistant text, thinking, tool calls, and tool
results. When an active Turn is too large to keep whole, a compact can represent
an older step range as a turn part while keeping later steps verbatim. A tool
call and its result are never split. After the Turn closes, LHC rebuilds its
whole-Turn representation from canonical messages.

**Stable addresses.** Turns and messages receive IDs in the durable record.
Rendered views keep those IDs visible so retrieval can move from a broad turn
to one exact message without loading unrelated history.

**Derivation.** The stored output of re-representing existing content — a
smoothed prompt, a turn compression, a chunk summary — attached to its
source. Seven types: four call a model, three are assembled
deterministically. Each carries its own state (`pending`, `ready`, `failed`,
`blocked`) and a source version, so a late-finishing derivation can't
overwrite a rebuild that happened after the source changed.

**Work queue and drain.** Derivation work is durable: queue rows are written
in the same transaction as the change that caused them, so nothing is lost
to a crash. *Draining* is processing that queue. It happens in the host's
process — there is no daemon.

**Host mode — the one that matters most here.**

- **Background**: the scheduler drains automatically after each intake
  commit, and picks up leftover work from a previous process on first touch.
- **Manual**: the scheduler is inert; the host must call `work.drain` itself.

**This fork runs in background mode.** Derivation happens continuously
during the session, spread across turns, so that by the time a compact is
needed the material it needs already exists.

**Smart compact.** The operation that produces a new thread view. It takes a
token target and per-band percentages and arranges Turns and chunks into
bands. For an active oversized Turn, it can also install turn parts at recorded
step edges. **Compact never calls a model**. It assembles from derivations that
already exist. Missing material can use a lower-fidelity representation. A
structural problem with the record makes compact refuse rather than install an
unproven view.

Compaction is fast when background derivation has prepared the required
representations. If derivation has not kept up, the new view can contain lower-
fidelity material or the compact can decline.

---

## How LHC is integrated into Codex

The integration is deliberately small and inventoried, because it has to
survive upstream merging into it indefinitely.

### Shape

```
codex-rs/lhc/
  vendor/long-horizon-context/   LHC itself (submodule, pinned)
  codex-lhc-host/                host mapping, storage, and rollout adapter
  goldens/                       capture mapping fixtures
```

Codex core owns the lifecycle seams that decide when capture, compact,
retrieval, and rollout installation occur. The adapter owns LHC-specific event
mapping, SDK calls, storage access, and rollout operations. Every core
touchpoint is marked with an `LHC-HOOK` comment and listed in `FORK.md`; the
tripwire verifies that inventory.

### Three seams

**1. Capture** — Codex's raw response items fan out to the adapter, which
maps them into LHC intake events and records them into the thread's SQLite
file. Provenance is carried explicitly (a typed `RawItemProvenance`, not
inferred from content) so LHC's own derived output can never be mistaken for
source material and re-ingested.

LHC capture is **on by default in this product fork**. The diagnostic kill switch
is `lhc_capture = false`, reserved for troubleshooting. It disables capture;
compaction requests fail with history preserved instead of using native compaction.

**2. The compaction ladder** — Codex already tries several compaction
strategies in order. The fork inserts an LHC arm at the front of that
ladder, in both the manual `/compact` path and the automatic
threshold-triggered path.

**PreTurn / manual / StandaloneTurn:** the arm either **installs** a banded
view as the session's history, or returns **unavailable** with a reason and
Codex proceeds down its native ladder. Failure paths (derivation not ready,
inference failure, no token reduction, cancellation) fail open. There is no
path that produces placeholder or partial content.

**MidTurn (turn parts):** when an active Turn crosses the context threshold,
Codex waits for a settled seam between provider requests. The current model
response must be complete, requested tools must be settled, capture must be
flushed, and no next provider request may have started.

At that seam, LHC can compact the active Turn at recorded step edges. The
canonical Turn does not close. Later steps remain verbatim, tool call/result
pairs stay together, and in-run steering remains in the same Turn. No synthetic
continuation Turn or boundary marker is created for a turn-parts thread.

Threads that already used the older forced-boundary continuation path retain
that compatibility path. A thread with turn parts does not use the old runtime.
MidTurn has one writer and never silently falls through to native Codex
compaction. Safe transient failures preserve the current body and can retry at
a later settled seam. Cancellation, abort, or a rollout state that cannot be
proved can stop the next provider request.

**3. Retrieval** — while capture is active, the extension registry exposes
`get_turns` and `get_messages` as direct typed tools. They resolve the current
thread from the live capture slot, validate IDs strictly, deduplicate in
request order, call the SDK, and return its bounded historical envelope
verbatim. Served and unserved outcomes are recorded as retrieval impressions;
invalid calls do not create false impressions.

### Derivation inference

Derivation calls run in-process through Codex's own `ModelClient`, on the
same auth as the CLI, pinned to a fixed model at the lowest reasoning effort
the model accepts. The pin is deliberate: it means derivation and the user's
own turns can never end up in a state where one has working credentials and
the other doesn't. Derivation never borrows the session's model.

### Maintenance contract

Four mechanisms keep the fork honest, all enforced by
`scripts/check-lhc-hooks.sh`:

- **`FORK.md`** — inventory of every core line the fork owns, one row each.
- **`LHC-HOOK` markers** — in-source, counted by the gate.
- **`patches/lhc/`** — the entire fork diff as a re-appliable series from
  one recorded upstream base, with the gate applying it at that base and
  requiring byte-identity with the working tree.
- **Tripwire** — sentinel count, vendor pin cleanliness, cross-crate compile,
  host and core tests, MidTurn and sustained-loop tests, fmt, clippy, goldens,
  the patch drill, and upstream-test-breakage checks.

Upstream is merged in (not rebased onto), so fork commits stay stable and
`git diff upstream/main...HEAD` is always the live answer to "what's
different here."

---

## Status

Capture, background derivation, banded compact/write-back, turn-parts MidTurn
Compact, resume, and stable-ID retrieval are integrated and gated. Product
releases use LHC thread schema 12. Opening schema-11 state migrates it to schema
12; downgrade to a schema-11 binary is unsupported after migration.

The tripwire covers the host seams, certified SDK, step capture, turn-parts and
legacy-thread exclusivity, rollout reconstruction and interrupted-swap
recovery, model-visible retrieval output, and patch reproduction. Capture is on
by default in product releases. See [Install & use](INSTALL.md) for release
installation, migration, storage, side-by-side commands, and troubleshooting.
