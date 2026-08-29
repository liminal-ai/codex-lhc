#!/usr/bin/env bash
set -u
SIDE="$1"; BIN="$2"
DEMO=/tmp/lim140-demo
export HOME=$DEMO/$SIDE-home CODEX_HOME=$DEMO/$SIDE-codex-home CODEX_SQLITE_HOME=$DEMO/$SIDE-codex-home CODEX_LHC_ROOT=$DEMO/$SIDE-lhc CODEX_API_KEY=dummy
curl -s -o /dev/null --max-time 1 -X POST http://127.0.0.1:4519/v1/responses || { python3 $DEMO/serve.py >$DEMO/$SIDE-server.log 2>&1 & sleep 1; }
PROVIDER='{name="LIM-140 empty-success shim",base_url="http://127.0.0.1:4519/v1",wire_api="responses",requires_openai_auth=false,supports_websockets=false,request_max_retries=0,stream_max_retries=0}'
$BIN --skip-git-repo-check -C $DEMO/cwd -m mock-model \
  -c "model_providers.mock_provider=$PROVIDER" -c 'model_provider="mock_provider"' \
  "LIM-140 empty-success demo: produce a final agent message."
echo "--- exit code: $?"
