"""Regression cases for what the frozen source exporter leaves behind."""

from collections import Counter
import json
import unittest

from export_codegraph import convert


class SourceEvidence(unittest.TestCase):
    def test_frozen_claim_changes_keep_history_and_strip_the_control_marker(self):
        source = {
            "sequence": 42,
            "kind": "claim",
            "changes": json.dumps([
                {"field": "claim_holder", "old": None, "new": "original worker"},
                {"field": "claim_marker", "old": "old-live-marker", "new": "new-live-marker"},
                {"field": "claim_expires_at", "old": None, "new": "2026-08-12T06:31:00Z"},
            ]),
            "created_at": "2026-08-12 06:30:00.120000",
        }
        record = convert("Change", "legacy-project", "legacy-item", source, 20000, None, Counter())
        retained = json.loads(record["evidence"]["changes"])
        self.assertEqual([entry["field"] for entry in retained],
                         ["claim_holder", "claim_expires_at"])
        self.assertNotIn("live-marker", json.dumps(record))


if __name__ == "__main__":
    unittest.main()
