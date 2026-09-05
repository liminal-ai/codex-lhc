"""Check tripwire exit status against disposable Git repositories in CI."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


COMMON = Path(__file__).with_name("lhc-tripwire-common.sh").resolve()


class TripwireTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.origin = self.root / "origin"
        self.repo = self.root / "checkout"
        self.env = {
            **os.environ,
            "TMPDIR": str(self.root),
            "GIT_AUTHOR_NAME": "Tripwire test",
            "GIT_AUTHOR_EMAIL": "test@example.invalid",
            "GIT_COMMITTER_NAME": "Tripwire test",
            "GIT_COMMITTER_EMAIL": "test@example.invalid",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
        }
        self.git("init", "-q", "-b", "main", str(self.origin))
        self.git("-C", str(self.origin), "commit", "-q", "--allow-empty", "-m", "base")
        self.git("clone", "-q", str(self.origin), str(self.repo))

    def git(self, *args):
        return subprocess.run(
            ["git", *args], env=self.env, check=True, capture_output=True, text=True
        )

    def run_check(self, prior_failure=0):
        return subprocess.run(
            [
                "bash",
                "-c",
                """set -u
source "$1"
lhc_tripwire_logs || exit 1
pin_status=0
lhc_check_pin "$2" || pin_status=$?
failed="$3"
if [ "$pin_status" -eq 1 ]; then failed=1; fi
lhc_report_result "$failed" "$pin_status"
""",
                "tripwire-test",
                str(COMMON),
                str(self.repo),
                str(prior_failure),
            ],
            env=self.env,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_verified_pin_passes(self):
        self.assertEqual(self.run_check().returncode, 0)

    def test_offline_preserves_warning_exit_policy(self):
        self.origin.rename(self.root / "offline")
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_off_main_pin_warns_without_changing_existing_exit_policy(self):
        self.git("-C", str(self.repo), "commit", "-q", "--allow-empty", "-m", "fork")
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_bad_local_repository_fails(self):
        self.repo = self.root / "missing"
        result = self.run_check()
        self.assertEqual(result.returncode, 1)

    def test_prior_layer_failure_survives_successful_or_unverified_pin(self):
        for offline in (False, True):
            with self.subTest(offline=offline):
                if offline:
                    self.origin.rename(self.root / "offline")
                result = self.run_check(prior_failure=1)
                self.assertEqual(result.returncode, 1)


if __name__ == "__main__":
    unittest.main()
