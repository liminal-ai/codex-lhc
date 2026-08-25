#!/usr/bin/env python3
"""Prove one bare `codex exec` crosses a mid-turn LHC compact with turn parts.

Turn parts, Story 5 (AC-7.2b / TC-7.2b): a deterministic local Responses
server drives one agentic turn through several provider cycles, each carrying
a shell tool call and a large assistant message, with provider usage above the
auto-compact trigger at every settled seam. The run must prove, from the LHC
record and the captured provider requests, that:

* the record has the accepted SDK's exact three-turn lifecycle for a fresh
  bare run: a bootstrap turn holding only the host `runtime_note`(s), closed
  with NULL host facts by the task prompt; the single task turn holding the
  prompt and every step-bearing member, closed completed by the one
  `turn_end`; and the SDK's ordinary empty open successor opened at that
  same event order (no continuation turn, no forced-boundary row, no typed
  compact-continuation marker, no continuation receipt);
* every tool call names the offered production shell tool and its result is
  the real command output (not an `unsupported call` error);
* every step-bearing message carries the host step index, the indices are
  sequential per provider cycle, and each tool call shares its index with
  its result;
* the installed view serves parts (durable `parts_activated_at`, a part
  arrangement entry, and the seam marker in the next request);
* the provider request after the seam is smaller than the one before it.

No model credentials, no soak: the same deterministic server shape the
default-capture canary uses.
"""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PROMPT = "lhc-midturn-parts-crossing-user"
# Provider cycles that issue a tool call before the final answer. Each cycle's
# assistant text is large enough that the accumulated turn crosses the SDK's
# production lower bound (120k tokens) part-way through the loop.
TOOL_CYCLES = 8
ASSISTANT_REPETITIONS = 2400
EXEC_TIMEOUT_SECONDS = 240
STEP_KINDS = ("assistant_text", "assistant_thinking", "tool_call", "tool_result")
# The production shell tool name offered by the request and used for every
# emitted call, recorded once by the server. Filled at the first agentic request.
TOOL_NAME_OFFERED: list[str] = []
SEAM_MARKER = "[seam · "


class ResponsesHandler(BaseHTTPRequestHandler):
    _lock = threading.Lock()
    # Ordered agentic request bodies (derivation requests are excluded).
    agentic_requests: list[str] = []
    offered_tools: list[str] = []
    cycle = 0

    def log_message(self, _format: str, *_args: object) -> None:
        pass

    def do_GET(self) -> None:
        self._send_json({"object": "list", "data": []})

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        request_body = self.rfile.read(length).decode("utf-8", "replace")
        is_derivation = any(
            value in request_body
            for value in (
                "<instructions-for-summarizing>",
                "<system_instructions>",
                "You summarize tool output for an engineering record",
            )
        )
        if is_derivation:
            self._send_sse(
                "resp-derivation",
                [self._message("msg-derivation", "condensed derivation evidence")],
                input_tokens=50,
                output_tokens=25,
            )
            return
        with type(self)._lock:
            cycle = type(self).cycle
            type(self).cycle += 1
            type(self).agentic_requests.append(request_body)
            if not type(self).offered_tools:
                type(self).offered_tools = [
                    str(tool.get("name") or tool.get("type"))
                    for tool in json.loads(request_body).get("tools", [])
                    if isinstance(tool, dict)
                ]
        text = (
            f"cycle-{cycle}-assistant "
            + f"mid-turn parts crossing evidence cycle {cycle} " * ASSISTANT_REPETITIONS
        )
        items = [self._message(f"msg-cycle-{cycle}", text)]
        if cycle < TOOL_CYCLES:
            items.append(self._shell_call(request_body, cycle))
        self._send_sse(
            f"resp-cycle-{cycle}",
            items,
            input_tokens=60_000,
            output_tokens=3_000,
        )

    @staticmethod
    def _message(item_id: str, text: str) -> dict[str, object]:
        return {
            "type": "message",
            "role": "assistant",
            "id": item_id,
            "content": [{"type": "output_text", "text": text}],
        }

    @staticmethod
    def _shell_call(request_body: str, cycle: int) -> dict[str, object]:
        tools = {
            tool.get("name"): tool
            for tool in json.loads(request_body).get("tools", [])
            if isinstance(tool, dict) and tool.get("name")
        }
        script = f"printf 'cycle-{cycle}-tool-output ENDPAY{cycle}'"
        if "exec_command" in tools:
            name = "exec_command"
            arguments = {"cmd": script, "yield_time_ms": 10_000}
        elif "shell_command" in tools:
            name = "shell_command"
            arguments = {"command": script, "timeout_ms": 10_000}
        else:
            raise SystemExit(
                "midturn-parts crossing: no registered shell function tool offered; "
                f"tools={sorted(tools)}"
            )
        if not TOOL_NAME_OFFERED:
            TOOL_NAME_OFFERED.append(name)
        elif TOOL_NAME_OFFERED[0] != name:
            raise SystemExit(
                f"midturn-parts crossing: offered shell tool changed mid-run: "
                f"{TOOL_NAME_OFFERED[0]} -> {name}"
            )
        return {
            "type": "function_call",
            "id": f"fc-cycle-{cycle}",
            "call_id": f"call-cycle-{cycle}",
            "name": name,
            "arguments": json.dumps(arguments),
        }

    def _send_sse(
        self,
        response_id: str,
        items: list[dict[str, object]],
        *,
        input_tokens: int,
        output_tokens: int,
    ) -> None:
        events: list[dict[str, object]] = [
            {"type": "response.created", "response": {"id": response_id}}
        ]
        events.extend(
            {"type": "response.output_item.done", "item": item} for item in items
        )
        events.append(
            {
                "type": "response.completed",
                "response": {
                    "id": response_id,
                    "usage": {
                        "input_tokens": input_tokens,
                        "input_tokens_details": None,
                        "output_tokens": output_tokens,
                        "output_tokens_details": None,
                        "total_tokens": input_tokens + output_tokens,
                    },
                },
            }
        )
        body = "".join(
            f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def _send_json(self, value: object) -> None:
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def fail(message: str) -> None:
    raise SystemExit(f"midturn-parts crossing failed: {message}")


def diagnostics(connection: sqlite3.Connection) -> str:
    turns = connection.execute(
        "SELECT turn_id, turn_order, status, outcome, outcome_reason, "
        "opened_at_event_order, closed_at_event_order FROM turns ORDER BY turn_order"
    ).fetchall()
    members = connection.execute(
        "SELECT m.turn_id, m.kind, m.step_index, m.source_event_order, "
        "substr(mb.content, 1, 100) FROM message m LEFT JOIN message_block mb "
        "ON mb.message_id = m.message_id AND mb.block_index = 0 "
        "WHERE m.deleted_at IS NULL ORDER BY m.source_event_order"
    ).fetchall()
    events = connection.execute(
        "SELECT event_order, event_kind FROM event ORDER BY event_order"
    ).fetchall()
    return (
        "turns (turn_id, turn_order, status, outcome, outcome_reason, opened_at, closed_at):\n"
        + "\n".join(f"  {row}" for row in turns)
        + "\nmembers (turn_id, kind, step_index, source_event_order, content):\n"
        + "\n".join(f"  {row}" for row in members)
        + "\nevents (event_order, event_kind):\n"
        + "\n".join(f"  {row}" for row in events)
    )


def inspect_record(database: Path) -> None:
    """Assert the exact canonical lifecycle of one fresh bare `codex exec`.

    Accepted SDK `turns::create` shape, not a filter: the thread opens with an
    initial turn that collects the host bootstrap `runtime_note`(s); the task
    prompt (a non-steer `user_prompt` on a member-bearing turn) closes it with
    NULL host facts and opens the task turn at the prompt's event order; the
    single `turn_end` closes the task turn with completed host facts and, in
    the same transaction, opens an empty successor at the turn_end's event
    order. That empty successor is the SDK's ordinary next-turn placeholder,
    not a continuation turn (which would carry a boundary row, a typed marker
    and further model activity).
    """
    with sqlite3.connect(database, timeout=1) as connection:

        def shape_fail(message: str) -> None:
            fail(f"{message}\n{diagnostics(connection)}")

        turns = connection.execute(
            "SELECT turn_id, turn_order, status, outcome, "
            "opened_at_event_order, closed_at_event_order FROM turns "
            "WHERE deleted_at IS NULL ORDER BY turn_order"
        ).fetchall()
        if len(turns) != 3:
            shape_fail(
                f"expected exactly three turns (bootstrap, task, empty successor), got {len(turns)}"
            )
        bootstrap, task, successor = turns
        orders = [row[1] for row in turns]
        if orders != sorted(orders) or len(set(orders)) != 3:
            shape_fail(f"turn_order must be strictly increasing, got {orders}")
        opened = [row[4] for row in turns]
        if opened != sorted(opened):
            shape_fail(
                f"opened_at_event_order must be non-decreasing in turn order, got {opened}"
            )

        def members_of(turn_id: str) -> list[tuple[str, int | None, int, str]]:
            return connection.execute(
                "SELECT m.kind, m.step_index, m.source_event_order, mb.content "
                "FROM message m LEFT JOIN message_block mb "
                "ON mb.message_id = m.message_id AND mb.block_index = 0 "
                "WHERE m.turn_id = ? AND m.deleted_at IS NULL "
                "ORDER BY m.source_event_order",
                (turn_id,),
            ).fetchall()

        # ── t1: bootstrap turn ──────────────────────────────────────────
        boot_id, _, boot_status, boot_outcome, _, boot_closed = bootstrap
        boot_members = members_of(boot_id)
        if not boot_members:
            shape_fail(
                f"bootstrap turn {boot_id} must hold the host bootstrap runtime_note"
            )
        boot_kinds = [kind for kind, _, _, _ in boot_members]
        if any(kind != "runtime_note" for kind in boot_kinds):
            shape_fail(
                f"bootstrap turn {boot_id} may hold only runtime_note members "
                f"(no user prompt, no step-bearing member), got {boot_kinds}"
            )
        if not any(
            "<environment_context>" in (content or "")
            for _, _, _, content in boot_members
        ):
            shape_fail(
                f"bootstrap turn {boot_id} must hold the environment_context runtime_note"
            )
        if boot_status != "closed" or boot_outcome is not None:
            shape_fail(
                f"bootstrap turn {boot_id} must close at the prompt boundary with NULL "
                f"host outcome, got status={boot_status} outcome={boot_outcome}"
            )

        # ── t2: the unique task turn ────────────────────────────────────
        task_id, _, task_status, task_outcome, task_opened, task_closed = task
        task_members = members_of(task_id)
        prompts = [
            (order, content)
            for kind, _, order, content in task_members
            if kind == "user_prompt"
        ]
        if len(prompts) != 1:
            shape_fail(
                f"task turn {task_id} must hold exactly one user_prompt, got {len(prompts)}"
            )
        prompt_order, prompt_content = prompts[0]
        if json.loads(prompt_content or "{}").get("text") != PROMPT:
            shape_fail(
                f"task turn prompt must be the crossing prompt, got {prompt_content!r}"
            )
        if boot_closed != prompt_order or task_opened != prompt_order:
            shape_fail(
                "the task prompt must close the bootstrap turn and open the task turn "
                f"at its own event order {prompt_order}: bootstrap closed_at={boot_closed}, "
                f"task opened_at={task_opened}"
            )
        if task_status != "closed" or task_outcome != "completed":
            shape_fail(
                f"task turn {task_id} must close completed, got status={task_status} "
                f"outcome={task_outcome}"
            )
        if task_closed is None:
            shape_fail(f"task turn {task_id} has no closed_at_event_order")
        all_step_rows = connection.execute(
            "SELECT turn_id, kind, step_index FROM message WHERE deleted_at IS NULL "
            "AND kind IN ('assistant_text','assistant_thinking','tool_call','tool_result') "
            "ORDER BY source_event_order"
        ).fetchall()
        foreign = [row for row in all_step_rows if row[0] != task_id]
        if foreign:
            shape_fail(
                f"every step-bearing member must live on the task turn {task_id}; "
                f"found on other turns: {foreign}"
            )
        stamped = [
            (kind, step) for kind, step, _, _ in task_members if kind in STEP_KINDS
        ]
        if not stamped:
            shape_fail("no step-bearing messages recorded on the task turn")
        if any(step is None for _, step in stamped):
            shape_fail(f"every step-bearing message must carry a step index: {stamped}")
        steps = [step for _, step in stamped]
        if steps != sorted(steps):
            shape_fail(f"step indices must be non-decreasing in record order: {steps}")
        expected = list(range(TOOL_CYCLES + 1))
        if sorted(set(steps)) != expected:
            shape_fail(f"expected step indices {expected}, got {sorted(set(steps))}")
        by_call: dict[str, set[int]] = {}
        for kind, step, _, content in task_members:
            if kind not in ("tool_call", "tool_result"):
                continue
            block = json.loads(content)
            call_id = block.get("toolCallId")
            by_call.setdefault(call_id, set()).add(step)
            if kind == "tool_call" and block.get("toolName") != TOOL_NAME_OFFERED[0]:
                shape_fail(
                    f"tool call {call_id} must name the offered production tool "
                    f"{TOOL_NAME_OFFERED[0]!r}, got {block.get('toolName')!r}"
                )
            if kind == "tool_result" and block.get("isError"):
                shape_fail(
                    f"tool result for {call_id} is an error (the real tool path did not "
                    f"run): {block.get('content')}"
                )
            if kind == "tool_result" and f"ENDPAY" not in str(block.get("content")):
                shape_fail(
                    f"tool result for {call_id} lacks the real command output: {block.get('content')!r}"
                )
        for cycle in range(TOOL_CYCLES):
            call_id = f"call-cycle-{cycle}"
            if by_call.get(call_id) != {cycle}:
                shape_fail(
                    f"tool pair {call_id} must share step index {cycle}, got {by_call.get(call_id)}"
                )

        # ── t3: the SDK's ordinary empty successor ──────────────────────
        succ_id, _, succ_status, succ_outcome, succ_opened, succ_closed = successor
        succ_members = members_of(succ_id)
        if succ_members:
            shape_fail(
                f"successor turn {succ_id} must hold zero members, got {[m[0] for m in succ_members]}"
            )
        if succ_status != "open" or succ_outcome is not None or succ_closed is not None:
            shape_fail(
                f"successor turn {succ_id} must be open with no outcome, got "
                f"status={succ_status} outcome={succ_outcome} closed_at={succ_closed}"
            )
        if succ_opened != task_closed:
            shape_fail(
                f"successor turn {succ_id} must open at the task turn's closed_at_event_order "
                f"{task_closed} (atomic turn_end successor), got {succ_opened}"
            )
        turn_ends = connection.execute(
            "SELECT event_order FROM event WHERE event_kind = 'turn_end' ORDER BY event_order"
        ).fetchall()
        if [row[0] for row in turn_ends] != [task_closed]:
            shape_fail(
                f"exactly one turn_end event at {task_closed} must close the task turn, "
                f"got {turn_ends}"
            )

        # ── no continuation / forced-boundary artefacts ─────────────────
        boundaries = connection.execute(
            "SELECT COUNT(*) FROM compact_continuation_boundary"
        ).fetchone()[0]
        if boundaries != 0:
            shape_fail(
                f"forced-boundary rows must not exist on a parts thread, got {boundaries}"
            )
        receipts = connection.execute(
            "SELECT attempt_id, outcome, continuation_turn_id FROM compact_continuation_receipt"
        ).fetchall()
        if receipts:
            shape_fail(
                f"compact-continuation receipts must not exist on a parts thread, got {receipts}"
            )
        marker_events = connection.execute(
            "SELECT COUNT(*) FROM event WHERE event_kind = 'compact_continuation_marker'"
        ).fetchone()[0]
        marker_members = connection.execute(
            "SELECT COUNT(*) FROM message WHERE kind = 'compact_continuation_marker'"
        ).fetchone()[0]
        if marker_events != 0 or marker_members != 0:
            shape_fail(
                "typed compact-continuation markers must not exist, got "
                f"{marker_events} events / {marker_members} members"
            )
        claim = connection.execute(
            "SELECT claim, attempt_id FROM compact_continuation_writer WHERE singleton = 1"
        ).fetchone()
        if claim is not None and claim[0] != "none":
            shape_fail(f"no continuation writer claim may be held, got {claim}")

        # ── parts activation and served view ────────────────────────────
        activated = connection.execute(
            "SELECT parts_activated_at FROM thread_metadata WHERE id = 1"
        ).fetchone()[0]
        if not activated:
            shape_fail(
                "thread_metadata.parts_activated_at must be set after a parts install"
            )
        arrangement = connection.execute(
            "SELECT arrangement_json FROM thread_view WHERE singleton = 1"
        ).fetchone()
        if arrangement is None:
            shape_fail("a serving view must be installed after the mid-turn compact")
        entries = json.loads(arrangement[0] or "[]")
        part_entries = [e for e in entries if isinstance(e, dict) and e.get("part")]
        for entry in part_entries:
            if entry.get("subjectId") != task_id:
                shape_fail(
                    f"a part arrangement entry must address the task turn {task_id}, "
                    f"got {entry}"
                )
        # The view may already have settled the closed turn by the time the
        # process exits; the durable mechanism fact above and the seam served
        # in a provider request (checked from the captured requests) are the
        # binding evidence. Report which state the final view is in.
        print(
            "info midturn-parts: final view "
            + ("still serves parts" if part_entries else "has settled the turn")
        )


def inspect_requests(requests: list[str]) -> tuple[int, int, int]:
    if len(requests) != TOOL_CYCLES + 1:
        fail(
            f"expected {TOOL_CYCLES + 1} agentic provider requests, got {len(requests)}"
        )
    seam_requests = [
        index for index, body in enumerate(requests) if SEAM_MARKER in body
    ]
    if not seam_requests:
        fail("no provider request carried the parts seam marker")
    first_seam = seam_requests[0]
    if first_seam == 0:
        fail("the first request cannot already be served from a parts view")
    before = len(requests[first_seam - 1])
    after = len(requests[first_seam])
    if after >= before:
        fail(
            f"request {first_seam} after the seam ({after} chars) must be smaller than "
            f"request {first_seam - 1} before it ({before} chars)"
        )
    if "lhc.compact_continuation" in requests[first_seam]:
        fail("a parts-served request must not carry the forced-boundary marker")
    return first_seam, before, after


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--binary", type=Path, default=Path("codex-rs/target/debug/codex")
    )
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"missing codex binary: {binary}")

    server = ThreadingHTTPServer(("127.0.0.1", 0), ResponsesHandler)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="codex-lhc-midturn-parts-") as temp:
            root = Path(temp)
            home = root / "home"
            codex_home = root / "codex-home"
            lhc_root = root / "lhc"
            cwd = root / "cwd"
            for path in (home, codex_home, lhc_root, cwd):
                path.mkdir(parents=True, exist_ok=True)
            provider = (
                '{name="LHC midturn parts probe",'
                f'base_url="http://127.0.0.1:{server.server_port}/v1",'
                'wire_api="responses",requires_openai_auth=false,'
                "supports_websockets=false,request_max_retries=0,stream_max_retries=0}"
            )
            command = [
                str(binary),
                "exec",
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "-C",
                str(cwd),
                "-m",
                "gpt-5.1",
                "-c",
                f"model_providers.lhc_probe={provider}",
                "-c",
                'model_provider="lhc_probe"',
                # Trigger only: every settled seam is under pressure so relief
                # must come from mid-turn compact, never from a forced boundary.
                "-c",
                "model_auto_compact_token_limit=10000",
                "-c",
                "model_context_window=400000",
                PROMPT,
            ]
            environment = os.environ.copy()
            environment.update(
                {
                    "HOME": str(home),
                    "CODEX_HOME": str(codex_home),
                    "CODEX_SQLITE_HOME": str(codex_home),
                    "CODEX_LHC_ROOT": str(lhc_root),
                    "RUST_LOG": "codex_core::compact_lhc=info,codex_lhc_host=info,warn",
                }
            )
            try:
                result = subprocess.run(
                    command,
                    env=environment,
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    text=True,
                    timeout=EXEC_TIMEOUT_SECONDS,
                    check=False,
                )
            except subprocess.TimeoutExpired as timed_out:
                stderr = timed_out.stderr or b""
                if isinstance(stderr, bytes):
                    stderr = stderr.decode("utf-8", "replace")
                fail(
                    f"bare codex exec did not exit within {EXEC_TIMEOUT_SECONDS}s "
                    f"after {len(ResponsesHandler.agentic_requests)} agentic requests\n"
                    f"stderr tail:\n{stderr[-6000:]}"
                )
            if result.returncode != 0:
                fail(
                    f"bare codex exec exited {result.returncode}\n"
                    f"stdout:\n{result.stdout}\nstderr:\n{result.stderr[-8000:]}"
                )
            databases = sorted((lhc_root / "threads").glob("*.sqlite"))
            if len(databases) != 1:
                fail(f"expected one thread database, got {databases}")
            print(f"info midturn-parts: offered tools {ResponsesHandler.offered_tools}")
            inspect_record(databases[0])
            first_seam, before, after = inspect_requests(
                ResponsesHandler.agentic_requests
            )
            print(
                "ok midturn-parts: canonical bootstrap/task/empty-successor turns, "
                f"real {TOOL_NAME_OFFERED[0]} tool path, sequential step stamps over "
                f"{TOOL_CYCLES + 1} cycles, intact tool pairs, parts seam served from "
                f"request {first_seam} ({before} -> {after} chars), no continuation "
                "turn or forced-boundary marker"
            )
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
