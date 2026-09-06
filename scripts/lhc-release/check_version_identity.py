#!/usr/bin/env python3
"""Validate that release, workspace, and CLI version identities agree.

The fork release is either the bare upstream version (``0.153.3``) or that
version with a fork revision suffix (``0.153.3-lhc.1``). The full release must
match lhc-release/VERSION; its numeric upstream base must match the Rust
workspace version, which the CLI inherits through version.workspace = true.
"""

import argparse
import re
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RELEASE_RE = re.compile(
    r"^(?P<upstream>(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*))"
    r"(?:-lhc\.(0|[1-9][0-9]*))?$"
)


def read_toml(path: Path) -> dict:
    with path.open("rb") as stream:
        return tomllib.load(stream)


def upstream_base(release: str) -> str:
    match = RELEASE_RE.fullmatch(release)
    if match is None:
        raise ValueError(
            f"release version must be X.Y.Z or X.Y.Z-lhc.N, got {release!r}"
        )
    return match.group("upstream")


def validate_version_identity(requested_release: str, root: Path = ROOT) -> str:
    upstream = upstream_base(requested_release)

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
    expected = {
        "lhc-release/VERSION": (release_version, requested_release),
        "Rust workspace version": (workspace_version, upstream),
    }
    mismatches = {
        name: actual for name, (actual, wanted) in expected.items() if actual != wanted
    }
    if mismatches:
        details = ", ".join(f"{name}={value!r}" for name, value in mismatches.items())
        raise ValueError(
            f"release identity mismatch for {requested_release!r} "
            f"(upstream base {upstream!r}): {details}"
        )
    return upstream


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True, help="full fork release")
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    try:
        upstream = validate_version_identity(args.version, args.root)
    except (KeyError, OSError, ValueError) as error:
        raise SystemExit(str(error)) from error
    print(f"release identity aligned at {args.version} (upstream base {upstream})")


if __name__ == "__main__":
    main()
