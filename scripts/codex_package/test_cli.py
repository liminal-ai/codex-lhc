#!/usr/bin/env python3

import argparse
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package.cli import parse_package_version
from codex_package.cli import parse_sha
from codex_package.cli import resolve_lhc_provenance


class PackageVersionTest(unittest.TestCase):
    def test_lhc_provenance_requires_exact_public_identity_pair(self) -> None:
        provenance = resolve_lhc_provenance(
            "9d4d18247942f35b356f28bdc4288f0e631a6da9", 11
        )
        self.assertEqual(
            provenance.sdk_commit, "9d4d18247942f35b356f28bdc4288f0e631a6da9"
        )
        with self.assertRaises(RuntimeError):
            resolve_lhc_provenance("9d4d18247942f35b356f28bdc4288f0e631a6da9", None)
        with self.assertRaises(argparse.ArgumentTypeError):
            parse_sha("9d4d182")

    def test_accepts_release_prerelease_and_build_versions(self) -> None:
        for version in (
            "0.0.0",
            "1.2.3",
            "0.0.0-internal.deadbeef",
            "1.2.3-alpha.1+build.01",
            "18446744073709551615.0.0",
        ):
            with self.subTest(version=version):
                self.assertEqual(parse_package_version(version), version)

    def test_rejects_versions_the_runtime_cannot_parse(self) -> None:
        for version in (
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-",
            "1.2.3-alpha..1",
            "1.2.3-01",
            "1.2.3+",
            "1.2.3+build..1",
            "18446744073709551616.0.0",
        ):
            with self.subTest(version=version):
                with self.assertRaises(argparse.ArgumentTypeError):
                    parse_package_version(version)


if __name__ == "__main__":
    unittest.main()
