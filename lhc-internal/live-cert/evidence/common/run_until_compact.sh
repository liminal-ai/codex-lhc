#!/usr/bin/env bash
# Drive tool-heavy codex exec / resume turns until a real LHC rewrite compact fires.
# A mid-turn compact that leaves the turn failed (e.g. ctc_ id API error) still counts
# as compact success if the rollout was rewritten.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=env.sh
source "$SCRIPT_DIR/env.sh"

SCENARIO_DIR="${1:?scenario evidence dir e.g. s1}"
MODE="${2:-fresh}"  # fresh | resume
SESSION_ID="${3:-}"
MAX_TURNS="${MAX_TURNS:-8}"
TIMEOUT_SECS="${TIMEOUT_SECS:-1200}"
START_TS=$(date +%s)

mkdir -p "$EVIDENCE_DIR/$SCENARIO_DIR"
LOG="$EVIDENCE_DIR/$SCENARIO_DIR/driver.log"
exec > >(tee -a "$LOG") 2>&1

echo "=== run_until_compact scenario=$SCENARIO_DIR mode=$MODE session=${SESSION_ID:-none} ==="
echo "start: $(date -Is)"

TOOL_PROMPTS=(
  "LIVE-CERT-MARKER-ALPHA: Read sample_data/readme.txt and sample_data/numbers.csv using tools. Run: ls -la sample_data; wc -l sample_data/*.txt; date. Quote the secret phrase BLUE-MARBLE-ORBIT-42. Do not modify files."
  "LIVE-CERT-MARKER-BETA: Read every sample_data/pad_*.txt via shell. Sum the value column of numbers.csv with python. Restate the secret phrase."
  "LIVE-CERT-MARKER-GAMMA: Run: find sample_data -type f | sort; md5sum sample_data/readme.txt; echo HELIOS. Remember markers ALPHA/BETA/GAMMA and secret phrase."
  "LIVE-CERT-MARKER-DELTA: Produce an inventory of sample_data with file sizes. Include secret phrase BLUE-MARBLE-ORBIT-42."
  "LIVE-CERT-MARKER-EPSILON: Run uname -a; pwd; ls sample_data | wc -l; head -3 sample_data/pad_1.txt. Summarize markers and secret phrase."
  "LIVE-CERT-MARKER-ZETA: Re-read numbers.csv and compute mean of value. Confirm BLUE-MARBLE-ORBIT-42 and Helios Compact."
  "LIVE-CERT-MARKER-ETA: Shell: for f in sample_data/pad_{1..10}.txt; do wc -c \$f; done. Report total bytes and secret phrase."
  "LIVE-CERT-MARKER-THETA: Read pad_11 through pad_20. Confirm BLUE-MARBLE-ORBIT-42 and all markers so far."
)

has_compact() {
  local r
  r="$(latest_rollout || true)"
  [[ -n "${r:-}" ]] || return 1
  local n
  n="$(count_compacted "$r")"
  [[ "$n" -ge 1 ]] && return 0
  [[ -f "${r}.prev" ]] && return 0
  return 1
}

turn=0
while [[ $turn -lt $MAX_TURNS ]]; do
  now=$(date +%s)
  elapsed=$((now - START_TS))
  if [[ $elapsed -ge $TIMEOUT_SECS ]]; then
    echo "TIMEOUT after ${elapsed}s without compact"
    copy_rollout_snapshot "$SCENARIO_DIR/timeout-state" || true
    exit 2
  fi

  if has_compact; then
    echo "COMPACT DETECTED before turn $turn"
    break
  fi

  prompt="${TOOL_PROMPTS[$turn]}"
  turn_log="$EVIDENCE_DIR/$SCENARIO_DIR/turn_${turn}.stdout"
  err_log="$EVIDENCE_DIR/$SCENARIO_DIR/turn_${turn}.stderr"
  echo "--- turn $turn (${elapsed}s elapsed) ---"
  echo "prompt: $prompt"

  copy_rollout_snapshot "$SCENARIO_DIR/pre-turn-$turn" || true

  set +e
  if [[ "$MODE" == "fresh" && $turn -eq 0 && -z "${SESSION_ID:-}" ]]; then
    RUST_LOG="${RUST_LOG}" codex_exec --json "$prompt" >"$turn_log" 2>"$err_log"
    rc=$?
  else
    if [[ -z "${SESSION_ID:-}" ]]; then
      r="$(latest_rollout || true)"
      if [[ -n "${r:-}" ]]; then
        SESSION_ID="$(session_id_from_rollout "$r")"
      fi
    fi
    echo "resume session=$SESSION_ID"
    RUST_LOG="${RUST_LOG}" codex_exec resume --json "$SESSION_ID" "$prompt" >"$turn_log" 2>"$err_log"
    rc=$?
  fi
  set -e
  echo "turn $turn exit=$rc"

  # Extract session id
  if [[ -z "${SESSION_ID:-}" ]]; then
    SESSION_ID="$(python3 - "$turn_log" <<'PY' || true
import json,sys
for line in open(sys.argv[1], errors="replace"):
    line=line.strip()
    if not line: continue
    try: o=json.loads(line)
    except: continue
    if o.get("type")=="thread.started" and o.get("thread_id"):
        print(o["thread_id"]); raise SystemExit
    if o.get("session_id"):
        print(o["session_id"]); raise SystemExit
PY
)"
  fi
  if [[ -z "${SESSION_ID:-}" ]]; then
    r="$(latest_rollout || true)"
    [[ -n "${r:-}" ]] && SESSION_ID="$(session_id_from_rollout "$r")"
  fi
  echo "session_id=$SESSION_ID"
  echo "$SESSION_ID" >"$EVIDENCE_DIR/$SCENARIO_DIR/session_id.txt"

  # Note rewrite log lines
  if grep -q 'LHC rollout rewrite installed' "$err_log" 2>/dev/null; then
    echo "saw rewrite log in turn $turn stderr"
  fi
  if grep -qi 'invalid_id_prefix\|ctc_' "$turn_log" "$err_log" 2>/dev/null; then
    echo "NOTE: ctc_/invalid_id_prefix error observed (post-compact API id issue)"
  fi

  copy_rollout_snapshot "$SCENARIO_DIR/post-turn-$turn" || true

  if has_compact; then
    echo "COMPACT DETECTED after turn $turn (rc=$rc)"
    break
  fi
  turn=$((turn + 1))
done

if ! has_compact; then
  echo "FAIL: no compact after $turn turns"
  exit 3
fi

r="$(latest_rollout)"
echo "final rollout: $r"
copy_rollout_snapshot "$SCENARIO_DIR/after-compact"
summarize_rollout "$r" | tee "$EVIDENCE_DIR/$SCENARIO_DIR/after-compact-summary.txt"
[[ -f "${r}.prev" ]] && cp -a "${r}.prev" "$EVIDENCE_DIR/$SCENARIO_DIR/rollout.prev"
if [[ -n "${SESSION_ID:-}" ]]; then
  query_lhc_turns "$SESSION_ID" "$EVIDENCE_DIR/$SCENARIO_DIR/lhc_db_query.txt" || true
fi
echo "$SESSION_ID" >"$EVIDENCE_DIR/$SCENARIO_DIR/session_id.txt"
echo "PASS compact phase for $SCENARIO_DIR"
exit 0
