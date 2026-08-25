import json
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]


class ReleaseWorkflowContractTests(unittest.TestCase):
    def test_musl_tool_install_preserves_target_without_outer_sudo(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release.yml").read_text()
        seed = text.split("  seed:\n", 1)[1].split("\n  qualify-seed:\n", 1)[0]
        remaining = text.split("  build-remaining:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]
        invocation = (
            'run: bash "${GITHUB_WORKSPACE}/.github/scripts/'
            'install-musl-build-tools.sh"'
        )

        self.assertEqual(text.count(invocation), 2)
        for line in text.splitlines():
            if "install-musl-build-tools.sh" in line:
                self.assertNotIn("sudo", line)
        self.assertIn("TARGET: x86_64-unknown-linux-musl", seed)
        self.assertIn("TARGET: ${{ matrix.target }}", remaining)
        self.assertIn("if: matrix.kind == 'linux'", remaining)

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
        self.assertIn(
            "Run supplemental source regressions (not artifact qualification)",
            qualification,
        )
        self.assertEqual(
            qualification.count("scripts/check-lhc-installed-lifecycle.py"), 2
        )
        self.assertEqual(qualification.count('--binary "$launcher"'), 2)
        self.assertNotIn("--enable lhc_capture", qualification)
        self.assertIn("env -u LHC_COMPACT_ALGORITHM", qualification)
        self.assertIn("--mode metadata-first", qualification)
        self.assertIn("LHC_COMPACT_ALGORITHM=legacy", qualification)
        self.assertIn("--mode legacy", qualification)
        self.assertIn("app-server daemon bootstrap", qualification)
        self.assertIn('cmp "$package/bin/codex" "$managed"', qualification)
        self.assertIn(
            "Preserve installed lifecycle qualification evidence", qualification
        )
        self.assertIn("if: always()", qualification)
        self.assertIn("codex-lhc-linux-x86_64-qualification-evidence", qualification)
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

    def test_remaining_matrix_is_portable_across_the_four_native_hosts(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release.yml").read_text()
        remaining = text.split("  build-remaining:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]

        # A Python with tomllib must exist before release identity validation,
        # which imports it via check_version_identity.py.
        setup_python = (
            "actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405"
        )
        self.assertIn(setup_python, remaining)
        self.assertIn('python-version: "3.13"', remaining)
        self.assertLess(
            remaining.index(setup_python),
            remaining.index("Validate release and public SDK identity"),
        )

        # Full package discovery on POSIX; exactly 11 applicable tests on
        # native Windows, which lacks POSIX executable mode bits.
        self.assertIn(
            "- name: Test canonical package builder\n        if: "
            "matrix.kind != 'windows'",
            remaining,
        )
        windows_packages = remaining.split(
            "Test exactly 11 applicable canonical package tests on Windows", 1
        )[1].split("- name: ", 1)[0]
        self.assertIn("if: matrix.kind == 'windows'", windows_packages)
        command = windows_packages.split("PYTHONPATH=scripts/codex_package", 1)[1]
        self.assertEqual(
            len(re.findall(r"test_\w+\.\w+\.test_\w+", command)), 11
        )

        # Linux keeps full release-helper discovery, including the musl UAPI
        # helper test and the POSIX installer ARM64 fixture mapping.
        self.assertIn(
            "- name: Test release helpers and POSIX installer\n        if: "
            "matrix.kind == 'linux'\n        shell: bash\n        run: python "
            "-m unittest discover -s scripts/lhc-release -p 'test_*.py'",
            remaining,
        )

        # macOS runs every release helper test except the Linux-only musl one.
        macos_helpers = remaining.split(
            "Test release helpers except the Linux-only musl helper", 1
        )[1].split("- name: ", 1)[0]
        self.assertIn("if: matrix.kind == 'macos'", macos_helpers)
        self.assertIn("exempted_test=test_install_musl_build_tools.py", macos_helpers)
        self.assertIn("for test_file in scripts/lhc-release/test_*.py", macos_helpers)
        self.assertIn('test "$#" -gt 0', macos_helpers)

        # Windows still runs only the PowerShell installer test.
        self.assertIn(
            "- name: Test Windows installer\n        if: matrix.kind == "
            "'windows'\n        shell: pwsh\n        run: "
            "./scripts/lhc-release/test_install.ps1",
            remaining,
        )

        # The build and archive contract is unchanged.
        self.assertIn("--lhc-thread-schema 12 --force", remaining)
        self.assertIn("verify_package_archive.py", remaining)

    def test_posix_installer_fixture_maps_linux_arm64(self) -> None:
        text = (ROOT / "scripts/lhc-release/test_install.py").read_text()
        self.assertIn('if system == "Linux" and machine in {"aarch64", "arm64"}', text)
        self.assertIn('return "linux-aarch64"', text)
        self.assertIn("def test_linux_aarch64_uses_native_release_fixture", text)

    def test_promotion_uses_maintainer_token_and_hard_public_readback(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release-promote.yml").read_text()
        self.assertIn("CODEX_LHC_RELEASE_TOKEN", text)
        self.assertIn("Hard public tag, release, and exact-asset postconditions", text)
        self.assertIn("verify_public_release.py", text)
        self.assertIn("env -u GH_TOKEN -u GITHUB_TOKEN", text)
        self.assertNotIn("cargo build", text)


if __name__ == "__main__":
    unittest.main()
