import json
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
RESUME_WORKFLOW = ROOT / ".github/workflows/lhc-release-supplemental.yml"


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

    def test_promotion_uses_maintainer_token_and_hard_public_readback(self) -> None:
        text = (ROOT / ".github/workflows/lhc-release-promote.yml").read_text()
        self.assertIn("CODEX_LHC_RELEASE_TOKEN", text)
        self.assertIn("Hard public tag, release, and exact-asset postconditions", text)
        self.assertIn("verify_public_release.py", text)
        self.assertIn("env -u GH_TOKEN -u GITHUB_TOKEN", text)
        self.assertIn(".github/workflows/lhc-release-supplemental.yml", text)
        self.assertIn(".github/workflows/lhc-release.yml)", text)
        self.assertIn('product_source="$workflow_source"', text)
        self.assertIn('artifact_name="codex-lhc-v${VERSION}-candidate"', text)
        self.assertIn("product_source=a25a81a8d7d6cbd6234011ad3787cd7e59a1f489", text)
        self.assertIn("git merge-base --is-ancestor", text)
        self.assertIn(
            "target_commitish: ${{ steps.identity.outputs.product_source }}", text
        )
        self.assertIn('--product-source "$PRODUCT_SOURCE"', text)
        self.assertIn('--workflow-source "$WORKFLOW_SOURCE"', text)
        self.assertIn("- Product source: %s", text)
        self.assertIn("- Workflow source: %s", text)
        self.assertIn("files: candidate/*", text)
        self.assertNotIn("cargo build", text)


class FanoutResumeWorkflowContractTests(unittest.TestCase):
    def test_native_preflights_gate_exact_four_target_matrix(self) -> None:
        text = RESUME_WORKFLOW.read_text()
        build = text.split("  build-resumed-platforms:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]
        matrix = build.split("      matrix:\n", 1)[1].split("    env:\n", 1)[0]

        self.assertIn(
            "needs: [retained-boundary, preflight-macos-bash32, preflight-linux-arm64]",
            build,
        )
        self.assertIn("needs: retained-boundary", text)
        self.assertIn("/bin/bash -c", text)
        self.assertIn('BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}" = 3.2', text)
        self.assertIn("runs-on: ubuntu-24.04-arm", text)
        self.assertIn("Exercise native fixture mapping and release tests", text)
        self.assertEqual(
            re.findall(r"platform: ([a-z0-9_-]+)", matrix),
            [
                "macos-aarch64",
                "linux-aarch64",
                "windows-x86_64",
                "windows-aarch64",
            ],
        )
        self.assertEqual(
            re.findall(r"target: ([a-z0-9_-]+)", matrix),
            [
                "aarch64-apple-darwin",
                "aarch64-unknown-linux-musl",
                "x86_64-pc-windows-msvc",
                "aarch64-pc-windows-msvc",
            ],
        )
        self.assertNotIn("linux-x86_64", matrix)
        self.assertNotIn("x86_64-unknown-linux-musl", matrix)

    def test_builds_replace_workflow_tree_with_fresh_a25_checkout(self) -> None:
        text = RESUME_WORKFLOW.read_text()
        build = text.split("  build-resumed-platforms:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]

        self.assertIn("Fresh full checkout of accepted product source", build)
        self.assertIn("ref: a25a81a8d7d6cbd6234011ad3787cd7e59a1f489", build)
        self.assertIn("submodules: recursive", build)
        self.assertIn("fetch-depth: 0", build)
        self.assertIn("clean: true", build)
        self.assertIn('test "$(git rev-parse HEAD)" = "$PRODUCT_SOURCE"', build)
        self.assertIn("status --porcelain=v1 --ignore-submodules=none", build)
        self.assertIn("test ! -e .github/workflows/lhc-release-supplemental.yml", build)
        self.assertIn('git diff --quiet "$PRODUCT_SOURCE" -- .', build)
        self.assertIn("scripts/build_codex_package.py", build)

    def test_product_fence_uses_pinned_python_with_tomllib(self) -> None:
        text = RESUME_WORKFLOW.read_text()
        build = text.split("  build-resumed-platforms:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]
        checkout = "      - name: Fresh full checkout of accepted product source"
        setup = (
            "      - uses: actions/setup-python@"
            "a309ff8b426b58ec0e2a45f0f869d46889d02405 # v6.2.0"
        )
        fence = "      - name: Fence product checkout from workflow source"
        self.assertEqual(build.count(setup), 1)
        self.assertLess(build.index(checkout), build.index(setup))
        self.assertLess(build.index(setup), build.index(fence))
        setup_block = build.split(setup, 1)[1].split(fence, 1)[0]
        self.assertIn('python-version: "3.13"', setup_block)
        fence_block = build.split(fence, 1)[1].split(
            "      - uses: ./.github/actions/setup-ci", 1
        )[0]
        version_probe = "python --version"
        tomllib_probe = (
            "python -c 'import sys, tomllib; assert sys.version_info[:2] == (3, 13)'"
        )
        identity_check = (
            'python scripts/lhc-release/check_version_identity.py --version "$VERSION"'
        )
        self.assertIn(version_probe, fence_block)
        self.assertIn(tomllib_probe, fence_block)
        self.assertIn(identity_check, fence_block)
        self.assertLess(
            fence_block.index(version_probe), fence_block.index(tomllib_probe)
        )
        self.assertLess(
            fence_block.index(tomllib_probe), fence_block.index(identity_check)
        )

    def test_shared_setup_skips_only_windows_with_prerequisites_preserved(self) -> None:
        text = RESUME_WORKFLOW.read_text()
        build = text.split("  build-resumed-platforms:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]
        shared_setup = "      - uses: ./.github/actions/setup-ci"
        dotslash = (
            "      - uses: facebook/install-dotslash@"
            "1e4e7b3e07eaca387acb98f1d4720e0bee8dbb6a # v2"
        )
        rust = (
            "      - uses: dtolnay/rust-toolchain@"
            "e081816240890017053eacbb1bdf337761dc5582 # 1.95.0"
        )
        windows_paths = "      - name: Configure Windows build paths"
        msvc = "      - uses: ./.github/actions/setup-msvc-env"
        self.assertEqual(build.count(shared_setup), 1)
        shared_block = build.split(shared_setup, 1)[1].split(dotslash, 1)[0]
        self.assertEqual(shared_block.strip(), "if: matrix.kind != 'windows'")
        self.assertLess(build.index(shared_setup), build.index(dotslash))
        self.assertLess(build.index(dotslash), build.index(rust))
        self.assertLess(build.index(rust), build.index(windows_paths))
        self.assertLess(build.index(windows_paths), build.index(msvc))
        windows_block = build.split(windows_paths, 1)[1].split(msvc, 1)[0]
        self.assertIn("if: matrix.kind == 'windows'", windows_block)
        self.assertIn("git config --global core.longpaths true", windows_block)
        self.assertIn('"CARGO_TARGET_DIR=$targetDir"', windows_block)
        msvc_block = build.split(msvc, 1)[1].split(
            "      - name: Test canonical package builder", 1
        )[0]
        self.assertIn("if: matrix.kind == 'windows'", msvc_block)
        self.assertIn("target: ${{ matrix.target }}", msvc_block)

    def test_product_checkout_has_one_platform_specific_preflight_exemption(
        self,
    ) -> None:
        text = RESUME_WORKFLOW.read_text()
        build = text.split("  build-resumed-platforms:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]
        exemption = build.split(
            "Test release helpers with one platform-specific preflight exemption",
            1,
        )[1].split("      - name: Test Windows installer", 1)[0]

        self.assertIn(
            "macos) exempted_test=test_install_musl_build_tools.py", exemption
        )
        self.assertIn("linux) exempted_test=test_install.py", exemption)
        self.assertEqual(exemption.count("continue"), 1)
        self.assertNotIn("|| true", exemption)
        self.assertNotIn("continue-on-error", build)
        self.assertIn(
            'PYTHONPATH="scripts/lhc-release${PYTHONPATH:+:${PYTHONPATH}}"',
            exemption,
        )
        self.assertIn('python -m unittest "$@"', exemption)

    def test_linux_normalizes_only_the_invalid_a25_gcc_frame_option(self) -> None:
        text = RESUME_WORKFLOW.read_text()
        build = text.split("  build-resumed-platforms:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]
        installer = 'run: bash "${GITHUB_WORKSPACE}/.github/scripts/install-musl-build-tools.sh"'
        normalization = "      - name: Normalize accepted a25 GCC frame warning option"
        self.assertIn(f"{installer}\n{normalization}", build)
        normalized = build.split(normalization, 1)[1].split(
            "      - name: Configure Windows build paths", 1
        )[0]
        self.assertIn("invalid_option=-Wno-error=frame-larger-than", normalized)
        self.assertIn("corrected_option=-Wno-error=frame-larger-than=", normalized)
        self.assertIn('test "$replacements" -eq 1', normalized)
        self.assertEqual(normalized.count("*) exit 1 ;;"), 2)
        self.assertIn('"$CC" "$corrected_option" -x c -c', normalized)
        self.assertIn('>> "$GITHUB_ENV"', normalized)
        self.assertNotIn("|| true", normalized)

    def test_windows_runs_exact_11_applicable_package_tests(self) -> None:
        text = RESUME_WORKFLOW.read_text()
        build = text.split("  build-resumed-platforms:\n", 1)[1].split(
            "\n  candidate:\n", 1
        )[0]
        self.assertIn(
            "Test canonical package builder\n        if: matrix.kind != 'windows'",
            build,
        )
        windows = build.split(
            "      - name: Test exactly 11 applicable canonical package tests on Windows",
            1,
        )[1].split(
            "      - name: Test release helpers with one platform-specific preflight exemption",
            1,
        )[0]
        selected = re.findall(
            r"^            (test_[A-Za-z0-9_.]+)(?: \\)?$", windows, re.M
        )
        self.assertEqual(
            selected,
            [
                "test_archive.ResolveZstdCommandTest.test_prefers_zstd_from_path",
                "test_archive.ResolveZstdCommandTest.test_falls_back_to_dotslash_manifest",
                "test_archive.ResolveZstdCommandTest.test_errors_when_no_zstd_or_dotslash_manifest_is_available",
                "test_cargo.SourceBinariesForTargetTest.test_windows_package_with_prebuilt_entrypoint_and_helpers_builds_nothing",
                "test_cargo.SourceBinariesForTargetTest.test_missing_windows_helpers_are_built",
                "test_cargo.SourceBinariesForTargetTest.test_build_uses_prebuilt_windows_helpers_without_running_cargo",
                "test_cli.PackageVersionTest.test_lhc_provenance_requires_exact_public_identity_pair",
                "test_cli.PackageVersionTest.test_accepts_release_prerelease_and_build_versions",
                "test_cli.PackageVersionTest.test_rejects_versions_the_runtime_cannot_parse",
                "test_layout.PackageLayoutTest.test_lhc_package_extends_canonical_metadata_with_exact_provenance",
                "test_zsh.ResolveZshBinTest.test_uses_manifest_override",
            ],
        )
        for omitted in (
            "test_layout.PackageLayoutTest.test_app_server_package_places_code_mode_host_beside_entrypoint",
            "test_layout.PackageLayoutTest.test_macos_package_preserves_prebuilt_resource_binaries",
            "test_zsh.ResolveZshBinTest.test_uses_prebuilt_executable_override",
        ):
            self.assertIn(f"# - {omitted}", windows)
            self.assertNotIn(f"            {omitted}\n", windows)
        self.assertNotIn("discover", windows)
        self.assertNotIn("|| true", windows)
        self.assertNotIn("continue-on-error", build)

    def test_retained_artifacts_are_downloaded_by_id_and_hard_fenced(self) -> None:
        text = RESUME_WORKFLOW.read_text()
        boundary = text.split("  retained-boundary:\n", 1)[1].split(
            "\n  preflight-macos-bash32:\n", 1
        )[0]
        candidate = text.split("  candidate:\n", 1)[1]

        for value in (
            "32669557893",
            "97274114206",
            "9501623903",
            "9501839613",
        ):
            self.assertIn(value, boundary)
        self.assertIn("actions/artifacts/9501623903/zip", boundary)
        self.assertIn("actions/artifacts/9501839613/zip", boundary)
        self.assertIn("actions: read", boundary)
        self.assertIn("verify_fanout_resume.py", boundary)
        self.assertNotIn("--platform-root", boundary)
        self.assertIn("name: codex-lhc-retained-boundary", boundary)
        self.assertLess(
            text.index("  retained-boundary:\n"),
            text.index("  preflight-macos-bash32:\n"),
        )
        self.assertIn("needs: [retained-boundary, build-resumed-platforms]", candidate)
        self.assertNotIn("gh api", candidate)
        self.assertIn("verify_fanout_resume.py", candidate)
        self.assertIn("name: codex-lhc-retained-boundary", candidate)
        self.assertIn("--seed-artifact-json", candidate)
        self.assertIn("--qualification-artifact-json", candidate)
        self.assertIn("--platform-root fanout", candidate)
        self.assertNotIn("scripts/build_codex_package.py", candidate)
        self.assertEqual(
            candidate.count("codex-lhc-v${VERSION}-linux-x86_64.tar.gz"), 1
        )

        verifier = (ROOT / "scripts/lhc-release/verify_fanout_resume.py").read_text()
        for value in (
            "356551728",
            "4873a72d9ee80725dc51c0e503792291bee71d9a63aefb29d6c841cb2e0d9356",
            "219676",
            "cadef3e1e4dcf3da2fb74ef648409c219dee9f447065f1ff861e17dc1f1ce11f",
        ):
            self.assertIn(value, verifier)
        self.assertIn('artifact.get("expired") is False', verifier)
        self.assertIn('job.get("conclusion") == "success"', verifier)

    def test_candidate_metadata_preserves_dual_source_identity(self) -> None:
        text = RESUME_WORKFLOW.read_text()
        candidate = text.split("  candidate:\n", 1)[1]

        self.assertIn("PRODUCT_SOURCE: a25a81a8d7d6cbd6234011ad3787cd7e59a1f489", text)
        self.assertIn("WORKFLOW_SOURCE: ${{ github.sha }}", text)
        self.assertIn('--source-commit "$PRODUCT_SOURCE"', candidate)
        self.assertIn('--workflow-source "$WORKFLOW_SOURCE"', candidate)
        self.assertIn("fanout-resume-receipt.json", candidate)
        self.assertIn("Resumed orchestration workflow source", candidate)
        self.assertIn("sha256sum -c SHA256SUMS", candidate)
        self.assertNotIn("softprops/action-gh-release", text)
        self.assertNotIn("gh release", text)
        for forbidden in ("daytona", "dgx", "burn-in", "gnome", "git tag"):
            self.assertNotIn(forbidden, text.lower())


if __name__ == "__main__":
    unittest.main()
