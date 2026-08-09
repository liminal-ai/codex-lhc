# Install & use

Build and run **this fork** from source: Codex with long-horizon context (LHC).
Official Codex installers and prebuilt `openai/codex` releases do **not**
include it.

See [`README.md`](README.md) for what the fork is; [`../FORK.md`](../FORK.md)
for the maintenance contract.

---

## 1. Clone the product branch with the LHC submodule

```bash
git clone --recurse-submodules https://github.com/liminal-ai/codex-lhc.git
cd codex-lhc
git checkout lhc   # default branch; the product lives here
```

If you already cloned without submodules:

```bash
git submodule update --init --recursive
```

The SDK lives at `codex-rs/lhc/vendor/long-horizon-context` and is pinned to a
specific certified commit. Do not retarget it casually: the pin is part of the
fork contract, and the gate fails if the submodule tree is dirty.

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

Run the source-built binary directly:

```bash
./target/release/codex
```

## 4. Enable LHC

LHC capture is behind a runtime feature flag, **off by default**. Enable it
in `~/.codex/config.toml`:

```toml
[features]
lhc_capture = true
```

The flag can also be set per-profile.

With the flag off, the build behaves as upstream Codex.

## 5. Use retrieval

With capture active, long-horizon views expose stable turn and message IDs in
the model's context. Codex can call two direct tools when compressed history
needs a closer look:

| Tool | Purpose |
|---|---|
| `get_turns` | Retrieve complete historical turns by stable turn ID |
| `get_messages` | Retrieve exact original messages by stable message ID |

You do not need a separate command to enable them. They are attached to the
live LHC thread when capture starts and are unavailable when capture is off or
the archive failed to open. Large results provide a continuation offset so the
model can fetch the next bounded slice.

## 6. Storage

Per-thread SQLite files, plus a registry, are written to:

```
~/.codex/lhc/
```

Override with `CODEX_LHC_ROOT`.

## 7. Verify the fork is intact

```bash
./scripts/check-lhc-hooks.sh
```

Runs the full tripwire — sentinel count, submodule cleanliness, compile,
test suites, fmt, clippy, goldens, and the patch-reproduction drill. All
layers green means the fork is intact.

## Notes

- **Never run a self-update path on this checkout.** It is a git-tracked
  source build; self-update would overwrite it.
- **Upstream binaries are not this fork.** Verify the path and build artifact
  before a live certification run; an official installer will not contain LHC.
- Derivation calls use the same auth as the CLI itself, on a pinned model.
  There is no separate LHC credential to configure.

## Next

- Product story and concepts: [`README.md`](README.md)
- Shared engine: [long-horizon-context](https://github.com/liminal-ai/long-horizon-context)
- Fork maintenance and sync: [`FORK.md`](../FORK.md)
