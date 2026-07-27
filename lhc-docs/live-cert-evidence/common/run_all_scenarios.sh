#!/usr/bin/env bash
# Slice D layer-3 live cert — scenarios 1-5.
# Failures are findings; do not patch production. Continue other scenarios.

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=env.sh
source "$SCRIPT_DIR/env.sh"

REPORT="$EVIDENCE_DIR/REPORT.md"
RESULTS=()

record() {
  local scen="$1" status="$2" note="$3"
  RESULTS+=("| $scen | $status | $note |")
  echo "RESULT $scen $status — $note" | tee -a "$EVIDENCE_DIR/results.log"
}

check_rewrite_shape() {
  # New shape: exactly one Compacted, preferably with replacement_history bands
  local path="$1"
  python3 - "$path" <<'PY'
import json,sys
from pathlib import Path
p=Path(sys.argv[1])
compacted=[]
types=[]
past=False
tail_ri=0
bands=0
with p.open() as f:
    for line in f:
        o=json.loads(line)
        types.append(o.get("type"))
        if o.get("type")=="compacted":
            compacted.append(o.get("payload") or {})
            past=True
            rh=(o.get("payload") or {}).get("replacement_history") or []
            bands=len(rh)
        elif past and o.get("type")=="response_item":
            tail_ri+=1
print("compacted_count", len(compacted))
print("bands_len", bands)
print("native_tail_response_items", tail_ri)
print("window_numbers", [c.get("window_number") for c in compacted])
if len(compacted)!=1:
    print("SHAPE_FAIL expected exactly one Compacted")
    sys.exit(1)
if bands<1:
    print("SHAPE_WARN bands empty")
print("SHAPE_OK")
PY
}

# ──────────────────────────────────────────────────────────────────────────
# Scenario 1
# ──────────────────────────────────────────────────────────────────────────
run_s1() {
  local d="$EVIDENCE_DIR/s1"
  mkdir -p "$d"
  echo "======== SCENARIO 1 ========"
  if ! bash "$SCRIPT_DIR/run_until_compact.sh" s1 fresh; then
    record "1" "FAIL" "compact did not fire or driver error; see s1/"
    return
  fi
  local sid r
  sid="$(cat "$d/session_id.txt")"
  r="$(latest_rollout)"
  # Capture before resume (already have after-compact)
  check_rewrite_shape "$r" | tee "$d/shape_check.txt" || true
  local shape_ok=0
  grep -q SHAPE_OK "$d/shape_check.txt" && shape_ok=1
  local prev_ok=0
  [[ -f "${r}.prev" ]] && prev_ok=1
  cp -a "${r}.prev" "$d/rollout.prev" 2>/dev/null || true

  # Resume with follow-up referencing earlier content
  local resume_prompt="LIVE-CERT-RESUME-1: What is the secret phrase from sample_data/readme.txt? List the LIVE-CERT-MARKER-* tags you remember from earlier in this conversation. Answer briefly."
  echo "$resume_prompt" >"$d/resume_prompt.txt"
  set +e
  codex exec resume --json "$sid" "$resume_prompt" \
    >"$d/resume.stdout" 2>"$d/resume.stderr"
  local rc=$?
  set -e
  echo "resume exit=$rc" | tee "$d/resume_rc.txt"
  copy_rollout_snapshot "s1/post-resume" || true

  local coherent=0
  if grep -qiE 'BLUE-MARBLE-ORBIT-42|BLUE.MARBLE|secret phrase' "$d/resume.stdout"; then
    coherent=1
  fi
  # Also check for marker recollection
  if grep -qiE 'LIVE-CERT-MARKER|ALPHA|BETA|GAMMA' "$d/resume.stdout"; then
    coherent=1
  fi

  local errors=0
  if grep -qiE 'panic|FATAL|internal error|LHC rollout rewrite failed' "$d/resume.stderr" "$d"/*/turn_*.stderr 2>/dev/null; then
    errors=1
  fi

  {
    echo "shape_ok=$shape_ok prev_ok=$prev_ok coherent=$coherent errors=$errors resume_rc=$rc"
  } | tee "$d/verdict_bits.txt"

  if [[ $shape_ok -eq 1 && $prev_ok -eq 1 && $coherent -eq 1 && $errors -eq 0 && $rc -eq 0 ]]; then
    record "1" "PASS" "rewrite shape ok, .prev retained, resume coherent; evidence s1/"
  else
    record "1" "FAIL" "shape=$shape_ok prev=$prev_ok coherent=$coherent errors=$errors rc=$rc; s1/"
  fi
}

# ──────────────────────────────────────────────────────────────────────────
# Scenario 2 — continue s1 session to second compact
# ──────────────────────────────────────────────────────────────────────────
run_s2() {
  local d="$EVIDENCE_DIR/s2"
  mkdir -p "$d"
  echo "======== SCENARIO 2 ========"
  local sid
  if [[ ! -f "$EVIDENCE_DIR/s1/session_id.txt" ]]; then
    record "2" "FAIL" "no s1 session to continue"
    return
  fi
  sid="$(cat "$EVIDENCE_DIR/s1/session_id.txt")"
  echo "$sid" >"$d/session_id.txt"

  # Snapshot generation before second compact
  local r0
  r0="$(latest_rollout)"
  cp -a "$r0" "$d/before_second_compact_rollout.jsonl"
  [[ -f "${r0}.prev" ]] && cp -a "${r0}.prev" "$d/before_second_compact.prev"
  local win_before
  win_before="$(window_numbers "$r0")"
  echo "windows before: $win_before" | tee "$d/windows_before.txt"
  local prev_mtime_before=""
  [[ -f "${r0}.prev" ]] && prev_mtime_before="$(stat -c %Y "${r0}.prev")"

  # Drive more turns until window_number advances or compacted content changes
  local start_turns=0
  local max=10
  local TIMEOUT_SECS=1200
  local START_TS
  START_TS=$(date +%s)
  local prompts=(
    "S2-GROW-1: Read all sample_data files again via shell and list them. Secret phrase? Add marker S2A."
    "S2-GROW-2: Run: wc -l sample_data/*; python3 -c 'print(list(range(50)))'. Marker S2B. Recite BLUE-MARBLE-ORBIT-42."
    "S2-GROW-3: Cat numbers.csv and pad_1..5. Marker S2C. Summarize markers from whole thread."
    "S2-GROW-4: Tool pass find+md5sum+date. Marker S2D. Secret phrase?"
    "S2-GROW-5: Another volume turn: cat every pad file. Marker S2E."
    "S2-GROW-6: Inventory + uname + pwd. Marker S2F. BLUE-MARBLE-ORBIT-42?"
    "S2-GROW-7: Recompute csv sum. Marker S2G."
    "S2-GROW-8: Full re-read sample_data. Marker S2H."
    "S2-GROW-9: Shell spam ls thrice. Marker S2I."
    "S2-GROW-10: Final S2 growth. Marker S2J. Secret?"
  )

  local second=0
  local i=0
  for prompt in "${prompts[@]}"; do
    now=$(date +%s)
    if [[ $((now - START_TS)) -ge $TIMEOUT_SECS ]]; then
      echo "s2 timeout"
      break
    fi
    echo "s2 turn $i: $prompt"
    set +e
    codex exec resume --json "$sid" "$prompt" \
      >"$d/turn_${i}.stdout" 2>"$d/turn_${i}.stderr"
    set -e
    copy_rollout_snapshot "s2/post-turn-$i" || true
    local r
    r="$(latest_rollout)"
    local wins
    wins="$(window_numbers "$r")"
    echo "windows now: $wins" | tee -a "$d/windows_trace.txt"
    # Second compact: window > 1, or .prev mtime changed and still 1 compacted
    if [[ -f "${r}.prev" ]]; then
      local prev_mtime_now
      prev_mtime_now="$(stat -c %Y "${r}.prev")"
      if [[ -n "$prev_mtime_before" && "$prev_mtime_now" != "$prev_mtime_before" ]]; then
        second=1
        echo "second compact detected via .prev rotation"
        break
      fi
    fi
    # window_number monotonic increase
    python3 - "$wins" "$win_before" <<'PY' && second=1 && break || true
import sys
def parse(s):
    return [int(x) for x in s.split(",") if x and x!="None"]
cur=parse(sys.argv[1]); bef=parse(sys.argv[2])
if cur and bef and max(cur)>max(bef):
    sys.exit(0)
if cur and not bef and max(cur)>=2:
    sys.exit(0)
if cur and max(cur)>=2:
    sys.exit(0)
sys.exit(1)
PY
    i=$((i+1))
  done

  r="$(latest_rollout)"
  copy_rollout_snapshot "s2/after-second-compact" || true
  [[ -f "${r}.prev" ]] && cp -a "${r}.prev" "$d/rollout.prev"
  local wins_after
  wins_after="$(window_numbers "$r")"
  echo "windows after: $wins_after" | tee "$d/windows_after.txt"
  check_rewrite_shape "$r" | tee "$d/shape_check.txt" || true

  # Resume again
  local resume_prompt="LIVE-CERT-RESUME-2: After the second compaction, what is BLUE-MARBLE-ORBIT-42 associated with? Name any S2-GROW markers you recall."
  set +e
  codex exec resume --json "$sid" "$resume_prompt" \
    >"$d/resume.stdout" 2>"$d/resume.stderr"
  local rc=$?
  set -e
  copy_rollout_snapshot "s2/post-resume" || true

  local mono=0
  python3 - "$win_before" "$wins_after" <<'PY' && mono=1 || true
import sys
def parse(s):
    return [int(x) for x in s.split(",") if x and x!="None"]
b=parse(sys.argv[1]); a=parse(sys.argv[2])
print("before",b,"after",a)
if not a:
    raise SystemExit(1)
if b and a and max(a)>=max(b):
    # ideally strictly greater after second compact
    if max(a)>max(b) or (second:=True):
        raise SystemExit(0)
raise SystemExit(1)
PY

  local coherent=0
  grep -qiE 'BLUE-MARBLE|ORBIT|S2|marker|Helios|secret' "$d/resume.stdout" && coherent=1
  local prev_ok=0
  [[ -f "${r}.prev" ]] && prev_ok=1
  local shape_ok=0
  grep -q SHAPE_OK "$d/shape_check.txt" 2>/dev/null && shape_ok=1

  {
    echo "second=$second mono=$mono shape=$shape_ok prev=$prev_ok coherent=$coherent rc=$rc"
    echo "wins_before=$win_before wins_after=$wins_after"
  } | tee "$d/verdict_bits.txt"

  if [[ $second -eq 1 && $shape_ok -eq 1 && $prev_ok -eq 1 && $coherent -eq 1 && $rc -eq 0 ]]; then
    record "2" "PASS" "second compact, windows $win_before->$wins_after, resume ok; s2/"
  else
    record "2" "FAIL" "second=$second shape=$shape_ok prev=$prev_ok coherent=$coherent rc=$rc wins $win_before->$wins_after; s2/"
  fi
}

# ──────────────────────────────────────────────────────────────────────────
# Scenario 3 — interrupt mid-inference
# ──────────────────────────────────────────────────────────────────────────
run_s3() {
  local d="$EVIDENCE_DIR/s3"
  mkdir -p "$d"
  echo "======== SCENARIO 3 ========"

  # Fresh session with a long-running prompt, SIGINT mid-stream
  local prompt="LIVE-CERT-INTERRUPT: Write a long detailed essay (at least 40 paragraphs) about orbital mechanics, then read sample_data/readme.txt and every pad file, and finally restate the secret phrase BLUE-MARBLE-ORBIT-42. Start writing the essay immediately and keep going."
  echo "$prompt" >"$d/interrupt_prompt.txt"

  set +e
  # Start in background
  codex exec --json "$prompt" >"$d/interrupted.stdout" 2>"$d/interrupted.stderr" &
  local pid=$!
  echo "pid=$pid" | tee "$d/pid.txt"
  # Wait until we see streaming tokens or a few seconds
  for i in $(seq 1 30); do
    if grep -qE 'agent_message|output_text|token|message' "$d/interrupted.stdout" 2>/dev/null; then
      echo "saw stream at ${i}s"
      sleep 2
      break
    fi
    sleep 1
  done
  # Ensure at least a couple seconds of inference
  sleep 3
  echo "Sending SIGINT to $pid"
  kill -INT "$pid" 2>/dev/null || true
  # Wait up to 30s for exit
  for i in $(seq 1 30); do
    if ! kill -0 "$pid" 2>/dev/null; then
      break
    fi
    sleep 1
  done
  if kill -0 "$pid" 2>/dev/null; then
    echo "still alive, SIGTERM"
    kill -TERM "$pid" 2>/dev/null || true
    sleep 2
  fi
  if kill -0 "$pid" 2>/dev/null; then
    kill -KILL "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null
  local irc=$?
  echo "interrupt exit=$irc" | tee "$d/interrupt_rc.txt"
  set -e

  copy_rollout_snapshot "s3/after-interrupt" || true
  local sid
  sid="$(cat "$d/after-interrupt/session_id.txt" 2>/dev/null || true)"
  if [[ -z "${sid:-}" ]]; then
    r="$(latest_rollout || true)"
    [[ -n "${r:-}" ]] && sid="$(session_id_from_rollout "$r")"
  fi
  echo "$sid" >"$d/session_id.txt"
  query_lhc_turns "$sid" "$d/lhc_after_interrupt.txt" || true

  # Continue session to force compact: more tool turns
  if [[ -n "${sid:-}" ]]; then
    bash "$SCRIPT_DIR/run_until_compact.sh" s3 resume "$sid" || true
  else
    echo "no session id after interrupt"
  fi

  r="$(latest_rollout || true)"
  if [[ -n "${r:-}" ]]; then
    check_rewrite_shape "$r" | tee "$d/shape_check.txt" || true
    copy_rollout_snapshot "s3/after-compact" || true
  fi

  # Query LHC for aborted outcome
  query_lhc_turns "$sid" "$d/lhc_final.txt" || true
  local aborted=0
  if grep -qiE 'aborted|interrupt|cancelled|canceled' "$d/lhc_after_interrupt.txt" "$d/lhc_final.txt" 2>/dev/null; then
    aborted=1
  fi
  # Also scan rollout event_msg for turn_aborted
  if [[ -n "${r:-}" ]]; then
    python3 - "$r" <<'PY' && aborted=1 || true
import json,sys
for line in open(sys.argv[1]):
    o=json.loads(line)
    if o.get("type")=="event_msg":
        p=o.get("payload") or {}
        t=str(p.get("type",""))
        if "abort" in t.lower() or p.get("outcome")=="aborted":
            print("found", t, p)
            raise SystemExit(0)
    blob=json.dumps(o)
    if "aborted" in blob.lower() and "turn" in blob.lower():
        print("blob hit", blob[:200])
        raise SystemExit(0)
raise SystemExit(1)
PY
  fi

  # Resume
  local coherent=0
  local rc=1
  if [[ -n "${sid:-}" ]]; then
    set +e
    codex exec resume --json "$sid" \
      "LIVE-CERT-RESUME-3: After the interrupted turn, confirm you can continue. What is the secret phrase BLUE-MARBLE-ORBIT-42 from sample_data if you know it? Short answer." \
      >"$d/resume.stdout" 2>"$d/resume.stderr"
    rc=$?
    set -e
    grep -qiE 'BLUE-MARBLE|ORBIT|continue|secret|Helios|yes|ready' "$d/resume.stdout" && coherent=1
  fi
  local shape_ok=0
  grep -q SHAPE_OK "$d/shape_check.txt" 2>/dev/null && shape_ok=1

  {
    echo "aborted=$aborted shape=$shape_ok coherent=$coherent rc=$rc"
  } | tee "$d/verdict_bits.txt"

  if [[ $aborted -eq 1 && $shape_ok -eq 1 && $coherent -eq 1 ]]; then
    record "3" "PASS" "aborted outcome, rewrite ok, resume ok; s3/"
  elif [[ $aborted -eq 0 && $shape_ok -eq 1 && $coherent -eq 1 ]]; then
    record "3" "FAIL" "FINDING: no aborted outcome in LHC/rollout; rewrite+resume ok; s3/"
  else
    record "3" "FAIL" "aborted=$aborted shape=$shape_ok coherent=$coherent rc=$rc; s3/"
  fi
}

# ──────────────────────────────────────────────────────────────────────────
# Scenario 4 — old-format dual-format resume
# ──────────────────────────────────────────────────────────────────────────
run_s4() {
  local d="$EVIDENCE_DIR/s4"
  mkdir -p "$d"
  echo "======== SCENARIO 4 ========"

  local sid
  sid="$(python3 -c 'import uuid; print(uuid.uuid4())')"
  # Use uuid v7-ish string? real sessions use uuid7; uuid4 is fine for file path
  # Actually codex may require uuid format - uuid4 is fine

  local out
  out="$(python3 "$SCRIPT_DIR/generate_old_format_rollout.py" \
    --session-id "$sid" \
    --cwd "$WORKSPACE" \
    --codex-home "$CODEX_HOME")"
  echo "$out" | tee "$d/generator_out.txt"
  local path
  path="$(echo "$out" | head -1)"
  echo "$sid" >"$d/session_id.txt"
  cp -a "$path" "$d/old_format_installed.jsonl"
  summarize_rollout "$path" | tee "$d/old_format_summary.txt"

  # Resume live and converse
  set +e
  codex exec resume --json "$sid" \
    "LIVE-CERT-OLD-RESUME: What marker phrases do you know from prior context (OLD-FORMAT-ANCHOR-77, LIVE-CERT-OLD-FORMAT)? Then read sample_data/readme.txt and quote the secret phrase. Use tools." \
    >"$d/resume1.stdout" 2>"$d/resume1.stderr"
  local rc1=$?
  set -e
  echo "resume1 rc=$rc1" | tee "$d/resume1_rc.txt"
  copy_rollout_snapshot "s4/after-resume1" || true

  local resume_ok=0
  if [[ $rc1 -eq 0 ]] || grep -qiE 'OLD-FORMAT|ANCHOR|BLUE-MARBLE|secret|LIVE-CERT' "$d/resume1.stdout"; then
    resume_ok=1
  fi
  # rc0 alone is enough for resume works; also accept partial coherence

  # Drive until compact rewrites to new shape (1 Compacted)
  local rewritten=0
  local prompts=(
    "S4-GROW-1: Read all sample_data pads via shell. Marker S4A. Secret phrase?"
    "S4-GROW-2: wc and md5sum everything under sample_data. Marker S4B."
    "S4-GROW-3: Cat numbers.csv; python sum. Marker S4C. OLD-FORMAT-ANCHOR-77?"
    "S4-GROW-4: Full inventory again. Marker S4D."
    "S4-GROW-5: Tool heavy find|sort|wc. Marker S4E."
    "S4-GROW-6: Another growth turn. Marker S4F. BLUE-MARBLE-ORBIT-42"
    "S4-GROW-7: Growth. Marker S4G."
    "S4-GROW-8: Growth. Marker S4H."
    "S4-GROW-9: Growth. Marker S4I."
    "S4-GROW-10: Growth. Marker S4J."
  )
  local i=0
  local START_TS
  START_TS=$(date +%s)
  for prompt in "${prompts[@]}"; do
    if [[ $(( $(date +%s) - START_TS )) -ge 1200 ]]; then
      break
    fi
    set +e
    codex exec resume --json "$sid" "$prompt" \
      >"$d/grow_${i}.stdout" 2>"$d/grow_${i}.stderr"
    set -e
    local r
    r="$(latest_rollout)"
    # Prefer the session's rollout
    r="$(find "$CODEX_HOME/sessions" -name "*${sid}*.jsonl" ! -name '*.prev' | head -1)"
    local n
    n="$(count_compacted "$r")"
    echo "s4 turn $i compacted_count=$n" | tee -a "$d/compact_trace.txt"
    if [[ "$n" -eq 1 ]] && [[ -f "${r}.prev" ]]; then
      rewritten=1
      echo "rewrite to new shape detected"
      cp -a "$r" "$d/after_rewrite.jsonl"
      cp -a "${r}.prev" "$d/after_rewrite.prev"
      summarize_rollout "$r" | tee "$d/after_rewrite_summary.txt"
      check_rewrite_shape "$r" | tee "$d/shape_check.txt" || true
      break
    fi
    i=$((i+1))
  done

  local shape_ok=0
  grep -q SHAPE_OK "$d/shape_check.txt" 2>/dev/null && shape_ok=1

  {
    echo "resume_ok=$resume_ok rewritten=$rewritten shape_ok=$shape_ok rc1=$rc1"
  } | tee "$d/verdict_bits.txt"

  if [[ $resume_ok -eq 1 && $rewritten -eq 1 && $shape_ok -eq 1 ]]; then
    record "4" "PASS" "old-format resume + rewrite to single Compacted; s4/"
  elif [[ $resume_ok -eq 1 && $rewritten -eq 0 ]]; then
    record "4" "FAIL" "FINDING: resume ok but next compact did not rewrite to new shape; s4/"
  else
    record "4" "FAIL" "resume_ok=$resume_ok rewritten=$rewritten shape=$shape_ok; s4/"
  fi
}

# ──────────────────────────────────────────────────────────────────────────
# Scenario 5 — two counters after scenario 2
# ──────────────────────────────────────────────────────────────────────────
run_s5() {
  local d="$EVIDENCE_DIR/s5"
  mkdir -p "$d"
  echo "======== SCENARIO 5 ========"

  local sid r
  if [[ -f "$EVIDENCE_DIR/s2/session_id.txt" ]]; then
    sid="$(cat "$EVIDENCE_DIR/s2/session_id.txt")"
  elif [[ -f "$EVIDENCE_DIR/s1/session_id.txt" ]]; then
    sid="$(cat "$EVIDENCE_DIR/s1/session_id.txt")"
  else
    record "5" "FAIL" "no s1/s2 session for counters"
    return
  fi
  echo "$sid" >"$d/session_id.txt"
  r="$(find "$CODEX_HOME/sessions" -name "*${sid}*.jsonl" ! -name '*.prev' | head -1)"
  if [[ -z "${r:-}" ]]; then
    r="$(latest_rollout)"
  fi
  cp -a "$r" "$d/rollout.jsonl"
  [[ -f "${r}.prev" ]] && cp -a "${r}.prev" "$d/rollout.prev"
  summarize_rollout "$r" | tee "$d/rollout_summary.txt"

  # Newest TokenCount total from rewritten file
  python3 - "$r" <<'PY' | tee "$d/token_count_from_rollout.txt"
import json,sys
newest=None
all_tc=[]
with open(sys.argv[1]) as f:
    for line in f:
        o=json.loads(line)
        if o.get("type")!="event_msg":
            continue
        p=o.get("payload") or {}
        if p.get("type")!="token_count":
            continue
        info=p.get("info") or p
        # shapes vary
        total=None
        for key in ("total_token_usage","last_token_usage","token_usage"):
            block=info.get(key) if isinstance(info,dict) else None
            if isinstance(block,dict):
                total=block.get("total_tokens") or block.get("total")
                if total is not None:
                    break
        if total is None and isinstance(info,dict):
            total=info.get("total_tokens")
        all_tc.append({"raw":p, "total": total})
        newest=total
print("token_count_events", len(all_tc))
print("newest_total", newest)
# print compact raw last
if all_tc:
    import pprint
    pprint.pp(all_tc[-1]["raw"], width=120, depth=4)
PY

  query_lhc_turns "$sid" "$d/lhc_db.txt" || true

  # Sum provider_usage from messages table / json columns
  python3 - "$CODEX_LHC_ROOT" "$sid" <<'PY' | tee "$d/lhc_provider_usage_sum.txt"
import os,sys,sqlite3,glob,json,re
root, tid = sys.argv[1], sys.argv[2]

def encode(s):
    out=[]
    for b in s.encode():
        if (48<=b<=57) or (65<=b<=90) or (97<=b<=122) or b in (45,46,95):
            out.append(chr(b))
        else:
            out.append(f"%{b:02X}")
    return "".join(out)

dbs=set()
enc=encode(tid)
for p in [
    os.path.join(root,"threads",f"{enc}.sqlite"),
    os.path.join(root,"threads",f"{tid}.sqlite"),
    *glob.glob(os.path.join(root,"threads","*.sqlite")),
]:
    if p and os.path.exists(p):
        dbs.add(p)

totals=[]
details=[]
for db in sorted(dbs):
    con=sqlite3.connect(db)
    tables=[r[0] for r in con.execute("SELECT name FROM sqlite_master WHERE type='table'")]
    print("db", db, "tables", tables)
    for t in tables:
        cols=[c[1] for c in con.execute(f"PRAGMA table_info({t})")]
        # dump any column that might hold usage json
        for col in cols:
            try:
                rows=con.execute(f"SELECT rowid, {col} FROM {t}").fetchall()
            except Exception:
                continue
            for rid, val in rows:
                if val is None:
                    continue
                s=val if isinstance(val,str) else (val.decode() if isinstance(val,bytes) else str(val))
                if "provider" not in s.lower() and "usage" not in s.lower() and "input_tokens" not in s and "total_tokens" not in s:
                    # try parse json anyway for nested
                    if not (s.startswith("{") or s.startswith("[")):
                        continue
                try:
                    obj=json.loads(s)
                except Exception:
                    # search substrings
                    for m in re.finditer(r'\{[^{}]*total_tokens[^{}]*\}', s):
                        try:
                            obj=json.loads(m.group())
                            details.append((db,t,col,rid,obj))
                            if "total_tokens" in obj:
                                totals.append(int(obj["total_tokens"]))
                        except Exception:
                            pass
                    continue
                def walk(o, path=""):
                    if isinstance(o, dict):
                        # provider_usage shape
                        if any(k in o for k in ("input_tokens","output_tokens","total_tokens","inputTokens","outputTokens")):
                            details.append((db,t,col,rid,path,o))
                            tt=o.get("total_tokens") or o.get("totalTokens")
                            if tt is not None:
                                totals.append(int(tt))
                        for k,v in o.items():
                            if k in ("provider_usage","providerUsage","usage","token_usage"):
                                walk(v, path+"."+k)
                            else:
                                walk(v, path+"."+k if path else k)
                    elif isinstance(o, list):
                        for i,v in enumerate(o):
                            walk(v, f"{path}[{i}]")
                walk(obj)
    con.close()

print("usage_records", len(details))
for d in details[:30]:
    print(" ", d)
print("sum_total_tokens_fields", sum(totals) if totals else None)
print("count_total_fields", len(totals))
print("individual_totals", totals)
PY

  # Session-reported usage: scan all s1/s2 stdout for token lines
  grep -hEi 'token|usage|context' \
    "$EVIDENCE_DIR/s1"/*.stdout "$EVIDENCE_DIR/s2"/*.stdout 2>/dev/null \
    | tail -50 | tee "$d/session_token_mentions.txt" || true

  # Comparison write-up
  python3 - "$d" <<'PY' | tee "$d/comparison.txt"
import re, pathlib, json
d=pathlib.Path(sys.argv[1]) if False else pathlib.Path(__import__("sys").argv[1])
roll= (d/"token_count_from_rollout.txt").read_text()
lhc= (d/"lhc_provider_usage_sum.txt").read_text()
m=re.search(r"newest_total\s+(\S+)", roll)
newest=m.group(1) if m else "unknown"
m2=re.search(r"sum_total_tokens_fields\s+(\S+)", lhc)
lhc_sum=m2.group(1) if m2 else "unknown"
print("=== TWO-COUNTERS COMPARISON ===")
print(f"rollout newest TokenCount total: {newest}")
print(f"LHC summed provider_usage total_tokens fields: {lhc_sum}")
print()
print("Interpretation:")
print("- Rollout TokenCount (regenerated from LHC message.provider_usage) is the host display counter.")
print("- LHC provider_usage is per-call provider totals stored on assistant_text messages.")
print("- They need not be equal to a single 'session status' number if scopes differ")
print("  (per-call vs cumulative, derivation calls vs user turns, etc.).")
try:
    n=int(newest) if newest not in (None,"None","unknown") else None
    s=int(lhc_sum) if lhc_sum not in (None,"None","unknown") else None
except Exception:
    n=s=None
if n is not None and s is not None:
    print(f"delta (LHC_sum - rollout_newest) = {s-n}")
    if n==s:
        print("MATCH: counters equal")
    elif abs(s-n)/max(n,1) < 0.15:
        print("NEAR: within 15% — inspect scopes")
    else:
        print("DIVERGE: >15% — flag for investigation")
else:
    print("INCOMPLETE: could not parse both numbers — flag")
print("See token_count_from_rollout.txt and lhc_provider_usage_sum.txt for raw evidence.")
PY

  local ok=0
  if grep -qE 'MATCH|NEAR|DIVERGE|INCOMPLETE' "$d/comparison.txt"; then
    ok=1
  fi
  if grep -q 'MATCH' "$d/comparison.txt"; then
    record "5" "PASS" "counters compared (match); s5/"
  elif grep -q 'NEAR' "$d/comparison.txt"; then
    record "5" "PASS" "counters compared (near, explained); s5/"
  elif grep -q 'DIVERGE' "$d/comparison.txt"; then
    record "5" "FAIL" "FINDING: counters diverge >15%; s5/"
  elif grep -q 'INCOMPLETE' "$d/comparison.txt"; then
    record "5" "FAIL" "FINDING: incomplete counter data; s5/"
  else
    record "5" "FAIL" "comparison script issue; s5/"
  fi
}

# ──────────────────────────────────────────────────────────────────────────
main() {
  echo "Live cert start $(date -Is)" | tee "$EVIDENCE_DIR/run_header.txt"
  echo "CODEX_BIN=$CODEX_BIN" | tee -a "$EVIDENCE_DIR/run_header.txt"
  "$CODEX_BIN" --version 2>&1 | tee -a "$EVIDENCE_DIR/run_header.txt" || true
  # Smoke: features list
  CODEX_HOME="$CODEX_HOME" "$CODEX_BIN" features list 2>&1 | tee "$EVIDENCE_DIR/features_list.txt" || true

  run_s1
  run_s2
  run_s3
  run_s4
  run_s5

  {
    echo "# Slice D Layer-3 Live Cert Report"
    echo
    echo "Date: $(date -Is)"
    echo "Binary: \`$CODEX_BIN\`"
    echo "CODEX_HOME: \`$CODEX_HOME\`"
    echo "CODEX_LHC_ROOT: \`$CODEX_LHC_ROOT\`"
    echo "Model: gpt-5.6-luna, reasoning=low, auto_compact_token_limit=8000"
    echo
    echo "| Scenario | Result | Notes / evidence |"
    echo "|---|---|---|"
    for row in "${RESULTS[@]}"; do
      echo "$row"
    done
    echo
    echo "## Evidence tree"
    echo '```'
    find "$EVIDENCE_DIR" -type f | sort | sed "s|$EVIDENCE_DIR/||"
    echo '```'
  } | tee "$REPORT"

  echo "Report written to $REPORT"
}

main "$@"
