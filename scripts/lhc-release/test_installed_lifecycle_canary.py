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
        self.assertNotIn('"lhc_capture"', source)
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
                                    "message": "lhc_compact_durable {}",
                                    "window_number": 1,
                                    "replacement_history": [{"type": "message"}],
                                },
                            }
                        ),
                    ]
                )
                + "\n"
            )
            inspection = MODULE.inspect_rollout(rollout)
            self.assertEqual(inspection, (1, 1, 1))
            self.assertTrue(MODULE.qualifies_lhc_boundary(inspection, True))

    def test_rollout_inspection_rejects_native_compacted_record(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            rollout.write_text(
                json.dumps(
                    {
                        "type": "compacted",
                        "payload": {
                            "message": "",
                            "window_number": 1,
                            "replacement_history": [{"type": "message"}],
                        },
                    }
                )
                + "\n"
            )
            inspection = MODULE.inspect_rollout(rollout)
            self.assertEqual(inspection, (0, 0, None))
            self.assertFalse(MODULE.qualifies_lhc_boundary(inspection, True))

    def test_boundary_requires_history_window_and_prev(self) -> None:
        self.assertFalse(MODULE.qualifies_lhc_boundary((1, 0, 1), True))
        self.assertFalse(MODULE.qualifies_lhc_boundary((1, 1, None), True))
        self.assertFalse(MODULE.qualifies_lhc_boundary((1, 1, 1), False))
        self.assertFalse(MODULE.qualifies_lhc_boundary((2, 1, 1), True))

    def test_session_id_reads_real_exec_event_shape(self) -> None:
        output = json.dumps({"type": "thread.started", "thread_id": "thread-1"})
        self.assertEqual(MODULE.session_id(output), "thread-1")


if __name__ == "__main__":
    unittest.main()
