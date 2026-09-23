import contextlib
import io
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


class CoalescedAudioHealthLineTest(unittest.TestCase):
    def setUp(self):
        self.meeting = xref.Meeting("infra", "2026-09-22", "hcl-daily")
        self.participant = xref.Participant("participant@example.com")
        self.meeting.participants[self.participant.email] = self.participant

    def test_one_coalesced_line_yields_every_sample(self):
        xref._classify(
            self.meeting,
            self.participant,
            100.0,
            "Updated audio health x3 (from current_user): "
            "audio health (buffer: 274ms) for peer: 9547815290404412626 | "
            "audio health (buffer: 0ms) for peer: 11322898268744594248 | "
            "audio health (buffer: 51ms) for peer: 3310028821455180942",
        )

        events = [e for e in self.participant.events if e["kind"] == "audio_health"]
        self.assertEqual(
            [(e["buffer_ms"], e["peer"]) for e in events],
            [
                (274, "9547815290404412626"),
                (0, "11322898268744594248"),
                (51, "3310028821455180942"),
            ],
        )

    def test_the_pre_coalescing_single_sample_line_still_parses(self):
        xref._classify(
            self.meeting,
            self.participant,
            100.0,
            "Updated audio health (buffer: 660ms) for peer: 12175 (from current_user)",
        )

        events = [e for e in self.participant.events if e["kind"] == "audio_health"]
        self.assertEqual([(e["buffer_ms"], e["peer"]) for e in events], [(660, "12175")])

    def test_a_display_name_cannot_mint_audio_health_events(self):
        probes = [
            # The security reviewer's probe: samples, no emitter prefix.
            "PARTICIPANT_JOINED display_name=audio health (buffer: 900ms) for "
            "peer: 7 | audio health (buffer: 900ms) for peer: 8 sid=42",
            # Carries the prefix too, so ONLY \A rejects these two.
            "PARTICIPANT_JOINED display_name=Updated audio health x2 (from x): "
            "audio health (buffer: 900ms) for peer: 7 sid=42",
            "PARTICIPANT_JOINED display_name=Updated audio health (buffer: 900ms) "
            "for peer: 7 sid=42",
        ]
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            for probe in probes:
                xref._classify(self.meeting, self.participant, 100.0, probe)

        events = [e for e in self.participant.events if e["kind"] == "audio_health"]
        self.assertEqual(events, [])

    def test_a_prefixed_line_warns_instead_of_silently_yielding_nothing(self):
        # What `console_log/color` would produce; the drop must be audible.
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            xref._classify(
                self.meeting,
                self.participant,
                100.0,
                "%cDEBUG%c health_reporter.rs:1440 %c\n"
                "Updated audio health x1 (from current_user): "
                "audio health (buffer: 274ms) for peer: 9547815290404412626",
            )

        events = [e for e in self.participant.events if e["kind"] == "audio_health"]
        self.assertEqual(events, [])
        self.assertIn("WARN: audio-health line not at message start", stderr.getvalue())

    def test_the_canary_stays_silent_on_lines_it_does_not_own(self):
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            # Matches NO rule, so it reaches the canary and must not trip it.
            xref._classify(
                self.meeting, self.participant, 100.0, "Rendering meeting view"
            )
            # Returns before the canary: pins that the canary sits BELOW it.
            xref._classify(
                self.meeting,
                self.participant,
                101.0,
                "Updated audio health x1 (from current_user): "
                "audio health (buffer: 274ms) for peer: 9547815290404412626",
            )

        self.assertEqual(stderr.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
