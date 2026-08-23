import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / ".github/scripts/install-musl-build-tools.sh"
LINUX_UAPI_VERSION = "6.8.0-25.25cross1"


class InstallMuslBuildToolsTests(unittest.TestCase):
    def test_empty_apt_argument_arrays_use_bash_32_safe_expansion(self) -> None:
        text = INSTALLER.read_text(encoding="utf-8")
        self.assertIn(
            '${apt_update_args[@]+"${apt_update_args[@]}"}',
            text,
        )
        self.assertIn(
            '${apt_install_args[@]+"${apt_install_args[@]}"}',
            text,
        )
        self.assertNotIn('update "${apt_update_args[@]}"', text)
        self.assertNotIn('install -y "${apt_install_args[@]}"', text)

    def test_each_target_uses_pinned_uapi_closure_after_musl_and_probes_it(
        self,
    ) -> None:
        targets = {
            "x86_64-unknown-linux-musl": {
                "arch": "x86_64",
                "macro": "__x86_64__",
                "package": (f"linux-libc-dev-amd64-cross_{LINUX_UAPI_VERSION}_all.deb"),
                "sha256": (
                    "bc504dcc35c15ff606df44ca081d0abaa613b2f3b7a56896d6211eced1368af3"
                ),
            },
            "aarch64-unknown-linux-musl": {
                "arch": "aarch64",
                "macro": "__aarch64__",
                "package": (f"linux-libc-dev-arm64-cross_{LINUX_UAPI_VERSION}_all.deb"),
                "sha256": (
                    "6a5a00b8ba8de66862e05493def87a5cbbb23b949601e45a5e3cbde56505cb6b"
                ),
            },
        }

        for target, expected in targets.items():
            with self.subTest(target=target), tempfile.TemporaryDirectory() as temp:
                temp_root = Path(temp)
                fake_bin = temp_root / "fake-bin"
                fake_bin.mkdir()
                command_log = temp_root / "commands.log"
                compiler_args = temp_root / "compiler-args.log"
                probe_source = temp_root / "probe.c"
                github_env = temp_root / "github-env"

                self._write_executable(
                    fake_bin / "sudo",
                    '#!/bin/sh\nprintf "%s\\n" "$*" >> "$COMMAND_LOG"\n',
                )
                self._write_executable(
                    fake_bin / "sha256sum",
                    "#!/bin/sh\ncat >/dev/null\n",
                )
                compiler = fake_bin / f"{expected['arch']}-linux-musl-gcc"
                self._write_executable(
                    compiler,
                    "#!/bin/sh\n"
                    'printf "%s\\n" "$*" >> "$COMPILER_ARGS"\n'
                    'cat > "$PROBE_SOURCE"\n',
                )

                tool_root = temp_root / f"codex-musl-tools-{target}"
                uapi_root = (
                    tool_root
                    / f"linux-uapi-{LINUX_UAPI_VERSION}-{expected['sha256']}"
                    / "usr"
                    / f"{expected['arch']}-linux-gnu"
                    / "include"
                )
                for header in ("linux/sched.h", "linux/loop.h", "asm/types.h"):
                    path = uapi_root / header
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.touch()
                (tool_root / expected["package"]).touch()
                libcap = tool_root / "libcap-2.75/prefix/lib/libcap.a"
                libcap.parent.mkdir(parents=True)
                libcap.touch()

                env = {
                    **os.environ,
                    "PATH": f"{fake_bin}:/usr/bin:/bin",
                    "TARGET": target,
                    "GITHUB_ENV": str(github_env),
                    "RUNNER_TEMP": str(temp_root),
                    "COMMAND_LOG": str(command_log),
                    "COMPILER_ARGS": str(compiler_args),
                    "PROBE_SOURCE": str(probe_source),
                }
                subprocess.run(["bash", str(INSTALLER)], env=env, check=True)

                include_flag = f"-idirafter{uapi_root}"
                exported = github_env.read_text()
                self.assertIn(f"CFLAGS=-pthread {include_flag}", exported)
                self.assertIn(f"CXXFLAGS=-pthread {include_flag}", exported)
                self.assertNotIn("-I/usr/include", exported)
                self.assertIn(include_flag, compiler_args.read_text())

                source = probe_source.read_text()
                self.assertIn("#include <linux/sched.h>", source)
                self.assertIn("#include <linux/loop.h>", source)
                self.assertIn("#include <asm/types.h>", source)
                self.assertIn(f"#ifndef {expected['macro']}", source)

                apt_commands = command_log.read_text()
                self.assertIn("apt-get update", apt_commands)
                self.assertIn("apt-get install -y", apt_commands)
                self.assertNotIn("linux-libc-dev", apt_commands)

    @staticmethod
    def _write_executable(path: Path, content: str) -> None:
        path.write_text(content)
        path.chmod(0o755)


if __name__ == "__main__":
    unittest.main()
