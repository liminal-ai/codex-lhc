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


class ResponsesHandler(BaseHTTPRequestHandler):
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


def captured_contents(database: Path) -> list[str]:
    with sqlite3.connect(database, timeout=1) as connection:
        return [
            row[0]
            for row in connection.execute(
                "SELECT content FROM message_block ORDER BY message_id, block_index"
            )
        ]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--binary",
        type=Path,
        default=Path("codex-rs/target/release/codex"),
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
            lhc_root = root / "lhc"
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
            result = subprocess.run(
                command,
                env=environment,
                capture_output=True,
                text=True,
                timeout=60,
                check=False,
            )
            if result.returncode != 0:
                raise SystemExit(
                    "bare codex exec failed\n"
                    f"stdout:\n{result.stdout}\n"
                    f"stderr:\n{result.stderr}"
                )

            deadline = time.monotonic() + 10
            databases: list[Path] = []
            contents: list[str] = []
            while time.monotonic() < deadline:
                databases = sorted((lhc_root / "threads").glob("*.sqlite"))
                try:
                    contents = [
                        content
                        for database in databases
                        for content in captured_contents(database)
                    ]
                except sqlite3.Error:
                    contents = []
                if any(PROMPT in content for content in contents) and any(
                    REPLY in content for content in contents
                ):
                    break
                time.sleep(0.1)
            else:
                raise SystemExit(
                    "bare codex exec did not capture both messages\n"
                    f"thread databases: {[str(path) for path in databases]}\n"
                    f"captured blocks: {contents}\n"
                    f"stdout:\n{result.stdout}\n"
                    f"stderr:\n{result.stderr}"
                )
            print(
                "ok bare-exec: default LHC capture wrote a thread database "
                "containing user and assistant messages"
            )
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
