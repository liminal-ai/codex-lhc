from __future__ import annotations

import json
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

from verify_package_archive import PLATFORMS, verify


SDK = "b408f89712cbbb525dbfc2f7b2c51ab3133c4f45"


def fixture(root: Path, platform: str) -> Path:
    target, suffix = PLATFORMS[platform]
    package = root / "package"
    required = [
        f"bin/codex{suffix}",
        f"bin/codex-code-mode-host{suffix}",
        f"codex-path/rg{suffix}",
    ]
    if platform.startswith("linux-"):
        required += ["codex-resources/bwrap", "codex-resources/zsh/bin/zsh"]
    if platform.startswith("windows-"):
        required += [
            "codex-resources/codex-command-runner.exe",
            "codex-resources/codex-windows-sandbox-setup.exe",
        ]
    for name in required:
        path = package / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"fixture")
    metadata = {
        "layoutVersion": 1,
        "version": "0.149.2",
        "target": target,
        "variant": "codex",
        "entrypoint": f"bin/codex{suffix}",
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
        "lhc": {
            "repository": "https://github.com/liminal-ai/long-horizon-context",
            "sdkCommit": SDK,
            "threadSchema": 12,
        },
    }
    (package / "codex-package.json").write_text(json.dumps(metadata))
    if platform.startswith("windows-"):
        archive_path = root / "package.zip"
        with zipfile.ZipFile(archive_path, "w") as archive:
            for path in package.rglob("*"):
                if path.is_file():
                    archive.write(path, path.relative_to(package))
    else:
        archive_path = root / "package.tar.gz"
        with tarfile.open(archive_path, "w:gz") as archive:
            for path in package.rglob("*"):
                archive.add(path, arcname=path.relative_to(package), recursive=False)
    return archive_path


class PackageContractTests(unittest.TestCase):
    def test_exact_five_platform_contract(self) -> None:
        self.assertEqual(
            list(PLATFORMS),
            [
                "linux-x86_64",
                "linux-aarch64",
                "windows-x86_64",
                "windows-aarch64",
                "macos-aarch64",
            ],
        )
        for platform in PLATFORMS:
            with self.subTest(platform=platform), tempfile.TemporaryDirectory() as tmp:
                verify(fixture(Path(tmp), platform), platform, "0.149.2", SDK)

    def test_missing_linux_zsh_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            archive = fixture(root, "linux-x86_64")
            package = root / "package"
            (package / "codex-resources/zsh/bin/zsh").unlink()
            with tarfile.open(archive, "w:gz") as output:
                for path in package.rglob("*"):
                    output.add(path, arcname=path.relative_to(package), recursive=False)
            with self.assertRaisesRegex(ValueError, "zsh"):
                verify(archive, "linux-x86_64", "0.149.2", SDK)


if __name__ == "__main__":
    unittest.main()
