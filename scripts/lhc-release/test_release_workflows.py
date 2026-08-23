import json
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]


class ReleaseWorkflowContractTests(unittest.TestCase):
    def test_linux_x86_64_seed_gates_paid_fanout(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release.yml").read_text()
        seed = text.split("  seed:\n", 1)[1].split("\n  qualify-seed:\n", 1)[0]
        qualification = text.split("  qualify-seed:\n", 1)[1].split(
            "\n  build-remaining:\n", 1
        )[0]
        remaining = text.split("  build-remaining:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]
        candidate = text.split("  candidate:\n", 1)[1]

        self.assertIn("PLATFORM: linux-x86_64", seed)
        self.assertIn("TARGET: x86_64-unknown-linux-musl", seed)
        self.assertIn("verify_package_archive.py", seed)
        self.assertIn("Prove default LHC capture on seed", seed)
        self.assertIn("Upload exact verified seed archive", seed)
        self.assertIn("needs: seed", qualification)
        self.assertIn("name: codex-lhc-linux-x86_64", qualification)
        self.assertIn("Download exact seed archive without rebuilding", qualification)
        self.assertIn("verify_package_archive.py", qualification)
        self.assertIn("--asset-dir", qualification)
        self.assertIn("--uninstall", qualification)
        self.assertIn("codex-cli $VERSION", qualification)
        self.assertIn("codex-code-mode-host", qualification)
        self.assertIn("codex-path/rg", qualification)
        self.assertIn("codex-resources/bwrap", qualification)
        self.assertIn("bundled_bwrap", qualification)
        self.assertIn("app-server generate-json-schema", qualification)
        self.assertIn(
            "seed_managed_codex_copies_running_fork_bytes_into_isolated_home",
            qualification,
        )
        self.assertIn("check-lhc-default-capture.py", qualification)
        self.assertIn("view_compact_bounded", qualification)
        self.assertIn("codex-lhc-host --lib materialize", qualification)
        self.assertIn("slice_d_", qualification)
        self.assertIn(
            "c1_resume_after_compact_no_reingest_and_durable_provenance_survives",
            qualification,
        )
        self.assertIn(
            "LHC_COMPACT_ALGORITHM",
            (ROOT / "scripts/lhc-release/make_manifest.py").read_text(),
        )
        self.assertIn("needs: qualify-seed", remaining)
        self.assertNotIn("needs: seed", remaining)
        self.assertIn("needs: [qualify-seed, build-remaining]", candidate)
        self.assertIn("pattern: codex-lhc-*", candidate)

    def test_remaining_matrix_has_exact_four_targets(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release.yml").read_text()
        remaining = text.split("  build-remaining:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]
        matrix = remaining.split("matrix:", 1)[1].split("    env:", 1)[0]
        self.assertEqual(
            re.findall(r"platform: ([a-z0-9_-]+)", matrix),
            [
                "linux-aarch64",
                "windows-x86_64",
                "windows-aarch64",
                "macos-aarch64",
            ],
        )
        self.assertEqual(
            re.findall(r"target: ([a-z0-9_-]+)", matrix),
            [
                "aarch64-unknown-linux-musl",
                "x86_64-pc-windows-msvc",
                "aarch64-pc-windows-msvc",
                "aarch64-apple-darwin",
            ],
        )
        self.assertNotIn("linux-x86_64", remaining)
        self.assertNotIn("x86_64-unknown-linux-musl", remaining)

    def test_seed_is_the_only_linux_x86_64_build(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release.yml").read_text()
        build_jobs = text.split("\n  candidate:\n", 1)[0]
        qualification = text.split("  qualify-seed:\n", 1)[1].split(
            "\n  build-remaining:\n", 1
        )[0]
        self.assertEqual(build_jobs.count("PLATFORM: linux-x86_64"), 1)
        self.assertEqual(build_jobs.count("TARGET: x86_64-unknown-linux-musl"), 1)
        self.assertNotIn("scripts/build_codex_package.py", qualification)

    def test_windows_arm64_uses_proven_standard_hosted_route(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release.yml").read_text()
        remaining = text.split("  build-remaining:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]
        evidence = json.loads(
            (ROOT / "lhc-release/windows-arm64-runner-evidence.json").read_text()
        )
        self.assertEqual(evidence["repository"], "liminal-ai/codex-lhc")
        self.assertEqual(evidence["repositoryVisibility"], "public")
        self.assertEqual(evidence["repositorySelfHostedRunnerCount"], 0)
        self.assertEqual(
            evidence["selectedRoute"],
            {
                "kind": "standard-github-hosted",
                "label": "windows-11-arm",
                "operatingSystem": "Windows 11",
                "architecture": "arm64",
                "cpu": 4,
                "memoryGiB": 16,
                "storageGiB": 14,
                "availability": "standard runner for public repositories",
                "source": "https://docs.github.com/en/actions/reference/runners/github-hosted-runners",
            },
        )
        self.assertIn("runs_on: windows-11-arm", remaining)
        self.assertNotIn("group:", remaining)

    def test_candidate_has_exact_hosted_platform_set(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release.yml").read_text()
        self.assertNotIn("macos-x86_64", text)
        self.assertNotIn("daytona", text.lower())
        self.assertNotIn("dgx", text.lower())
        self.assertIn("scripts/build_codex_package.py", text)
        self.assertIn("verify_package_archive.py", text)

        readiness = (ROOT / ".github/workflows/lhc-platform-readiness.yml").read_text()
        self.assertEqual(
            re.findall(r"          - label: ([a-z0-9_-]+)", readiness),
            [
                "linux-x86_64",
                "linux-aarch64",
                "windows-x86_64",
                "windows-aarch64",
                "macos-aarch64",
            ],
        )

    def test_promotion_uses_maintainer_token_and_hard_public_readback(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release-promote.yml").read_text()
        self.assertIn("CODEX_LHC_RELEASE_TOKEN", text)
        self.assertIn("Hard public tag, release, and exact-asset postconditions", text)
        self.assertIn("verify_public_release.py", text)
        self.assertIn("env -u GH_TOKEN -u GITHUB_TOKEN", text)
        self.assertNotIn("cargo build", text)


if __name__ == "__main__":
    unittest.main()
