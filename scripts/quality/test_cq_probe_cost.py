#!/usr/bin/env python3
"""Tests for cq_probe_cost.py on a fixture run folder."""

import contextlib
import io
import json
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cq_collect  # noqa: E402
import cq_local  # noqa: E402
import cq_probe_cost  # noqa: E402

ROWS = [
    "proc,{t},10,chrome,{c},0,100", "proc,{t},11,node,0,{n},100", "proc,{t},12,bot,{b},0,100",
    "proc,{t},13,conmon,{b},0,100",
]


def csv(clk, samples):
    lines = [f"meta,1,host,load-a,{clk},8,0,0,5"]
    for t, c, n, b in samples:
        lines += [r.format(t=t, c=c, n=n, b=b) for r in ROWS]
    return "\n".join(lines) + "\n"


class ProbeCost(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="cq-probe-cost-")
        self.addCleanup(shutil.rmtree, self.dir, True)
        os.makedirs(os.path.join(self.dir, "host"))
        self.events(1000, 1010)
        with open(os.path.join(self.dir, cq_collect.PLAN_FILE), "w", encoding="utf-8") as fh:
            json.dump({"processes": [{"fleet": "browser"}, {"fleet": "rust"}]}, fh)

    def events(self, start, end):
        with open(os.path.join(self.dir, cq_collect.EVENTS_FILE), "w", encoding="utf-8") as fh:
            for kind, wall in (("join_start", start - 100), ("hold_start", start), ("hold_end", end)):
                fh.write(json.dumps({"type": kind, "wall": wall}) + "\n")

    def write(self, text):
        with open(os.path.join(self.dir, "host", "host-raw.csv"), "w", encoding="utf-8") as fh:
            fh.write(text)

    def run_main(self):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = cq_probe_cost.main([self.dir])
        return rc, out.getvalue(), err.getvalue()

    def test_cores_are_clock_ticks_over_clk_tck_and_seconds_for_chrome_and_node_only(self):
        self.write(csv(100, [(995, 0, 0, 0), (1000, 0, 0, 0), (1005, 500, 25, 9000), (1010, 1000, 75, 18000),
                             (1015, 9000, 900, 0)]))
        rc, out, _ = self.run_main()
        self.assertEqual(rc, 0)
        self.assertIn("2 intervals, CLK_TCK 100, 8 CPUs): peak 1.10 cores, mean 1.08 cores; 1 probe(s)", out)
        self.write(csv(250, [(1000, 0, 0, 0), (1005, 500, 25, 0)]))
        self.assertIn("peak 0.42 cores", self.run_main()[1])

    def test_a_process_seen_in_only_one_sample_or_restarted_adds_nothing(self):
        ticks = {1000: {"10": 400}, 1005: {"10": 100, "11": 500}}
        self.assertEqual(cq_probe_cost.cores_by_interval(100, ticks, 1000, 1010), [0.0])

    def test_the_probes_node_process_found_by_its_command_line_counts(self):
        rows = []
        for t, c, n in ((1000, 0, 0), (1005, 500, 250)):
            rows += [f"proc,{t},10,chrome-headless,{c},0,100", f"proc,{t},20,{cq_local.PROBE_NODE_COMM},{n},0,100",
                     f"proc,{t},30,MainThread,{c},0,100"]
        self.write("meta,1,host,load-a,100,8,0,0,5\n" + "\n".join(rows) + "\n")
        self.assertIn("peak 1.50 cores", self.run_main()[1])

    def test_samples_with_no_process_in_common_are_an_error_not_zero(self):
        self.write("meta,1,host,load-a,100,8,0,0,5\nproc,1000,10,chrome,0,0,1\nproc,1005,11,chrome,500,0,1\n")
        rc, out, err = self.run_main()
        self.assertEqual((rc, out), (3, ""))
        self.assertIn("no probe process was in two consecutive samples", err)

    def test_missing_hold_samples_or_clock_ticks_exit_3(self):
        self.write(csv(100, [(1000, 0, 0, 0)]))
        self.assertEqual(self.run_main()[0], 3)
        self.write("proc,1000,10,chrome,1,0,1\n")
        rc, _, err = self.run_main()
        self.assertEqual(rc, 3)
        self.assertIn("no meta row with a positive CLK_TCK", err)
        for clk in (0, -100):
            self.write(csv(clk, [(1000, 0, 0, 0), (1005, 500, 25, 0)]))
            rc, out, err = self.run_main()
            self.assertEqual((rc, out), (3, ""), clk)
            self.assertIn("no meta row with a positive CLK_TCK", err)
        with open(os.path.join(self.dir, cq_collect.EVENTS_FILE), "w", encoding="utf-8") as fh:
            fh.write(json.dumps({"type": "join_start", "wall": 1}) + "\n")
        rc, _, err = self.run_main()
        self.assertEqual(rc, 3)
        self.assertIn("no complete hold", err)

    def test_several_probes_also_give_the_peak_shared_out_over_them(self):
        with open(os.path.join(self.dir, cq_collect.PLAN_FILE), "w", encoding="utf-8") as fh:
            json.dump({"processes": [{"fleet": "browser"}, {"fleet": "browser"}]}, fh)
        self.write(csv(100, [(1000, 0, 0, 0), (1005, 1000, 0, 0)]))
        self.assertIn("2 probe(s); the peak over 2 probes, 1.00 cores each", self.run_main()[1])


if __name__ == "__main__":
    unittest.main()
