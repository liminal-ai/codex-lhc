#!/usr/bin/env python3
"""Verify the bounded v0.149 fanout-resume inputs and build receipts."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import zipfile


PRODUCT_SOURCE = "a25a81a8d7d6cbd6234011ad3787cd7e59a1f489"
LHC_SDK_COMMIT = "9d4d18247942f35b356f28bdc4288f0e631a6da9"
SEED_RUN_ID = 32669557893
SEED_ARTIFACT = {
    "id": 9501623903,
    "name": "codex-lhc-linux-x86_64",
    "size": 356551728,
    "digest": "sha256:4873a72d9ee80725dc51c0e503792291bee71d9a63aefb29d6c841cb2e0d9356",
}
QUALIFICATION_JOB = {
    "id": 97274114206,
    "name": "Qualify exact downloaded linux-x86_64 seed",
}
QUALIFICATION_ARTIFACT = {
    "id": 9501839613,
    "name": "codex-lhc-linux-x86_64-qualification-evidence",
    "size": 219676,
    "digest": "sha256:cadef3e1e4dcf3da2fb74ef648409c219dee9f447065f1ff861e17dc1f1ce11f",
}
PLATFORMS = {
    "linux-aarch64": ("aarch64-unknown-linux-musl", "tar.gz"),
    "macos-aarch64": ("aarch64-apple-darwin", "tar.gz"),
    "windows-aarch64": ("aarch64-pc-windows-msvc", "zip"),
    "windows-x86_64": ("x86_64-pc-windows-msvc", "zip"),
}


def load_json(path: Path) -> dict:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object: {path}")
    return value


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def validate_run(run: dict) -> None:
    require(run.get("id") == SEED_RUN_ID, "seed run id mismatch")
    require(run.get("head_sha") == PRODUCT_SOURCE, "seed run head mismatch")
    require(
        run.get("path") == ".github/workflows/lhc-release.yml",
        "seed run workflow mismatch",
    )


def validate_job(job: dict) -> None:
    require(job.get("id") == QUALIFICATION_JOB["id"], "qualification job id mismatch")
    require(
        job.get("name") == QUALIFICATION_JOB["name"],
        "qualification job name mismatch",
    )
    require(job.get("run_id") == SEED_RUN_ID, "qualification job run mismatch")
    require(job.get("head_sha") == PRODUCT_SOURCE, "qualification job head mismatch")
    require(job.get("conclusion") == "success", "qualification job did not succeed")


def validate_artifact(
    artifact: dict, expected: dict, download: Path, *, evidence: bool, version: str
) -> None:
    label = expected["name"]
    require(artifact.get("id") == expected["id"], f"{label} id mismatch")
    require(artifact.get("name") == label, f"{label} name mismatch")
    require(artifact.get("size_in_bytes") == expected["size"], f"{label} size mismatch")
    require(
        artifact.get("digest") == expected["digest"], f"{label} API digest mismatch"
    )
    require(artifact.get("expired") is False, f"{label} is expired")
    workflow_run = artifact.get("workflow_run") or {}
    require(workflow_run.get("id") == SEED_RUN_ID, f"{label} run mismatch")
    require(workflow_run.get("head_sha") == PRODUCT_SOURCE, f"{label} head mismatch")
    require(download.is_file(), f"missing downloaded artifact: {download}")
    require(
        download.stat().st_size == expected["size"], f"{label} downloaded size mismatch"
    )
    actual_digest = f"sha256:{sha256(download)}"
    require(actual_digest == expected["digest"], f"{label} downloaded digest mismatch")

    with zipfile.ZipFile(download) as archive:
        files = [name for name in archive.namelist() if not name.endswith("/")]
        require(bool(files), f"{label} archive has no files")
        for name in files:
            parts = PurePosixPath(name).parts
            require(
                not PurePosixPath(name).is_absolute() and ".." not in parts,
                f"{label} contains unsafe path {name!r}",
            )
        if not evidence:
            expected_name = f"codex-lhc-v{version}-linux-x86_64.tar.gz"
            require(files == [expected_name], f"seed archive members mismatch: {files}")


def validate_platform_receipts(
    root: Path, *, version: str, workflow_source: str, current_run_id: str
) -> list[dict]:
    expected_archives = {
        f"codex-lhc-v{version}-{platform}.{kind}"
        for platform, (_, kind) in PLATFORMS.items()
    }
    found_archives = {
        path.name for path in root.glob(f"codex-lhc-v{version}-*") if path.is_file()
    }
    require(
        found_archives == expected_archives,
        f"resumed platform archives {sorted(found_archives)} != {sorted(expected_archives)}",
    )
    expected_receipts = {f"provenance-{platform}.json" for platform in PLATFORMS}
    found_receipts = {path.name for path in root.glob("provenance-*.json")}
    require(
        found_receipts == expected_receipts,
        f"platform receipts {sorted(found_receipts)} != {sorted(expected_receipts)}",
    )

    receipts = []
    for platform, (target, kind) in sorted(PLATFORMS.items()):
        archive_name = f"codex-lhc-v{version}-{platform}.{kind}"
        archive = root / archive_name
        receipt = load_json(root / f"provenance-{platform}.json")
        expected = {
            "schemaVersion": 1,
            "platform": platform,
            "target": target,
            "archive": archive_name,
            "archiveSha256": sha256(archive),
            "archiveBytes": archive.stat().st_size,
            "productSource": PRODUCT_SOURCE,
            "workflowSource": workflow_source,
            "lhcSdkCommit": LHC_SDK_COMMIT,
            "sourceTreeClean": True,
            "buildRunId": current_run_id,
        }
        require(receipt == expected, f"mixed or invalid provenance for {platform}")
        receipts.append(receipt)
    return receipts


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--workflow-source", required=True)
    parser.add_argument("--current-run-id", required=True)
    parser.add_argument("--run-json", type=Path, required=True)
    parser.add_argument("--job-json", type=Path, required=True)
    parser.add_argument("--seed-artifact-json", type=Path, required=True)
    parser.add_argument("--qualification-artifact-json", type=Path, required=True)
    parser.add_argument("--seed-download", type=Path, required=True)
    parser.add_argument("--qualification-download", type=Path, required=True)
    parser.add_argument("--platform-root", type=Path, required=True)
    parser.add_argument("--receipt-output", type=Path, required=True)
    args = parser.parse_args()

    require(
        len(args.workflow_source) == 40
        and all(char in "0123456789abcdef" for char in args.workflow_source),
        "workflow source must be a 40-hex SHA",
    )
    require(
        args.workflow_source != PRODUCT_SOURCE, "workflow source equals product source"
    )
    require(args.current_run_id.isdecimal(), "current run id must be decimal")

    run = load_json(args.run_json)
    job = load_json(args.job_json)
    seed_artifact = load_json(args.seed_artifact_json)
    qualification_artifact = load_json(args.qualification_artifact_json)
    validate_run(run)
    validate_job(job)
    validate_artifact(
        seed_artifact,
        SEED_ARTIFACT,
        args.seed_download,
        evidence=False,
        version=args.version,
    )
    validate_artifact(
        qualification_artifact,
        QUALIFICATION_ARTIFACT,
        args.qualification_download,
        evidence=True,
        version=args.version,
    )
    builds = validate_platform_receipts(
        args.platform_root,
        version=args.version,
        workflow_source=args.workflow_source,
        current_run_id=args.current_run_id,
    )

    receipt = {
        "schemaVersion": 1,
        "productSource": PRODUCT_SOURCE,
        "workflowSource": args.workflow_source,
        "lhcSdkCommit": LHC_SDK_COMMIT,
        "orchestrationRunId": args.current_run_id,
        "seed": {
            "runId": SEED_RUN_ID,
            "artifactId": SEED_ARTIFACT["id"],
            "artifactName": SEED_ARTIFACT["name"],
            "artifactBytes": SEED_ARTIFACT["size"],
            "artifactDigest": SEED_ARTIFACT["digest"],
        },
        "qualification": {
            "jobId": QUALIFICATION_JOB["id"],
            "conclusion": "success",
            "evidenceArtifactId": QUALIFICATION_ARTIFACT["id"],
            "evidenceArtifactName": QUALIFICATION_ARTIFACT["name"],
            "evidenceArtifactBytes": QUALIFICATION_ARTIFACT["size"],
            "evidenceArtifactDigest": QUALIFICATION_ARTIFACT["digest"],
        },
        "resumedBuilds": builds,
    }
    args.receipt_output.write_text(
        json.dumps(receipt, indent=2) + "\n", encoding="utf-8"
    )
    print("verified qualified seed and exact four-platform resumed fanout")


if __name__ == "__main__":
    main()
