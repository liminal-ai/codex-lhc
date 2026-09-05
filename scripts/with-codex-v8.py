#!/usr/bin/env python3
"""Run a local build/check with the same verified V8 pair as package builds."""

import argparse
import os
from pathlib import Path
import subprocess


def main():
    os.environ["CODEX_REPO_ROOT"] = str(Path(__file__).resolve().parents[1])
    from codex_package.targets import TARGET_SPECS, default_target
    from codex_package.v8 import resolve_codex_v8_cargo_env

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGET_SPECS, default=default_target())
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        parser.error("provide a command, e.g. just test -p codex-code-mode-runtime")
    env = dict(os.environ)
    env.update(resolve_codex_v8_cargo_env(TARGET_SPECS[args.target], environ=env))
    return subprocess.run(command, env=env, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
