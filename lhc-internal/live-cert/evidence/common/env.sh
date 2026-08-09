#!/usr/bin/env bash
# Shared env for Slice D layer-3 live certification.
# NEVER points at the user's real ~/.codex sessions.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
export LIVE_CERT_ROOT="${LIVE_CERT_ROOT:-$REPO_ROOT/lhc-docs}"
export EVIDENCE_DIR="${EVIDENCE_DIR:-$LIVE_CERT_ROOT/live-cert-evidence}"
export SCRATCH_HOME="${SCRATCH_HOME:-$LIVE_CERT_ROOT/live-cert-scratch}"
export WORKSPACE="${WORKSPACE:-$LIVE_CERT_ROOT/live-cert-workspace}"

# Prefer freshly-built fork binary
if [[ -x "$REPO_ROOT/codex-rs/target/release/codex" ]]; then
  export CODEX_BIN="$REPO_ROOT/codex-rs/target/release/codex"
elif [[ -x "$REPO_ROOT/codex-rs/target/debug/codex" ]]; then
  export CODEX_BIN="$REPO_ROOT/codex-rs/target/debug/codex"
else
  echo "ERROR: no codex binary at codex-rs/target/{release,debug}/codex" >&2
  exit 1
fi

export CODEX_HOME="$SCRATCH_HOME"
export CODEX_LHC_ROOT="$SCRATCH_HOME/lhc"
# Auth lives under CODEX_HOME (copied from real auth at setup)
mkdir -p "$CODEX_HOME" "$CODEX_LHC_ROOT" "$EVIDENCE_DIR" "$WORKSPACE"

# Low compact threshold + LHC feature (also in config.toml; CLI flags reinforce)
export RUST_LOG="${RUST_LOG:-codex_core::compact_lhc=debug,codex_lhc_host=debug,info}"

# Flags that work on both top-level and exec/resume subcommands.
# Compact threshold is intentionally high here; scenario scripts lower it
# with a trailing `-c model_auto_compact_token_limit=N` after derivation settles.
CONFIG_FLAGS=(
  --enable lhc_capture
  --disable remote_compaction_v2
  -c 'model="gpt-5.6-luna"'
  -c 'model_reasoning_effort="low"'
  -c 'model_auto_compact_token_limit=200000'
)

# exec-only flags (must come after the `exec` subcommand)
EXEC_FLAGS=(
  --dangerously-bypass-approvals-and-sandbox
  --skip-git-repo-check
  -C "$WORKSPACE"
)

# Usage:
#   codex_exec --json "prompt"
#   codex_exec resume --json "$sid" "prompt"
codex_exec() {
  # Caller passes args after `exec` (including optional `resume` and extra -c flags).
  "$CODEX_BIN" exec "${CONFIG_FLAGS[@]}" "${EXEC_FLAGS[@]}" "$@" </dev/null
}

# Back-compat name used in older script drafts
codex() {
  codex_exec "$@"
}

find_rollouts() {
  find "$CODEX_HOME/sessions" -name 'rollout-*.jsonl' 2>/dev/null | sort
}

latest_rollout() {
  find_rollouts | tail -n1
}

session_id_from_rollout() {
  local path="$1"
  python3 - "$path" <<'PY'
import json,sys
with open(sys.argv[1]) as f:
    for line in f:
        o=json.loads(line)
        if o.get("type")=="session_meta":
            p=o.get("payload") or {}
            print(p.get("id") or p.get("session_id") or "")
            break
PY
}

count_compacted() {
  local path="$1"
  python3 - "$path" <<'PY'
import json,sys
n=0
with open(sys.argv[1]) as f:
    for line in f:
        try:
            o=json.loads(line)
        except Exception:
            continue
        if o.get("type")=="compacted":
            n+=1
print(n)
PY
}

window_numbers() {
  local path="$1"
  python3 - "$path" <<'PY'
import json,sys
nums=[]
with open(sys.argv[1]) as f:
    for line in f:
        try:
            o=json.loads(line)
        except Exception:
            continue
        if o.get("type")=="compacted":
            p=o.get("payload") or {}
            nums.append(p.get("window_number"))
print(",".join(str(x) for x in nums))
PY
}

summarize_rollout() {
  local path="$1"
  python3 - "$path" <<'PY'
import json,sys,collections
from pathlib import Path
path=Path(sys.argv[1])
counts=collections.Counter()
compacted=[]
token_totals=[]
with path.open() as f:
    for line in f:
        try:
            o=json.loads(line)
        except Exception:
            continue
        t=o.get("type")
        counts[t]+=1
        if t=="compacted":
            p=o.get("payload") or {}
            rh=p.get("replacement_history") or []
            compacted.append({
                "window_number": p.get("window_number"),
                "window_id": p.get("window_id"),
                "previous_window_id": p.get("previous_window_id"),
                "first_window_id": p.get("first_window_id"),
                "message_len": len(p.get("message") or ""),
                "bands_len": len(rh),
            })
        if t=="event_msg":
            p=o.get("payload") or {}
            if p.get("type")=="token_count":
                info=p.get("info") or {}
                total=info.get("total_token_usage") or info.get("last_token_usage") or {}
                if isinstance(total, dict):
                    token_totals.append(total.get("total_tokens") or total.get("total") or total)
print("path:", path)
print("lines_by_type:", dict(counts))
print("compacted_count:", len(compacted))
for i,c in enumerate(compacted):
    print(f"  compacted[{i}]:", c)
if token_totals:
    print("token_count_events:", len(token_totals))
    print("newest_token_total:", token_totals[-1])
    print("all_token_totals:", token_totals)
prev=str(path)+".prev"
print("prev_exists:", Path(prev).exists())
if Path(prev).exists():
    print("prev_size:", Path(prev).stat().st_size)
print("active_size:", path.stat().st_size)
PY
}

query_lhc_turns() {
  local thread_id="$1"
  local out="${2:-/dev/stdout}"
  python3 - "$CODEX_LHC_ROOT" "$thread_id" <<'PY' | tee "$out"
import os,sys,sqlite3,glob,json
root, tid = sys.argv[1], sys.argv[2]

def encode(s):
    out=[]
    for b in s.encode():
        if (48<=b<=57) or (65<=b<=90) or (97<=b<=122) or b in (45,46,95):
            out.append(chr(b))
        else:
            out.append(f"%{b:02X}")
    return "".join(out)

candidates=[]
enc=encode(tid)
for p in (
    os.path.join(root,"threads",f"{enc}.sqlite"),
    os.path.join(root,"threads",f"{tid}.sqlite"),
    *glob.glob(os.path.join(root,"threads","*.sqlite")),
):
    if p and os.path.exists(p) and p not in candidates:
        candidates.append(p)

for db in candidates:
    # Prefer exact thread id match; still dump others if none match tid
    base=os.path.basename(db)
    exact = tid in base or enc in base
    print("=== DB:", db, "exact_match=", exact)
    try:
        con=sqlite3.connect(db)
        tables=[r[0] for r in con.execute(
            "SELECT name FROM sqlite_master WHERE type='table'").fetchall()]
        print("tables:", tables)
        if "turns" in tables:
            cols=[c[1] for c in con.execute("PRAGMA table_info(turns)")]
            rows=con.execute("SELECT * FROM turns ORDER BY turn_order").fetchall()
            print(f"--- turns ({len(rows)}) ---")
            for r in rows:
                d=dict(zip(cols,r))
                print(d)
            aborted=[dict(zip(cols,r)) for r in rows if (r[cols.index("outcome")] or "").lower()=="aborted"]
            print("aborted_turns:", aborted)
        if "message" in tables:
            cols=[c[1] for c in con.execute("PRAGMA table_info(message)")]
            rows=con.execute("SELECT * FROM message ORDER BY source_event_order").fetchall()
            print(f"--- message ({len(rows)}) ---")
            usage_sum_total=0
            usage_n=0
            for r in rows:
                d=dict(zip(cols,r))
                pu=d.get("provider_usage")
                if isinstance(pu,str) and pu:
                    try:
                        u=json.loads(pu)
                        d["provider_usage_parsed"]=u
                        if "total_tokens" in u:
                            usage_sum_total += int(u["total_tokens"])
                            usage_n += 1
                    except Exception:
                        pass
                for k,v in list(d.items()):
                    if isinstance(v,str) and len(v)>240 and k!="provider_usage":
                        d[k]=v[:240]+"..."
                print(d)
            print("provider_usage_calls:", usage_n, "sum_total_tokens:", usage_sum_total)
        if "thread_view" in tables:
            rows=con.execute("SELECT view_id,compact_point,covered_from,profile_name FROM thread_view").fetchall()
            print("thread_view:", rows)
        if "thread_view_band" in tables:
            rows=con.execute(
                "SELECT view_id,band,token_count,length(rendered_text) FROM thread_view_band"
            ).fetchall()
            print("thread_view_band:", rows)
        con.close()
        if exact:
            break
    except Exception as e:
        print("error", e)
PY
}

copy_rollout_snapshot() {
  local label="$1"  # e.g. s1/before-compact
  local dest_dir="$EVIDENCE_DIR/$label"
  mkdir -p "$dest_dir"
  local r
  r="$(latest_rollout || true)"
  if [[ -z "${r:-}" ]]; then
    echo "no rollout yet" | tee "$dest_dir/NO_ROLLOUT.txt"
    return 1
  fi
  cp -a "$r" "$dest_dir/rollout.jsonl"
  if [[ -f "${r}.prev" ]]; then
    cp -a "${r}.prev" "$dest_dir/rollout.jsonl.prev"
  fi
  summarize_rollout "$r" | tee "$dest_dir/summary.txt"
  echo "$r" > "$dest_dir/source_path.txt"
  session_id_from_rollout "$r" | tee "$dest_dir/session_id.txt"
}

echo "env: CODEX_BIN=$CODEX_BIN"
echo "env: CODEX_HOME=$CODEX_HOME"
echo "env: CODEX_LHC_ROOT=$CODEX_LHC_ROOT"
echo "env: WORKSPACE=$WORKSPACE"
echo "env: EVIDENCE_DIR=$EVIDENCE_DIR"
