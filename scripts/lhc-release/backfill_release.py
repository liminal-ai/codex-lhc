#!/usr/bin/env python3
"""Upload Windows/macOS archives onto an existing Linux-only GitHub release.

Does not retarget the tag and does not replace the Linux archive bytes.
Requires GH_TOKEN.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path


LINUX_NAME = "codex-lhc-v{version}-linux-x86_64.tar.gz"
WINDOWS_NAME = "codex-lhc-v{version}-windows-x86_64.zip"
MACOS_NAME = "codex-lhc-v{version}-macos-aarch64.tar.gz"
REPLACE_NAMES = ("install.sh", "install.ps1", "release-manifest.json", "SHA256SUMS")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def gh(repo: str, *args: str, capture: bool = True) -> str:
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not token:
        raise SystemExit("GH_TOKEN is required")
    env = os.environ.copy()
    env["GH_TOKEN"] = token
    cmd = ["gh", "-R", repo, *args]
    result = subprocess.run(cmd, env=env, text=True, capture_output=capture, check=False)
    if result.returncode != 0:
        err = (result.stderr or result.stdout or "").strip()
        raise SystemExit(f"gh {' '.join(args)} failed: {err}")
    return (result.stdout or "").strip()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--combined-dir", type=Path, required=True)
    parser.add_argument("--repo", required=True)
    args = parser.parse_args()
    version = args.version.lstrip("v")
    tag = f"v{version}"
    combined = args.combined_dir.resolve()
    if not combined.is_dir():
        raise SystemExit(f"combined dir missing: {combined}")

    windows = combined / WINDOWS_NAME.format(version=version)
    macos = combined / MACOS_NAME.format(version=version)
    linux = combined / LINUX_NAME.format(version=version)
    for path in (windows, macos, *[(combined / name) for name in REPLACE_NAMES]):
        if not path.is_file():
            raise SystemExit(f"required file missing: {path}")

    release = json.loads(gh(args.repo, "release", "view", tag, "--json", "tagName,targetCommitish,body,assets"))
    old_digests = {asset["name"]: asset.get("digest") or "" for asset in release.get("assets", [])}

    if linux.is_file() and LINUX_NAME.format(version=version) in old_digests:
        public = json.loads(
            gh(
                args.repo,
                "api",
                f"repos/{args.repo}/releases/tags/{tag}",
            )
        )
        linux_asset = next(
            (asset for asset in public.get("assets", []) if asset["name"] == linux.name),
            None,
        )
        if linux_asset is None:
            raise SystemExit("existing release is missing the Linux archive")

    local_linux = sha256(linux) if linux.is_file() else None

    upload = [str(windows), str(macos)]
    for name in REPLACE_NAMES:
        upload.append(str(combined / name))
    gh(args.repo, "release", "upload", tag, *upload, "--clobber")

    note = (
        f"{release.get('body') or ''}".rstrip()
        + "\n\n---\n\nCross-platform backfill: added Windows x86_64 and macOS aarch64 "
        "archives plus updated installers/manifest/SHA256SUMS. "
        "Tag target and Linux archive bytes were not changed.\n"
    )
    gh(args.repo, "release", "edit", tag, "--notes", note)

    refreshed = json.loads(
        gh(args.repo, "release", "view", tag, "--json", "tagName,targetCommitish,assets")
    )
    if refreshed["tagName"] != tag:
        raise SystemExit("tag name changed")
    if refreshed["targetCommitish"] != release["targetCommitish"]:
        raise SystemExit(
            f"tag/release target changed: {release['targetCommitish']} -> {refreshed['targetCommitish']}"
        )

    by_name = {asset["name"]: asset for asset in refreshed.get("assets", [])}
    expected_local = {
        windows.name: sha256(windows),
        macos.name: sha256(macos),
    }
    for name in REPLACE_NAMES:
        expected_local[(combined / name).name] = sha256(combined / name)
    if local_linux is not None:
        expected_local[linux.name] = local_linux

    for name, digest in expected_local.items():
        asset = by_name.get(name)
        if asset is None:
            raise SystemExit(f"public release is missing {name}")
        public_digest = (asset.get("digest") or "").removeprefix("sha256:")
        if not public_digest:
            # gh release view may omit digest; fall back to API download URL hash via gh api
            api_asset = json.loads(
                gh(args.repo, "api", f"repos/{args.repo}/releases/assets/{asset['id']}")
            )
            public_digest = (api_asset.get("digest") or "").removeprefix("sha256:")
        if public_digest and public_digest != digest:
            raise SystemExit(f"public digest mismatch for {name}: {public_digest} != {digest}")
        if name == linux.name and old_digests.get(name):
            old = old_digests[name].removeprefix("sha256:")
            if old and public_digest and old != public_digest:
                raise SystemExit(f"Linux archive bytes changed: {old} -> {public_digest}")

    print(f"backfill {tag}: uploaded {windows.name} and {macos.name}; Linux/tag unchanged")


if __name__ == "__main__":
    main()
