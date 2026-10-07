#!/usr/bin/env python3
"""Regression tests for check_call_quality_dashboard.py and call_quality_dashboard_parity.py (#2985).

Guard tests break one property of the real dashboard (or the scorer config) and assert the
guard notices. Parity tests drive main() against a fake Prometheus.
"""
from __future__ import annotations

import io
import json
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
import urllib.parse
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE / "quality"))

import call_quality_dashboard_parity as parity  # noqa: E402
import cq_prom  # noqa: E402
import cq_score  # noqa: E402

GUARD = HERE / "check_call_quality_dashboard.py"
DASHBOARD = HERE.parent / "helm/grafana/dashboards/call-quality.json"
EXAMPLE_MANIFEST = HERE / "quality/example_run_manifest.json"


class Guard(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="cq-dash-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.text = DASHBOARD.read_text(encoding="utf-8")

    def run_guard(self, dash_text=None, config=None):
        path = self.tmp / "call-quality.json"
        path.write_text(self.text if dash_text is None else dash_text)
        argv = [sys.executable, str(GUARD), "--dashboard", str(path)]
        if config is not None:
            (self.tmp / "cfg.json").write_text(json.dumps(config))
            argv += ["--config", str(self.tmp / "cfg.json")]
        return subprocess.run(argv, capture_output=True, text=True)

    def assert_flags(self, needle, dash_text=None, config=None):
        res = self.run_guard(dash_text, config)
        self.assertEqual(res.returncode, 1, res.stdout + res.stderr)
        self.assertIn(needle, res.stdout)

    def mutated(self, old, new, count=None):
        self.assertTrue(old in self.text, f"mutation target {old!r} is not in the dashboard")
        return self.text.replace(old, new) if count is None else self.text.replace(old, new, count)

    def edit(self, fn):
        dash = json.loads(self.text)
        fn(dash)
        return json.dumps(dash)

    def panel(self, dash, title):
        return next(p for p in dash["panels"] if p.get("title") == title)

    def test_the_real_dashboard_passes(self):
        res = self.run_guard()
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)

    def test_a_config_band_change_fails(self):
        res = self.run_guard(config={"bands": {"A": {"red": 0.06}}})
        self.assertEqual(res.returncode, 1, res.stdout)
        self.assertIn("A k query differs from the scorer-derived expression", res.stdout)
        self.assertIn("thresholds [0.05] != [0.06]", res.stdout)

    def test_a_config_constant_change_fails(self):
        for group, key, value, dim in (
                ("dimensions", "expand_ops_full_scale", 80, "A"),
                ("dimensions", "near_frozen_fps", 6, "Q")):
            with self.subTest(key=key):
                self.assert_flags(f"{dim} hold query differs", config={group: {key: {"value": value}}})

    def test_a_k_red_fail_change_fails(self):
        self.assert_flags("thresholds [1, 2] != [1, 1]", config={"quality_gates": {"k_red_fail": {"value": 1}}})

    def test_a_configured_latency_metric_requires_a_dashboard_update(self):
        self.assert_flags("still says L is pending",
                          config={"latency_gate": {"audio_delay_metric": {"value": cq_score.M_PPS}}})

    def test_query_mutations_fail(self):
        for name, old, new, needle in (
                ("A comparison flipped", "/ 100 > clamp_min", "/ 100 < clamp_min", "A hold query differs"),
                ("talker join dropped", ' * on (to_peer) group_left () max by (to_peer) (label_replace(', ' + 0 * (',
                 "A hold query differs"),
                ("can_listen == 0", "== 1) or on", "== 0) or on", "A hold query differs"),
                ("V offset changed", "[5m] offset 15s", "[5m] offset 30s", "offset 30s != scrape_step_s 15s"),
                ("V offset dropped", "[5m] offset 15s", "[5m]", "V hold query differs"),
                ("V reset branch keeps the name", '\\"$obs\\"} * 1)', '\\"$obs\\"})', "V hold query differs"),
                ("V oldest session wins", "topk by (peer_id)", "bottomk by (peer_id)",
                 "V hold query differs"),
                ("Q fps >= 0", "> bool 0) * (", ">= bool 0) * (", "Q hold query differs"),
                ("Q near-frozen 6", "< bool 5)", "< bool 6)", "Q hold query differs"),
                ("A nominal 45", "} / 50,", "} / 45,", "A hold query differs"),
                ("S failed re-elections", 'result=\\"proceeded\\"', 'result=\\"failed\\"', "S hold query differs"),
                ("S new sessions only", " unless on (session_id) ", " and on (session_id) ", "S hold query differs"),
                ("k strict", ">= bool 0.1\\n", "> bool 0.1\\n", "Q k query differs"),
                ("p95 0.9", "quantile(0.95,", "quantile(0.9,", "p95 query differs"),
                ("p95 max", "quantile(0.95,", "max(", "p95 query differs"),
                ("subquery 30s", ":15s]", ":30s]", "subquery step 30s != scrape_step_s 15s"),
                ("subquery default step", '(last_over_time(videocall_peer_info{meeting_id=\\"$meeting\\"}[$__range])))',
                 '(max_over_time(videocall_peer_info{meeting_id=\\"$meeting\\"}[$__range:])))',
                 "(Participants seen): subquery step (default) != scrape_step_s 15s"),
                ("selector without meeting", 'count(count by (peer_id) (last_over_time(videocall_peer_info',
                 'sum(up{job=\\"metrics-api\\"}) + count(count by (peer_id) (last_over_time(videocall_peer_info',
                 "(Participants seen): up selector must carry exactly"),
                ("family never fetched", 'count(count by (peer_id) (last_over_time(videocall_peer_info',
                 'sum(up{job=\\"metrics-api\\"}) + count(count by (peer_id) (last_over_time(videocall_peer_info',
                 "(Participants seen): up is not fetched by cq_score.step_queries"),
                ("name-regex selector", 'count(count by (peer_id) (last_over_time(videocall_peer_info',
                 'count({__name__=~\\"videocall_.*\\"}) + count(count by (peer_id) (last_over_time(videocall_peer_info',
                 "(Participants seen): {...} selector must name its metric literally"),
                ("meeting regex", 'meeting_id=\\"$meeting\\"', 'meeting_id=~\\"$meeting|.*\\"',
                 "selector must carry exactly"),
                ("meeting widened", 'count by (from_peer) (videocall_peer_can_listen{meeting_id=\\"$meeting\\",',
                 'count by (from_peer) (videocall_peer_can_listen{meeting_id=\\"$meeting\\",meeting_id=~\\".*\\",',
                 "(Reporters over time): videocall_peer_can_listen selector must carry exactly"),
                ("meeting dropped", 'videocall_peer_can_listen{meeting_id=\\"$meeting\\",',
                 "videocall_peer_can_listen{", "videocall_peer_can_listen selector must carry exactly"),
                ("quantile_over_time", "sum_over_time(((videocall_video_fps",
                 "quantile_over_time(0.95, ((videocall_video_fps", "only quantile(0.95, ...)"),
                ("unfetched family", "videocall_client_active_server_rtt_ms{", "videocall_audio_concealment_pct{",
                 "videocall_audio_concealment_pct is not fetched by cq_score.step_queries"),
                ("per-pair not reduced", "max by (from_peer) (videocall_video_content_staleness_ms",
                 "max by (to_peer) (videocall_video_content_staleness_ms", "must be reduced by (from_peer)"),
                ("tag removed", "cq:S:p95", "untagged", "no panel tagged cq:S:p95"),
                ("latency notice removed", "latency not gated: #2948 pending", "latency", "no text panel says")):
            with self.subTest(name):
                self.assert_flags(needle, self.mutated(old, new))

    def test_a_renamed_meeting_variable_fails(self):
        def rename(dash):
            next(v for v in dash["templating"]["list"] if v["name"] == "meeting")["name"] = "mtg"
        self.assert_flags("$meeting must be a query variable", self.edit(rename))

    def test_a_panel_threshold_change_fails(self):
        def bump(dash):
            self.panel(dash, "V per observer over the hold")["fieldConfig"]["defaults"]["thresholds"]["steps"][-1][
                "value"] = 0.05
        self.assert_flags("thresholds [0.05] != [0.03]", self.edit(bump))

    def test_a_p95_threshold_at_the_band_fails(self):
        def at_band(dash):
            self.panel(dash, "p95 A")["fieldConfig"]["defaults"]["thresholds"]["steps"][-1]["value"] = 0.05
        self.assert_flags("thresholds [0.05] != [0.05000000000000001]", self.edit(at_band))

    def test_a_table_column_threshold_change_fails(self):
        def bump(dash):
            table = self.panel(dash, "Participants, worst A first")
            props = next(o for o in table["fieldConfig"]["overrides"] if o["matcher"]["options"] == "Q")["properties"]
            next(p for p in props if p["id"] == "thresholds")["value"]["steps"][-1]["value"] = 0.2
        self.assert_flags("target Q: thresholds [0.2]", self.edit(bump))

    def test_a_datasource_uid_fails(self):
        def panel(dash):
            self.panel(dash, "Participants seen")["datasource"]["uid"] = "abc123"

        def variable(dash):
            dash["templating"]["list"][0]["datasource"]["uid"] = "abc123"

        def annotation(dash):
            dash["annotations"]["list"].append({"datasource": {"type": "prometheus", "uid": "abc123"}, "expr": "1"})

        def by_name(dash):
            self.panel(dash, "Participants seen")["targets"][0]["datasource"] = "Prometheus"
        for fn, where in ((panel, "dashboard.panels[2]"), (variable, "dashboard.templating.list[0]"),
                          (annotation, "dashboard.annotations.list[1]"), (by_name, "dashboard.panels[2].targets[0]")):
            with self.subTest(fn.__name__):
                self.assert_flags(f"{where}: datasource", self.edit(fn))

    def test_a_table_rename_or_merge_change_fails(self):
        def swap(dash):
            rename = self.panel(dash, "Participants, worst A first")["transformations"][1]["options"]["renameByName"]
            rename["Value #A"], rename["Value #V"] = rename["Value #V"], rename["Value #A"]

        def unmerged(dash):
            del self.panel(dash, "Participants, worst A first")["transformations"][0]
        for fn, needle in ((swap, "renameByName"), (unmerged, "transformations ['organize'] != ['merge', 'organize']")):
            with self.subTest(fn.__name__):
                self.assert_flags(f"(Participants, worst A first): {needle}", self.edit(fn))

    def test_a_fast_refresh_fails(self):
        def fast(dash):
            dash["refresh"] = "5s"
        self.assert_flags("refresh '5s': must be off or at least 15s", self.edit(fast))

    def test_an_unreadable_dashboard_is_a_usage_error(self):
        res = subprocess.run([sys.executable, str(GUARD), "--dashboard", str(self.tmp / "absent.json")],
                             capture_output=True, text=True)
        self.assertEqual(res.returncode, 2, res.stdout + res.stderr)


def matrix(rows):
    return json.dumps({"status": "success", "data": {"resultType": "matrix", "result": [
        {"metric": labels, "values": [[1.0, str(v)]]} for labels, v in rows]}}).encode()


class Parity(unittest.TestCase):
    OBS = ["probe-000@bots-app.local", "probe-001@bots-app.local", "probe-002@bots-app.local"]
    S_PARTICIPANTS = ["probe-000@bots-app.local", "probe-001@bots-app.local", "speaker-000", "viewer-000"]

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="cq-parity-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.manifest = json.loads(EXAMPLE_MANIFEST.read_text())
        self.hold_end = next(s for s in self.manifest["steps"] if s["step_id"] == "n5")["hold_end"]
        self.scored = {u: {"A": 0.02 * (i + 1), "V": 0.1, "Q": 0.0, "S": 99.0} for i, u in enumerate(self.OBS)}
        stability = {u: {"S": 12.0 if u == "viewer-000" else 0.0} for u in self.S_PARTICIPANTS}
        p95 = {"A": cq_score.percentile([v["A"] for v in self.scored.values()], 95), "V": 0.1, "Q": 0.0, "S": 99.0}
        s_p95 = cq_score.percentile([v["S"] for v in stability.values()], 95)
        self.result = {"headline_step": "n5", "steps": [{
            "step_id": "n5", "headline_cell": "UxU", "p95_table": {"UxU": p95},
            "cells": {"UxU": {"participants": self.scored}}, "stability": stability,
            "quality_gates": [{"gate": "G-Q9", "value": {"p95": s_p95, "k_red": 1}}]}]}
        self.served = {d: {u: v[d] for u, v in self.scored.items()} for d in "AVQ"}
        self.served["S"] = {u: v["S"] for u, v in stability.items()}
        self.p95 = dict(p95, S=s_p95)
        self.sent = []

    def transport(self, url, body, headers):
        q = urllib.parse.parse_qs(body.decode())
        self.sent.append(q)
        expr = q["query"][0]
        dim = ("A" if "neteq_expand" in expr else "V" if "freeze_seconds" in expr else
               "S" if "reelection" in expr else "Q")
        if expr.lstrip().startswith("quantile(0.95"):
            return matrix([({}, self.p95[dim])])
        return matrix([({"participant": u}, v) for u, v in self.served[dim].items()])

    def run_main(self, transport=None):
        (self.tmp / "m.json").write_text(json.dumps(self.manifest))
        (self.tmp / "r.json").write_text(json.dumps(self.result))
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = parity.main(["--manifest", str(self.tmp / "m.json"), "--result", str(self.tmp / "r.json"),
                              "--prom-url", "http://prom.invalid"], transport=transport or self.transport)
        return rc, out.getvalue() + err.getvalue()

    def test_tolerance_is_the_larger_of_absolute_and_relative(self):
        self.assertTrue(parity.within(0.0049, 0.0))
        self.assertFalse(parity.within(0.0051, 0.0))
        self.assertTrue(parity.within(1.04, 1.0))
        self.assertFalse(parity.within(1.06, 1.0))

    def test_matching_values_pass_and_every_query_is_fully_interpolated(self):
        rc, out = self.run_main()
        self.assertEqual(rc, 0, out)
        self.assertEqual(len(self.sent), 8)
        for q in self.sent:
            self.assertNotIn("$", q["query"][0].replace('"$1"', ""))
            self.assertEqual((q["start"][0], q["end"][0]), (f"{self.hold_end:.3f}", f"{self.hold_end:.3f}"))

    def test_a_participant_value_outside_tolerance_fails(self):
        self.served["V"][self.OBS[1]] = 0.12
        rc, out = self.run_main()
        self.assertEqual(rc, 1, out)
        self.assertIn(f"{self.OBS[1]} V", out)
        self.assertIn("OUTSIDE", out)

    def test_a_p95_outside_tolerance_fails(self):
        self.p95["Q"] = 0.006
        rc, out = self.run_main()
        self.assertEqual(rc, 1, out)
        self.assertRegex(out, r"p95 Q\s+0\.00000\s+0\.00600")

    def test_a_participant_missing_from_the_dashboard_fails(self):
        del self.served["A"][self.OBS[0]]
        rc, out = self.run_main()
        self.assertEqual(rc, 1, out)
        self.assertRegex(out, rf"{re.escape(self.OBS[0])} A\s+0\.02000\s+missing")

    def test_s_compares_every_unshaped_participant_with_g_q9(self):
        rc, out = self.run_main()
        self.assertEqual(rc, 0, out)
        self.assertRegex(out, r"viewer-000 S\s+12\.00000\s+12\.00000")
        self.assertNotIn("probe-002@bots-app.local S", out)
        self.served["S"]["viewer-000"] = 0.0
        rc, out = self.run_main()
        self.assertEqual(rc, 1, out)
        self.assertRegex(out, r"viewer-000 S\s+12\.00000\s+0\.00000")

    def test_a_query_error_is_exit_2(self):
        def broken(url, body, headers):
            raise cq_prom.PromError("boom")
        rc, out = self.run_main(broken)
        self.assertEqual(rc, 2, out)

    def test_variables_match_grafana_escaping(self):
        step = next(s for s in self.manifest["steps"] if s["step_id"] == "n5")
        v = parity.variables_for(self.manifest, step)
        self.assertEqual((v["__range"], v["__range_s"], v["meeting"]), ("600s", "600", "scale-r42"))
        expr = parity.interpolate('x{from_peer=~"$obs"}[$__range] / $__range_s, "$1", ${talkers}', v)
        literal = re.search(r'=~("(?:[^"\\]|\\.)*")', expr).group(1)
        regex = json.loads(literal)
        for u in self.OBS:
            self.assertTrue(re.fullmatch(regex, u), (regex, u))
        self.assertFalse(re.fullmatch(regex, "probe-000@bots-appXlocal"))
        self.assertIn("[600s] / 600", expr)
        self.assertIn('"$1"', expr)
        self.assertTrue(expr.endswith("speaker-000"), expr)
        parts = re.compile(json.loads('"' + v["participants"] + '"'))
        self.assertEqual([u for u in self.S_PARTICIPANTS + ["probe-002@bots-app.local"] if parts.fullmatch(u)],
                         self.S_PARTICIPANTS)


if __name__ == "__main__":
    unittest.main(verbosity=1)
