#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import http.server
import os
from pathlib import Path
import platform
import shutil
import socketserver
import subprocess
import tarfile
import tempfile
import threading
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / "scripts/lhc-release/install.sh"
VERSION = (ROOT / "lhc-release/VERSION").read_text(encoding="utf-8").strip()


def native_unix_platform() -> str:
    system = platform.system()
    machine = platform.machine().lower()
    if system == "Linux" and machine in {"x86_64", "amd64"}:
        return "linux-x86_64"
    if system == "Linux" and machine in {"aarch64", "arm64"}:
        return "linux-aarch64"
    if system == "Darwin" and machine in {"arm64", "aarch64"}:
        return "macos-aarch64"
    raise RuntimeError(f"unsupported POSIX installer fixture host: {system} {machine}")


def asset_name(version: str) -> str:
    return f"codex-lhc-v{version}-{native_unix_platform()}.tar.gz"


ASSET = asset_name(VERSION)


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, _format: str, *_args: object) -> None:
        pass


class InstallTest(unittest.TestCase):
    def test_linux_aarch64_uses_native_release_fixture(self) -> None:
        with (
            mock.patch.object(platform, "system", return_value="Linux"),
            mock.patch.object(platform, "machine", return_value="aarch64"),
        ):
            self.assertEqual(native_unix_platform(), "linux-aarch64")

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        release = self.root / "release"
        release.mkdir()
        self.add_release(VERSION)

        handler = lambda *args, **kwargs: QuietHandler(  # noqa: E731
            *args, directory=str(release), **kwargs
        )
        self.server = socketserver.TCPServer(("127.0.0.1", 0), handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

        curl = self.root / "bin" / "curl"
        curl.parent.mkdir()
        real_curl = shutil.which("curl")
        assert real_curl
        curl.write_text(
            f'#!/bin/sh\nexec "{real_curl}" "$@" | cat\n',
            encoding="utf-8",
        )
        curl.chmod(0o755)

    def add_release(self, version: str) -> None:
        """Publish a fixture release for `version` on the local server."""
        release = self.root / "release"
        payload = self.root / f"payload-{version}"
        (payload / "bin").mkdir(parents=True)
        for name in ("codex", "codex-code-mode-host"):
            path = payload / "bin" / name
            path.write_text("#!/bin/sh\nprintf '%s\\n' fixture\n", encoding="utf-8")
            path.chmod(0o755)
        (payload / "codex-package.json").write_text(
            f'{{"version":"{version}","lhc":{{"sdkCommit":"test-pin"}}}}\n',
            encoding="utf-8",
        )
        asset = asset_name(version)
        with tarfile.open(release / asset, "w:gz") as archive:
            for child in payload.iterdir():
                archive.add(child, arcname=child.name)
        digest = hashlib.sha256((release / asset).read_bytes()).hexdigest()
        with (release / "SHA256SUMS").open("a", encoding="utf-8") as sums:
            sums.write(f"{digest}  {asset}\n")

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.temp.cleanup()

    def run_installer(
        self, *args: str, path_prefix: str = "", version: str = VERSION
    ) -> subprocess.CompletedProcess[str]:
        prefix = self.root / "prefix"
        store = self.root / "store"
        env = os.environ.copy()
        env["HOME"] = str(self.root / "home")
        env["PATH"] = f"{path_prefix}{self.root / 'bin'}:/usr/bin:/bin"
        env["CODEX_LHC_PREFIX"] = str(prefix)
        env["CODEX_LHC_INSTALL_ROOT"] = str(store)
        env["CODEX_LHC_REPOSITORY"] = "fixture/repo"
        # Rewrite GitHub download URLs through a tiny curl shim.
        shim = self.root / "bin" / "curl"
        real_curl = shutil.which("curl")
        assert real_curl
        base = f"http://127.0.0.1:{self.server.server_address[1]}"
        shim.write_text(
            "#!/bin/sh\n"
            'args=""\n'
            'for arg in "$@"; do\n'
            '  case "$arg" in\n'
            f'    https://github.com/*) arg="{base}/$(basename "$arg")" ;;\n'
            "  esac\n"
            '  args="$args $(printf %s "$arg" | sed "s/\'/\'\\\\\'\'/g")"\n'
            "done\n"
            f'eval exec "{real_curl}" $args\n',
            encoding="utf-8",
        )
        shim.chmod(0o755)
        return subprocess.run(
            ["sh", str(INSTALLER), "--version", version, *args],
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    def test_installs_as_codex_lhc_by_default_and_uninstalls_safely(self) -> None:
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        command = self.root / "prefix/bin/codex-lhc"
        self.assertTrue(command.is_symlink())
        self.assertFalse((self.root / "prefix/bin/codex").exists())
        self.assertEqual(
            (self.root / "store/installed-name").read_text(encoding="utf-8").strip(),
            "codex-lhc",
        )
        self.assertIn("LHC engine updated to test-pin", result.stdout)
        self.assertEqual(
            subprocess.check_output([command], text=True).strip(), "fixture"
        )

        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"v{VERSION} -> v{VERSION}", result.stdout)
        self.assertTrue(command.is_symlink())

        result = self.run_installer("--uninstall")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(command.exists())
        self.assertFalse((self.root / "store").exists())

    def test_existing_stock_codex_is_preserved_beside_default_name(self) -> None:
        stock = self.root / "prefix/bin/codex"
        stock.parent.mkdir(parents=True)
        stock.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        stock.chmod(0o755)
        result = self.run_installer(path_prefix=f"{stock.parent}:")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "prefix/bin/codex-lhc").is_symlink())
        self.assertFalse(stock.is_symlink())
        self.assertEqual(stock.read_text(encoding="utf-8"), "#!/bin/sh\nexit 0\n")

    def test_stored_name_survives_rerun_without_name(self) -> None:
        result = self.run_installer("--name", "codex-memory")
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "prefix/bin/codex-memory").is_symlink())
        self.assertFalse((self.root / "prefix/bin/codex-lhc").exists())

    def test_command_conflict_is_refused_before_switching_package(self) -> None:
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        store = self.root / "store"
        current = store / "current"
        self.assertEqual(current.resolve(), (store / "versions" / VERSION).resolve())

        stock = self.root / "prefix/bin/codex"
        self.assertFalse(stock.exists())
        stock.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        stock.chmod(0o755)
        newer = "0.0.0-conflict"
        self.add_release(newer)
        result = self.run_installer("--name", "codex", version=newer)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("choose another name with --name", result.stderr)

        self.assertEqual(current.resolve(), (store / "versions" / VERSION).resolve())
        self.assertFalse((store / "versions" / newer).exists())
        self.assertEqual(
            (store / "installed-version").read_text(encoding="utf-8").strip(), VERSION
        )
        self.assertEqual(
            (store / "installed-name").read_text(encoding="utf-8").strip(), "codex-lhc"
        )
        self.assertFalse(stock.is_symlink())
        self.assertTrue((self.root / "prefix/bin/codex-lhc").is_symlink())

    def test_custom_name(self) -> None:
        result = self.run_installer("--name", "codex-memory")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "prefix/bin/codex-memory").is_symlink())

    def test_uninstall_refuses_unmanaged_store(self) -> None:
        store = self.root / "store"
        store.mkdir()
        sentinel = store / "keep-me"
        sentinel.write_text("owned by user", encoding="utf-8")
        result = self.run_installer("--name", "codex-lhc", "--uninstall")
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(sentinel.exists())


if __name__ == "__main__":
    unittest.main()
