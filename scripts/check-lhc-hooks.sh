#!/usr/bin/env bash
# LHC fork tripwires — run after every upstream sync and before every push.
# Three layers (FORK.md "Sync drill"): sentinel count, compile, golden smoke.
# Exit nonzero on any tripped layer. Keep this script dependency-free.
#
# WHAT THIS SCRIPT ACTUALLY RUNS (keep in lockstep with FORK.md inventory —
# Phase 3 lesson: a gate you haven't enumerated is a gate you haven't run):
#   1. grep count of LHC-HOOK sentinels in core vs EXPECTED_HOOKS below
#   2a. cargo check -p codex-core -p codex-app-server -p codex-extension-api
#       (the crates that *carry* the hooks — not just the adapter)
#   2b. cargo test -p codex-lhc-host --lib
#   2c. cargo test -p codex-lhc-host --features test-util --test certification
#   2d. cargo test -p codex-core --lib lhc_capture_e2e  (F11 seam wiring)
#   2e. cargo fmt --check for the adapter crate
#   2f. dirty-submodule hard fail (F12): pin must match what we build
#   3. golden presence under codex-rs/lhc/goldens/ (byte-checked by 2c)
set -u
cd "$(dirname "$0")/.."
command -v cargo >/dev/null 2>&1 || . "$HOME/.cargo/env" 2>/dev/null || true
fail=0

# ── Layer 1: sentinel count ────────────────────────────────────────────
EXPECTED_HOOKS=30
found=$(grep -rl "LHC-HOOK" codex-rs --include="*.rs" --include="*.toml" 2>/dev/null \
        | grep -v "codex-rs/lhc/" | xargs -r grep -o "LHC-HOOK" | wc -l)
if [ "$found" -ne "$EXPECTED_HOOKS" ]; then
  echo "TRIPWIRE sentinel: expected $EXPECTED_HOOKS LHC-HOOK markers in core, found $found"
  grep -rn "LHC-HOOK" codex-rs --include="*.rs" --include="*.toml" 2>/dev/null | grep -v "codex-rs/lhc/"
  fail=1
else
  echo "ok sentinel: $found/$EXPECTED_HOOKS LHC-HOOK markers"
fi

# ── Layer 2f first: dirty submodule hard fail ──────────────────────────
vendor=codex-rs/lhc/vendor/long-horizon-context
if [ -d "$vendor/.git" ] || [ -f "$vendor/.git" ]; then
  if [ -n "$(git -C "$vendor" status --porcelain 2>/dev/null)" ]; then
    echo "TRIPWIRE vendor: submodule working tree is DIRTY — pin does not match what is built"
    git -C "$vendor" status --porcelain | head -20
    fail=1
  else
    pin=$(git -C "$vendor" log -1 --format=%h 2>/dev/null)
    echo "ok vendor: clean at ${pin:-MISSING}"
  fi
else
  echo "TRIPWIRE vendor: missing $vendor"
  fail=1
fi

# ── Layer 2a: compile the crates that carry hooks ─────────────────────
# Scoped check (not --all-targets): upstream has pre-existing test-only
# breakage (ItemCompletedEvent.started_at_ms) that is not ours.
if cargo check -q -p codex-core -p codex-app-server -p codex-extension-api \
    --manifest-path codex-rs/Cargo.toml >/tmp/lhc-hook-check.log 2>&1; then
  echo "ok check: codex-core + codex-app-server + codex-extension-api"
else
  echo "TRIPWIRE check: hooked crates failed to compile — first errors:"
  grep -E "^error" -A5 /tmp/lhc-hook-check.log | head -40
  fail=1
fi

# ── Layer 2b/2c: adapter unit + certification ─────────────────────────
if cargo test -q -p codex-lhc-host --lib --manifest-path codex-rs/Cargo.toml \
    >/tmp/lhc-hook-lib.log 2>&1; then
  echo "ok lib-test: codex-lhc-host --lib"
else
  echo "TRIPWIRE lib-test: codex-lhc-host --lib failed:"
  grep -E "^error|FAILED|panicked" -A3 /tmp/lhc-hook-lib.log | head -30
  fail=1
fi
# Neutralize UPDATE_LHC_GOLDENS so a polluted env cannot rewrite fixtures
# while the header claims byte-checked goldens (H9).
if env -u UPDATE_LHC_GOLDENS cargo test -q -p codex-lhc-host --features test-util \
    --test certification --manifest-path codex-rs/Cargo.toml \
    >/tmp/lhc-hook-cert.log 2>&1; then
  echo "ok cert-test: codex-lhc-host certification"
else
  echo "TRIPWIRE cert-test: certification failed:"
  grep -E "^error|FAILED|panicked" -A5 /tmp/lhc-hook-cert.log | head -40
  fail=1
fi

# ── Layer 2d: F11 e2e through real Session seam ───────────────────────
if cargo test -q -p codex-core --lib lhc_capture_e2e \
    --manifest-path codex-rs/Cargo.toml >/tmp/lhc-hook-e2e.log 2>&1; then
  echo "ok e2e: codex-core lhc_capture_e2e (real Session seam)"
else
  echo "TRIPWIRE e2e: Session seam test failed:"
  grep -E "^error|FAILED|panicked" -A5 /tmp/lhc-hook-e2e.log | head -40
  fail=1
fi

if cargo fmt --check --manifest-path codex-rs/lhc/codex-lhc-host/Cargo.toml >/dev/null 2>&1; then
  echo "ok fmt: codex-lhc-host"
else
  echo "TRIPWIRE fmt: codex-lhc-host — run cargo fmt"
  fail=1
fi

# ── Layer 3: golden presence (byte-equality is layer 2c) ───────────────
goldens=codex-rs/lhc/goldens
if [ ! -d "$goldens" ]; then
  echo "TRIPWIRE golden: missing $goldens"
  fail=1
else
  n=$(find "$goldens" -name '*.json' | wc -l)
  if [ "$n" -lt 15 ]; then
    echo "TRIPWIRE golden: expected >=15 golden json files, found $n"
    fail=1
  else
    echo "ok golden: $n fixture files (byte-checked by certification mapping_goldens)"
  fi
fi

pin=$(git -C "$vendor" log -1 --format=%h 2>/dev/null)
echo "vendor pin: ${pin:-MISSING} (policy: certified lhc-rs-port commits only — FORK.md)"
[ -n "$pin" ] || fail=1

if [ "$fail" -eq 0 ]; then echo "ALL TRIPWIRES GREEN"; else echo "TRIPWIRES FAILED"; fi
exit "$fail"
