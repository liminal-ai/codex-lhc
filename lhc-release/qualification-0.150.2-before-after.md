# LIM-140 qualification evidence: paired before/after burn-in (Lee directive)

Same script (`scripts/lhc-burnin-empty-success.py`), same mock provider, same
scenario, run 2026-08-29 against both binaries. The script asserts the
LIM-134 behavior (turn Failed, exit nonzero, no fabricated answer), so PASS
means the fix is present and FAIL means the original empty-success bug fired.

## BEFORE — installed 0.149.0 (bug live)

Binary: `/home/leemoore/.local/share/codex-lhc/versions/0.149.0/bin/codex`
(the launcher's live target), sha256
`262c325ef67fd24fa4a388a3719899615c9e98fde5c8c092eb6d44a89fd27fec`.
Invoked as `codex exec` through a 2-line wrapper (0.149.0 ships only the
multicall binary; the wrapper adds no flags):

    #!/usr/bin/env bash
    exec /home/leemoore/.local/share/codex-lhc/versions/0.149.0/bin/codex exec "$@"

Result — bug reproduced on every iteration:

    summary 0 pass / 10 fail / 10 total wall=5.45s binary=/tmp/lim140-old-exec.sh

Every iteration: exit 0, `turn.completed` with all-zero usage, no agent
message item, no `turn.failed`. Exemplar (iteration-001 stdout):

    {"type":"turn.started"}
    {"type":"turn.completed","usage":{"input_tokens":0,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0}}

Full per-iteration logs (stdout/stderr/mock-server/meta) preserved at run
time under `/tmp/lim140-before-logs/iteration-001..010`.

## AFTER — qualified 0.150.2 (fix proven)

Binary: frozen-tree release `codex-rs/target/release/codex-exec`, sha256
`dc7e6b8c7d4e60b6fe2b4e64b0e0f4922cc4b832da72daaa28e2e2ce915381ee`
(matches the P5 identity block; source ec0ebb1f59, SDK pin 5207952).

    summary 10 pass / 0 fail / 10 total wall=4.71s binary=.../codex-rs/target/release/codex-exec

Every iteration: exit 1, `turn.failed` with "Turn completed without
producing an agent message." This N=10 paired run is in addition to the
earlier N=50 (50/50 pass, Grok run 20260829-042036-524e94 plus independent
verifier and steward reruns). Logs preserved at run time under
`/tmp/lim140-after-logs/`.

## Verdict

Identical harness discriminates 0/10 vs 10/10. The installed 0.149.0
fabricates success on the empty-turn shape; the 0.150.2 candidate fails
truthfully. Independently re-read by the steward (iteration-001 of both
dirs) before acceptance.
