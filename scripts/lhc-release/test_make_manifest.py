#!/usr/bin/env python3
"""Platform-set tests for make_manifest.py."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/lhc-release/make_manifest.py"


def write_archive(dist: Path, version: str, platform: str, kind: str) -> Path:
    if kind == "tar":
        path = dist / f"codex-lhc-v{version}-{platform}.tar.gz"
    else:
        path = dist / f"codex-lhc-v{version}-{platform}.zip"
    path.write_bytes(platform.encode("utf-8") + b"\n")
    return path


def run_manifest(dist: Path, version: str, platforms: list[str], extra: list[str] | None = None) -> subprocess.CompletedProcess[str]:
    cmd = [
        sys.executable,
        str(SCRIPT),
        "--dist",
        str(dist),
        "--version",
        version,
        "--source-commit",
        "a" * 40,
        "--upstream-commit",
        "b" * 40,
        "--lhc-sdk-commit",
        "c" * 40,
        "--run-id",
        "linux-run",
    ]
    for platform in platforms:
        cmd.extend(["--expected-platform", platform])
    if extra:
        cmd.extend(extra)
    return subprocess.run(cmd, text=True, capture_output=True, check=False)


class PlatformSetTests(unittest.TestCase):
    def test_single_linux_platform(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, "0.2.2", "linux-x86_64", "tar")
            result = run_manifest(dist, "0.2.2", ["linux-x86_64"])
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = json.loads((dist / "release-manifest.json").read_text())
            self.assertEqual([item["platform"] for item in manifest["artifacts"]], ["linux-x86_64"])
            self.assertEqual(manifest["buildRunId"], "linux-run")
            self.assertNotIn("supplementalRunId", manifest)
            self.assertNotIn("buildRunId", manifest["artifacts"][0])

    def test_three_platforms_with_per_artifact_runs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, "0.2.2", "linux-x86_64", "tar")
            write_archive(dist, "0.2.2", "windows-x86_64", "zip")
            write_archive(dist, "0.2.2", "macos-aarch64", "tar")
            result = run_manifest(
                dist,
                "0.2.2",
                ["linux-x86_64", "windows-x86_64", "macos-aarch64"],
                extra=[
                    "--supplemental-run-id",
                    "supp-run",
                    "--artifact-run",
                    "linux-x86_64:31970651653",
                    "--artifact-run",
                    "windows-x86_64:SUPPLEMENTAL_RUN",
                    "--artifact-run",
                    "macos-aarch64:SUPPLEMENTAL_RUN",
                ],
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = json.loads((dist / "release-manifest.json").read_text())
            self.assertEqual(manifest["buildRunId"], "linux-run")
            self.assertEqual(manifest["supplementalRunId"], "supp-run")
            by_platform = {item["platform"]: item for item in manifest["artifacts"]}
            self.assertEqual(set(by_platform), {"linux-x86_64", "windows-x86_64", "macos-aarch64"})
            self.assertEqual(by_platform["linux-x86_64"]["buildRunId"], "31970651653")
            self.assertEqual(by_platform["windows-x86_64"]["buildRunId"], "SUPPLEMENTAL_RUN")
            self.assertEqual(by_platform["macos-aarch64"]["buildRunId"], "SUPPLEMENTAL_RUN")

    def test_missing_platform_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, "0.2.2", "linux-x86_64", "tar")
            result = run_manifest(
                dist,
                "0.2.2",
                ["linux-x86_64", "windows-x86_64", "macos-aarch64"],
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("artifact platforms", result.stderr)


if __name__ == "__main__":
    unittest.main()
