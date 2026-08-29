#!/usr/bin/env bash
# LIM-140 demo common: start shim, launch TUI, clean up. Args: SIDE BINARY
set -u
SIDE="$1"; BINARY="$2"
DEMO=/tmp/lim140-demo
export HOME=$DEMO/$SIDE-home
export CODEX_HOME=$DEMO/$SIDE-codex-home
export CODEX_SQLITE_HOME=$DEMO/$SIDE-codex-home
export CODEX_LHC_ROOT=$DEMO/$SIDE-lhc
export CODEX_API_KEY=dummy
pkill -f 'lim140-demo/serve.py' 2>/dev/null; sleep 0.3
python3 $DEMO/serve.py >$DEMO/$SIDE-server.log 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null' EXIT
sleep 1
PROVIDER='{name="LIM-140 empty-success shim",base_url="http://127.0.0.1:4519/v1",wire_api="responses",requires_openai_auth=false,supports_websockets=false,request_max_retries=0,stream_max_retries=0}'
"$BINARY" \
  --dangerously-bypass-approvals-and-sandbox \
  -C $DEMO/cwd \
  -m mock-model \
  -c "model_providers.mock_provider=$PROVIDER" \
  -c 'model_provider="mock_provider"' \
  2>$DEMO/$SIDE-tui-stderr.log
