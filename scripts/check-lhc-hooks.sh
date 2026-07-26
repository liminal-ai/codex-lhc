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
EXPECTED_HOOKS=36
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
  # Fail on git error (not a repo, broken gitdir) — do not treat empty status as clean.
  if ! vendor_status=$(git -C "$vendor" status --porcelain 2>/tmp/lhc-vendor-git.err); then
    echo "TRIPWIRE vendor: git status failed in $vendor"
    cat /tmp/lhc-vendor-git.err 2>/dev/null | head -10
    fail=1
  elif [ -n "$vendor_status" ]; then
    echo "TRIPWIRE vendor: submodule working tree is DIRTY — pin does not match what is built"
    echo "$vendor_status" | head -20
    fail=1
  else
    if ! pin=$(git -C "$vendor" log -1 --format=%h 2>/tmp/lhc-vendor-git.err); then
      echo "TRIPWIRE vendor: git log failed in $vendor"
      cat /tmp/lhc-vendor-git.err 2>/dev/null | head -10
      fail=1
    elif [ -z "$pin" ]; then
      echo "TRIPWIRE vendor: empty pin from git log"
      fail=1
    else
      echo "ok vendor: clean at $pin"
    fi
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

# ── Layer 2d1b: fork must not break upstream's own tests ──────────────
# The `lhc_capture` feature flag adds a config.toml schema field, so
# core's `config_schema_matches_fixture` fails unless the checked-in
# fixture is regenerated (`just write-config-schema`). That broke from
# Chunk 1 and no gate noticed for two chunks, because every other layer
# runs a targeted LHC test subset. Any fork change that alters upstream
# config surface must keep upstream's own test green.
if cargo test -q -p codex-core --lib config_schema_matches_fixture \
    --manifest-path codex-rs/Cargo.toml >/tmp/lhc-hook-schema.log 2>&1; then
  echo "ok upstream-schema: core config_schema_matches_fixture"
else
  echo "TRIPWIRE upstream-schema: config schema fixture is stale —"
  echo "  run: cargo run -p codex-core --bin codex-write-config-schema"
  grep -E "^error|FAILED|panicked" -A5 /tmp/lhc-hook-schema.log | head -30
  fail=1
fi

# ── Layer 2d2: Chunk 2b compact arm + capture→rebuild (law 1/2) ───────
if cargo test -q -p codex-lhc-host --lib compact_bridge \
    --manifest-path codex-rs/Cargo.toml >/tmp/lhc-hook-bridge.log 2>&1; then
  echo "ok compact-bridge: codex-lhc-host produce + marker"
else
  echo "TRIPWIRE compact-bridge: failed:"
  grep -E "^error|FAILED|panicked" -A5 /tmp/lhc-hook-bridge.log | head -40
  fail=1
fi
if cargo test -q -p codex-core --lib compact_lhc \
    --manifest-path codex-rs/Cargo.toml >/tmp/lhc-hook-arm.log 2>&1; then
  echo "ok compact-arm: law1 write-back + law2 prefill + fail-open"
else
  echo "TRIPWIRE compact-arm: failed:"
  grep -E "^error|FAILED|panicked" -A5 /tmp/lhc-hook-arm.log | head -40
  fail=1
fi

if cargo fmt --check --manifest-path codex-rs/lhc/codex-lhc-host/Cargo.toml >/dev/null 2>&1; then
  echo "ok fmt: codex-lhc-host"
else
  echo "TRIPWIRE fmt: codex-lhc-host — run cargo fmt"
  fail=1
fi

# Clippy on the adapter only (--no-deps: vendored lhc has pre-existing noise).
# Deny unused/dead_code — the class of defect that shipped the heuristic sticker
# and silent no-op imports. Do not -D warnings workspace-wide (pre-existing style).
if cargo clippy -q -p codex-lhc-host --lib --no-deps --manifest-path codex-rs/Cargo.toml \
    -- -D unused -D dead_code >/tmp/lhc-hook-clippy.log 2>&1; then
  echo "ok clippy: codex-lhc-host --no-deps (-D unused -D dead_code)"
else
  echo "TRIPWIRE clippy: codex-lhc-host failed:"
  grep -E "^error|warning:" /tmp/lhc-hook-clippy.log | head -40
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

# ── Layer 4: patch 0007 *reproduces* working tree (not merely applies) ─
# A patch that applies cleanly but reconstructs pre-fix code is the green
# gate over broken state G2 closed. Apply 0007 on a detached HEAD worktree
# and require byte-identity with the live tree for every file it touches.
patch0007=patches/lhc/0007-lhc-compact-arm.patch
main_root=$(pwd -P)
if [ ! -f "$patch0007" ]; then
  echo "TRIPWIRE patch-repro: missing $patch0007"
  fail=1
else
  scratch=$(mktemp -d)
  repro_fail=0
  if ! git worktree add --detach "$scratch" HEAD >/tmp/lhc-patch-wt.log 2>&1; then
    echo "TRIPWIRE patch-repro: git worktree add failed"
    head -10 /tmp/lhc-patch-wt.log
    repro_fail=1
  elif ! (cd "$scratch" && git apply "$main_root/$patch0007" >/tmp/lhc-patch-apply.log 2>&1); then
    echo "TRIPWIRE patch-repro: $patch0007 failed to apply on HEAD"
    head -20 /tmp/lhc-patch-apply.log
    repro_fail=1
  else
    for f in \
      codex-rs/core/Cargo.toml \
      codex-rs/core/src/compact.rs \
      codex-rs/core/src/compact_lhc.rs \
      codex-rs/core/src/compact_lhc_tests.rs \
      codex-rs/core/src/lhc_inference_bridge.rs \
      codex-rs/core/src/lib.rs \
      codex-rs/core/src/session/mod.rs \
      codex-rs/core/src/session/turn.rs \
      codex-rs/core/src/state/session.rs \
      codex-rs/core/src/tasks/compact.rs
    do
      if [ ! -f "$scratch/$f" ] || [ ! -f "$main_root/$f" ]; then
        echo "TRIPWIRE patch-repro: missing $f after apply"
        repro_fail=1
        continue
      fi
      if ! diff -q "$scratch/$f" "$main_root/$f" >/dev/null 2>&1; then
        echo "TRIPWIRE patch-repro: $f differs from working tree after applying $patch0007"
        diff -u "$scratch/$f" "$main_root/$f" | head -30
        repro_fail=1
      fi
    done
  fi
  git worktree remove --force "$scratch" >/dev/null 2>&1 || rm -rf "$scratch"
  if [ "$repro_fail" -eq 0 ]; then
    echo "ok patch-repro: 0007 applies on HEAD and matches working tree"
  else
    fail=1
  fi
fi

pin=$(git -C "$vendor" log -1 --format=%h 2>/dev/null)
echo "vendor pin: ${pin:-MISSING} (policy: certified lhc-rs-port commits only — FORK.md)"
[ -n "$pin" ] || fail=1

if [ "$fail" -eq 0 ]; then echo "ALL TRIPWIRES GREEN"; else echo "TRIPWIRES FAILED"; fi
exit "$fail"
