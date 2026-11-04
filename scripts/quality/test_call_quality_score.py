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
    M_CAN_LISTEN, M_DISCARDS, M_EXPAND, M_FPS, M_FREEZE, M_HEALTH, M_PPS, M_REELECT, M_RTT, M_SENT, M_STALE, M_UNDERRUN)

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
            payload = self.payload("_cfs" if "container_cpu_cfs_periods_total" in query
                                   else query.removeprefix("min_over_time(").split("{", 1)[0])
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
            w.pair(M_DISCARDS, o, sess(t), 0)
            w.pair(M_UNDERRUN, o, sess(t), 0)
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

    def test_recorded_transport_fields_are_checked(self):
        def bad(m):
            m["participants"][0].update(transport_observed="quic", transport_fallbacks=-1)
        errs = self.errors_for(bad)
        self.assertIn("transport_observed: must be one of", errs)
        self.assertIn("transport_fallbacks: must be a non-negative integer", errs)

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
        for x in w.series[M_SENT]:
            if x["metric"]["session_id"] == sess("o02"):
                x["values"] = [v for v in x["values"] if v[0] <= mid + STEP]
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

    def test_t8_short_identities_in_structured_fields_are_pseudonymised(self):
        users = ["Li", "bob@example.com"]
        w = healthy_world(users, [], audio_expand=0.0)
        w.pair(M_PPS, users[0], sess(users[1]), 50)
        w.add(M_RTT, {"peer_id": "Li", "session_id": sess("Li"), "server_type": "webtransport"}, 40, hi=HS)
        w.add(M_RTT, {"peer_id": "Li", "session_id": "777", "server_type": "websocket"}, 40, lo=HS + STEP)
        w.add(cq_score.M_PEER_INFO, {"peer_id": "Li", "session_id": "777"}, 1)
        w.add("videocall_relay_connection_rtt_ms", {"room": "scale-t1", "session_id": sess("Li")}, 280)
        for lst in w.series.values():
            for x in lst:
                for k, v in list(x["metric"].items()):
                    if v == sess("Li"):
                        x["metric"][k] = "42"
        with tempfile.TemporaryDirectory() as tmp:
            rc, _ = main_with_stderr(["--meeting", "scale-t1", "--window", f"{HS},{HE}", "--prom-url",
                                      "http://prom.invalid", "--out-dir", tmp], transport=w.transport([]), environ={})
            self.assertEqual(rc, 0)
            result = read_json(os.path.join(tmp, "result.json"))
            with open(os.path.join(tmp, "report.md"), encoding="utf-8") as fh:
                report = fh.read()
        step = result["steps"][0]
        self.assertTrue(result["run"]["pseudonymised"])
        self.assertTrue(step["transport_timeline"] and step["server_diagnostics"]["quic"]["by_user"])
        blob = json.dumps(result)
        self.assertIn(" <- ", step["diagnostics"]["max_staleness_pair"])
        self.assertTrue([f for f in step["flags"] if "transport switched" in f])
        for raw in ('"Li"', '"42"', '"Li <- ', ' <- Li"', '"Li: '):
            self.assertNotIn(raw, blob)
        for raw in ("| Li ", "| 42 "):
            self.assertNotIn(raw, report)

    def test_t8_a_session_seen_only_as_a_publisher_is_pseudonymised(self):
        m, w, _ = self.world()
        ghost = "88888"
        for value in (50, 40):
            w.pair(M_PPS, "o00", ghost, value)
        data = w.data()
        r = cq_score.score_step(m, m["steps"][0], data, cq_score.load_config())
        self.assertIn(ghost, r["diagnostics"]["unknown_publisher_sessions"])
        self.assertIn(ghost, gate(r, "G-Q8", "quality_gates")["detail"])
        out = cq_privacy.pseudonymise(r, cq_privacy.identities_to_hide(m, {"s1": data}, real=False), b"salt")
        self.assertNotIn(ghost, json.dumps(out))

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
        sessions = {lb[k] for series in data.values() for lb, _ in series for k in ("session_id", "to_peer") if lb.get(k)}
        self.assertEqual(hidden, {"intruder@example.com"} | sessions)
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
        stricter_is_higher = {"health_reports_min_ratio", "n_min_observers", "discard_weight", "playout_underrun_weight"}
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



AUDIO_FIXTURES = os.path.join(HERE, "testdata", "audio_stalls_20261026")


def fixture_step(run, cfg=None):
    with open(os.path.join(AUDIO_FIXTURES, f"{run}.json"), encoding="utf-8") as fh:
        doc = json.load(fh)
    w = World()
    for name, payload in doc["payloads"].items():
        w.series[name] = payload["data"]["result"]
    for u, sid in {**doc["observers"], **doc["bots"]}.items():
        w.add(M_SENT, {"peer_id": u, "session_id": sid}, 50, *doc["hold"])
    parts = [dict(part(u), publishes={"camera": False, "mic": False, "screen": False}) for u in doc["observers"]]
    parts += [dict(part(b, "rust", False, b in doc["talkers"]),
                   publishes={"camera": True, "mic": b in doc["talkers"], "screen": False}) for b in doc["bots"]]
    m = manifest(parts)
    m["steps"][0]["hold_end"] = doc["hold"][1]
    return cq_score.score_step(m, m["steps"][0], w.data(), cfg or cq_score.load_config())


def report_of(r):
    return cq_report.render_markdown({"run": {"meeting_id": "m", "mode": "run", "bands_version": "b", "commit": "c",
                                              "scrape_step_s": 15}, "verdict": r["verdict"], "headline_step": "s1",
                                      "steps": [r]})


def a_of(r, user):
    return r["cells"]["UxU"]["participants"][user]["A"]


class AudioStallTest(unittest.TestCase):
    OBS = [f"o{i:02d}" for i in range(10)]

    def world(self):
        m = manifest([part(o) for o in self.OBS] + [part("t1", "rust", False, True)])
        return m, healthy_world(self.OBS, ["t1"])

    def bump(self, w, name, recv, at, amount, cause=None):
        for s in w.series[name]:
            if s["metric"]["from_peer"] == recv and (cause is None or s["metric"].get("cause") == cause):
                s["values"] = [[ts, str(float(v) + (amount if ts >= at else 0))] for ts, v in s["values"]]

    def test_discarded_packets_add_their_duration_to_that_observers_a_only(self):
        m, w = self.world()
        before = score(m, w)
        self.bump(w, M_DISCARDS, "o01", HS + 10 * STEP, 40)
        after = score(m, w)
        self.assertAlmostEqual(a_of(after, "o01") - a_of(before, "o01"), 40 * 20 / 15000 / 41)
        self.assertEqual([a_of(after, o) for o in self.OBS if o != "o01"],
                         [a_of(before, o) for o in self.OBS if o != "o01"])
        self.assertAlmostEqual(after["cells"]["UxU"]["participants"]["o01"]["A_terms"]["discarded"],
                               40 * 20 / 15000 / 41)

    def test_playout_silence_adds_to_concealment_and_can_turn_g_q3_red(self):
        m, w = self.world()
        self.bump(w, M_UNDERRUN, "o02", HS + 10 * STEP, 1500)
        r = score(m, w)
        self.assertAlmostEqual(a_of(r, "o02"), 0.1 / 41)
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "pass")
        for o in self.OBS[:3]:
            for k in range(3):
                self.bump(w, M_UNDERRUN, o, HS + (11 + k) * STEP, 15000)
        self.assertEqual(gate(score(m, w), "G-Q3", "quality_gates")["status"], "fail")

    def test_playout_silence_is_added_to_concealment_not_maxed_and_capped_at_one(self):
        m = manifest([part(o) for o in self.OBS] + [part("t1", "rust", False, True)])
        w = healthy_world(self.OBS, ["t1"], audio_expand=10)
        self.bump(w, M_UNDERRUN, "o01", HS + 10 * STEP, 1500)
        self.bump(w, M_UNDERRUN, "o02", HS + 10 * STEP, 15000)
        r = score(m, w)
        self.assertAlmostEqual(a_of(r, "o01"), (40 * 0.1 + 0.2) / 41)
        self.assertAlmostEqual(a_of(r, "o02"), (40 * 0.1 + 1.0) / 41)

    def test_discards_count_only_beyond_the_concealment_and_silence_of_their_bucket(self):
        m = manifest([part(o) for o in self.OBS] + [part("t1", "rust", False, True)])
        w = healthy_world(self.OBS, ["t1"], audio_expand=10)
        self.bump(w, M_DISCARDS, "o01", HS + 10 * STEP, 60)
        self.bump(w, M_DISCARDS, "o02", HS + 10 * STEP, 150)
        self.bump(w, M_UNDERRUN, "o03", HS + 10 * STEP, 1500)
        self.bump(w, M_DISCARDS, "o03", HS + 10 * STEP, 150)
        r = score(m, w)
        self.assertAlmostEqual(a_of(r, "o01"), 41 * 0.1 / 41)
        self.assertAlmostEqual(a_of(r, "o02"), (40 * 0.1 + 0.2) / 41)
        self.assertAlmostEqual(a_of(r, "o03"), (40 * 0.1 + 0.2) / 41)

    def test_an_absent_counter_is_not_measured_never_zero(self):
        m, w = self.world()
        base = score(m, w)
        w.series[M_UNDERRUN] = [s for s in w.series[M_UNDERRUN] if s["metric"]["from_peer"] != "o03"]
        r = score(m, w)
        self.assertEqual(r["diagnostics"]["audio_counter_coverage"]["o03"],
                         {"UxU": {"discarded audio": [1, 1], "playout silence": [0, 1]}})
        self.assertIn("dimension A: playout silence measured for 9 of 10 observer-talker pairs; A leaves it out where "
                      "it is missing", r["notices"])
        self.assertIn("| o03 | 0 | 0 | 0 | 0 | not measured | 0 |", report_of(r))
        self.assertEqual(a_of(r, "o03"), a_of(base, "o03"))
        self.assertEqual(gate(r, "G-Q8", "quality_gates")["status"], "pass")

    def test_one_talker_short_reads_as_partial_coverage(self):
        m = manifest([part(o) for o in self.OBS] + [part("t1", "rust", False, True), part("t2", "rust", False, True)])
        w = healthy_world(self.OBS, ["t1", "t2"])
        w.series[M_DISCARDS] = [x for x in w.series[M_DISCARDS]
                                if (x["metric"]["from_peer"], x["metric"]["to_peer"]) != ("o05", sess("t2"))]
        r = score(m, w)
        self.assertEqual(r["diagnostics"]["audio_counter_coverage"]["o05"]["UxU"]["discarded audio"], [1, 2])
        self.assertIn("dimension A: discarded audio measured for 19 of 20 observer-talker pairs; A leaves it out where "
                      "it is missing", r["notices"])
        self.assertIn("| o05 | 0 | 0 | 0 | 0 (1/2 talkers) | 0 | 0 |", report_of(r))

    def test_coverage_is_scoped_to_the_cell_it_is_shown_in(self):
        m = manifest([part(o) for o in self.OBS] + [part("t1", "rust", False, True),
                                                     part("t2", "rust", False, True, shaped=True)])
        w = healthy_world(self.OBS, ["t1", "t2"])
        w.series[M_DISCARDS] = [x for x in w.series[M_DISCARDS] if x["metric"]["to_peer"] != sess("t2")]
        r = score(m, w)
        self.assertEqual(r["diagnostics"]["audio_counter_coverage"]["o05"]["UxS"]["discarded audio"], [0, 1])
        text = report_of(r)
        shaped, unshaped = text.split("**UxS**")[1].split("**UxU**")
        self.assertIn("| o05 | 0 | 0 | 0 | 0 | 0 | 0 |", unshaped)
        self.assertIn("| o05 | 0 | 0 | 0 | not measured | 0 | 0 |", shaped)

    def test_a_real_meeting_shows_no_mechanism_table(self):
        m, w = self.world()
        r = score(m, w, mode="real")
        self.assertFalse([p for c in r["cells"].values() for p in c["participants"].values() if "A_terms" in p])
        self.assertNotIn("Dimension A by mechanism", report_of(r))

    def test_a_reconnect_books_the_lower_reading_and_lists_what_the_upper_adds(self):
        for second, booked, unproven in ((15, 15, [f"o04 <- t1: {M_DISCARDS} continuation or rebuild (10)"]),
                                         (3, 13, []), (0, 10, [])):
            with self.subTest(second=second):
                m, w = self.world()
                self.bump(w, M_DISCARDS, "o04", HS + 5 * STEP, 10)
                w.new_session("o04", HS + 20 * STEP)
                for x in w.series[M_DISCARDS]:
                    if x["metric"]["session_id"] == sess("o04", 2):
                        x["values"] = [[ts, str(second)] for ts, _ in x["values"]]
                r = score(m, w)
                self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o04"]["A_terms"]["discarded"],
                                       booked * 20 / 15000 / 41)
                self.assertEqual(r["diagnostics"]["counter_lineage_unproven"], unproven)
                self.assertEqual(r["uncertain_gates"], [])

    def baseline_world(self, before, who=("o04",)):
        m, w = self.world()
        for x in w.series[M_DISCARDS]:
            if x["metric"]["from_peer"] in who:
                x["values"] = [[ts, str(before if ts < HS + 10 * STEP else before + 30)] for ts, _ in x["values"]
                               if ts > HS]
        return m, w

    def test_two_kept_changes_continue_from_the_largest_earlier_value(self):
        m, w = self.world()
        self.bump(w, M_DISCARDS, "o04", HS + 5 * STEP, 10)
        w.new_session("o04", HS + 15 * STEP, n=2)
        w.new_session("o04", HS + 25 * STEP, n=3, prev=2)
        for x in w.series[M_DISCARDS]:
            if x["metric"]["session_id"] in (sess("o04", 2), sess("o04", 3)):
                x["values"] = [[ts, "20"] for ts, _ in x["values"]]
        r = score(m, w)
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o04"]["A_terms"]["discarded"], 20 * 20 / 15000 / 41)

    def test_a_counter_without_a_baseline_books_nothing_until_its_next_sample(self):
        m, w = self.baseline_world(100)
        r = score(m, w)
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o04"]["A_terms"]["discarded"], 30 * 20 / 15000 / 41)
        self.assertEqual(r["diagnostics"]["counter_lineage_unproven"], [f"o04 <- t1: {M_DISCARDS} baseline (100)"])
        self.assertEqual((r["verdict"], r["uncertain_gates"]), ("PASS", []))

    def test_a_gate_the_two_readings_disagree_on_is_uncertain_and_blocks_a_pass(self):
        m, w = self.baseline_world(3000, ("o03", "o04"))
        m["steps"][0]["hold_end"] = HS + 12 * STEP
        for x in w.series[M_HEALTH]:
            x["values"] = [v for v in x["values"] if v[0] <= HS + 12 * STEP]
        r = score(m, w)
        self.assertEqual((gate(r, "G-Q3", "quality_gates")["status"], r["uncertain_gates"]), ("uncertain", ["G-Q3"]))
        self.assertEqual((r["verdict"], r["invalid_reasons"]), ("INVALID", ["G-Q3"]))
        self.assertNotIn("Blocking unmeasured gates", report_of(r))

    def test_a_fail_under_both_readings_stands(self):
        m, w = self.world()
        for o in ("o00", "o01", "o02"):
            self.bump(w, M_UNDERRUN, o, HS + 5 * STEP, 20)
            w.new_session(o, HS + 20 * STEP)
            for x in w.series[M_UNDERRUN]:
                if x["metric"]["session_id"] == sess(o, 2):
                    x["values"] = [[ts, str(20 + 15000 * min(3, max(0, (ts - HS) // STEP - 19)))]
                                   for ts, _ in x["values"]]
        r = score(m, w)
        self.assertEqual((gate(r, "G-Q3", "quality_gates")["status"], r["verdict"]), ("fail", "FAIL"))
        self.assertEqual(r["invalid_reasons"], [])

    def test_discard_causes_are_separate_counters_added_after_their_increase(self):
        m, w = self.world()
        for s in list(w.series[M_DISCARDS]):
            s["metric"]["cause"] = "smart_flush"
            w.series[M_DISCARDS].append({"metric": dict(s["metric"], cause="resync_to_live"),
                                         "values": [[ts, "0"] for ts, _ in s["values"]][::2]})
        self.bump(w, M_DISCARDS, "o05", HS + 10 * STEP, 10, "resync_to_live")
        r = score(m, w)
        self.assertEqual(gate(r, "G-Q8", "quality_gates")["status"], "pass")
        self.assertAlmostEqual(r["cells"]["UxU"]["participants"]["o05"]["A_terms"]["discarded"], 10 * 20 / 15000 / 41)

    def test_tiles_that_froze_in_one_bucket_count_and_spread_freezes_do_not(self):
        def freezes(starts):
            m = manifest([part("o1")] + [part(p, "rust", False) for p in ("p1", "p2", "p3", "p4")])
            w = World()
            for u in ("o1", "p1", "p2", "p3", "p4"):
                w.presence(u)
            for p, start in zip(("p1", "p2", "p3"), starts):
                w.pair(M_FPS, "o1", sess(p), 25)
                w.pair(M_FREEZE, "o1", sess(p), lambda t, start=start: 2.0 * (t >= start))
            w.pair(M_FPS, "o1", sess("p4"), 0)
            w.pair(M_FREEZE, "o1", sess("p4"), lambda t: t - HS)
            return score(m, w)["diagnostics"]["correlated_freeze"]["o1"]
        cf = freezes([HS + 10 * STEP] * 3)
        self.assertEqual((cf["buckets_froze_together"], cf["freeze_s_in_those_buckets"]), ([HS + 10 * STEP], 6.0))
        cf = freezes([HS + k * STEP for k in (20, 21, 22)])
        self.assertEqual((cf["buckets_froze_together"], cf["max_share_frozen"]), ([], 1 / 3))

    def test_delay_diagnostics_are_reported_and_change_no_gate(self):
        m, w = self.world()
        base = score(m, w)
        for o in self.OBS:
            w.pair(cq_score.M_BUFFER, o, sess("t1"), lambda t: 800 if t > HS + 20 * STEP else 100)
            w.pair(cq_score.M_TARGET, o, sess("t1"), lambda t: 300 if t > HS + 20 * STEP else 120)
            w.pair(cq_score.M_ACCEL, o, sess("t1"), 4)
        r = score(m, w)
        self.assertEqual(r["diagnostics"]["audio_delay"]["o00"],
                         {"buffer_ms_p95": 800, "target_delay_at_cap_share": 20 / 41, "accelerate_ops_per_sec_mean": 4.0})
        self.assertEqual((r["verdict"], r["validity"], r["quality_gates"]),
                         (base["verdict"], base["validity"], base["quality_gates"]))

    def test_the_report_states_the_resolution_and_the_split(self):
        m, w = self.world()
        self.bump(w, M_DISCARDS, "o01", HS + 10 * STEP, 40)
        r = score(m, w)
        text = cq_report.render_markdown({"run": {"meeting_id": "m", "mode": "run", "bands_version": "b", "commit": "c",
                                                  "scrape_step_s": 15}, "verdict": r["verdict"],
                                          "headline_step": "s1", "steps": [r]})
        self.assertIn("1 s NetEQ windows read once per 15 s scrape, so dimension A sees 1 s in 15", text)
        self.assertIn("a pair missing from a receiver whose per-peer stats are at the cap", text)
        self.assertIn("| o01 | 0.001301 | 0 | 0 | 0.001301 | 0 | 0 |", text)
        self.assertIn("### Video tiles that froze together", text)
        self.assertIn("### Audio jitter-buffer delay", text)

    def test_per_receiver_audio_diagnostics_are_pseudonymised(self):
        human = "someone@example.com"
        m = manifest([part(human), part("t1", "rust", False, True)])
        w = healthy_world([human], ["t1"])
        w.pair(cq_score.M_BUFFER, human, sess("t1"), 100)
        w.series[M_UNDERRUN] = []
        hidden = cq_privacy.pseudonymise(score(m, w), {human}, b"salt")
        self.assertNotIn(human, json.dumps(hidden["diagnostics"]))

    def test_diagnostic_thresholds_are_validated(self):
        for key, bad in (("correlated_freeze_share", 0), ("correlated_freeze_share", 1.5),
                         ("correlated_freeze_min_pairs", 0), ("correlated_freeze_min_pairs", 2.5),
                         ("target_delay_cap_ms", 0), ("target_delay_cap_ms", True)):
            cfg = cq_score.load_config()
            cfg["diagnostics"][key]["value"] = bad
            with self.subTest(key=key, bad=bad), self.assertRaises(cq_score.ConfigError):
                cq_score.validate_config(cfg)

    def test_the_26_oct_hcl_daily_runs(self):
        want = {"20261026T1200Z": (0.3012, 0.1117), "20261026T1212Z": (0.0042, 0.0508),
                "20261026T1217Z": (0.0423, 0.0178), "20261026T1221Z": (0.2778, 0.3622)}
        for run, (tab, probe) in want.items():
            with self.subTest(run=run):
                r = fixture_step(run)
                self.assertAlmostEqual(a_of(r, "human-tab"), tab, places=4)
                self.assertAlmostEqual(a_of(r, "probe"), probe, places=4)
                self.assertEqual({u: c["UxU"]["playout silence"][0]
                                  for u, c in r["diagnostics"]["audio_counter_coverage"].items()},
                                 {"human-tab": 0, "probe": 0})
        small = fixture_step("20261026T1212Z")
        self.assertEqual(gate(small, "G-Q3", "quality_gates")["status"], "pass")
        self.assertEqual(small["cells"]["UxU"]["participants"]["probe"]["A_terms"]["discarded"], 0)
        self.assertEqual(small["diagnostics"]["correlated_freeze"]["human-tab"]["buckets_froze_together"], [1015.0])
        self.assertEqual(small["diagnostics"]["audio_delay"]["probe"]["target_delay_at_cap_share"], 1.0)



class CounterSessionTest(unittest.TestCase):
    def freeze_world(self, pubs=("p1",)):
        m = manifest([part("o1")] + [part(p, "rust", False) for p in pubs])
        w = World()
        for u in ("o1",) + tuple(pubs):
            w.presence(u)
        for p in pubs:
            w.pair(M_FPS, "o1", sess(p), 25)
        return m, w

    def handover(self, w, pubs, at, value, old=None):
        """The receiver moves to a new session at `at`; its decoders report `value(pub)` there."""
        w.presence("o1", sess("o1", 2), at, HE)
        for p in pubs:
            if old:
                for x in w.series[M_FREEZE]:
                    if x["metric"]["to_peer"] == sess(p):
                        x["values"] = [[t, str(old(t))] for t, _ in x["values"] if t <= at]
            w.pair(M_FREEZE, "o1", sess(p), value(p), recv_session=sess("o1", 2), lo=at)

    def test_v_freeze_continues_on_the_lower_reading_and_a_zero_prior_is_proven(self):
        for before, second, booked, unproven in ((4.0, 6.0, 6.0, [f"o1 <- p1: {M_FREEZE} continuation or rebuild (4)"]),
                                                 (4.0, 1.0, 5.0, []), (0.0, 1.0, 1.0, [])):
            with self.subTest(before=before, second=second):
                m, w = self.freeze_world()
                w.pair(M_FREEZE, "o1", sess("p1"), lambda t, b=before: b * (t >= HS + 5 * STEP))
                self.handover(w, ("p1",), HS + 20 * STEP, lambda p, v=second: v)
                r = score(m, w)
                self.assertEqual(r["diagnostics"]["max_freeze_seconds_pair"], booked)
                self.assertEqual(r["diagnostics"]["counter_lineage_unproven"], unproven)

    def test_v_is_uncertain_when_only_the_upper_reading_crosses_red(self):
        m, w = self.freeze_world()
        w.pair(M_FREEZE, "o1", sess("p1"), lambda t: 30.0 * (t >= HS))
        self.handover(w, ("p1",), HS + 20 * STEP, lambda p: 30.0)
        r = score(m, w)
        self.assertEqual(r["uncertain_gates"], ["G-Q4"])

    def test_an_unknown_earlier_value_in_the_chain_is_not_a_proven_rebuild(self):
        m, w = self.freeze_world()
        at = HS + 20 * STEP
        for n, lo, value in ((2, at, 5.0), (3, at + 2 * STEP, 7.0)):
            w.presence("o1", sess("o1", n), lo=at)
            w.pair(M_FREEZE, "o1", sess("p1"), value, recv_session=sess("o1", n), lo=lo)
        self.assertEqual(score(m, w)["diagnostics"]["counter_lineage_unproven"],
                         [f"o1 <- p1: {M_FREEZE} continuation or rebuild (5)"] * 2)

    def test_the_handover_scrape_keeps_the_old_sessions_final_increase(self):
        m, w = self.freeze_world()
        at = HS + 20 * STEP
        w.pair(M_FREEZE, "o1", sess("p1"), lambda t: 6.0 * (t >= at))
        self.handover(w, ("p1",), at, lambda p: 0.0)
        r = score(m, w)
        self.assertEqual(r["diagnostics"]["max_freeze_seconds_pair"], 6.0)
        self.assertEqual(r["diagnostics"]["counter_lineage_unproven"], [])

    def test_a_replaced_sessions_later_increase_is_reported_not_booked(self):
        m, w = self.freeze_world()
        w.pair(M_FREEZE, "o1", sess("p1"), lambda t: 4.0 * (t >= HS + 5 * STEP) + 9.0 * (t >= HS + 25 * STEP))
        w.presence("o1", sess("o1", 2), HS + 20 * STEP, HE)
        w.pair(M_FREEZE, "o1", sess("p1"), 0.0, recv_session=sess("o1", 2), lo=HS + 20 * STEP)
        r = score(m, w)
        self.assertEqual(r["diagnostics"]["max_freeze_seconds_pair"], 4.0)
        self.assertEqual(r["diagnostics"]["counter_lineage_unproven"],
                         [f"o1 <- p1: {M_FREEZE} after a later session started (9)"])

    def publisher_rejoins(self, old_after):
        """p1 rejoins at `at`; o1's decoder for p1's new session reports 3 s, the old one 2 s then `old_after`."""
        m, w = self.freeze_world()
        at = HS + 20 * STEP
        w.series[M_SENT] = [x for x in w.series[M_SENT] if x["metric"]["peer_id"] != "p1"]
        w.presence("p1", hi=at)
        w.presence("p1", sess("p1", 2), at, HE)
        w.pair(M_FREEZE, "o1", sess("p1"), lambda t: 2.0 * (t >= HS + 5 * STEP) + old_after * (t > at + STEP))
        w.pair(M_FREEZE, "o1", sess("p1", 2), 3.0, lo=at + STEP)
        return score(m, w)["diagnostics"]

    def test_a_publishers_new_session_books_its_counter_from_zero(self):
        d = self.publisher_rejoins(0.0)
        self.assertEqual((d["max_freeze_seconds_pair"], d["counter_lineage_unproven"]), (5.0, []))

    def test_a_publishers_old_session_increase_after_its_new_session_is_reported_not_booked(self):
        d = self.publisher_rejoins(4.0)
        self.assertEqual((d["max_freeze_seconds_pair"], d["counter_lineage_unproven"]),
                         (5.0, [f"o1 <- p1: {M_FREEZE} after a later session started (4)"]))

    def test_a_series_first_sampled_at_hold_start_has_a_known_baseline(self):
        m, w = self.freeze_world()
        w.pair(M_FREEZE, "o1", sess("p1"), lambda t: 4.0 + 2.0 * (t >= HS + 5 * STEP))
        d = score(m, w)["diagnostics"]
        self.assertEqual((d["max_freeze_seconds_pair"], d["counter_lineage_unproven"]), (2.0, []))

    def test_a_bucket_at_exactly_the_share_froze_together(self):
        pubs = ("p1", "p2", "p3", "p4", "p5")
        m, w = self.freeze_world(pubs)
        for p in pubs[:4]:
            w.pair(M_FREEZE, "o1", sess(p), lambda t: 1.0 * (t >= HS + 5 * STEP))
        w.pair(M_FREEZE, "o1", sess("p5"), 0.0)
        cf = score(m, w)["diagnostics"]["correlated_freeze"]["o1"]
        self.assertEqual(cf["buckets_froze_together"], [HS + 5 * STEP])

    def test_retained_freeze_totals_make_no_tiles_froze_together_bucket(self):
        pubs = ("p1", "p2", "p3")
        for value, together in ((lambda p: 2.0, []), (lambda p: 0.5, [HS + 20 * STEP])):
            with self.subTest(together=together):
                m, w = self.freeze_world(pubs)
                for p in pubs:
                    w.pair(M_FREEZE, "o1", sess(p), lambda t: 2.0 * (t >= HS + 5 * STEP))
                self.handover(w, pubs, HS + 20 * STEP, value)
                cf = score(m, w)["diagnostics"]["correlated_freeze"]["o1"]
                self.assertEqual(cf["buckets_froze_together"], [HS + 5 * STEP] + together)

    def reelect_world(self, before, first_after, then=None):
        m = manifest([part("o1"), part("t1", "rust", False, True)])
        w = World()
        mid = HS + 20 * STEP
        w.presence("o1", sess("o1", 1), JOIN, mid)
        w.presence("o1", sess("o1", 2), mid + STEP, HE)
        w.presence("t1")
        w.add(M_REELECT, {"session_id": sess("o1", 1), "result": "proceeded"}, before, JOIN, mid)
        w.add(M_REELECT, {"session_id": sess("o1", 2), "result": "proceeded"},
              lambda t: first_after if then is None or t < HE - 10 * STEP else then, mid + STEP)
        return score(m, w)["stability"]["o1"]

    def test_a_page_wide_reelection_count_is_not_booked_again_by_a_new_session(self):
        st = self.reelect_world(1, 1)
        self.assertEqual((st["migrations"], st["unplanned_reconnects"]), (0, 1))
        self.assertGreater(st["S"], 0)

    def test_reelections_after_the_session_change_still_count(self):
        self.assertEqual(self.reelect_world(1, 1, then=2)["migrations"], 1)
        self.assertEqual(self.reelect_world(3, 1)["migrations"], 1)


    def test_a_failed_reelection_in_an_older_concurrent_tab_is_counted(self):
        m, w = Review2963Test().base()
        newer = sess("o04", 2)
        w.presence("o04", newer, lo=HS - 30)
        w.add(M_REELECT, {"session_id": newer, "result": "failed"}, 1, lo=HS - 30)
        w.add(M_REELECT, {"session_id": sess("o04"), "result": "failed"}, 1, lo=HS + 10 * STEP)
        r = score(m, w)
        self.assertEqual((r["stability"]["o04"]["reelection_failed"], r["verdict"]), (1, "FAIL"))

    def tied(self, result, loud=()):
        m, w = Review2963Test().base()
        for x in w.series[M_EXPAND]:
            if x["metric"]["from_peer"] in loud:
                x["values"] = [[ts, "100"] for ts, _ in x["values"]]
        at = HS + 20 * STEP
        for x in w.series[M_SENT]:
            if x["metric"]["session_id"] == sess("o04"):
                x["values"] = [v for v in x["values"] if v[0] <= at]
        for n, hi in ((2, at + STEP), (3, HE)):
            w.presence("o04", sess("o04", n), lo=at, hi=hi)
            w.add(M_REELECT, {"session_id": sess("o04", n), "result": result}, 1, lo=at, hi=hi)
        return score(m, w)

    def test_tied_sessions_leave_reelections_unproven(self):
        r = self.tied("proceeded")
        self.assertEqual(r["stability"]["o04"]["migrations"], 0)
        self.assertEqual(gate(r, "G-Q9", "quality_gates")["status"], "uncertain")
        self.assertNotEqual(r["verdict"], "PASS")
        self.assertEqual(gate(self.tied("failed"), "G-Q2", "quality_gates")["status"], "uncertain")

    def test_a_proven_fail_stands_beside_uncertain_gates(self):
        r = self.tied("proceeded", loud=("o00", "o01"))
        self.assertEqual(gate(r, "G-Q3", "quality_gates")["status"], "fail")
        self.assertEqual((r["verdict"], r["invalid_reasons"]), ("FAIL", []))
        self.assertIn("G-Q9", r["uncertain_gates"])
        self.assertIn("Uncertain gates", report_of(r))

    def test_a_long_overlap_is_concurrent_not_a_replacement(self):
        m, w = Review2963Test().base()
        mid = HS + 20 * STEP
        w.add(M_REELECT, {"session_id": sess("o04"), "result": "proceeded"}, 1, lo=JOIN)
        w.presence("o04", sess("o04", 2), lo=mid)
        w.add(M_REELECT, {"session_id": sess("o04", 2), "result": "proceeded"}, 1, lo=mid)
        self.assertIn("o04: re-election proceeded not provably from this hold (1)",
                      score(m, w)["diagnostics"]["counter_lineage_unproven"])

    def test_a_concurrent_tab_count_above_the_first_value_is_not_its_continuation(self):
        m, w = Review2963Test().base()
        mid = HS + 20 * STEP
        w.add(M_REELECT, {"session_id": sess("o04"), "result": "proceeded"}, 3, lo=JOIN)
        w.presence("o04", sess("o04", 2), lo=mid)
        w.add(M_REELECT, {"session_id": sess("o04", 2), "result": "proceeded"}, 1, lo=mid)
        self.assertEqual(score(m, w)["diagnostics"]["counter_lineage_unproven"], [])

    def test_a_count_present_from_a_lone_sessions_first_report_is_unproven(self):
        m, w = Review2963Test().base()
        mid = HS + 20 * STEP
        w.series[M_SENT] = [x for x in w.series[M_SENT] if x["metric"]["peer_id"] != "o04"]
        w.presence("o04", sess("o04", 2), lo=mid)
        w.add(M_REELECT, {"session_id": sess("o04", 2), "result": "proceeded"}, 1, lo=mid)
        self.assertIn("o04: re-election proceeded not provably from this hold (1)",
                      score(m, w)["diagnostics"]["counter_lineage_unproven"])

    def test_results_do_not_depend_on_series_order(self):
        import random

        def storm():
            m, w = Review2963Test().base()
            at, later = HS + 20 * STEP, HS + 30 * STEP
            w.new_session("o04", at, n=2)
            w.new_session("o04", later, n=3, prev=2)
            w.presence("o04", sess("o04", 1), lo=at, hi=at + 15)
            for n, (lo, hi, count) in {0: (JOIN, at + 15, 1), 1: (at, at + 15, 2), 2: (at, later + 15, 3),
                                       3: (later, HE, 3)}.items():
                w.add(M_REELECT, {"session_id": sess("o04", n), "result": "proceeded"}, count, lo=lo, hi=hi)
            for x in w.series[M_DISCARDS] + w.series[M_FREEZE]:
                if x["metric"]["session_id"] == sess("o04", 3):
                    x["values"] = [[ts, "7"] for ts, _ in x["values"]]
            for x in w.series[M_FREEZE]:
                if x["metric"]["from_peer"] in ("o00", "o01"):
                    x["values"] = [[ts, str(3.0 * (ts >= later))] for ts, _ in x["values"]]
            return m, w
        m, w = storm()
        want = json.dumps(score(m, w), sort_keys=True)
        rng = random.Random(3101)
        for _ in range(6):
            m, w = storm()
            for lst in w.series.values():
                rng.shuffle(lst)
            self.assertEqual(json.dumps(score(m, w), sort_keys=True), want)


FIXTURES = os.path.join(HERE, "testdata", "relay_evidence_20261026")
DAILY_PODS = 'namespace="videocall",pod=~"videocall-websocket-.*|videocall-webtransport-.*|nats-[0-9]+"'
ESC, DROPS, SESSIONS = ("relay_downlink_shed_escalations_total", "relay_session_drops_total",
                        "relay_active_sessions_per_room")


class RelayEvidenceTest(unittest.TestCase):
    def fetched(self, w, cfg=None, users=("o1",), fail=(), recorded=None, exact=False, whole=False):
        m = manifest([dict(part(u), **(recorded or {}).get(u, {})) for u in users] + [part("t1", "rust", False, True)])
        cfg = cfg or cq_score.load_config()
        fake = w.transport([])

        def transport(url, body, headers):
            query = urllib.parse.parse_qs(body.decode())["query"][0]
            if any(f == query if exact else f in query for f in fail):
                raise urllib.error.HTTPError(url, 400, "bad", {}, None)
            return fake(url, body, headers)
        client = cq_prom.PromClient("http://prom.invalid", transport=transport)
        data = cq_score.fetch_step_data(client, m, m["steps"][0], cfg, set(users))
        if whole:
            return cq_score.score_run(m, {"s1": data}, cfg)
        return cq_score.score_step(m, m["steps"][0], data, cfg)

    def world(self, transports=("webtransport",), users=("o1",)):
        w = healthy_world(list(users), ["t1"])
        for t in transports:
            w.add(SESSIONS, {"room": "scale-t1", "transport": t}, 1)
        return w

    def fixture(self, run, whole=False):
        with open(os.path.join(FIXTURES, f"{run}.json"), encoding="utf-8") as fh:
            doc = json.load(fh)
        w = self.world((), doc["users"])
        for name, payload in doc["payloads"].items():
            w.series[name] = payload["data"]["result"]
        for user, sessions in doc["users"].items():
            for sid in sessions:
                w.add(cq_score.M_PEER_INFO, {"peer_id": user, "session_id": sid}, 1)
        recorded = {u: dict(r) for u, r in doc.get("bots_app_records", {}).items()}
        return self.fetched(w, self.cfg(DAILY_PODS), tuple(doc["users"]), recorded=recorded, whole=whole)

    def cfg(self, pods):
        cfg = cq_score.load_config()
        cfg["server_diagnostics"]["throttle_pod_selector"]["value"] = pods
        return cfg

    def test_stage_one_escalations_are_reported_beside_stage_two(self):
        w = self.world()
        w.add(ESC, {"transport": "webtransport", "stage": "one"}, lambda t: 3 * (t > HS + 10 * STEP))
        w.add(ESC, {"transport": "webtransport", "stage": "two"}, 0)
        r = self.fetched(w)
        self.assertEqual(r["server_diagnostics"]["shed_escalations"],
                         {"status": "measured", "by": {"webtransport/one": 3.0, "webtransport/two": 0.0}})
        report = cq_report.render_markdown({"run": {"meeting_id": "m", "mode": "run", "bands_version": "b",
                                                    "commit": "c"}, "verdict": "PASS", "headline_step": "s1",
                                            "steps": [r]})
        self.assertIn("webtransport/one 3, webtransport/two 0", report)
        self.assertEqual(cq_report._fmt(37503.0), "37503")

    def test_an_absent_counter_reads_zero_only_where_the_relay_exports_it_for_this_room(self):
        w = self.world(("webtransport", "websocket"))
        w.add(ESC, {"transport": "webtransport", "stage": "one"}, 0)
        w.add("videocall_outbound_channel_drops_total", {"transport": "websocket", "kind": "video"}, 0)
        sd = self.fetched(w)["server_diagnostics"]
        self.assertEqual(sd["shed_escalations"]["by"], {"webtransport/one": 0.0, "webtransport/two": 0.0})
        self.assertEqual(sd["outbound_channel_drops"]["by"], {"websocket/audio": 0.0, "websocket/video": 0.0})
        self.assertIn("no series on webtransport", sd["outbound_channel_drops"]["detail"])
        self.assertEqual(sd["stream_resets"]["status"], "not collected")
        self.assertIn("never incremented, or not exported", sd["stream_resets"]["detail"])
        sd = self.fetched(self.world(("websocket",)))["server_diagnostics"]
        self.assertEqual(sd["shed_escalations"]["status"], "not collected")
        for status in [self.fetched(self.world(()))["server_diagnostics"][k]["status"]
                       for k in ("shed_escalations", "stream_resets", "session_drops", "quic", "cpu_throttling")]:
            self.assertEqual(status, "not collected")

    def test_a_failed_room_query_makes_its_dependants_read_query_failed(self):
        w = self.world(("webtransport",))
        w.add(ESC, {"transport": "webtransport", "stage": "one"}, 0)
        sd = self.fetched(w, fail=("relay_active_sessions_per_room",))["server_diagnostics"]
        self.assertEqual((sd["room_transports"], sd["stream_resets"]["status"]), ("query failed", "query failed"))
        self.assertEqual(sd["shed_escalations"]["by"], {"webtransport/one": 0.0})
        self.assertIn("relay_active_sessions_per_room query failed", sd["shed_escalations"]["detail"])

    def test_wt_session_closes_carry_only_reason_keys(self):
        w = self.world()
        w.add("relay_wt_session_closes_total", {"transport": "webtransport", "reason": "idle"},
              lambda t: 3 * (t > HS + 10 * STEP))
        r = self.fetched(w)
        self.assertEqual(r["server_diagnostics"]["wt_session_closes"]["by"], {"webtransport/idle": 3.0})
        report = cq_report.render_markdown({"run": {"meeting_id": "m", "mode": "run", "bands_version": "b",
                                                    "commit": "c"}, "verdict": "PASS", "headline_step": "s1",
                                            "steps": [r]})
        self.assertNotIn("webtransport 0", report)

    def test_a_room_session_gauge_stuck_at_zero_does_not_put_its_transport_in_the_room(self):
        w = self.world(("webtransport",))
        w.add(SESSIONS, {"room": "scale-t1", "transport": "websocket"}, 0)
        w.add("videocall_outbound_channel_drops_total", {"transport": "websocket", "kind": "video"}, 0)
        sd = self.fetched(w)["server_diagnostics"]
        self.assertEqual(sd["room_transports"], ["webtransport"])
        self.assertNotIn("websocket/audio", sd["outbound_channel_drops"]["by"])

    def test_a_failed_rtt_query_names_no_transport_and_splits_no_cell(self):
        m = manifest([part("o1"), part("t1", "rust", False, True)])
        w = healthy_world(["o1"], ["t1"])
        w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1"), "server_type": "webtransport"}, 40)

        def transport(url, body, headers):
            if urllib.parse.parse_qs(body.decode())["query"][0].startswith(M_RTT):
                raise urllib.error.HTTPError(url, 400, "bad", {}, None)
            return w.transport([])(url, body, headers)
        cfg = cq_score.load_config()
        data = cq_score.fetch_step_data(cq_prom.PromClient("http://prom.invalid", transport=transport), m,
                                        m["steps"][0], cfg, {"o1"})
        r = cq_score.score_step(m, m["steps"][0], data, cfg, split_transport=True)
        self.assertEqual(r["transport"]["o1"], "query failed")
        self.assertEqual(set(r["cells"]), {"UxU"})
        self.assertEqual(r["cells"]["UxU"]["transport_mix"], "query failed")
        self.assertIn("--split-transport: no per-transport cells, the RTT query failed", r["flags"])

    def test_a_failed_layer_query_keeps_the_decoded_fps(self):
        r = self.fetched(self.world(), fail=(cq_score.M_ACTIVE_LAYERS,))
        self.assertEqual(r["layer_mix"]["publisher_active_video_layers_p50"], "query failed")
        self.assertEqual(r["layer_mix"]["decoded_fps_p50"], self.fetched(self.world())["layer_mix"]["decoded_fps_p50"])
        self.assertIsNotNone(r["layer_mix"]["decoded_fps_p50"])

    def test_every_diagnostic_query_failure_reads_query_failed_and_changes_no_gate(self):
        m = manifest([part("o1"), part("t1", "rust", False, True)])
        cfg = self.cfg('pod=~"relay-.*"')
        queries = {name: q for name, q, _, _ in cq_score.step_queries(m, m["steps"][0], cfg, {"o1"})
                   if name in cq_score.DIAGNOSTIC_SERIES}
        self.assertTrue({cq_score.M_RTT, cq_score.M_ACTIVE_LAYERS, "_room_sessions", "_cfs"} <= set(queries))
        clean = self.fetched(self.world(), cfg)
        for name, query in sorted(queries.items()):
            with self.subTest(name=name):
                r = self.fetched(self.world(), cfg, fail=(query,), exact=True)
                self.assertEqual((r["verdict"], r["validity"], r["quality_gates"]),
                                 (clean["verdict"], clean["validity"], clean["quality_gates"]))
                self.assertIn('"query failed"', json.dumps(r))
                self.assertIn("query failed", report_of(r))

    def test_a_failed_diagnostic_query_reads_query_failed_and_scoring_goes_on(self):
        cfg = self.cfg('pod=~"relay-.*"')
        r = self.fetched(self.world(), cfg,
                         fail=("relay_downlink_shed_escalations_total", "container_cpu_cfs", "path_lost_packets"))
        sd = r["server_diagnostics"]
        self.assertEqual([sd[k]["status"] for k in ("shed_escalations", "cpu_throttling", "quic")],
                         ["query failed"] * 3)
        clean = self.fetched(self.world(), cfg)
        self.assertEqual((r["verdict"], r["quality_gates"]), (clean["verdict"], clean["quality_gates"]))
        with self.assertRaises(cq_prom.PromError):
            self.fetched(self.world(), fail=(M_PPS,))

    def test_audio_drops_are_split_by_kind_transport_and_receiver(self):
        w = self.world(("webtransport", "websocket"), ("o1", "o2"))
        for user, kind, n in (("o1", "audio", 40), ("o1", "priority_drop_video", 7), ("o2", "audio", 2)):
            w.add(DROPS, {"room": "scale-t1", "transport": "webtransport", "session_id": sess(user), "kind": kind},
                  lambda t, n=n: n * (t > HS))
        w.add("videocall_outbound_channel_drops_total", {"transport": "webtransport", "kind": "audio"},
              lambda t: 9 * (t > HS))
        sd = self.fetched(w, users=("o1", "o2"))["server_diagnostics"]
        self.assertEqual(sd["session_drops"]["by"], {"webtransport/audio": 42.0, "webtransport/priority_drop_video": 7.0})
        self.assertIn("no series on websocket", sd["session_drops"]["detail"])
        self.assertEqual(sd["session_drops"]["by_user"], {"o1": {"audio": 40.0, "priority_drop_video": 7.0},
                                                          "o2": {"audio": 2.0}})
        self.assertEqual(sd["outbound_channel_drops"]["by"], {"webtransport/audio": 9.0})

    def test_a_quic_gauge_without_a_series_is_unknown_not_zero(self):
        w = self.world()
        labels = {"room": "scale-t1", "session_id": "s-a"}
        w.add("videocall_relay_connection_path_sent_packets", labels, lambda t: 9000 + 600 * (t > HS))
        w.add("videocall_relay_connection_rtt_ms", labels, 280)
        q = self.fetched(w)["server_diagnostics"]["quic"]
        self.assertEqual((q["lost"], q["loss_fraction"], q["congestion_events"], q["loss_sessions"]),
                         (None, None, None, [0, 1]))
        self.assertEqual(q["by_user"]["?"][0]["sent"], 600.0)

    def test_a_quic_session_first_seen_after_the_hold_start_books_no_unknown_baseline(self):
        w = self.world()
        w.presence("o1", "s-c")
        labels = {"room": "scale-t1", "session_id": "s-c"}
        w.add("videocall_relay_connection_path_lost_packets", labels, lambda t: 500 + 3 * (t > HS + 10 * STEP),
              lo=HS + 5 * STEP)
        w.add("videocall_relay_connection_path_sent_packets", labels, lambda t: 9000 + 100 * (t > HS + 10 * STEP),
              lo=HS + 5 * STEP)
        q = self.fetched(w)["server_diagnostics"]["quic"]
        self.assertEqual((q["lost"], q["sent"], q["loss_fraction"], q["loss_sessions"]), (None, None, None, [0, 1]))

    def test_quic_loss_uses_each_sessions_own_increase_when_a_session_ends_mid_hold(self):
        w = self.world()
        mid = HS + 20 * STEP
        for sid, hi, lost, sent, rtt in (("s-a", mid, 100, 1000, 280), ("s-b", HE, 1, 1000, 40)):
            labels = {"room": "scale-t1", "session_id": sid}
            w.add("videocall_relay_connection_path_lost_packets", labels, lambda t, k=lost: 500 + k * (t > HS), hi=hi)
            w.add("videocall_relay_connection_path_sent_packets", labels, lambda t, k=sent: 9000 + k * (t > HS), hi=hi)
            w.add("videocall_relay_connection_rtt_ms", labels, rtt, hi=hi)
        w.add(cq_score.M_PEER_INFO, {"peer_id": "o1", "session_id": "s-a"}, 1)
        q = self.fetched(w)["server_diagnostics"]["quic"]
        self.assertEqual((q["lost"], q["sent"]), (101.0, 2000.0))
        self.assertAlmostEqual(q["loss_fraction"], 101 / 2000)
        self.assertEqual(q["by_user"]["o1"], [dict(q["by_user"]["o1"][0], session_id="s-a", loss_fraction=0.1)])
        self.assertEqual((q["rtt_ms_median"], q["by_user"]["?"][0]["rtt_ms_median"]), (160.0, 40.0))

    def quic(self, sessions, born=()):
        w = self.world()
        for sid in born:
            w.presence("o1", sid, lo=HS + STEP)
        for sid, (lost, sent, lo) in sessions.items():
            labels = {"room": "scale-t1", "session_id": sid}
            for name, value in (("lost_packets", lost), ("sent_packets", sent)):
                if value is not None:
                    w.add(f"videocall_relay_connection_path_{name}", labels, value, lo=lo)
        return self.fetched(w)["server_diagnostics"]["quic"]

    def test_quic_sessions_with_unknown_or_zero_counts(self):
        q = self.quic({"s-a": (lambda t: 7 * (t > HS), None, HS), "s-b": (lambda t: 3 * (t > HS), lambda t: 200 * (t > HS), HS)})
        self.assertEqual((q["lost"], q["sent"], q["loss_sessions"]), (3.0, 200.0, [1, 2]))
        q = self.quic({"s-a": (lambda t: 20 * (t > HS), 0, HS), "s-b": (0, lambda t: 100 * (t > HS), HS)})
        self.assertAlmostEqual(q["loss_fraction"], 20 / 100)
        q = self.quic({"s-c": (lambda t: 5 + 2 * (t > HS + 5 * STEP), lambda t: 100 + 50 * (t > HS + 5 * STEP),
                               HS + STEP)}, born=("s-c",))
        self.assertEqual((q["lost"], q["sent"]), (7.0, 150.0))

    def test_cpu_throttling_is_not_collected_without_a_selector_and_scoped_by_it(self):
        w = self.world()
        w.add("_cfs", {"pod": "relay-wt-1"}, 0.0)
        w.add("_cfs", {"pod": "grafana-1"}, 0.4)
        seen = []
        fake = w.transport(seen)
        m = manifest([part("o1"), part("t1", "rust", False, True)])
        cfg = cq_score.load_config()
        cq_score.fetch_step_data(cq_prom.PromClient("http://p.invalid", transport=fake), m, m["steps"][0], cfg, {"o1"})
        self.assertFalse([q for _, q, _ in seen if "cfs" in q])
        self.assertEqual(self.fetched(w)["server_diagnostics"]["cpu_throttling"],
                         {"status": "not collected", "detail": "no relay and NATS pod selector for this target"})
        cpu = self.fetched(w, self.cfg('namespace="vc",pod=~"relay-.*"'))["server_diagnostics"]["cpu_throttling"]
        self.assertEqual((cpu["status"], list(cpu["by_pod"]), cpu["max"]), ("measured", ["relay-wt-1"], 0.0))
        cq_score.fetch_step_data(cq_prom.PromClient("http://p.invalid", transport=fake), m, m["steps"][0],
                                 self.cfg('namespace="vc",pod=~"relay-.*"'), {"o1"})
        self.assertIn('rate(container_cpu_cfs_throttled_periods_total{namespace="vc",pod=~"relay-.*"}[1m])',
                      next(q for _, q, _ in seen if "cfs" in q))

    def test_the_throttle_selector_must_be_matchers_or_null(self):
        for bad in ("", "  ", 'up{job="x"}', 3):
            with self.subTest(bad=bad), self.assertRaises(cq_score.ConfigError):
                cq_score.validate_config(self.cfg(bad))

    def test_a_counter_series_with_no_finite_sample_is_not_a_measured_zero(self):
        w = self.world()
        w.add(ESC, {"transport": "webtransport", "stage": "one"}, "NaN")
        self.assertEqual(self.fetched(w)["server_diagnostics"]["shed_escalations"]["status"], "not collected")

    def test_no_session_id_survives_pseudonymisation(self):
        m = manifest([part("o1"), part("t1", "rust", False, True)])
        w = self.world()
        sid = sess("o1")
        w.add(DROPS, {"room": "scale-t1", "transport": "webtransport", "session_id": sid, "kind": "audio"}, 1)
        w.add("videocall_relay_connection_rtt_ms", {"room": "scale-t1", "session_id": sid}, 280)
        w.add(M_RTT, {"peer_id": "o1", "session_id": sid, "server_type": "webtransport"}, 40)
        w.add(cq_score.M_PEER_INFO, {"peer_id": "o1", "session_id": "999123"}, 1)
        cfg = cq_score.load_config()
        data = cq_score.fetch_step_data(cq_prom.PromClient("http://p.invalid", transport=w.transport([])), m,
                                        m["steps"][0], cfg, {"o1"})
        r = cq_score.score_step(m, m["steps"][0], data, cfg)
        self.assertIn(sid, json.dumps(r))
        out = cq_privacy.pseudonymise(r, cq_privacy.identities_to_hide(m, {"s1": data}, False), b"salt")
        report = cq_report.render_markdown({"run": {"meeting_id": "m", "mode": "run", "bands_version": "b",
                                                    "commit": "c"}, "verdict": "PASS", "headline_step": "s1",
                                            "steps": [out]})
        for raw in (sid, sess("t1"), "999123"):
            self.assertNotIn(raw, json.dumps(out) + report)

    def test_per_user_relay_evidence_is_pseudonymised(self):
        human = "someone@example.com"
        w = self.world(users=(human,))
        w.add(DROPS, {"room": "scale-t1", "transport": "webtransport", "session_id": sess(human), "kind": "audio"}, 1)
        sd = self.fetched(w, users=(human,))["server_diagnostics"]
        self.assertIn(human, sd["session_drops"]["by_user"])
        hidden = cq_privacy.pseudonymise({"server_diagnostics": sd}, {human}, b"salt")
        self.assertNotIn(human, json.dumps(hidden))

    def test_the_26_oct_hcl_daily_runs(self):
        want = {"20261026T1200Z": (7.0, 171.0, 2411.0, 728 / 37503, 0.0, 270.0),
                "20261026T1212Z": (0.0, 0.0, 0.0, 43 / 27127, 0.0, 13.0),
                "20261026T1217Z": (0.0, 0.0, 0.0, 11 / 21906, 0.0034, 8.0),
                "20261026T1221Z": (4.0, 95.0, 911.0, 865 / 31256, 0.0, 377.0)}
        for run, (stage_one, resets, audio_drops, loss, cfs, congestion) in want.items():
            with self.subTest(run=run):
                sd = self.fixture(run)["server_diagnostics"]
                self.assertEqual(sd["shed_escalations"]["by"]["webtransport/one"], stage_one)
                self.assertEqual(sd["shed_escalations"]["by"]["webtransport/two"], 0.0)
                self.assertEqual(sd["stream_resets"]["by"]["webtransport/write_timeout"], resets)
                if audio_drops:
                    self.assertEqual(sd["session_drops"]["by"]["webtransport/audio"], audio_drops)
                else:
                    self.assertEqual(sd["session_drops"]["status"], "not collected")
                self.assertAlmostEqual(sd["quic"]["loss_fraction"], loss)
                self.assertEqual(sd["quic"]["congestion_events"], congestion)
                self.assertAlmostEqual(sd["cpu_throttling"]["max"], cfs, places=4)
                self.assertEqual(sorted(sd["cpu_throttling"]["by_pod"])[0], "nats-0")



class TransportTimelineTest(unittest.TestCase):
    def test_a_switch_mid_run_is_recorded_not_collapsed_to_the_majority(self):
        human = "someone@example.com"
        m = manifest([part(human), part("t1", "rust", False, True)])
        w = healthy_world([human], ["t1"])
        mid = HS + 30 * STEP
        w.add(M_RTT, {"peer_id": human, "session_id": sess(human), "server_type": "webtransport"}, 40, JOIN, mid)
        w.add(M_RTT, {"peer_id": human, "session_id": sess(human, 2), "server_type": "websocket"}, 40, mid + STEP, HE)
        w.add(cq_score.M_PEER_INFO, {"peer_id": human, "session_id": sess(human, 2)}, 1)
        r = score(m, w)
        tl = r["transport_timeline"][human]
        self.assertEqual([(x["server_type"], x["from"], x["to"]) for x in tl["sessions"]],
                         [("webtransport", JOIN, mid), ("websocket", mid + STEP, HE)])
        self.assertEqual((tl["switches"], tl["switches_in_hold"]), (1, 1))
        self.assertEqual(r["transport"][human], "webtransport")
        self.assertIn(f"{human}: transport switched webtransport -> websocket (switches 1, in the hold 1)", r["flags"])
        report = cq_report.render_markdown({"run": {"meeting_id": "m", "mode": "run", "bands_version": "b",
                                                    "commit": "c"}, "verdict": "PASS", "headline_step": "s1",
                                            "steps": [r]})
        self.assertIn("webtransport → websocket (switches 1, in the hold 1)", report)
        hidden = cq_privacy.pseudonymise(r, {human}, b"salt")
        self.assertNotIn(human, json.dumps(hidden["transport_timeline"]))

    def test_the_majority_transport_counts_the_hold_only(self):
        m = manifest([part("o1"), part("t1", "rust", False, True)])
        w = healthy_world(["o1"], ["t1"])
        w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1", 1), "server_type": "webtransport"}, 40,
              HS - 60 * STEP, HS - STEP)
        w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1"), "server_type": "websocket"}, 40)
        w.add(cq_score.M_PEER_INFO, {"peer_id": "o1", "session_id": sess("o1", 1)}, 1)
        r = score(m, w)
        self.assertEqual(r["transport"]["o1"], "websocket")
        self.assertEqual((r["transport_timeline"]["o1"]["switches"], r["transport_timeline"]["o1"]["switches_in_hold"]),
                         (1, 0))

    def test_transport_is_fetched_from_the_join_so_a_pre_hold_switch_is_seen(self):
        m = cq_manifest.load_manifest(EXAMPLE)
        step = m["steps"][0]
        start = next(s for name, _, s, _ in cq_score.step_queries(m, step, cq_score.load_config(), None) if name == M_RTT)
        self.assertLessEqual(start, step["join_start"])

    def test_a_fallback_the_health_reports_miss_is_reconciled_from_the_bots_app_record(self):
        r = RelayEvidenceTest().fixture("20261026T1200Z", whole=True)
        self.assertEqual(r["steps"][0]["transport_timeline"]["probe"]["switches"], 0)
        self.assertEqual(r["transport_fallbacks"]["probe"], {"preferred": "webtransport", "observed": "websocket",
                                                             "fallbacks": 1, "fallbacks_in_health_reports": 0})
        self.assertIn("transport: 1 participant(s) whose bots-app record counts fallbacks from the preferred transport "
                      "that their health reports in the scored windows do not show", r["notices"])

    def two_steps(self, preferred, fallbacks, path, user="o1", fail_first=False):
        mid = HS + 20 * STEP
        p = dict(part(user), transport_preferred=preferred, transport_fallbacks=fallbacks,
                 transport_observed=path[-1] if path[-1] != "unknown" else "websocket")
        m = manifest([p, part("t1", "rust", False, True)])
        for x in m["participants"]:
            x["steps"] = ["s1", "s2"]
        m["steps"] = [dict(m["steps"][0], hold_end=mid, headline=True),
                      dict(m["steps"][0], step_id="s2", join_start=mid, hold_start=mid + STEP, headline=False)]
        w = healthy_world([user], ["t1"])
        for i, st in enumerate(path):
            lo = JOIN if not i else mid + 5 * STEP * i
            hi = HE if i == len(path) - 1 else mid + 5 * STEP * (i + 1) - STEP
            w.add(M_RTT, {"peer_id": user, "session_id": sess(user, i), "server_type": st}, 40, lo, hi)
            w.add(cq_score.M_PEER_INFO, {"peer_id": user, "session_id": sess(user, i)}, 1, lo, hi)
        data = w.data()
        first = dict(data, **{M_RTT: cq_score.FailedQuery()}) if fail_first else data
        return cq_score.score_run(m, {"s1": first, "s2": data}, cq_score.load_config())

    def test_fallbacks_are_reconciled_over_the_run_in_the_preferred_direction(self):
        r = self.two_steps("webtransport", 1, ["webtransport", "websocket"])
        self.assertEqual(r["steps"][0]["transport_timeline"]["o1"]["switches"], 0)
        self.assertEqual(r["transport_fallbacks"]["o1"]["fallbacks_in_health_reports"], 1)
        self.assertFalse([n for n in r["notices"] if n.startswith("transport:")])
        r = self.two_steps("webtransport", 1, ["websocket", "webtransport"])
        self.assertEqual(r["transport_fallbacks"]["o1"]["fallbacks_in_health_reports"], 0)
        self.assertTrue([n for n in r["notices"] if n.startswith("transport:")])
        r = self.two_steps("websocket", 1, ["websocket", "webtransport"])
        self.assertEqual(r["transport_fallbacks"]["o1"]["fallbacks_in_health_reports"], 1)
        r = self.two_steps("webtransport", 1, ["webtransport", "unknown"])
        self.assertEqual(r["transport_fallbacks"]["o1"]["fallbacks_in_health_reports"], 0)

    def sessions_run(self, spans, preferred="webtransport", fallbacks=1):
        p = dict(part("o1"), transport_preferred=preferred, transport_observed=spans[-1][2], transport_fallbacks=fallbacks)
        m = manifest([p, part("t1", "rust", False, True)])
        w = healthy_world(["o1"], ["t1"])
        for n, (lo, hi, st) in enumerate(spans, 1):
            w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1", n), "server_type": st}, 40, lo, hi)
            w.add(cq_score.M_PEER_INFO, {"peer_id": "o1", "session_id": sess("o1", n)}, 1)
        r = cq_score.score_run(m, {"s1": w.data()}, cq_score.load_config())
        tl = r["steps"][0]["transport_timeline"]["o1"]
        return tl["switches"], tl["switches_unproven"], r["transport_fallbacks"]["o1"]["fallbacks_in_health_reports"]

    def test_a_successor_of_tied_sessions_is_unproven_whatever_the_transport_names(self):
        for a, b in (("webtransport", "websocket"), ("websocket", "webtransport")):
            with self.subTest(successor=a):
                self.assertEqual(self.sessions_run([(JOIN, JOIN + 15, a), (JOIN, JOIN + 15, b), (JOIN + 30, HE, a)]),
                                 (0, 3, None))

    def test_a_session_beside_a_long_lived_tab_is_not_a_switch(self):
        self.assertEqual(self.sessions_run([(JOIN, HE, "webtransport"), (JOIN + 15, JOIN + 30, "webtransport"),
                                            (HS, HE, "websocket")]), (0, 1, None))

    def test_later_reconnects_after_a_tie_add_nothing_and_the_path_names_no_order(self):
        spans = [(JOIN, JOIN + 15, "webtransport"), (JOIN, JOIN + 15, "websocket")]
        spans += [(JOIN + 30 * k, JOIN + 30 * k + 15, "webtransport") for k in range(1, 7)]
        self.assertEqual(self.sessions_run(spans)[:2], (0, 3))
        self.assertEqual(self.sessions_run([(JOIN, JOIN + 15, "websocket"), (JOIN, JOIN + 15, "webtransport"),
                                            (JOIN + 30, JOIN + 75, "webtransport"), (JOIN + 90, HE, "websocket")])[:2],
                         (0, 4))
        for a, b in (("webtransport", "websocket"), ("websocket", "webtransport")):
            p = dict(part("o1"), transport_preferred="webtransport", transport_observed=a, transport_fallbacks=0)
            m = manifest([p, part("t1", "rust", False, True)])
            w = healthy_world(["o1"], ["t1"])
            for n, (lo, hi, st) in enumerate([(JOIN, JOIN + 15, a), (JOIN, JOIN + 15, b), (JOIN + 30, HE, a)], 1):
                w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1", n), "server_type": st}, 40, lo, hi)
                w.add(cq_score.M_PEER_INFO, {"peer_id": "o1", "session_id": sess("o1", n)}, 1)
            tl = score(m, w)["transport_timeline"]["o1"]
            self.assertEqual(tl["path"], f"websocket+webtransport -> {a}")

    def test_every_session_beside_a_live_session_on_another_transport_is_unproven(self):
        self.assertEqual(self.sessions_run([(JOIN, HE, "websocket"), (JOIN, JOIN + 15, "webtransport"),
                                            (JOIN + 30, JOIN + 45, "webtransport"), (JOIN + 60, HE, "webtransport")]),
                         (0, 4, None))

    def test_a_record_without_rtt_history_or_preference_leaves_fallbacks_unknown(self):
        for preferred, rtt in (("webtransport", False), (None, True)):
            with self.subTest(preferred=preferred):
                p = dict(part("o1"), transport_preferred=preferred, transport_observed="websocket",
                         transport_fallbacks=1)
                w = healthy_world(["o1"], ["t1"])
                if rtt:
                    for n, (lo, hi, st) in enumerate([(JOIN, JOIN + 15, "webtransport"), (JOIN + 30, HE, "websocket")], 1):
                        w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1", n), "server_type": st}, 40, lo, hi)
                        w.add(cq_score.M_PEER_INFO, {"peer_id": "o1", "session_id": sess("o1", n)}, 1)
                r = cq_score.score_run(manifest([p, part("t1", "rust", False, True)]), {"s1": w.data()},
                                       cq_score.load_config())
                self.assertIsNone(r["transport_fallbacks"]["o1"]["fallbacks_in_health_reports"])
                self.assertFalse([n for n in r["notices"] if n.startswith("transport:")])

    def test_a_chain_of_four_sessions_counts_each_switch(self):
        self.assertEqual(self.sessions_run([(JOIN, JOIN + 15, "webtransport"), (JOIN + 30, JOIN + 45, "websocket"),
                                            (JOIN + 60, JOIN + 75, "webtransport"), (JOIN + 90, HE, "websocket")]),
                         (3, 0, 2))

    def test_a_failed_rtt_query_in_one_step_leaves_the_run_unknown(self):
        r = self.two_steps("webtransport", 1, ["webtransport"], fail_first=True)
        self.assertEqual(r["steps"][1]["transport_timeline"]["o1"]["status"], "measured")
        self.assertIsNone(r["transport_fallbacks"]["o1"]["fallbacks_in_health_reports"])
        self.assertFalse([n for n in r["notices"] if n.startswith("transport:")])

    def test_a_failed_rtt_query_leaves_fallbacks_unknown_and_quiet(self):
        recorded = {"o1": {"transport_preferred": "webtransport", "transport_observed": "websocket",
                           "transport_fallbacks": 1}}
        r = RelayEvidenceTest().fetched(healthy_world(["o1"], ["t1"]), recorded=recorded, whole=True,
                                        fail=(f'{M_RTT}{{meeting_id="scale-t1"}}',), exact=True)
        self.assertIsNone(r["transport_fallbacks"]["o1"]["fallbacks_in_health_reports"])
        self.assertFalse([n for n in r["notices"] if n.startswith("transport:")])

    def test_a_quick_reconnect_within_the_linger_is_a_switch(self):
        self.assertEqual(self.sessions_run([(JOIN, HS + 15 * STEP, "webtransport"),
                                            (HS + 13 * STEP, HS + 25 * STEP, "websocket"),
                                            (HS + 24 * STEP, HE, "websocket")]), (1, 0, 1))

    def test_two_concurrent_tabs_are_not_a_switch_and_leave_fallbacks_unknown(self):
        p = dict(part("o1"), transport_preferred="webtransport", transport_observed="websocket", transport_fallbacks=1)
        m = manifest([p, part("t1", "rust", False, True)])
        w = healthy_world(["o1"], ["t1"])
        for n, st in ((1, "webtransport"), (2, "websocket")):
            w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1", n), "server_type": st}, 40, JOIN + 15 * n)
            w.add(cq_score.M_PEER_INFO, {"peer_id": "o1", "session_id": sess("o1", n)}, 1)
        r = cq_score.score_run(m, {"s1": w.data()}, cq_score.load_config())
        tl = r["steps"][0]["transport_timeline"]["o1"]
        self.assertEqual((tl["switches"], tl["switches_unproven"]), (0, 1))
        self.assertIsNone(r["transport_fallbacks"]["o1"]["fallbacks_in_health_reports"])
        self.assertIn("o1: 1 session(s) started alongside a session on another transport; their transport changes "
                      "are not counted as switches", r["steps"][0]["flags"])
        self.assertIn("(1 started alongside another transport)", cq_report.render_markdown(r))
        self.assertFalse([n for n in r["notices"] if n.startswith("transport:")])

    def test_the_run_level_reconciliation_is_pseudonymised(self):
        human = "someone@example.com"
        r = self.two_steps("webtransport", 1, ["webtransport", "websocket"], human)
        self.assertIn(human, r["transport_fallbacks"])
        self.assertNotIn(human, json.dumps(cq_privacy.pseudonymise(r, {human}, b"salt")["transport_fallbacks"]))

    def test_a_non_observer_browser_gets_its_timeline_and_pre_hold_rtt_picks_no_transport(self):
        m = manifest([part("o1"), part("b1", observer=False, talker=True)])
        w = healthy_world(["o1"], ["b1"])
        w.add(M_RTT, {"peer_id": "b1", "session_id": sess("b1"), "server_type": "websocket"}, 40)
        w.add(M_RTT, {"peer_id": "o1", "session_id": sess("o1"), "server_type": "webtransport"}, 40, JOIN, HS - STEP)
        client = cq_prom.PromClient("http://prom.invalid", transport=w.transport([]))
        r = cq_score.score_step(m, m["steps"][0], cq_score.fetch_step_data(client, m, m["steps"][0],
                                                                           cq_score.load_config(), {"o1"}),
                                cq_score.load_config())
        self.assertEqual([x["server_type"] for x in r["transport_timeline"]["b1"]["sessions"]], ["websocket"])
        self.assertEqual(r["transport"]["o1"], "unknown")

    def test_the_record_is_shown_without_any_rtt_history(self):
        text = cq_report._transport_text("unknown", {"status": "measured", "sessions": [], "switches": 0,
                                                     "switches_in_hold": 0,
                                                     "recorded": {"preferred": "webtransport", "observed": "websocket",
                                                                  "fallbacks": 1}})
        self.assertEqual(text, "unknown; bots-app record: preferred webtransport, last websocket, 1 fallbacks")

    def test_the_26_oct_tab_switches(self):
        want = {"20261026T1212Z": (["websocket", "webtransport", "webtransport", "webtransport"], 0, 0, 3),
                "20261026T1217Z": (["webtransport", "websocket"], 1, 0, 0),
                "20261026T1221Z": (["websocket", "webtransport"], 1, 0, 0)}
        for run, (path, switches, in_hold, unproven) in want.items():
            with self.subTest(run=run):
                tl = RelayEvidenceTest().fixture(run)["transport_timeline"]["human-tab"]
                self.assertEqual([x["server_type"] for x in tl["sessions"]], path)
                self.assertEqual((tl["switches"], tl["switches_in_hold"], tl["switches_unproven"]),
                                 (switches, in_hold, unproven))


if __name__ == "__main__":
    unittest.main(verbosity=1)
