#!/usr/bin/env python3
import os
from pathlib import Path

from daytona import CreateSandboxFromSnapshotParams, Daytona, FileUpload

from common import expect_success, fail, require_candidate, require_key


def main() -> None:
    require_key()
    candidate = require_candidate()
    source_version = (
        Path(__file__).resolve().parents[2] / "lhc-release/VERSION"
    ).read_text(encoding="utf-8").strip()
    version = os.environ.get("CODEX_LHC_VERSION", source_version)
    archive = candidate / f"codex-lhc-v{version}-windows-x86_64.zip"
    capture_probe = Path(__file__).resolve().parents[1] / "check-lhc-default-capture.py"
    required = [archive, candidate / "install.ps1", candidate / "SHA256SUMS", candidate / "release-manifest.json"]
    for path in required:
        if not path.is_file():
            fail(f"candidate is missing {path.name}")
    if not capture_probe.is_file():
        fail(f"smoke checkout is missing {capture_probe}")

    daytona = Daytona()
    sandbox = daytona.create(
        CreateSandboxFromSnapshotParams(
            snapshot=os.environ.get("DAYTONA_WINDOWS_SNAPSHOT", "windows-medium"),
            labels={
                "purpose": "codex-lhc-release-smoke",
                "platform": "windows",
                "candidate-run": os.environ.get("GITHUB_RUN_ID", "local"),
            },
            auto_stop_interval=15,
            ephemeral=True,
        ),
        timeout=float(os.environ.get("DAYTONA_CREATE_TIMEOUT", "300")),
    )
    print(f"OK: created Windows sandbox {sandbox.id}")
    try:
        remote = r"C:\Users\daytona\codex-lhc-candidate"
        expect_success(sandbox.process.exec(f'powershell -NoProfile -Command "New-Item -Force -ItemType Directory \'{remote}\' | Out-Null"'), "create candidate directory")
        sandbox.fs.upload_files(
            [FileUpload(source=str(path), destination=rf"{remote}\{path.name}") for path in required]
            + [FileUpload(source=str(capture_probe), destination=rf"{remote}\{capture_probe.name}")]
        )
        install = (
            "powershell -NoProfile -ExecutionPolicy Bypass -File "
            rf'"{remote}\install.ps1" -Version {version} -Name codex-lhc-smoke '
            rf'-Prefix C:\codex-lhc-prefix -InstallRoot C:\codex-lhc-packages -AssetDir "{remote}"'
        )
        expect_success(sandbox.process.exec(install, timeout=240), "Windows install")
        expect_success(sandbox.process.exec(r"cmd /c C:\codex-lhc-prefix\bin\codex-lhc-smoke.cmd --version"), "codex --version")
        expect_success(sandbox.process.exec(r"cmd /c C:\codex-lhc-prefix\bin\codex-lhc-smoke.cmd --help > C:\codex-help.txt"), "codex --help")
        expect_success(
            sandbox.process.exec(rf"cmd /c C:\codex-lhc-packages\versions\{version}\bin\codex-code-mode-host.exe --help > C:\host-help.txt"),
            "code-mode host --help",
        )
        expect_success(
            sandbox.process.exec(
                rf"python {remote}\{capture_probe.name} --binary "
                rf"C:\codex-lhc-packages\versions\{version}\bin\codex.exe",
                timeout=120,
            ),
            "bare installed Codex captures a complete LHC turn",
        )
        expect_success(sandbox.process.exec(r'powershell -NoProfile -Command "New-Item -Force -ItemType Directory C:\codex-lhc-data | Out-Null; Set-Content C:\codex-lhc-data\smoke-marker preserve"'), "create data marker")
        uninstall = (
            "powershell -NoProfile -ExecutionPolicy Bypass -File "
            rf'"{remote}\install.ps1" -Name codex-lhc-smoke '
            r'-Prefix C:\codex-lhc-prefix -InstallRoot C:\codex-lhc-packages -Uninstall'
        )
        expect_success(sandbox.process.exec(uninstall), "Windows uninstall")
        expect_success(
            sandbox.process.exec(
                r'powershell -NoProfile -Command "if ((Test-Path C:\codex-lhc-prefix\bin\codex-lhc-smoke.cmd) '
                r'-or (Test-Path C:\codex-lhc-packages) -or -not (Test-Path C:\codex-lhc-data\smoke-marker)) { exit 1 }"'
            ),
            "Windows cleanup and data preservation",
        )
        print("SMOKE_PASS windows")
    finally:
        daytona.delete(sandbox, timeout=120, wait=True)
        print(f"OK: deleted Windows sandbox {sandbox.id}")


if __name__ == "__main__":
    main()
