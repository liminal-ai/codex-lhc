#!/usr/bin/env python3
import argparse
import hashlib
import json
import re
from pathlib import Path


SHA_RE = re.compile(r"^[0-9a-f]{40}$")
RUN_ID_RE = re.compile(r"^[1-9][0-9]*$")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def require_sha(name: str, value: str) -> None:
    if not SHA_RE.fullmatch(value):
        raise SystemExit(f"{name} must be a 40-hex SHA, got {value!r}")


def require_run_id(name: str, value: str) -> None:
    if not RUN_ID_RE.fullmatch(value):
        raise SystemExit(f"{name} must be a non-empty decimal run id, got {value!r}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dist", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--upstream-commit", required=True)
    parser.add_argument("--lhc-sdk-commit", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument(
        "--expected-platform",
        action="append",
        dest="expected_platforms",
        required=True,
        help="Repeat for each required platform (e.g. --expected-platform linux-x86_64)",
    )
    args = parser.parse_args()

    require_sha("source-commit", args.source_commit)
    require_sha("upstream-commit", args.upstream_commit)
    require_sha("lhc-sdk-commit", args.lhc_sdk_commit)
    require_run_id("run-id", args.run_id)

    if len(args.expected_platforms) != len(set(args.expected_platforms)):
        raise SystemExit("duplicate --expected-platform entries")

    expected = set(args.expected_platforms)

    archives = sorted(
        path
        for path in args.dist.iterdir()
        if path.name.startswith(f"codex-lhc-v{args.version}-")
        and path.suffix in {".gz", ".zip"}
    )
    artifacts = []
    found = set()
    prefix = f"codex-lhc-v{args.version}-"
    for archive in archives:
        platform = archive.name.removeprefix(prefix)
        platform = platform.removesuffix(".tar.gz").removesuffix(".zip")
        found.add(platform)
        entry = {
            "platform": platform,
            "path": archive.name,
            "sha256": sha256(archive),
            "bytes": archive.stat().st_size,
        }
        artifacts.append(entry)
    if found != expected:
        raise SystemExit(f"artifact platforms {sorted(found)} != {sorted(expected)}")

    manifest = {
        "product": "codex-lhc",
        "release": args.version,
        "sourceCommit": args.source_commit,
        "upstreamCodexCommit": args.upstream_commit,
        "lhcSdkCommit": args.lhc_sdk_commit,
        "lhcThreadSchema": 12,
        "buildRunId": args.run_id,
        "captureDefault": "on",
        "compactAlgorithmDefault": "bounded-selector",
        "compactAlgorithmRollback": {
            "environment": "LHC_COMPACT_ALGORITHM",
            "value": "legacy",
        },
        "artifacts": artifacts,
        "migration": {
            "id": "thread-schema-11-to-12",
            "fromSchema": 11,
            "toSchema": 12,
            "rollbackSupported": False,
            "rollbackBoundary": f"Opening a schema-v11 thread with v{args.version} migrates it to schema 12; downgrade to v0.149.0 is unsupported.",
        },
    }
    manifest_path = args.dist / "release-manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

    checksum_paths = sorted(
        path
        for path in args.dist.iterdir()
        if path.is_file() and path.name != "SHA256SUMS"
    )
    (args.dist / "SHA256SUMS").write_text(
        "".join(f"{sha256(path)}  {path.name}\n" for path in checksum_paths),
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
