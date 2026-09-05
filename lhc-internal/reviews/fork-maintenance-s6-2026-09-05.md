# S6 — explicit compaction stages and owners

S6a is commit `454b425cbd`: preparation and SDK worker boundaries.
S6b is commit `7b293f69c9`: it moves installation coordination, retaining the original facade paths.
The refactor starts from S11 commit `e6052c7f73` and adds no context policy.

| Location | Responsibility and authority |
|---|---|
| [compact_lhc.rs](../../codex-rs/core/src/compact_lhc.rs) | Existing facade, strict routing, MidTurn orchestration, startup reconciliation, test controls; caller interfaces stay here. |
| [preparation.rs](../../codex-rs/core/src/compact_lhc/preparation.rs) | Required capture readiness, callback selection, source/provenance preparation and SDK production. An SDK view does not make a new host generation authoritative. |
| [workers.rs](../../codex-rs/core/src/compact_lhc/workers.rs) | Dedicated runtime threads, typed timeout/cancellation results, required joins, and marker retry boundaries. These remain host execution policy. |
| [installation.rs](../../codex-rs/core/src/compact_lhc/installation.rs) | Materialize and validate; stamp IDs and durable marker; flush and classify/swap generations; reopen; install matching in-memory history/window; publish the bounded marker. |

Before the swap, the old rollout remains authoritative. After a proven new
active generation, reopen failure cannot undo the durable compact: finish the
matching host install or deny sampling with the existing recovery disposition.
No lock, flush, cancellation check, write, retry, or provider-request ordering
changed. Existing boxed-future boundaries remain in place.

Verification of the mechanical change:

- Reapplying only sibling visibility and explicit original tracing targets to
  each extracted source range reproduces all three modules after formatting.
- The remaining facade source is unchanged after removing the moved ranges and
  added module/import declarations.
- All 19 original public/crate-visible declarations retain their original root
  paths, including constants in non-test builds.
- No external caller file changed in S6. Existing cross-stage tests deliberately
  stay at the facade and continue exercising those original paths.
- Original logging targets, messages, thread names, timeout values, and typed
  error dispositions remain unchanged.
- The main module is 1,515 lines (previously 3,191); preparation is 334, workers
  674, and installation 724. Transaction coordination remains a single function
  within its owner module to preserve ordering and compiler constraints.
- S6a passed all 132 compact tests with zero retries. S6b passes core check with
  test targets. `just fix -p codex-core --lib` and `just fmt` completed; unrelated
  auto-fixes were reverted so caller files and moved bodies remain unchanged.

Final combined fork qualification passed: [remaining-slices handoff](fork-maintenance-remaining-2026-09-05.md).
