# codex-lhc — LHC context management for Codex

Fork of [`openai/codex`](https://github.com/openai/codex) adding
[LHC](https://github.com/liminal-ai/long-horizon-context) (Long Horizon
Context): event-sourced capture of every session into a per-thread SQLite
record, with banded compaction replacing native auto-compact — full
history preserved and rebuildable at full fidelity.

- Fork work lives on **`lhc`** (default branch). `main` tracks upstream.
- Never run any self-update path on this checkout — it is a git-tracked
  source build.
- Plan of record: `docs/lhc-rs-port/phase4-codex-integration-brief.md` in
  the long-horizon-context repo (mission, chunks, seam map, laws, cast).

## Layout

- `codex-rs/lhc/vendor/long-horizon-context` — submodule, pinned to
  **certified `lhc-rs-port` commits only** (gate-green at the pin).
  Bumps record old→new pin here and in the commit body.
- `codex-rs/lhc/codex-lhc-host` — the adapter crate (fork-only,
  standalone workspace until patch 0001 lands). All LHC logic lives
  here; core touchpoints only call into it.
- `patches/` — re-appliable patch per core touchpoint (see its README).
- `scripts/check-lhc-hooks.sh` — the three tripwire layers. The script
  header enumerates exactly what it runs; keep that list truthful
  (a gate you haven't enumerated is a gate you haven't run).

## Touchpoint inventory (core lines owned by the fork)

| # | File | Marker | Purpose | Patch |
|---|------|--------|---------|-------|
| — | none yet — Chunk 0 lands zero core touches | | | |

Expected hooks when Chunk 1–2 land (~4): root `Cargo.toml` members entry;
`RawItemContributor` registration in `app-server/src/extensions.rs`;
~2 capture lines in `core/src/session/mod.rs::record_conversation_items`;
one feature-gated LHC arm in `core/src/tasks/compact.rs::run`.

Rule: any commit that adds/changes an `LHC-HOOK` line updates, in the
SAME commit: `EXPECTED_HOOKS` in the tripwire script, this inventory,
and the `patches/` series.

## Sync drill (merge-based; weekly minimum — upstream runs ~760 commits/mo)

1. `git fetch upstream && git checkout lhc && git merge upstream/main`
2. Expect conflicts near `core/src/session/mod.rs` (~70 commits/mo on
   that file). Resolve keeping hook lines intact; the sentinels mark them.
3. Watch item, every sync: the compaction dispatch ladder in
   `core/src/tasks/compact.rs` (`RemoteCompactionV2` migration). If the
   local dispatch sites shrink toward deprecation, STOP and surface —
   the LHC arm's placement needs redesign, not improvisation.
4. `./scripts/check-lhc-hooks.sh` — all layers green before push.
5. Commit with tripwire output summarized in the body; push to origin
   (the fork), never upstream.

## History-reset recovery (unlikely here; rehearsed for uniformity)

Upstream history is stable and PRs are accepted, but the drill matches
grok-build-lhc so one maintainer procedure covers both forks:
fresh clone of upstream → branch `lhc` → restore fork-owned files
(FORK.md, patches/, scripts/, `codex-rs/lhc/`) from the old checkout or
origin → `git apply patches/*.patch` → tripwires green → force-push
`lhc` to origin with Lee's sign-off.

## Scheduled verification (not "accepted limitations")

Open items are verified at a named checkpoint, never parked permanently.

| Item | Checkpoint |
|------|-----------|
| Golden smoke armed | Chunk 1 certification |
| Real-vs-simulated write-back body diff | Chunk 2 cert (live harness) or Chunk 3 live cert |
| Band-shape tolerance eval (Codex models vs LHC-shaped history) | Chunk 2, BEFORE the bridge is built |
| Auth-lane ruling (derivation calls vs plan quota) | Chunk 2 start, with Lee |
| Upstream-PR candidacy of `RawItemContributor` | after Chunk 3 sign-off |

## Laws (Phase 3 scar tissue — binding; full text in the Phase 4 brief)

1. Write-back is the architecture: after an LHC compact, host state IS
   the LHC body (`replace_compacted_history`). Divergent host/LHC
   conversation state is suspect by default.
2. The LHC compact arm feeds the same accounting native compaction
   feeds (threshold-untrips test).
3. Census fail-open paths and full-conversation consumers at Chunk 2
   start; every fallback must fit the window.
4. Capture is idempotent under LHC's own write-back (crash-injection
   loop test).
5. Bands may compress to text; the live tail conserves host-native
   kinds — never flatten structured items to prose in durable state.
6. Classify on the typed session view (source linkage, variants, ids);
   never reconstruct structure from rendered text.
7. Per-entry classification fails toward synthetic; whole-index
   construction failure fails the operation.
8. A test that cannot fail is not a test; fixtures must be shapes the
   host can actually produce.

## Host obligations from the port's acceptance record

- Pass canonical ISO timestamps only (`YYYY-MM-DDTHH:MM:SS(.mmm)Z`) to
  work-queue/scheduler APIs (Amendment D boundary).
- Do not inject a production SDK clock expecting event-stamp provenance
  (recorded clock-plumbing divergence).
- Certify at the rollout/replacement-history level, not raw request
  level (ContextManager normalizes items).
