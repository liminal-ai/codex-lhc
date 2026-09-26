# Codex-LHC sync log

One line per upstream sync. Fields: date, upstream tag, BASE, candidate SHA,
conflicts, notes. Notes must name live-proof mechanics that are not default
production knobs.

## Records

2026-09-24 rust-v0.156.1 BASE=b412ff32c4 notes: release 0.156.1; local Linux candidate 0.156.1-local.1. No push.
2026-09-25 rust-v0.156.1 BASE=b412ff32c4 candidate=da9e8c6c88fd32282b9887d46e1f155a90082a55 notes: released v0.156.1 (candidate run 36094985977, promote run 36104370044, tag at da9e8c6c88); fix d2bb3f2bf2 keeps only msg-prefixed host ids on rebuilt user messages (subagent reply after compaction, reproduced on 4745976fca run 36069745127); live-qualified on the exact Linux x86_64 artifact; cutover on lee-box same day.

2026-09-19 rust-v0.155.1 BASE=be2951ea34 candidate=dc30d2bfd969e587b0c74a422a6a5c9379cbf903 conflicts=37-merge-tree (0.155.1 retarget Cargo.toml-version-only) notes: release 0.155.1; F2 proven-binding turn ids (unproven stay synthetic+warn; no migration); untagged morning def59737 burn-in does not carry; tagged artifact gets own fold/resume/F2 TUI/Guardian qualification. No cutover.

2026-09-19 rust-v0.155.1 BASE=be2951ea34 candidate=50780da7c746da2639061968cabf7d34be03f8db conflicts=Cargo.toml-version-only notes: shipped-binary live fold compact_point=197 nonempty smooth band receipt_total=39927 model_window=160000; fold trigger used **forced `-c model_auto_compact_token_limit=8000` for mechanics proof only** (grow argv 350000 is not a fit claim and exceeds the 160000 window; effective bound is model_context_window=160000). Early-only recall LIVE-FOLD-0 in folded band, absent tail and recall prompt; resume answered `0`. No cutover.

2026-09-25 rust-v0.157.0 BASE=00c972ed5d6ff6499317fd41b7f23605b8e6850d candidate=3487fd369c9ae9b1824f710e7840f4afc0d70e2d conflicts=21 (workflow-delete, Cargo.toml, session/mod.rs, rollout_reconstruction.rs, turn.rs, history/lib.rs, state/migrations.rs, models.json, 2 guardian tests, 2 tui tests, 9 snapshots) notes: merge on wrenn/codex-lhc-0.157.0 off cdee7779f0; daemon default OFF; ThreadOwned default kept; fold writes resume_metadata; thread-history migration 0007->0008 with checksum relabel repair. No push yet.

2026-09-25 rust-v0.157.0 BASE=00c972ed5d candidate=3487fd369c9ae9b1824f710e7840f4afc0d70e2d notes: released v0.157.0 (candidate run 36148327496, promote run 36165953062, tag at 3487fd369c); fork main fast-forwarded to 3487fd369c; live-qualified on the exact linux-x86_64 artifact (subagent, two compactions, Guardian-reviewed approvals, kill, reopen, exec resume) and migration/rollback rehearsed on a copy of the 0.156.1 home; Lee waived the separate release test.

2026-09-26 rust-v0.157.0 BASE=00c972ed5d notes: 0.157.0-lhc.1 cut on wrenn/codex-lhc-0.157.0-lhc.1 (8b6a686c2cf6): fix 0371504ce2 (compact import warns and skips zero-event and unrepresentable items; Chester's stop of 2026-09-25); candidate run 36219930180 green, byte-checked, live-qualified on the exact linux-x86_64 seed; NOT promoted: superseded by 0.157.1 (Lee, 2026-09-26: Windows users need upstream's fixes).
2026-09-26 rust-v0.157.1 BASE=36650394c5b38c2990ccf2a3457165ca3e9d9726 conflicts=Cargo.toml-version-only notes: merged onto the 0.157.0-lhc.1 cut as ea33ea5c77 (four Windows-only upstream fixes; Cargo.lock 155 version lines; no fork-owned file touched; LHC core pin 47e360a3 unchanged; no store migration); release 0.157.1 carries fix 0371504ce2; candidate pending.
