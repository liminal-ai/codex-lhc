# Codex-LHC sync log

One line per upstream sync. Fields: date, upstream tag, BASE, candidate SHA,
conflicts, notes. Notes must name live-proof mechanics that are not default
production knobs.

## Records

2026-09-24 rust-v0.156.1 BASE=b412ff32c4 notes: release 0.156.1; local Linux candidate 0.156.1-local.1. No push.
2026-09-25 rust-v0.156.1 BASE=b412ff32c4 candidate=da9e8c6c88fd32282b9887d46e1f155a90082a55 notes: released v0.156.1 (candidate run 36094985977, promote run 36104370044, tag at da9e8c6c88); fix d2bb3f2bf2 keeps only msg-prefixed host ids on rebuilt user messages (subagent reply after compaction, reproduced on 4745976fca run 36069745127); live-qualified on the exact Linux x86_64 artifact; cutover on lee-box same day.

2026-09-19 rust-v0.155.1 BASE=be2951ea34 candidate=dc30d2bfd969e587b0c74a422a6a5c9379cbf903 conflicts=37-merge-tree (0.155.1 retarget Cargo.toml-version-only) notes: release 0.155.1; F2 proven-binding turn ids (unproven stay synthetic+warn; no migration); untagged morning def59737 burn-in does not carry; tagged artifact gets own fold/resume/F2 TUI/Guardian qualification. No cutover.

2026-09-19 rust-v0.155.1 BASE=be2951ea34 candidate=50780da7c746da2639061968cabf7d34be03f8db conflicts=Cargo.toml-version-only notes: shipped-binary live fold compact_point=197 nonempty smooth band receipt_total=39927 model_window=160000; fold trigger used **forced `-c model_auto_compact_token_limit=8000` for mechanics proof only** (grow argv 350000 is not a fit claim and exceeds the 160000 window; effective bound is model_context_window=160000). Early-only recall LIVE-FOLD-0 in folded band, absent tail and recall prompt; resume answered `0`. No cutover.
