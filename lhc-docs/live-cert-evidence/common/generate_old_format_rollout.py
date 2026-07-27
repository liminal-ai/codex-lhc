#!/usr/bin/env python3
"""Generate an old-shape (appended Compacted) rollout for dual-format live cert.

Mirrors the committed dual-format fixture in:
  codex-rs/core/src/compact_lhc_slice_d_tests.rs
    slice_d_dual_format_fixture_file_round_trip
    slice_d_dual_format_old_appended_compacted_reconstructs

Old shape: session_meta → pre-compact items → Compacted1 → tail1 → Compacted2 → tail2
(no rewrite; multiple Compacted records retained in one file).
"""

from __future__ import annotations

import argparse
import json
import uuid
from datetime import datetime, timezone
from pathlib import Path


def now_ts() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def line(item_type: str, payload: dict, ts: str | None = None) -> dict:
    return {"timestamp": ts or now_ts(), "type": item_type, "payload": payload}


def user_msg(text: str) -> dict:
    return {
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    }


def assistant_msg(text: str) -> dict:
    return {
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text}],
    }


def compacted(
    message: str,
    bands: list,
    window_number: int,
    window_id: str,
    previous_window_id: str | None,
    first_window_id: str,
) -> dict:
    payload = {
        "message": message,
        "replacement_history": bands,
        "window_number": window_number,
        "window_id": window_id,
        "first_window_id": first_window_id,
    }
    if previous_window_id is not None:
        payload["previous_window_id"] = previous_window_id
    return payload


def generate(session_id: str, cwd: str) -> list[dict]:
    first_win = str(uuid.uuid4())
    win1 = str(uuid.uuid4())
    win2 = str(uuid.uuid4())
    ts = now_ts()

    bands1 = [user_msg("old-band-v1"), assistant_msg("old-sum-v1")]
    bands2 = [user_msg("old-band-v2 LIVE-CERT-OLD-FORMAT"), assistant_msg("old-sum-v2")]

    lines: list[dict] = []
    lines.append(
        line(
            "session_meta",
            {
                "id": session_id,
                "session_id": session_id,
                "timestamp": ts,
                "cwd": cwd,
                "originator": "codex_exec",
                "cli_version": "live-cert-old-format",
                "source": "exec",
                "model_provider": "openai",
            },
            ts,
        )
    )
    # Pre-compact content (must NOT leak into dual-format resume history)
    lines.append(line("response_item", user_msg("pre-compact-user SECRET-SHOULD-NOT-RESUME")))
    lines.append(line("response_item", assistant_msg("pre-compact-asst")))
    # First Compacted (older generation)
    lines.append(
        line(
            "compacted",
            compacted("legacy compact 1", bands1, 1, win1, None, first_win),
        )
    )
    lines.append(line("response_item", user_msg("after-c1 post-first-compact")))
    lines.append(line("response_item", assistant_msg("reply-c1")))
    # Second Compacted (newest boundary — dual-format resume starts here)
    lines.append(
        line(
            "compacted",
            compacted("legacy compact 2", bands2, 2, win2, win1, first_win),
        )
    )
    lines.append(
        line(
            "response_item",
            user_msg(
                "post-2-true-tail Remember the live-cert marker phrase: "
                "OLD-FORMAT-ANCHOR-77 and the band marker LIVE-CERT-OLD-FORMAT"
            ),
        )
    )
    lines.append(
        line(
            "response_item",
            assistant_msg(
                "Acknowledged OLD-FORMAT-ANCHOR-77 and LIVE-CERT-OLD-FORMAT. Ready to continue."
            ),
        )
    )
    # Event that makes the session discoverable / resumable
    lines.append(
        line(
            "event_msg",
            {
                "type": "user_message",
                "message": "post-2-true-tail Remember the live-cert marker phrase: OLD-FORMAT-ANCHOR-77",
                "images": [],
            },
        )
    )
    return lines


def install(session_id: str, cwd: str, codex_home: Path) -> Path:
    lines = generate(session_id, cwd)
    # sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl
    now = datetime.now(timezone.utc)
    day_dir = codex_home / "sessions" / f"{now:%Y}" / f"{now:%m}" / f"{now:%d}"
    day_dir.mkdir(parents=True, exist_ok=True)
    ts_slug = now.strftime("%Y-%m-%dT%H-%M-%S")
    path = day_dir / f"rollout-{ts_slug}-{session_id}.jsonl"
    with path.open("w") as f:
        for obj in lines:
            f.write(json.dumps(obj, ensure_ascii=False) + "\n")
    return path


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--session-id", default=str(uuid.uuid4()))
    ap.add_argument("--cwd", required=True)
    ap.add_argument("--codex-home", required=True)
    ap.add_argument("--print-only", action="store_true")
    args = ap.parse_args()
    if args.print_only:
        for obj in generate(args.session_id, args.cwd):
            print(json.dumps(obj))
        return
    path = install(args.session_id, args.cwd, Path(args.codex_home))
    print(path)
    print(args.session_id)


if __name__ == "__main__":
    main()
