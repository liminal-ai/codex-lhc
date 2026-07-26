# Install

Build and run the fork from source. Assumes the repo is already cloned.

See [`README.md`](README.md) for what the fork is; [`../FORK.md`](../FORK.md)
for the maintenance contract.

---

## 1. Get the vendored SDK

LHC is a submodule and is not present in a plain clone.

```bash
git submodule update --init --recursive
```

The submodule is pinned to a specific certified commit
(`codex-rs/lhc/vendor/long-horizon-context`, tracking branch
`lhc-rs-port-codex-pin`). Do not bump it casually — the pin is part of the
fork's contract and the gate fails if the submodule tree is dirty.

## 2. Toolchain

Rust is pinned by `codex-rs/rust-toolchain.toml` to **1.95.0** with
`clippy`, `rustfmt`, and `rust-src`. With `rustup` installed the correct
toolchain is selected automatically inside `codex-rs/`.

```bash
just install       # rustup show active-toolchain + cargo fetch
```

## 3. Build

```bash
cd codex-rs
cargo build --release -p codex-cli
```

Produces the `codex` binary. LHC is a normal workspace dependency, not a
cargo feature — there are no extra build flags.

## 4. Configure

LHC capture is behind a runtime feature flag, **off by default**. Enable it
in `~/.codex/config.toml`:

```toml
[features]
lhc_capture = true
```

The flag can also be set per-profile.

With the flag off, the build behaves as upstream Codex.

## 5. Storage

Per-thread SQLite files, plus a registry, are written to:

```
~/.codex/lhc/
```

Override with `CODEX_LHC_ROOT`.

## Verify

```bash
./scripts/check-lhc-hooks.sh
```

Runs the full tripwire — sentinel count, submodule cleanliness, compile,
test suites, fmt, clippy, goldens, and the patch-reproduction drill. All
layers green means the fork is intact.

## Notes

- **Never run a self-update path on this checkout.** It is a git-tracked
  source build; self-update would overwrite it.
- Derivation calls use the same auth as the CLI itself, on a pinned model.
  There is no separate LHC credential to configure.
