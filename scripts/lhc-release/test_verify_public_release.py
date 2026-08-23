from __future__ import annotations

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from verify_public_release import expected_asset_names
from verify_public_release import validate_public_state
from verify_public_release import verify_downloaded_assets


VERSION = "0.149.0"
SOURCE = "a" * 40
WORKFLOW = "b" * 40


def release(*, resumed: bool = False, workflow_source: str = SOURCE) -> dict:
    return {
        "tag_name": f"v{VERSION}",
        "draft": False,
        "prerelease": False,
        "body": (f"- Product source: {SOURCE}\n- Workflow source: {workflow_source}\n"),
        "assets": [
            {"name": name, "browser_download_url": f"https://example.test/{name}"}
            for name in sorted(expected_asset_names(VERSION, resumed=resumed))
        ],
    }


class PublicReleasePostconditionTests(unittest.TestCase):
    def test_exact_public_identity_and_assets_pass(self) -> None:
        urls = validate_public_state(
            tag_sha=SOURCE,
            release=release(),
            version=VERSION,
            product_source=SOURCE,
            workflow_source=SOURCE,
        )
        self.assertEqual(set(urls), expected_asset_names(VERSION))

    def test_resumed_public_identity_tags_product_and_discloses_workflow(self) -> None:
        urls = validate_public_state(
            tag_sha=SOURCE,
            release=release(resumed=True, workflow_source=WORKFLOW),
            version=VERSION,
            product_source=SOURCE,
            workflow_source=WORKFLOW,
        )
        self.assertEqual(set(urls), expected_asset_names(VERSION, resumed=True))

    def test_wrong_tag_or_missing_asset_fails(self) -> None:
        with self.assertRaisesRegex(ValueError, "tag target"):
            validate_public_state(
                tag_sha="b" * 40,
                release=release(),
                version=VERSION,
                product_source=SOURCE,
                workflow_source=SOURCE,
            )
        incomplete = release()
        incomplete["assets"].pop()
        with self.assertRaisesRegex(ValueError, "public assets"):
            validate_public_state(
                tag_sha=SOURCE,
                release=incomplete,
                version=VERSION,
                product_source=SOURCE,
                workflow_source=SOURCE,
            )

        undisclosed = release(resumed=True, workflow_source=WORKFLOW)
        undisclosed["body"] = f"- Product source: {SOURCE}\n"
        with self.assertRaisesRegex(ValueError, "does not disclose Workflow source"):
            validate_public_state(
                tag_sha=SOURCE,
                release=undisclosed,
                version=VERSION,
                product_source=SOURCE,
                workflow_source=WORKFLOW,
            )

    def test_downloaded_bytes_and_checksums_are_hard_postconditions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            candidate = Path(tmp)
            bodies: dict[str, bytes] = {
                name: f"bytes:{name}\n".encode()
                for name in expected_asset_names(VERSION)
                if name not in {"SHA256SUMS", "release-manifest.json"}
            }
            bodies["release-manifest.json"] = json.dumps(
                {"sourceCommit": SOURCE, "productSource": SOURCE}
            ).encode()
            sums = "".join(
                f"{hashlib.sha256(body).hexdigest()}  {name}\n"
                for name, body in sorted(bodies.items())
            ).encode()
            bodies["SHA256SUMS"] = sums
            for name, body in bodies.items():
                (candidate / name).write_bytes(body)
            verify_downloaded_assets(
                candidate,
                bodies,
                product_source=SOURCE,
                workflow_source=SOURCE,
            )
            bodies["install.sh"] = b"tampered"
            with self.assertRaisesRegex(ValueError, "candidate bytes"):
                verify_downloaded_assets(
                    candidate,
                    bodies,
                    product_source=SOURCE,
                    workflow_source=SOURCE,
                )

    def test_resumed_downloaded_manifest_refuses_workflow_relabel(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            candidate = Path(tmp)
            bodies: dict[str, bytes] = {
                name: f"bytes:{name}\n".encode()
                for name in expected_asset_names(VERSION, resumed=True)
                if name not in {"SHA256SUMS", "release-manifest.json"}
            }
            bodies["release-manifest.json"] = json.dumps(
                {
                    "sourceCommit": SOURCE,
                    "productSource": SOURCE,
                    "workflowSource": SOURCE,
                }
            ).encode()
            bodies["SHA256SUMS"] = "".join(
                f"{hashlib.sha256(body).hexdigest()}  {name}\n"
                for name, body in sorted(bodies.items())
            ).encode()
            for name, body in bodies.items():
                (candidate / name).write_bytes(body)
            with self.assertRaisesRegex(ValueError, "workflowSource mismatch"):
                verify_downloaded_assets(
                    candidate,
                    bodies,
                    product_source=SOURCE,
                    workflow_source=WORKFLOW,
                )

            bodies["release-manifest.json"] = json.dumps(
                {
                    "sourceCommit": WORKFLOW,
                    "productSource": WORKFLOW,
                    "workflowSource": WORKFLOW,
                }
            ).encode()
            bodies["SHA256SUMS"] = "".join(
                f"{hashlib.sha256(body).hexdigest()}  {name}\n"
                for name, body in sorted(bodies.items())
                if name != "SHA256SUMS"
            ).encode()
            for name, body in bodies.items():
                (candidate / name).write_bytes(body)
            with self.assertRaisesRegex(ValueError, "sourceCommit is not product"):
                verify_downloaded_assets(
                    candidate,
                    bodies,
                    product_source=SOURCE,
                    workflow_source=WORKFLOW,
                )


if __name__ == "__main__":
    unittest.main()
