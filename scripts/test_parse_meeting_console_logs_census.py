#!/usr/bin/env python3
"""Regression tests for the Error Census in parse_meeting_console_logs.sh."""
import gzip
import json
import platform
import re
import os
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                      "parse_meeting_console_logs.sh")

# Real pulled logs are COMPACT json; the script greps that exact byte sequence.
COMPACT = (",", ":")

# Must match CENSUS_MAX_ROWS in the script.
MAX_ROWS = 40


def rec(msg, level="error", ts="2026-08-12T15:01:00.000Z", seq=1, omit_msg=False):
    body = {"seq": seq, "ts": ts, "level": level}
    if not omit_msg:
        body["msg"] = msg
    return json.dumps(body, separators=COMPACT)


def preamble(email):
    return json.dumps({
        "seq": 0, "ts": "2026-08-12T15:00:00.000Z", "level": "preamble",
        "msg": f"cores=8 platform=Linux memory=8 network= battery= email={email}",
    }, separators=COMPACT)


def truncate(path, fraction):
    with open(path, "rb") as fh:
        raw = fh.read()
    with open(path, "wb") as fh:
        fh.write(raw[: max(1, int(len(raw) * fraction))])


def gzip_loss_is_detectable():
    """Probe the local zgrep: the script detects a truncated member only from
    exit >= 2 or a gzip diagnostic on stderr, and not every zgrep emits either."""
    probe = tempfile.mkdtemp(prefix="census-probe-")
    try:
        path = os.path.join(probe, "probe.log.gz")
        with gzip.open(path, "wt") as fh:
            for i in range(200):
                fh.write(rec(f"probe {i}", seq=i + 1) + "\n")
        truncate(path, 0.6)
        try:
            r = subprocess.run(["zgrep", "-H", '"level":"error"', path],
                               capture_output=True, text=True, timeout=60)
        except (OSError, subprocess.SubprocessError):
            return False
        return r.returncode >= 2 or bool(r.stderr.strip())
    finally:
        shutil.rmtree(probe, ignore_errors=True)


# Never skip on Linux (CI): there the detector is expected to work, so a silent
# skip would hide a regression rather than a platform limit. Linux first, so the
# probe is not even spawned where its result cannot change the outcome.
GZIP_LOSS_DETECTABLE = platform.system() == "Linux" or gzip_loss_is_detectable()
NO_GZIP_SIGNAL = ("this zgrep reports neither exit>=2 nor a gzip diagnostic on a "
                  "truncated member, so the script cannot detect the loss")


class CensusTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="census-test-")

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def write_session(self, email, session_ts, lines):
        """One gzipped chunk named like a real pulled log: <id>_<ts>_00001.log.gz"""
        path = os.path.join(self.dir, f"{email}_{session_ts}_00001.log.gz")
        with gzip.open(path, "wt") as fh:
            fh.write(preamble(email) + "\n")
            for line in lines:
                fh.write(line + "\n")

    def census(self):
        out = subprocess.run(["bash", SCRIPT, self.dir], capture_output=True,
                             text=True, timeout=300).stdout
        start = out.find("### Error Census")
        self.assertNotEqual(start, -1, f"no census section in output:\n{out[:2000]}")
        end = out.find("### Re-election Events", start)
        return out[start:end if end != -1 else len(out)]

    def rows(self, census, needle):
        return [ln for ln in census.splitlines()
                if ln.startswith("|") and not ln.startswith("|---")
                and not ln.startswith("| Count") and needle in ln]

    def test_shared_signature_counts_distinct_participants_and_is_flagged(self):
        shared = "Failed to construct 'AudioWorkletNode'"
        self.write_session("alice@x.com", "1786546775631", [rec(shared)])
        self.write_session("bob@x.com", "1786546775632", [rec(shared)])
        c = self.census()
        row = self.rows(c, "AudioWorkletNode")
        self.assertEqual(len(row), 1, f"expected one row, got {row}")
        self.assertRegex(row[0], r"\|\s*2\s*\|\s*2\s*⚠\s*\|")
        self.assertIn("MORE THAN ONE person", c)

    def test_two_sessions_of_one_participant_count_as_one_person(self):
        msg = "Election failed: no candidates"
        self.write_session("alice@x.com", "1786546775631", [rec(msg)])
        self.write_session("alice@x.com", "1786546999999", [rec(msg, seq=2)])
        c = self.census()
        row = self.rows(c, "Election failed")
        self.assertEqual(len(row), 1)
        self.assertRegex(row[0], r"\|\s*2\s*\|\s*1\s*\|")
        self.assertNotIn("⚠", row[0])
        self.assertIn("confined to a single participant", c)

    def test_underscored_participant_ids_are_not_merged(self):
        """`SAFE_USER_ID_RE` permits '_', where the script-wide key truncates."""
        self.write_session("alex_one@x.com", "1786546775631", [rec("shared boom")])
        self.write_session("alex_two@x.com", "1786546775632", [rec("shared boom")])
        row = self.rows(self.census(), "shared boom")
        self.assertEqual(len(row), 1)
        self.assertRegex(row[0], r"\|\s*2\s*\|\s*2\s*⚠\s*\|",
                         "underscored ids collapsed into one participant")

    def test_multi_underscore_ids_are_distinct_people(self):
        """Ids sharing a prefix before the SECOND '_' land in one session key, so
        the census must attribute per FILE."""
        self.write_session("alice_team_one@x.com", "1786546775631", [rec("multi boom")])
        self.write_session("alice_team_two@x.com", "1786546775632", [rec("multi boom")])
        row = self.rows(self.census(), "multi boom")
        self.assertEqual(len(row), 1)
        self.assertRegex(row[0], r"\|\s*2\s*\|\s*2\s*⚠\s*\|",
                         "multi-underscore ids merged into one participant")

    def test_unparsable_line_is_reported_not_silently_dropped(self):
        """Under `jq -R` the run does NOT abort, so rows stay nonzero and an
        emptiness-only guard would print the all-clear over lost input."""
        path = os.path.join(self.dir, "bob@x.com_1786546775632_00001.log.gz")
        with gzip.open(path, "wt") as fh:
            fh.write(preamble("bob@x.com") + "\n")
            fh.write('{"seq":1,"level":"error","msg":\n')       # truncated mid-write
        self.write_session("alice@x.com", "1786546775631", [rec("alice only")])
        c = self.census()
        self.assertIn("could not be parsed", c)
        self.assertIn("lower bound", c)
        self.assertNotIn("confined to a single participant", c)

    def test_shared_count_covers_rows_beyond_the_display_cap(self):
        n = MAX_ROWS + 6
        for who in ("alice@x.com", "bob@x.com"):
            self.write_session(who, "178654677563" + ("1" if who[0] == "a" else "2"),
                               [rec(f"shared defect kind {chr(97 + i)}{i}zz", seq=i)
                                for i in range(n)])
        c = self.census()
        m = re.search(r"\*\*(\d+) signature\(s\) affect MORE THAN ONE person", c)
        self.assertIsNotNone(m, f"no shared-defect summary in:\n{c}")
        self.assertEqual(int(m.group(1)), n,
                         "shared count stopped at the display cap")

    def test_non_13_digit_session_timestamp_is_one_participant(self):
        """meeting-api requires only digits, any width; a fixed-width pattern
        leaves the whole filename as the identity."""
        for i, ts in enumerate(("17865467756", "178654677563100"), start=1):
            path = os.path.join(self.dir, f"dana@x.com_{ts}_0000{i}.log.gz")
            with gzip.open(path, "wt") as fh:
                fh.write(preamble("dana@x.com") + "\n")
                fh.write(rec("width boom", seq=i) + "\n")
        row = self.rows(self.census(), "width boom")
        self.assertEqual(len(row), 1)
        self.assertRegex(row[0], r"\|\s*2\s*\|\s*1\s*\|",
                         "one participant's chunks counted as several people")
        self.assertNotIn("⚠", row[0])

    def test_long_ids_are_normalised_into_one_signature(self):
        self.write_session("alice@x.com", "1786546775631", [
            rec("peer 9513947928687989270 timed out"),
            rec("peer 1124371052844996302 timed out", seq=2),
        ])
        rows = self.rows(self.census(), "timed out")
        self.assertEqual(len(rows), 1, f"long ids not normalised: {rows}")
        self.assertRegex(rows[0], r"\|\s*2\s*\|")

    def test_volatile_durations_still_group(self):
        self.write_session("alice@x.com", "1786546775631", [
            rec("budget exhausted (12s cumulative)"),
            rec("budget exhausted (37s cumulative)", seq=2),
        ])
        rows = self.rows(self.census(), "budget exhausted")
        self.assertEqual(len(rows), 1, f"durations fragmented: {rows}")

    def test_semantic_numbers_are_not_merged(self):
        self.write_session("alice@x.com", "1786546775631", [
            rec("JMAP error response (401 Unauthorized)"),
            rec("JMAP error response (500 Server Error)", seq=2),
            rec("WebSocket closed: code=1006", seq=3),
            rec("WebSocket closed: code=1000", seq=4),
        ])
        c = self.census()
        for token in ("401", "500", "1006", "1000"):
            self.assertIn(token, c, f"{token} was normalised away")

    def test_panic_reason_line_distinguishes_panics_in_one_file(self):
        """The discriminating reason is on line 2; line 1 alone collapses every
        panic in a file into one row."""
        self.write_session("alice@x.com", "1786546775631", [
            rec("panicked at host.rs:213:25:\ncalled `Result::unwrap()` on an `Err` "
                "value: Dropped(ValueDroppedError)\n\nStack:\n    at foo"),
            rec("panicked at host.rs:99:1:\nindex out of bounds: the len is 0\n\nStack:",
                seq=2),
        ])
        c = self.census()
        self.assertEqual(len(self.rows(c, "panicked at")), 2,
                         "two different panics collapsed into one signature")
        self.assertIn("Dropped(ValueDroppedError)", c)
        self.assertIn("index out of bounds", c)

    def test_long_message_is_marked_as_truncated(self):
        self.write_session("alice@x.com", "1786546775631", [rec("Ezz" + "x" * 200)])
        row = self.rows(self.census(), "Ezz")[0]
        self.assertIn("…", row, "no truncation marker: two defects sharing a "
                                "100-char prefix would silently merge")

    def test_no_errors_reports_clean(self):
        self.write_session("alice@x.com", "1786546775631",
                           [rec("all good", level="info")])
        c = self.census()
        self.assertIn("No error-level lines", c)
        self.assertNotIn("⚠", c)

    def test_errors_counted_but_no_rows_does_not_report_clean(self):
        """Fixture is the all-dropped path (a frame-only error), not a parse
        failure: rows are 0 while the error count is 1."""
        self.write_session("alice@x.com", "1786546775631", [
            rec("    at https://example.com/app.js:1835:25"),
        ])
        c = self.census()
        self.assertNotIn("No error-level lines", c)
        self.assertNotIn("confined to a single participant", c)
        self.assertIn("Census produced NO rows despite", c)

    def test_empty_message_does_not_forge_a_null_signature(self):
        r'''`("" | split("\n"))[0]` is null, which `jq -r` prints as "null" — a
        forged signature that ranks first. Two overlapping guards deliver this, so
        it goes red only when both are removed.'''
        self.write_session("alice@x.com", "1786546775631", [rec("", omit_msg=True)])
        self.write_session("bob@x.com", "1786546775632", [rec("", omit_msg=True)])
        c = self.census()
        self.assertEqual(self.rows(c, "null"), [], "forged 'null' signature row")
        self.assertNotIn("MORE THAN ONE person", c)

    def test_jq_dying_midstream_is_counted_as_loss(self):
        """A jq shim emitting one line then exiting non-zero — the observable shape
        of a mid-stream death, which leaves no PARSEFAIL to count."""
        self.write_session("alice@x.com", "1786546775631",
                           [rec("first"), rec("second", seq=2), rec("third", seq=3)])
        shim = os.path.join(self.dir, "shim")
        os.makedirs(shim, exist_ok=True)
        with open(os.path.join(shim, "jq"), "w") as fh:
            real_jq = shutil.which("jq") or "/usr/bin/jq"
            fh.write("#!/bin/sh\n"
                     'case "$*" in *PARSEFAIL*) echo "alice@x.com\tfirst"; exit 2;; esac\n'
                     f'exec {real_jq} "$@"\n')
        os.chmod(os.path.join(shim, "jq"), 0o755)
        env = dict(os.environ, PATH=shim + os.pathsep + os.environ["PATH"])
        out = subprocess.run(["bash", SCRIPT, self.dir], capture_output=True,
                             text=True, timeout=300, env=env).stdout
        start = out.find("### Error Census")
        self.assertNotEqual(start, -1, f"no census section:\n{out[:1500]}")
        c = out[start:out.find("### Re-election Events", start)]
        self.assertIn("could not be parsed", c, "silent mid-stream loss went unreported")
        self.assertIn("lower bound", c)
        self.assertNotIn("confined to a single participant", c)

    def test_long_duration_groups_with_a_short_one(self):
        """`[0-9]{6,}` before the duration rule makes "1500000ms" a different
        signature from "500ms" — one defect, two rows, People under-reported."""
        self.write_session("alice@x.com", "1786546775631", [
            rec("stalled for 500ms"),
            rec("stalled for 1500000ms", seq=2),
        ])
        rows = self.rows(self.census(), "stalled for")
        self.assertEqual(len(rows), 1, f"duration split by the id rule: {rows}")
        # Pin MERGED, not merely not-split: a lone Count=1 row would also be len 1.
        self.assertRegex(rows[0], r"\|\s*2\s*\|")

    @unittest.skipUnless(GZIP_LOSS_DETECTABLE, NO_GZIP_SIGNAL)
    def test_truncated_gzip_chunk_is_reported(self):
        """A truncated .log.gz decompresses only its prefix, so the missing lines
        never exist for the JSON-layer reconciliation to count."""
        path = os.path.join(self.dir, "alice@x.com_1786546775631_00001.log.gz")
        with gzip.open(path, "wt") as fh:
            fh.write(preamble("alice@x.com") + "\n")
            for i in range(200):
                fh.write(rec(f"boom {i % 3}", seq=i + 1) + "\n")
        truncate(path, 0.6)
        c = self.census()
        self.assertIn("truncated or corrupt", c)
        self.assertIn("lower bound", c)
        self.assertNotIn("confined to a single participant", c)

    @unittest.skipUnless(GZIP_LOSS_DETECTABLE, NO_GZIP_SIGNAL)
    def test_corrupt_chunk_with_no_surviving_error_is_not_reported_clean(self):
        """Chunk 2 is cut below its gzip header, so it recovers nothing: error_count
        is 0 and every loss warning lives on the other branch."""
        self.write_session("alice@x.com", "1786546775631",
                           [rec("joined", level="info")])
        path = os.path.join(self.dir, "alice@x.com_1786546775631_00002.log.gz")
        with gzip.open(path, "wt") as fh:
            for i in range(50):
                fh.write(rec(f"late boom {i % 3}", seq=i + 2) + "\n")
        truncate(path, 0.02)
        c = self.census()
        self.assertNotIn("No error-level lines", c,
                         "all-clear printed over a detected corrupt chunk")
        self.assertIn("truncated or corrupt", c)
        self.assertIn("cannot say the meeting was clean", c)

    @unittest.skipUnless(GZIP_LOSS_DETECTABLE, NO_GZIP_SIGNAL)
    def test_co_occurring_losses_are_each_reported(self):
        """gzip-layer and JSON-layer loss are distinct facts that co-occur; a
        warning chain reports only the first and the operator acts on a lower
        count than the tool measured."""
        path = os.path.join(self.dir, "alice@x.com_1786546775631_00001.log.gz")
        with gzip.open(path, "wt") as fh:
            fh.write(preamble("alice@x.com") + "\n")
            for i in range(200):
                fh.write(rec(f"boom {i % 3}", seq=i + 1) + "\n")
        truncate(path, 0.6)
        bad = os.path.join(self.dir, "bob@x.com_1786546775632_00001.log.gz")
        with gzip.open(bad, "wt") as fh:
            fh.write(preamble("bob@x.com") + "\n")
            fh.write('{"seq":1,"level":"error","msg":\n')
        c = self.census()
        self.assertIn("truncated or corrupt", c)
        self.assertIn("could not be parsed", c, "JSON-layer count suppressed")

    @unittest.skipUnless(GZIP_LOSS_DETECTABLE, NO_GZIP_SIGNAL)
    def test_losses_are_reported_when_no_row_survives(self):
        """Zero rows is the path where a chain hid the warnings: alice's only error
        is a dropped frame, her 2nd chunk is cut below its gzip header, bob's never
        closes."""
        self.write_session("alice@x.com", "1786546775631",
                           [rec("    at https://example.com/app.js:1835:25")])
        cut = os.path.join(self.dir, "alice@x.com_1786546775631_00002.log.gz")
        with gzip.open(cut, "wt") as fh:
            for i in range(50):
                fh.write(rec(f"unreachable {i}", seq=i + 2) + "\n")
        truncate(cut, 0.02)
        bad = os.path.join(self.dir, "bob@x.com_1786546775632_00001.log.gz")
        with gzip.open(bad, "wt") as fh:
            fh.write(preamble("bob@x.com") + "\n")
            fh.write('{"seq":1,"level":"error","msg":\n')
        c = self.census()
        self.assertIn("Census produced NO rows", c)
        self.assertIn("truncated or corrupt", c, "gzip-layer loss suppressed at 0 rows")
        self.assertIn("could not be parsed", c, "JSON-layer loss suppressed at 0 rows")
        self.assertNotIn("No error-level lines", c)

    def test_stack_frame_lines_are_dropped_as_signatures(self):
        r'''Each alternative of the filter needs a fixture whose FIRST line matches
        it — `split("\n")[0]` already hides frames that follow line 1, so a
        multi-line fixture does not exercise this rule at all.'''
        self.write_session("alice@x.com", "1786546775631", [
            rec("    at https://example.com/app.js:1835:25"),
            rec("<?>.wasm-function[1666]@[wasm code]", seq=2),
            rec("Stack:", seq=3),
            rec("a real defect", seq=4),
        ])
        c = self.census()
        self.assertEqual(len(self.rows(c, "a real defect")), 1)
        for frame in ("app.js", "wasm-function", "Stack:"):
            self.assertEqual(self.rows(c, frame), [], f"{frame} became a signature")

    def test_tab_in_message_cannot_corrupt_the_grouping_key(self):
        self.write_session("alice@x.com", "1786546775631", [
            rec("decode failed\tcodec=av01"),
            rec("decode failed\tcodec=vp09", seq=2),
        ])
        rows = self.rows(self.census(), "decode failed")
        self.assertEqual(len(rows), 2, f"tab merged distinct errors: {rows}")

    def test_carriage_return_is_not_emitted_raw(self):
        self.write_session("alice@x.com", "1786546775631", [rec("decode failed\r")])
        row = self.rows(self.census(), "decode failed")[0]
        self.assertNotIn("\r", row, "raw CR garbles the row under less/markdown")

    def test_pipe_in_message_is_escaped(self):
        self.write_session("alice@x.com", "1786546775631", [rec("bad | pipe | here")])
        row = self.rows(self.census(), "pipe")[0]
        self.assertIn(r"\|", row, "unescaped pipe would split the table")
        # 4 structural pipes: leading, after Count, after People, trailing.
        self.assertEqual(row.count("|") - row.count(r"\|"), 4)

    def test_table_is_capped_with_a_footer(self):
        n = MAX_ROWS + 5
        self.write_session("alice@x.com", "1786546775631",
                           [rec(f"decode failed for track {chr(97 + i % 26)}bc{i}zz", seq=i)
                            for i in range(n)])
        c = self.census()
        body = [ln for ln in c.splitlines()
                if ln.startswith("| ") and not ln.startswith("| Count")]
        self.assertLessEqual(len(body), MAX_ROWS,
                             f"emitted {len(body)} rows, cap is {MAX_ROWS}")
        self.assertIn("further signature(s) omitted", c)


if __name__ == "__main__":
    unittest.main(verbosity=2)
