#!/usr/bin/env python3
"""Upload Windows/macOS archives onto an existing Linux-only GitHub release.

Does not retarget the tag and does not replace the Linux archive bytes.
Requires GH_TOKEN when using the live `gh` client.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable


LINUX_NAME = "codex-lhc-v{version}-linux-x86_64.tar.gz"
WINDOWS_NAME = "codex-lhc-v{version}-windows-x86_64.zip"
MACOS_NAME = "codex-lhc-v{version}-macos-aarch64.tar.gz"
REPLACE_NAMES = ("install.sh", "install.ps1", "release-manifest.json", "SHA256SUMS")
BACKFILL_MARKER = "<!-- cross-platform-backfill -->"
BACKFILL_NOTE = (
    "Cross-platform backfill: added Windows x86_64 and macOS aarch64 "
    "archives plus updated installers/manifest/SHA256SUMS. "
    "Tag target and Linux archive bytes were not changed."
)


class BackfillError(SystemExit):
    pass


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def default_gh(repo: str, *args: str) -> str:
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not token:
        raise BackfillError("GH_TOKEN is required")
    env = os.environ.copy()
    env["GH_TOKEN"] = token
    result = subprocess.run(
        ["gh", "-R", repo, *args],
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        err = (result.stderr or result.stdout or "").strip()
        raise BackfillError(f"gh {' '.join(args)} failed: {err}")
    return (result.stdout or "").strip()


def default_download(repo: str, url: str, dest: Path, gh: Callable[..., str]) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not token:
        raise BackfillError("GH_TOKEN is required")
    result = subprocess.run(
        [
            "gh",
            "api",
            "-H",
            "Accept: application/octet-stream",
            url,
        ],
        env={**os.environ, "GH_TOKEN": token},
        stdout=dest.open("wb"),
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != 0:
        err = (result.stderr or b"").decode("utf-8", "replace").strip()
        raise BackfillError(f"download {url} failed: {err}")


def resolve_tag_commit(repo: str, tag: str, gh: Callable[..., str]) -> str:
    ref = json.loads(gh(repo, "api", f"repos/{repo}/git/refs/tags/{tag}"))
    sha = ref["object"]["sha"]
    if ref["object"]["type"] == "tag":
        peeled = json.loads(gh(repo, "api", f"repos/{repo}/git/tags/{sha}"))
        sha = peeled["object"]["sha"]
    return sha


def merge_backfill_note(body: str | None) -> str:
    current = (body or "").rstrip()
    if BACKFILL_MARKER in current:
        return current + "\n"
    addition = f"\n\n---\n\n{BACKFILL_MARKER}\n{BACKFILL_NOTE}\n"
    return (current + addition) if current else addition.lstrip()


def asset_record(asset: dict, digest: str) -> dict:
    return {
        "name": asset.get("name"),
        "id": asset.get("id"),
        "size": asset.get("size"),
        "sha256": digest,
    }


def run_backfill(
    *,
    version: str,
    combined_dir: Path,
    repo: str,
    receipt_path: Path | None = None,
    gh: Callable[..., str] = default_gh,
    download: Callable[..., None] | None = None,
    now: Callable[[], datetime] | None = None,
) -> dict:
    download_fn = download or (lambda r, url, dest: default_download(r, url, dest, gh))
    clock = now or (lambda: datetime.now(timezone.utc))
    version = version.lstrip("v")
    tag = f"v{version}"
    combined = combined_dir.resolve()
    if not combined.is_dir():
        raise BackfillError(f"combined dir missing: {combined}")

    windows = combined / WINDOWS_NAME.format(version=version)
    macos = combined / MACOS_NAME.format(version=version)
    linux = combined / LINUX_NAME.format(version=version)
    required = [linux, windows, macos, *[(combined / name) for name in REPLACE_NAMES]]
    for path in required:
        if not path.is_file():
            raise BackfillError(f"required file missing: {path}")

    manifest = json.loads((combined / "release-manifest.json").read_text(encoding="utf-8"))
    source = manifest.get("sourceCommit")
    if not source:
        raise BackfillError("combined manifest is missing sourceCommit")

    release = json.loads(
        gh(repo, "release", "view", tag, "--json", "tagName,targetCommitish,body,assets")
    )
    tag_sha = resolve_tag_commit(repo, tag, gh)
    release_target = release["targetCommitish"]
    if tag_sha != release_target or release_target != source:
        raise BackfillError(
            f"tag/release/manifest identity mismatch: tag={tag_sha} "
            f"release={release_target} manifest={source}"
        )

    scratch = combined / ".backfill-scratch"
    before_dir = scratch / "before"
    after_dir = scratch / "after"
    before_dir.mkdir(parents=True, exist_ok=True)
    after_dir.mkdir(parents=True, exist_ok=True)

    old_assets = []
    public_linux = None
    for asset in release.get("assets") or []:
        dest = before_dir / asset["name"]
        url = asset.get("url") or f"repos/{repo}/releases/assets/{asset['id']}"
        download_fn(repo, url, dest)
        digest = sha256(dest)
        old_assets.append(asset_record(asset, digest))
        if asset["name"] == linux.name:
            public_linux = dest

    if public_linux is None:
        raise BackfillError("existing release is missing the Linux archive")
    public_linux_hash = sha256(public_linux)
    local_linux_hash = sha256(linux)
    if public_linux_hash != local_linux_hash:
        raise BackfillError(
            f"preexisting Linux hash mismatch: public={public_linux_hash} local={local_linux_hash}"
        )

    upload = [str(windows), str(macos)]
    for name in REPLACE_NAMES:
        upload.append(str(combined / name))
    gh(repo, "release", "upload", tag, *upload, "--clobber")
    gh(repo, "release", "edit", tag, "--notes", merge_backfill_note(release.get("body")))

    refreshed = json.loads(
        gh(repo, "release", "view", tag, "--json", "tagName,targetCommitish,assets")
    )
    if refreshed["tagName"] != tag:
        raise BackfillError("tag name changed")
    if refreshed["targetCommitish"] != release_target:
        raise BackfillError(
            f"tag/release target changed: {release_target} -> {refreshed['targetCommitish']}"
        )

    expected_final = {
        linux.name: local_linux_hash,
        windows.name: sha256(windows),
        macos.name: sha256(macos),
    }
    for name in REPLACE_NAMES:
        expected_final[name] = sha256(combined / name)

    new_assets = []
    by_name = {asset["name"]: asset for asset in refreshed.get("assets") or []}
    for name, expected in expected_final.items():
        asset = by_name.get(name)
        if asset is None:
            raise BackfillError(f"public release is missing {name}")
        dest = after_dir / name
        url = asset.get("url") or f"repos/{repo}/releases/assets/{asset['id']}"
        download_fn(repo, url, dest)
        actual = sha256(dest)
        if actual != expected:
            raise BackfillError(f"final download mismatch for {name}: {actual} != {expected}")
        new_assets.append(asset_record(asset, actual))

    receipt = {
        "tag": tag,
        "targetCommitish": release_target,
        "timestamp": clock().strftime("%Y-%m-%dT%H:%M:%SZ"),
        "combinedManifest": {
            "release": manifest.get("release"),
            "sourceCommit": source,
            "upstreamCodexCommit": manifest.get("upstreamCodexCommit"),
            "lhcSdkCommit": manifest.get("lhcSdkCommit"),
            "buildRunId": manifest.get("buildRunId"),
            "supplementalRunId": manifest.get("supplementalRunId"),
        },
        "oldAssets": old_assets,
        "newAssets": new_assets,
        "linuxUnchanged": True,
        "linuxSha256": local_linux_hash,
    }
    out = receipt_path or (combined / "backfill-receipt.json")
    out.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    print(f"backfill {tag}: uploaded {windows.name} and {macos.name}; Linux/tag unchanged")
    return receipt


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--combined-dir", type=Path, required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--receipt", type=Path, default=None)
    args = parser.parse_args()
    run_backfill(
        version=args.version,
        combined_dir=args.combined_dir,
        repo=args.repo,
        receipt_path=args.receipt,
    )


if __name__ == "__main__":
    main()
