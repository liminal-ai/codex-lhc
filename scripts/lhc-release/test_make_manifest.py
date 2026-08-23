#!/usr/bin/env python3
"""Platform-set and validation tests for make_manifest.py."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/lhc-release/make_manifest.py"
SHA_A = "a" * 40
SHA_B = "b" * 40
SHA_C = "c" * 40
ALIGNED_VERSION = "0.149.0"


def write_archive(dist: Path, version: str, platform: str, kind: str) -> Path:
    if kind == "tar":
        path = dist / f"codex-lhc-v{version}-{platform}.tar.gz"
    else:
        path = dist / f"codex-lhc-v{version}-{platform}.zip"
    path.write_bytes(platform.encode("utf-8") + b"\n")
    return path


def run_manifest(
    dist: Path,
    version: str,
    platforms: list[str],
    extra: list[str] | None = None,
    *,
    source: str = SHA_A,
    upstream: str = SHA_B,
    sdk: str = SHA_C,
    run_id: str = "31970651653",
    workflow_source: str | None = None,
    resume_receipt: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    cmd = [
        sys.executable,
        str(SCRIPT),
        "--dist",
        str(dist),
        "--version",
        version,
        "--source-commit",
        source,
        "--upstream-commit",
        upstream,
        "--lhc-sdk-commit",
        sdk,
        "--run-id",
        run_id,
    ]
    if workflow_source is not None:
        cmd.extend(["--workflow-source", workflow_source])
    if resume_receipt is not None:
        cmd.extend(["--resume-receipt", str(resume_receipt)])
    for platform in platforms:
        cmd.extend(["--expected-platform", platform])
    if extra:
        cmd.extend(extra)
    return subprocess.run(cmd, text=True, capture_output=True, check=False)


class PlatformSetTests(unittest.TestCase):
    def test_single_linux_platform(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, ALIGNED_VERSION, "linux-x86_64", "tar")
            result = run_manifest(dist, ALIGNED_VERSION, ["linux-x86_64"])
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = json.loads((dist / "release-manifest.json").read_text())
            self.assertEqual(
                [item["platform"] for item in manifest["artifacts"]], ["linux-x86_64"]
            )
            self.assertEqual(manifest["buildRunId"], "31970651653")
            self.assertEqual(manifest["productSource"], SHA_A)
            self.assertNotIn("supplementalRunId", manifest)
            self.assertEqual(manifest["compactAlgorithmDefault"], "metadata-first")
            self.assertEqual(
                manifest["compactAlgorithmRollback"],
                {"environment": "LHC_COMPACT_ALGORITHM", "value": "legacy"},
            )
            self.assertNotIn("buildRunId", manifest["artifacts"][0])

    def test_five_platforms_from_one_exact_build_run(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, ALIGNED_VERSION, "linux-x86_64", "tar")
            write_archive(dist, ALIGNED_VERSION, "linux-aarch64", "tar")
            write_archive(dist, ALIGNED_VERSION, "windows-x86_64", "zip")
            write_archive(dist, ALIGNED_VERSION, "windows-aarch64", "zip")
            write_archive(dist, ALIGNED_VERSION, "macos-aarch64", "tar")
            result = run_manifest(
                dist,
                ALIGNED_VERSION,
                [
                    "linux-x86_64",
                    "linux-aarch64",
                    "windows-x86_64",
                    "windows-aarch64",
                    "macos-aarch64",
                ],
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = json.loads((dist / "release-manifest.json").read_text())
            self.assertEqual(manifest["buildRunId"], "31970651653")
            self.assertNotIn("supplementalRunId", manifest)
            self.assertEqual(
                [item["platform"] for item in manifest["artifacts"]],
                [
                    "linux-aarch64",
                    "linux-x86_64",
                    "macos-aarch64",
                    "windows-aarch64",
                    "windows-x86_64",
                ],
            )

    def test_missing_platform_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, ALIGNED_VERSION, "linux-x86_64", "tar")
            result = run_manifest(
                dist,
                ALIGNED_VERSION,
                [
                    "linux-x86_64",
                    "linux-aarch64",
                    "windows-x86_64",
                    "windows-aarch64",
                    "macos-aarch64",
                ],
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("artifact platforms", result.stderr)

    def test_duplicate_expected_platform_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, ALIGNED_VERSION, "linux-x86_64", "tar")
            result = run_manifest(
                dist, ALIGNED_VERSION, ["linux-x86_64", "linux-x86_64"]
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("duplicate --expected-platform", result.stderr)

    def test_rejects_non_hex_source(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, ALIGNED_VERSION, "linux-x86_64", "tar")
            result = run_manifest(
                dist, ALIGNED_VERSION, ["linux-x86_64"], source="not-a-sha"
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("40-hex SHA", result.stderr)

    def test_rejects_non_decimal_run_id(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, ALIGNED_VERSION, "linux-x86_64", "tar")
            result = run_manifest(
                dist, ALIGNED_VERSION, ["linux-x86_64"], run_id="linux-run"
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("decimal run id", result.stderr)

    def test_resume_manifest_keeps_product_and_workflow_sources_distinct(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, ALIGNED_VERSION, "linux-x86_64", "tar")
            receipt = dist / "fanout-resume-receipt.json"
            receipt.write_text(
                json.dumps(
                    {
                        "productSource": SHA_A,
                        "workflowSource": SHA_B,
                        "lhcSdkCommit": SHA_C,
                        "seed": {"runId": 32669557893, "artifactId": 9501623903},
                        "qualification": {
                            "jobId": 97274114206,
                            "evidenceArtifactId": 9501839613,
                        },
                    }
                ),
                encoding="utf-8",
            )
            result = run_manifest(
                dist,
                ALIGNED_VERSION,
                ["linux-x86_64"],
                workflow_source=SHA_B,
                resume_receipt=receipt,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = json.loads((dist / "release-manifest.json").read_text())
            self.assertEqual(manifest["sourceCommit"], SHA_A)
            self.assertEqual(manifest["productSource"], SHA_A)
            self.assertEqual(manifest["workflowSource"], SHA_B)
            self.assertEqual(
                manifest["fanoutResume"],
                {
                    "receipt": receipt.name,
                    "seedRunId": 32669557893,
                    "seedArtifactId": 9501623903,
                    "qualificationJobId": 97274114206,
                    "qualificationEvidenceArtifactId": 9501839613,
                },
            )

    def test_resume_manifest_refuses_mixed_source_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            write_archive(dist, ALIGNED_VERSION, "linux-x86_64", "tar")
            receipt = dist / "fanout-resume-receipt.json"
            receipt.write_text(
                json.dumps(
                    {
                        "productSource": SHA_B,
                        "workflowSource": SHA_C,
                        "lhcSdkCommit": SHA_C,
                    }
                ),
                encoding="utf-8",
            )
            result = run_manifest(
                dist,
                ALIGNED_VERSION,
                ["linux-x86_64"],
                workflow_source=SHA_C,
                resume_receipt=receipt,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("source identity mismatch", result.stderr)


if __name__ == "__main__":
    unittest.main()
