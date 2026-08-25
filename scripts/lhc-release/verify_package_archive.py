#!/usr/bin/env python3
"""Verify the Codex-LHC extension of the canonical Codex package contract."""

from __future__ import annotations

import argparse
import io
import json
import tarfile
import zipfile
from pathlib import Path


PLATFORMS = {
    "linux-x86_64": ("x86_64-unknown-linux-musl", ""),
    "linux-aarch64": ("aarch64-unknown-linux-musl", ""),
    "windows-x86_64": ("x86_64-pc-windows-msvc", ".exe"),
    "windows-aarch64": ("aarch64-pc-windows-msvc", ".exe"),
    "macos-aarch64": ("aarch64-apple-darwin", ""),
}


def archive_files(path: Path) -> tuple[set[str], bytes]:
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            names = {
                name.rstrip("/") for name in archive.namelist() if name.rstrip("/")
            }
            return names, archive.read("codex-package.json")
    with tarfile.open(path, "r:gz") as archive:
        names = {
            member.name.rstrip("/")
            for member in archive.getmembers()
            if member.name.rstrip("/")
        }
        member = archive.getmember("codex-package.json")
        stream = archive.extractfile(member)
        if stream is None:
            raise ValueError("codex-package.json is not a regular file")
        return names, stream.read()


def verify(path: Path, platform: str, version: str, sdk_commit: str) -> dict:
    if platform not in PLATFORMS:
        raise ValueError(f"unsupported release platform: {platform}")
    target, suffix = PLATFORMS[platform]
    names, metadata_bytes = archive_files(path)
    metadata = json.load(io.BytesIO(metadata_bytes))
    expected_metadata = {
        "layoutVersion": 1,
        "version": version,
        "target": target,
        "variant": "codex",
        "entrypoint": f"bin/codex{suffix}",
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
        "lhc": {
            "repository": "https://github.com/liminal-ai/long-horizon-context",
            "sdkCommit": sdk_commit,
            "threadSchema": 12,
        },
    }
    if metadata != expected_metadata:
        raise ValueError(f"unexpected codex-package.json: {metadata!r}")

    required = {
        "codex-package.json",
        f"bin/codex{suffix}",
        f"bin/codex-code-mode-host{suffix}",
        f"codex-path/rg{suffix}",
    }
    if platform.startswith("linux-"):
        required |= {"codex-resources/bwrap", "codex-resources/zsh/bin/zsh"}
    if platform.startswith("windows-"):
        required |= {
            "codex-resources/codex-command-runner.exe",
            "codex-resources/codex-windows-sandbox-setup.exe",
        }
    missing = required - names
    if missing:
        raise ValueError(f"missing canonical package files: {sorted(missing)}")
    forbidden = {
        name
        for name in names
        if "codex-app-server" in name or "codex-responses-api-proxy" in name
    }
    if forbidden:
        raise ValueError(f"unexpected release companions: {sorted(forbidden)}")
    return metadata


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--platform", choices=sorted(PLATFORMS), required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--lhc-sdk-commit", required=True)
    args = parser.parse_args()
    verify(args.archive, args.platform, args.version, args.lhc_sdk_commit)
    print(f"verified canonical Codex-LHC package: {args.platform}")


if __name__ == "__main__":
    main()
