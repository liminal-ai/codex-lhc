#!/usr/bin/env python3
import argparse
import hashlib
import json
from pathlib import Path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dist", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--upstream-commit", required=True)
    parser.add_argument("--lhc-sdk-commit", required=True)
    parser.add_argument("--run-id", required=True)
    args = parser.parse_args()

    archives = sorted(
        path
        for path in args.dist.iterdir()
        if path.name.startswith(f"codex-lhc-v{args.version}-")
        and path.suffix in {".gz", ".zip"}
    )
    expected = {"linux-x86_64"}
    artifacts = []
    found = set()
    prefix = f"codex-lhc-v{args.version}-"
    for archive in archives:
        platform = archive.name.removeprefix(prefix)
        platform = platform.removesuffix(".tar.gz").removesuffix(".zip")
        found.add(platform)
        artifacts.append(
            {
                "platform": platform,
                "path": archive.name,
                "sha256": sha256(archive),
                "bytes": archive.stat().st_size,
            }
        )
    if found != expected:
        raise SystemExit(f"artifact platforms {sorted(found)} != {sorted(expected)}")

    manifest = {
        "product": "codex-lhc",
        "release": args.version,
        "sourceCommit": args.source_commit,
        "upstreamCodexCommit": args.upstream_commit,
        "lhcSdkCommit": args.lhc_sdk_commit,
        "lhcThreadSchema": 6,
        "buildRunId": args.run_id,
        "captureDefault": "on",
        "artifacts": artifacts,
        "migration": {
            "id": "none",
            "fromSchema": 6,
            "toSchema": 6,
            "rollbackSupported": True,
            "rollbackBoundary": None,
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
