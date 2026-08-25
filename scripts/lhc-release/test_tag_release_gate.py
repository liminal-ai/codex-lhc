"""Regression tests for tag_release_gate.sh.

The fake gh reproduces real gh behavior exactly: on a missing object it prints
the API 404 JSON error body to stdout and exits 1 (the behavior that broke the
original inline gate); on an existing object it prints the object JSON and
exits 0.
"""

import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

GATE = Path(__file__).with_name("tag_release_gate.sh")

CANDIDATE_SHA = "3aa12d7c00bb656c92311003c17c7585b179502b"

FAKE_GH = """#!/usr/bin/env bash
# Fake gh driven by FAKE_TAG_EXISTS / FAKE_RELEASE_EXISTS.
case "$1" in
  api)
    case "$2" in
      */git/refs/tags/*)
        if [ "${FAKE_TAG_EXISTS:-0}" = 1 ]; then
          printf '{"object":{"sha":"%s","type":"commit"}}' "$FAKE_TAG_SHA"
          exit 0
        fi
        printf '{"message":"Not Found","documentation_url":"x","status":"404"}'
        exit 1
        ;;
    esac
    ;;
  release)
    if [ "${FAKE_RELEASE_EXISTS:-0}" = 1 ]; then
      printf '{"tagName":"v0.149.1","targetCommitish":"%s"}' "$FAKE_RELEASE_SHA"
      exit 0
    fi
    exit 1
    ;;
esac
echo "fake gh: unhandled args: $*" >&2
exit 64
"""


class TagReleaseGateTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        fake = Path(self.tmp.name) / "gh"
        fake.write_text(FAKE_GH)
        fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
        self.env = dict(
            os.environ,
            GH_BIN=str(fake),
            GITHUB_REPOSITORY="liminal-ai/codex-lhc",
            FAKE_TAG_SHA=CANDIDATE_SHA,
            FAKE_RELEASE_SHA=CANDIDATE_SHA,
        )

    def tearDown(self):
        self.tmp.cleanup()

    def run_gate(self, mode, *, tag_exists, release_exists):
        env = dict(
            self.env,
            FAKE_TAG_EXISTS="1" if tag_exists else "0",
            FAKE_RELEASE_EXISTS="1" if release_exists else "0",
        )
        return subprocess.run(
            ["bash", str(GATE), mode, "0.149.1", CANDIDATE_SHA],
            env=env,
            capture_output=True,
            text=True,
        )

    def test_forward_missing_tag_and_release_passes(self):
        result = self.run_gate("forward", tag_exists=False, release_exists=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("forward: no tag or release", result.stdout)

    def test_forward_existing_tag_rejects(self):
        result = self.run_gate("forward", tag_exists=True, release_exists=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("forbids existing tag", result.stderr)

    def test_forward_existing_release_rejects(self):
        result = self.run_gate("forward", tag_exists=False, release_exists=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("forbids existing release", result.stderr)

    def test_backfill_mode_is_not_supported(self):
        result = self.run_gate("backfill", tag_exists=False, release_exists=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("mode must be forward", result.stderr)


if __name__ == "__main__":
    unittest.main()
