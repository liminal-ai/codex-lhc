#!/usr/bin/env python3
"""Scenario regression the LIM-134 empty-success repair against a built codex-exec.

A completed Responses turn with a nonblank prompt and no assistant message
must yield turn Failed, a nonzero process exit, and no fabricated answer.

The mock speaks the same SSE shape as `sse(vec![ev_response_created, ev_completed])`
in `codex-rs/core/tests/common/responses.rs`. Provider wiring matches exec
tests: a local `model_providers.*` override with `wire_api="responses"` and
`supports_websockets=false` so the binary uses HTTP immediately instead of
retrying websocket upgrade.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BINARY = REPO_ROOT / "codex-rs" / "target" / "release" / "codex-exec"
LIM134_FAILURE = "Turn completed without producing an agent message."
PROMPT = "LIM-140 empty-success scenario: produce a final agent message."
ITERATION_TIMEOUT_SECONDS = 25
API_KEY_ENV_NAMES = (
    "OPENAI_API_KEY",
    "OPENAI_API_KEY_STAGING",
    "OPENAI_ACCESS_TOKEN",
    "CHATGPT_ACCESS_TOKEN",
)


def sse_empty_completed(response_id: str = "resp-empty-success") -> bytes:
    created = {"type": "response.created", "response": {"id": response_id}}
    completed = {
        "type": "response.completed",
        "response": {
            "id": response_id,
            "usage": {
                "input_tokens": 0,
                "input_tokens_details": None,
                "output_tokens": 0,
                "output_tokens_details": None,
                "total_tokens": 0,
            },
        },
    }
    body = "".join(
        f"event: {event['type']}\ndata: {json.dumps(event, separators=(',', ':'))}\n\n"
        for event in (created, completed)
    )
    return body.encode("utf-8")


class MockResponsesHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    request_log: list[str]
    log_lock: threading.Lock

    def log_message(self, format: str, *args: object) -> None:
        line = f"{self.address_string()} - {format % args}"
        with self.log_lock:
            self.request_log.append(line)

    def _path(self) -> str:
        return urlparse(self.path).path.rstrip("/")

    def _read_body(self) -> bytes:
        length = int(self.headers.get("Content-Length", "0") or 0)
        if length <= 0:
            return b""
        return self.rfile.read(length)

    def _send_bytes(self, status: int, content_type: str, body: bytes) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def do_GET(self) -> None:
        upgrade = (self.headers.get("Upgrade") or "").lower()
        if upgrade == "websocket":
            self._send_bytes(404, "text/plain", b"websocket not supported\n")
            return
        path = self._path()
        if path.endswith("/models"):
            self._send_bytes(
                200,
                "application/json",
                json.dumps({"models": []}, separators=(",", ":")).encode("utf-8"),
            )
            return
        self._send_bytes(404, "text/plain", b"not found\n")

    def do_POST(self) -> None:
        self._read_body()
        path = self._path()
        if path.endswith("/responses"):
            self._send_bytes(200, "text/event-stream", sse_empty_completed())
            return
        self._send_bytes(404, "text/plain", b"not found\n")


def start_mock_server() -> tuple[ThreadingHTTPServer, threading.Thread, list[str]]:
    request_log: list[str] = []
    MockResponsesHandler.request_log = request_log
    MockResponsesHandler.log_lock = threading.Lock()
    server = ThreadingHTTPServer(("127.0.0.1", 0), MockResponsesHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread, request_log


def child_env(codex_home: Path, home: Path) -> dict[str, str]:
    env = os.environ.copy()
    for name in API_KEY_ENV_NAMES:
        env.pop(name, None)
    for name in list(env):
        if name.upper().endswith("_PROXY") or name.upper() in {"ALL_PROXY", "NO_PROXY"}:
            env.pop(name, None)
    env.update(
        {
            "HOME": str(home),
            "CODEX_HOME": str(codex_home),
            "CODEX_SQLITE_HOME": str(codex_home),
            "CODEX_API_KEY": "dummy",
            "NO_PROXY": "*",
            "no_proxy": "*",
        }
    )
    return env


def exec_command(binary: Path, cwd: Path, base_url: str) -> list[str]:
    provider = (
        '{name="LIM-140 empty-success mock",'
        f'base_url="{base_url}",'
        'wire_api="responses",requires_openai_auth=false,'
        "supports_websockets=false,request_max_retries=0,stream_max_retries=0}"
    )
    return [
        str(binary),
        "--skip-git-repo-check",
        "--json",
        "-C",
        str(cwd),
        "-m",
        "mock-model",
        "-c",
        f"model_providers.mock_provider={provider}",
        "-c",
        'model_provider="mock_provider"',
        "-c",
        "features.enable_request_compression=false",
        "-c",
        "analytics.enabled=false",
        "-c",
        "approval_policy=never",
        PROMPT,
    ]


def jsonl_events(stdout: str) -> tuple[list[dict[str, Any]], list[str]]:
    events: list[dict[str, Any]] = []
    non_json: list[str] = []
    for raw in stdout.splitlines():
        line = raw.strip()
        if not line:
            continue
        try:
            parsed = json.loads(line)
        except json.JSONDecodeError:
            non_json.append(raw)
            continue
        if isinstance(parsed, dict):
            events.append(parsed)
        else:
            non_json.append(raw)
    return events, non_json


def agent_message_texts(events: list[dict[str, Any]]) -> list[str]:
    texts: list[str] = []
    for event in events:
        item = event.get("item")
        if not isinstance(item, dict):
            continue
        if item.get("type") != "agent_message":
            continue
        text = item.get("text")
        if isinstance(text, str) and text.strip():
            texts.append(text)
    return texts


def failure_messages(events: list[dict[str, Any]]) -> list[str]:
    messages: list[str] = []
    for event in events:
        if event.get("type") != "turn.failed":
            continue
        error = event.get("error")
        if isinstance(error, dict) and isinstance(error.get("message"), str):
            messages.append(error["message"])
        elif isinstance(event.get("message"), str):
            messages.append(event["message"])
    return messages


def evaluate_iteration(
    returncode: int | None,
    stdout: str,
    stderr: str,
    timed_out: bool,
    elapsed: float,
) -> tuple[bool, list[str]]:
    reasons: list[str] = []
    if timed_out:
        reasons.append(f"iteration exceeded {ITERATION_TIMEOUT_SECONDS}s timeout")
        return False, reasons
    if elapsed >= 30:
        reasons.append(f"iteration completed in {elapsed:.2f}s (>= 30s stop tripwire)")
    if returncode == 0:
        reasons.append("exit code was 0; expected nonzero")
    elif returncode is None:
        reasons.append("missing exit code")

    events, non_json = jsonl_events(stdout)
    types = [event.get("type") for event in events]
    if "turn.failed" not in types:
        reasons.append("JSONL missing turn.failed")
    if "turn.completed" in types:
        reasons.append("JSONL contains turn.completed (empty success was not reclassified)")

    messages = failure_messages(events)
    combined = "\n".join([stdout, stderr, *messages])
    if LIM134_FAILURE not in combined:
        reasons.append(f"missing LIM-134 failure message {LIM134_FAILURE!r}")

    fabricated = agent_message_texts(events)
    if fabricated:
        reasons.append(f"fabricated agent message text: {fabricated!r}")
    if non_json:
        reasons.append(f"non-JSON stdout lines in --json mode: {non_json!r}")

    lowered = combined.lower()
    for needle in ("api.openai.com", "chatgpt.com", "openai.com/v1"):
        if needle in lowered:
            reasons.append(f"output mentions real API host {needle}")

    return not reasons, reasons


def write_iteration_logs(
    directory: Path,
    index: int,
    command: list[str],
    returncode: int | None,
    stdout: str,
    stderr: str,
    elapsed: float,
    timed_out: bool,
    passed: bool,
    reasons: list[str],
    mock_log: list[str],
) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "stdout.txt").write_text(stdout, encoding="utf-8")
    (directory / "stderr.txt").write_text(stderr, encoding="utf-8")
    (directory / "mock-server.log").write_text("\n".join(mock_log) + "\n", encoding="utf-8")
    meta = {
        "iteration": index,
        "passed": passed,
        "reasons": reasons,
        "returncode": returncode,
        "elapsed_seconds": elapsed,
        "timed_out": timed_out,
        "command": command,
        "failure_message": LIM134_FAILURE,
        "prompt": PROMPT,
    }
    (directory / "meta.json").write_text(
        json.dumps(meta, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def run_iteration(
    index: int,
    binary: Path,
    server: ThreadingHTTPServer,
    keep_logs: Path | None,
    mock_log: list[str],
) -> tuple[bool, str, Path | None]:
    base_url = f"http://127.0.0.1:{server.server_port}/v1"
    with tempfile.TemporaryDirectory(prefix=f"lim140-empty-success-{index:03d}-") as temp:
        root = Path(temp)
        home = root / "home"
        codex_home = root / "codex-home"
        cwd = root / "cwd"
        home.mkdir()
        codex_home.mkdir()
        cwd.mkdir()
        command = exec_command(binary, cwd, base_url)
        env = child_env(codex_home, home)
        started = time.monotonic()
        timed_out = False
        returncode: int | None = None
        stdout = ""
        stderr = ""
        try:
            completed = subprocess.run(
                command,
                env=env,
                cwd=str(cwd),
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=ITERATION_TIMEOUT_SECONDS,
                check=False,
            )
            returncode = completed.returncode
            stdout = completed.stdout or ""
            stderr = completed.stderr or ""
        except subprocess.TimeoutExpired as error:
            timed_out = True
            stdout = error.stdout or ""
            stderr = error.stderr or ""
            if isinstance(stdout, bytes):
                stdout = stdout.decode("utf-8", errors="replace")
            if isinstance(stderr, bytes):
                stderr = stderr.decode("utf-8", errors="replace")
        elapsed = time.monotonic() - started
        passed, reasons = evaluate_iteration(
            returncode, stdout, stderr, timed_out, elapsed
        )
        log_dir: Path | None = None
        if keep_logs is not None:
            log_dir = keep_logs / f"iteration-{index:03d}"
        elif not passed:
            log_dir = Path(tempfile.mkdtemp(prefix=f"lim140-empty-success-fail-{index:03d}-"))
        if log_dir is not None:
            write_iteration_logs(
                log_dir,
                index,
                command,
                returncode,
                stdout,
                stderr,
                elapsed,
                timed_out,
                passed,
                reasons,
                mock_log,
            )
        status = "PASS" if passed else "FAIL"
        detail = ""
        if not passed:
            detail = f" reasons={reasons}"
            if log_dir is not None:
                detail += f" logs={log_dir}"
        line = (
            f"iteration {index:03d} {status} exit={returncode} "
            f"time={elapsed:.2f}s{detail}"
        )
        return passed, line, log_dir


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Deterministic scenario regression for the LIM-134 empty-success repair "
            "against a built codex-exec binary."
        )
    )
    parser.add_argument("--iterations", type=int, default=50, metavar="N")
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    parser.add_argument("--keep-logs", type=Path, metavar="DIR")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.iterations < 1:
        print("error: --iterations must be >= 1", file=sys.stderr)
        return 2
    binary = args.binary.resolve()
    if not binary.is_file():
        print(f"error: missing codex-exec binary: {binary}", file=sys.stderr)
        return 2
    keep_logs = args.keep_logs.resolve() if args.keep_logs is not None else None
    if keep_logs is not None:
        if keep_logs.exists():
            shutil.rmtree(keep_logs)
        keep_logs.mkdir(parents=True)

    server, _thread, mock_log = start_mock_server()
    started = time.monotonic()
    passed = 0
    failed = 0
    fail_logs: list[Path] = []
    try:
        for index in range(1, args.iterations + 1):
            ok, line, log_dir = run_iteration(
                index, binary, server, keep_logs, mock_log
            )
            print(line, flush=True)
            if ok:
                passed += 1
            else:
                failed += 1
                if log_dir is not None:
                    fail_logs.append(log_dir)
    finally:
        server.shutdown()
        server.server_close()

    wall = time.monotonic() - started
    print(
        f"summary {passed} pass / {failed} fail / {args.iterations} total "
        f"wall={wall:.2f}s binary={binary}",
        flush=True,
    )
    if failed:
        if fail_logs:
            print("preserved fail logs:", flush=True)
            for path in fail_logs:
                print(f"  {path}", flush=True)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
