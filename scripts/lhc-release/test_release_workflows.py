from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]


class ReleaseWorkflowContractTests(unittest.TestCase):
    def test_candidate_has_exact_hosted_platform_set(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release.yml").read_text()
        matrix = text.split("matrix:", 1)[1].split("    env:", 1)[0]
        self.assertEqual(
            re.findall(r"platform: ([a-z0-9_-]+)", matrix),
            [
                "linux-x86_64",
                "linux-aarch64",
                "windows-x86_64",
                "windows-aarch64",
                "macos-aarch64",
            ],
        )
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
