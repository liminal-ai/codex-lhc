# LHC core-touchpoint patches

This directory holds the re-appliable patch series for codex-lhc core
touchpoints. It is **not** the repo-root `patches/` directory (that is
upstream's third-party Bazel/Windows patch collection).

Regenerate after any hook change (same commit as the hook):

```bash
# From a clean base (upstream tip) with only the fork-owned tree applied,
# emit one patch per inventory row in FORK.md. Example workflow for the
# orchestrator at commit time:

git diff upstream/main -- codex-rs/Cargo.toml > patches/lhc/0001-workspace-member.patch
# ... etc for each touchpoint file set
```

The working tree currently carries all Chunk 1 hooks uncommitted; the
orchestrator regenerates this series at commit. See FORK.md touchpoint
inventory for the authoritative file list.

## Series (Chunk 1)

| Patch | Touchpoints |
|-------|-------------|
| 0001-workspace-member | `codex-rs/Cargo.toml` members + path dep |
| 0002-raw-item-contributor | `ext/extension-api` RawItemContributor + registry |
| 0003-feature-flag | `features` Feature::LhcCapture |
| 0004-session-raw-item-hook | `core` session raw-item fan-out, e2e, ModelOutput provenance sites |
| 0005-app-server-dep | `app-server/Cargo.toml` |
| 0006-app-server-install | `app-server/src/extensions.rs` install call (+ cwd, H3 test) |
| 0007-lhc-compact-arm | `compact_lhc.rs` + tests + `lhc_inference_bridge.rs` + `lib.rs` mods + `InitialContextInjection: Clone` + manual/auto ladder hooks + `tasks/lifecycle.rs` idle-derivation seed + core runtime dep on `codex-lhc-host` |

Chunk 2b: body from real `lhc.compact` + view map; NoReduction fail-open;
marker after write-back with derived digests on slot; timeout detaches.
Tripwire layer 4 applies 0007 on HEAD and requires byte-identity with the
working tree (apply-only is not enough).

**Runtime cost note:** `codex-core` takes a runtime dep on `codex-lhc-host`
(not dev-only), so every core build compiles LHC + bundled SQLite regardless
of `Feature::LhcCapture`. No dependency cycle. Feature-gating the crate dep
would require a larger modularization; document rather than contort.

Rule zero (Chunk 1 fix round 3): storage invariants must round-trip through
`LhcSession`; host/core seams must exercise production registration paths.
