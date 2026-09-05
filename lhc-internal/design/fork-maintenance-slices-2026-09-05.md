# Codex-LHC maintenance proposal — bounded work slices

Status: **first maintenance batch authorized by Lee after steward review, 2026-09-05**.
S7/S9/S10 remain deferred; this does not authorize releases or remote publication.
Implementation and verification evidence: [batch handoff](../reviews/fork-maintenance-2026-09-05.md).

Prepared 2026-09-05 against fork commit `b27db354c678c6a983f44d90f8795add0947cbb6`
(upstream sync `rust-v0.153.3`), with SDK pin `5207952`. Findings come from source
inspection, not a fresh qualification run. Recheck affected paths after the next
upstream sync. This proposal does not supersede [FORK.md](../../FORK.md).

## Purpose and recommendation

Make this fork cheaper to maintain and more dependable for the growing agent
group. Preserve the investment in LHC rather than start another broad product
initiative. The underlying separation between durable history and served context
is sound; the concern is accumulated integration complexity, stale operating
instructions, and specific reconstruction gaps.

Revised after T3Code steward review: first batch **S1 → S2 → S4 → S5 → S8**,
with the new **S11 context-management compatibility audit** alongside that batch
and before declaring the selected release qualified. Follow with **S6**. S3 is
useful but not urgent. **S7, S9, and S10 are deferred outside this maintenance
batch**. Approval of early slices should not imply approval of the whole list.

Images are existing SDK capability to integrate, not open SDK research. Shared
Rust LHC implements content blocks at schema 13 in `e9456a6e`; its port ledger
records migration and parity gates. Grok's September 4 slice 3 wires user and
tool-result images against that pin. The Codex pin is 69 commits behind the
locally available shared-main ref. That count is dated evidence, not a target:
select a certified commit explicitly rather than bumping blindly to latest.

The proposed `.4` release sync is a separate work item. Verify the exact upstream
tag and scope before starting; this review has not verified that release. Do not
bundle behavior changes or restructuring into its merge. A newly discovered
build or correctness blocker may change ordering, but record that explicitly.

| Slice | Deliverable | Relative size | Dependency / sequencing |
|---|---|---|---|
| S1 | Accurate current operating documentation | Small | Independent; refresh version details after sync |
| S2 | One coherent tripwire pin check and isolated logs | Small | Independent |
| S3 | Repeatable patch-series regeneration | Medium, later | Stabilize merged base first; uses existing recovery drill |
| S4 | Resolve or precisely contain the V8 build exception | Small investigation; fix size unknown | Check against selected release; independent of LHC changes |
| S5 | Typed compact worker outcomes | Medium | Prefer after sync; before S6 |
| S6 | Separate compact preparation and installation responsibilities | Bounded refactor, after first batch | S5; retain external call sites and behavior |
| S7 | Contract assessment for deferred identity/rollback work | Deferred | Only if those capabilities become product priorities |
| S8 | Schema-13 pin update and Codex image adapter wiring | Medium, first batch | Existing certified SDK and Grok implementation; no S7 dependency |
| S9 | Inter-agent identity and provenance fidelity | Deferred; outside maintenance | S7 and demonstrated native multi-agent use |
| S10 | Rollback honored by every served representation | Deferred; shared LHC design | S7 and deliberate import/move/fork/rollback semantics |
| S11 | Qualify upstream notes/reset mode against strict LHC | Small audit; remediation sized from findings | First batch / selected-release qualification |

Sizes are scope judgments, not promised durations. S8 has an established
implementation pattern; Codex-specific mapping and qualification remain work.

## S1 — Repair documentation and separate current law from history

**Problem/evidence.** [The product README](../../lhc-docs/README.md) still says
manual/pre-turn compaction can use a native fallback. The current
[compact module](../../codex-rs/core/src/compact_lhc.rs) and FORK.md require strict
LHC routing. FORK.md mixes current requirements with extensive dated merge and
incident accounts. Its side-branch warning for the SDK pin is stale against the
locally available `origin/main` ancestry; refresh that evidence before editing.
The generic sync drill also names upstream/main while the latest recorded sync
uses an exact stable tag.

**Work.** Correct behavioral claims first. Keep current invariants, touchpoints,
qualification requirements, and recovery entry points in FORK.md. Move dated
execution narratives into linked internal history, preserving evidence and
existing references. State explicitly how the upstream sync target is selected.
Describe the actual core/adapter split rather than claiming all logic is in the
adapter. Qualify “full fidelity” claims against the documented capture boundary.

**Acceptance.** A new maintainer can determine compact routing, failure behavior,
SDK provenance, and the update procedure without reading incident history. No
current invariant is lost during the move. Links resolve; historical documents
are visibly historical. Documentation-only validation is sufficient; do not add
tests that merely assert prose.

**Boundary.** No runtime changes and no redesign of documentation infrastructure.

## S2 — Simplify the existing tripwire without weakening it

**Problem/evidence.** [check-lhc-hooks.sh](../../scripts/check-lhc-hooks.sh) has two
consecutive SDK ancestry checks and remote fetches near its end. Its opening
“three layers” description conflicts with the expanded inventory. Fixed files
under `/tmp/lhc-hook-*` and `/tmp/lhc-vendor-git.err` can collide across runs.

**Work.** Consolidate pin checking, distinguish unavailable remote evidence from
an actual off-main pin, and use one per-run log directory whose location is
reported. Preserve all current qualification layers and their failure status.
Make the header and final report describe what actually ran.

**Acceptance.** Ancestry has one check and at most one fetch per run; offline
results are labeled honestly. Two invocations cannot overwrite one another's
logs. A failing layer still makes the overall run fail, and skipped evidence
does not become a success claim. Exercise these control paths with temporary
repositories or command stubs, then validate the real gate during qualification.

**Boundary.** This is not a replacement CI system or a reduction in certification.
Retain the existing before-push gate; local targeted checks are not substitutes.

## S3 — Make patch regeneration a reproducible operation

**Problem/evidence.** [patches/lhc/README.md](../../patches/lhc/README.md) requires
seven patch groups regenerated against one BASE and describes manual commands,
including broad `git add -N .`. Ownership is repeated across prose and patches.
The existing byte-equality recovery drill is valuable and should remain.

**Work.** Add a small regeneration command with an explicit base and a checked-in
file-group manifest. Generate into a temporary location, validate coverage, and
replace the series only on success. Avoid broad staging of unrelated files.
Represent known exclusions explicitly, including the vendor/adapter tree and
cargo-generated lockfile. Use the manifest for mechanical grouping; do not make
all architectural prose generated output.

**Acceptance.** Regeneration twice is byte-stable. Applying the series to the
recorded base reproduces the intended fork-owned files. Missing ownership,
duplicate ownership, deletions/renames, and unintended upstream-file inclusion
are handled explicitly. The caller's index and unrelated files remain unchanged.
Keep the existing recovery coverage check sufficiently independent to catch a
file accidentally omitted from the manifest. Prove these properties with a
temporary-repository test and the real history-reset drill.

**Boundary.** Preserve merge-based maintenance and the existing patch purpose.
Do not introduce a new patch-stack development model.

## S4 — Verify and retire or contain the V8 exception

**Problem/evidence.** The fork's
[code-mode-runtime dependency](../../codex-rs/code-mode-runtime/Cargo.toml)
omits upstream's `v8_enable_sandbox` feature because an artifact was unavailable.
The comment calls this local, but the dependency edit is unconditional. This
source difference alone does not establish the effective feature set of every
binary: Cargo feature unification and release build paths must be checked.

**Work.** For the selected release, record the pinned V8 version, effective
features for actual local/release targets, artifact availability, and why the
exception remains necessary. If supported artifacts now exist, restore upstream
configuration and validate. Otherwise propose an explicitly scoped build policy
with a retirement condition. The V8 feature is distinct from Codex's process
sandbox; keep that distinction clear.

**Acceptance.** Produce evidence for each supported release target and a concrete
disposition: restored upstream behavior, a validated bounded alternative, or a
named unresolved dependency. Verify affected binaries through the existing build
and code-mode checks. Dependency changes include required Cargo/Bazel lock
updates. Do not silently broaden the exception to make a build pass.

**Boundary.** First landing may be evidence and corrected scope documentation.
No commitment to building a new V8 artifact service. Do not modify the protected
Codex sandbox environment-variable logic named in AGENTS.md.

## S5 — Carry typed outcomes through compact worker boundaries

**Problem/evidence.** `is_cancel_reason` and `is_worker_timeout_reason` in
[compact_lhc.rs](../../codex-rs/core/src/compact_lhc.rs) classify errors by text.
Their callers use those classifications to decide cancellation, continued
sampling, or failure. This makes behavior depend on wording.

**Work.** Trace the error producers and retain typed cancellation, timeout,
worker/join failure, and operation failure across the relevant thread helpers.
Keep explanatory text and error sources for diagnostics. Reuse existing outcome
types where appropriate. Write a before/after disposition table for pre-turn,
manual, mid-turn parts, and legacy forced-boundary paths before changing them.

**Acceptance.** Recovery control flow no longer inspects diagnostic substrings.
Changing a message does not alter disposition. Behavioral tests exercise actual
typed timeout, cancellation, storage/operation failure, and an ordinary error
whose message contains a formerly significant word. Preserve current history,
retry, sampling, and terminal-event behavior for each entry path. Any discovered
policy inconsistency is reported separately rather than silently normalized.

**Boundary.** No timeout tuning, new fallback policy, SDK schema change, or public
API expansion merely to support this refactor.

## S6 — Make the compaction stages and owners explicit

**Problem/evidence.** The core compact module is 3,182 lines at the inspected
commit. It owns readiness, worker hops, materialization, durable replacement,
and host installation. The issue is intertwined responsibilities, not a line
count target. Existing comments also describe async nesting/query-depth pressure.
This file is fork-owned and has no upstream twin, so its internal reorganization
does not create direct upstream merge conflicts. The steward identifies 19
external call sites: inventory them at the implementation base and retain their
contract. Runtime transaction and recovery risks still require the existing tests.

**Work.** Map the current transition/ownership sequence as part of the refactor,
including which generation is authoritative at every failure point. Then land separate
mechanical extractions: (a) preparation/readiness and worker execution, followed
by (b) installation coordination. Keep Session-dependent behavior in core; move
only genuinely host-independent work into the existing adapter. Move relevant
tests and invariant documentation with the code. Avoid a generic framework or
new traits solely to make the diagram look cleaner.

**Acceptance.** A reviewer can locate preparation, durable installation, and
in-memory installation without tracing one long function. Existing cancellation,
swap-failure, generation-identity, resume, and continued-turn proofs still pass.
Order of commits, locks, flushes, cancellation checks, and provider requests is
unchanged. Preserve the existing boxed-future/compiler constraints where needed.
Each landing is independently reviewable and behavior-preserving.

**Boundary.** No storage redesign, new context policy, broad module cleanup, or
attempt to move every Session dependency into the adapter.

## S7 — Deferred contract assessment for identity and rollback

**Status.** Deferred outside the first batch; no prerequisite for S8. Native
inter-agent identity is not part of Lee's current usage, and rollback belongs
with deliberate shared-LHC lifecycle design.

**Problem/evidence.** The authoritative gap inventory in
[materialize.rs](../../codex-rs/lhc/codex-lhc-host/src/materialize.rs) names three
distinct gaps. Images are addressed directly by S8. This assessment covers only
inter-agent messages losing author/recipient structure and rolled-back content
remaining in compressed bands, if either becomes an accepted product priority.

**Work.** For those two cases, map native input → durable capture → served
view → rollout rewrite → resumed provider/display surfaces. Identify what can be
fixed in the host, what needs a certified SDK change, and what old rows cannot
recover. Specify archive retrieval versus active-context semantics, particularly
for rollback. Name the exact candidate SDK commit/schema and certification
evidence before proposing a pin change.

**Acceptance.** A compact matrix records today's behavior, intended behavior,
required ownership changes, old-record behavior, and one concrete reproduction
per gap. Reviewers can accept S9 and S10 independently with known dependencies.
Do not expand this into an exhaustive taxonomy of every host event.

## S8 — Update the certified SDK pin and wire Codex images

**Evidence.** Rust schema-13 content blocks and blob extraction are implemented;
the shared port ledger records 846 passing gate cases, including content-block
and migration coverage. This is recorded certification, not a fresh run here.
Grok's mapping converts user images and tool-result images into SDK content
blocks, then restores image content from blob-inlined served entries. Its
September 4 certification includes image capture/compact/restart drills.

References: [Rust port ledger](../../../long-horizon-context/packages/lhc-rs/PORT_STATUS.md),
[content-block tests](../../../long-horizon-context/packages/lhc-rs/tests/content_blocks.rs),
[Grok slice 3](../../../grok-build/FORK.md),
[Grok image mapping](../../../grok-build/crates/lhc/grok-lhc-host/MAPPING.md),
[Grok live evidence](../../../grok-build/crates/lhc/grok-lhc-host/LIVE_CERT_2026-09-04.md).
These neighboring-repository references were available at review time.

**Work.** Select a certified schema-13 SDK pin (the known reference is
`e9456a6e`), advance the submodule, and repair adapter API drift. Wire Codex user
image inputs and supported tool-result images through ordered content blocks.
Use existing blob extraction for embedded data and preserve external references.
Reconstruct Codex image items where the SDK serves full blocks; compressed bands
remain text placeholders by design. Audit image attributes specific to Codex
instead of copying Grok types literally. Keep text-only behavior compatible.

**Acceptance.** Copy-based migration from the current schema 12 preserves turn
parts, stable IDs, and existing text history. Production-path tests cover pasted
images and image-bearing tool results through capture, compact, and resume.
Check the next provider request structurally: image bytes/references, ordering,
and supported image attributes survive whenever full blocks are served. Verify
that compressed bands use bounded placeholders rather than base64 text. Include
the missing-payload/reference case and an old placeholder-only record. Retain
current mid-turn parts and rollout-recovery qualification under the new SDK.
Report historical data that cannot be recovered rather than guessing it.

**Boundary.** Images only. Audio/documents are not silently added to this slice.
No new SDK representation design, media-generation feature, or media UI. Do not
patch the vendor working tree. A structural request assertion is required:
a model describing an image after resume might be using its earlier textual
description and does not alone prove that image content was restored.

## S9 — Preserve inter-agent identity and provenance

**Status.** Deferred outside fork maintenance. Lee is not currently using the
native Codex multi-agent message shape this slice would preserve. Relay work
does not by itself establish demand for it.

**Work.** Implement S7's host metadata mapping for supported inter-agent messages:
author, recipient, message kind, and provenance. Restore the corresponding host
representation where available rather than inventing identity from prose.

**Acceptance.** Send a representative message through the production registration
path; inspect durable capture, post-compact model context, and resumed history.
Identity survives, and the message is not silently promoted into a fresh user
instruction. Legacy identity-less rows have an explicit, compatible disposition.
Verify any affected app-server/display contract through its real public seam.

**Boundary.** No relay redesign, cross-thread memory service, or coordination UI.
Preserving known metadata is the entire feature.

## S10 — Honor rollback in served context while retaining the archive

**Status.** Deferred to shared-LHC lifecycle design alongside import, move, and
fork semantics. Not a first-batch fork maintenance item.

**Work.** Implement S7's approved rollback semantics in the durable selection and
derivation paths, then connect the host rollback event. Define how stale derived
bands are invalidated/rebuilt and what is served while that work is pending.
Preserve original archive evidence and stable historical addresses. Specify how
explicit historical retrieval represents rolled-back material.

**Acceptance.** Roll back material in the live tail and in an already-compressed
span; in both cases it stays out of subsequent active context across another
compact and resume. Repeated identical prompts cannot confuse identity. Cover
interruption/restart at the new rollback boundary and old-thread compatibility.
No deleted active instruction reappears because an old summary remains cached.

**Boundary.** This is not physical archive deletion or a privacy-erasure API.
It likely requires shared SDK ownership and must not be represented as a quick
host-only patch until S7 proves otherwise.

## S11 — Qualify upstream context-management notes and reset

**Problem/evidence.** The 0.153 sync record in FORK.md explicitly leaves behavior
with `features.context_management.experimental_mode` enabled as an unqualified
finding. Its notes/history extension has provider/auth configuration gates, and
the `new_context` tool advertises a new window without summarization.

Source tracing at the inspected commit shows the tool calls
`request_new_context_window`; the follow-up loop consumes that request and calls
`run_auto_compact`, which delegates to strict LHC. That path is not evidence of
a native-compaction bypass. The open question is whether the exposed tool,
token-budget reminders, notes/history hints, and LHC's resulting window state
form a coherent supported contract. The native-reset test
`new_context_tool_skips_auto_compact_fallback` is ignored for its native premise;
generic window-advance coverage does not establish this specific opt-in path.

References: [tool handler](../../codex-rs/core/src/tools/handlers/new_context_window.rs),
[turn loop](../../codex-rs/core/src/session/turn.rs),
[notes extension](../../codex-rs/ext/history-notes/src/extension.rs),
[native test allowlist](../../scripts/lhc-native-routing-ignored-tests.txt).

**Work.** Trace registration, provider/auth gates, reset request consumption,
reminders, contributed context, and history/notes lookup across a window change.
Exercise default-off and enabled modes through actual app-server/core paths.
Identify whether the supported fork policy is compatible coexistence or explicit
rejection/gating of an incompatible mode; retain strict LHC compaction in either
case. Bring a concrete proposed policy for review if enabled semantics conflict.
Do not adopt native clearing simply to make upstream tests pass.

**Acceptance.** Add active fork-specific coverage for explicit `new_context`
below the automatic threshold, threshold-driven transitions with notes enabled,
and the next provider request and resumed history. Include relevant unavailable
notes-backend behavior. Check capture continuity, window identity, pending tool
pairs, and truthful tool/reminder text. If the mode is intentionally rejected,
prove rejection before incompatible tools/instructions are exposed, while the
default path remains unchanged. Wire the accepted coverage into a named existing
tripwire layer, update the ignore owner references, and state the supported
configuration in current docs. Do not weaken checks by merely expanding ignores.

**Boundary.** Compatibility work for inherited upstream behavior, not a new memory
product or a replacement for LHC retrieval. The audit is required in the first
batch; size any discovered implementation fix from the actual affected paths.

## Verification and review

Implementation follows the then-current AGENTS.md and FORK.md requirements:
targeted `just test` suites, formatting/linting as applicable, patch regeneration
for touched fork-owned files, and full existing fork qualification before push.
Respect the repository's separate approval rule for the complete Rust suite;
the implementation authorization does not replace that separate approval rule. API/config/schema changes require
their normal generated fixtures and compatibility checks. Keep platform support
and connected app-server/exec-server OS differences in scope where relevant.

For the reviewing agent:

1. Confirm the findings still hold at the chosen base; distinguish stale comments
   from demonstrated runtime defects.
2. Challenge the payoff of each slice. Which should be dropped or deferred to
   protect time for shipping software outside this infrastructure?
3. Check that S5 preserves every existing outcome and S6 preserves the actual
   transaction/recovery boundaries; reject accidental policy changes.
4. Identify existing tooling that could satisfy S3 before adding another tool.
5. Validate the V8 build-scope claim and whether S4 is a release blocker.
6. Confirm S8 uses already-certified image capabilities and retains the Codex
   turn-parts/recovery contracts. Keep S9/S10 deferred unless priorities change.
7. Check that S11 actually exercises enabled notes/reset behavior rather than
   relying on marker counts or generic compaction tests.

Requested review outcome: accept, revise, or defer each slice with reasons and a
recommended first batch. This is a proposal for selection, not an implementation
campaign or a commitment to complete every item.
