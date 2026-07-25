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

Chunk 2 will add compact-dispatch patch(es).

Rule zero (Chunk 1 fix round 3): storage invariants must round-trip through
`LhcSession`; host/core seams must exercise production registration paths.
