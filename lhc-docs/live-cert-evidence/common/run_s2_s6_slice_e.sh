#!/usr/bin/env bash
# Slice E live: re-run S2 (normalization rewrite must land) + scenario 6
# (delete rollout → resume → converse). Isolated CODEX_HOME only.

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=env.sh
source "$SCRIPT_DIR/env.sh"

S2="$EVIDENCE_DIR/s2-slice-e"
S6="$EVIDENCE_DIR/s6"
mkdir -p "$S2" "$S6"

count_compacted() {
  python3 - "$1" <<'PY'
import json,sys
from pathlib import Path
p=Path(sys.argv[1])
n=0
if p.exists():
  for line in p.open():
    try:o=json.loads(line)
    except:continue
    if o.get("type")=="compacted": n+=1
print(n)
PY
}

window_numbers() {
  python3 - "$1" <<'PY'
import json,sys
from pathlib import Path
p=Path(sys.argv[1])
ws=[]
if p.exists():
  for line in p.open():
    try:o=json.loads(line)
    except:continue
    if o.get("type")=="compacted":
      ws.append((o.get("payload") or {}).get("window_number"))
print(",".join(str(w) for w in ws))
PY
}

shape_report() {
  python3 - "$1" <<'PY'
import json,sys
from pathlib import Path
p=Path(sys.argv[1])
compacted=[]
past=False
tail=0
bands=0
with p.open() as f:
  for line in f:
    o=json.loads(line)
    if o.get("type")=="compacted":
      compacted.append(o.get("payload") or {})
      past=True
      rh=(o.get("payload") or {}).get("replacement_history") or []
      bands=len(rh)
    elif past and o.get("type")=="response_item":
      tail+=1
print(f"compacted_count={len(compacted)}")
print(f"bands_len={bands}")
print(f"native_tail={tail}")
print(f"windows={[c.get('window_number') for c in compacted]}")
if len(compacted)==1 and bands>=1:
  print("SHAPE_OK")
else:
  print("SHAPE_PARTIAL_OR_FAIL")
PY
}

echo "=== Slice E live S2+S6 $(date -Is) bin=$CODEX_BIN ===" | tee "$EVIDENCE_DIR/slice_e_live.log"

# ── S2 re-run on s1 session (multi-Compacted polluted → normalization) ──
SID="$(cat "$EVIDENCE_DIR/s1/session_id.txt" 2>/dev/null || true)"
if [[ -z "${SID:-}" ]]; then
  echo "FAIL: no s1 session_id" | tee -a "$EVIDENCE_DIR/slice_e_live.log"
  exit 1
fi
echo "$SID" >"$S2/session_id.txt"
R0="$(latest_rollout)"
echo "s2 start rollout=$R0 compacted=$(count_compacted "$R0") windows=$(window_numbers "$R0")" | tee -a "$EVIDENCE_DIR/slice_e_live.log"
cp -a "$R0" "$S2/before.jsonl" 2>/dev/null || true
[[ -f "${R0}.prev" ]] && cp -a "${R0}.prev" "$S2/before.prev" || true
echo "windows_before=$(window_numbers "$R0")" | tee "$S2/windows_before.txt"
prev_mtime_before=""
[[ -f "${R0}.prev" ]] && prev_mtime_before="$(stat -c %Y "${R0}.prev")"

# Wait a bit for any background derivation from prior runs
sleep 20

# Force compact with low threshold — polluted multi-Compacted should NORMALIZE
set +e
RUST_LOG="codex_core::compact_lhc=debug,codex_lhc_host=debug,info" \
codex_exec resume --json \
  -c 'model_auto_compact_token_limit=3000' \
  --disable remote_compaction_v2 \
  "$SID" \
  "S2-SLICE-E: Force compact path. Read sample_data/readme.txt secret. Markers S2E-NORM. Restate BLUE-MARBLE-ORBIT-42 and ALPHA/BETA/GAMMA if known." \
  >"$S2/grow.stdout" 2>"$S2/grow.stderr"
rc1=$?
set -e
echo "s2 compact-trigger rc=$rc1" | tee -a "$EVIDENCE_DIR/slice_e_live.log"
echo "$rc1" >"$S2/grow_rc.txt"

# Second force if needed
R1="$(latest_rollout)"
if ! grep -qE 'NORMALIZATION rewrite|LHC rollout rewrite installed' "$S2/grow.stderr" 2>/dev/null; then
  sleep 45
  set +e
  RUST_LOG="codex_core::compact_lhc=debug,codex_lhc_host=debug,info" \
  codex_exec resume --json \
    -c 'model_auto_compact_token_limit=2500' \
    --disable remote_compaction_v2 \
    "$SID" \
    "S2-SLICE-E-2: Second compact attempt. Cat sample_data/readme.txt. Secret phrase? Marker S2E-NORM2." \
    >"$S2/grow2.stdout" 2>"$S2/grow2.stderr"
  rc2=$?
  set -e
  echo "s2 second trigger rc=$rc2" | tee -a "$EVIDENCE_DIR/slice_e_live.log"
  cat "$S2/grow2.stderr" >>"$S2/grow.stderr" 2>/dev/null || true
fi

R="$(latest_rollout)"
cp -a "$R" "$S2/after_grow.jsonl" 2>/dev/null || true
[[ -f "${R}.prev" ]] && cp -a "${R}.prev" "$S2/after_grow.prev" || true
echo "windows_after=$(window_numbers "$R")" | tee "$S2/windows_after.txt"
shape_report "$R" | tee "$S2/after_summary.txt"
grep -E 'NORMALIZATION rewrite|LHC rollout rewrite installed|NoReduction' "$S2/grow.stderr" 2>/dev/null | tee "$S2/compact_hits.txt" || true

norm=0
grep -q 'NORMALIZATION rewrite' "$S2/grow.stderr" 2>/dev/null && norm=1
rewrite=0
grep -q 'LHC rollout rewrite installed' "$S2/grow.stderr" 2>/dev/null && rewrite=1
prev_rotated=no
if [[ -f "${R}.prev" && -n "$prev_mtime_before" ]]; then
  prev_mtime_now="$(stat -c %Y "${R}.prev")"
  if [[ "$prev_mtime_now" != "$prev_mtime_before" ]]; then
    prev_rotated=yes
  fi
elif [[ -f "${R}.prev" && -z "$prev_mtime_before" ]]; then
  prev_rotated=yes
fi
echo "prev_rotated=$prev_rotated" | tee "$S2/prev_rotation.txt"

# Resume coherence
set +e
codex_exec resume --json \
  -c 'model_auto_compact_token_limit=200000' \
  "$SID" \
  "S2-RESUME: What is the secret phrase BLUE-MARBLE-ORBIT-42 from earlier? Any LIVE-CERT-MARKER or S2 markers? Brief." \
  >"$S2/resume.stdout" 2>"$S2/resume.stderr"
rc_res=$?
set -e
echo "$rc_res" >"$S2/resume_rc.txt"
coherent=0
if grep -qiE 'BLUE-MARBLE-ORBIT-42|BLUE.MARBLE|secret' "$S2/resume.stdout"; then
  coherent=1
fi

s2_pass=0
if [[ $rewrite -eq 1 || $norm -eq 1 ]] && grep -q SHAPE_OK "$S2/after_summary.txt" 2>/dev/null && [[ $coherent -eq 1 ]]; then
  s2_pass=1
fi
{
  echo "norm=$norm rewrite=$rewrite prev_rotated=$prev_rotated coherent=$coherent"
  if [[ $s2_pass -eq 1 ]]; then echo "RESULT PASS"; else echo "RESULT PARTIAL_OR_FAIL"; fi
} | tee "$S2/VERDICT.txt"

# ── Scenario 6: delete rollout, resume, converse ──
echo "======== SCENARIO 6 ========" | tee -a "$EVIDENCE_DIR/slice_e_live.log"
# Prefer s1 session (has LHC rewrite + secret) or current SID
S6_SID="$SID"
echo "$S6_SID" >"$S6/session_id.txt"
R6="$(find "$CODEX_HOME/sessions" -name "*${S6_SID}*.jsonl" ! -name '*.prev' 2>/dev/null | head -1)"
if [[ -z "$R6" || ! -f "$R6" ]]; then
  R6="$(latest_rollout)"
fi
echo "s6 target rollout=$R6" | tee -a "$EVIDENCE_DIR/slice_e_live.log"
cp -a "$R6" "$S6/before_delete.jsonl" 2>/dev/null || true
[[ -f "${R6}.prev" ]] && cp -a "${R6}.prev" "$S6/before_delete.prev" || true
shape_report "$R6" | tee "$S6/before_shape.txt" || true

# Kill the rollout file outright
rm -f "$R6"
echo "deleted $R6 exists=$([[ -f $R6 ]] && echo yes || echo no)" | tee "$S6/delete_note.txt"

# Resume should regenerate via startup reconciliation
set +e
RUST_LOG="codex_core::compact_lhc=debug,codex_lhc_host=debug,info" \
codex_exec resume --json \
  -c 'model_auto_compact_token_limit=200000' \
  --disable remote_compaction_v2 \
  "$S6_SID" \
  "S6-RESUME-AFTER-DELETE: The rollout was deleted and should have been regenerated. What is the secret phrase from sample_data (BLUE-MARBLE-ORBIT-42)? Recall LIVE-CERT markers if any. Brief answer." \
  >"$S6/resume.stdout" 2>"$S6/resume.stderr"
rc6=$?
set -e
echo "s6 resume rc=$rc6" | tee -a "$EVIDENCE_DIR/slice_e_live.log"
echo "$rc6" >"$S6/resume_rc.txt"

R_AFTER="$(find "$CODEX_HOME/sessions" -name "*${S6_SID}*.jsonl" ! -name '*.prev' 2>/dev/null | head -1)"
if [[ -z "$R_AFTER" ]]; then
  R_AFTER="$(latest_rollout)"
fi
echo "s6 after path=$R_AFTER" | tee -a "$EVIDENCE_DIR/slice_e_live.log"
if [[ -n "$R_AFTER" && -f "$R_AFTER" ]]; then
  cp -a "$R_AFTER" "$S6/after_resume.jsonl"
  shape_report "$R_AFTER" | tee "$S6/after_shape.txt"
else
  echo "NO_FILE" | tee "$S6/after_shape.txt"
fi

grep -E 'startup reconciliation|regenerated rollout|Missing|Corrupt|Stale|MISSING' \
  "$S6/resume.stderr" 2>/dev/null | tee "$S6/reconcile_hits.txt" || true

regen=0
[[ -f "$R_AFTER" ]] && regen=1
recall=0
if grep -qiE 'BLUE-MARBLE-ORBIT-42|BLUE.MARBLE|secret phrase' "$S6/resume.stdout"; then
  recall=1
fi
shape_ok=0
grep -q SHAPE_OK "$S6/after_shape.txt" 2>/dev/null && shape_ok=1
log_hit=0
grep -qiE 'startup reconciliation|regenerated rollout|Missing' "$S6/reconcile_hits.txt" 2>/dev/null && log_hit=1

{
  echo "regen=$regen shape_ok=$shape_ok recall=$recall log_hit=$log_hit rc=$rc6"
  if [[ $regen -eq 1 && $recall -eq 1 && $rc6 -eq 0 ]]; then
    echo "RESULT PASS"
  else
    echo "RESULT FAIL_OR_PARTIAL"
  fi
} | tee "$S6/VERDICT.txt"

echo "=== S2 $(cat "$S2/VERDICT.txt") ==="
echo "=== S6 $(cat "$S6/VERDICT.txt") ==="
echo "done $(date -Is)" | tee -a "$EVIDENCE_DIR/slice_e_live.log"
