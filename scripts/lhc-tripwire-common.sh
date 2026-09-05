#!/usr/bin/env bash
# Shared tripwire reporting; sourcing this file performs no work.

lhc_tripwire_logs() {
  # Keep failed-run evidence; the caller/operator may remove it after review.
  lhc_log_dir=$(mktemp -d "${TMPDIR:-/tmp}/lhc-tripwire.XXXXXXXX") || return 1
  echo "Tripwire logs: $lhc_log_dir"
}

# 0 = verified ancestry, 1 = invalid local repository/pin,
# 2 = ancestry unavailable, 3 = refreshed pin is off the shared main line.
# Ancestry is not SDK certification; off-main pins retain the existing WARN policy.
lhc_check_pin() {
  local repo="$1"
  local pin behind status
  if ! pin=$(git -C "$repo" rev-parse --verify HEAD 2>"$lhc_log_dir/pin.log"); then
    echo "TRIPWIRE pin: missing or invalid local SDK pin"
    return 1
  fi
  echo "vendor pin: $pin (certification is a separate requirement)"
  if ! git -C "$repo" fetch origin main --quiet >>"$lhc_log_dir/pin.log" 2>&1; then
    echo "UNVERIFIED pin: could not refresh origin/main; cached ancestry is not current evidence"
    return 2
  fi
  if ! git -C "$repo" rev-parse --verify origin/main >>"$lhc_log_dir/pin.log" 2>&1; then
    echo "UNVERIFIED pin: refreshed origin/main ref is unavailable"
    return 2
  fi
  git -C "$repo" merge-base --is-ancestor "$pin" origin/main >>"$lhc_log_dir/pin.log" 2>&1
  status=$?
  case "$status" in
    0)
      if ! behind=$(git -C "$repo" rev-list --count "$pin"..origin/main 2>>"$lhc_log_dir/pin.log"); then
        echo "TRIPWIRE pin: could not read SDK ancestry distance"
        return 1
      fi
      echo "ok pin: on shared main line ($behind commits behind shared tip)"
      ;;
    1)
      echo "WARN pin: OFF the shared main line — reconcile before treating this as a resting state"
      return 3
      ;;
    *)
      echo "TRIPWIRE pin: ancestry check failed (git exit $status)"
      return 1
      ;;
  esac
}

lhc_report_result() {
  local failed="$1" pin_status="$2"
  echo "Tripwire logs retained at: $lhc_log_dir"
  if [ "$failed" -ne 0 ]; then
    echo "TRIPWIRES FAILED"
    return 1
  fi
  case "$pin_status" in
    0) echo "ALL TRIPWIRES GREEN" ;;
    2) echo "LOCAL TRIPWIRES GREEN; SDK ancestry UNVERIFIED" ;;
    3) echo "LOCAL TRIPWIRES GREEN; SDK pin WARN (off shared main line)" ;;
    *) echo "TRIPWIRES FAILED (invalid pin check status)"; return 1 ;;
  esac
}
