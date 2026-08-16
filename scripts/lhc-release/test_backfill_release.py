#!/usr/bin/env python3
from __future__ import annotations

import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

from backfill_release import BACKFILL_MARKER, BackfillError, run_backfill, sha256


VERSION = "0.2.2"
SOURCE = "a" * 40


def write_combined(root: Path) -> Path:
    combined = root / "combined"
    combined.mkdir()
    files = {
        f"codex-lhc-v{VERSION}-linux-x86_64.tar.gz": b"linux-bytes\n",
        f"codex-lhc-v{VERSION}-windows-x86_64.zip": b"windows-bytes\n",
        f"codex-lhc-v{VERSION}-macos-aarch64.tar.gz": b"macos-bytes\n",
        "install.sh": b"#!/bin/sh\n",
        "install.ps1": b"# ps1\n",
        "SHA256SUMS": b"deadbeef  ignored\n",
        "release-manifest.json": json.dumps(
            {
                "release": VERSION,
                "sourceCommit": SOURCE,
                "upstreamCodexCommit": "b" * 40,
                "lhcSdkCommit": "c" * 40,
                "buildRunId": "1",
                "supplementalRunId": "2",
            }
        ).encode("utf-8"),
    }
    for name, body in files.items():
        (combined / name).write_bytes(body)
    return combined


class FakeGh:
    def __init__(self, *, tag_sha: str = SOURCE, release_target: str = SOURCE, linux_bytes: bytes | None = None, fail_upload: bool = False):
        self.tag_sha = tag_sha
        self.release_target = release_target
        self.linux_bytes = linux_bytes if linux_bytes is not None else b"linux-bytes\n"
        self.fail_upload = fail_upload
        self.assets = {
            f"codex-lhc-v{VERSION}-linux-x86_64.tar.gz": {
                "id": 11,
                "name": f"codex-lhc-v{VERSION}-linux-x86_64.tar.gz",
                "size": len(self.linux_bytes),
                "url": "asset://linux",
                "bytes": self.linux_bytes,
            }
        }
        self.notes = "original notes"
        self.calls: list[tuple[str, ...]] = []

    def __call__(self, repo: str, *args: str) -> str:
        self.calls.append(args)
        if args[:3] == ("api", f"repos/{repo}/git/refs/tags/v{VERSION}"):
            return json.dumps({"object": {"sha": self.tag_sha, "type": "commit"}})
        if args[:2] == ("release", "view"):
            return json.dumps(
                {
                    "tagName": f"v{VERSION}",
                    "targetCommitish": self.release_target,
                    "body": self.notes,
                    "assets": [
                        {k: v for k, v in asset.items() if k != "bytes"}
                        for asset in self.assets.values()
                    ],
                }
            )
        if args[:2] == ("release", "upload"):
            if self.fail_upload:
                raise BackfillError("gh release upload failed: boom")
            for path in args[3:]:
                if path == "--clobber":
                    continue
                data = Path(path).read_bytes()
                name = Path(path).name
                current = self.assets.get(name, {"id": 100 + len(self.assets), "url": f"asset://{name}"})
                self.assets[name] = {
                    "id": current["id"],
                    "name": name,
                    "size": len(data),
                    "url": current["url"],
                    "bytes": data,
                }
            return ""
        if args[:2] == ("release", "edit"):
            self.notes = args[args.index("--notes") + 1]
            return ""
        raise AssertionError(args)

    def download(self, repo: str, url: str, dest: Path) -> None:
        for asset in self.assets.values():
            if asset["url"] == url or f"releases/assets/{asset['id']}" in url:
                dest.write_bytes(asset["bytes"])
                return
        raise BackfillError(f"download {url} failed: missing")


class BackfillTests(unittest.TestCase):
    def test_wrong_tag_target(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            combined = write_combined(root)
            gh = FakeGh(tag_sha="d" * 40)
            with self.assertRaises(BackfillError) as ctx:
                run_backfill(version=VERSION, combined_dir=combined, repo="liminal-ai/codex-lhc", gh=gh, download=gh.download)
            self.assertIn("identity mismatch", str(ctx.exception))

    def test_missing_linux(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            combined = write_combined(root)
            (combined / f"codex-lhc-v{VERSION}-linux-x86_64.tar.gz").unlink()
            gh = FakeGh()
            with self.assertRaises(BackfillError) as ctx:
                run_backfill(version=VERSION, combined_dir=combined, repo="liminal-ai/codex-lhc", gh=gh, download=gh.download)
            self.assertIn("required file missing", str(ctx.exception))

    def test_preexisting_linux_hash_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            combined = write_combined(root)
            gh = FakeGh(linux_bytes=b"other-linux\n")
            with self.assertRaises(BackfillError) as ctx:
                run_backfill(version=VERSION, combined_dir=combined, repo="liminal-ai/codex-lhc", gh=gh, download=gh.download)
            self.assertIn("preexisting Linux hash mismatch", str(ctx.exception))

    def test_upload_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            combined = write_combined(root)
            gh = FakeGh(fail_upload=True)
            with self.assertRaises(BackfillError) as ctx:
                run_backfill(version=VERSION, combined_dir=combined, repo="liminal-ai/codex-lhc", gh=gh, download=gh.download)
            self.assertIn("upload", str(ctx.exception))

    def test_final_download_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            combined = write_combined(root)
            gh = FakeGh()

            def bad_download(repo: str, url: str, dest: Path) -> None:
                if dest.parent.name == "after" and dest.name.endswith(".zip"):
                    dest.write_bytes(b"tampered\n")
                    return
                gh.download(repo, url, dest)

            with self.assertRaises(BackfillError) as ctx:
                run_backfill(
                    version=VERSION,
                    combined_dir=combined,
                    repo="liminal-ai/codex-lhc",
                    gh=gh,
                    download=bad_download,
                )
            self.assertIn("final download mismatch", str(ctx.exception))

    def test_successful_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            combined = write_combined(root)
            gh = FakeGh()
            receipt_path = root / "receipt.json"
            receipt = run_backfill(
                version=VERSION,
                combined_dir=combined,
                repo="liminal-ai/codex-lhc",
                receipt_path=receipt_path,
                gh=gh,
                download=gh.download,
                now=lambda: datetime(2026, 8, 16, 23, 0, tzinfo=timezone.utc),
            )
            self.assertTrue(receipt["linuxUnchanged"])
            self.assertEqual(receipt["targetCommitish"], SOURCE)
            self.assertEqual(receipt["combinedManifest"]["sourceCommit"], SOURCE)
            self.assertEqual(receipt["linuxSha256"], sha256(combined / f"codex-lhc-v{VERSION}-linux-x86_64.tar.gz"))
            self.assertEqual(receipt["timestamp"], "2026-08-16T23:00:00Z")
            self.assertIn(BACKFILL_MARKER, gh.notes)
            self.assertTrue(receipt_path.is_file())
            names = {asset["name"] for asset in receipt["newAssets"]}
            self.assertIn(f"codex-lhc-v{VERSION}-windows-x86_64.zip", names)
            self.assertIn(f"codex-lhc-v{VERSION}-macos-aarch64.tar.gz", names)
            run_backfill(
                version=VERSION,
                combined_dir=combined,
                repo="liminal-ai/codex-lhc",
                receipt_path=root / "receipt2.json",
                gh=gh,
                download=gh.download,
            )
            self.assertEqual(gh.notes.count(BACKFILL_MARKER), 1)


if __name__ == "__main__":
    unittest.main()
