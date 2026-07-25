#!/usr/bin/env bash
# LHC fork tripwires — run after every upstream sync and before every push.
# Three layers (FORK.md "Sync drill"): sentinel count, compile, golden smoke.
# Exit nonzero on any tripped layer. Keep this script dependency-free.
#
# WHAT THIS SCRIPT ACTUALLY RUNS (keep in lockstep with FORK.md inventory —
# Phase 3 lesson: a gate you haven't enumerated is a gate you haven't run):
#   1. grep count of LHC-HOOK sentinels in core vs the expected total below
#   2. cargo test  --manifest-path codex-rs/lhc/codex-lhc-host/Cargo.toml
#      cargo fmt   --check for the adapter crate
#   3. golden smoke (capture->rebuild diff) — armed by Chunk 1 certification
set -u
cd "$(dirname "$0")/.."
command -v cargo >/dev/null 2>&1 || . "$HOME/.cargo/env" 2>/dev/null || true
fail=0

# ── Layer 1: sentinel count ────────────────────────────────────────────
# Every core touchpoint carries an `LHC-HOOK` marker (`// LHC-HOOK` in .rs,
# `# LHC-HOOK` in .toml). Expected total is maintained here and in FORK.md,
# updated in the SAME commit as any hook change.
EXPECTED_HOOKS=0
found=$(grep -rl "LHC-HOOK" codex-rs --include="*.rs" --include="*.toml" 2>/dev/null \
        | grep -v "codex-rs/lhc/" | xargs -r grep -o "LHC-HOOK" | wc -l)
if [ "$found" -ne "$EXPECTED_HOOKS" ]; then
  echo "TRIPWIRE sentinel: expected $EXPECTED_HOOKS LHC-HOOK markers in core, found $found"
  grep -rn "LHC-HOOK" codex-rs --include="*.rs" --include="*.toml" 2>/dev/null | grep -v "codex-rs/lhc/"
  fail=1
else
  echo "ok sentinel: $found/$EXPECTED_HOOKS LHC-HOOK markers"
fi

# ── Layer 2: compile + fmt (adapter + vendored port, repo toolchain) ───
if cargo test -q --manifest-path codex-rs/lhc/codex-lhc-host/Cargo.toml >/tmp/lhc-hook-compile.log 2>&1; then
  echo "ok compile+test: codex-lhc-host + vendored lhc"
else
  echo "TRIPWIRE compile: codex-lhc-host failed — first errors:"
  grep -E "^error" -A5 /tmp/lhc-hook-compile.log | head -20
  fail=1
fi
if cargo fmt --check --manifest-path codex-rs/lhc/codex-lhc-host/Cargo.toml >/dev/null 2>&1; then
  echo "ok fmt: codex-lhc-host"
else
  echo "TRIPWIRE fmt: codex-lhc-host — run cargo fmt"
  fail=1
fi

# ── Layer 3: golden smoke ──────────────────────────────────────────────
# Armed by Chunk 1 certification (capture goldens + rebuild diff). Until
# then this is a LOUD skip, never a silent pass.
if [ -d codex-rs/lhc/goldens ]; then
  echo "TRIPWIRE golden: goldens exist but runner not wired — fix this script"
  fail=1
else
  echo "SKIP golden smoke: no goldens yet (armed by Chunk 1 certification)"
fi

pin=$(git -C codex-rs/lhc/vendor/long-horizon-context log -1 --format=%h 2>/dev/null)
echo "vendor pin: ${pin:-MISSING} (policy: certified lhc-rs-port commits only — FORK.md)"
[ -n "$pin" ] || fail=1

if [ "$fail" -eq 0 ]; then echo "ALL TRIPWIRES GREEN"; else echo "TRIPWIRES FAILED"; fi
exit "$fail"
