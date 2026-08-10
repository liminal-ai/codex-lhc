# Workflow Strategy

The workflows in this directory are split so that pull requests get fast, review-friendly signal while `main` still gets the full cross-platform verification pass.

## Pull Requests

- `bazel.yml` is the main pre-merge verification path for Rust code.
  It runs Bazel `test` and Bazel `clippy` on the supported Bazel targets,
  including the generated Rust test binaries needed to lint inline `#[cfg(test)]`
  code.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes

## Post-Merge On `main`

- `bazel.yml` also runs on pushes to `main`.
  This re-verifies the merged Bazel path and helps keep the BuildBuddy caches warm.
- `rust-ci-full.yml` is the full Cargo-native verification workflow.
  It keeps the heavier checks off the PR path while still validating them after merge:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix via per-platform archive-backed shards
  - Windows ARM64 nextest archives cross-compiled on Windows x64, then replayed on native Windows ARM64 shards
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`
  - Linux remote-env tests

## Codex-LHC Release Lane

- `lhc-platform-readiness.yml` is the non-release preflight. It runs the
  LHC host, capture, compaction, resume, installer, and native executable
  checks on Linux x86_64, Windows x86_64, and Apple Silicon macOS. It does not
  create release artifacts.
- `lhc-release.yml` builds one immutable candidate for those three platforms.
- `lhc-smoke-daytona.yml` consumes that exact candidate and requires native
  install/capture/uninstall smoke on all three platforms.
- `lhc-release-promote.yml` publishes only when the candidate and smoke runs
  succeeded at the same source SHA; it never rebuilds.

Run the readiness workflow before starting the delegated candidate build.
Candidate build, platform smoke, and promotion are separate explicit actions.

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Reserve `rust-ci-full.yml` for heavyweight Cargo-native coverage that Bazel does not replace yet.
