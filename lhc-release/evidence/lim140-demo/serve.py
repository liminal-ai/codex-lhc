#!/usr/bin/env python3
"""LIM-140 demo: empty-success scenario shim provider on a fixed port.
Reuses the scenario-regression script's MockResponsesHandler verbatim."""
import importlib.util
import threading
from http.server import ThreadingHTTPServer

spec = importlib.util.spec_from_file_location(
    "scenario", "/srv/work/wt/codex-lhc-0150-2-qual/scripts/lhc-empty-success-scenario.py"
)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

mod.MockResponsesHandler.request_log = []
mod.MockResponsesHandler.log_lock = threading.Lock()
server = ThreadingHTTPServer(("127.0.0.1", 4519), mod.MockResponsesHandler)
print("empty-success shim serving on 127.0.0.1:4519", flush=True)
server.serve_forever()
