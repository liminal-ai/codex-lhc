#!/usr/bin/env python3
"""Regression tests for check_version_identity.py."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/lhc-release/check_version_identity.py"
UPSTREAM_VERSION = "0.153.3"
FORK_RELEASE = "0.153.3-lhc.1"
WORKSPACE_CLI = "version.workspace = true"


def write_fixture(
    root: Path, *, release: str, workspace: str, cli_version: str = WORKSPACE_CLI
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
    def test_repository_accepts_checked_in_bare_release(self) -> None:
        result = run_check(UPSTREAM_VERSION)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            f"aligned at {UPSTREAM_VERSION} (upstream base {UPSTREAM_VERSION})",
            result.stdout,
        )

    def test_fork_release_accepted_when_upstream_base_matches_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_fixture(root, release=FORK_RELEASE, workspace=UPSTREAM_VERSION)
            result = run_check(FORK_RELEASE, root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            f"aligned at {FORK_RELEASE} (upstream base {UPSTREAM_VERSION})",
            result.stdout,
        )

    def test_fork_release_rejected_when_upstream_base_mismatches_workspace(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_fixture(root, release=FORK_RELEASE, workspace="0.153.2")
            result = run_check(FORK_RELEASE, root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Rust workspace version='0.153.2'", result.stderr)

    def test_requested_release_must_equal_checked_in_release(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_fixture(root, release=FORK_RELEASE, workspace=UPSTREAM_VERSION)
            result = run_check(UPSTREAM_VERSION, root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"lhc-release/VERSION='{FORK_RELEASE}'", result.stderr)

    def test_non_fork_release_forms_are_rejected(self) -> None:
        for version in ("v0.153.3", "0.153.3-alpha.1", "0.153.3-lhc.01", "0.153"):
            with self.subTest(version=version):
                result = run_check(version)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("X.Y.Z or X.Y.Z-lhc.N", result.stderr)

    def test_cli_literal_version_is_rejected_even_when_values_match(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_fixture(
                root,
                release=UPSTREAM_VERSION,
                workspace=UPSTREAM_VERSION,
                cli_version=f'version = "{UPSTREAM_VERSION}"',
            )
            result = run_check(UPSTREAM_VERSION, root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version.workspace = true", result.stderr)


if __name__ == "__main__":
    unittest.main()
