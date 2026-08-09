import os
import sys
from pathlib import Path


def fail(message: str) -> None:
    print(f"FAIL: {message}", file=sys.stderr)
    raise SystemExit(1)


def require_candidate() -> Path:
    candidate = os.environ.get("CODEX_LHC_CANDIDATE_DIR")
    if not candidate:
        fail("CODEX_LHC_CANDIDATE_DIR is not set")
    path = Path(candidate).resolve()
    if not path.is_dir():
        fail(f"candidate directory does not exist: {path}")
    return path


def require_key() -> None:
    if not os.environ.get("DAYTONA_API_KEY"):
        fail("DAYTONA_API_KEY is not set")


def expect_success(result: object, operation: str) -> str:
    exit_code = getattr(result, "exit_code", None)
    output = getattr(result, "result", "") or ""
    print(output)
    if exit_code != 0:
        fail(f"{operation} exited {exit_code}")
    return output
