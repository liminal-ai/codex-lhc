# Install & use

Install or build **this fork**: Codex with long-horizon context (LHC). Official
Codex installers and `openai/codex` releases do **not** include it.

See [`README.md`](README.md) for what the fork is; [`../FORK.md`](../FORK.md)
for the maintenance contract.

---

## Release install

The [latest release](https://github.com/liminal-ai/codex-lhc/releases/latest)
contains release notes, checksums, a provenance manifest, installers, and all
five native packages.

### Linux and macOS

Download the installer, inspect it, then run it. Without `--version`, the
installer resolves the latest published release:

```bash
curl -fsSLO https://github.com/liminal-ai/codex-lhc/releases/latest/download/install.sh
sh install.sh
```

To install a specific release:

```bash
sh install.sh --version 0.149.2
```

Supported release targets are Linux x86-64/ARM64 and Apple Silicon macOS.

### Windows

Run these commands in PowerShell:

```powershell
Invoke-WebRequest https://github.com/liminal-ai/codex-lhc/releases/latest/download/install.ps1 -OutFile install.ps1
.\install.ps1
```

To install a specific release:

```powershell
.\install.ps1 -Version 0.149.2
```

The installer selects the Windows x86-64 or ARM64 package from the process
architecture.

### Command name and managed updates

The default command name is deliberate:

| Existing command | Installed command |
|---|---|
| no `codex` found | `codex` — Codex + LHC becomes the primary Codex |
| `codex` already exists | `codex-lhc` — stock and LHC builds remain side by side |

Choose another name or prefix explicitly on Linux or macOS:

```bash
sh install.sh --name codex-memory
sh install.sh --prefix /opt/codex-lhc
```

On Windows:

```powershell
.\install.ps1 -Name codex-memory
.\install.ps1 -Prefix C:\Tools\CodexLHC
```

Re-running the installer updates the managed package. The Unix installer
prints the fork version transition and installed LHC SDK commit. Neither
installer replaces an unrelated command.

Uninstall an installer-managed command on Linux or macOS:

```bash
sh install.sh --name codex-lhc --uninstall
```

On Windows:

```powershell
.\install.ps1 -Name codex-lhc -Uninstall
```

Uninstall removes only installer-owned packages and command links. It preserves
Codex configuration and LHC archives.

For published releases, **re-running this fork installer is the supported
update path**. Do not use upstream's `codex update`: that channel installs
official OpenAI builds without LHC.

## Upgrade and compatibility

The current `v0.149.2` release retains LHC thread schema 12; it introduces no
new migration.

> **One-way migration:** opening schema-11 state with `v0.149.1` or newer
> migrates it to schema 12. After migration, downgrade to `v0.149.0` is
> unsupported.

The bounded selector is the default. To run the legacy eager selector on Linux
or macOS:

```bash
LHC_COMPACT_ALGORITHM=legacy codex-lhc
```

On Windows PowerShell:

```powershell
$env:LHC_COMPACT_ALGORITHM = "legacy"
codex-lhc
```

This setting changes compact selection. It does not reverse a schema migration
or make schema-12 state compatible with `v0.149.0`.

Threads that already used the older forced-boundary MidTurn path retain that
compatibility behavior. New turn-parts threads do not switch between the old
and new MidTurn mechanisms.

> **Disk usage:** LHC retains the full transcript plus derived views in local
> SQLite archives. Long-running sessions can use substantially more disk space
> than stock Codex. The default archive root is `~/.codex/lhc/`; set
> `CODEX_LHC_ROOT` to place it on another volume.

Release assets include `SHA256SUMS` and `release-manifest.json`. They record the
fork source commit, upstream base, LHC SDK commit, thread schema, target, and
capture default. Releases publish packages for Linux x86-64/ARM64, Windows
x86-64/ARM64, and Apple Silicon macOS from the same source identity. Build from
source on other architectures.

## Build from source

### 1. Clone the product branch with the LHC submodule

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

### 2. Toolchain

Rust is pinned by `codex-rs/rust-toolchain.toml` to **1.95.0** with
`clippy`, `rustfmt`, and `rust-src`. With `rustup` installed the correct
toolchain is selected automatically inside `codex-rs/`.

```bash
just install       # rustup show active-toolchain + cargo fetch
```

### 3. Build

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

### 4. LHC default and troubleshooting

LHC capture is **on by default** in this product fork. Disable it only to
isolate a storage or integration problem:

```toml
[features]
lhc_capture = false
```

The flag can also be set per-profile.

With the flag off, no LHC worker or SQLite archive is opened and native Codex
compaction remains available.

### 5. Use retrieval

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

### 6. Storage

Per-thread SQLite files, plus a registry, are written to:

```
~/.codex/lhc/
```

Override with `CODEX_LHC_ROOT`.

### 7. Verify the fork is intact

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
