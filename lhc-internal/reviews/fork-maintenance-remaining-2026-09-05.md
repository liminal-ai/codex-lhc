# Remaining maintenance slices — verification handoff

Branch: `maintenance/lhc-images-context-stages-20260905` (local review branch).
Base: `10384cc52d` on `lhc`. No upstream release sync is part of these slices.

| Slice | Commit | Result |
|---|---|---|
| S8 | `daa196b373` | Schema-13 SDK pin and ordered user/tool image blocks, image-detail preservation, copy-based migration and structural provider-request/restart proofs. |
| S11 | `e6052c7f73` | Conservative startup rejection of incompatible upstream notes/reset mode while LHC is active; authenticated app-server, default-mode, and unchanged-rollout resume proofs. |
| S6a | `454b425cbd` | Mechanical extraction of preparation and dedicated runtime worker boundaries. |
| S6b | `7b293f69c9` | Mechanical extraction of installation coordination, preserving all 19 original declarations and caller paths. |

The final combined fork tripwire exited **0**, with **ALL TRIPWIRES GREEN** and
zero test retries. [Retained output](fork-maintenance-remaining-2026-09-05-tripwire.txt).
The vendor is clean at `e9456a6ee23a10cf04e15b16bcf77738c52b0f7c`, verified on
shared main. Patch reconstruction reproduces all **108** covered files exactly.

Qualification includes bare CLI capture, the sustained nine-cycle production
parts loop, 212 host tests, 24 certification tests, core capture/schema checks,
48 compact-arm tests, 47 mid-turn tests, nine core full-loop/compatibility tests,
two authenticated app-server startup rejection cases, and 18 crash/recovery
cases. The seven-test app-server notes/backend suite and ten release-workflow
checks also passed independently. S6a passed all 132 compact unit tests.

After that full run, the migration assertion was strengthened to compare all
thread metadata, including a non-null sticky turn-parts activation marker.
That targeted test passed without retries; it changes only the test fixture and
assertions, not production code. Final formatting completed.

Detailed scope and evidence:

- [S8 images and migration](fork-maintenance-s8-2026-09-05.md)
- [S11 notes/reset policy](fork-maintenance-s11-2026-09-05.md)
- [S6 stage ownership and mechanical audit](fork-maintenance-s6-2026-09-05.md)

S11 deliberately rejects the inherited reset contract; it does not adapt native
clearing into a new LHC feature. Full image blocks restore images; compressed
bands and missing payloads remain bounded placeholders. Legacy image markers
cannot recover lost attributes. Existing unpaired structured-output refusal is
unchanged. Audio, identity, and rollback expansion remain outside this batch.

S3 remains a later maintenance item. S7/S9/S10 and the separate upstream sync
retain their previous disposition. These commits are ready for independent
review; no remote publication or merge has been performed for this batch.
