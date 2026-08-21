#!/usr/bin/env python3
"""Validate that release, workspace, and CLI version identities agree."""

import argparse
import re
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SEMVER_RE = re.compile(
    r"^(0|[1-9][0-9]*)\."
    r"(0|[1-9][0-9]*)\."
    r"(0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)


def read_toml(path: Path) -> dict:
    with path.open("rb") as stream:
        return tomllib.load(stream)


def validate_version_identity(requested_version: str, root: Path = ROOT) -> str:
    if not SEMVER_RE.fullmatch(requested_version):
        raise ValueError(f"release version is not SemVer: {requested_version!r}")

    release_version = (root / "lhc-release/VERSION").read_text(encoding="utf-8").strip()
    workspace_version = read_toml(root / "codex-rs/Cargo.toml")["workspace"]["package"][
        "version"
    ]
    cli_version = read_toml(root / "codex-rs/cli/Cargo.toml")["package"]["version"]

    if cli_version != {"workspace": True}:
        raise ValueError(
            "codex-rs/cli/Cargo.toml must declare version.workspace = true, "
            f"got {cli_version!r}"
        )
    identities = {
        "requested release version": requested_version,
        "lhc-release/VERSION": release_version,
        "Rust workspace version": workspace_version,
    }
    mismatches = {
        name: value for name, value in identities.items() if value != requested_version
    }
    if mismatches:
        details = ", ".join(f"{name}={value!r}" for name, value in mismatches.items())
        raise ValueError(f"release identity mismatch: {details}")
    return requested_version


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    try:
        version = validate_version_identity(args.version, args.root)
    except (KeyError, OSError, ValueError) as error:
        raise SystemExit(str(error)) from error
    print(f"release identity aligned at {version}")


if __name__ == "__main__":
    main()
