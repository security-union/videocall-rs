import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))

import meeting_quality_xref as xref


class RuleR6SuppressionTeardownTest(unittest.TestCase):
    def setUp(self):
        self.meeting = xref.Meeting("infra", "2026-07-24", "hcl-daily")
        self.participant = xref.Participant("participant@example.com")
        self.participant.display_name = "Participant"
        self.meeting.participants[self.participant.email] = self.participant

    def r6_findings(self):
        return [
            finding
            for finding in xref.run_rules(self.meeting, None)
            if finding.rule == "R6"
        ]

    def test_protective_emergency_does_not_imply_teardown(self):
        for index in range(12):
            xref._classify(
                self.meeting,
                self.participant,
                float(index),
                "ProtectiveMode: EMERGENCY cap 3->1 (speaker-only) "
                "audio_buffer_ms=1200 natural=3",
            )

        self.assertEqual(
            self.r6_findings(),
            [],
        )

    def test_exact_failed_reason_counts_teardowns_and_deduplicates_overlap(self):
        exact = (
            'Connection state changed: Failed { error: "cpu-stall suppression budget '
            'exhausted", last_known_server: None } in video call client'
        )
        generic = (
            'Connection state changed: Failed { error: "handshake timeout", '
            'last_known_server: None } in video call client'
        )
        xref._classify(self.meeting, self.participant, 100.0, generic)
        xref._classify(self.meeting, self.participant, 101.0, exact)
        xref._classify(self.meeting, self.participant, 101.0, exact)
        xref._classify(self.meeting, self.participant, 102.0, exact)

        findings = self.r6_findings()

        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].rule, "R6")
        self.assertEqual(findings[0].severity, "HIGH")
        self.assertIn("2 full reconnects", findings[0].title)
        self.assertIn("1970-01-01 00:01:41Z", findings[0].evidence[1])
        self.assertIn("1970-01-01 00:01:42Z", findings[0].evidence[1])


if __name__ == "__main__":
    unittest.main()
