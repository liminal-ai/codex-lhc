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
    archive = candidate / f"codex-lhc-v{version}-linux-x86_64.tar.gz"
    capture_probe = Path(__file__).resolve().parents[1] / "check-lhc-default-capture.py"
    required = [archive, candidate / "install.sh", candidate / "SHA256SUMS", candidate / "release-manifest.json"]
    for path in required:
        if not path.is_file():
            fail(f"candidate is missing {path.name}")
    if not capture_probe.is_file():
        fail(f"smoke checkout is missing {capture_probe}")

    daytona = Daytona()
    sandbox = daytona.create(
        CreateSandboxFromSnapshotParams(
            snapshot=os.environ.get("DAYTONA_LINUX_SNAPSHOT", "daytona-medium"),
            labels={
                "purpose": "codex-lhc-release-smoke",
                "platform": "linux",
                "candidate-run": os.environ.get("GITHUB_RUN_ID", "local"),
            },
            auto_stop_interval=15,
            ephemeral=True,
        ),
        timeout=float(os.environ.get("DAYTONA_CREATE_TIMEOUT", "240")),
    )
    print(f"OK: created Linux sandbox {sandbox.id}")
    try:
        remote = "/tmp/codex-lhc-candidate"
        expect_success(sandbox.process.exec(f"mkdir -p {remote}"), "create candidate directory")
        sandbox.fs.upload_files(
            [FileUpload(source=str(path), destination=f"{remote}/{path.name}") for path in required]
            + [FileUpload(source=str(capture_probe), destination=f"{remote}/{capture_probe.name}")]
        )
        command = (
            f"HOME=/tmp/lhc-home CODEX_HOME=/tmp/lhc-data CODEX_LHC_ROOT=/tmp/lhc-data/lhc "
            f"sh {remote}/install.sh --version {version} --name codex-lhc-smoke "
            f"--prefix /tmp/lhc-prefix --install-root /tmp/lhc-packages --asset-dir {remote}"
        )
        expect_success(sandbox.process.exec(command, timeout=180), "Linux install")
        expect_success(sandbox.process.exec("/tmp/lhc-prefix/bin/codex-lhc-smoke --version"), "codex --version")
        expect_success(sandbox.process.exec("/tmp/lhc-prefix/bin/codex-lhc-smoke --help >/tmp/codex-help.txt"), "codex --help")
        expect_success(
            sandbox.process.exec(f"/tmp/lhc-packages/versions/{version}/bin/codex-code-mode-host --help >/tmp/host-help.txt"),
            "code-mode host --help",
        )
        expect_success(
            sandbox.process.exec(
                f"python3 {remote}/{capture_probe.name} --binary /tmp/lhc-prefix/bin/codex-lhc-smoke",
                timeout=120,
            ),
            "bare installed Codex captures a complete LHC turn",
        )
        expect_success(sandbox.process.exec("mkdir -p /tmp/lhc-data/lhc && echo preserve >/tmp/lhc-data/lhc/smoke-marker"), "create data marker")
        expect_success(
            sandbox.process.exec(
                "HOME=/tmp/lhc-home sh /tmp/codex-lhc-candidate/install.sh --name codex-lhc-smoke "
                "--prefix /tmp/lhc-prefix --install-root /tmp/lhc-packages --uninstall"
            ),
            "Linux uninstall",
        )
        expect_success(
            sandbox.process.exec(
                "test ! -e /tmp/lhc-prefix/bin/codex-lhc-smoke && "
                "test ! -e /tmp/lhc-packages && test -f /tmp/lhc-data/lhc/smoke-marker"
            ),
            "Linux cleanup and data preservation",
        )
        print("SMOKE_PASS linux")
    finally:
        daytona.delete(sandbox, timeout=120, wait=True)
        print(f"OK: deleted Linux sandbox {sandbox.id}")


if __name__ == "__main__":
    main()
