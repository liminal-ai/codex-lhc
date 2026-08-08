#!/usr/bin/env bash
# LHC fork tripwires — run after every upstream sync and before every push.
# Three layers (FORK.md "Sync drill"): sentinel count, compile, golden smoke.
# Exit nonzero on any tripped layer. Keep this script dependency-free.
#
# WHAT THIS SCRIPT ACTUALLY RUNS (keep in lockstep with FORK.md inventory —
# Phase 3 lesson: a gate you haven't enumerated is a gate you haven't run):
#   0.  vendor submodule tree CLEAN (F12) — dirt in the certified port = fail.
#       Checked at start AND end so fmt-churn / accidental vendor edits cannot
#       slip through after later layers run.
#   1.  grep count of LHC-HOOK sentinels in core vs EXPECTED_HOOKS below
#   2a. cargo check -p codex-core -p codex-app-server -p codex-extension-api
#       (the crates that *carry* the hooks — not just the adapter)
#   2b. cargo test -p codex-lhc-host --lib
#   2c. cargo test -p codex-lhc-host --features test-util --test certification
#   2d. cargo test -p codex-core --lib lhc_capture_e2e  (F11 seam wiring)
#   2d1b. cargo test -p codex-core --lib config_schema_matches_fixture
#   2d2. cargo test compact_bridge + compact_lhc (capture→rebuild / arm)
#   2e. cargo fmt --check for the adapter crate
#   2e2. cargo clippy -p codex-lhc-host --lib --no-deps (-D unused -D dead_code)
#   3.  golden presence under codex-rs/lhc/goldens/ (byte-checked by 2c)
#   4.  history-reset drill: apply patches/lhc/0*.patch at patches/lhc/BASE and
#       require byte-identity with the live tree, plus full fork-file coverage
#   5.  slice D certification (cargo test -p codex-core --lib slice_d_
#       -- --test-threads=1): drill + dual-format + display + layer-2 matrix
#   0'. vendor CLEAN re-check (end of run)
set -u
cd "$(dirname "$0")/.."
command -v cargo >/dev/null 2>&1 || . "$HOME/.cargo/env" 2>/dev/null || true
fail=0

# ── Layer 0: vendor submodule CLEAN (F12) — also rechecked at end ──────
vendor=codex-rs/lhc/vendor/long-horizon-context
check_vendor_clean() {
  local phase="$1"
  if [ ! -d "$vendor/.git" ] && [ ! -f "$vendor/.git" ]; then
    echo "TRIPWIRE vendor[$phase]: missing $vendor"
    return 1
  fi
  # Fail on git error (not a repo, broken gitdir) — do not treat empty status as clean.
  local vendor_status
  if ! vendor_status=$(git -C "$vendor" status --porcelain 2>/tmp/lhc-vendor-git.err); then
    echo "TRIPWIRE vendor[$phase]: git status failed in $vendor"
    cat /tmp/lhc-vendor-git.err 2>/dev/null | head -10
    return 1
  fi
  if [ -n "$vendor_status" ]; then
    echo "TRIPWIRE vendor[$phase]: submodule working tree is DIRTY — certified port dirt is a fail"
    echo "  (fmt-churn or accidental edit in vendor/ must be restored before green)"
    echo "$vendor_status" | head -20
    return 1
  fi
  local pin
  if ! pin=$(git -C "$vendor" log -1 --format=%h 2>/tmp/lhc-vendor-git.err); then
    echo "TRIPWIRE vendor[$phase]: git log failed in $vendor"
    cat /tmp/lhc-vendor-git.err 2>/dev/null | head -10
    return 1
  fi
  if [ -z "$pin" ]; then
    echo "TRIPWIRE vendor[$phase]: empty pin from git log"
    return 1
  fi
  echo "ok vendor[$phase]: CLEAN at $pin"
  return 0
}
if ! check_vendor_clean start; then
  fail=1
fi

# ── Layer 1: sentinel count ────────────────────────────────────────────
EXPECTED_HOOKS=52
found=$(grep -rl "LHC-HOOK" codex-rs --include="*.rs" --include="*.toml" 2>/dev/null \
        | grep -v "codex-rs/lhc/" | xargs -r grep -o "LHC-HOOK" | wc -l)
if [ "$found" -ne "$EXPECTED_HOOKS" ]; then
  echo "TRIPWIRE sentinel: expected $EXPECTED_HOOKS LHC-HOOK markers in core, found $found"
  grep -rn "LHC-HOOK" codex-rs --include="*.rs" --include="*.toml" 2>/dev/null | grep -v "codex-rs/lhc/"
  fail=1
else
  echo "ok sentinel: $found/$EXPECTED_HOOKS LHC-HOOK markers"
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
# Filter is the `tests` submodule only — slice D lives in `slice_d_tests` and
# is gated by layer 5 (so crash-injection failpoint races don't contaminate
# the arm suite when both run under the broad `compact_lhc` substring).
if RUST_MIN_STACK=8388608 cargo test -q -p codex-core --lib 'compact_lhc::tests::' \
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

# ── Layer 4: the recovery drill reproduces this tree ──────────────────
# The layer this replaced applied 0007 alone to a worktree at HEAD. That can
# only be green while the work is uncommitted: once committed, HEAD already
# contains 0007 and the apply must fail. It was red from 3aa3a44d22 onward.
#
# What the series is actually for is FORK.md's history-reset drill, so test
# that: apply the WHOLE series to a detached worktree at the recorded upstream
# base and require byte-identity with the live tree for every file the series
# touches. True before and after a commit.
#
# Also enforces coverage: every fork-owned file under codex-rs/ outside the
# adapter tree must be in some patch. A file that is fork-owned and in no patch
# reconstructs as upstream's version — which is how 0007 came to declare a
# module whose file no patch carried (FORK.md §History-reset R3).
base_file=patches/lhc/BASE
main_root=$(pwd -P)
repro_fail=0
if [ ! -f "$base_file" ]; then
  echo "TRIPWIRE patch-repro: missing $base_file (upstream base for the series)"
  repro_fail=1
elif ! patch_base=$(git rev-parse --verify "$(cat "$base_file")^{commit}" 2>/dev/null); then
  echo "TRIPWIRE patch-repro: $base_file does not name a commit in this repo"
  repro_fail=1
else
  series=$(ls patches/lhc/0*.patch 2>/dev/null)
  if [ -z "$series" ]; then
    echo "TRIPWIRE patch-repro: no patches under patches/lhc/"
    repro_fail=1
  fi

  # Coverage: fork-owned core files (tracked-changed + untracked) vs series.
  covered=$(grep -h "^diff --git" $series 2>/dev/null | sed 's|^diff --git a/||;s| b/.*||' | sort -u)
  tracked=$(git diff --name-only "$patch_base" -- codex-rs/ ':!codex-rs/lhc/' 2>/dev/null)
  untracked=$(git ls-files --others --exclude-standard -- codex-rs/ 2>/dev/null | grep -v '^codex-rs/lhc/')
  # Cargo.lock is cargo-regenerated by policy (FORK.md inventory row 8).
  owned=$(printf '%s\n%s\n' "$tracked" "$untracked" | grep -v '^$' | grep -v '^codex-rs/Cargo.lock$' | sort -u)
  uncovered=$(comm -23 <(echo "$owned") <(echo "$covered"))
  if [ -n "$uncovered" ]; then
    echo "TRIPWIRE patch-repro: fork-owned file(s) in NO patch — the drill would"
    echo "  reconstruct upstream's version of these:"
    echo "$uncovered" | sed 's/^/    /'
    repro_fail=1
  fi
  dupes=$(echo "$covered" | uniq -d)
  [ -n "$dupes" ] && { echo "TRIPWIRE patch-repro: file in >1 patch (all patches share one base):"; echo "$dupes" | sed 's/^/    /'; repro_fail=1; }

  # The drill itself.
  scratch=$(mktemp -d)
  if ! git worktree add --detach "$scratch" "$patch_base" >/tmp/lhc-patch-wt.log 2>&1; then
    echo "TRIPWIRE patch-repro: git worktree add at $patch_base failed"
    head -10 /tmp/lhc-patch-wt.log
    repro_fail=1
  else
    applied=1
    for p in $series; do
      if ! (cd "$scratch" && git apply "$main_root/$p" >/tmp/lhc-patch-apply.log 2>&1); then
        echo "TRIPWIRE patch-repro: $p failed to apply at base $patch_base"
        head -20 /tmp/lhc-patch-apply.log
        applied=0
        repro_fail=1
        break
      fi
    done
    if [ "$applied" -eq 1 ]; then
      for f in $covered; do
        if [ ! -f "$scratch/$f" ]; then
          echo "TRIPWIRE patch-repro: $f missing from reconstructed tree"
          repro_fail=1
        elif [ ! -f "$main_root/$f" ]; then
          echo "TRIPWIRE patch-repro: $f in series but absent from working tree"
          repro_fail=1
        elif ! cmp -s "$scratch/$f" "$main_root/$f"; then
          echo "TRIPWIRE patch-repro: $f differs after the drill — first diff:"
          diff -u "$scratch/$f" "$main_root/$f" | head -30
          repro_fail=1
          break
        fi
      done
    fi
  fi
  git worktree remove --force "$scratch" >/dev/null 2>&1 || rm -rf "$scratch"
fi
if [ "$repro_fail" -eq 0 ]; then
  echo "ok patch-repro: drill at $(cut -c1-10 "$base_file") reproduces $(echo "$covered" | wc -l) files byte-identically"
else
  fail=1
fi

# ── Layer 5: slice D certification suite (drill + dual-format + L2 matrix) ─
# The regenerate-and-resume drill plus dual-format, display consumers, and the
# full layer-2 deterministic matrix. Serial threads avoid failpoint races
# between crash-injection scenarios that share the global SWAP_FAILPOINT.
if cargo test -q -p codex-core --lib slice_d_ \
    --manifest-path codex-rs/Cargo.toml -- --test-threads=1 \
    >/tmp/lhc-hook-drill.log 2>&1; then
  echo "ok slice-d: drill + dual-format + display + layer-2 matrix"
else
  echo "TRIPWIRE slice-d: certification suite failed:"
  grep -E "^error|FAILED|panicked" -A5 /tmp/lhc-hook-drill.log | head -40
  fail=1
fi

pin=$(git -C codex-rs/lhc/vendor/long-horizon-context log -1 --format=%h 2>/dev/null)
# Pin-drift check: a pin off the shared certified line is a PENDING
# RECONCILIATION, not a resting state. This warns on every run (sync
# drill included) until the pin is an ancestor of origin/main (the retired
# lhc-rs-port working branch folded into main 2026-08-08).
git -C codex-rs/lhc/vendor/long-horizon-context fetch origin main --quiet 2>/dev/null || true
if git -C codex-rs/lhc/vendor/long-horizon-context merge-base --is-ancestor HEAD origin/main 2>/dev/null; then
  behind=$(git -C codex-rs/lhc/vendor/long-horizon-context rev-list --count HEAD..origin/main 2>/dev/null)
  echo "ok pin: on certified shared line (behind shared tip by ${behind:-?} commits)"
else
  echo "WARN pin: OFF the shared certified line — side-branch pin awaiting"
  echo "  reconciliation (fold into main + re-pin; policy: FORK.md)."
  echo "  This warning repeats every run until resolved. It is not a resting state."
fi
echo "vendor pin: ${pin:-MISSING} (policy: certified main commits only — FORK.md)"
# Pin-drift check: a pin off the shared certified line is a PENDING
# reconciliation, not a resting state — this renags every run until fixed.
git -C codex-rs/lhc/vendor/long-horizon-context fetch origin main --quiet 2>/dev/null || true
if git -C codex-rs/lhc/vendor/long-horizon-context rev-parse --verify --quiet origin/main >/dev/null; then
  if git -C codex-rs/lhc/vendor/long-horizon-context merge-base --is-ancestor HEAD origin/main; then
    behind=$(git -C codex-rs/lhc/vendor/long-horizon-context rev-list --count HEAD..origin/main)
    echo "ok pin: on certified shared line ($behind commits behind shared tip)"
  else
    echo "WARN pin: OFF the shared certified line — side-branch pin awaiting"
    echo "  reconciliation (fold into main + re-pin; see FORK.md). Repeats every run."
  fi
else
  echo "SKIP pin-drift: shared branch unreachable (offline?)"
fi
[ -n "$pin" ] || fail=1

# ── Layer 0' (end): vendor CLEAN re-check — catch fmt-churn mid-run ────
if ! check_vendor_clean end; then
  fail=1
fi

if [ "$fail" -eq 0 ]; then echo "ALL TRIPWIRES GREEN"; else echo "TRIPWIRES FAILED"; fi
exit "$fail"
