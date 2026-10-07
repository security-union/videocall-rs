#!/usr/bin/env python3
"""Tests for the call quality scorer. No network: Prometheus responses are built in-process."""

import contextlib
import io
import json
import os
import re
import sys
import tempfile
import unittest
import urllib.error
import urllib.parse
import zlib
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import call_quality_score as cli  # noqa: E402
import cq_manifest  # noqa: E402
import cq_privacy  # noqa: E402
import cq_prom  # noqa: E402
import cq_report  # noqa: E402
import cq_score  # noqa: E402
from cq_score import (  # noqa: E402
    M_CAN_LISTEN, M_EXPAND, M_FPS, M_FREEZE, M_HEALTH, M_PPS, M_REELECT, M_RTT, M_SENT, M_STALE)

HS, HE, JOIN, STEP = 1000.0, 1600.0, 940.0, 15
BOT_HEALTH_INTERVAL_S = 5
METRICS_API_SESSION_TIMEOUT_S = 30
DIOXUS_CANVAS_LIMIT = 30
RELAY_PATHS = ("relay_layer_filtered_total", "relay_viewport_filtered_total", "relay_layer_preference_updates_total",
               "relay_keyframe_requests_total")
EXAMPLE = os.path.join(HERE, "example_run_manifest.json")


def part(uid, fleet="browser", observer=True, talker=False, shaped=False, join_ts=950.0, leave_ts=None):
    return {
        "user_id": uid, "fleet": fleet, "role": "test", "observer": observer, "talker": talker,
        "publishes": {"camera": True, "mic": talker, "screen": False},
        "network": {"profile": "lossy_mobile" if shaped else "none", "shaped": shaped,
                    "direction": "both" if shaped else "none", "shaper": "netem" if shaped else "none"},
        "transport_intended": "auto", "join_ts": join_ts, "leave_ts": leave_ts, "steps": ["s1"],
    }


def manifest(participants):
    m = {
        "schema": cq_manifest.SCHEMA, "run_id": "t1", "meeting_id": "scale-t1", "environment": "test",
        "code": {"commit": "abc", "images": {}}, "scenario": {"file": "x", "sha256": "y"},
        "clock": {"sync": "unknown"},
        "steps": [{"step_id": "s1", "n_target": len(participants), "join_start": JOIN, "hold_start": HS,
                   "hold_end": HE, "headline": True}],
        "participants": participants,
    }
    return cq_manifest.require_valid(m)


def sess(user, n=0):
    return f"{zlib.crc32(user.encode())}{n}"


class World:
    """Builds Prometheus query_range payloads (the real API JSON shape) for one meeting."""

    def __init__(self):
        self.series = defaultdict(list)

    def grid(self, lo=HS, hi=HE):
        t, out = lo, []
        while t <= hi:
            out.append(t)
            t += STEP
        return out

    def add(self, name, labels, value, lo=HS, hi=HE):
        labels = {"__name__": name, "meeting_id": "scale-t1", **labels}
        vals = [[t, str(value(t) if callable(value) else value)] for t in self.grid(lo, hi)]
        self.series[name].append({"metric": labels, "values": vals})

    def presence(self, user, session=None, lo=JOIN, hi=HE):
        self.add(M_SENT, {"peer_id": user, "session_id": session or sess(user)}, 50, lo, hi)

    def pair(self, name, recv, pub_session, value, recv_session=None, lo=HS, hi=HE):
        self.add(name, {"from_peer": recv, "session_id": recv_session or sess(recv), "to_peer": pub_session},
                 value, lo, hi)

    def new_session(self, user, at, n=2, prev=0):
        """Move the user's series from `at` on to a fresh session, as a rejoin does."""
        old, new = sess(user, prev), sess(user, n)
        for lst in self.series.values():
            for s in list(lst):
                lab = s["metric"]
                mine = lab.get("session_id") == old and user in (lab.get("peer_id"), lab.get("from_peer"))
                if not mine and lab.get("to_peer") != old:
                    continue
                later = [v for v in s["values"] if v[0] >= at]
                s["values"] = [v for v in s["values"] if v[0] < at]
                if not s["values"]:
                    lst.remove(s)
                moved = dict(lab, **({"session_id": new} if mine else {}))
                if lab.get("to_peer") == old:
                    moved["to_peer"] = new
                if later:
                    lst.append({"metric": moved, "values": later})

    def relay_paths(self, names=RELAY_PATHS, value=lambda t: t - JOIN, outcome="accepted"):
        for name in names:
            extra = {"outcome": outcome} if name == "relay_layer_preference_updates_total" else {}
            self.add(name, {"room": "scale-t1", **extra}, value, JOIN, HE)

    def health(self, delta):
        self.series[M_HEALTH].append({"metric": {"__name__": M_HEALTH}, "values": [
            [t, str(delta * (t - HS) / (HE - HS))] for t in self.grid()]})

    def payload(self, name):
        return {"status": "success", "data": {"resultType": "matrix", "result": self.series.get(name, [])}}

    def data(self):
        return {name: cq_prom.parse_matrix(self.payload(name)) for name in list(self.series)}

    def reporters_payload(self, query):
        inner = query.split("(", 2)[2]
        names = re.search(r'__name__=~"([^"]*)"', inner)
        families = names.group(1).split("|") if names else [inner.split("{", 1)[0]]
        reporters = sorted({x["metric"]["from_peer"] for name in families for x in self.series.get(name, [])})
        result = [{"metric": {"from_peer": r}, "values": [[t, "1"] for t in self.grid()]} for r in reporters]
        return {"status": "success", "data": {"resultType": "matrix", "result": result}}

    def transport(self, seen):
        def fake(url, body, headers):
            query = urllib.parse.parse_qs(body.decode())["query"][0]
            seen.append((url, query, dict(headers)))
            if query.startswith("count by (from_peer)"):
                return json.dumps(self.reporters_payload(query)).encode()
            payload = self.payload(query.removeprefix("min_over_time(").split("{", 1)[0])
            if query.startswith("min_over_time("):
                payload = self.payload(M_PPS)
            for label, raw in re.findall(r'(\w+)="((?:[^"\\]|\\.)*)"', query):
                payload["data"]["result"] = [x for x in payload["data"]["result"]
                                             if label not in x["metric"] or x["metric"][label] == raw]
            for label, raw in re.findall(r'(\w+)=~"((?:[^"\\]|\\.)*)"', query):
                rx = re.compile(raw.replace("\\\\", "\\"))
                payload["data"]["result"] = [x for x in payload["data"]["result"]
                                             if label not in x["metric"] or rx.fullmatch(x["metric"][label])]
            return json.dumps(payload).encode()
        return fake


def healthy_world(observers, talkers, rust_extra=0, audio_expand=0.0, fps=30.0, staleness=100.0):
    w = World()
    for u in observers + talkers:
        w.presence(u)
    for o in observers:
        for u in observers + talkers:
            if u != o:
                w.pair(M_CAN_LISTEN, o, sess(u), 1)
    for o in observers:
        for t in talkers:
            w.pair(M_PPS, o, sess(t), 50)
            w.pair(M_EXPAND, o, sess(t), audio_expand)
        for u in observers + talkers:
            if u != o:
                w.pair(M_FPS, o, sess(u), fps)
                w.pair(M_STALE, o, sess(u), staleness)
                w.pair(M_FREEZE, o, sess(u), 0)
    n_browser, n_rust = len(observers), len(talkers) + rust_extra
    w.health((HE - HS) * (n_browser / 5 + n_rust / BOT_HEALTH_INTERVAL_S))
    w.relay_paths()
    return w


def score(m, w, **kw):
    return cq_score.score_step(m, m["steps"][0], w.data(), cq_score.load_config(), **kw)


def read_json(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def quiet_main(argv, **kw):
    return main_with_stderr(argv, **kw)[0]


def main_with_stderr(argv, **kw):
    err = io.StringIO()
    with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
        rc = cli.main(argv, **kw)
    return rc, err.getvalue()


def gate(result, name, section="validity"):
    return next(g for g in result[section] if g["gate"] == name)


class ManifestValidatorTest(unittest.TestCase):
    def test_example_manifest_is_valid(self):
        self.assertEqual(cq_manifest.validate_manifest(cq_manifest.load_manifest(EXAMPLE)), [])

    def errors_for(self, mutate):
        m = cq_manifest.load_manifest(EXAMPLE)
        mutate(m)
        return "\n".join(cq_manifest.validate_manifest(m))

    def test_missing_required_field(self):
        self.assertIn("$.meeting_id: required field missing", self.errors_for(lambda m: m.pop("meeting_id")))

    def test_duplicate_user_id(self):
        def dup(m):
            m["participants"][1]["user_id"] = m["participants"][0]["user_id"]
        self.assertIn("duplicate user_id", self.errors_for(dup))

    def test_exactly_one_headline(self):
        def two(m):
            m["steps"][0]["headline"] = True
        self.assertIn("exactly one step must have headline=true (found 2)", self.errors_for(two))

    def test_window_order(self):
        def bad(m):
            m["steps"][1]["hold_end"] = m["steps"][1]["hold_start"]
        self.assertIn("join_start <= hold_start < hold_end", self.errors_for(bad))

    def test_rust_bot_cannot_observe(self):
        def bad(m):
            m["participants"][3]["observer"] = True
        self.assertIn("Rust bots cannot be observers", self.errors_for(bad))

    def test_unknown_step_and_schema_version(self):
        def bad(m):
            m["participants"][0]["steps"] = ["nope"]
            m["schema"] = "call-quality-run-manifest/v2"
        errs = self.errors_for(bad)
        self.assertIn("unknown step_id 'nope'", errs)
        self.assertIn("unsupported major version", errs)

    def test_enum_checked(self):
        def bad(m):
            m["participants"][0]["transport_intended"] = "quic"
        self.assertIn("transport_intended: must be one of", self.errors_for(bad))


class CounterDeltaTest(unittest.TestCase):
    def test_reset_baseline_and_absent_before_is_zero(self):
        pre_existing = [(HS - 30, 1.0), (HS, 5.0), (HS + 15, 7.0), (HS + 30, 2.0), (HS + 45, 4.0)]
        self.assertEqual(cq_prom.counter_delta(pre_existing, HS), 2.0 + 2.0 + 2.0)
        born_in_window = [(HS + 60, 1.0), (HS + 75, 1.0)]
        self.assertEqual(cq_prom.counter_delta(born_in_window, HS), 1.0)


class AggregationTest(unittest.TestCase):
    def test_ratio_of_sums_per_receiver_not_mean_of_ratios(self):
        late = HS + 30 * STEP
        m = manifest([part("o1"), part("t1", "rust", False, True), part("t2", "rust", False, True, join_ts=late)])
        w = World()
        for u in ("o1", "t1"):
            w.presence(u)
        w.presence("t2", lo=late)
        w.pair(M_CAN_LISTEN, "o1", sess("t1"), 1)
        w.pair(M_CAN_LISTEN, "o1", sess("t2"), 1, lo=late)
        w.pair(M_PPS, "o1", sess("t1"), 50)
        w.pair(M_EXPAND, "o1", sess("t1"), 10)
        w.pair(M_PPS, "o1", sess("t2"), 50, lo=late)
        w.pair(M_EXPAND, "o1", sess("t2"), 50, lo=late)
        r = score(m, w)
        a = r["cells"]["UxU"]["participants"]["o1"]["A"]
        self.assertAlmostEqual(a, (41 * 0.1 + 11 * 0.5) / (41 + 11))

    def test_participant_merge_across_sessions_and_reconnect_accounting(self):
        m = manifest([part("o1"), part("t1", "rust", False, True)])
        w = World()
        s1, s2 = sess("o1", 1), sess("o1", 2)
        mid = HS + 20 * STEP
        w.presence("o1", s1, JOIN, mid)
        w.presence("o1", s2, mid, HE)
        w.presence("t1")
        for s, lo, hi, expand in ((s1, HS, mid - STEP, 0.0), (s2, mid, HE, 20.0)):
            w.pair(M_CAN_LISTEN, "o1", sess("t1"), 1, recv_session=s, lo=lo, hi=hi)
            w.pair(M_PPS, "o1", sess("t1"), 50, recv_session=s, lo=lo, hi=hi)
            w.pair(M_EXPAND, "o1", sess("t1"), expand, recv_session=s, lo=lo, hi=hi)
        r = score(m, w)
        self.assertEqual(list(r["cells"]["UxU"]["participants"]), ["o1"])
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o1"]["A"], (2 * 1.0 + 21 * 0.2) / 41)
        self.assertEqual(r["stability"]["o1"]["sessions_in_hold"], 2)
        self.assertEqual(r["stability"]["o1"]["unplanned_reconnects"], 1)
        self.assertAlmostEqual(r["stability"]["o1"]["S"], 1 / (600 / 3600))

        w.add(M_REELECT, {"session_id": s2, "result": "proceeded"}, 1, lo=mid)
        r = score(m, w)
        self.assertEqual(r["stability"]["o1"]["migrations"], 1)
        self.assertEqual(r["stability"]["o1"]["unplanned_reconnects"], 0)


class CellAndFilterTest(unittest.TestCase):
    def test_headline_cell_excludes_shaped_publishers_and_receivers(self):
        m = manifest([part("u1"), part("sh", shaped=True), part("tu", "rust", False, True),
                      part("ts", "rust", False, True, shaped=True)])
        w = World()
        for u in ("u1", "sh", "tu", "ts"):
            w.presence(u)
        w.pair(M_FPS, "u1", sess("tu"), 30)
        w.pair(M_FPS, "u1", sess("ts"), 3)
        w.pair(M_FPS, "sh", sess("tu"), 3)
        r = score(m, w)
        self.assertEqual(r["cells"]["UxU"]["participants"]["u1"]["Q"], 0.0)
        self.assertEqual(r["cells"]["UxS"]["participants"]["u1"]["Q"], 1.0)
        self.assertEqual(r["cells"]["SxU"]["participants"]["sh"]["Q"], 1.0)
        self.assertNotIn("sh", r["cells"]["UxU"]["participants"])

    def test_audio_only_from_talkers_and_receive_shortfall_counts_as_loss(self):
        m = manifest([part("o1"), part("t1", "rust", False, True), part("v1", "rust", False, False)])
        w = World()
        for u in ("o1", "t1", "v1"):
            w.presence(u)
        w.pair(M_CAN_LISTEN, "o1", sess("t1"), 1)
        w.pair(M_PPS, "o1", sess("t1"), lambda t: 50 if t < HS + 30 * STEP else 40)
        w.pair(M_EXPAND, "o1", sess("t1"), 0)
        w.pair(M_PPS, "o1", sess("v1"), 50)
        w.pair(M_EXPAND, "o1", sess("v1"), 100)
        r = score(m, w)
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o1"]["A"], 11 * 0.2 / 41)
        self.assertAlmostEqual(r["cells"]["UxU"]["audio_loss_share"]["o1"], 11 / 41)

    def test_missing_audio_samples_from_a_present_talker_count_as_full_loss(self):
        m = manifest([part("o1"), part("t1", "rust", False, True)])
        w = World()
        for u in ("o1", "t1"):
            w.presence(u)
        w.pair(M_CAN_LISTEN, "o1", sess("t1"), 1)
        w.pair(M_PPS, "o1", sess("t1"), 50, hi=HS + 20 * STEP)
        w.pair(M_EXPAND, "o1", sess("t1"), 0, hi=HS + 20 * STEP)
        r = score(m, w)
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o1"]["A"], 20 / 41)

    def test_declared_talker_without_its_own_reporter_still_counts(self):
        m = manifest([part("o1"), part("t1", "rust", False, True)])
        w = World()
        w.presence("o1")
        w.presence("t1", hi=HS + 20 * STEP)
        w.pair(M_CAN_LISTEN, "o1", sess("t1"), 1)
        w.pair(M_PPS, "o1", sess("t1"), 50, hi=HS + 20 * STEP)
        w.pair(M_EXPAND, "o1", sess("t1"), 0, hi=HS + 20 * STEP)
        r = score(m, w)
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o1"]["A"], 20 / 41)
        self.assertEqual(r["diagnostics"]["talker_not_sending_samples"], 20)

    def test_rust_reporters_are_excluded(self):
        m = manifest([part("o1"), part("t1", "rust", False, True)])
        w = healthy_world(["o1"], ["t1"])
        w.pair(M_FPS, "t1", sess("o1"), 1)
        w.pair(M_PPS, "t1", sess("o1"), 50)
        w.pair(M_EXPAND, "t1", sess("o1"), 100)
        r = score(m, w)
        self.assertEqual(set(r["cells"]["UxU"]["participants"]), {"o1"})
        self.assertEqual(r["cells"]["UxU"]["participants"]["o1"]["Q"], 0.0)
        self.assertEqual(r["diagnostics"]["excluded_reporters"], ["t1"])
        self.assertEqual(gate(r, "G-V10")["status"], "fail")


class SplitTransportTest(unittest.TestCase):
    def test_cells_split_by_actual_transport(self):
        m = manifest([part("o1"), part("o2"), part("t1", "rust", False, True)])
        w = healthy_world(["o1", "o2"], ["t1"])
        w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1"), "server_type": "webtransport"}, 40)
        w.add(M_RTT, {"peer_id": "o2", "session_id": sess("o2"), "server_type": "websocket"}, 60)
        r = score(m, w, split_transport=True)
        self.assertEqual(set(r["cells"]), {"UxU", "UxU/webtransport", "UxU/websocket"})
        self.assertEqual(r["headline_cell"], "UxU")
        self.assertTrue(r["cells"]["UxU/websocket"]["report_only"])

    def test_split_transport_still_gates_every_observer(self):
        obs = [f"o{i:02d}" for i in range(12)]
        m = manifest([part(o) for o in obs] + [part("t1", "rust", False, True)])
        for no_rtt in (True, False):
            w = healthy_world(obs, ["t1"])
            for o in obs:
                if not (no_rtt and o in ("o00", "o01")):
                    w.add(M_RTT, {"peer_id": o, "session_id": sess(o), "server_type": "webtransport"}, 40)
            for s in w.series[M_EXPAND]:
                if s["metric"]["from_peer"] in ("o00", "o01"):
                    s["values"] = [[ts, "100"] for ts, _ in s["values"]]
            r = score(m, w, split_transport=True)
            self.assertEqual(r["verdict"], "FAIL", no_rtt)
            self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "fail", no_rtt)
            self.assertEqual(r["cells"]["UxU"]["n"], 12, no_rtt)


class GateTest(unittest.TestCase):
    def observers(self, n):
        return [f"o{i:02d}" for i in range(n)]

    def build(self, n=10, expand=0.0, health_ratio=1.0, bad_observers=()):
        obs = self.observers(n)
        m = manifest([part(o) for o in obs] + [part("t1", "rust", False, True)])
        w = healthy_world(obs, ["t1"], audio_expand=expand)
        if health_ratio != 1.0:
            w.series[M_HEALTH].clear()
            w.health((HE - HS) * (n / 5 + 1 / BOT_HEALTH_INTERVAL_S) * health_ratio)
        for o in bad_observers:
            w.series[M_EXPAND] = [s for s in w.series[M_EXPAND] if s["metric"]["from_peer"] != o]
            w.pair(M_EXPAND, o, sess("t1"), 6.0)
        return m, w

    def test_healthy_run_passes(self):
        m, w = self.build()
        r = score(m, w)
        self.assertEqual(r["verdict"], "PASS")

    def test_audio_p95_in_red_fails(self):
        m, w = self.build(expand=10.0)
        r = score(m, w)
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "fail")
        self.assertEqual(r["verdict"], "FAIL")

    def test_k_in_red_fails_even_when_p95_is_green(self):
        m, w = self.build(n=40, bad_observers=("o00", "o01"))
        r = score(m, w)
        dim = r["cells"]["UxU"]["dimensions"]["A"]
        self.assertLess(dim["p95"], 0.05)
        self.assertEqual(dim["k_red"], 2)
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "fail")

    def test_incomplete_pipeline_is_invalid_not_failed(self):
        m, w = self.build(expand=10.0, health_ratio=0.5)
        r = score(m, w)
        self.assertEqual(gate(r, "G-V1")["status"], "fail")
        self.assertEqual(r["verdict"], "INVALID")

    def test_too_few_observers_is_invalid(self):
        m, w = self.build(n=4)
        self.assertEqual(score(m, w)["verdict"], "INVALID")

    def test_hidden_participant_by_peer_cap_invalidates(self):
        m, w = self.build()
        w.series = defaultdict(list, {k: [s for s in v if s["metric"].get("to_peer") != sess("t1")]
                                      for k, v in w.series.items()})
        r = score(m, w)
        g = gate(r, "G-V6")
        self.assertEqual(g["status"], "fail")
        self.assertIn("t1", g["detail"])
        self.assertEqual(r["verdict"], "INVALID")

    def test_late_join_and_drop_fail(self):
        m, w = self.build()
        w.series[M_SENT] = [s for s in w.series[M_SENT] if s["metric"]["peer_id"] != "o03"]
        w.presence("o03", lo=HS + 60)
        w.add(M_REELECT, {"session_id": sess("o04"), "result": "failed"}, 1, lo=HS + 105)
        r = score(m, w)
        self.assertIn("o03", gate(r, "G-Q1", "quality_gates")["detail"])
        self.assertIn("o04", gate(r, "G-Q2", "quality_gates")["detail"])
        self.assertEqual(r["verdict"], "FAIL")

    def test_receive_shortfall_is_scored_as_audio_loss_and_fails(self):
        obs = self.observers(10)
        m = manifest([part(o) for o in obs] + [part("t1", "rust", False, True)])
        w = healthy_world(obs, ["t1"], audio_expand=50.0)
        w.series[M_PPS] = []
        for o in obs:
            w.pair(M_PPS, o, sess("t1"), lambda t: 50 if t < HS + 20 * STEP else 30)
        r = score(m, w)
        self.assertEqual(gate(r, "G-V9")["status"], "pass")
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "fail")
        self.assertNotIn("A", r["cells"]["UxU"]["absent_dimensions"])
        self.assertEqual(r["verdict"], "FAIL")

    def test_no_talker_declared_is_invalid(self):
        obs = self.observers(10)
        m = manifest([part(o) for o in obs] + [part("v1", "rust", False, False)])
        w = healthy_world(obs, ["v1"])
        r = score(m, w)
        self.assertIn("no talker declared", gate(r, "G-V9")["detail"])
        self.assertEqual(r["verdict"], "INVALID")

    def test_missing_required_dimension_is_invalid(self):
        m, w = self.build()
        w.series.pop(M_FPS)
        r = score(m, w)
        self.assertEqual(r["cells"]["UxU"]["missing_required_dimensions"], ["V", "Q"])
        self.assertEqual(r["verdict"], "INVALID")


class BlockerRegressionTest(unittest.TestCase):
    def run_world(self, n=10, talker_pps=50.0, expand=0.0, drop=(), shaped_obs=0, shaped_pps=50.0):
        obs = [f"o{i:02d}" for i in range(n)]
        sh = [f"s{i:02d}" for i in range(shaped_obs)]
        m = manifest([part(o) for o in obs] + [part(x, shaped=True) for x in sh] + [part("t1", "rust", False, True)])
        w = healthy_world(obs + sh, ["t1"], audio_expand=expand)
        w.series[M_PPS] = []
        for o in obs:
            w.pair(M_PPS, o, sess("t1"), talker_pps)
        for x in sh:
            w.pair(M_PPS, x, sess("t1"), shaped_pps)
        for name in drop:
            w.series.pop(name, None)
        return m, w

    def test_b1_receive_loss_from_a_continuous_talker_is_bad_audio(self):
        m, w = self.run_world(talker_pps=20.0, expand=100.0)
        r = score(m, w)
        self.assertNotIn("A", r["cells"]["UxU"]["absent_dimensions"])
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "fail")
        self.assertEqual(r["verdict"], "FAIL")

    def test_b1_shaped_receivers_cannot_remove_audio_from_the_headline(self):
        m, w = self.run_world(shaped_obs=4, shaped_pps=30.0)
        r = score(m, w)
        self.assertNotIn("A", r["cells"]["UxU"]["absent_dimensions"])
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "pass")
        self.assertEqual(r["verdict"], "PASS")

    def test_b1_declared_talkers_without_audio_series_is_invalid(self):
        m, w = self.run_world(drop=(M_PPS, M_EXPAND))
        self.assertEqual(score(m, w)["verdict"], "INVALID")

    def test_b2_missing_freeze_and_staleness_is_invalid(self):
        m, w = self.run_world(drop=(M_FREEZE, M_STALE))
        r = score(m, w)
        self.assertEqual(r["verdict"], "INVALID")
        self.assertIn("V", json.dumps(r["validity"]))

    def test_b2_missing_health_counter_is_invalid(self):
        m, w = self.run_world(drop=(M_HEALTH,))
        r = score(m, w)
        self.assertEqual(gate(r, "G-V1")["status"], "fail")
        self.assertEqual(r["verdict"], "INVALID")

    def test_b3_failed_reelection_series_born_at_one_is_a_drop(self):
        m, w = self.run_world()
        w.add(M_REELECT, {"session_id": sess("o03"), "result": "failed"}, 1, lo=HS + 120)
        r = score(m, w)
        self.assertIn("o03", gate(r, "G-Q2", "quality_gates")["detail"])
        self.assertEqual(r["verdict"], "FAIL")

    def test_b3_proceeded_series_born_at_one_on_new_session_is_a_migration(self):
        m, w = self.run_world()
        s2 = sess("o02", 2)
        mid = HS + 20 * STEP
        w.presence("o02", s2, mid, HE)
        w.add(M_REELECT, {"session_id": s2, "result": "proceeded"}, 1, lo=mid)
        r = score(m, w)
        self.assertEqual(r["stability"]["o02"]["migrations"], 1)
        self.assertEqual(r["stability"]["o02"]["unplanned_reconnects"], 0)

    def test_m2_real_meeting_with_no_data_is_an_error(self):
        rc, err = main_with_stderr(["--meeting", "no-such-meeting", "--window", f"{HS},{HE}", "--prom-url",
                                    "http://prom.invalid"], transport=World().transport([]), environ={})
        self.assertEqual(rc, cli.EXIT_ERROR)
        self.assertIn("no participants found for meeting 'no-such-meeting'", err)

    def test_m2_real_meeting_without_pair_series_is_an_error(self):
        w = World()
        w.presence("alice@example.com")
        rc, err = main_with_stderr(["--meeting", "scale-t1", "--window", f"{HS},{HE}", "--prom-url",
                                    "http://prom.invalid"], transport=w.transport([]), environ={})
        self.assertEqual(rc, cli.EXIT_ERROR)
        self.assertIn("no per-pair quality series", err)

    def test_b2_gate_outside_the_not_measured_allow_list_blocks(self):
        m, w = self.run_world()
        cfg = cq_score.load_config()
        cfg["validity_gates"]["allowed_not_measured"]["value"] = ["G-V2", "G-V3", "G-V4"]
        r = cq_score.score_step(m, m["steps"][0], w.data(), cfg)
        self.assertEqual(r["invalid_reasons"], ["G-V5"])
        self.assertEqual(r["verdict"], "INVALID")
        self.assertEqual(score(m, w)["verdict"], "PASS")

    def test_m3_g_v8_detects_unmapped_and_unexpected_identities(self):
        m, w = self.run_world()
        w.series[M_SENT] = [x for x in w.series[M_SENT] if x["metric"]["peer_id"] != "o05"]
        w.series[cq_score.M_CAN_LISTEN] = [x for x in w.series[cq_score.M_CAN_LISTEN]
                                           if x["metric"]["to_peer"] != sess("o05")]
        w.presence("intruder@example.com")
        r = score(m, w)
        g = gate(r, "G-V8")
        self.assertEqual(g["status"], "fail")
        self.assertIn("o05", g["detail"])
        self.assertIn("intruder@example.com", g["detail"])

    def test_m3_excluded_reporters_come_from_unfiltered_data(self):
        m, w = self.run_world()
        w.pair(cq_score.M_CAN_LISTEN, "t1", sess("o00"), 1)
        seen = []
        client = cq_prom.PromClient("http://prom.invalid", transport=w.transport(seen))
        data = cq_score.fetch_step_data(client, m, m["steps"][0], cq_score.load_config(),
                                        {p["user_id"] for p in m["participants"] if p["observer"]})
        r = cq_score.score_step(m, m["steps"][0], data, cq_score.load_config())
        self.assertEqual(r["diagnostics"]["excluded_reporters"], ["t1"])


class TeamDecisionTest(unittest.TestCase):
    """Discussion #2913 team decisions T1-T3, T7, T8, T15 and manifest additions (doc v0.4)."""

    def world(self, n=10, **kw):
        obs = [f"o{i:02d}" for i in range(n)]
        m = manifest([part(o) for o in obs] + [part("t1", "rust", False, True)])
        return m, healthy_world(obs, ["t1"], **kw), obs

    drop = staticmethod(lambda w, fams, keep, window: NeverPassOnMissingDataTest.drop(w, fams, keep, window))

    def replace_pair(self, w, name, recv, value):
        w.series[name] = [s for s in w.series[name]
                          if (s["metric"]["from_peer"], s["metric"]["to_peer"]) != (recv, sess("t1"))]
        w.pair(name, recv, sess("t1"), value)

    def test_t1_no_trend_score_and_a_p95_table_per_cell(self):
        m, w, _ = self.world()
        r = score(m, w)
        self.assertNotIn("weights", cq_score.load_config())
        self.assertNotIn("trend_score", r["cells"]["UxU"])
        self.assertEqual(set(r["p95_table"]["UxU"]), set(cq_score.DIMENSIONS) | {"staleness_ms"})
        self.assertEqual(r["p95_table"]["UxU"]["staleness_ms"], 100.0)

    def test_t1_amber_dimension_without_a_tripped_gate_passes(self):
        m, w, _ = self.world(audio_expand=4.0)
        r = score(m, w)
        self.assertAlmostEqual(r["p95_table"]["UxU"]["A"], 0.04)
        self.assertEqual(r["verdict"], "PASS")

    def test_t2_one_receiver_with_zero_packets_is_a_split_and_fails(self):
        m, w, _ = self.world(n=20)
        self.replace_pair(w, M_PPS, "o00", lambda t: 0 if HS + 10 * STEP <= t < HS + 14 * STEP else 50)
        r = score(m, w)
        self.assertEqual((r["split"]["split"], r["split"]["healthy"]), (4, 37))
        self.assertEqual(r["split"]["zero_packet_receivers"], {"o00 <- t1": 4})
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "pass")
        self.assertEqual(gate(r, "G-Q7", "quality_gates")["status"], "fail")
        self.assertEqual(r["verdict"], "FAIL")

    def test_t2_room_wide_loss_is_not_a_split_but_fails_audio(self):
        m, w, obs = self.world()
        for o in obs:
            self.replace_pair(w, M_PPS, o, lambda t: 0 if t < HS + 10 * STEP else 50)
        r = score(m, w)
        self.assertEqual((r["split"]["split"], r["split"]["none_received"]), (0, 10))
        self.assertEqual(gate(r, "G-Q7", "quality_gates")["status"], "pass")
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "fail")

    def test_t2_real_meeting_buckets_nobody_receives_are_muted_not_split(self):
        users = ["alice@example.com", "bob@example.com", "carol@example.com"]
        m = cq_manifest.synthesize_real_meeting_manifest("scale-t1", HS, HE, users)
        w = World()
        for u in users:
            w.presence(u)
        for r_ in ("alice@example.com", "carol@example.com"):
            w.pair(M_CAN_LISTEN, r_, sess("bob@example.com"), 1)
        w.pair(M_PPS, "alice@example.com", sess("bob@example.com"), lambda t: 50 if t < HS + 20 * STEP else 0)
        w.pair(M_PPS, "carol@example.com", sess("bob@example.com"),
               lambda t: 50 if t < HS + 18 * STEP else 0)
        s = cq_score.score_step(m, m["steps"][0], w.data(), cq_score.load_config(), mode="real")["split"]
        self.assertEqual((s["healthy"], s["split"]), (18, 2))
        self.assertGreaterEqual(s["none_received"], 21)

    def drop_pair(self, w, recv, lo):
        for k, v in w.series.items():
            for s in v:
                if (s["metric"].get("from_peer"), s["metric"].get("to_peer")) == (recv, sess("t1")):
                    s["values"] = [x for x in s["values"] if x[0] < lo]

    def test_t3_receiver_pruning_a_talker_below_the_cap_is_loss(self):
        m, w, _ = self.world(n=20)
        self.drop_pair(w, "o00", HS + 20 * STEP)
        r = score(m, w)
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o00"]["A"], 21 / 41)
        self.assertEqual(r["diagnostics"]["coverage_ambiguous_samples"], 0)
        self.assertEqual(r["split"]["zero_packet_receivers"], {"o00 <- t1": 21})
        self.assertEqual(r["verdict"], "FAIL")

    def test_t3_room_wide_prune_while_the_talker_sends_fails(self):
        m, w, obs = self.world()
        for o in obs:
            self.drop_pair(w, o, HS + 20 * STEP)
        r = score(m, w)
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "fail")
        self.assertEqual(r["verdict"], "FAIL")

    def test_rr2_a_declared_cap_in_the_manifest_excuses_nothing(self):
        m, w, _ = self.world()
        m["peer_stats_cap"] = {"limit": 1, "selection": "lexicographic"}
        self.drop_pair(w, "o00", HS + 20 * STEP)
        r = score(m, w)
        self.assertEqual(r["diagnostics"]["coverage_ambiguous_samples"], 0)
        self.assertEqual(r["verdict"], "FAIL")

    def test_rr2_a_pair_missing_from_a_report_at_the_observed_cap_is_invalid(self):
        m, w, _ = self.world()
        for k in range(cq_score.DEPLOYED_PEER_STATS_CAP):
            w.pair(M_CAN_LISTEN, "o00", f"9{k:05d}", 1)
        self.drop_pair(w, "o00", HS + 20 * STEP)
        r = score(m, w)
        g = gate(r, "G-V12")
        self.assertEqual(g["status"], "fail")
        self.assertIn("coverage: can't distinguish loss from peer_stats cap", g["detail"])
        self.assertEqual(r["verdict"], "INVALID")

    def test_m3_stale_pps_gauge_with_can_listen_0_is_loss(self):
        m, w, _ = self.world(n=20)
        self.replace_pair(w, M_CAN_LISTEN, "o00", lambda t: 0 if t >= HS + 30 * STEP else 1)
        r = score(m, w)
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o00"]["A"], 11 / 41)
        self.assertEqual(r["split"]["zero_packet_receivers"], {"o00 <- t1": 11})

    def test_b2_talker_health_lost_mid_hold_is_a_drop_and_loss_not_mute(self):
        m, w, obs = self.world()
        w.series[M_SENT] = [s for s in w.series[M_SENT] if s["metric"]["peer_id"] != "t1"]
        w.presence("t1", hi=HS + 20 * STEP)
        for o in obs:
            self.replace_pair(w, M_PPS, o, lambda t: 50 if t < HS + 21 * STEP else 0)
        r = score(m, w)
        self.assertIn("t1", gate(r, "G-Q2", "quality_gates")["detail"])
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "fail")
        self.assertEqual(r["verdict"], "FAIL")

    def test_t2_latency_is_not_gated_until_2948_and_staleness_is_diagnostic(self):
        m, w, _ = self.world(staleness=5000.0)
        r = score(m, w)
        self.assertEqual(gate(r, "G-Q5", "quality_gates")["status"], "not_measured")
        self.assertEqual(r["notices"], ["latency not gated: #2948 pending", "G-Q9 unplanned-reconnect gate: proposal, team decision pending (#2913)"])
        self.assertEqual(r["verdict"], "PASS")
        self.assertEqual(r["diagnostics"]["max_staleness_ms"], 5000.0)
        md = cq_report.render_markdown({"run": {"meeting_id": "x", "mode": "run", "bands_version": "b",
                                                "commit": "c"}, "verdict": r["verdict"],
                                        "headline_step": "s1", "steps": [r]})
        self.assertIn("> **LATENCY NOT GATED: #2948 PENDING**", md.splitlines()[:4])

    def latency_cfg(self, metric="test_audio_delay_ms", allowed=True):
        cfg = cq_score.load_config()
        cfg["latency_gate"]["audio_delay_metric"]["value"] = metric
        cfg["latency_gate"]["allowed_unmeasured"]["value"] = allowed
        return cfg

    def test_t2_latency_gate_switches_on_via_config(self):
        m, w, obs = self.world()
        m["clock"]["sync"] = "chrony"
        for o in obs:
            w.pair("test_audio_delay_ms", o, sess("t1"), 500.0)
        r = cq_score.score_step(m, m["steps"][0], w.data(), self.latency_cfg())
        self.assertEqual(gate(r, "G-Q5", "quality_gates")["status"], "fail")
        self.assertEqual(r["notices"], ["G-Q9 unplanned-reconnect gate: proposal, team decision pending (#2913)"])
        self.assertEqual(r["verdict"], "FAIL")
        m["clock"]["sync"] = "unknown"
        r = cq_score.score_step(m, m["steps"][0], w.data(), self.latency_cfg())
        self.assertEqual(r["p95_table"]["UxU"]["L"], 0.0)

    def test_t2_latency_configured_but_absent_or_waiver_off_is_invalid(self):
        m, w, _ = self.world()
        r = cq_score.score_step(m, m["steps"][0], w.data(), self.latency_cfg())
        self.assertIn("G-Q5", r["invalid_reasons"])
        self.assertEqual(r["verdict"], "INVALID")
        r = cq_score.score_step(m, m["steps"][0], w.data(), self.latency_cfg(metric=None, allowed=False))
        self.assertEqual(r["invalid_reasons"], ["G-Q5"])

    def test_t7_any_per_pair_series_from_a_rust_bot_invalidates(self):
        m, w, _ = self.world()
        w.pair(M_FPS, "t1", sess("o00"), 30)
        client = cq_prom.PromClient("http://prom.invalid", transport=w.transport([]))
        data = cq_score.fetch_step_data(client, m, m["steps"][0], cq_score.load_config(),
                                        {p["user_id"] for p in m["participants"] if p["observer"]})
        r = cq_score.score_step(m, m["steps"][0], data, cq_score.load_config())
        self.assertEqual(r["verdict"], "INVALID")
        self.assertIn("t1", gate(r, "G-V10")["detail"])

    def test_t15_relay_path_not_exercised_invalidates_and_is_configurable(self):
        m, w, _ = self.world()
        self.assertEqual(tuple(cq_score.required_relay_paths(cq_score.load_config())), RELAY_PATHS)
        for name in RELAY_PATHS:
            w.series.pop(name)
        w.relay_paths([p for p in RELAY_PATHS if p != "relay_layer_filtered_total"])
        w.series["relay_keyframe_requests_total"][0]["values"] = [[t, "7"] for t in w.grid(JOIN, HE)]
        r = score(m, w)
        g = gate(r, "G-V11")
        self.assertEqual(g["status"], "fail")
        self.assertIn("path not exercised", g["detail"])
        self.assertIn("relay_layer_filtered_total", g["detail"])
        self.assertIn("relay_keyframe_requests_total", g["detail"])
        self.assertEqual(r["verdict"], "INVALID")
        cfg = cq_score.load_config()
        cfg["validity_gates"]["relay_path_exemptions"]["value"] = {
            "relay_layer_filtered_total": "pin-layer 0", "relay_keyframe_requests_total": "no loss injected"}
        r = cq_score.score_step(m, m["steps"][0], w.data(), cfg)
        self.assertEqual(r["verdict"], "PASS")
        self.assertIn("relay path exempted: relay_layer_filtered_total (pin-layer 0)", r["notices"])
        queries = [q for _, q, _, _ in cq_score.step_queries(m, m["steps"][0], cfg, None)]
        self.assertIn('relay_viewport_filtered_total{room="scale-t1"}', queries)
        self.assertIn('relay_layer_preference_updates_total{room="scale-t1",outcome="accepted"}', queries)
        self.assertFalse(any(q.startswith("relay_layer_filtered_total") for q in queries))

    def real_meeting(self, extra_args=(), environ=None):
        users = ["alice@example.com", "bob@example.com"]
        w = healthy_world(users, [], audio_expand=0.0)
        w.pair(M_PPS, users[0], sess(users[1]), 50)
        w.pair(M_FPS, users[0], sess(users[1]), 30)
        w.add(cq_score.M_PEER_INFO, {"peer_id": users[1], "session_id": sess(users[1]), "display_name": "Bob B"}, 1)
        with tempfile.TemporaryDirectory() as tmp:
            rc, err = main_with_stderr(["--meeting", "scale-t1", "--window", f"{HS},{HE}", "--prom-url",
                                        "http://prom.invalid", "--exclude-regex", "^dave@example\\.com$",
                                        "--out-dir", tmp, *extra_args], transport=w.transport([]),
                                       environ=environ or {})
            if rc != 0:
                return rc, err, None, None
            with open(os.path.join(tmp, "report.md"), encoding="utf-8") as fh:
                return rc, err, read_json(os.path.join(tmp, "result.json")), fh.read()

    def test_t7_g_v1_counts_session_level_bot_health_at_5_s(self):
        obs = [f"o{i:02d}" for i in range(10)]
        viewers = [f"v{i}" for i in range(4)]
        m = manifest([part(o) for o in obs] + [part("t1", "rust", False, True)]
                     + [dict(part(v, "rust", False, False), publishes={"camera": False, "mic": False, "screen": False})
                        for v in viewers])
        w = healthy_world(obs, ["t1"])
        for v in viewers:
            w.presence(v)
        w.series[M_HEALTH].clear()
        w.health((HE - HS) * (len(obs) / 5 + 5 / BOT_HEALTH_INTERVAL_S))
        r = score(m, w)
        self.assertEqual(gate(r, "G-V1")["value"], 1.0)
        self.assertEqual(gate(r, "G-V10")["status"], "pass")
        self.assertEqual(r["verdict"], "PASS")

    def test_t8_real_meeting_identities_are_pseudonymised_by_default(self):
        rc, _, result, report = self.real_meeting()
        self.assertEqual(rc, 0)
        blob = json.dumps(result) + report
        for raw in ("alice@example.com", "bob@example.com", "dave@example", "Bob B"):
            self.assertNotIn(raw, blob)
        step = result["steps"][0]
        self.assertTrue(result["run"]["pseudonymised"])
        self.assertEqual(set(step["cells"]["UxU"]["participants"]) & set(step["stability"]),
                         set(step["cells"]["UxU"]["participants"]))
        self.assertTrue(all(u.startswith("p-") for u in step["stability"]))
        rc, _, result, _ = self.real_meeting(["--no-pseudonymise"])
        self.assertIn("alice@example.com", result["steps"][0]["stability"])

    def test_t8_no_pseudonymise_is_refused_in_ci(self):
        rc, err, _, _ = self.real_meeting(["--no-pseudonymise"], environ={"CI": "true"})
        self.assertEqual(rc, cli.EXIT_ERROR)
        self.assertIn("refused in CI", err)

    def test_t8_run_mode_hides_identities_outside_the_manifest_but_keeps_bot_ids(self):
        import cq_privacy
        m, w, _ = self.world()
        w.presence("intruder@example.com")
        data = w.data()
        r = cq_score.score_step(m, m["steps"][0], data, cq_score.load_config())
        hidden = cq_privacy.identities_to_hide(m, {"s1": data}, real=False)
        self.assertEqual(hidden, {"intruder@example.com"})
        out = cq_privacy.pseudonymise(r, hidden, b"salt")
        detail = gate(out, "G-V8")["detail"]
        self.assertNotIn("intruder@example.com", detail)
        self.assertIn(cq_privacy.pseudonym("intruder@example.com", b"salt"), detail)
        self.assertIn("o00", out["stability"])

    def test_m3_minor_human_observer_ids_are_hidden_in_printed_queries(self):
        m = cq_manifest.load_manifest(EXAMPLE)
        m["participants"][0]["fleet"] = "human"
        m["participants"][0]["user_id"] = "alice@example.com"
        out = io.StringIO()
        with tempfile.TemporaryDirectory() as tmp:
            man = os.path.join(tmp, "m.json")
            with open(man, "w") as fh:
                json.dump(m, fh)
            with contextlib.redirect_stdout(out):
                rc = cli.main(["--manifest", man, "--print-queries"], environ={})
        self.assertEqual(rc, 0)
        self.assertNotIn("alice", out.getvalue())
        self.assertIn("probe-001@bots-app", out.getvalue())

    def test_m2_cli_rejects_a_waiving_config(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "o.json")
            with open(path, "w") as fh:
                json.dump({"validity_gates": {"allowed_not_measured": {"value": ["G-Q5"]}}}, fh)
            rc, err = main_with_stderr(["--manifest", EXAMPLE, "--config", path, "--validate-only"])
        self.assertEqual(rc, cli.EXIT_ERROR)
        self.assertIn("allowed_not_measured", err)

    def test_manifest_accepts_stagger_placement_and_absent_leave_ts(self):
        m = cq_manifest.load_manifest(EXAMPLE)
        m["participants"][0].pop("leave_ts")
        self.assertEqual(cq_manifest.validate_manifest(m), [])
        m["participants"][0]["placement"] = {"ordinal": -1, "node": "n"}
        m["participants"][1]["stagger_ms"] = "soon"
        m["participants"][2]["placement"] = None
        errs = "\n".join(cq_manifest.validate_manifest(m))
        self.assertIn("placement.ordinal: must be a non-negative integer", errs)
        self.assertIn("stagger_ms: must be a num", errs)
        self.assertEqual(len(cq_manifest.validate_manifest(m)), 2)

    def vanishing(self, last, events=(), rejoin_at=None, n_target=11):
        obs = [f"o{i:02d}" for i in range(10)]
        parts = [part(o) for o in obs] + [part("t1", "rust", False, True)]
        del parts[3]["leave_ts"]
        m = manifest(parts)
        m["steps"][0]["n_target"] = n_target
        m["events"] = list(events)
        w = healthy_world(obs, ["t1"])
        w.series[M_SENT] = [s for s in w.series[M_SENT] if s["metric"]["peer_id"] != "o03"]
        if rejoin_at is None:
            w.presence("o03", hi=last)
        else:
            w.presence("o03", hi=events[0]["at"])
            w.presence("o03", sess("o03", 2), lo=rejoin_at, hi=last)
        return score(m, w)

    def ev(self, at, action, who):
        return {"at": at, "step_id": "s1", "action": action, "participants": [who]}

    def test_b3_absent_leave_ts_mid_hold_without_a_leave_event_is_a_drop(self):
        r = self.vanishing(HS + 60)
        self.assertIn("o03", gate(r, "G-Q2", "quality_gates")["detail"])
        self.assertIn("counted as present (a drop)", r["flags"][0])
        self.assertEqual(r["verdict"], "FAIL")

    def test_b3_absent_leave_ts_at_teardown_or_at_a_final_leave_event_is_planned(self):
        r = self.vanishing(HE)
        self.assertEqual(gate(r, "G-Q2", "quality_gates")["status"], "pass")
        self.assertIn("o03: leave_ts absent, treated as left", r["flags"][0])
        r = self.vanishing(HS + 60, [self.ev(HS + 60, "leave", "o03")], n_target=10)
        self.assertEqual(gate(r, "G-Q2", "quality_gates")["status"], "pass")
        self.assertEqual(r["verdict"], "PASS")

    def test_rr2_b3_a_crash_after_a_declared_leave_and_rejoin_is_a_drop(self):
        events = [self.ev(HS + 60, "leave", "o03"), self.ev(HS + 75, "rejoin", "o03")]
        r = self.vanishing(HS + 300, events, rejoin_at=HS + 75)
        self.assertIn("o03", gate(r, "G-Q2", "quality_gates")["detail"])
        self.assertEqual(r["verdict"], "FAIL")
        r = self.vanishing(HE, events, rejoin_at=HS + 75)
        self.assertEqual(r["stability"]["o03"]["unplanned_reconnects"], 0)
        self.assertEqual(r["verdict"], "PASS")

    def test_rr2_b3_a_declared_rejoin_that_never_happens_is_a_drop(self):
        events = [self.ev(HS + 60, "leave", "o03"), self.ev(HS + 75, "rejoin", "o03")]
        r = self.vanishing(HS + 60, events)
        self.assertIn("counted as present (a drop)", r["flags"][0])
        self.assertIn("o03", gate(r, "G-Q2", "quality_gates")["detail"])

    def test_rr2_split_reads_a_missing_can_listen_sample_as_not_hearing(self):
        m, w, _ = self.world(n=20)
        for s in w.series[M_CAN_LISTEN]:
            if (s["metric"]["from_peer"], s["metric"]["to_peer"]) == ("o00", sess("t1")):
                s["values"] = [v for v in s["values"] if v[0] < HS + 30 * STEP]
        self.assertEqual(score(m, w)["split"]["zero_packet_receivers"], {"o00 <- t1": 11})

    def test_rr2_talker_absence_is_excused_only_inside_its_declared_leave_window(self):
        m, w, obs = self.world()
        m["events"] = [self.ev(HS + 10 * STEP, "leave", "t1"), self.ev(HS + 20 * STEP, "rejoin", "t1")]
        self.drop(w, (M_SENT,), lambda lab: lab.get("peer_id") == "t1", (HS + 10 * STEP + 1, HS + 20 * STEP))
        w.new_session("t1", HS + 20 * STEP)
        for o in obs:
            self.replace_pair(w, M_PPS, o, lambda t: 0 if HS + 10 * STEP <= t < HS + 20 * STEP else 50)
        self.assertEqual(score(m, w)["verdict"], "PASS")
        for o in obs:
            self.replace_pair(w, M_PPS, o, lambda t: 0 if HS + 10 * STEP <= t < HS + 25 * STEP else 50)
        self.assertEqual(score(m, w)["verdict"], "FAIL")

    def test_rr2_m1_mute_events_are_validated_not_excused(self):
        m, w, _ = self.world()
        m["participants"][0]["publishes"]["mic"] = True
        ok = [self.ev(HS + 60, "mute", "o00"), self.ev(HS + 120, "unmute", "o00")]
        m["events"] = ok
        self.assertEqual(score(m, w)["verdict"], "PASS")
        bad = {
            "talker": [self.ev(HS + 60, "mute", "t1"), self.ev(HS + 120, "unmute", "t1")],
            "no mic": [self.ev(HS + 60, "mute", "o01"), self.ev(HS + 120, "unmute", "o01")],
            "unpaired": [self.ev(HS + 60, "mute", "o00"), self.ev(HS + 90, "mute", "o00")],
            "outside step": [self.ev(JOIN - 5, "mute", "o00"), self.ev(HS + 120, "unmute", "o00")],
            "rejoin first": [self.ev(HS + 60, "rejoin", "o00")],
        }
        for label, events in bad.items():
            m["events"] = events
            r = score(m, w)
            self.assertEqual(gate(r, "G-V13")["status"], "fail", label)
            self.assertEqual(r["verdict"], "INVALID", label)

    def test_rr2_m2_config_cannot_widen_excuses_beyond_built_in_limits(self):
        cases = [{"validity_gates": {"health_reports_min_ratio": {"value": 0.5}}},
                 {"validity_gates": {"health_reports_max_ratio": {"value": 3}}},
                 {"validity_gates": {"n_min_observers": {"value": 2}}},
                 {"validity_gates": {"relay_path_exemptions": {"value": {p: "x" for p in RELAY_PATHS[:3]}}}}]
        for override in cases:
            with tempfile.TemporaryDirectory() as tmp:
                path = os.path.join(tmp, "o.json")
                with open(path, "w") as fh:
                    json.dump(override, fh)
                with self.assertRaises(cq_score.ConfigError, msg=override):
                    cq_score.load_config(path)

    def test_m1_g_v1_has_an_upper_bound_and_catches_silent_reporters(self):
        obs = [f"o{i:02d}" for i in range(10)]
        bots = [f"b{i:02d}" for i in range(50)]
        m = manifest([part(o) for o in obs] + [part("t1", "rust", False, True)]
                     + [part(b, "rust", False, False) for b in bots])
        w = healthy_world(obs, ["t1"])
        for b in bots:
            w.presence(b)
        w.series[M_SENT] = [s for s in w.series[M_SENT] if s["metric"]["peer_id"] not in obs[:2]]
        w.series[M_HEALTH].clear()
        w.health((HE - HS) * 51 / 1)
        g = gate(score(m, w), "G-V1")
        self.assertEqual(g["status"], "fail")
        self.assertIn("ratio outside", g["detail"])
        self.assertIn("o00, o01", g["detail"])

    def test_m2_config_cannot_waive_fail_closed_checks(self):
        cases = [{"validity_gates": {"required_dimensions": {"value": ["A", "Q", "S"]}}},
                 {"validity_gates": {"allowed_not_measured": {"value": ["G-V2", "G-Q4"]}}},
                 {"validity_gates": {"relay_path_exemptions": {"value": {"relay_keyframe_requests_total": ""}}}}]
        for override in cases:
            with tempfile.TemporaryDirectory() as tmp:
                path = os.path.join(tmp, "o.json")
                with open(path, "w") as fh:
                    json.dump(override, fh)
                with self.assertRaises(cq_score.ConfigError, msg=override):
                    cq_score.load_config(path)

    def test_m2_and_m1_minor_overrides_and_notices_reach_the_top_of_the_result(self):
        m, w, _ = self.world()
        cfg = cq_score.load_config()
        cfg["quality_gates"]["k_red_fail"]["value"] = 3
        result = cq_score.score_run(m, {"s1": w.data()}, cfg)
        self.assertEqual(result["notices"], ["G-Q9 unplanned-reconnect gate: proposal, team decision pending (#2913)", "latency not gated: #2948 pending"])
        self.assertEqual(len(result["config_overrides"]), 1)
        self.assertIn("quality_gates.k_red_fail", result["config_overrides"][0])
        md = cq_report.render_markdown(result)
        self.assertIn("quality_gates.k_red_fail", md.split("## Step")[0])

    def test_m5_preference_updates_count_only_when_accepted(self):
        m, w, _ = self.world()
        w.series.pop("relay_layer_preference_updates_total")
        w.relay_paths(["relay_layer_preference_updates_total"], outcome="rate_limited")
        self.assertIn("relay_layer_preference_updates_total", gate(score(m, w), "G-V11")["detail"])

    def test_m2_minor_an_identity_equal_to_a_verdict_word_does_not_corrupt_the_result(self):
        import cq_privacy
        m, w, _ = self.world()
        r = cq_score.score_run(m, {"s1": w.data()}, cq_score.load_config())
        out = cq_privacy.pseudonymise(r, {"PASS", "pass", "o00"}, b"k")
        self.assertEqual(out["verdict"], "PASS")
        self.assertEqual(gate(out["steps"][0], "G-V1")["status"], "pass")
        self.assertNotIn("o00", out["steps"][0]["stability"])


class NeverPassOnMissingDataTest(unittest.TestCase):
    OBS = [f"o{i:02d}" for i in range(10)]
    WINDOWS = {"whole": (HS, HE + 1), "from_mid": (HS + 20 * STEP, HE + 1),
               "one": (HS + 20 * STEP, HS + 20 * STEP + 1), "three": (HS + 20 * STEP, HS + 23 * STEP)}
    RECEIVERS = {"one": OBS[:1], "some": OBS[:5], "all": OBS}

    def base(self):
        m = manifest([part(o) for o in self.OBS] + [part("t1", "rust", False, True)])
        return m, healthy_world(self.OBS, ["t1"])

    @staticmethod
    def drop(w, families, keep, window):
        lo, hi = window
        for name in families:
            for s in w.series.get(name, []):
                if keep(s["metric"]):
                    s["values"] = [v for v in s["values"] if not lo <= v[0] < hi]

    def scenarios(self):
        talker_pair = (M_PPS, M_EXPAND, M_CAN_LISTEN)
        for rname, recv in self.RECEIVERS.items():
            for fams in [(f,) for f in talker_pair] + [cq_score.PAIR_METRICS]:
                for wname, window in self.WINDOWS.items():
                    def mut(m, w, fams=fams, recv=recv, window=window):
                        self.drop(w, fams, lambda lab: lab.get("from_peer") in recv
                                  and lab.get("to_peer") == sess("t1"), window)
                    yield f"pair {'+'.join(f[10:24] for f in fams)} {rname} {wname}", mut
        for who in (["t1"], ["o00"], self.OBS):
            for wname, window in self.WINDOWS.items():
                def mut(m, w, who=who, window=window):
                    self.drop(w, (M_SENT,), lambda lab: lab.get("peer_id") in who, window)
                yield f"presence {who[0]}x{len(who)} {wname}", mut
        for wname in ("whole", "from_mid"):
            def mut(m, w, window=self.WINDOWS[wname]):
                self.drop(w, (M_HEALTH,), lambda lab: True, window)
            yield f"health counter {wname}", mut
        for fam in (M_PPS, M_EXPAND, M_CAN_LISTEN, M_FPS, M_FREEZE, M_SENT, M_HEALTH) + RELAY_PATHS:
            yield f"family {fam} absent", lambda m, w, fam=fam: w.series.pop(fam, None)
        for cap in ({"limit": 1, "selection": "lexicographic"}, {"limit": 5, "selection": "media_first"}):
            for rname, wname in (("all", "from_mid"), ("some", "whole")):
                def mut(m, w, cap=cap, recv=self.RECEIVERS[rname], window=self.WINDOWS[wname]):
                    m["peer_stats_cap"] = cap
                    self.drop(w, cq_score.PAIR_METRICS, lambda lab: lab.get("from_peer") in recv
                              and lab.get("to_peer") == sess("t1"), window)
                yield f"declared cap {cap['selection']} {rname} {wname}", mut

        def talker_health_lost(m, w):
            self.drop(w, (M_SENT,), lambda lab: lab.get("peer_id") == "t1", self.WINDOWS["from_mid"])
            self.drop(w, (M_PPS,), lambda lab: lab.get("to_peer") == sess("t1"), self.WINDOWS["from_mid"])
        yield "S1b-B2 talker HEALTH lost, receivers lose it", talker_health_lost

        def leave_rejoin_then_crash(m, w):
            m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "leave", "participants": ["o03"]},
                           {"at": HS + 75, "step_id": "s1", "action": "rejoin", "participants": ["o03"]}]
            del m["participants"][3]["leave_ts"]
            self.drop(w, (M_SENT,), lambda lab: lab.get("peer_id") == "o03", (HS + 60, HS + 75))
            self.drop(w, (M_SENT,), lambda lab: lab.get("peer_id") == "o03", (HS + 300, HE + 1))
        yield "R2-B3 leave, rejoin, then crash", leave_rejoin_then_crash

        def mute_over_loss(m, w):
            m["events"] = [{"at": HS + 10 * STEP, "step_id": "s1", "action": "mute", "participants": ["t1"]},
                           {"at": HS + 20 * STEP, "step_id": "s1", "action": "unmute", "participants": ["t1"]}]
            self.drop(w, cq_score.PAIR_METRICS, lambda lab: lab.get("to_peer") == sess("t1"),
                      (HS + 10 * STEP, HS + 20 * STEP))
        yield "R2-m1 mute event over a loss", mute_over_loss

    def test_baseline_passes(self):
        m, w = self.base()
        self.assertEqual(score(m, w)["verdict"], "PASS")

    def test_no_missing_data_variant_passes(self):
        names = []
        for name, mutate in self.scenarios():
            m, w = self.base()
            mutate(m, w)
            names.append(name)
            with self.subTest(name):
                self.assertNotEqual(score(m, w)["verdict"], "PASS")
        self.assertGreaterEqual(len(names), 80)


class PipelineGapAndVideoCoverageTest(unittest.TestCase):
    OBS = [f"o{i:02d}" for i in range(10)]

    def base(self, **kw):
        m = manifest([part(o) for o in self.OBS] + [part("t1", "rust", False, True)])
        return m, healthy_world(self.OBS, ["t1"], **kw)

    @staticmethod
    def failed_scrape(w, ts):
        for series in w.series.values():
            for s in series:
                s["values"] = [v for v in s["values"] if v[0] != ts]

    def test_one_failed_scrape_does_not_fail_a_healthy_run(self):
        m, w = self.base()
        self.failed_scrape(w, HS + 20 * STEP)
        r = score(m, w)
        self.assertEqual(gate(r, "G-V14")["value"]["points"], 1)
        self.assertEqual(r["verdict"], "PASS")

    def test_up_zero_marks_a_pipeline_gap(self):
        m, w = self.base()
        cfg = cq_score.load_config()
        cfg["validity_gates"]["scrape_up_selector"]["value"] = 'up{job="metrics-api"}'
        w.add("_up", {}, lambda t: 0 if t == HS + 20 * STEP else 1)
        data = w.data()
        r = cq_score.score_step(m, m["steps"][0], data, cfg)
        self.assertEqual(gate(r, "G-V14")["value"]["points"], 1)

    def test_real_loss_next_to_a_failed_scrape_still_fails(self):
        m, w = self.base()
        TeamDecisionTest.drop_pair(None, w, "o00", HS + 20 * STEP)
        self.failed_scrape(w, HS + 10 * STEP)
        self.assertEqual(score(m, w)["verdict"], "FAIL")

    def test_two_consecutive_or_too_many_pipeline_gaps_are_invalid_not_fail(self):
        m, w = self.base()
        for ts in (HS + 20 * STEP, HS + 21 * STEP):
            self.failed_scrape(w, ts)
        r = score(m, w)
        self.assertEqual(gate(r, "G-V14")["status"], "fail")
        self.assertEqual(gate(r, "G-Q2", "quality_gates")["status"], "pass")
        self.assertEqual(r["verdict"], "INVALID")
        m, w = self.base()
        for ts in (HS + 5 * STEP, HS + 15 * STEP, HS + 25 * STEP):
            self.failed_scrape(w, ts)
        self.assertEqual(score(m, w)["verdict"], "INVALID")

    def mass_reconnect(self, who, blank_health):
        m, w = self.base()
        ts = HS + 20 * STEP
        for s in list(w.series[M_SENT]):
            if s["metric"]["peer_id"] in who:
                u = s["metric"]["peer_id"]
                s["values"] = [v for v in s["values"] if v[0] < ts]
                w.presence(u, sess(u, 2), lo=ts + STEP)
        NeverPassOnMissingDataTest.drop(w, cq_score.PAIR_METRICS, lambda lab: lab.get("from_peer") in who
                                        or lab.get("to_peer") in {sess(u) for u in who}, (ts, ts + 1))
        if blank_health:
            self.failed_scrape(w, ts)
        return score(m, w)

    def test_r4_1_mass_reconnect_with_a_successful_scrape_is_not_a_pipeline_gap(self):
        everyone = self.OBS + ["t1"]
        for who in (everyone, everyone[:6], everyone[:5]):
            r = self.mass_reconnect(who, blank_health=False)
            self.assertEqual(gate(r, "G-V14")["value"]["points"], 0, len(who))
            self.assertEqual(r["verdict"], "FAIL", len(who))

    def test_r4_1_only_a_failed_scrape_is_excluded(self):
        m, w = self.base()
        NeverPassOnMissingDataTest.drop(w, (M_HEALTH,), lambda lab: True, (HS + 20 * STEP, HS + 20 * STEP + 1))
        self.assertEqual(gate(score(m, w), "G-V14")["value"]["points"], 1)

    def test_q1_unplanned_reconnects_are_gated_by_default(self):
        m, w = self.base()
        for u in self.OBS[:2]:
            w.presence(u, sess(u, 2), lo=HS + 20 * STEP)
        r = score(m, w)
        g = gate(r, "G-Q9", "quality_gates")
        self.assertEqual(g["status"], "fail")
        self.assertIn("team decision pending (#2913)", g["detail"])
        self.assertEqual(r["verdict"], "FAIL")
        cfg = cq_score.load_config()
        cfg["quality_gates"]["reconnect_gate_enabled"]["value"] = False
        r = cq_score.score_step(m, m["steps"][0], w.data(), cfg)
        self.assertEqual(gate(r, "G-Q9", "quality_gates")["status"], "disabled")
        self.assertIn("reconnect-masked", gate(r, "G-Q8", "quality_gates")["value"])
        self.assertNotIn("o00", gate(r, "G-Q2", "quality_gates")["detail"])
        self.assertEqual(r["verdict"], "FAIL")

    def test_r3_b1_camera_publisher_lost_by_every_receiver_fails(self):
        m, w = self.base()
        NeverPassOnMissingDataTest.drop(w, cq_score.PAIR_METRICS, lambda lab: lab.get("to_peer") == sess("o05"),
                                        (HS + 20 * STEP, HE + 1))
        r = score(m, w)
        self.assertIn("camera pair", gate(r, "G-Q8", "quality_gates")["value"])
        self.assertEqual(gate(r, "G-Q4", "quality_gates")["status"], "fail")
        self.assertEqual(r["verdict"], "FAIL")

    def test_2963_b1_camera_video_reaching_nobody_fails_although_entries_exist(self):
        m, w = self.base()
        NeverPassOnMissingDataTest.drop(w, (M_FREEZE, M_FPS, M_STALE), lambda lab: lab.get("to_peer") == sess("o05"),
                                        (HS, HE + 1))
        r = score(m, w)
        self.assertIn("camera pair", gate(r, "G-Q8", "quality_gates")["value"])
        self.assertEqual(r["verdict"], "FAIL")

    def test_2963_b1_above_the_canvas_limit_a_missing_tracker_is_invalid(self):
        extra = [f"c{i:02d}" for i in range(DIOXUS_CANVAS_LIMIT)]
        cams = [dict(part(c, "rust", False, False), publishes={"camera": True, "mic": False, "screen": False})
                for c in extra]
        m = manifest([part(o) for o in self.OBS] + [part("t1", "rust", False, True)] + cams)
        w = healthy_world(self.OBS, ["t1"])
        for c in extra:
            w.presence(c)
            for o in self.OBS:
                w.pair(M_CAN_LISTEN, o, sess(c), 1)
        r = score(m, w)
        self.assertIn(f"more than {DIOXUS_CANVAS_LIMIT} camera publishers", gate(r, "G-V12")["detail"])
        self.assertIn("N=200 with cameras on) is INVALID by construction", gate(r, "G-V12")["detail"])
        self.assertEqual(r["verdict"], "INVALID")

    def test_r3_m1_a_late_joiner_makes_the_hold_not_steady(self):
        parts = [part(o) for o in self.OBS] + [part("t1", "rust", False, True)]
        parts[2]["join_ts"] = HS - 5
        m = manifest(parts)
        w = healthy_world(self.OBS, ["t1"])
        g = gate(score(m, w), "G-V15")
        self.assertEqual(g["status"], "fail")
        self.assertIn("hold not steady", g["detail"])
        self.assertIn("o02", g["detail"])

    def test_r3_m1_minor_reporting_inside_a_declared_leave_window_is_invalid(self):
        m, w = self.base()
        m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "leave", "participants": ["o03"]},
                       {"at": HS + 180, "step_id": "s1", "action": "rejoin", "participants": ["o03"]}]
        r = score(m, w)
        self.assertIn("kept reporting inside its declared leave window", gate(r, "G-V13")["detail"])
        self.assertEqual(r["verdict"], "INVALID")

    def test_healthy_runs_with_normal_sampling_noise_pass(self):
        import random
        for seed in range(20):
            rng = random.Random(seed)
            m, w = self.base()
            for name, lo, hi in ((M_PPS, 49, 51), (M_EXPAND, 0, 1), (M_STALE, 50, 300), (M_FPS, 25, 30)):
                for s in w.series[name]:
                    s["values"] = [[ts, str(rng.uniform(lo, hi))] for ts, _ in s["values"]]
            points = w.grid()[2:-2]
            blanks = set()
            for ts in rng.sample(points, rng.randint(0, 2)):
                if not any(abs(ts - b) <= STEP for b in blanks):
                    blanks.add(ts)
                    self.failed_scrape(w, ts)
            with self.subTest(seed=seed, blanks=sorted(blanks)):
                self.assertEqual(score(m, w)["verdict"], "PASS")


class Review2963Test(unittest.TestCase):
    OBS = [f"o{i:02d}" for i in range(10)]

    def base(self, extra=(), **kw):
        m = manifest([part(o) for o in self.OBS] + [part("t1", "rust", False, True)] + list(extra))
        w = healthy_world(self.OBS, ["t1"], **kw)
        return m, w

    def test_b2_rust_viewers_that_stop_mid_hold_fail(self):
        viewers = [dict(part(f"v{i:02d}", "rust", False, False), publishes={"camera": False, "mic": False,
                                                                          "screen": False}) for i in range(50)]
        m, w = self.base(viewers)
        for v in viewers:
            w.presence(v["user_id"], hi=HE if v["user_id"] >= "v04" else HS + 20 * STEP)
        w.series[M_HEALTH].clear()
        w.health((HE - HS) * (len(self.OBS) / 5 + 51 / BOT_HEALTH_INTERVAL_S))
        r = score(m, w)
        self.assertIn("v00", gate(r, "G-Q2", "quality_gates")["detail"])
        self.assertIn("v00 @", gate(r, "G-Q8", "quality_gates")["detail"])
        self.assertNotEqual(r["verdict"], "PASS")

    def test_b3_health_intervals_are_bounded(self):
        for key, value in (("browser_health_interval_s", 11), ("rust_health_interval_s", 6),
                           ("rust_health_interval_s", 0.5)):
            with tempfile.TemporaryDirectory() as tmp:
                path = os.path.join(tmp, "o.json")
                with open(path, "w") as fh:
                    json.dump({"validity_gates": {key: {"value": value}}}, fh)
                with self.assertRaises(cq_score.ConfigError, msg=(key, value)):
                    cq_score.load_config(path)

    def test_b4_off_screen_tiles_do_not_dilute_freezes(self):
        m, w = self.base()
        for s in w.series[M_FPS]:
            if s["metric"]["to_peer"] != sess("t1"):
                s["values"] = [[ts, "0"] for ts, _ in s["values"]]
        frozen = (HS + 10 * STEP, HS + 14 * STEP)
        for s in w.series[M_FREEZE]:
            if s["metric"]["to_peer"] == sess("t1"):
                s["values"] = [[ts, str(min(max(0.0, ts - frozen[0]), frozen[1] - frozen[0]))] for ts, _ in s["values"]]
        for s in w.series[M_FPS]:
            if s["metric"]["to_peer"] == sess("t1"):
                s["values"] = [[ts, "0" if frozen[0] < ts <= frozen[1] else "30"] for ts, _ in s["values"]]
        r = score(m, w)
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o00"]["V"], 60 / 600)
        self.assertEqual(gate(r, "G-Q4", "quality_gates")["status"], "fail")

    def test_b5_a_declared_leave_longer_than_the_retention_window_is_valid(self):
        m, w = self.base()
        m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "leave", "participants": ["o03"]},
                       {"at": HS + 180, "step_id": "s1", "action": "rejoin", "participants": ["o03"]}]
        w.series[M_SENT] = [s for s in w.series[M_SENT] if s["metric"]["peer_id"] != "o03"]
        w.presence("o03", hi=HS + 60 + METRICS_API_SESSION_TIMEOUT_S + STEP)
        w.presence("o03", sess("o03", 2), lo=HS + 180)
        r = score(m, w)
        self.assertEqual(gate(r, "G-V13")["status"], "pass", gate(r, "G-V13")["detail"])
        self.assertEqual(r["stability"]["o03"]["unplanned_reconnects"], 0)
        self.assertEqual(r["verdict"], "PASS")

    def test_b5_absent_leave_ts_within_the_retention_window_of_a_final_leave_is_planned(self):
        parts = [part(o) for o in self.OBS] + [part("t1", "rust", False, True)]
        del parts[3]["leave_ts"]
        m = manifest(parts)
        m["steps"][0]["n_target"] = 10
        m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "leave", "participants": ["o03"]}]
        w = healthy_world(self.OBS, ["t1"])
        w.series[M_SENT] = [s for s in w.series[M_SENT] if s["metric"]["peer_id"] != "o03"]
        w.presence("o03", hi=HS + 60 + METRICS_API_SESSION_TIMEOUT_S + STEP)
        r = score(m, w)
        self.assertIn("treated as left", r["flags"][0])
        self.assertEqual(r["verdict"], "PASS")

    def test_b6_a_quick_reconnect_is_masked_missing_data_and_the_new_session_is_scored(self):
        m, w = self.base()
        cut = HS + 20 * STEP
        new = sess("o02", 2)
        w.series[M_SENT] = [s for s in w.series[M_SENT] if s["metric"]["peer_id"] != "o02"]
        w.presence("o02", hi=cut + METRICS_API_SESSION_TIMEOUT_S)
        w.presence("o02", new, lo=cut + STEP)
        w.pair(M_CAN_LISTEN, "o02", sess("t1"), 1, recv_session=new, lo=cut + STEP)
        w.pair(M_PPS, "o02", sess("t1"), 0, recv_session=new, lo=cut + STEP)
        w.pair(M_EXPAND, "o02", sess("t1"), 100, recv_session=new, lo=cut + STEP)
        r = score(m, w)
        self.assertEqual(r["stability"]["o02"]["max_presence_gap_s"], STEP)
        g8 = gate(r, "G-Q8", "quality_gates")
        self.assertIn("reconnect-masked", g8["value"])
        self.assertIn("o02 @", g8["detail"])
        self.assertNotIn("o02", gate(r, "G-Q2", "quality_gates")["detail"])
        self.assertGreater(r["cells"]["UxU"]["participants"]["o02"]["A"], 0.4)

    def rejoin_world(self, delay):
        m, w = self.base()
        m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "leave", "participants": ["o03"]},
                       {"at": HS + 180, "step_id": "s1", "action": "rejoin", "participants": ["o03"]}]
        start = min(t for t in w.grid() if t >= HS + 180 + delay)
        blank = (HS + 60 + METRICS_API_SESSION_TIMEOUT_S + STEP + 1, start)
        NeverPassOnMissingDataTest.drop(
            w, list(w.series), lambda lab: "o03" in (lab.get("from_peer"), lab.get("peer_id"))
            or lab.get("to_peer") == sess("o03"), blank)
        w.new_session("o03", start)
        return m, w

    def test_f1_a_planned_rejoin_with_a_normal_startup_gap_passes(self):
        m, w = self.rejoin_world(5)
        self.assertEqual(score(m, w)["verdict"], "PASS")

    def test_f1_a_rejoin_slower_than_the_grace_is_not_honoured(self):
        m, w = self.rejoin_world(45)
        r = score(m, w)
        self.assertIn("rejoin for o03", gate(r, "G-V13")["detail"])
        self.assertIn("o03 @", gate(r, "G-Q8", "quality_gates")["detail"])
        self.assertEqual(r["verdict"], "INVALID")

    def test_f1_rejoin_grace_has_a_built_in_max(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "o.json")
            with open(path, "w") as fh:
                json.dump({"quality_gates": {"rejoin_grace_s": {"value": 60}}}, fh)
            with self.assertRaises(cq_score.ConfigError):
                cq_score.load_config(path)

    def test_generator_verdict_must_be_boolean_true(self):
        m, w = self.base()
        for value in ("false", 1, None):
            r = score(m, w, generator_verdict={"ok": value})
            self.assertEqual(gate(r, "G-V5")["status"], "fail", value)

    def test_configured_selectors_that_return_nothing_fail(self):
        m, w = self.base()
        cfg = cq_score.load_config()
        cfg["validity_gates"]["restarts_selector"]["value"] = "kube_pod_container_status_restarts_total"
        cfg["validity_gates"]["scrape_up_selector"]["value"] = 'up{job="metrics-api"}'
        r = cq_score.score_step(m, m["steps"][0], w.data(), cfg)
        self.assertEqual(gate(r, "G-V4")["status"], "fail")
        self.assertEqual(gate(r, "G-V3")["status"], "fail")
        w.add("_up", {}, 1)
        w.add("_restarts", {}, 3)
        r = cq_score.score_step(m, m["steps"][0], w.data(), cfg)
        self.assertEqual(gate(r, "G-V3")["status"], "pass")
        self.assertEqual(gate(r, "G-V4")["status"], "pass")
        w.series["_up"][0]["values"][5][1] = "0"
        self.assertEqual(gate(cq_score.score_step(m, m["steps"][0], w.data(), cfg), "G-V3")["status"], "fail")

    def test_unexpected_exceptions_exit_3_not_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            verdict = os.path.join(tmp, "v.json")
            with open(verdict, "w") as fh:
                json.dump([1], fh)
            m = cq_manifest.load_manifest(EXAMPLE)
            rc, err = main_with_stderr(["--manifest", EXAMPLE, "--prom-url", "http://prom.invalid",
                                        "--generator-verdict", verdict], transport=World().transport([]), environ={})
        self.assertEqual(rc, cli.EXIT_ERROR, err)
        self.assertIn("internal error", err)

    def test_prometheus_warnings_and_null_results(self):
        with self.assertRaises(cq_prom.PromError):
            cq_prom.parse_matrix({"status": "success", "warnings": ["partial response"],
                                  "data": {"resultType": "matrix", "result": []}})
        self.assertEqual(cq_prom.parse_matrix({"status": "success", "data": {"resultType": "matrix",
                                                                            "result": None}}), [])
        with self.assertRaises(cq_prom.PromError):
            cq_prom.parse_matrix({"status": "success", "data": {"resultType": "vector", "result": []}})

    def test_unhonoured_events_invalidate_and_netem_reshapes(self):
        m, w = self.base()
        for action in ("camera-off", "outage"):
            m["events"] = [{"at": HS + 60, "step_id": "s1", "action": action, "participants": ["o01"]}]
            r = score(m, w)
            self.assertIn("not honoured", gate(r, "G-V13")["detail"], action)
        m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "netem", "participants": ["o01"]}]
        r = score(m, w)
        self.assertNotIn("o01", r["cells"]["UxU"]["participants"])
        self.assertIn("o01", r["cells"]["SxU"]["participants"])

    def test_one_outlier_trips_p95_without_k_of_n(self):
        band = cq_score.load_config()["bands"]["A"]
        d = cq_score.summarize_dimension({f"u{i}": (1.0 if i == 0 else 0.0) for i in range(10)}, band, 2)
        self.assertEqual(d["k_red"], 1)
        self.assertTrue(d["gate_fail"])

    def test_environment_constants_match_the_scorer(self):
        def read(rel, pattern):
            with open(os.path.join(HERE, "..", "..", rel), encoding="utf-8") as fh:
                return int(re.search(pattern, fh.read(), re.S).group(1))
        timeout = read("actix-api/src/bin/metrics_server.rs",
                       r"fn cleanup_stale_sessions\b.*?let timeout = Duration::from_secs\((\d+)\)")
        canvas = read("dioxus-ui/src/constants.rs", r"pub const CANVAS_LIMIT: usize = (\d+);")
        self.assertEqual(cq_score.METRICS_SESSION_TIMEOUT_S, timeout)
        self.assertEqual(cq_score.CANVAS_LIMIT, canvas)

    def test_in_ci(self):
        self.assertTrue(cli.in_ci({"CI": "true"}))
        for env in ({}, {"CI": ""}, {"CI": "false"}, {"CI": "0"}):
            self.assertFalse(cli.in_ci(env), env)


class Review2963Round2Test(unittest.TestCase):
    OBS = Review2963Test.OBS

    def base(self, extra=()):
        m = manifest([part(o) for o in self.OBS] + [part("t1", "rust", False, True)] + list(extra))
        return m, healthy_world(self.OBS, ["t1"])

    def test_b1_receiver_session_replaced_just_before_the_hold_scores_the_same_in_any_series_order(self):
        cut = HS - 20
        m, w = self.base()
        w.new_session("o02", cut + 5)
        w.presence("o02", sess("o02"), lo=cut + 5, hi=cut + METRICS_API_SESSION_TIMEOUT_S)
        for name, value in ((M_PPS, 0), (M_EXPAND, 100), (M_CAN_LISTEN, 1)):
            w.pair(name, "o02", sess("t1"), value, hi=cut + METRICS_API_SESSION_TIMEOUT_S + STEP)
        results = []
        for flip in (False, True):
            if flip:
                for lst in w.series.values():
                    lst.reverse()
            r = score(m, w)
            results.append((r["verdict"], r["cells"]["UxU"]["participants"]["o02"]["A"]))
        self.assertEqual(results, [("PASS", 0.0), ("PASS", 0.0)])

    def test_b1_sessions_that_cannot_be_ordered_are_missing_data(self):
        m, w = self.base()
        w.pair(M_PPS, "o02", sess("t1"), 0, recv_session="unknown-session", hi=HS + STEP)
        g8 = gate(score(m, w), "G-Q8", "quality_gates")
        self.assertIn("session order unknown", g8["value"])
        self.assertEqual(g8["status"], "fail")

    def test_nan_and_infinity_are_rejected_by_every_loader(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "o.json")
            for group, key in (("quality_gates", "rejoin_grace_s"), ("quality_gates", "join_deadline_s"),
                               ("quality_gates", "drop_gap_s"), ("validity_gates", "health_reports_min_ratio")):
                for literal in ("NaN", "Infinity", "-Infinity", "1e999"):
                    with open(path, "w") as fh:
                        fh.write(f'{{"{group}": {{"{key}": {{"value": {literal}}}}}}}')
                    with self.assertRaises(cq_score.ConfigError, msg=(key, literal)):
                        cq_score.load_config(path)
            with open(EXAMPLE, encoding="utf-8") as fh:
                text = fh.read()
            bad = os.path.join(tmp, "m.json")
            with open(bad, "w") as fh:
                fh.write(re.sub(r'"join_ts": [0-9.]+', '"join_ts": NaN', text, count=1))
            with self.assertRaises(cq_manifest.ManifestError):
                cq_manifest.load_manifest(bad)
            m = cq_manifest.load_manifest(EXAMPLE)
            m["participants"][0]["join_ts"] = float("nan")
            self.assertIn("NaN", "\n".join(cq_manifest.validate_manifest(m)))
            verdict = os.path.join(tmp, "v.json")
            with open(verdict, "w") as fh:
                fh.write('{"ok": true, "detail": "x", "loss": NaN}')
            rc, err = main_with_stderr(["--manifest", EXAMPLE, "--prom-url", "http://prom.invalid",
                                        "--generator-verdict", verdict], transport=World().transport([]), environ={})
        self.assertEqual(rc, cli.EXIT_ERROR, err)
        self.assertIn("NaN", err)

    def test_a_declared_rejoin_that_never_happened_does_not_absorb_a_real_reconnect(self):
        m, w = self.base()
        m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "leave", "participants": ["o03"]},
                       {"at": HS + 105, "step_id": "s1", "action": "rejoin", "participants": ["o03"]}]
        w.new_session("o03", HS + 20 * STEP)
        r = score(m, w)
        self.assertIn("rejoin for o03 at 1105", gate(r, "G-V13")["detail"])
        self.assertEqual(r["verdict"], "INVALID")

    def test_freeze_inside_a_decoding_step_is_counted_once(self):
        m, w = self.base()
        for s in w.series[M_FPS]:
            if s["metric"]["to_peer"] != sess("t1"):
                s["values"] = [[ts, "0"] for ts, _ in s["values"]]
        frozen = (HS + 10 * STEP, HS + 14 * STEP)
        for s in w.series[M_FREEZE]:
            if s["metric"]["to_peer"] == sess("t1") and s["metric"]["from_peer"] == "o00":
                s["values"] = [[ts, str(min(max(0.0, ts - frozen[0]), frozen[1] - frozen[0]) / 2)]
                               for ts, _ in s["values"]]
        r = score(m, w)
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o00"]["V"], 30 / 600)

    def test_g_q9_counts_reconnects_of_viewers_that_neither_observe_nor_talk(self):
        viewers = [dict(part(f"v{i:02d}", "rust", False, False), publishes={"camera": False, "mic": False,
                                                                          "screen": False}) for i in range(20)]
        m, w = self.base(viewers)
        for v in viewers:
            w.presence(v["user_id"])
        for v in ("v00", "v01"):
            w.new_session(v, HS + 20 * STEP)
        w.series[M_HEALTH].clear()
        w.health((HE - HS) * (len(self.OBS) / 5 + 21 / BOT_HEALTH_INTERVAL_S))
        r = score(m, w)
        self.assertEqual(r["stability"]["v00"]["unplanned_reconnects"], 1)
        self.assertEqual(gate(r, "G-Q9", "quality_gates")["status"], "fail")

    def test_a_new_session_that_cannot_listen_yet_is_not_hidden_by_the_old_session(self):
        m, w = self.base()
        cut = HS + 20 * STEP
        linger = cut + METRICS_API_SESSION_TIMEOUT_S
        w.new_session("o02", cut + STEP)
        w.presence("o02", sess("o02"), lo=cut + STEP, hi=linger)
        w.pair(M_CAN_LISTEN, "o02", sess("t1"), 1, lo=cut + STEP, hi=linger)
        for s in w.series[M_CAN_LISTEN]:
            if s["metric"]["session_id"] == sess("o02", 2) and s["metric"]["to_peer"] == sess("t1"):
                s["values"] = [[ts, "0" if ts <= linger else v] for ts, v in s["values"]]
        r = score(m, w)
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o02"]["A"], 4 / 41)

    def test_d28_publishes_is_the_join_state_and_events_change_it(self):
        m, w = self.base()
        self.assertFalse(m["participants"][1]["publishes"]["mic"])

        def ev(at, action):
            return {"at": at, "step_id": "s1", "action": action, "participants": ["o01"]}
        for events in ([ev(HS + 60, "unmute")], [ev(HS + 60, "unmute"), ev(HS + 120, "mute")]):
            m["events"] = events
            r = score(m, w)
            self.assertEqual(gate(r, "G-V13")["status"], "pass", gate(r, "G-V13")["detail"])
            self.assertEqual(r["verdict"], "PASS")
        for events in ([ev(HS + 60, "mute")], [ev(HS + 60, "unmute"), ev(HS + 120, "unmute")]):
            m["events"] = events
            self.assertIn("not paired", gate(score(m, w), "G-V13")["detail"])
        m["participants"][0]["publishes"]["mic"] = True
        m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "unmute", "participants": ["o00"]}]
        self.assertEqual(score(m, w)["verdict"], "INVALID")

    def test_d28_mic_state_at_hold_start_comes_from_join_snapshot_and_earlier_events(self):
        def talker_muted_at_join(events, pps_before=50):
            m, w = self.base()
            m["participants"][-1]["publishes"]["mic"] = False
            m["events"] = [{"at": at, "step_id": "s1", "action": a, "participants": [u]} for at, a, u in events]
            for s in w.series[M_PPS]:
                if s["metric"]["to_peer"] == sess("t1"):
                    s["values"] = [[ts, str(pps_before) if ts < HS + 120 else v] for ts, v in s["values"]]
            return score(m, w)
        r = talker_muted_at_join([(JOIN + 10, "unmute", "t1")])
        self.assertEqual(r["verdict"], "PASS", gate(r, "G-V13")["detail"])
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o00"]["A"], 0.0)
        r = talker_muted_at_join([(HS + 105, "unmute", "t1")], pps_before=0)
        self.assertIn("declared talker t1 is muted at hold_start", gate(r, "G-V13")["detail"])
        self.assertEqual(r["verdict"], "INVALID")
        r = talker_muted_at_join([])
        self.assertIn("declared talker t1 is muted at hold_start", gate(r, "G-V13")["detail"])
        self.assertEqual(r["verdict"], "INVALID")
        m, w = self.base()
        m["events"] = [{"at": JOIN + 10, "step_id": "s1", "action": "unmute", "participants": ["o01"]}]
        self.assertEqual(score(m, w)["verdict"], "PASS")

    def test_d28_ending_muted_is_valid_for_a_non_talker_but_not_for_a_talker(self):
        m, w = self.base()
        m["participants"][1]["publishes"]["mic"] = True
        m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "mute", "participants": ["o01"]}]
        r = score(m, w)
        self.assertEqual(r["verdict"], "PASS", gate(r, "G-V13")["detail"])
        m["participants"][-1]["publishes"]["mic"] = True
        for at in (JOIN + 10, HS + 60):
            m["events"] = [{"at": at, "step_id": "s1", "action": "mute", "participants": ["t1"]}]
            self.assertIn("t1", gate(score(m, w), "G-V13")["detail"], at)
            self.assertEqual(score(m, w)["verdict"], "INVALID", at)

    def test_d28_a_user_id_never_recorded_is_invalid_not_a_manifest_error(self):
        parts = [part(o) for o in self.OBS] + [part("t1", "rust", False, True), part("late", observer=False)]
        parts[-1]["user_id"] = None
        m = manifest(parts)
        r = score(m, healthy_world(self.OBS, ["t1"]))
        self.assertIn("participants[11] (browser/test)", gate(r, "G-V8")["detail"])
        self.assertEqual(r["verdict"], "INVALID")
        with tempfile.TemporaryDirectory() as tmp:
            man = os.path.join(tmp, "m.json")
            with open(man, "w") as fh:
                json.dump(m, fh)
            rc, err = main_with_stderr(["--manifest", man, "--prom-url", "http://prom.invalid", "--out-dir", tmp],
                                       transport=healthy_world(self.OBS, ["t1"]).transport([]), environ={})
            self.assertEqual((rc, read_json(os.path.join(tmp, "result.json"))["verdict"]),
                             (2, "INVALID"), err)

    def test_join_start_off_the_hold_grid_still_samples_hold_end(self):
        def evaluated(fake):
            def query_range(url, body, headers):
                form = urllib.parse.parse_qs(body.decode())
                start, end = float(form["start"][0]), float(form["end"][0])
                payload = json.loads(fake(url, body, headers))
                for series in payload["data"]["result"]:
                    raw = [(float(t), v) for t, v in series["values"]]
                    lattice = (start + k * STEP for k in range(int((end - start) // STEP) + 1))
                    series["values"] = [[ts, live[-1]] for ts in lattice
                                        for live in [[v for t, v in raw if ts - STEP < t <= ts]] if live]
                return json.dumps(payload).encode()
            return query_range
        parts = [part(o) for o in self.OBS] + [part("t1", "rust", False, True)]
        m = manifest(parts)
        m["steps"][0]["join_start"] = HS - 10 * STEP + 5
        w = healthy_world(self.OBS, ["t1"])
        with tempfile.TemporaryDirectory() as tmp:
            man = os.path.join(tmp, "m.json")
            with open(man, "w") as fh:
                json.dump(m, fh)
            rc, err = main_with_stderr(["--manifest", man, "--prom-url", "http://prom.invalid", "--out-dir", tmp],
                                       transport=evaluated(w.transport([])), environ={})
            r = read_json(os.path.join(tmp, "result.json"))["steps"][0]
        self.assertEqual(gate(r, "G-Q8", "quality_gates")["status"], "pass", gate(r, "G-Q8", "quality_gates"))
        self.assertEqual((rc, r["verdict"]), (0, "PASS"), err)

    def test_g_q7_minimum_spans_at_least_two_scrapes(self):
        m = manifest([part(o) for o in self.OBS])
        cfg = cq_score.load_config()
        expr = next(q for name, q, _, _ in cq_score.step_queries(m, m["steps"][0], cfg, None) if name == "_pps_min")
        window = int(re.search(r"\[(\d+)s\]$", expr.rstrip(")")).group(1))
        self.assertGreaterEqual(window, 2 * cq_score.cv(cfg, "sampling", "scrape_step_s"))


class CoverageAndSizeTest(unittest.TestCase):
    OBS = Review2963Test.OBS

    def base(self, obs=None, extra=()):
        obs = self.OBS if obs is None else obs
        m = manifest([part(o) for o in obs] + [part("t1", "rust", False, True)] + list(extra))
        return m, healthy_world(obs, ["t1"])

    def test_video_seen_by_one_observer_is_too_few_for_v_and_q(self):
        m, w = self.base()
        for s in w.series[M_FPS]:
            if s["metric"]["from_peer"] != "o00":
                s["values"] = [[ts, "0"] for ts, _ in s["values"]]
        r = score(m, w)
        self.assertEqual(r["verdict"], "INVALID")
        g = gate(r, "G-V7")
        self.assertEqual((g["status"], g["value"]["V"], g["value"]["Q"]), ("fail", 1, 1), g)

    def test_one_observer_short_of_the_floor_is_invalid(self):
        m, w = self.base(self.OBS[:9])
        r = score(m, w)
        self.assertEqual(gate(r, "G-V7")["status"], "fail")
        self.assertEqual(r["verdict"], "INVALID")

    def test_fewer_participants_than_n_target_is_invalid(self):
        m, w = self.base()
        self.assertEqual(score(m, w)["verdict"], "PASS")
        m["steps"][0]["n_target"] = 200
        r = score(m, w)
        self.assertEqual(r["verdict"], "INVALID")
        self.assertEqual((gate(r, "G-V16")["status"], gate(r, "G-V16")["value"]), ("fail", 11))
        m, w = self.base(extra=[part("gone", observer=False, leave_ts=HS - 10)])
        m["steps"][0]["n_target"] = 12
        m["events"] = [{"at": HS - 10, "step_id": "s1", "action": "leave", "participants": ["gone"]}]
        self.assertEqual(gate(score(m, w), "G-V16")["status"], "fail")

    def test_scrape_step_is_fixed_at_the_15_s_scrape(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "o.json")
            for value in (5, 30):
                with open(path, "w") as fh:
                    json.dump({"sampling": {"scrape_step_s": {"value": value}}}, fh)
                with self.assertRaises(cq_score.ConfigError, msg=value):
                    cq_score.load_config(path)

    def test_exit_codes(self):
        self.assertEqual((cli.EXIT, cli.EXIT_ERROR), ({"PASS": 0, "REPORT": 0, "FAIL": 1, "INVALID": 2}, 3))

    def test_a_report_holding_exactly_the_cap_is_ambiguous(self):
        for extra, verdict in ((cq_score.DEPLOYED_PEER_STATS_CAP - 9, "INVALID"),
                               (cq_score.DEPLOYED_PEER_STATS_CAP - 10, "FAIL")):
            m, w = self.base()
            for k in range(extra):
                w.pair(M_CAN_LISTEN, "o00", f"9{k:05d}", 1)
            for lst in w.series.values():
                for s in lst:
                    if (s["metric"].get("from_peer"), s["metric"].get("to_peer")) == ("o00", sess("t1")):
                        s["values"] = [x for x in s["values"] if x[0] < HS + 20 * STEP]
            self.assertEqual(score(m, w)["verdict"], verdict, extra)

    def test_health_ratio_survives_a_failed_scrape_at_hold_start_and_counts_leaves(self):
        m, w = self.base()
        for s in w.series[M_HEALTH]:
            s["values"] = [[ts, str(1000 + float(v))] for ts, v in s["values"] if ts != HS]
        self.assertEqual(gate(score(m, w), "G-V1")["status"], "pass", gate(score(m, w), "G-V1"))
        m, w = self.base()
        for p in m["participants"][:3]:
            p["leave_ts"] = HS + 4 * STEP
        m["events"] = [{"at": HS + 4 * STEP, "step_id": "s1", "action": "leave",
                        "participants": [p["user_id"] for p in m["participants"][:3]]}]
        w.series[M_HEALTH].clear()
        w.health((HE - HS) * (7 / 5 + 1 / BOT_HEALTH_INTERVAL_S) + 3 * 4 * STEP / 5)
        self.assertEqual(gate(score(m, w), "G-V1")["status"], "pass", gate(score(m, w), "G-V1"))


    def test_health_ratio_subtracts_declared_leave_windows(self):
        m, w = self.base()
        m["events"] = [{"at": at, "step_id": "s1", "action": a, "participants": ["o03", "o04"]}
                       for at, a in ((HS + 60, "leave"), (HS + 480, "rejoin"))]
        w.series[M_HEALTH].clear()
        w.health((HE - HS) * (10 / 5 + 1 / BOT_HEALTH_INTERVAL_S) - 2 * 420 / 5)
        g = gate(score(m, w), "G-V1")
        self.assertEqual((g["status"], g["value"]), ("pass", 1.0), g)

    def test_health_ratio_is_taken_over_the_sampled_span(self):
        m, w = self.base()
        for s in w.series[M_HEALTH]:
            s["values"] = [x for x in s["values"] if HS + 4 * STEP <= x[0] <= HE - 4 * STEP]
        g = gate(score(m, w), "G-V1")
        self.assertEqual((g["status"], g["value"]), ("pass", 1.0), g)

    def test_join_window_queries_start_on_the_hold_grid_before_join_start(self):
        m, _ = self.base()
        m["steps"][0]["join_start"] = HS - 10 * STEP + 5
        q = {name: start for name, _, start, _ in cq_score.step_queries(m, m["steps"][0], cq_score.load_config(),
                                                                         None)}
        self.assertEqual(q[M_SENT], HS - 10 * STEP)


class SoleTalkerAndBoundaryTest(unittest.TestCase):
    OBS = [f"o{i:02d}" for i in range(11)]

    def test_an_observer_that_is_the_only_talker_and_reports_no_pairs_is_missing_data(self):
        m = manifest([part(o, talker=o == "o00") for o in self.OBS])
        w = healthy_world(self.OBS[1:], ["o00"])
        w.series[M_HEALTH].clear()
        w.health((HE - HS) * len(self.OBS) / 5)
        self.assertNotIn("o00", {s["metric"]["from_peer"] for lst in w.series.values() for s in lst
                                 if "from_peer" in s["metric"]})
        r = score(m, w)
        g8 = gate(r, "G-Q8", "quality_gates")
        self.assertIn("observer report", g8["value"], g8)
        self.assertIn("o00 @", g8["detail"])
        self.assertNotEqual(r["verdict"], "PASS")

    def test_an_observer_seen_only_by_itself_is_hidden(self):
        m, w = Review2963Test().base()
        for lst in w.series.values():
            lst[:] = [s for s in lst if s["metric"].get("to_peer") != sess("o00")]
        w.pair(M_CAN_LISTEN, "o00", sess("o00"), 1)
        g = gate(score(m, w), "G-V6")
        self.assertEqual(g["status"], "fail")
        self.assertIn("o00", g["detail"])

    def test_a_talker_unmuting_is_scored_from_one_scrape_after_the_unmute(self):
        m, w = Review2963Test().base()
        m["participants"][-1]["publishes"]["mic"] = False
        m["events"] = [{"at": HS - 5, "step_id": "s1", "action": "unmute", "participants": ["t1"]}]
        for s in w.series[M_PPS]:
            if s["metric"]["to_peer"] == sess("t1"):
                s["values"] = [[ts, "0" if ts <= HS else v] for ts, v in s["values"]]
        r = score(m, w)
        self.assertEqual(r["cells"]["UxU"]["participants"]["o00"]["A"], 0.0)
        self.assertEqual(r["verdict"], "PASS")

    def test_p95_exactly_at_red_does_not_trip_the_band(self):
        band = cq_score.load_config()["bands"]["A"]
        vals = {f"u{i}": (band["red"] if i >= 19 else 0.0) for i in range(21)}
        d = cq_score.summarize_dimension(vals, band, 99)
        self.assertEqual(d["p95"], band["red"])
        self.assertFalse(d["gate_fail"])

    def test_split_rate_exactly_at_red_passes(self):
        m, w = Review2963Test().base()
        for s in w.series[M_PPS]:
            if (s["metric"]["from_peer"], s["metric"]["to_peer"]) == ("o00", sess("t1")):
                s["values"] = [[ts, "0" if ts == HS + 20 * STEP else v] for ts, v in s["values"]]
        cfg = cq_score.load_config()
        cfg["quality_gates"]["split_rate_red"]["value"] = 1 / 41
        g = gate(cq_score.score_step(m, m["steps"][0], w.data(), cfg), "G-Q7", "quality_gates")
        self.assertEqual((g["status"], g["value"]), ("pass", round(1 / 41, 4)), g)

    def test_a_final_leave_with_null_leave_ts_excuses_hold_end(self):
        m, w = Review2963Test().base()
        m["steps"][0]["n_target"] -= 1
        m["events"] = [{"at": HS + 20 * STEP, "step_id": "s1", "action": "leave", "participants": ["o03"]}]
        cut = HS + 20 * STEP + METRICS_API_SESSION_TIMEOUT_S
        for lst in w.series.values():
            for s in lst:
                lab = s["metric"]
                if "o03" in (lab.get("from_peer"), lab.get("peer_id")) or lab.get("to_peer") == sess("o03"):
                    s["values"] = [x for x in s["values"] if x[0] <= cut]
        r = score(m, w)
        self.assertNotIn("@ 1600", gate(r, "G-Q8", "quality_gates")["detail"])
        self.assertEqual(gate(r, "G-V16")["value"], 10)
        self.assertEqual(r["verdict"], "PASS", gate(r, "G-Q8", "quality_gates"))

    def test_argument_errors_exit_3_not_invalid(self):
        for argv in (["--bogus"], ["--manifest", EXAMPLE, "--prom-url", "http://x", "--timeout", "abc"]):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as cm:
                cli.main(argv, environ={})
            self.assertEqual(cm.exception.code, 3, argv)


class RecordedLeaveTest(unittest.TestCase):
    OBS = Review2963Test.OBS

    def world(self, leaver, left, event_at=None):
        viewer = dict(part("v00", "rust", False, False), publishes={"camera": False, "mic": False, "screen": False})
        parts = [part(o) for o in self.OBS] + [part("t1", "rust", False, True), viewer]
        next(p for p in parts if p["user_id"] == leaver)["leave_ts"] = left
        m = manifest(parts)
        if event_at is not None:
            m["steps"][0]["n_target"] -= 1
            m["events"] = [{"at": event_at, "step_id": "s1", "action": "leave", "participants": [leaver]}]
        w = healthy_world(self.OBS, ["t1"])
        w.presence("v00")
        w.series[M_HEALTH].clear()
        w.health((HE - HS) * (len(self.OBS) / 5 + 2 / BOT_HEALTH_INTERVAL_S))
        for lst in w.series.values():
            for s in lst:
                lab = s["metric"]
                if leaver in (lab.get("from_peer"), lab.get("peer_id")) or lab.get("to_peer") == sess(leaver):
                    s["values"] = [x for x in s["values"] if x[0] <= left]
        return score(m, w)

    def test_a_recorded_leave_inside_the_hold_without_a_leave_event_is_a_drop(self):
        for leaver, left in (("v00", HS + 1), ("o03", HS + 60)):
            r = self.world(leaver, left)
            self.assertIn(leaver, gate(r, "G-Q2", "quality_gates")["detail"], leaver)
            self.assertIn(f"{leaver} @", gate(r, "G-Q8", "quality_gates")["detail"], leaver)
            self.assertIn("counted as present (a drop)", " ".join(r["flags"]))
            self.assertEqual(r["verdict"], "FAIL", leaver)

    def test_a_recorded_leave_at_a_declared_final_leave_is_planned(self):
        for leaver, left, event_at in (("v00", HS + 1, HS + 1), ("o03", HS + 60, HS + 60),
                                       ("o03", HS + 60, HS + 60 - STEP), ("o03", HS + 60, HS + 60 + STEP)):
            r = self.world(leaver, left, event_at)
            self.assertEqual(r["verdict"], "PASS", (leaver, event_at, gate(r, "G-Q8", "quality_gates")))

    def test_a_leave_event_more_than_a_scrape_from_the_recorded_leave_does_not_excuse_it(self):
        r = self.world("o03", HS + 60, HS + 60 + 2 * STEP)
        self.assertIn("o03 @", gate(r, "G-Q8", "quality_gates")["detail"])
        self.assertNotEqual(r["verdict"], "PASS")

    def test_a_recorded_leave_at_teardown_is_planned(self):
        for left in (HE - STEP, HE, HE + 30):
            self.assertEqual(self.world("o03", left)["verdict"], "PASS", left)


class StepSizeTest(unittest.TestCase):
    def test_a_final_leave_inside_the_hold_takes_the_step_below_n_target(self):
        obs = RecordedLeaveTest.OBS
        viewer = dict(part("v00", "rust", False, False), publishes={"camera": False, "mic": False, "screen": False})
        parts = [part(o) for o in obs] + [part("t1", "rust", False, True), viewer]
        parts[-1]["leave_ts"] = HS + 1
        m = manifest(parts)
        m["events"] = [{"at": HS + 1, "step_id": "s1", "action": "leave", "participants": ["v00"]}]
        w = healthy_world(obs, ["t1"])
        w.presence("v00")
        for lst in w.series.values():
            for x in lst:
                if x["metric"].get("peer_id") == "v00":
                    x["values"] = [v for v in x["values"] if v[0] <= HS + 1]
        r = score(m, w)
        self.assertEqual((gate(r, "G-V16")["status"], gate(r, "G-V16")["value"]), ("fail", 11))
        self.assertEqual([g["gate"] for g in r["validity"] if g["status"] == "fail"], ["G-V16"])
        self.assertEqual(r["verdict"], "INVALID")

    def test_a_talker_unmuting_inside_the_hold_is_invalid(self):
        m, w = Review2963Test().base()
        m["participants"][-1]["publishes"]["mic"] = False
        m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "unmute", "participants": ["t1"]}]
        r = score(m, w)
        self.assertIn("declared talker t1 is muted at hold_start", gate(r, "G-V13")["detail"])
        self.assertEqual(r["verdict"], "INVALID")

    def test_a_leave_followed_by_a_confirmed_rejoin_still_counts(self):
        m, w = Review2963Test().base()
        m["events"] = [{"at": HS + 60, "step_id": "s1", "action": "leave", "participants": ["o03"]},
                       {"at": HS + 180, "step_id": "s1", "action": "rejoin", "participants": ["o03"]}]
        w.series[M_SENT] = [s for s in w.series[M_SENT] if s["metric"]["peer_id"] != "o03"]
        w.presence("o03", hi=HS + 60 + METRICS_API_SESSION_TIMEOUT_S + STEP)
        w.presence("o03", sess("o03", 2), lo=HS + 180)
        r = score(m, w)
        self.assertEqual((gate(r, "G-V16")["status"], gate(r, "G-V16")["value"]), ("pass", 11))
        self.assertEqual(r["verdict"], "PASS")


class NonFiniteAndDuplicateTest(unittest.TestCase):
    def base(self):
        return Review2963Test().base()

    def test_an_infinite_freeze_sample_does_not_turn_a_frozen_run_into_a_pass(self):
        m, w = self.base()
        for s in w.series[M_FREEZE]:
            s["values"] = [[ts, "+Inf" if ts == HS + 20 * STEP else str(ts - HS)] for ts, _ in s["values"]]
        r = score(m, w)
        self.assertEqual(gate(r, "G-Q4", "quality_gates")["status"], "fail")
        self.assertEqual(r["verdict"], "FAIL")

    def test_parse_matrix_drops_infinite_samples(self):
        series = cq_prom.parse_matrix({"status": "success", "data": {"resultType": "matrix", "result": [
            {"metric": {}, "values": [[1, "+Inf"], [2, "-Inf"], [3, "NaN"], [4, "1"]]}]}})
        self.assertEqual(series, [({}, [(4.0, 1.0)])])

    def test_a_non_finite_dimension_value_fails_its_gate(self):
        band = cq_score.load_config()["bands"]["A"]
        for bad in (float("nan"), float("inf")):
            d = cq_score.summarize_dimension({"u0": 0.0, "u1": bad}, band, 2)
            self.assertTrue(d["gate_fail"], bad)

    def test_result_json_never_holds_nan_or_infinity(self):
        real = cq_score.score_run

        def poisoned(*a, **kw):
            out = real(*a, **kw)
            out["steps"][0]["diagnostics"]["poison"] = float("inf")
            return out
        with tempfile.TemporaryDirectory() as tmp:
            cq_score.score_run = poisoned
            try:
                rc = quiet_main(["--manifest", EXAMPLE, "--prom-url", "http://prom.invalid", "--out-dir", tmp],
                                transport=World().transport([]), environ={})
            finally:
                cq_score.score_run = real
            with open(os.path.join(tmp, "result.json"), encoding="utf-8") as fh:
                result = json.loads(fh.read(), parse_constant=lambda c: self.fail(f"bare {c} in result.json"))
        self.assertEqual(rc, 2)
        self.assertIsNone(result["steps"][0]["diagnostics"]["poison"])
        self.assertEqual(result["run"]["non_finite_values_nulled"], ["$.steps[0].diagnostics.poison"])

    def test_conflicting_duplicate_series_are_missing_data_in_either_order(self):
        outcomes = []
        for first in (True, False):
            m, w = self.base()
            dup = {"metric": {"__name__": M_PPS, "meeting_id": "scale-t1", "from_peer": "o00",
                              "session_id": sess("o00"), "to_peer": sess("t1")},
                   "values": [[ts, "0"] for ts in w.grid()]}
            w.series[M_PPS].insert(0 if first else len(w.series[M_PPS]), dup)
            r = score(m, w)
            g8 = gate(r, "G-Q8", "quality_gates")
            self.assertIn("conflicting duplicate series", g8["value"], first)
            outcomes.append((r["verdict"], r["cells"]["UxU"]["participants"]["o00"]["A"]))
        self.assertEqual(outcomes[0], outcomes[1])
        self.assertEqual(outcomes[0][0], "FAIL")


class NonFiniteCounterTest(unittest.TestCase):
    def test_a_non_finite_health_sample_is_not_a_failed_scrape(self):
        m, w = Review2963Test().base()
        gap = HS + 20 * STEP
        for lst in w.series.values():
            for s in lst:
                if s["metric"].get("from_peer") == "o00":
                    s["values"] = [x for x in s["values"] if x[0] != gap]
        w.series[M_HEALTH][0]["values"] = [[ts, "+Inf" if ts == gap else v]
                                           for ts, v in w.series[M_HEALTH][0]["values"]]
        r = score(m, w)
        g8 = gate(r, "G-Q8", "quality_gates")
        self.assertIn("non-finite samples", g8["value"])
        self.assertIn("o00 @", g8["detail"])
        self.assertEqual(r["verdict"], "FAIL")

    def test_a_non_finite_failed_reelection_counter_is_not_zero(self):
        for values in (lambda t: "+Inf" if t >= HS + 105 else "0", lambda t: "+Inf"):
            m, w = Review2963Test().base()
            w.series[M_REELECT].append({"metric": {"__name__": M_REELECT, "meeting_id": "scale-t1",
                                                   "session_id": sess("o04"), "result": "failed"},
                                        "values": [[t, values(t)] for t in w.grid(JOIN, HE)]})
            r = score(m, w)
            self.assertIn(M_REELECT, gate(r, "G-Q8", "quality_gates")["detail"])
            self.assertNotEqual(r["verdict"], "PASS")

    def test_an_overflowing_proceeded_count_does_not_hide_a_reconnect(self):
        m, w = Review2963Test().base()
        w.new_session("o02", HS + 20 * STEP)
        w.series[M_REELECT].append({"metric": {"__name__": M_REELECT, "meeting_id": "scale-t1",
                                               "session_id": sess("o02"), "result": "proceeded"},
                                    "values": [[HS, "0"], [HS + STEP, "1.5e308"], [HS + 2 * STEP, "0"],
                                               [HS + 3 * STEP, "1.5e308"]]})
        self.assertEqual(score(m, w)["stability"]["o02"]["unplanned_reconnects"], 1)

    def test_conflicting_duplicates_on_the_pps_minimum_are_listed(self):
        m, w = Review2963Test().base()
        for value in ("50", "0"):
            w.series["_pps_min"].append({"metric": {"meeting_id": "scale-t1", "from_peer": "o00",
                                                    "session_id": sess("o00"), "to_peer": sess("t1")},
                                         "values": [[ts, value] for ts in w.grid()]})
        g8 = gate(score(m, w), "G-Q8", "quality_gates")
        self.assertIn("conflicting duplicate series", g8["value"])

    def test_non_finite_values_inside_tuples_are_nulled(self):
        nulled = []
        self.assertEqual(cli.finite_or_null({"a": (1.0, float("inf"))}, "$", nulled), {"a": [1.0, None]})
        self.assertEqual(nulled, ["$.a[1]"])

    def test_a_final_leave_in_the_last_scrape_still_counts(self):
        m, w = Review2963Test().base()
        m["events"] = [{"at": HE - 10, "step_id": "s1", "action": "leave", "participants": ["o03"]}]
        r = score(m, w)
        self.assertEqual((gate(r, "G-V16")["status"], gate(r, "G-V16")["value"]), ("pass", 11))

    def test_a_talker_unmuting_exactly_at_hold_start_is_invalid(self):
        m, w = Review2963Test().base()
        m["participants"][-1]["publishes"]["mic"] = False
        m["events"] = [{"at": HS, "step_id": "s1", "action": "unmute", "participants": ["t1"]}]
        r = score(m, w)
        self.assertIn("declared talker t1 is muted at hold_start", gate(r, "G-V13")["detail"])
        self.assertEqual(r["verdict"], "INVALID")


class ScreenConfigAndCoverageTest(unittest.TestCase):
    def base(self):
        return Review2963Test().base()

    def test_a_declared_screen_share_is_invalid(self):
        m, w = self.base()
        self.assertEqual(score(m, w)["verdict"], "PASS")
        m["participants"][5]["publishes"]["screen"] = True
        r = score(m, w)
        self.assertIn("screen share by o05", gate(r, "G-V13")["detail"])
        self.assertEqual(r["verdict"], "INVALID")

    def test_a_pre_hold_rejoin_does_not_absorb_an_in_hold_reconnect(self):
        m, w = self.base()
        m["events"] = [{"at": HS - 50, "step_id": "s1", "action": "leave", "participants": ["o03"]},
                       {"at": HS - 30, "step_id": "s1", "action": "rejoin", "participants": ["o03"]}]
        w.new_session("o03", HS - 30)
        w.new_session("o03", HS + 20 * STEP, n=3, prev=2)
        r = score(m, w)
        self.assertEqual(r["stability"]["o03"]["unplanned_reconnects"], 1)
        self.assertNotEqual(r["verdict"], "PASS")

    def test_a_stale_series_for_a_migrated_publisher_session_is_missing_data_in_either_order(self):
        outcomes = []
        for stale_last in (False, True):
            m, w = self.base()
            w.new_session("t1", HS + 20 * STEP)
            for s in w.series[M_PPS]:
                if s["metric"]["to_peer"] == sess("t1", 2) and s["metric"]["from_peer"] == "o00":
                    s["values"] = [[ts, "0"] for ts, _ in s["values"]]
            w.series[M_PPS] = [s for s in w.series[M_PPS]
                               if (s["metric"]["from_peer"], s["metric"]["to_peer"]) != ("o00", sess("t1"))]
            stale = {"metric": {"__name__": M_PPS, "meeting_id": "scale-t1", "from_peer": "o00",
                                "session_id": sess("o00"), "to_peer": sess("t1")},
                     "values": [[ts, "50"] for ts in w.grid()]}
            w.series[M_PPS].insert(len(w.series[M_PPS]) if stale_last else 0, stale)
            r = score(m, w)
            self.assertIn("session order unknown", gate(r, "G-Q8", "quality_gates")["value"], stale_last)
            outcomes.append((r["verdict"], r["cells"]["UxU"]["participants"]["o00"]["A"]))
        self.assertEqual(outcomes[0], outcomes[1])
        self.assertNotEqual(outcomes[0][0], "PASS")


class GateKnobAndSeriesClassTest(unittest.TestCase):
    EDGES = [("quality_gates", "k_red_fail", 1, 2), ("quality_gates", "split_rate_red", 0, 0.02),
             ("dimensions", "expand_ops_full_scale", 1, 100), ("dimensions", "nominal_talker_pps", 50, 50),
             ("quality_gates", "join_deadline_s", 30, 30), ("quality_gates", "drop_gap_s", 0, 30),
             ("quality_gates", "rejoin_grace_s", 0, 20), ("quality_gates", "per_talker_A_red", 0, 0.05)]

    def load(self, group, key, value):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "o.json")
            with open(path, "w") as fh:
                json.dump({group: {key: {"value": value}}}, fh)
            return cq_score.load_config(path)

    def test_gate_knobs_accept_their_edges_and_reject_just_outside(self):
        for group, key, lo, hi in self.EDGES:
            for ok in (lo, hi):
                self.assertEqual(cq_score.cv(self.load(group, key, ok), group, key), ok)
            step = max(abs(hi) * 0.05, 0.001)
            for bad in (lo - step, hi + step):
                with self.assertRaises(cq_score.ConfigError, msg=(key, bad)):
                    self.load(group, key, bad)

    def test_gate_knobs_can_only_be_made_stricter_than_the_shipped_default(self):
        cfg = cq_score.load_config()
        stricter_is_lower = {"k_red_fail", "split_rate_red", "expand_ops_full_scale", "drop_gap_s", "rejoin_grace_s",
                             "health_reports_max_ratio", "per_talker_A_red"}
        stricter_is_higher = {"health_reports_min_ratio", "n_min_observers"}
        fixed = {"scrape_step_s", "browser_health_interval_s", "nominal_talker_pps", "join_deadline_s"}
        self.assertEqual({k for _, k in cq_score.CONFIG_BOUNDS}, stricter_is_lower | stricter_is_higher | fixed)
        for (group, key), (lo, hi) in cq_score.CONFIG_BOUNDS.items():
            default = cq_score.cv(cfg, group, key)
            if key in stricter_is_lower:
                self.assertEqual(hi, default, key)
            elif key in stricter_is_higher:
                self.assertEqual(lo, default, key)
            elif key in fixed:
                self.assertEqual((lo, hi), (default, default), key)
        for key, value in (("k_red_fail", True), ("reconnect_gate_enabled", 0)):
            with self.assertRaises(cq_score.ConfigError, msg=key):
                self.load("quality_gates", key, value)

    def test_g_q7_between_red_and_twice_red_fails(self):
        m, w = Review2963Test().base()
        for s in w.series[M_PPS]:
            if (s["metric"]["from_peer"], s["metric"]["to_peer"]) == ("o00", sess("t1")):
                s["values"] = [[ts, "0" if ts == HS + 20 * STEP else v] for ts, v in s["values"]]
        g = gate(score(m, w), "G-Q7", "quality_gates")
        red = cq_score.cv(cq_score.load_config(), "quality_gates", "split_rate_red")
        self.assertTrue(red < g["value"] <= 2 * red, g)
        self.assertEqual(g["status"], "fail")

    def scored_series(self):
        m, _ = Review2963Test().base()
        cfg = cq_score.load_config()
        cfg["validity_gates"]["restarts_selector"]["value"] = "kube_pod_container_status_restarts_total"
        cfg["validity_gates"]["scrape_up_selector"]["value"] = 'up{job="metrics-api"}'
        names = {name for name, _, _, _ in cq_score.step_queries(m, m["steps"][0], cfg, None)}
        return m, cfg, names

    def world(self, server_mapped_failure=False):
        _, w = Review2963Test().base()
        w.add("_up", {}, 1)
        w.add("_restarts", {}, 0)
        if server_mapped_failure:
            w.add(cq_score.M_SERVER_CONN, {"session_id": "srv-t1", "customer_email": "t1"}, 1, JOIN, HE)
            w.add(M_REELECT, {"session_id": "srv-t1", "result": "failed"}, lambda t: int(t > HS + 300), JOIN, HE)
            for o in Review2963Test.OBS:
                w.add(M_RTT, {"peer_id": o, "session_id": sess(o), "server_type": "webtransport"}, 40)
            for x in w.series[M_EXPAND]:
                if x["metric"]["from_peer"] == "o00":
                    x["values"] = [[ts, "100"] for ts, _ in x["values"]]
        return w

    def test_a_non_finite_sample_on_any_gate_series_never_passes(self):
        m, cfg, names = self.scored_series()
        self.assertEqual(cq_score.score_step(m, m["steps"][0], self.world().data(), cfg)["verdict"], "PASS")
        for name in sorted(names - cq_score.DIAGNOSTIC_SERIES):
            for bad in ("NaN", "+Inf"):
                w = self.world()
                w.series[name].append({"metric": {"__name__": name, "meeting_id": "scale-t1"},
                                       "values": [[HS + 20 * STEP, bad]]})
                r = cq_score.score_step(m, m["steps"][0], w.data(), cfg)
                self.assertNotEqual(r["verdict"], "PASS", (name, bad))

    def test_a_diagnostic_series_feeds_no_gate(self):
        for split in (False, True):
            self.check_diagnostic_series(split)

    def check_diagnostic_series(self, split):
        m, cfg, names = self.scored_series()

        def gates(w):
            r = cq_score.score_step(m, m["steps"][0], w.data(), cfg, split_transport=split)
            return r["verdict"], [(g["gate"], g["status"], g["value"]) for g in r["validity"] + r["quality_gates"]]
        baseline = gates(self.world(True))
        self.assertIn(("G-Q2", "fail", 1), baseline[1])
        self.assertIn("fail", [st for g, st, _ in baseline[1] if g == "G-Q3"])
        diagnostic = names & cq_score.DIAGNOSTIC_SERIES
        self.assertTrue(diagnostic)
        for name in sorted(diagnostic):
            for bad in ("NaN", "+Inf", "1e9", "0", "-1"):
                w = self.world(True)
                for s in w.series[name]:
                    s["values"] = [[ts, bad] for ts, _ in s["values"]]
                for labels in ({"from_peer": "o00", "to_peer": sess("t1"), "peer_id": "o00"},
                               {"from_peer": "t1", "peer_id": "intruder", "customer_email": "intruder",
                                "result": "failed", "server_type": "websocket"}):
                    w.series[name].append({"metric": {"__name__": name, "meeting_id": "scale-t1", "room": "scale-t1",
                                                      "session_id": sess("o00"), **labels},
                                           "values": [[ts, bad if ts < HS + 20 * STEP else str(2 * float(bad))]
                                                      for ts in w.grid(JOIN, HE)]})
                self.assertEqual(gates(w), baseline, (name, bad))
            w = self.world(True)
            w.series.pop(name, None)
            self.assertEqual(gates(w), baseline, (name, "absent"))

    def test_receiver_sessions_born_together_cannot_pick_the_verdict_by_id(self):
        outcomes = []
        for sid in ("0000", "zzzz"):
            for bad_is_new in (True, False):
                m, w = Review2963Test().base()
                w.presence("o02", sid, lo=JOIN, hi=HE)
                for name, good, bad in ((M_PPS, 50, 0), (M_EXPAND, 0, 100)):
                    for s in w.series[name]:
                        if (s["metric"]["from_peer"], s["metric"]["to_peer"]) == ("o02", sess("t1")):
                            s["values"] = [[ts, str(good if bad_is_new else bad)] for ts, _ in s["values"]]
                    w.pair(name, "o02", sess("t1"), bad if bad_is_new else good, recv_session=sid)
                r = score(m, w)
                g8 = gate(r, "G-Q8", "quality_gates")
                self.assertIn("session order unknown", g8["value"], (sid, bad_is_new))
                outcomes.append(r["verdict"])
        self.assertEqual(set(outcomes), {"FAIL"})

    def test_rust_health_interval_is_one_of_the_bot_cadences(self):
        for value in (2.5, 2, 4):
            with self.assertRaises(cq_score.ConfigError, msg=value):
                self.load("validity_gates", "rust_health_interval_s", value)
        for value in (1, 5):
            self.assertEqual(cq_score.cv(self.load("validity_gates", "rust_health_interval_s", value),
                                         "validity_gates", "rust_health_interval_s"), value)

    def test_three_series_of_one_receiver_cannot_pick_the_verdict_by_order(self):
        def world(order):
            m, w = Review2963Test().base()
            w.new_session("t1", HS + 10 * STEP)
            w.new_session("t1", HS + 30 * STEP, n=3, prev=2)
            w.series[M_REELECT].append({"metric": {"__name__": M_REELECT, "meeting_id": "scale-t1",
                                                   "session_id": sess("t1"), "result": "proceeded"},
                                        "values": [[t, str((t >= HS + 10 * STEP) + (t >= HS + 30 * STEP))]
                                                   for t in w.grid(JOIN, HE)]})
            mine = [s for s in w.series[M_PPS] if s["metric"]["from_peer"] == "o00"
                    and s["metric"]["to_peer"] in (sess("t1"), sess("t1", 2), sess("t1", 3))]
            others = [s for s in w.series[M_PPS] if s not in mine]
            stale_at = (HS + 10 * STEP, HS + 11 * STEP)
            by_peer = {s["metric"]["to_peer"]: s for s in mine}
            s2 = by_peer[sess("t1", 2)]
            s2["values"] = [[ts, "0" if ts in stale_at else v] for ts, v in s2["values"]]
            by_peer[sess("t1")]["values"] += [[ts, "50"] for ts in stale_at]
            w.series[M_PPS] = others + [by_peer[p] for p in order]
            return score(m, w)
        import itertools
        outcomes = set()
        for order in itertools.permutations((sess("t1"), sess("t1", 2), sess("t1", 3))):
            r = world(order)
            self.assertIn("session order unknown", gate(r, "G-Q8", "quality_gates")["value"], order)
            outcomes.add((r["verdict"], round(r["cells"]["UxU"]["participants"]["o00"]["A"], 6)))
        self.assertEqual(len(outcomes), 1, outcomes)
        self.assertNotEqual(next(iter(outcomes))[0], "PASS")


class ErrorRedactionTest(unittest.TestCase):
    def test_errors_never_echo_hidden_identities(self):
        m = cq_manifest.load_manifest(EXAMPLE)
        m["participants"][0]["fleet"] = "human"

        def failing(url, body, headers):
            raise RuntimeError(f"cannot reach {url}")
        with tempfile.TemporaryDirectory() as tmp:
            man = os.path.join(tmp, "m.json")
            with open(man, "w") as fh:
                json.dump(m, fh)
            rc, err = main_with_stderr(["--manifest", man, "--prom-url", "http://prom.invalid:9090"],
                                       transport=failing, environ={})
            self.assertEqual(rc, 3, err)
            self.assertIn("request failed", err)
            self.assertNotIn("probe-000", err)


class UrlAndArgumentHardeningTest(unittest.TestCase):
    def test_a_malformed_prom_url_is_an_error_not_a_verdict(self):
        rc, err = main_with_stderr(["--manifest", EXAMPLE, "--prom-url", "http://[::1"], environ={})
        self.assertEqual(rc, 3, err)

    def test_abbreviated_flags_are_refused_and_never_echo_their_values(self):
        for argv in (["--prom", "http://u:s3cr3t@h:1"], ["--prom-url", "http://x", "--exclude", "secret-person"]):
            with tempfile.TemporaryDirectory() as tmp:
                err = io.StringIO()
                with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()), \
                        self.assertRaises(SystemExit) as cm:
                    cli.main(["--manifest", EXAMPLE, "--out-dir", tmp] + argv,
                             transport=World().transport([]), environ={})
                self.assertEqual(cm.exception.code, 3, argv)
                self.assertFalse(os.path.exists(os.path.join(tmp, "report.md")), argv)
                for leaked in ("s3cr3t", "secret-person"):
                    self.assertNotIn(leaked, err.getvalue(), argv)

    def test_a_prom_url_with_credentials_is_refused_without_echoing_them(self):
        for url, secrets in (("http://admin:S3C/R3T@127.0.0.1:9", ("S3C", "R3T")),
                             ("http://u:p?w#d@h:1", ("p?w", "#d")), ("http://a@b:c@h", ("b:c",)),
                             ("http://user:s3cr3t@prom.invalid", ("s3cr3t",))):
            rc, err = main_with_stderr(["--manifest", EXAMPLE, "--prom-url", url], environ={})
            self.assertEqual(rc, 3, err)
            self.assertIn("--auth-basic-env", err)
            for secret in secrets:
                self.assertNotIn(secret, err, url)

    def test_manifest_errors_pseudonymise_human_ids(self):
        m = cq_manifest.load_manifest(EXAMPLE)
        for p in m["participants"][:2]:
            p["fleet"], p["user_id"] = "human", "jane.doe@corp.example"
        with tempfile.TemporaryDirectory() as tmp:
            man = os.path.join(tmp, "m.json")
            with open(man, "w") as fh:
                json.dump(m, fh)
            rc, err = main_with_stderr(["--manifest", man, "--prom-url", "http://prom.invalid"], environ={})
        self.assertEqual(rc, 3, err)
        self.assertIn("duplicate user_id", err)
        self.assertNotIn("jane.doe", err)

    def test_nulled_paths_never_carry_a_raw_identity(self):
        m = cq_manifest.load_manifest(EXAMPLE)
        m["participants"][0]["fleet"] = "human"
        real = cq_score.score_run

        def poisoned(*a, **kw):
            out = real(*a, **kw)
            out["steps"][0]["split"]["zero_packet_receivers"]["probe-000@bots-app.local <- t"] = float("inf")
            return out
        with tempfile.TemporaryDirectory() as tmp:
            man = os.path.join(tmp, "m.json")
            with open(man, "w") as fh:
                json.dump(m, fh)
            cq_score.score_run = poisoned
            try:
                rc = quiet_main(["--manifest", man, "--prom-url", "http://prom.invalid", "--out-dir", tmp],
                                transport=World().transport([]), environ={})
            finally:
                cq_score.score_run = real
            with open(os.path.join(tmp, "result.json"), encoding="utf-8") as fh:
                text = fh.read()
        self.assertEqual(rc, 2)
        self.assertNotIn("probe-000", text)
        self.assertEqual(len(json.loads(text)["run"]["non_finite_values_nulled"]), 1)

    def test_report_labels_cannot_inject_markdown_or_mentions(self):
        evil = "@octocat [x](http://evil) | **b** `c` <img>"
        m = manifest([part("o1"), part("o2"), part("t1", "rust", False, True)])
        w = healthy_world(["o1", "o2"], ["t1"])
        w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1"), "server_type": evil}, 40)
        result = cq_score.score_run(m, {"s1": w.data()}, cq_score.load_config(), split_transport=True)
        result["steps"][0]["flags"].append(evil)
        md = cq_report.render_markdown(result, "cmd")
        self.assertNotIn("@octocat", md)
        self.assertIsNone(re.search(r"(?<!\\)\]\(", md))
        self.assertNotIn("http://evil", md)
        self.assertNotIn("<img>", md)
        self.assertNotIn("**b**", md)
        self.assertNotIn(" | **b", md)

    def test_rust_health_interval_rejects_a_boolean(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "o.json")
            with open(path, "w") as fh:
                json.dump({"validity_gates": {"rust_health_interval_s": {"value": True}}}, fh)
            with self.assertRaises(cq_score.ConfigError):
                cq_score.load_config(path)

    def test_split_cells_are_marked_report_only_in_the_report(self):
        m = manifest([part("o1"), part("o2"), part("t1", "rust", False, True)])
        w = healthy_world(["o1", "o2"], ["t1"])
        w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1"), "server_type": "webtransport"}, 40)
        result = cq_score.score_run(m, {"s1": w.data()}, cq_score.load_config(), split_transport=True)
        md = cq_report.render_markdown(result, "cmd")
        headers = {line.split("**")[1]: line for line in md.splitlines() if line.startswith("**UxU")}
        self.assertIn("report-only (not gated)", headers["UxU/webtransport"])
        self.assertIn("report-only (not gated)", headers["UxU/unknown"])
        self.assertNotIn("report-only", headers["UxU"])

    def test_receiver_sessions_with_the_same_first_sample_tie_whatever_their_last(self):
        outcomes = []
        for bad_on_shorter in (True, False):
            m, w = Review2963Test().base()
            w.presence("o02", "zzzz", lo=JOIN, hi=HE - 10 * STEP)
            for name, good, bad in ((M_PPS, 50, 0), (M_EXPAND, 0, 100)):
                for x in w.series[name]:
                    if (x["metric"]["from_peer"], x["metric"]["to_peer"]) == ("o02", sess("t1")):
                        x["values"] = [[ts, str(good if bad_on_shorter else bad)] for ts, _ in x["values"]]
                w.pair(name, "o02", sess("t1"), bad if bad_on_shorter else good, recv_session="zzzz",
                       hi=HE - 10 * STEP)
            r = score(m, w)
            self.assertIn("session order unknown", gate(r, "G-Q8", "quality_gates")["value"], bad_on_shorter)
            outcomes.append(r["verdict"])
        self.assertEqual(outcomes, ["FAIL", "FAIL"])


class ObserverReportEdgeTest(unittest.TestCase):
    def report_gap(self, ts_drop, obs=None, talkers=("t1",)):
        obs = obs or Review2963Test.OBS
        m = manifest([part(o) for o in obs] + [part(t, "rust", False, True) for t in talkers])
        w = healthy_world(list(obs), list(talkers))
        for lst in w.series.values():
            for s in lst:
                if s["metric"].get("from_peer") == "o00":
                    s["values"] = [x for x in s["values"] if x[0] != ts_drop]
        return score(m, w)

    def assert_observer_report_miss(self, r, at):
        g8 = gate(r, "G-Q8", "quality_gates")
        self.assertIn("observer report", g8["value"], g8)
        self.assertIn(f"o00 @ {at:.0f}", g8["detail"])
        self.assertNotEqual(r["verdict"], "PASS")

    def test_an_empty_report_only_at_hold_end_is_missing(self):
        self.assert_observer_report_miss(self.report_gap(HE), HE)

    def test_an_empty_report_only_at_hold_start_is_missing(self):
        self.assert_observer_report_miss(self.report_gap(HS), HS)

    def test_an_empty_report_with_exactly_one_other_participant_present_is_missing(self):
        self.assert_observer_report_miss(self.report_gap(HS + 20 * STEP, obs=["o00"]), HS + 20 * STEP)

    def test_a_self_pair_is_not_a_report(self):
        m, w = Review2963Test().base()
        cut = HS + 20 * STEP
        for lst in w.series.values():
            for s in lst:
                if s["metric"].get("from_peer") == "o00":
                    s["values"] = [x for x in s["values"] if x[0] != cut]
        w.pair(M_CAN_LISTEN, "o00", sess("o00"), 1)
        self.assert_observer_report_miss(score(m, w), cut)

    def test_a_recorded_leave_two_scrapes_before_hold_end_is_a_drop(self):
        r = RecordedLeaveTest().world("o03", HE - 2 * STEP)
        self.assertIn("counted as present (a drop)", " ".join(r["flags"]))
        self.assertEqual(r["verdict"], "FAIL")


class TalkerCoverageTest(unittest.TestCase):
    OBS = [f"o{i:02d}" for i in range(10)]

    def run_with(self, n_talkers, silent_until=None, cfg=None):
        talkers = [f"t{i:02d}" for i in range(n_talkers)]
        m = manifest([part(o) for o in self.OBS] + [part(t, "rust", False, True) for t in talkers])
        w = healthy_world(self.OBS, talkers)
        if silent_until is not None:
            for s in w.series[M_PPS]:
                if s["metric"]["to_peer"] == sess("t00"):
                    s["values"] = [[ts, "0" if ts < silent_until else v] for ts, v in s["values"]]
        return cq_score.score_step(m, m["steps"][0], w.data(), cfg or cq_score.load_config())

    def test_a_talker_nobody_hears_is_not_diluted_by_many_talkers(self):
        r = self.run_with(21, silent_until=HE + 1)
        self.assertLess(r["cells"]["UxU"]["participants"]["o00"]["A"], 0.05)
        g = gate(r, "G-Q3", "quality_gates")
        self.assertEqual(g["status"], "fail", g)
        self.assertEqual(g["value"]["worst_talker_A"], 1.0)
        self.assertIn("t00 (1.000)", g["detail"])
        self.assertEqual(r["verdict"], "FAIL")

    def test_a_talker_nobody_hears_for_half_the_hold_fails(self):
        r = self.run_with(20, silent_until=HS + (HE - HS) / 2)
        g = gate(r, "G-Q3", "quality_gates")
        self.assertEqual(g["status"], "fail", g)
        self.assertAlmostEqual(r["cells"]["UxU"]["talker_A"]["t00"], 20 / 41)
        self.assertEqual(r["verdict"], "FAIL")

    def test_many_healthy_talkers_pass(self):
        r = self.run_with(21)
        g = gate(r, "G-Q3", "quality_gates")
        self.assertEqual((g["status"], g["value"]["worst_talker_A"]), ("pass", 0.0))
        self.assertEqual(r["verdict"], "PASS")

    def test_a_shaped_talker_nobody_hears_is_not_in_the_per_talker_gate(self):
        talkers = ["t00", "t01"]
        m = manifest([part(o) for o in self.OBS] + [part("t00", "rust", False, True),
                                                    part("t01", "rust", False, True, shaped=True)])
        w = healthy_world(self.OBS, talkers)
        for s in w.series[M_PPS]:
            if s["metric"]["to_peer"] == sess("t01"):
                s["values"] = [[ts, "0"] for ts, _ in s["values"]]
        r = score(m, w)
        self.assertEqual(set(r["cells"]["UxU"]["talker_A"]), {"t00"})
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "pass")
        self.assertEqual(r["verdict"], "PASS")

    def test_a_human_talker_is_pseudonymised_in_talker_a(self):
        human = "alice.human@example.com"
        m = manifest([part(o) for o in self.OBS] + [part(human, "human", False, True)])
        w = healthy_world(self.OBS, [human])
        r = score(m, w)
        self.assertIn(human, r["cells"]["UxU"]["talker_A"])
        hidden = cq_privacy.pseudonymise(r, cq_privacy.identities_to_hide(m, {"s1": w.data()}, False), b"salt")
        self.assertNotIn("alice.human", json.dumps(hidden))

    def test_split_cells_carry_no_talker_a(self):
        r = self.run_with(2)
        self.assertIn("talker_A", r["cells"]["UxU"])
        m = manifest([part(o) for o in self.OBS] + [part("t00", "rust", False, True)])
        w = healthy_world(self.OBS, ["t00"])
        r = score(m, w, split_transport=True)
        self.assertIn("talker_A", r["cells"]["UxU"])
        self.assertNotIn("talker_A", r["cells"]["UxU/unknown"])

    def test_a_talker_at_the_per_talker_red_line_passes_and_just_above_fails(self):
        cfg = cq_score.load_config()
        r = self.run_with(21, silent_until=HS + STEP, cfg=cfg)
        a = r["cells"]["UxU"]["talker_A"]["t00"]
        cfg["quality_gates"]["per_talker_A_red"]["value"] = a
        self.assertEqual(gate(self.run_with(21, silent_until=HS + STEP, cfg=cfg), "G-Q3", "quality_gates")["status"],
                         "pass")
        cfg["quality_gates"]["per_talker_A_red"]["value"] = a * 0.99
        self.assertEqual(gate(self.run_with(21, silent_until=HS + STEP, cfg=cfg), "G-Q3", "quality_gates")["status"],
                         "fail")


class WorkedExampleTest(unittest.TestCase):
    """Doc section 10: gates read p95 and k-of-n per dimension; there is no blended score."""

    def test_doc_worked_example(self):
        bands = cq_score.load_config()["bands"]
        p95 = {"A": 0.018, "V": 0.009, "Q": 0.02}
        for d, v in p95.items():
            summary = cq_score.summarize_dimension({f"u{i}": v for i in range(16)}, bands[d], 2)
            self.assertAlmostEqual(summary["p95"], v)
            self.assertFalse(summary["gate_fail"])
        two_red = {f"u{i}": (0.06 if i < 2 else 0.0) for i in range(40)}
        self.assertTrue(cq_score.summarize_dimension(two_red, bands["A"], 2)["gate_fail"])

    def test_percentile_is_linear_interpolation(self):
        self.assertAlmostEqual(cq_score.percentile(list(range(1, 17)), 95), 15.25)


class CliTest(unittest.TestCase):
    def test_end_to_end_with_manifest(self):
        m = cq_manifest.load_manifest(EXAMPLE)
        headline = next(s for s in m["steps"] if s["headline"])
        w = World()
        seen = []
        with tempfile.TemporaryDirectory() as tmp:
            man = os.path.join(tmp, "m.json")
            with open(man, "w") as fh:
                json.dump(m, fh)
            rc = cli.main(["--manifest", man, "--prom-url", "http://prom.invalid", "--auth-bearer-env", "CQ_TOKEN",
                           "--out-dir", tmp], transport=w.transport(seen), environ={"CQ_TOKEN": "t0k"})
            result = read_json(os.path.join(tmp, "result.json"))
            with open(os.path.join(tmp, "report.md"), encoding="utf-8") as fh:
                report = fh.read()
        self.assertEqual(rc, 2)
        self.assertEqual(result["verdict"], "INVALID")
        self.assertEqual(result["headline_step"], headline["step_id"])
        self.assertIn("## Step `n5`", report)
        self.assertTrue(all(h.get("Authorization") == "Bearer t0k" for _, _, h in seen))
        self.assertTrue(all(u == "http://prom.invalid/api/v1/query_range" for u, _, _ in seen))
        pair_q = [q for _, q, _ in seen if q.startswith(M_FPS)]
        self.assertIn('from_peer=~"probe-000@bots-app\\\\.local|', pair_q[0])

    def test_missing_secret_env_is_an_error(self):
        rc = quiet_main(["--manifest", EXAMPLE, "--prom-url", "http://prom.invalid", "--auth-bearer-env", "CQ_NONE"],
                      transport=World().transport([]), environ={})
        self.assertEqual(rc, cli.EXIT_ERROR)

    def test_real_meeting_mode_reports_without_manifest_gates(self):
        w = healthy_world(["alice@example.com", "bob@example.com"], [], audio_expand=0.0)
        w.pair(M_PPS, "alice@example.com", sess("bob@example.com"), 50)
        w.pair(M_EXPAND, "alice@example.com", sess("bob@example.com"), 5)
        w.pair(M_FPS, "alice@example.com", sess("bob@example.com"), 30)
        with tempfile.TemporaryDirectory() as tmp:
            rc = cli.main(["--meeting", "scale-t1", "--window", f"{HS},{HE}", "--prom-url", "http://prom.invalid",
                           "--out-dir", tmp, "--no-pseudonymise"], transport=w.transport([]), environ={})
            result = read_json(os.path.join(tmp, "result.json"))
        step = result["steps"][0]
        self.assertEqual(rc, 0)
        self.assertEqual(result["verdict"], "REPORT")
        self.assertTrue(step["dimension_A_status"].startswith("pause-confounded"))
        for g in ("G-V1", "G-V5", "G-V7", "G-V8", "G-V9", "G-V10", "G-V11", "G-Q1"):
            section = "quality_gates" if g.startswith("G-Q") else "validity"
            self.assertEqual(gate(step, g, section)["status"], "not_applicable", g)
        self.assertIn("alice@example.com", step["cells"]["UxU"]["participants"])

    def test_invalid_manifest_exits_with_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            man = os.path.join(tmp, "m.json")
            with open(man, "w") as fh:
                json.dump({"schema": cq_manifest.SCHEMA}, fh)
            self.assertEqual(quiet_main(["--manifest", man, "--validate-only"]), cli.EXIT_ERROR)


class ClientHardeningTest(unittest.TestCase):
    def test_retries_retryable_errors_with_backoff_then_succeeds(self):
        calls, sleeps = [], []
        ok = json.dumps({"status": "success", "data": {"resultType": "matrix", "result": []}}).encode()

        def flaky(url, body, headers):
            calls.append(1)
            if len(calls) < 3:
                raise urllib.error.HTTPError(url, 503, "busy", {}, None)
            return ok

        client = cq_prom.PromClient("http://p.invalid", transport=flaky, retries=3, backoff_s=0.5,
                                    sleep=sleeps.append)
        self.assertEqual(client.range("up", 0, 10, 15), [])
        self.assertEqual(sleeps, [0.5, 1.0])

    def test_non_retryable_error_fails_immediately(self):
        def bad(url, body, headers):
            raise urllib.error.HTTPError(url, 400, "bad query", {}, None)

        client = cq_prom.PromClient("http://p.invalid", transport=bad, sleep=lambda s: self.fail("slept"))
        with self.assertRaises(cq_prom.PromError):
            client.range("up{", 0, 10, 15)

    def test_window_parsing_is_strict(self):
        self.assertEqual(cli.parse_window("2026-09-30T09:00:00Z,2026-09-30T09:10:00+00:00")[1]
                         - cli.parse_window("2026-09-30T09:00:00Z,2026-09-30T09:10:00+00:00")[0], 600)
        for bad in ("2026-09-30T09:00:00,2026-09-30T09:10:00", "1600,1000", "1000", "a,b"):
            with self.assertRaises(cli.UsageError, msg=bad):
                cli.parse_window(bad)

    def test_prom_url_credentials_are_redacted_in_the_report(self):
        argv = ["--manifest", "m.json", "--prom-url", "https://user:secret@prom.invalid:9090/api/x?token=abc"]
        text = " ".join(cli.redacted_argv(argv))
        self.assertIn("https://prom.invalid:9090/api/x", text)
        self.assertNotIn("secret", text)
        self.assertNotIn("abc", text)

    def test_human_reporters_count_toward_expected_health_reports(self):
        obs = [f"o{i:02d}" for i in range(10)]
        parts = [part(o, fleet="human") for o in obs] + [part("t1", "rust", False, True)]
        m = manifest(parts)
        w = healthy_world(obs, ["t1"])
        self.assertEqual(gate(score(m, w), "G-V1")["value"], 1.0)


class ConfigAndQueryTest(unittest.TestCase):
    def test_every_threshold_is_marked_as_proposal(self):
        cfg = cq_score.load_config()
        for group in ("sampling", "bands", "dimensions", "quality_gates", "latency_gate", "validity_gates"):
            for key, entry in cfg[group].items():
                self.assertEqual(entry.get("status"), "proposal", f"{group}.{key}")

    def test_config_override_is_deep_merged(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "o.json")
            with open(path, "w") as fh:
                json.dump({"quality_gates": {"k_red_fail": {"value": 1}}}, fh)
            cfg = cq_score.load_config(path)
        self.assertEqual(cq_score.cv(cfg, "quality_gates", "k_red_fail"), 1)
        self.assertEqual(cq_score.cv(cfg, "quality_gates", "join_deadline_s"), 30)
        self.assertEqual(cfg["quality_gates"]["k_red_fail"]["status"], "proposal")

    def test_selector_escapes_regex_and_string(self):
        q = cq_prom.selector("m", [("from_peer", "=~", cq_prom.any_of_regex(['a.b+c@x.io', 'q"z']))])
        self.assertEqual(q, 'm{from_peer=~"a\\\\.b\\\\+c@x\\\\.io|q\\"z"}')

    def test_parse_matrix_drops_nan_and_rejects_errors(self):
        out = cq_prom.parse_matrix({"status": "success", "data": {"resultType": "matrix", "result": [
            {"metric": {"a": "1"}, "values": [[1, "NaN"], [2, "3.5"]]}]}})
        self.assertEqual(out, [({"a": "1"}, [(2.0, 3.5)])])
        with self.assertRaises(cq_prom.PromError):
            cq_prom.parse_matrix({"status": "error", "error": "bad query"})

    def test_step_queries_restrict_pair_metrics_to_observers(self):
        m = cq_manifest.load_manifest(EXAMPLE)
        queries = cq_score.step_queries(m, m["steps"][0], cq_score.load_config(), {"probe-000@bots-app.local"})
        pair = [q for name, q, _, _ in queries if name in cq_score.PAIR_METRICS]
        self.assertEqual(len(pair), len(cq_score.PAIR_METRICS))
        self.assertTrue(all('from_peer=~"probe-000@bots-app\\\\.local"' in q for q in pair))


if __name__ == "__main__":
    unittest.main(verbosity=1)
