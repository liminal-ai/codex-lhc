#!/usr/bin/env python3
"""Unauthenticated readback for an exact public Codex-LHC release."""

from __future__ import annotations

import argparse
import hashlib
import json
import urllib.request
from pathlib import Path


def expected_asset_names(version: str) -> set[str]:
    return {
        f"codex-lhc-v{version}-linux-x86_64.tar.gz",
        f"codex-lhc-v{version}-linux-aarch64.tar.gz",
        f"codex-lhc-v{version}-windows-x86_64.zip",
        f"codex-lhc-v{version}-windows-aarch64.zip",
        f"codex-lhc-v{version}-macos-aarch64.tar.gz",
        "install.sh",
        "install.ps1",
        "release-manifest.json",
        "SHA256SUMS",
    }


def validate_public_state(
    *, tag_sha: str, release: dict, version: str, source_sha: str
) -> dict[str, str]:
    if tag_sha != source_sha:
        raise ValueError(f"public tag target {tag_sha} != {source_sha}")
    if (
        release.get("tag_name") != f"v{version}"
        or release.get("draft") is not False
        or release.get("prerelease") is not False
    ):
        raise ValueError(
            "public release identity is missing, mismatched, or still draft"
        )
    assets = release.get("assets") or []
    urls = {asset.get("name"): asset.get("browser_download_url") for asset in assets}
    expected = expected_asset_names(version)
    if set(urls) != expected or any(not url for url in urls.values()):
        raise ValueError(f"public assets {sorted(urls)} != expected {sorted(expected)}")
    return urls


def verify_downloaded_assets(candidate_dir: Path, downloaded: dict[str, bytes]) -> None:
    for name, body in downloaded.items():
        candidate = candidate_dir / name
        if not candidate.is_file() or candidate.read_bytes() != body:
            raise ValueError(f"public asset does not match candidate bytes: {name}")
    checksums = downloaded["SHA256SUMS"].decode("utf-8")
    for line in checksums.splitlines():
        digest, name = line.split(None, 1)
        name = name.strip()
        actual = hashlib.sha256(downloaded[name]).hexdigest()
        if actual != digest:
            raise ValueError(f"public checksum mismatch for {name}")


def fetch_json(url: str) -> dict:
    with urllib.request.urlopen(url) as response:  # noqa: S310 - fixed GitHub API URL
        return json.load(response)


def fetch_bytes(url: str) -> bytes:
    with urllib.request.urlopen(url) as response:  # noqa: S310 - public asset URL from GitHub API
        return response.read()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--candidate-dir", type=Path, required=True)
    args = parser.parse_args()

    api = f"https://api.github.com/repos/{args.repository}"
    ref = fetch_json(f"{api}/git/ref/tags/v{args.version}")
    tag_sha = ref["object"]["sha"]
    if ref["object"]["type"] == "tag":
        tag_sha = fetch_json(f"{api}/git/tags/{tag_sha}")["object"]["sha"]
    release = fetch_json(f"{api}/releases/tags/v{args.version}")
    urls = validate_public_state(
        tag_sha=tag_sha,
        release=release,
        version=args.version,
        source_sha=args.source_sha,
    )
    downloaded = {name: fetch_bytes(url) for name, url in urls.items()}
    verify_downloaded_assets(args.candidate_dir, downloaded)
    print(
        f"Public v{args.version} tag, release, and all expected assets "
        f"match candidate {args.source_sha}."
    )


if __name__ == "__main__":
    main()
