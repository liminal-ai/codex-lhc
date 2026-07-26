# codex-lhc — what this fork is

A fork of [`openai/codex`](https://github.com/openai/codex) that replaces
Codex's native context compaction with
[LHC](https://github.com/liminal-ai/long-horizon-context) (Long Horizon
Context).

This page is for someone deciding whether the fork is interesting: what
problem it attacks, the LHC concepts you need to read the code, and how LHC
is wired into Codex. For the maintenance contract — touchpoint inventory,
laws, tripwire, sync and recovery drills — see [`FORK.md`](../FORK.md).

---

## The problem

An agent's context window is finite, so every long session eventually has to
throw something away. The usual answer is to summarize the older part of the
conversation into a block of prose and drop the originals. That works once.
Done repeatedly it compounds: summaries get summarized, detail that mattered
is gone with no way back, and the agent's sense of the session degrades
sharply somewhere in the first day of work.

The result is memory with a cliff. Everything up to the compact is sharp,
everything before it is a paragraph, and nothing in between.

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

Recent work reads verbatim. Work from earlier today reads as smooth prose
that still carries texture. Yesterday is detailed summary. Last week is a
line about what came of it. Each compact re-ranks the whole thread down the
ramp, so material ages continuously rather than falling off an edge.

That shape is the point: it degrades the way human memory of a project
degrades — over days and weeks, not hours — and it stays honest, because the
full record is still there underneath and every band is rebuildable from it.

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

**Turns and chunks.** A turn is one full exchange: a prompt plus everything
that follows it. Chunks are groups of turns, and they are what the summary
bands are built over.

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

**Smart compact.** The operation that produces a new thread view: it takes a
token target and per-band percentages and arranges turns and chunks into
bands. **Compact never calls a model** — it assembles from derivations that
already exist. Missing material degrades an entry to a cruder rung; damage
to the record itself makes compact refuse rather than write a bad view.

That last point explains the fork's whole cadence design. Compaction is
cheap and fast *if* derivation kept up during the session. If it didn't,
compaction doesn't get slow — it gets degraded, or declines.

---

## How LHC is integrated into Codex

The integration is deliberately small and inventoried, because it has to
survive upstream merging into it indefinitely.

### Shape

```
codex-rs/lhc/
  vendor/long-horizon-context/   LHC itself (submodule, pinned)
  codex-lhc-host/                the adapter — ALL LHC logic lives here
  goldens/                       capture mapping fixtures
```

Core touchpoints never contain LHC logic. They call into the adapter crate
and nothing else. Every one is marked with an `LHC-HOOK` comment and listed
in `FORK.md`'s touchpoint inventory; the tripwire counts them.

### Two seams

**1. Capture** — Codex's raw response items fan out to the adapter, which
maps them into LHC intake events and records them into the thread's SQLite
file. Provenance is carried explicitly (a typed `RawItemProvenance`, not
inferred from content) so LHC's own derived output can never be mistaken for
source material and re-ingested.

Gated by `Feature::LhcCapture` (config key `lhc_capture`), **default off**.

**2. The compaction ladder** — Codex already tries several compaction
strategies in order. The fork inserts an LHC arm at the front of that
ladder, in both the manual `/compact` path and the automatic
threshold-triggered path.

The arm either **installs** a banded view as the session's history, or
returns **unavailable** with a reason and Codex proceeds down its native
ladder exactly as before. Every failure path — derivation not ready,
inference failure, no token reduction achieved, cancellation — fails open.
There is no path that produces placeholder or partial content: LHC either
delivers a real banded body or gets out of the way.

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
- **Tripwire** — 13 layers: sentinel count, vendor pin cleanliness, cross-crate
  compile, five test suites, fmt, clippy, goldens, the patch drill, and an
  upstream-test-breakage check.

Upstream is merged in (not rebased onto), so fork commits stay stable and
`git diff upstream/main...HEAD` is always the live answer to "what's
different here."

---

## Status

Working and gated, with the capture flag off by default. The compaction arm
is verified offline end-to-end and has been exercised against live models.
Known open items — including derivation *quality* on the pinned model, which
has been proven to run but not yet judged for output quality — are recorded
in `codex-rs/lhc/CHUNK3-CERTIFICATION.md` rather than left implicit.
