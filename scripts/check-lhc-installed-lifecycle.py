#!/usr/bin/env python3
"""Bounded installed-binary LHC Compact, resume, and reconciliation canary.

This adapts the production live-cert rewrite and missing-rollout reconciliation
sequence to the deterministic local Responses server used by the default-capture
canary, so release qualification needs neither model credentials nor a soak.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


PROCESS_TIMEOUT_SECONDS = 60
GROW_TURNS = 4
LEGACY_DIAGNOSTIC = "LHC_COMPACT_ALGORITHM=legacy"


class ResponsesHandler(BaseHTTPRequestHandler):
    _lock = threading.Lock()
    _counter = 0

    def log_message(self, _format: str, *_args: object) -> None:
        pass

    def do_GET(self) -> None:
        self._send_json({"object": "list", "data": []})

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        request_body = self.rfile.read(length).decode("utf-8", "replace")
        with type(self)._lock:
            type(self)._counter += 1
            sequence = type(self)._counter
        marker = f"installed-lhc-lifecycle-reply-{sequence}"
        is_derivation = any(
            value in request_body
            for value in (
                "<instructions-for-summarizing>",
                "<system_instructions>",
                "You summarize tool output for an engineering record",
            )
        )
        text = (
            marker + " condensed derivation evidence"
            if is_derivation
            else marker + " " + ("bounded compact materialization evidence " * 400)
        )
        response_id = f"resp-installed-lifecycle-{sequence}"
        events = [
            {"type": "response.created", "response": {"id": response_id}},
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "id": f"msg-installed-lifecycle-{sequence}",
                    "content": [{"type": "output_text", "text": text}],
                },
            },
            {
                "type": "response.completed",
                "response": {
                    "id": response_id,
                    "usage": {
                        "input_tokens": 50 if is_derivation else 3000,
                        "input_tokens_details": None,
                        "output_tokens": 25 if is_derivation else 3000,
                        "output_tokens_details": None,
                        "total_tokens": 75 if is_derivation else 6000,
                    },
                },
            },
        ]
        body = "".join(
            f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_json(self, value: object) -> None:
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def inspect_rollout(path: Path) -> tuple[int, int, object | None]:
    lhc_boundaries = []
    with path.open(encoding="utf-8") as stream:
        for line in stream:
            value = json.loads(line)
            if value.get("type") == "compacted":
                payload = value.get("payload") or {}
                message = payload.get("message")
                if isinstance(message, str) and message.startswith(
                    ("lhc_compact_durable", "lhc_compact_marker")
                ):
                    lhc_boundaries.append(payload)
    if not lhc_boundaries:
        return 0, 0, None
    boundary = lhc_boundaries[-1]
    return (
        len(lhc_boundaries),
        len(boundary.get("replacement_history") or []),
        boundary.get("window_number"),
    )


def qualifies_lhc_boundary(
    inspection: tuple[int, int, object | None], previous_exists: bool
) -> bool:
    count, replacement_history_entries, window_number = inspection
    return (
        count == 1
        and replacement_history_entries > 0
        and window_number is not None
        and previous_exists
    )


def captured_closed_turns(lhc_root: Path) -> int:
    total = 0
    for database in (lhc_root / "threads").glob("*.sqlite"):
        with sqlite3.connect(database) as connection:
            total += connection.execute(
                "SELECT COUNT(*) FROM turns WHERE status = 'closed'"
            ).fetchone()[0]
    return total


def session_id(stdout: str) -> str:
    for line in stdout.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get("type") == "thread.started" and value.get("thread_id"):
            return value["thread_id"]
        if value.get("session_id"):
            return value["session_id"]
    raise RuntimeError(f"installed canary did not emit a session id:\n{stdout}")


def run_command(
    command: list[str], environment: dict[str, str]
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        command,
        env=environment,
        capture_output=True,
        text=True,
        timeout=PROCESS_TIMEOUT_SECONDS,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"installed canary command failed ({result.returncode}): {command}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def latest_rollout(codex_home: Path, thread_id: str) -> Path:
    matches = sorted((codex_home / "sessions").rglob(f"*{thread_id}*.jsonl"))
    if not matches:
        raise RuntimeError(f"no rollout for installed canary thread {thread_id}")
    return matches[-1]


def run_lifecycle(binary: Path, root: Path, mode: str) -> None:
    home = root / "home"
    codex_home = root / "codex-home"
    lhc_root = root / "lhc"
    cwd = root / "workspace"
    for path in (home, codex_home, lhc_root, cwd):
        path.mkdir(parents=True, exist_ok=True)

    server = ThreadingHTTPServer(("127.0.0.1", 0), ResponsesHandler)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    try:
        provider = (
            '{name="installed lifecycle probe",'
            f'base_url="http://127.0.0.1:{server.server_port}/v1",'
            'wire_api="responses",requires_openai_auth=false,'
            "supports_websockets=false,request_max_retries=0,stream_max_retries=0}"
        )
        environment = os.environ.copy()
        environment.update(
            {
                "HOME": str(home),
                "CODEX_HOME": str(codex_home),
                "CODEX_SQLITE_HOME": str(codex_home),
                "CODEX_LHC_ROOT": str(lhc_root),
                "RUST_LOG": "codex_core::compact_lhc=debug,codex_lhc_host=debug,info",
            }
        )
        if mode == "legacy":
            environment["LHC_COMPACT_ALGORITHM"] = "legacy"
        else:
            environment.pop("LHC_COMPACT_ALGORITHM", None)

        common = [
            str(binary),
            "exec",
            "--disable",
            "remote_compaction_v2",
            "-c",
            f"model_providers.installed_probe={provider}",
            "-c",
            'model_provider="installed_probe"',
            "-c",
            'model="gpt-5.1"',
            "--dangerously-bypass-approvals-and-sandbox",
            "--skip-git-repo-check",
            "-C",
            str(cwd),
        ]
        logs = []
        first = run_command(
            [
                *common,
                "-c",
                "model_auto_compact_token_limit=200000",
                "--json",
                "INSTALLED-LHC-GROW-0",
            ],
            environment,
        )
        logs.append(first.stderr)
        thread_id = session_id(first.stdout)
        for turn in range(1, GROW_TURNS):
            result = run_command(
                [
                    *common,
                    "-c",
                    "model_auto_compact_token_limit=200000",
                    "resume",
                    "--json",
                    thread_id,
                    f"INSTALLED-LHC-GROW-{turn}",
                ],
                environment,
            )
            logs.append(result.stderr)

        for attempt in range(2):
            result = run_command(
                [
                    *common,
                    "-c",
                    "model_auto_compact_token_limit=3000",
                    "resume",
                    "--json",
                    thread_id,
                    f"INSTALLED-LHC-COMPACT-{attempt}",
                ],
                environment,
            )
            logs.append(result.stderr)
            rollout = latest_rollout(codex_home, thread_id)
            inspection = inspect_rollout(rollout)
            count, bands, window_number = inspection
            previous_exists = Path(f"{rollout}.prev").is_file()
            if qualifies_lhc_boundary(inspection, previous_exists):
                break
        else:
            raise RuntimeError(
                "installed launcher did not produce an LHC Compact rewrite: "
                f"lhc_boundaries={count} replacement_history={bands} "
                f"window_number={window_number!r} prev={previous_exists}"
            )

        resumed = run_command(
            [
                *common,
                "-c",
                "model_auto_compact_token_limit=200000",
                "resume",
                "--json",
                thread_id,
                "INSTALLED-LHC-RESUME-AFTER-COMPACT",
            ],
            environment,
        )
        logs.append(resumed.stderr)
        if "installed-lhc-lifecycle-reply" not in resumed.stdout:
            raise RuntimeError("installed launcher did not resume after Compact")

        rollout = latest_rollout(codex_home, thread_id)
        rollout.unlink()
        reconciled = run_command(
            [
                *common,
                "-c",
                "model_auto_compact_token_limit=200000",
                "resume",
                "--json",
                thread_id,
                "INSTALLED-LHC-RESUME-AFTER-ROLLOUT-DELETE",
            ],
            environment,
        )
        logs.append(reconciled.stderr)
        regenerated = latest_rollout(codex_home, thread_id)
        count, bands, window_number = inspect_rollout(regenerated)
        if count != 1 or bands == 0 or window_number is None:
            raise RuntimeError(
                "installed launcher did not reconcile materialized Compact history"
            )
        if captured_closed_turns(lhc_root) < GROW_TURNS:
            raise RuntimeError(
                "installed launcher did not preserve captured closed turns"
            )

        diagnostics = "\n".join(logs)
        if mode == "legacy" and LEGACY_DIAGNOSTIC not in diagnostics:
            raise RuntimeError(
                "legacy installed lifecycle did not select the legacy algorithm"
            )
        if mode == "metadata-first" and LEGACY_DIAGNOSTIC in diagnostics:
            raise RuntimeError(
                "default installed lifecycle unexpectedly selected legacy"
            )
        print(
            f"INSTALLED_LHC_LIFECYCLE_PASS mode={mode} thread={thread_id} "
            f"compacted={count} bands={bands} window={window_number} "
            f"closed_turns={captured_closed_turns(lhc_root)}"
        )
    finally:
        server.shutdown()
        server.server_close()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--mode", choices=("metadata-first", "legacy"), required=True)
    parser.add_argument("--root", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"missing installed binary: {binary}")
    if args.root is not None:
        run_lifecycle(binary, args.root.resolve(), args.mode)
    else:
        with tempfile.TemporaryDirectory(prefix="codex-lhc-installed-") as temp:
            run_lifecycle(binary, Path(temp), args.mode)


if __name__ == "__main__":
    main()
