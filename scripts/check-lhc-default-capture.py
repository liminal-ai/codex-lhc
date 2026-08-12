#!/usr/bin/env python3
"""Prove that a bare `codex exec` captures a real turn into LHC."""

import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


PROMPT = "lhc-default-capture-probe-user"
REPLY = "lhc-default-capture-probe-assistant"
EXEC_EXIT_BOUND_SECONDS = 30
POST_TURN_EXIT_BOUND_SECONDS = 12


class ResponsesHandler(BaseHTTPRequestHandler):
    response_completed = threading.Event()
    response_completed_at: float | None = None

    def log_message(self, _format: str, *_args: object) -> None:
        pass

    def do_GET(self) -> None:
        body = json.dumps({"object": "list", "data": []}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        events = [
            {
                "type": "response.created",
                "response": {"id": "resp-lhc-default-capture"},
            },
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "id": "msg-lhc-default-capture",
                    "content": [{"type": "output_text", "text": REPLY}],
                },
            },
            {
                "type": "response.completed",
                "response": {
                    "id": "resp-lhc-default-capture",
                    "usage": {
                        "input_tokens": 1,
                        "input_tokens_details": None,
                        "output_tokens": 1,
                        "output_tokens_details": None,
                        "total_tokens": 2,
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
        self.wfile.flush()
        type(self).response_completed_at = time.monotonic()
        type(self).response_completed.set()


def captured_state(database: Path) -> tuple[list[str], list[str], int]:
    with sqlite3.connect(database, timeout=1) as connection:
        contents = [
            row[0]
            for row in connection.execute(
                "SELECT content FROM message_block ORDER BY message_id, block_index"
            )
        ]
        event_kinds = [
            row[0]
            for row in connection.execute(
                "SELECT event_kind FROM event ORDER BY event_order"
            )
        ]
        completed_turns = connection.execute(
            "SELECT COUNT(*) FROM turns "
            "WHERE status = 'closed' AND outcome = 'completed' "
            "AND closed_at_event_order IS NOT NULL"
        ).fetchone()[0]
        return contents, event_kinds, completed_turns


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--binary",
        type=Path,
        default=Path("codex-rs/target/release/codex"),
    )
    parser.add_argument(
        "--lhc-root",
        type=Path,
        help="persist the probe database at this path instead of inside the temporary root",
    )
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"missing codex binary: {binary}")

    server = ThreadingHTTPServer(("127.0.0.1", 0), ResponsesHandler)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="codex-lhc-default-") as temp:
            root = Path(temp)
            home = root / "home"
            codex_home = root / "codex-home"
            lhc_root = args.lhc_root.resolve() if args.lhc_root else root / "lhc"
            lhc_root.mkdir(parents=True, exist_ok=True)
            cwd = root / "cwd"
            home.mkdir()
            codex_home.mkdir()
            cwd.mkdir()
            provider = (
                '{name="LHC default capture probe",'
                f'base_url="http://127.0.0.1:{server.server_port}/v1",'
                'wire_api="responses",requires_openai_auth=false,'
                "supports_websockets=false,request_max_retries=0,stream_max_retries=0}"
            )
            command = [
                str(binary),
                "exec",
                "--skip-git-repo-check",
                "-C",
                str(cwd),
                "-m",
                "gpt-5.1",
                "-c",
                f"model_providers.lhc_probe={provider}",
                "-c",
                'model_provider="lhc_probe"',
                PROMPT,
            ]
            environment = os.environ.copy()
            environment.update(
                {
                    "HOME": str(home),
                    "CODEX_HOME": str(codex_home),
                    "CODEX_SQLITE_HOME": str(codex_home),
                    "CODEX_LHC_ROOT": str(lhc_root),
                }
            )
            started = time.monotonic()
            try:
                result = subprocess.run(
                    command,
                    env=environment,
                    capture_output=True,
                    text=True,
                    timeout=EXEC_EXIT_BOUND_SECONDS,
                    check=False,
                )
            except subprocess.TimeoutExpired as error:
                raise SystemExit(
                    "bare codex exec exceeded the total process safety timeout\n"
                    f"timeout: {EXEC_EXIT_BOUND_SECONDS}s\n"
                    f"stdout:\n{error.stdout or ''}\n"
                    f"stderr:\n{error.stderr or ''}"
                ) from error
            elapsed = time.monotonic() - started
            completed_at = ResponsesHandler.response_completed_at
            if not ResponsesHandler.response_completed.is_set() or completed_at is None:
                raise SystemExit(
                    "bare codex exec exited without the deterministic response.completed marker\n"
                    f"stdout:\n{result.stdout}\n"
                    f"stderr:\n{result.stderr}"
                )
            post_turn_elapsed = time.monotonic() - completed_at
            if post_turn_elapsed > POST_TURN_EXIT_BOUND_SECONDS:
                raise SystemExit(
                    "bare codex exec exceeded the post-turn shutdown bound\n"
                    f"elapsed: {post_turn_elapsed:.2f}s\n"
                    f"bound: {POST_TURN_EXIT_BOUND_SECONDS}s\n"
                    f"stdout:\n{result.stdout}\n"
                    f"stderr:\n{result.stderr}"
                )
            if result.returncode != 0:
                raise SystemExit(
                    "bare codex exec failed\n"
                    f"stdout:\n{result.stdout}\n"
                    f"stderr:\n{result.stderr}"
                )

            databases = sorted((lhc_root / "threads").glob("*.sqlite"))
            states = [captured_state(database) for database in databases]
            contents = [content for state in states for content in state[0]]
            event_kinds = [kind for state in states for kind in state[1]]
            completed_turns = sum(state[2] for state in states)
            if not (
                any(PROMPT in content for content in contents)
                and any(REPLY in content for content in contents)
                and "turn_end" in event_kinds
                and completed_turns > 0
            ):
                raise SystemExit(
                    "bare codex exec exited without a durable completed turn\n"
                    f"thread databases: {[str(path) for path in databases]}\n"
                    f"captured blocks: {contents}\n"
                    f"event kinds: {event_kinds}\n"
                    f"completed turns: {completed_turns}\n"
                    f"stdout:\n{result.stdout}\n"
                    f"stderr:\n{result.stderr}"
                )
            print(
                "ok bare-exec: bounded exit persisted prompt, assistant, "
                f"turn_end, and completed turn in {elapsed:.2f}s total, "
                f"{post_turn_elapsed:.2f}s after response.completed"
            )
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
