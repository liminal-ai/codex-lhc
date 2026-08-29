# LIM-140 interactive old-vs-new demonstration (Lee gate evidence)

Empty-success scenario (a completed provider turn with zero output and no
assistant message) served by a local shim provider, driven against the
installed 0.149.0 and the qualified 0.150.2 candidate — on both the
interactive TUI and the headless exec surface. Captured 2026-08-29 via
gnome-terminal + cua-driver.

## Run it live (one command per side)

Each script is self-contained: it starts the shim on 127.0.0.1:4519 if
needed, uses isolated HOME/CODEX_HOME dirs under /tmp/lim140-demo (create
them or edit DEMO=), and cleans up.

- `demo-old-exec.sh` — 0.149.0 `codex exec`: the bug. Prompt in, nothing
  out, `tokens used 0`, **exit code 0**.
- `demo-new-exec.sh` — 0.150.2 `codex-exec`: the fix. Same scenario, red
  `ERROR: Turn completed without producing an agent message.`, **exit 1**.
- `demo-old.sh` / `demo-new.sh` — the same pairing on the interactive TUI.

## Screenshots

| File | What it shows |
| --- | --- |
| `exec-old-0149-fabricated-success.png` | 0.149.0 exec: prompt, no answer, no error, exit 0 — fabricated success, visible. |
| `exec-new-01502-honest-failure.png` | 0.150.2 exec: same scenario, legible red ERROR with the LIM-134 reason, exit 1. |
| `tui-old-0149-silent-success.png` | 0.149.0 TUI: two consecutive prompts each complete in silence — no answer, no error, input box ready again. The bug's interactive face is silence. |
| `tui-new-01502-still-silent.png` | 0.150.2 TUI: **same silent face** — see finding below. |

## Finding: the truthful-failure guarantee is exec-scoped

On the 0.150.2 TUI the empty turn still completes silently: no error is
rendered and the session rollout records `task_complete` (verified in the
demo session's rollout JSONL; the LHC record shows a turn with a user
prompt and no assistant text). This matches LIM-134's story scope — the
reclassification (`DirectTurnAnswerEvidence`) lives in the exec output
processor, which the interactive TUI does not route through. So:

- Headless/automation consumers (the surface the original incident hit)
  get the truthful failure: proven by the N=50 + paired N=10 scenario
  regressions and visible in `exec-new-01502-honest-failure.png`.
- Interactive TUI users still see silent success on an empty turn. There
  has never been a rendered failure face for this state on the TUI; that
  would be new scope, not a regression of this campaign.

## Terminology note

Per Lee's ruling, shim-replay runs are **scenario regression tests**, not
burn-ins. The script formerly named `lhc-burnin-empty-success.py` is now
`scripts/lhc-empty-success-scenario.py`; prior evidence docs updated.
