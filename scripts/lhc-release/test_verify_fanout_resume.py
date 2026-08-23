#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

SCRIPT = Path(__file__).with_name("verify_fanout_resume.py")
SPEC = importlib.util.spec_from_file_location("verify_fanout_resume", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
resume = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(resume)


WORKFLOW_SOURCE = "f" * 40
RUN_ID = "40000000000"
VERSION = "0.149.0"


def write_zip(path: Path, members: dict[str, bytes]) -> dict:
    with zipfile.ZipFile(path, "w") as archive:
        for name, body in members.items():
            archive.writestr(name, body)
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    return {
        "id": 1,
        "name": "fixture",
        "size": path.stat().st_size,
        "digest": f"sha256:{digest}",
    }


def write_platform_set(root: Path) -> None:
    for platform, (target, kind) in resume.PLATFORMS.items():
        archive_name = f"codex-lhc-v{VERSION}-{platform}.{kind}"
        archive = root / archive_name
        archive.write_bytes(f"{platform}\n".encode())
        receipt = {
            "schemaVersion": 1,
            "platform": platform,
            "target": target,
            "archive": archive_name,
            "archiveSha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
            "archiveBytes": archive.stat().st_size,
            "productSource": resume.PRODUCT_SOURCE,
            "workflowSource": WORKFLOW_SOURCE,
            "lhcSdkCommit": resume.LHC_SDK_COMMIT,
            "sourceTreeClean": True,
            "buildRunId": RUN_ID,
        }
        (root / f"provenance-{platform}.json").write_text(
            json.dumps(receipt), encoding="utf-8"
        )


class FanoutResumeVerificationTests(unittest.TestCase):
    def test_retained_run_and_qualification_job_are_exact(self) -> None:
        resume.validate_run(
            {
                "id": resume.SEED_RUN_ID,
                "head_sha": resume.PRODUCT_SOURCE,
                "path": ".github/workflows/lhc-release.yml",
            }
        )
        resume.validate_job(
            {
                "id": resume.QUALIFICATION_JOB["id"],
                "name": resume.QUALIFICATION_JOB["name"],
                "run_id": resume.SEED_RUN_ID,
                "head_sha": resume.PRODUCT_SOURCE,
                "conclusion": "success",
            }
        )

        with self.assertRaisesRegex(ValueError, "did not succeed"):
            resume.validate_job(
                {
                    "id": resume.QUALIFICATION_JOB["id"],
                    "name": resume.QUALIFICATION_JOB["name"],
                    "run_id": resume.SEED_RUN_ID,
                    "head_sha": resume.PRODUCT_SOURCE,
                    "conclusion": "failure",
                }
            )

    def test_artifact_refuses_expiration_digest_and_missing_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            download = root / "artifact.zip"
            expected = write_zip(download, {"evidence/report.json": b"{}"})
            artifact = {
                "id": expected["id"],
                "name": expected["name"],
                "size_in_bytes": expected["size"],
                "digest": expected["digest"],
                "expired": False,
                "workflow_run": {
                    "id": resume.SEED_RUN_ID,
                    "head_sha": resume.PRODUCT_SOURCE,
                },
            }
            resume.validate_artifact(
                artifact, expected, download, evidence=True, version=VERSION
            )

            accepted_bytes = download.read_bytes()
            mutated_bytes = bytearray(accepted_bytes)
            mutated_bytes[len(mutated_bytes) // 2] ^= 1
            download.write_bytes(mutated_bytes)
            with self.assertRaisesRegex(ValueError, "downloaded digest mismatch"):
                resume.validate_artifact(
                    artifact, expected, download, evidence=True, version=VERSION
                )
            download.write_bytes(accepted_bytes)

            expired = {**artifact, "expired": True}
            with self.assertRaisesRegex(ValueError, "expired"):
                resume.validate_artifact(
                    expired, expected, download, evidence=True, version=VERSION
                )
            bad_digest = {**artifact, "digest": "sha256:" + "0" * 64}
            with self.assertRaisesRegex(ValueError, "API digest"):
                resume.validate_artifact(
                    bad_digest, expected, download, evidence=True, version=VERSION
                )

            empty = root / "empty.zip"
            empty_expected = write_zip(empty, {})
            empty_artifact = {
                "id": empty_expected["id"],
                "name": empty_expected["name"],
                "size_in_bytes": empty_expected["size"],
                "digest": empty_expected["digest"],
                "expired": False,
                "workflow_run": artifact["workflow_run"],
            }
            with self.assertRaisesRegex(ValueError, "has no files"):
                resume.validate_artifact(
                    empty_artifact,
                    empty_expected,
                    empty,
                    evidence=True,
                    version=VERSION,
                )

    def test_exact_four_platform_receipts_accept(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_platform_set(root)
            receipts = resume.validate_platform_receipts(
                root,
                version=VERSION,
                workflow_source=WORKFLOW_SOURCE,
                current_run_id=RUN_ID,
            )
            self.assertEqual(
                [receipt["platform"] for receipt in receipts],
                sorted(resume.PLATFORMS),
            )

    def test_platform_receipts_refuse_missing_archive_and_mixed_source(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_platform_set(root)
            (root / f"codex-lhc-v{VERSION}-linux-aarch64.tar.gz").unlink()
            with self.assertRaisesRegex(ValueError, "resumed platform archives"):
                resume.validate_platform_receipts(
                    root,
                    version=VERSION,
                    workflow_source=WORKFLOW_SOURCE,
                    current_run_id=RUN_ID,
                )

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_platform_set(root)
            receipt_path = root / "provenance-windows-x86_64.json"
            receipt = json.loads(receipt_path.read_text())
            receipt["productSource"] = WORKFLOW_SOURCE
            receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "mixed or invalid provenance"):
                resume.validate_platform_receipts(
                    root,
                    version=VERSION,
                    workflow_source=WORKFLOW_SOURCE,
                    current_run_id=RUN_ID,
                )

            receipt["productSource"] = resume.PRODUCT_SOURCE
            receipt["sourceTreeClean"] = False
            receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "mixed or invalid provenance"):
                resume.validate_platform_receipts(
                    root,
                    version=VERSION,
                    workflow_source=WORKFLOW_SOURCE,
                    current_run_id=RUN_ID,
                )

            receipt["sourceTreeClean"] = True
            receipt["lhcSdkCommit"] = "0" * 40
            receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "mixed or invalid provenance"):
                resume.validate_platform_receipts(
                    root,
                    version=VERSION,
                    workflow_source=WORKFLOW_SOURCE,
                    current_run_id=RUN_ID,
                )


if __name__ == "__main__":
    unittest.main()
