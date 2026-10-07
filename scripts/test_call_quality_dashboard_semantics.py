#!/usr/bin/env python3
"""The Call Quality dashboard's queries agree with the scorer on synthetic series (#2985).

One synthetic meeting is fed both to cq_score.score_step and, through `promtool test rules`, to
the dashboard's own table, p95 and over-time queries. V and S must match the scorer exactly. A and
Q must match the scorer run on a hold that starts one scrape step later, because a PromQL subquery
excludes its start point and the scorer's grid includes hold_start. An over-time value at t with
window w must match the scorer on the hold (t - w, t].

Env: REQUIRE_PROMTOOL=1 makes a missing container runtime a failure instead of a skip.
"""
from __future__ import annotations

import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE / "quality"))

import call_quality_dashboard_parity as parity  # noqa: E402
import check_call_quality_dashboard as check  # noqa: E402
import cq_prom  # noqa: E402
import cq_score  # noqa: E402
from check_prometheus_rules_parse import runtime  # noqa: E402

IMAGE = "prom/prometheus:v3.11.2"
STEP = 15
HS, HE = 300, 600
WINDOWS = [(w, at) for w in (30, 60) for at in (345, 420, 600)]
OBSERVERS = ("r1", "r2", "r3", "r4", "r5", "r6")
HOUR = 3600
REPLACED_BEFORE_HOLD = ("r4", "r5")
MEETING = "m"
SAMPLE = re.compile(r"\{([^}]*)\}\s+([-+0-9.eE]+|NaN|[+-]Inf)")
LABEL = re.compile(r'(\w+)="((?:[^"\\]|\\.)*)"')

# user -> (observer, talker, camera, join_ts, [(session_id, first_t, last_t)])
PEOPLE = {
    "r1": (True, False, False, 0, [("1", 0, HE)]),
    "r2": (True, False, False, 0, [("2", 0, HE)]),
    "r3": (True, False, False, 0, [("81", 0, 420), ("82", 405, HE)]),
    "r4": (True, False, False, 0, [("91", 0, 315), ("92", 285, HE)]),
    "r5": (True, False, False, 0, [("96", 0, 315), ("95", 285, HE)]),
    "r6": (True, False, False, 0, [("10", 0, HE)]),
    "t1": (False, True, True, 0, [("3", 0, HE)]),
    "v1": (False, False, False, 0, [("41", 0, 390), ("42", 405, HE)]),
    "v2": (False, False, False, 0, [("51", 0, 315), ("52", 285, HE)]),
    "j1": (False, False, False, 450, [("6", 450, HE)]),
    "m1": (False, False, False, 0, [("71", 0, 420), ("72", 405, HE)]),
}


def times(first=0, last=HE):
    return range(first, last + 1, STEP)


def constant(value, first=0, last=HE):
    return {t: value for t in times(first, last)}


def with_points(base, points):
    out = dict(base)
    out.update(points)
    return out


def freeze_with_reset():
    out = {t: 10.0 for t in times(0, HS)}
    for t in times(HS + STEP, HE):
        out[t] = 12.0 if t == 315 else 3.0 if t < 450 else 18.0
    return out


def series():
    """[(metric, labels, {t: value})]; a series that ends before HE goes stale after its last sample."""
    out = []
    for user, (_, _, _, _, sessions) in PEOPLE.items():
        for sid, first, last in sessions:
            labels = {"meeting_id": MEETING, "session_id": sid, "peer_id": user}
            out.append((cq_score.M_SENT, labels, constant(50.0, first, last)))
            out.append((cq_score.M_PEER_INFO, labels, constant(1.0, first, last)))
    out.append((cq_score.M_REELECT, {"meeting_id": MEETING, "session_id": "71", "result": "proceeded"},
                constant(1.0, 405, 420)))
    talker = PEOPLE["t1"][4][0][0]
    stale_from = 390
    pairs = [
        ("1", "r1", {cq_score.M_PPS: with_points(constant(50.0), {360: 40.0}),
                     cq_score.M_EXPAND: with_points(constant(0.0), {420: 30.0}),
                     cq_score.M_CAN_LISTEN: with_points(constant(1.0), {480: 0.0}),
                     cq_score.M_FREEZE: freeze_with_reset(),
                     cq_score.M_FPS: with_points(constant(30.0), {330: 0.0, 390: 0.0, 405: 3.0, 495: 2.0})}),
        ("2", "r2", {cq_score.M_PPS: constant(50.0),
                     cq_score.M_EXPAND: with_points(constant(0.0), {540: 100.0}),
                     cq_score.M_CAN_LISTEN: constant(1.0),
                     cq_score.M_FREEZE: {t: (0.0 if t < 525 else 6.0) for t in times()},
                     cq_score.M_FPS: constant(30.0)}),
        ("81", "r3", {cq_score.M_PPS: {t: (50.0 if t < 375 else 0.0) for t in times(0, 420)},
                      cq_score.M_EXPAND: {t: (0.0 if t < 375 else 100.0) for t in times(0, 420)},
                      cq_score.M_CAN_LISTEN: constant(1.0, 0, 420),
                      cq_score.M_FREEZE: {t: (0.0 if t < 375 else 1.0 if t < stale_from else 3.0)
                                          for t in times(0, 420)},
                      cq_score.M_FPS: {t: (30.0 if t < 360 else 3.0) for t in times(0, 420)}}),
        ("82", "r3", {cq_score.M_PPS: constant(50.0, 405),
                      cq_score.M_EXPAND: with_points(constant(0.0, 405), {510: 20.0}),
                      cq_score.M_CAN_LISTEN: constant(1.0, 405),
                      cq_score.M_FREEZE: {t: (2.0 if t < 510 else 6.0) for t in times(405)},
                      cq_score.M_FPS: with_points(constant(30.0, 405), {405: 0.0, 525: 4.0})}),
        ("10", "r6", {cq_score.M_PPS: constant(50.0),
                      cq_score.M_EXPAND: constant(0.0),
                      cq_score.M_CAN_LISTEN: constant(1.0),
                      cq_score.M_FREEZE: {t: (0.0 if t < 465 else 9.0) for t in times()},
                      cq_score.M_FPS: with_points(constant(30.0), {405: 3.0, 510: 3.0})}),
    ]
    for recv in REPLACED_BEFORE_HOLD:
        (old, _, _), (new, born, _) = PEOPLE[recv][4]
        pairs += [
            (old, recv, {cq_score.M_PPS: constant(50.0, 0, 315),
                         cq_score.M_EXPAND: with_points(constant(0.0, 0, 315), {300: 100.0, 315: 100.0}),
                         cq_score.M_CAN_LISTEN: constant(1.0, 0, 315),
                         cq_score.M_FREEZE: with_points(constant(0.0, 0, 315), {300: 9.0, 315: 9.0}),
                         cq_score.M_FPS: with_points(constant(30.0, 0, 315), {300: 2.0, 315: 2.0})}),
            (new, recv, {cq_score.M_PPS: constant(50.0, born),
                         cq_score.M_EXPAND: constant(0.0, born),
                         cq_score.M_CAN_LISTEN: constant(1.0, born),
                         cq_score.M_FREEZE: with_points(constant(1.0, born), {555: 4.0}),
                         cq_score.M_FPS: with_points(constant(30.0, born), {555: 0.0})})]
    for sid, recv, metrics in pairs:
        for metric, values in metrics.items():
            out.append((metric, {"meeting_id": MEETING, "session_id": sid, "from_peer": recv, "to_peer": talker},
                        values))
    return out


def manifest(hold_start=HS, hold_end=HE):
    parts = [{"user_id": u, "fleet": "browser" if obs else "rust", "role": "probe" if obs else "bot",
              "observer": obs, "talker": talk, "publishes": {"camera": cam, "mic": talk, "screen": False},
              "network": {"profile": "none", "shaped": False, "direction": "none", "shaper": "none"},
              "transport_intended": "auto", "join_ts": float(join or STEP), "leave_ts": None, "steps": ["s"]}
             for u, (obs, talk, cam, join, _) in PEOPLE.items()]
    return {"meeting_id": MEETING, "clock": {"sync": "none"}, "events": [], "participants": parts,
            "steps": [{"step_id": "s", "n_target": len(parts), "join_start": 0.0, "hold_start": float(hold_start),
                       "hold_end": float(hold_end), "headline": True}]}


def scorer_data(raw, hold_start, hold_end=HE):
    """What fetch_step_data returns for these series: query_range samples on the scorer's own grid."""
    wide = {cq_score.M_SENT, cq_score.M_PEER_INFO, cq_score.M_REELECT}
    data = {}
    for metric, labels, values in raw:
        lo = 0 if metric in wide else hold_start
        samples = [(float(t), v) for t, v in sorted(values.items()) if lo <= t <= hold_end]
        data.setdefault(metric, []).append((dict(labels), samples))
        if metric == cq_score.M_PPS:
            mins = [(float(t), min(values[s] for s in (t - STEP, t) if s in values))
                    for t, _ in sorted(values.items()) if hold_start <= t <= hold_end]
            data.setdefault("_pps_min", []).append((dict(labels), mins))
    return data


def score(hold_start, hold_end=HE):
    cfg = cq_score.load_config()
    m = manifest(hold_start, hold_end)
    return cq_score.score_step(m, m["steps"][0], scorer_data(series(), hold_start, hold_end), cfg)


def promtool_values(values):
    out, ended = [], False
    for t in times():
        if t in values:
            out.append(repr(values[t]))
        elif values and t > max(values) and not ended:
            out.append("stale")
            ended = True
        else:
            out.append("_")
    return " ".join(out)


def labels_text(name, labels):
    return name + "{" + ",".join(f'{k}="{v}"' for k, v in sorted(labels.items())) + "}"


def run_promtool(exe, exprs):
    """{case: {participant, from_peer or None: value}} for each {case: (expression, eval time)}."""
    test = {"rule_files": [], "evaluation_interval": f"{STEP}s", "tests": [{
        "interval": f"{STEP}s",
        "input_series": [{"series": labels_text(m, lb), "values": promtool_values(v)} for m, lb, v in series()],
        "promql_expr_test": [{"expr": f'label_replace({e}, "cq_case", "{case}", "", "")',
                              "eval_time": f"{at}s", "exp_samples": []} for case, (e, at) in exprs.items()],
    }]}
    work = Path(tempfile.mkdtemp(prefix="cq-sem-"))
    try:
        (work / "t.json").write_text(json.dumps(test))
        argv = [exe, "run", "--rm", "--entrypoint", "promtool"]
        if hasattr(os, "getuid"):
            argv += ["--user", f"{os.getuid()}:{os.getgid()}"]
        res = subprocess.run(argv + ["-v", f"{work}:/t", IMAGE, "test", "rules", "/t/t.json"],
                             capture_output=True, text=True)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    out = {}
    for line in (res.stdout + res.stderr).splitlines():
        if line.strip().startswith("got:"):
            for labels, value in SAMPLE.findall(line):
                lb = dict(LABEL.findall(labels))
                out.setdefault(lb["cq_case"], {})[lb.get("participant", lb.get("from_peer"))] = float(value)
    if not out:
        raise AssertionError(f"promtool returned no samples (exit {res.returncode}):\n{res.stdout}{res.stderr}")
    return out


class Semantics(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.exe = runtime()
        if cls.exe is None:
            if os.environ.get("REQUIRE_PROMTOOL") == "1":
                raise AssertionError("REQUIRE_PROMTOOL=1 but no docker/podman runtime is available")
            raise unittest.SkipTest("no container runtime for promtool")
        dash = json.loads(parity.DASHBOARD.read_text(encoding="utf-8"))
        m = manifest()
        variables = parity.variables_for(m, m["steps"][0])
        exprs = {f"{dim}:{role}": (parity.interpolate(e, variables), HE) for dim, role, e in parity.panel_queries(dash)}
        for _, t, dim, role in check.tagged_targets(dash):
            if role == "series":
                for w, at in WINDOWS:
                    exprs[f"{dim}:series:{w}:{at}"] = (parity.interpolate(t["expr"], dict(variables, __interval=f"{w}s")), at)
            if role == "k":
                exprs[f"{dim}:k"] = (parity.interpolate(t["expr"], variables), HE)
                for w, at in WINDOWS if dim != "S" else [(HOUR, HE)]:
                    short = dict(variables, __range=f"{w}s", __range_s=str(w))
                    exprs[f"{dim}:k:{w}:{at}"] = (parity.interpolate(t["expr"], short), at)
        cls.got = run_promtool(cls.exe, exprs)
        cls.at_hs, cls.after_hs = score(HS), score(HS + STEP)
        cls.windows = {(w, at): (score(at - w, at), score(at - w + STEP, at)) for w, at in WINDOWS}
        cls.hour = score(HE - HOUR, HE)

    def assert_close(self, got, want, what):
        self.assertIsNotNone(got, f"{what}: dashboard returned no value")
        self.assertTrue(math.isclose(got, want, rel_tol=1e-9, abs_tol=1e-12), f"{what}: dashboard {got} != scorer {want}")

    def test_the_reset_fixture_is_the_case_under_test(self):
        freeze = freeze_with_reset()
        self.assertEqual(cq_prom.counter_delta(sorted(freeze.items()), HS), 20.0)
        self.assertLess(freeze[HE] - freeze[HS], 20.0)
        fps = dict(next(v for m, lb, v in series() if m == cq_score.M_FPS and lb["from_peer"] == "r1"))
        self.assertTrue(0 < freeze[330] < freeze[315] and fps[330] == 0)

    def test_the_reconnect_fixture_is_the_case_under_test(self):
        pair = {(m, lb["session_id"]): v for m, lb, v in series() if lb.get("from_peer") == "r3"}
        self.assertGreater(max(pair[(cq_score.M_FPS, "81")]), min(pair[(cq_score.M_FPS, "82")]))
        self.assertEqual((pair[(cq_score.M_FREEZE, "82")][405], pair[(cq_score.M_FPS, "82")][405]), (2.0, 0.0))
        self.assertEqual(self.at_hs["stability"]["r3"]["unplanned_reconnects"], 1)
        for recv in REPLACED_BEFORE_HOLD:
            (old, _, _), (new, _, _) = PEOPLE[recv][4]
            replaced = {(m, lb["session_id"]): v for m, lb, v in series() if lb.get("from_peer") == recv}
            self.assertTrue(min(replaced[(cq_score.M_FPS, new)]) < HS < max(replaced[(cq_score.M_FPS, old)]))

    def test_v_matches_the_scorer_through_a_counter_reset_and_an_observer_reconnect(self):
        for user in OBSERVERS:
            want = self.at_hs["cells"]["UxU"]["participants"][user]["V"]
            self.assert_close(self.got["V:table"].get(user), want, f"V {user}")
        self.assert_close(self.got["V:p95"].get(None), self.at_hs["p95_table"]["UxU"]["V"], "p95 V")

    def test_s_matches_the_scorer_for_every_participant(self):
        stability = self.at_hs["stability"]
        self.assertEqual(stability["v1"]["unplanned_reconnects"], 1)
        for user in PEOPLE:
            self.assert_close(self.got["S:table"].get(user), stability[user]["S"], f"S {user}")
        g_q9 = next(g for g in self.at_hs["quality_gates"] if g["gate"] == "G-Q9")
        self.assert_close(self.got["S:p95"].get(None), g_q9["value"]["p95"], "p95 S")

    def test_a_and_q_match_the_scorer_on_the_subquery_grid(self):
        cell = self.after_hs["cells"]["UxU"]
        for dim in ("A", "Q"):
            for user in OBSERVERS:
                want = cell["participants"][user][dim]
                self.assert_close(self.got[f"{dim}:table"].get(user), want, f"{dim} {user}")
            self.assert_close(self.got[f"{dim}:p95"].get(None), self.after_hs["p95_table"]["UxU"][dim], f"p95 {dim}")
        self.assertGreater(cell["participants"]["r1"]["A"], 0)
        self.assertGreater(cell["participants"]["r1"]["Q"], 0)

    def test_k_matches_the_scorers_k_red(self):
        g_q9 = next(g for g in self.at_hs["quality_gates"] if g["gate"] == "G-Q9")
        hour_q9 = next(g for g in self.hour["quality_gates"] if g["gate"] == "G-Q9")
        holds = [(dim, (self.at_hs if dim == "V" else self.after_hs)["cells"]["UxU"]["dimensions"][dim]["k_red"], "")
                 for dim in ("A", "V", "Q")] + [("S", g_q9["value"]["k_red"], ""),
                                                ("S", hour_q9["value"]["k_red"], f":{HOUR}:{HE}")]
        windows = [(dim, (exact if dim == "V" else shifted)["cells"]["UxU"]["dimensions"][dim]["k_red"], f":{w}:{at}")
                   for (w, at), (exact, shifted) in self.windows.items() for dim in ("A", "V", "Q")]
        self.assertTrue({k for _, k, _ in holds + windows} >= {0, 1})
        for dim, want, where in holds + windows:
            self.assertEqual(self.got.get(f"{dim}:k{where}", {}).get(None), want, f"k {dim}{where}")

    def test_every_red_band_has_a_value_exactly_on_it(self):
        red = {d: b["red"] for d, b in cq_score.load_config()["bands"].items()}
        on_band = {d for d in ("A", "V", "Q") for v in (self.at_hs if d == "V" else self.after_hs)["cells"]["UxU"][
            "participants"].values() if v.get(d) == red[d]}
        on_band |= {"S" for st in self.hour["stability"].values() if st["S"] == red["S"]}
        self.assertEqual(on_band, {"A", "V", "Q", "S"})

    def test_over_time_panels_match_the_scorer_on_windows_longer_than_one_step(self):
        for (w, at), (exact, shifted) in self.windows.items():
            for dim in ("A", "V", "Q"):
                scored = (exact if dim == "V" else shifted)["cells"]["UxU"]["participants"]
                want = {u: v[dim] for u, v in scored.items() if dim in v}
                got = self.got.get(f"{dim}:series:{w}:{at}", {})
                self.assertEqual(set(got), set(want), f"{dim} over {w}s at {at}: observers")
                for user, value in want.items():
                    self.assert_close(got[user], value, f"{dim} {user} over {w}s at {at}")

if __name__ == "__main__":
    unittest.main(verbosity=1)
