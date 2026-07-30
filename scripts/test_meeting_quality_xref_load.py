import contextlib
import gzip
import io
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))

import meeting_quality_xref as xref


class MeetingQualityXrefLoadTest(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.meeting = xref.Meeting("room", "2026-07-16", "hcl-daily")

    def write_gzip(self, name, data):
        path = os.path.join(self.temp_dir.name, name)
        with open(path, "wb") as fh:
            fh.write(data)
        return name

    @staticmethod
    def log_line(message):
        return (
            json.dumps(
                {
                    "ts": "2026-07-16T12:00:00Z",
                    "level": "info",
                    "msg": message,
                }
            )
            + "\n"
        )

    def load(self, filename):
        warnings = io.StringIO()
        with contextlib.redirect_stderr(warnings):
            participant = xref.load_participant(
                self.meeting,
                "participant@example.com",
                [filename],
                self.temp_dir.name,
            )
        return participant, warnings.getvalue()

    def test_symbolic_union_cap_warns_and_load_continues(self):
        line = self.log_line(
            "AQ_STATUS: video_tier=medium(4) audio_tier=high(0) "
            "target_fps=0 target_bitrate=400 encoder_queue_depth=0 "
            "active_layers=1 union_cap=capped"
        )
        filename = self.write_gzip(
            "symbolic.log.gz",
            gzip.compress(line.encode(), mtime=0),
        )

        participant, warnings = self.load(filename)

        aq_events = [event for event in participant.events if event["kind"] == "aq"]
        self.assertEqual(len(aq_events), 1)
        self.assertIsNone(aq_events[0]["union_cap"])
        self.assertIn("WARN: invalid AQ_STATUS union_cap='capped'", warnings)

    def test_zlib_corruption_keeps_prior_lines_and_warns(self):
        good_lines = "".join(
            self.log_line(f"SESSION_ASSIGNED received on connection wt_0: {index}")
            for index in range(400)
        )
        bad_lines = "".join(
            self.log_line(f"SESSION_ASSIGNED received on connection wt_0: {index}")
            for index in range(400, 420)
        )
        corrupt_member = bytearray(gzip.compress(bad_lines.encode(), mtime=0))
        corrupt_member[10] ^= 0xFF
        filename = self.write_gzip(
            "corrupt.log.gz",
            gzip.compress(good_lines.encode(), mtime=0) + corrupt_member,
        )

        participant, warnings = self.load(filename)

        retained = len(participant.own_sessions)
        self.assertGreater(retained, 0)
        self.assertTrue(
            participant.own_sessions.issubset({str(index) for index in range(400)})
        )
        self.assertIn("WARN: truncated/corrupt gzip corrupt.log.gz", warnings)
        self.assertIn(f"using {retained} lines read before the bad tail", warnings)


if __name__ == "__main__":
    unittest.main()
