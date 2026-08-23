#!/usr/bin/env python3
"""Unauthenticated readback for an exact public Codex-LHC release."""

from __future__ import annotations

import argparse
import hashlib
import json
import urllib.request
from pathlib import Path


def expected_asset_names(version: str, *, resumed: bool = False) -> set[str]:
    names = {
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
    if resumed:
        names |= {
            "RELEASE_NOTES.md",
            "codex-lhc-linux-x86_64-qualification-evidence.zip",
            "fanout-resume-receipt.json",
        }
    return names


def validate_public_state(
    *,
    tag_sha: str,
    release: dict,
    version: str,
    product_source: str,
    workflow_source: str,
) -> dict[str, str]:
    if tag_sha != product_source:
        raise ValueError(f"public tag target {tag_sha} != {product_source}")
    if (
        release.get("tag_name") != f"v{version}"
        or release.get("draft") is not False
        or release.get("prerelease") is not False
    ):
        raise ValueError(
            "public release identity is missing, mismatched, or still draft"
        )
    body = release.get("body") or ""
    for label, value in (
        ("Product source", product_source),
        ("Workflow source", workflow_source),
    ):
        if f"- {label}: {value}" not in body:
            raise ValueError(f"public release body does not disclose {label}")

    assets = release.get("assets") or []
    urls = {asset.get("name"): asset.get("browser_download_url") for asset in assets}
    expected = expected_asset_names(version, resumed=workflow_source != product_source)
    if set(urls) != expected or any(not url for url in urls.values()):
        raise ValueError(f"public assets {sorted(urls)} != expected {sorted(expected)}")
    return urls


def verify_downloaded_assets(
    candidate_dir: Path,
    downloaded: dict[str, bytes],
    *,
    product_source: str,
    workflow_source: str,
) -> None:
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

    manifest = json.loads(downloaded["release-manifest.json"])
    if manifest.get("sourceCommit") != product_source:
        raise ValueError("public manifest sourceCommit is not product source")
    if manifest.get("productSource", manifest.get("sourceCommit")) != product_source:
        raise ValueError("public manifest productSource mismatch")
    if workflow_source != product_source:
        if manifest.get("workflowSource") != workflow_source:
            raise ValueError("public manifest workflowSource mismatch")
    elif manifest.get("workflowSource", product_source) != product_source:
        raise ValueError("single-source public manifest has mixed workflowSource")


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
    parser.add_argument("--product-source", required=True)
    parser.add_argument("--workflow-source", required=True)
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
        product_source=args.product_source,
        workflow_source=args.workflow_source,
    )
    downloaded = {name: fetch_bytes(url) for name, url in urls.items()}
    verify_downloaded_assets(
        args.candidate_dir,
        downloaded,
        product_source=args.product_source,
        workflow_source=args.workflow_source,
    )
    print(
        f"Public v{args.version} tag, release, and all expected assets "
        f"match product {args.product_source} via workflow {args.workflow_source}."
    )


if __name__ == "__main__":
    main()
