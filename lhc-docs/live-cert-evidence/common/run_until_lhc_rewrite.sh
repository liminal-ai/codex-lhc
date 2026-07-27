#!/usr/bin/env bash
# Build tool-heavy history with HIGH compact threshold (derivation settles),
# then drop threshold to force LHC compact rewrite.
# Success = "LHC rollout rewrite installed" in stderr + exactly 1 Compacted + .prev

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=env.sh
source "$SCRIPT_DIR/env.sh"

SCENARIO_DIR="${1:?s1}"
SESSION_ID="${2:-}"
mkdir -p "$EVIDENCE_DIR/$SCENARIO_DIR"
LOG="$EVIDENCE_DIR/$SCENARIO_DIR/driver.log"
exec > >(tee -a "$LOG") 2>&1

echo "=== run_until_lhc_rewrite $SCENARIO_DIR session=${SESSION_ID:-new} $(date -Is) ==="

# Phase A: grow session without compact (high limit already in config)
GROW_PROMPTS=(
  "LIVE-CERT-MARKER-ALPHA: Read sample_data/readme.txt and numbers.csv with tools. Run ls -la sample_data; date. Quote secret phrase BLUE-MARBLE-ORBIT-42."
  "LIVE-CERT-MARKER-BETA: Cat all sample_data/pad_*.txt via shell. Sum numbers.csv values with python. Restate secret phrase."
  "LIVE-CERT-MARKER-GAMMA: find sample_data | sort; md5sum sample_data/readme.txt; wc -l sample_data/*.txt. Remember ALPHA/BETA/GAMMA and secret."
)

for i in "${!GROW_PROMPTS[@]}"; do
  prompt="${GROW_PROMPTS[$i]}"
  echo "--- grow turn $i ---"
  out="$EVIDENCE_DIR/$SCENARIO_DIR/grow_${i}.stdout"
  err="$EVIDENCE_DIR/$SCENARIO_DIR/grow_${i}.stderr"
  set +e
  if [[ -z "${SESSION_ID:-}" ]]; then
    # high threshold (config default 200000)
    codex_exec --json \
      -c 'model_auto_compact_token_limit=200000' \
      --disable remote_compaction_v2 \
      "$prompt" >"$out" 2>"$err"
    rc=$?
    SESSION_ID="$(python3 -c 'import json,sys
for l in open(sys.argv[1],errors="replace"):
  try:o=json.loads(l)
  except:continue
  if o.get("type")=="thread.started":
    print(o["thread_id"]);break
' "$out")"
  else
    codex_exec resume --json \
      -c 'model_auto_compact_token_limit=200000' \
      --disable remote_compaction_v2 \
      "$SESSION_ID" "$prompt" >"$out" 2>"$err"
    rc=$?
  fi
  set -e
  echo "grow $i rc=$rc session=$SESSION_ID"
  echo "$SESSION_ID" >"$EVIDENCE_DIR/$SCENARIO_DIR/session_id.txt"
  copy_rollout_snapshot "$SCENARIO_DIR/post-grow-$i" || true
  # Allow background derivation to catch up
  echo "waiting 45s for LHC background derivation..."
  sleep 45
done

# Check pending work items
if [[ -n "${SESSION_ID:-}" ]]; then
  query_lhc_turns "$SESSION_ID" "$EVIDENCE_DIR/$SCENARIO_DIR/lhc_pre_compact.txt" || true
fi

# Snapshot BEFORE compact trigger
r="$(latest_rollout)"
cp -a "$r" "$EVIDENCE_DIR/$SCENARIO_DIR/before_compact_rollout.jsonl"
echo "before compact: $(count_compacted "$r") compacted, prev=$([[ -f ${r}.prev ]] && echo yes || echo no)"

# Phase B: force compact with low threshold + disable remote v2
# Pre-turn compact should fire; derivation should be settled after waits.
echo "--- compact-trigger turn ---"
out="$EVIDENCE_DIR/$SCENARIO_DIR/compact_trigger.stdout"
err="$EVIDENCE_DIR/$SCENARIO_DIR/compact_trigger.stderr"
set +e
RUST_LOG="codex_core::compact_lhc=debug,codex_lhc_host=debug,info" \
codex_exec resume --json \
  -c 'model_auto_compact_token_limit=3000' \
  --disable remote_compaction_v2 \
  "$SESSION_ID" \
  "LIVE-CERT-MARKER-COMPACT: One more tool pass: ls sample_data; cat sample_data/readme.txt. Restate BLUE-MARBLE-ORBIT-42 and all LIVE-CERT-MARKER tags." \
  >"$out" 2>"$err"
rc=$?
set -e
echo "compact-trigger rc=$rc"

# If no rewrite, try a second trigger turn after more wait
r="$(latest_rollout)"
rewrote=0
if grep -q 'LHC rollout rewrite installed' "$err" 2>/dev/null; then
  rewrote=1
fi
if [[ -f "${r}.prev" ]]; then
  rewrote=1
fi
# Exactly one compacted after rewrite (new shape)
n="$(count_compacted "$r")"

if [[ $rewrote -eq 0 ]]; then
  echo "no rewrite yet; wait 60s and retry trigger"
  sleep 60
  out2="$EVIDENCE_DIR/$SCENARIO_DIR/compact_trigger2.stdout"
  err2="$EVIDENCE_DIR/$SCENARIO_DIR/compact_trigger2.stderr"
  set +e
  RUST_LOG="codex_core::compact_lhc=debug,codex_lhc_host=debug,info" \
  codex_exec resume --json \
    -c 'model_auto_compact_token_limit=2000' \
    --disable remote_compaction_v2 \
    "$SESSION_ID" \
    "LIVE-CERT-MARKER-COMPACT2: Shell: date; wc -l sample_data/pad_1.txt. Secret phrase?" \
    >"$out2" 2>"$err2"
  rc2=$?
  set -e
  echo "compact-trigger2 rc=$rc2"
  if grep -q 'LHC rollout rewrite installed' "$err2" 2>/dev/null; then
    rewrote=1
  fi
  r="$(latest_rollout)"
  [[ -f "${r}.prev" ]] && rewrote=1
fi

copy_rollout_snapshot "$SCENARIO_DIR/after-compact" || true
r="$(latest_rollout)"
summarize_rollout "$r" | tee "$EVIDENCE_DIR/$SCENARIO_DIR/after-compact-summary.txt"
[[ -f "${r}.prev" ]] && cp -a "${r}.prev" "$EVIDENCE_DIR/$SCENARIO_DIR/rollout.prev"
cp -a "$r" "$EVIDENCE_DIR/$SCENARIO_DIR/after-compact-rollout.jsonl"
query_lhc_turns "$SESSION_ID" "$EVIDENCE_DIR/$SCENARIO_DIR/lhc_post_compact.txt" || true

# Grep rewrite evidence
grep -h 'LHC rollout rewrite installed\|LHC compact arm installed\|failing open\|NoReduction\|Unavailable' \
  "$EVIDENCE_DIR/$SCENARIO_DIR"/compact_trigger*.stderr \
  "$EVIDENCE_DIR/$SCENARIO_DIR"/grow_*.stderr 2>/dev/null \
  | tee "$EVIDENCE_DIR/$SCENARIO_DIR/compact_log_hits.txt" || true

n="$(count_compacted "$r")"
prev=0; [[ -f "${r}.prev" ]] && prev=1
{
  echo "rewrote=$rewrote compacted_count=$n prev=$prev session=$SESSION_ID"
} | tee "$EVIDENCE_DIR/$SCENARIO_DIR/rewrite_bits.txt"

if [[ $rewrote -eq 1 && $n -eq 1 ]]; then
  echo "PASS LHC rewrite for $SCENARIO_DIR"
  exit 0
fi
echo "FAIL no LHC rewrite (rewrote=$rewrote n=$n prev=$prev)"
exit 3
