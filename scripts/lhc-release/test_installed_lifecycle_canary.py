from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-lhc-installed-lifecycle.py"
SPEC = importlib.util.spec_from_file_location("installed_lifecycle", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class InstalledLifecycleCanaryTests(unittest.TestCase):
    def test_canary_drives_binary_and_validates_compact_resume_reconciliation(
        self,
    ) -> None:
        source = SCRIPT.read_text()
        self.assertIn("run_command(", source)
        self.assertIn(
            "installed launcher did not produce an LHC Compact rewrite", source
        )
        self.assertIn("installed launcher did not resume after Compact", source)
        self.assertIn("rollout.unlink()", source)
        self.assertIn("did not reconcile materialized Compact history", source)
        self.assertIn('environment["LHC_COMPACT_ALGORITHM"] = "legacy"', source)
        self.assertIn(
            "default installed lifecycle unexpectedly selected legacy", source
        )

    def test_rollout_inspection_requires_one_compact_with_bands(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            rollout.write_text(
                "\n".join(
                    [
                        json.dumps({"type": "session_meta", "payload": {"id": "t"}}),
                        json.dumps(
                            {
                                "type": "compacted",
                                "payload": {
                                    "replacement_history": [{"type": "message"}]
                                },
                            }
                        ),
                    ]
                )
                + "\n"
            )
            self.assertEqual(MODULE.inspect_rollout(rollout), (1, 1))

    def test_session_id_reads_real_exec_event_shape(self) -> None:
        output = json.dumps({"type": "thread.started", "thread_id": "thread-1"})
        self.assertEqual(MODULE.session_id(output), "thread-1")


if __name__ == "__main__":
    unittest.main()
