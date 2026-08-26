#!/usr/bin/env python3
"""Regression tests for check_version_identity.py."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/lhc-release/check_version_identity.py"
ALIGNED_VERSION = "0.149.2"


def write_fixture(
    root: Path, *, release: str, workspace: str, cli_version: str
) -> None:
    (root / "lhc-release").mkdir()
    (root / "codex-rs/cli").mkdir(parents=True)
    (root / "lhc-release/VERSION").write_text(f"{release}\n", encoding="utf-8")
    (root / "codex-rs/Cargo.toml").write_text(
        f'[workspace.package]\nversion = "{workspace}"\n', encoding="utf-8"
    )
    (root / "codex-rs/cli/Cargo.toml").write_text(
        f'[package]\nname = "codex-cli"\n{cli_version}\n', encoding="utf-8"
    )


def run_check(version: str, root: Path = ROOT) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), "--version", version, "--root", str(root)],
        text=True,
        capture_output=True,
        check=False,
    )


class VersionIdentityTests(unittest.TestCase):
    def test_repository_accepts_aligned_upstream_prerelease(self) -> None:
        result = run_check(ALIGNED_VERSION)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"aligned at {ALIGNED_VERSION}", result.stdout)

    def test_cli_literal_version_is_rejected_even_when_values_match(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_fixture(
                root,
                release=ALIGNED_VERSION,
                workspace=ALIGNED_VERSION,
                cli_version=f'version = "{ALIGNED_VERSION}"',
            )
            result = run_check(ALIGNED_VERSION, root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version.workspace = true", result.stderr)

    def test_release_version_mismatch_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_fixture(
                root,
                release="0.148.0-alpha.20",
                workspace=ALIGNED_VERSION,
                cli_version="version.workspace = true",
            )
            result = run_check(ALIGNED_VERSION, root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lhc-release/VERSION", result.stderr)


if __name__ == "__main__":
    unittest.main()
