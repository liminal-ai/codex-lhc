from __future__ import annotations

import hashlib
import tempfile
import unittest
from pathlib import Path

from verify_public_release import expected_asset_names
from verify_public_release import validate_public_state
from verify_public_release import verify_downloaded_assets


VERSION = "0.149.1"
SOURCE = "a" * 40


def release() -> dict:
    return {
        "tag_name": f"v{VERSION}",
        "draft": False,
        "prerelease": False,
        "assets": [
            {"name": name, "browser_download_url": f"https://example.test/{name}"}
            for name in sorted(expected_asset_names(VERSION))
        ],
    }


class PublicReleasePostconditionTests(unittest.TestCase):
    def test_exact_public_identity_and_assets_pass(self) -> None:
        urls = validate_public_state(
            tag_sha=SOURCE, release=release(), version=VERSION, source_sha=SOURCE
        )
        self.assertEqual(set(urls), expected_asset_names(VERSION))

    def test_wrong_tag_or_missing_asset_fails(self) -> None:
        with self.assertRaisesRegex(ValueError, "tag target"):
            validate_public_state(
                tag_sha="b" * 40,
                release=release(),
                version=VERSION,
                source_sha=SOURCE,
            )
        incomplete = release()
        incomplete["assets"].pop()
        with self.assertRaisesRegex(ValueError, "public assets"):
            validate_public_state(
                tag_sha=SOURCE,
                release=incomplete,
                version=VERSION,
                source_sha=SOURCE,
            )

    def test_downloaded_bytes_and_checksums_are_hard_postconditions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            candidate = Path(tmp)
            bodies = {
                name: f"bytes:{name}\n".encode()
                for name in expected_asset_names(VERSION)
                if name != "SHA256SUMS"
            }
            sums = "".join(
                f"{hashlib.sha256(body).hexdigest()}  {name}\n"
                for name, body in sorted(bodies.items())
            ).encode()
            bodies["SHA256SUMS"] = sums
            for name, body in bodies.items():
                (candidate / name).write_bytes(body)
            verify_downloaded_assets(candidate, bodies)
            bodies["install.sh"] = b"tampered"
            with self.assertRaisesRegex(ValueError, "candidate bytes"):
                verify_downloaded_assets(candidate, bodies)


if __name__ == "__main__":
    unittest.main()
