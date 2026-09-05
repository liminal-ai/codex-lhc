# Fork maintenance verification handoff — 2026-09-05

Branch: `maintenance/fork-slices-20260905`. Base: `b27db354c678c6a983f44d90f8795add0947cbb6`
(`rust-v0.153.3`). Upstream `.153.4` was located but has not been merged.
No release, installation replacement, or remote push has been performed.

## S1 — current docs

Commit `de95923a32`. Corrected strict LHC behavior, diagnostic capture disabling,
SDK ancestry status, exact-release sync targeting, and capture fidelity limits.
Dated merge reports and the obsolete verification backlog moved intact into
`lhc-internal/history/fork-maintenance-through-2026-09-04.md`. Current scheduler
ownership remains in FORK.md; the old compact-time settle policy is historical.

Validation: documentation inspection and diff whitespace checks. No runtime change.

## S2 — tripwire evidence

Commit `bc350f365f`. Unique retained log directories, one SDK ancestry refresh,
explicit unverified/off-main summaries, and preserved failure exit status.
Existing Rust tests now use the repository's `just test` runner. Reporting tests
are included in the tripwire. Off-main/offline statuses preserve the previous
warning-only exit policy, but do not print ALL TRIPWIRES GREEN.

Validation: six temporary-repository tests passed, including concurrent log
isolation and earlier failures surviving good/unavailable pin evidence. Bash
syntax checks passed. Full fork qualification passed; see the final evidence below.

## S4 — V8 sandbox feature

Commit `c5ec3d5f70`.

Restored the exact upstream `v8_enable_sandbox` dependency feature. Package builds
already select `ptrcomp_sandbox_release` artifacts; platform readiness now uses
the same existing checksum-verifying setup action. Local commands use
`scripts/with-codex-v8.py`, which delegates artifact resolution to the existing
package module rather than maintaining another downloader.

Verified archive/binding pairs against the published SHA-256 manifests at
`openai/codex` tag `rusty-v8-v150.4.0` for all five release targets: GNU Linux
x86-64/ARM64, Windows MSVC x86-64/ARM64, and Apple Silicon macOS. This establishes
artifact availability/integrity, not successful native execution on all platforms.

Local validation: `python3 scripts/with-codex-v8.py just test -p codex-code-mode-runtime`
passed all 71 tests (including JIT-disabled execution). Ten existing release-workflow
tests passed. `rusty_v8_bazel.py check-module-bazel` passed; `just bazel-lock-update`
completed with no lockfile change. Hosted readiness/canary has not been run.

Build cache note: `/srv` filled during core compilation. The local ignored
`codex-rs/target` cache was moved to `/tmp/codex-lhc-maintenance-target-20260905`
and replaced by a symlink, preserving the usual binary paths. The unrelated
root `target/` and other pre-existing untracked files were left alone.

## S5 — typed worker outcomes

Commit `3c83e54ea5`.

Implementation distinguishes cancellation, timeout, worker failure, operation
failure, and retained SDK errors. The SDK cancellation variant is mapped before
formatting. Thread joins, cancellation checks, and timeout durations are unchanged.
The parts worker already used typed policy and remains unchanged.

Disposition to verify:

| Boundary | Cancellation | Timeout | Other failure |
|---|---|---|---|
| Pre-turn/manual producer | Cancelled attempt | Continue with existing body | Failed attempt |
| Materialize surface read | Cancelled attempt | No added timeout | Failed attempt |
| Legacy forced-boundary worker | Existing blocked result | Existing body; next request allowed | Existing blocked result |
| Mid-turn parts | Existing typed policy | Existing retry policy | Existing retry policy |

Intentional correction: an ordinary failure mentioning “aborted” or “timeout”
no longer impersonates a control-flow event. Worker tests exercise real SDK storage failures and SDK-side cancellation with
an uncancelled turn token. A separate conversion test verifies that misleading
inference diagnostics stay inference failures while retaining their text. The
invalid-directory worker fixture emits a generic open error, so its path name
alone does not test diagnostic wording. Existing compact tests cover the joined timeout,
cancellation, and generation recovery paths.

Core `cargo check` passed. The initial 131 `compact_lhc::` tests passed; the queue-loss
fixture exceeded the default 60-second limit on its first attempt and passed
on nextest's standard retry (53.8 seconds). No timeout limit or assertion was
weakened. The additional diagnostic-conversion regression passed separately. Full-tripwire
results are recorded below.

## Final qualification and reviewer entry points

The full fork tripwire finished with **ALL TRIPWIRES GREEN**. Retained transcript:
[tripwire output](fork-maintenance-2026-09-05-tripwire.txt). Detailed logs remain at
`/tmp/lhc-tripwire.DQLoQw2a` for this checkout.

- All three hooked crates checked successfully.
- Bare CLI capture persisted a completed turn with bounded exit.
- Bare CLI sustained pressure crossed the parts seam over nine tool cycles,
  retaining pairs and reducing request content from 632,711 to 197,235 characters.
- Host library: 210 passed. Certification: 22 passed. Capture seam: 13 passed.
- Compact arm, mid-turn unit tests, and five full-loop tests passed.
- Schema fixture, adapter formatting/clippy, and all 21 golden fixtures passed.
- Eighteen serial crash/recovery and dual-format tests passed.
- The certified vendor tree remained clean; the refreshed pin is on shared main.

`just fmt` completed after behavioral validation. Its unrelated Python and vendor
formatting churn was restored; only the intended changes were retained. The final
patches were regenerated and the patch-only recovery drill repeated: **101 files
byte-identical** after formatting. No behavioral tests were rerun solely for
formatting. Documentation links and `git diff --check` passed.

This is local fork qualification, not independent review, the complete Rust
workspace suite, hosted cross-platform readiness, or release promotion. The
complete Rust suite remains subject to the repository's separate approval rule.

Suggested review order: S5's typed outcomes and unchanged join/cancellation order;
S4's feature/artifact agreement and local wrapper; S2's exit/reporting semantics;
then S1's separation of current requirements from history. The semantic source
range is `b27db354c6..3c83e54ea5`; generated patches duplicate the implementation
and can be reviewed through the reproduction proof.

## Next work

S8 has not changed the vendor pin yet. The reference implementation is Grok's
schema-13 image mapping at SDK `e9456a6e`; Codex needs its own structural request
proof, including image detail and tool-result shape. S11's source audit confirms
that `new_context` reaches strict LHC; the unqualified issue is advertised reset
semantics and enabled notes/history behavior, not an observed native bypass.
