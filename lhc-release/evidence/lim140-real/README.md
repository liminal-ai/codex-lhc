# LIM-140 real-integration reproduction attempt (Lee directive)

Everything in this directory is **fully integrated**: real binaries, real
provider (api.openai.com), real auth, real thread state, no shims or mocks.
Run 2026-08-29. Binaries: installed 0.149.0
(`~/.local/share/codex-lhc/versions/0.149.0/bin/codex`) vs qualified 0.150.2
(frozen tree release build, source `ec0ebb1f59` line).

## Verdict up front

The wild empty-success bug did **not** re-fire on demand — on either binary,
under every trigger condition recovered from the incident record, including
a byte-frozen clone of the original incident thread. Every attempt produced
an honest result. The full analysis of why, and the basis for calling the
bug fixed anyway, is below; the trigger is a sub-second internal race whose
window has physically closed on this machine.

## What the record says the wild trigger was

Mined from the live LHC thread databases (1,617 threads) and codex rollouts:
53 wild-era empty-success turns (2026-08-07..08-23 plus one on 08-27).
Signature: double `turn_end` — `aborted` (reasons: Other 21, interrupted 15,
Unauthorized 5, ServerOverloaded 4, ContextWindowExceeded 1, BadRequest 1)
immediately followed by a fabricated `completed`. All fabricating versions:
0.146.0 / 0.148.0-alpha.19/20 / installed 0.149.0. 25 of 53 were first-turn
one-shot `codex exec` calls — the relay's direct-job shape.

The campaign's frozen causal map
(`~/.local/state/lhc-campaigns/codex-empty-turn-cascade/evidence/incident-a-causal-map.md`)
pins the primary incident chain: `codex exec resume` on a large thread →
LHC capture-open scheduled on a background thread (the pre-fix SDK **copies
the full 386–413MB thread database on open**) → strict pre-turn compact
decision runs first and fails (`UnsupportedOperation` → `BadRequest`) → the
old branch returns `Ok(None)` → treated as success → `task_complete`
(`last_agent_message=null`) → **exit 0, empty stdout** → Console records
"(empty reply)" success. The provider is never contacted. A second wild
class embedded real provider errors (400 unsupported-model, model-at-capacity)
into `task_complete` on 0.148-alpha builds.

## Reproduction attempts (all real, all honest)

| # | Trigger | Binary | Result |
| --- | --- | --- | --- |
| 1 | Invalid API key → real 401 | 0.149.0 | exit 1, `turn.failed` — honest |
| 2 | SIGINT mid-real-turn (real auth, real stream) | 0.149.0 | exit 1 — honest |
| 3 | Real 400: unsupported model w/ ChatGPT auth (exact error text of two wild fabrications), 3 config variants | 0.149.0 | exit 1, `turn.failed` every time — honest |
| 4 | **Resume of the byte-frozen incident thread** (386MB DB copy, exact wild CLI shape `exec resume 019fe69b…`) | 0.149.0 | compact/capture race did not fire; run proceeded to a real provider request (401 on the frozen home's stale auth) — exit 1, honest |
| 5 | Real happy path, fresh thread | 0.150.2 | exit 0 **with** agent message (exact expected text) |
| 6 | Interactive TUI, real provider, cua-driver-driven | both | real answers rendered on screen (screenshots) |

Attempt 4 replicates the original investigators' own result: their repro
against the same frozen clones (`repro-prev.*` in the incident dir) also
returned an honest answer. The frozen originals were not modified; all runs
used copies.

## Why it cannot be reproduced on demand

The trigger is a race between the strict pre-turn compact decision
(immediate, on the turn path) and background capture-open. Its window is the
capture-open latency. In the wild that window was seconds wide: the pre-fix
SDK copied the full 386–413MB thread database on open, on a machine at 91%
disk with heavy parallel campaign load (the 50GiB snapshot-leak era). On a
quiet machine the copy completes before the compact decision needs it, and
the race is lost — 4/4 incident attempts fired in the wild; 0/N fire now.
Re-widening the window requires artificially delaying capture-open
(instrumentation), which is by definition no longer "everything real".

## Basis for declaring it fixed without a live re-fire

1. **The causal chain is fully evidenced, not hypothesized**: 4/4 incident
   attempts show the identical persisted signature, and the causal map pins
   the laundering to source lines (`turn.rs` `Ok(None)` return,
   `compact_lhc.rs` error mapping) present in the incident binary.
2. **LIM-134 removed the mechanism, not the symptom**: the `Ok(None)`
   laundering path no longer exists (`TurnTerminal` is the sole owner of
   terminal lifecycle — reviewed by three independent lanes); required
   compacts now *wait* (bounded, cancellable) for capture-open, closing the
   race at its source; and the exec processor reclassifies any message-less
   completed turn to Failed.
3. **The backstop is trigger-independent**: whatever internal path produces
   the bug's observable shape (completed turn, no agent message), 0.150.2
   exec cannot exit 0 on it. This is proven end-to-end on the real release
   binary by the scenario regressions (N=50 + paired N=10 + the shimmed
   old-vs-new pair), which force exactly that shape. The guarantee guards
   the outcome, so re-firing the specific race is not required to trust it.
4. **Caveat (already reported)**: the guarantee is exec-scoped; the
   interactive TUI still renders an empty turn silently. The incident
   surface (relay → exec one-shots) is covered.

## Cost note

Real-provider spend for this evidence: three short turns (two TUI, one
exec happy-path) plus one partial SIGINT'd turn — negligible. The one
deliberately NOT-run test: resuming the incident clone on 0.150.2 with live
auth, which would push a large compacted context through a real turn
(~100k+ input tokens); available on request if Lee wants the fixed path
demonstrated on the exact wild artifact.
