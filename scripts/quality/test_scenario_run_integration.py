#!/usr/bin/env python3
"""The runner core with fake adapters, end to end through the real collector and the real scorer, whose Prometheus
is the scorer suite's World fixture (#2914 PR-4)."""

import contextlib
import io
import json
import os
import random
import shutil
import signal
import sys
import tempfile
import threading
import unittest
from unittest import mock
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cq_collect  # noqa: E402
import cq_runner  # noqa: E402
import cq_runner_fakes as fakes  # noqa: E402
import scenario_run  # noqa: E402
import test_call_quality_score as sq  # noqa: E402
import test_scenario_run as ut  # noqa: E402

SEEDS = range(int(os.environ.get("CQ_PROPERTY_SEEDS", "4")))
LAST_BROWSER_JOIN = sq.HS - 30 - cq_runner.JOIN_SETTLE_S


class WorldRun:
    """A fake run timed so its hold is exactly the World's [HS, HE], plus the World that matches what happened."""

    def __init__(self, tc, rng, probes=10, viewers=None, events="[]", fleet=None, stack=None, tree=None,
                 tools=None, allow_dirty=False, expect="null", plan=None):
        stagger = rng.choice([0, 1])
        plan = plan or ut.make_plan(probes=probes, viewers=viewers or rng.randint(1, 3), stagger=f"{stagger}s",
                                    events=events, expect=expect)
        delays, launch = {}, sq.JOIN
        browsers = [p for p in plan["processes"] if p["fleet"] == "browser"]
        stagger = browsers[0]["join_stagger_s"]
        for k, proc in enumerate(browsers):
            room = LAST_BROWSER_JOIN - launch
            delays[proc["participants"][0]] = room if k == len(browsers) - 1 else rng.randint(1, int(room * 4)) / 4
            launch += stagger if k + 1 < len(browsers) else 0
        fleet = dict({"join_delay": delays, "media_delay": rng.randint(20, int(sq.HS - 30 - launch) * 4) / 4},
                     **(fleet or {}))
        self.tc, self.rng, self.gone = tc, rng, {}
        self.h = ut.Harness(tc, plan=plan, fleet=fleet, stack=stack, tree=tree, allow_dirty=allow_dirty,
                            tools=tools or fakes.InProcessTools())
        self.h.stack.up_s = sq.JOIN - self.h.clock.wall
        if not isinstance(self.h.tools, ut.StubScorer):
            self.h.tools.transport = self.transport

    def transport(self, url, body, headers):
        return self.world().transport([])(url, body, headers)

    def world(self):
        plan, fleet = self.h.plan, self.h.fleet
        uids = [p["user_id"] for p in plan["participants"]]
        obs = [p["user_id"] for p in plan["participants"] if p["observer"]]
        talkers = [p["user_id"] for p in plan["participants"] if p["talker"]]
        rust = [u for u in uids if u not in obs and u not in talkers]
        w = sq.healthy_world(obs, talkers, rust_extra=len(rust))
        for u in rust:
            w.presence(u)
        w.add("up", {"job": "relay-ws"}, 1)
        gone = dict(fleet.left)
        gone.update({u: t for pid, t in fleet.crash.items() for u in fleet.procs[pid]["participants"]})
        gone.update(self.gone)
        for series in w.series.values():
            for s in series:
                lab = s["metric"]
                lab["meeting_id"] = plan["meeting_id"]
                if "room" in lab:
                    lab["room"] = plan["meeting_id"]
                for uid, t in gone.items():
                    if uid in (lab.get("peer_id"), lab.get("from_peer")) or lab.get("to_peer") == sq.sess(uid):
                        s["values"] = [v for v in s["values"] if v[0] < t]
        return w

    def run(self):
        return self.h.run()

    def result(self):
        path = os.path.join(self.h.dir, "score", "result.json")
        return sq.read_json(path) if os.path.exists(path) else None


def failing(result):
    return sorted(g["gate"] for s in result["steps"] for g in s["validity"] + s["quality_gates"]
                  if g["status"] == "fail")


class EndToEnd(unittest.TestCase):

    def test_a_healthy_fake_run_scores_pass_through_the_real_collector_and_scorer(self):
        for k in SEEDS:
            with self.subTest(seed=k):
                r = WorldRun(self, random.Random(f"healthy/{k}"))
                run = r.run()
                self.assertEqual((r.h.mark("hold_start"), r.h.mark("hold_end")), (sq.HS, sq.HE))
                self.assertEqual((run["verdict"], run["scorer_verdict"], r.h.failed()), ("PASS", "PASS", []),
                                 (run["error"], r.result() and failing(r.result())))
                parts = sq.read_json(os.path.join(r.h.dir, "manifest.json"))["participants"]
                self.assertEqual({p["user_id"]: p["join_ts"] for p in parts},
                                 {e["participants"][0]: e["join_ts"] for e in r.h.lines("joined")})

    def test_planned_leaves_score_pass(self):
        for k, (probes, events) in enumerate(((10, ut.LEAVE_VIEWER), (11, ut.LEAVE_PROBE)) * len(SEEDS)):
            with self.subTest(seed=k, events=events):
                r = WorldRun(self, random.Random(f"leave/{k}"), probes=probes, events=events)
                run = r.run()
                self.assertEqual(run["verdict"], "PASS", (run["error"], r.result() and failing(r.result()),
                                                          r.h.failed(), r.h.log("collector")))
                leaver = r.h.plan["events"][0]["participants"][0]
                part = next(p for p in sq.read_json(os.path.join(r.h.dir, "manifest.json"))["participants"]
                            if p["user_id"] == leaver)
                self.assertEqual(part["leave_ts"], r.h.lines("event")[0]["t_issued"] + 0.25)

    def test_no_run_folder_file_names_a_user_path(self):
        for fleet, verdict in (({}, "PASS"), ({"verdict_ok": {"local": False}}, "INVALID")):
            with self.subTest(verdict):
                r = WorldRun(self, random.Random("paths"), fleet=fleet)
                self.assertEqual((r.run()["verdict"], r.result()["verdict"]), (verdict, verdict))
                with open(os.path.join(r.h.dir, "score", "report.md"), encoding="utf-8") as fh:
                    self.assertIn("--manifest $RUN/manifest.json --prom-url", fh.read())
                for base, _, files in os.walk(r.h.dir):
                    for name in files:
                        with open(os.path.join(base, name), encoding="utf-8") as fh:
                            text = fh.read()
                        self.assertNotIn(r.h.dir, text, name)
                        self.assertNotIn(HERE, text, name)

    def test_a_disabled_reconnect_gate_still_scores_pass(self):
        with ut.config_set("quality_gates", "reconnect_gate_enabled", False):
            r = WorldRun(self, random.Random("no-reconnect-gate"))
            run = r.run()
        self.assertEqual((run["verdict"], run["error"]), ("PASS", None))
        self.assertEqual(next(g["status"] for g in r.result()["steps"][0]["quality_gates"] if g["gate"] == "G-Q9"),
                         "disabled")

    def test_the_real_cli_entry_points_against_an_http_prometheus(self):
        r = WorldRun(self, random.Random("subprocess"), tools=cq_runner.SubprocessTools())
        handler = type("H", (BaseHTTPRequestHandler,), {"do_POST": _answer(r), "log_message": lambda *a: None})
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)
        r.h.stack.url = f"http://127.0.0.1:{httpd.server_address[1]}"
        run = r.run()
        self.assertEqual((run["verdict"], r.h.failed()), ("PASS", []), (run["error"], r.h.log("scorer")))
        self.assertIn("manifest OK: ", r.h.log("validate"))


def _answer(r):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        out = r.transport(self.path, body, dict(self.headers))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)
    return do_POST


class NeverPassUnderFaults(unittest.TestCase):

    def check(self, name, expect_scored, build, assert_more=None, seeds=SEEDS):
        for k in seeds:
            rng = random.Random(f"{name}/{k}")
            r = build(rng)
            run = r.run()
            with self.subTest(fault=name, seed=k):
                self.assertNotEqual(run["verdict"], "PASS", run["gates"])
                self.assertNotEqual(run["exit_code"], 0)
                self.assertEqual(r.result() is not None, expect_scored, (run["error"], r.h.failed()))
                if assert_more:
                    assert_more(r, run, rng)

    def test_a_starved_host_is_invalid_through_g_v5(self):
        def more(r, run, rng):
            self.assertEqual((run["verdict"], r.h.failed(), failing(r.result())), ("INVALID", [], ["G-V5"]))
        self.check("starved", True, lambda rng: WorldRun(
            self, rng, fleet={"hosts": ["local", "ci"], "verdict_ok": {rng.choice(["local", "ci"]): False}}),
            more)

    def test_a_missing_or_off_window_host_verdict_is_invalid_through_g_v5(self):
        def more(r, run, rng):
            self.assertEqual(failing(r.result()), ["G-V5"])
        variants = iter([{"hosts": ["local", "ci"], "missing_verdicts": ["ci"]}, {"window_shift": -15.0},
                         {"extra_verdicts": [{"host": "local"}]}] * len(SEEDS))
        self.check("missing-verdict", True, lambda rng: WorldRun(self, rng, fleet=next(variants)), more,
                   seeds=range(max(3, len(SEEDS))))

    def test_a_crash_in_the_last_scrape_seen_only_at_reap_never_passes(self):
        """Stamped at reap time instead, the same folder scores PASS: the collector would see no exit before
        teardown, and the scorer reads a last sample within one scrape of hold_end as a planned exit."""
        def build(rng):
            return WorldRun(self, rng, fleet={"crash": {rng.choice(["rust-pub", "rust-role-viewers"]):
                                                        sq.HE - rng.randint(1, 14)}, "silent_crash": True})

        def more(r, run, rng):
            pid, t = next(iter(r.h.fleet.crash.items()))
            self.assertIn(f"{r.h.fleet.procs[pid]['participants'][0]}: left at {t:.3f}", r.h.log("collector"))
            self.assertIn("R3", r.h.failed())
            self.assertEqual(reap_time_counterfactual(r), ("manifest", "PASS"))
        self.check("reap", False, build, more)

    def test_a_mid_hold_exit_without_a_leave_never_passes(self):
        self.check("crash", False, lambda rng: WorldRun(self, rng, fleet={"crash": {
            rng.choice(["probe-02", "rust-role-viewers"]): rng.randint(int(sq.HS) + 30, int(sq.HE) - 60)}}))

    def test_a_failed_planned_leave_never_passes(self):
        def more(r, run, rng):
            self.assertIn("R6", r.h.failed())
            self.assertIn("was not confirmed", r.h.log("collector"))
        self.check("failed-leave", False, lambda rng: WorldRun(
            self, rng, events=ut.LEAVE_VIEWER, fleet={"event_results": {"e0": rng.choice(["timeout", "http-500"])}}),
            more)

    def test_runner_gates_cap_a_real_scorer_pass(self):
        faults = [("clock jump", {}, "R8", lambda r: r.h.clock.jumps.append((1200.0, 3.0))),
                  ("dirty tree", {"tree": {"dirty": True}, "allow_dirty": True}, "R9", None),
                  ("tree changed", {"tree": {"dirty_from": 1300.0}}, "R9", None),
                  ("stack restart", {"stack": {"restart": True}}, "R7", None)]
        for name, kw, gate, prep in faults:
            def build(rng, kw=kw, prep=prep):
                r = WorldRun(self, rng, **kw)
                if prep:
                    prep(r)
                return r

            def more(r, run, rng, gate=gate):
                self.assertEqual((run["scorer_verdict"], run["verdict"], r.h.failed()), ("PASS", "INVALID", [gate]))
            self.check(name, True, build, more)

    def test_an_interrupted_or_unjoined_run_is_invalid_and_not_scored(self):
        def build(rng):
            if rng.random() < 0.5:
                return WorldRun(self, rng, fleet={"interrupt_at": rng.randint(int(sq.HS), int(sq.HE))})
            r = WorldRun(self, rng)
            r.h.fleet.never_join = {rng.choice(list(r.h.fleet.parts))}
            return r

        def more(r, run, rng):
            self.assertEqual(run["verdict"], "INVALID")
            self.assertTrue(os.path.exists(os.path.join(r.h.dir, "manifest.partial.json")))
        self.check("partial", False, build, more)


def reap_time_counterfactual(r):
    """Rewrites each stopped line to its reap time and reruns collector and scorer on a copy of the folder."""
    tmp = tempfile.mkdtemp()
    try:
        run_dir = os.path.join(tmp, "run")
        shutil.copytree(r.h.dir, run_dir)
        path = os.path.join(run_dir, cq_collect.EVENTS_FILE)
        lines = cq_collect.read_events(path)
        with open(path, "w", encoding="utf-8") as fh:
            fh.writelines(json.dumps(dict(e, wall=e["logged"]["wall"]) if e["type"] == "stopped" else e) + "\n"
                          for e in lines)
        with contextlib.redirect_stderr(io.StringIO()):
            rc = cq_collect.main(["--run-dir", run_dir])
        if rc != cq_collect.EXIT_OK:
            return "collector exit", rc
        out = os.path.join(tmp, "score")
        args = ["--manifest", os.path.join(run_dir, "manifest.json"), "--prom-url", "http://prom.invalid",
                "--config", _forced(run_dir, r),
                "--generator-verdict", os.path.join(run_dir, "generator-verdict.json"), "--out-dir", out]
        sq.main_with_stderr(args, transport=r.world().transport([]), environ={})
        return "manifest", sq.read_json(os.path.join(out, "result.json"))["verdict"]
    finally:
        shutil.rmtree(tmp)


def _forced(run_dir, r):
    path = os.path.join(run_dir, "forced-config.json")
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(cq_runner.forced_scorer_config(r.h.stack.selector, {}), fh)
    return path


class CiSmokeShape(unittest.TestCase):

    def test_a_two_probe_run_is_invalid_with_exactly_the_declared_gates(self):
        expect = "{verdict: INVALID, invalid_gates: [G-V7]}"
        r = WorldRun(self, random.Random("smoke"), probes=2, viewers=1, expect=expect)
        run = r.run()
        self.assertEqual((run["verdict"], run["scorer_verdict"], r.h.failed()), ("INVALID", "INVALID", []))
        headline = next(s for s in r.result()["steps"] if s["headline"])
        self.assertEqual(sorted(g["gate"] for g in headline["validity"] if g["status"] == "fail"), ["G-V7"])
        self.assertEqual(run["expectation"], {"met": True, "problems": []})

    def test_a_wrong_declaration_is_not_met(self):
        r = WorldRun(self, random.Random("smoke2"), probes=2, viewers=1,
                     expect="{verdict: INVALID, invalid_gates: [G-V7, G-V11]}")
        run = r.run()
        self.assertFalse(run["expectation"]["met"])


CLI_WALL = 1791288000.0


class FailWorldRun(WorldRun):
    def world(self):
        w = super().world()
        for s in w.series[sq.M_EXPAND]:
            s["values"] = [[t, "10.0"] for t, _ in s["values"]]
        return w


def recollect(run_dir):
    out = tempfile.mkdtemp()
    try:
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            rc = cq_collect.main(["--run-dir", run_dir, "--out-dir", out])
        return rc, err.getvalue()
    finally:
        shutil.rmtree(out)


def assert_consistent(tc, run_dir, run, allow_pass=False):
    with open(os.path.join(run_dir, "run.json"), encoding="utf-8") as fh:
        on_disk = json.load(fh)
    tc.assertEqual((on_disk["verdict"], on_disk["exit_code"], on_disk["gates"]),
                   (run["verdict"], cq_runner.RANK[run["verdict"]], run["gates"]))
    if not allow_pass:
        tc.assertNotEqual(run["verdict"], "PASS")
    lines = cq_collect.read_events(os.path.join(run_dir, cq_collect.EVENTS_FILE))
    tc.assertEqual((lines[-1]["type"], lines[-1]["verdict"]), ("done", run["verdict"]))
    launched = {e["proc_id"]: e["wall"] for e in lines if e["type"] == "launched"}
    r5 = next(g for g in run["gates"] if g["gate"] == "R5")["detail"]
    stopped = [e for e in lines if e["type"] == "stopped"]
    for e in stopped:
        tc.assertLessEqual(launched[e["proc_id"]] - cq_runner.EXIT_SLACK_S, e["wall"])
        tc.assertLessEqual(e["wall"], e["logged"]["wall"] + cq_runner.EXIT_SLACK_S)
    for pid in set(launched) - {e["proc_id"] for e in stopped}:
        tc.assertIn(pid, r5)
    for e in (e for e in lines if e["type"] == "event" and e["result"] == "ok"):
        tc.assertLessEqual(e["t_issued"], e["t_confirmed"])
    rc, err = recollect(run_dir)
    tc.assertNotIn("Traceback", err)
    tc.assertIn(rc, (cq_collect.EXIT_OK, cq_collect.EXIT_PARTIAL, cq_collect.EXIT_ERROR))
    if run["collector_exit"] is not None:
        tc.assertEqual(rc, run["collector_exit"], err)


def _wrap(obj, name, before=None, after=None):
    fn = getattr(obj, name)

    def wrapped(*args):
        if before:
            before(*args)
        out = fn(*args)
        return after(out, *args) if after else out
    setattr(obj, name, wrapped)


def _lost_confirmation(r):
    def lost(out, ev):
        raise cq_runner.AdapterError("no reply within 10 s")
    _wrap(r.h.fleet, "apply", after=lost)


def _ok_without_time(r):
    _wrap(r.h.fleet, "apply", after=lambda out, ev: cq_runner.EventOutcome(None, "ok"))


def _late_join(r):
    plan = r.h.plan
    late = [p for p in plan["processes"] if p["fleet"] == "browser"][-1]
    launched = sq.JOIN + late["join_stagger_s"] * (len([p for p in plan["processes"] if p["fleet"] == "browser"]) - 1)
    deadline = sq.JOIN + plan["steps"][0]["join_window_s"] + cq_runner.JOIN_BUDGET_EXTRA_S
    r.h.fleet.join_delay[late["participants"][0]] = deadline + 2.0 - launched
    r.late = late["participants"][0]


def _stop_raises(r):
    def fail(proc):
        if proc["proc_id"] == "rust-role-viewers":
            raise RuntimeError("docker stop: daemon gone")
    _wrap(r.h.fleet, "stop", before=fail)


def _poll_raises(exc):
    def prep(r):
        def fail():
            if r.h.clock.wall >= 1200.0:
                raise exc
        _wrap(r.h.fleet, "poll", before=fail)
    return prep


def _stray_exit(r):
    sent = []

    def stray(out):
        if r.h.clock.wall >= 1200.0 and not sent:
            sent.append(1)
            return out + [cq_runner.Exit("ghost", 1199.0, 0)]
        return out
    _wrap(r.h.fleet, "poll", after=stray)


class LifecycleFaults(unittest.TestCase):

    def fault(self, name, verdict, gates, collector, fleet=None, events="[]", prep=None, scored=False, more=None):
        r = WorldRun(self, random.Random(name), events=events, fleet=fleet)
        if prep:
            prep(r)
        run = r.run()
        with self.subTest(fault=name):
            assert_consistent(self, r.h.dir, run)
            self.assertEqual(run["verdict"], verdict, (run["error"], r.h.failed()))
            self.assertLessEqual(set(gates), set(r.h.failed()))
            self.assertEqual((run["collector_exit"], r.result() is not None), (collector, scored), r.h.log("collector"))
            if r.h.runner.s.stack_started:
                self.assertEqual(r.h.stack.calls[-1], ("down",))
            if more:
                more(r, run)

    def test_a_crash_mid_hold_or_in_the_settle_out_is_refused_at_its_exit_time(self):
        for pid, t in (("probe-02", 1301.3), ("rust-role-viewers", 1301.3), ("rust-role-viewers", sq.HE + 12.3)):
            def more(r, run, pid=pid, t=t):
                self.assertEqual([e["wall"] for e in r.h.lines("stopped") if e["proc_id"] == pid], [t])
                self.assertIn(f"left at {t:.3f}", r.h.log("collector"))
                self.assertIn(pid, r.h.gate("R5")["detail"])
            self.fault(f"crash {pid} {t}", "INVALID", ["R5"], cq_collect.EXIT_ERROR, fleet={"crash": {pid: t}},
                       more=more)

    def test_a_late_join_beyond_the_budget_aborts_with_a_partial_manifest(self):
        def more(r, run):
            self.assertIn(f"not joined: {r.late}", run["aborted"])
            self.assertIn(r.late, r.h.gate("R2")["detail"])
            self.assertNotIn("hold_start", run["timeline"])
            self.assertTrue(os.path.exists(os.path.join(r.h.dir, "manifest.partial.json")))
            self.assertEqual(len([c for c in r.h.fleet.calls if c[0] == "stop"]), len(r.h.plan["processes"]))
        self.fault("late join", "INVALID", ["R1", "R2"], cq_collect.EXIT_PARTIAL, prep=_late_join, more=more)

    def test_a_leave_that_never_confirms_never_passes(self):
        for name, prep, result in (("lost reply", _lost_confirmation, "error: no reply within 10 s"),
                                   ("ok without a time", _ok_without_time, "unconfirmed: t_confirmed None")):
            def more(r, run, result=result):
                self.assertEqual([(e["t_confirmed"], e["result"][:len(result)]) for e in r.h.lines("event")],
                                 [(None, result)])
                self.assertIn("was not confirmed", r.h.log("collector"))
            self.fault(name, "INVALID", ["R5", "R6"], cq_collect.EXIT_ERROR, events=ut.LEAVE_VIEWER, prep=prep,
                       more=more)

    def test_a_stop_that_raises_writes_no_stopped_line_and_every_other_process_is_stopped(self):
        for name, kw in (("adapter error", {"fleet": {"stop_fails": ["rust-role-viewers"]}}),
                         ("runtime error", {"prep": _stop_raises})):
            def more(r, run):
                self.assertIn("rust-role-viewers: stop failed", r.h.gate("R5")["detail"])
                self.assertIn("no stopped line names it", r.h.log("collector"))
                others = [p["proc_id"] for p in r.h.plan["processes"] if p["proc_id"] != "rust-role-viewers"]
                self.assertEqual(sorted(e["proc_id"] for e in r.h.lines("stopped")), sorted(others))
            self.fault(f"stop {name}", "INVALID", ["R5"], cq_collect.EXIT_ERROR, more=more, **kw)

    def test_a_poll_that_raises_is_an_error_that_still_tears_down_and_collects(self):
        for exc, text in ((cq_runner.AdapterError("docker events closed"), "hold: docker events closed"),
                          (RuntimeError("bug"), "hold: internal error RuntimeError: bug")):
            def more(r, run, text=text):
                self.assertEqual(run["error"], text)
                self.assertEqual(len(r.h.lines("stopped")), len(r.h.plan["processes"]))
            self.fault(f"poll {text}", "ERROR", ["R1"], cq_collect.EXIT_PARTIAL, prep=_poll_raises(exc), more=more)

    def test_an_exit_for_an_unknown_process_caps_a_real_pass(self):
        def more(r, run):
            self.assertEqual((run["scorer_verdict"], r.h.failed()), ("PASS", ["R5"]))
            self.assertIn("'ghost'", r.h.gate("R5")["detail"])
        self.fault("stray exit", "INVALID", ["R5"], cq_collect.EXIT_OK, prep=_stray_exit, scored=True, more=more)

    def test_a_backward_wall_step_past_the_drift_limit_never_passes(self):
        def jump(delta):
            return lambda r: r.h.clock.jumps.append((1200.0, delta))

        def more(r, run):
            self.assertEqual((run["scorer_verdict"], r.h.failed()), ("PASS", ["R8"]))
        self.fault("step back 1.5 s", "INVALID", ["R8"], cq_collect.EXIT_OK, prep=jump(-1.5), scored=True,
                   more=more)
        self.fault("step back 1 h", "INVALID", ["R1", "R5", "R8"], cq_collect.EXIT_ERROR, prep=jump(-3600.0))


class WallClockStepsBack(unittest.TestCase):

    def test_wsl2_sized_backward_steps_stay_within_r8_and_keep_the_folder_consistent(self):
        for k in SEEDS:
            rng = random.Random(f"wsl2/{k}")
            r = WorldRun(self, rng, events=ut.LEAVE_VIEWER)
            for _ in range(3):
                r.h.clock.jumps.append((rng.uniform(sq.HS + 1, sq.HE + 60), -rng.uniform(0.006, 0.010)))
            run = r.run()
            with self.subTest(seed=k):
                self.assertFalse(r.h.clock.jumps)
                assert_consistent(self, r.h.dir, run, allow_pass=True)
                self.assertEqual((run["verdict"], r.h.failed()), ("PASS", []), (run["error"], r.h.gate("R8")))
                walls = [r.h.mark(m) for m in ("hold_start", "hold_end", "teardown_start")]
                self.assertEqual(walls, sorted(walls))

    def test_a_backward_step_inside_an_event_call_is_confirmed_never_before_its_issue(self):
        r = WorldRun(self, random.Random("apply-step"), events=ut.LEAVE_VIEWER, fleet={"confirm_delay": 0.004})

        def step(ev):
            r.h.clock.wall -= 0.008
        _wrap(r.h.fleet, "apply", before=step)
        run = r.run()
        line = r.h.lines("event")[0]
        self.assertGreater(line["t_issued"], line["wall"])
        self.assertEqual((line["result"], line["t_confirmed"]), ("ok", line["t_issued"]))
        self.assertEqual((run["verdict"], r.h.failed()), ("PASS", []), (run["error"], r.h.log("collector")))
        assert_consistent(self, r.h.dir, run, allow_pass=True)

    def test_a_confirmation_before_its_issue_with_no_clock_step_is_never_confirmed(self):
        r = WorldRun(self, random.Random("apply-early"), events=ut.LEAVE_VIEWER,
                     fleet={"confirm_delay": 0.004, "confirm_skew": -0.012})
        run = r.run()
        line = r.h.lines("event")[0]
        self.assertEqual(line["t_confirmed"], None)
        self.assertTrue(line["result"].startswith("unconfirmed"), line)
        self.assertIn("R6", r.h.failed())
        assert_consistent(self, r.h.dir, run)


class EventSemantics(unittest.TestCase):
    EVENTS = ("[{at: hold+15s, select: {role: probe-ref, count: 1}, action: unmute}, "
              "{at: hold+5m, select: {role: viewers, count: 1}, action: leave}, "
              "{at: hold+565s, select: {role: probe-ref, count: 1}, action: mute}]")

    def test_confirmed_events_lie_in_their_window_and_reach_the_manifest_once_each(self):
        for k in range(8):
            rng = random.Random(f"events/{k}")
            h = ut.Harness(self, events=self.EVENTS, fleet={"confirm_delay": rng.uniform(0.01, 3.0)})
            extra = rng.uniform(0, 0.4)
            h.clock.sleep = lambda s, h=h, extra=extra: h.clock.advance(s + extra) if s > 0 else None
            h.run()
            with self.subTest(seed=k):
                self.assertTrue(h.gate("R6")["ok"], h.gate("R6"))
                hs, he = h.mark("hold_start"), h.mark("hold_end")
                lines = h.lines("event")
                offsets = {e["event_id"]: e["at_offset_s"] for e in h.plan["events"]}
                self.assertEqual(sorted(e["event_id"] for e in lines), sorted(offsets))
                for e in lines:
                    self.assertEqual(e["result"], "ok")
                    self.assertLessEqual(hs + offsets[e["event_id"]], e["t_issued"])
                    self.assertLessEqual(e["t_issued"], hs + offsets[e["event_id"]] + extra + 1e-6)
                    self.assertLessEqual(e["t_issued"], e["t_confirmed"])
                    self.assertLessEqual(e["t_confirmed"], e["wall"])
                    self.assertLessEqual(hs + 15, e["t_issued"])
                    self.assertLessEqual(e["t_issued"], hs + h.plan["steps"][0]["hold_s"] - 30 + extra + 1e-6)
                with open(os.path.join(h.dir, "manifest.json"), encoding="utf-8") as fh:
                    events = json.load(fh)["events"]
                want = sorted((e["t_confirmed"] if e["action"] == "unmute" else e["t_issued"], e["action"])
                              for e in lines)
                self.assertEqual([(e["at"], e["action"]) for e in events], want)
                self.assertTrue(all(e["step_id"] == "s1" and hs <= e["at"] <= he for e in events))


class CollectorContractNotes(unittest.TestCase):

    def test_max_abs_skew_ms_comes_from_per_node_and_widens_the_collector_tolerance(self):
        for skew, verdict in ((-1200, "PASS"), (0, "INVALID")):
            with self.subTest(skew=skew):
                r = WorldRun(self, random.Random("skew"), fleet={"facts": {
                    "sync": "ntp", "max_abs_skew_ms": 0,
                    "per_node": [{"node": "local", "skew_ms": 0}, {"node": "ci", "skew_ms": skew}]}})
                r.h.fleet.join_delay[r.h.plan["participants"][0]["user_id"]] = -3.0
                run = r.run()
                self.assertEqual(run["verdict"], verdict, (r.h.failed(), r.h.log("collector")))
                if skew:
                    with open(os.path.join(r.h.dir, "manifest.json"), encoding="utf-8") as fh:
                        self.assertEqual(json.load(fh)["clock"]["max_abs_skew_ms"], abs(skew))
                else:
                    self.assertIn("a skewed clock?", r.h.log("collector"))

    def test_a_host_name_never_reaches_result_json_or_report_md(self):
        r = WorldRun(self, random.Random("host"), fleet={"hosts": ["local", "build-host-7"]})
        run = r.run()
        self.assertEqual((run["verdict"], failing(r.result())), ("INVALID", ["G-V5"]))
        self.assertIn("is not named", sq.gate(r.result()["steps"][0], "G-V5")["detail"])
        for name in ("result.json", "report.md"):
            with open(os.path.join(r.h.dir, "score", name), encoding="utf-8") as fh:
                text = fh.read()
            self.assertIn("G-V5", text)
            self.assertNotIn("build-host-7", text)

    def test_an_undeclared_or_truthy_host_verdict_is_invalid_through_g_v5(self):
        for name, fleet in (("undeclared host", {"extra_verdicts": [{"host": "ci"}]}),
                            ("ok is 1", {"verdict_ok": {"local": 1}})):
            with self.subTest(name):
                r = WorldRun(self, random.Random(name), fleet=fleet)
                run = r.run()
                self.assertEqual((run["verdict"], r.h.failed(), failing(r.result())), ("INVALID", [], ["G-V5"]))


class _ClockProxy:
    def __init__(self, target):
        self.target = target

    @property
    def wall(self):
        return self.target.wall

    def now(self):
        return self.target.now()

    def sleep(self, seconds):
        self.target.sleep(seconds)


class CliWorld:
    """scenario_run.main over WorldRun adapters, with the real collector and scorer."""

    def __init__(self, tc, text=None, world=WorldRun, tree=None, fleet=None, stack=None, hook=None):
        self.tc, self.world, self.fleet_kw, self.stack_kw, self.hook = tc, world, fleet, stack, hook
        tmp = tempfile.mkdtemp()
        tc.addCleanup(shutil.rmtree, tmp)
        self.repo, self.out = os.path.join(tmp, "repo"), os.path.join(tmp, "runs")
        os.makedirs(self.repo)
        self.scenario = os.path.join(tmp, "t.yaml")
        with open(self.scenario, "w", encoding="utf-8") as fh:
            fh.write(text or ut.scenario_text())
        self.clock = _ClockProxy(fakes.FakeClock(wall=CLI_WALL))
        self.tree = fakes.FakeTree(self.clock, self.repo, **(tree or {}))
        self.tools = fakes.InProcessTools()
        self.dir, self.r, self.fired, self.factory_calls = os.path.join(self.out, ut.RUN_ID), None, None, 0

    def factory(self, plan):
        self.factory_calls += 1
        self.r = self.world(self.tc, random.Random("cli"), plan=plan, fleet=self.fleet_kw, stack=self.stack_kw)
        self.clock.target = self.r.h.clock
        self.tools.transport = self.r.transport
        self.fired = self.hook(self) if self.hook else None
        return self.r.h.stack, self.r.h.fleet

    def main(self, *argv, environ=None):
        out, err = io.StringIO(), io.StringIO()
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                rc = scenario_run.main(["--scenario", self.scenario, *(argv or ("--out-dir", self.out))],
                                       adapters={"local": self.factory}, tree=self.tree, clock=self.clock,
                                       tools=self.tools, environ=environ or {})
        except KeyboardInterrupt:
            self.tc.fail("KeyboardInterrupt escaped scenario_run.main")
        return rc, out.getvalue(), err.getvalue()

    def run_json(self):
        with open(os.path.join(self.dir, "run.json"), encoding="utf-8") as fh:
            return json.load(fh)

    def lines(self):
        return cq_collect.read_events(os.path.join(self.dir, cq_collect.EVENTS_FILE))

    @staticmethod
    def gate(run, name):
        return next(g for g in run["gates"] if g["gate"] == name)["detail"]

    @staticmethod
    def failed(run):
        return sorted(g["gate"] for g in run["gates"] if not g["ok"])

    def stops(self):
        return [c[1] for c in self.r.h.fleet.calls if c[0] == "stop"]


class CliExitCodes(unittest.TestCase):

    def check(self, cli, rc, verdict, gates, *argv):
        code, out, err = cli.main(*argv)
        run = cli.run_json()
        self.assertEqual((code, run["verdict"], run["exit_code"], out), (rc, verdict, rc, ""), err)
        self.assertEqual(cli.failed(run), gates)
        failed = ", ".join(g["gate"] for g in run["gates"] if not g["ok"]) or "none"
        self.assertIn(f"{ut.RUN_ID}: {verdict} (scorer {run['scorer_verdict']}; failed runner gates {failed})", err)
        assert_consistent(self, cli.dir, run, allow_pass=verdict == "PASS")
        return run, err

    def test_each_verdict_path_exits_with_its_code(self):
        cases = [("PASS", {}, 0, "PASS", []),
                 ("FAIL", {"world": FailWorldRun}, 1, "FAIL", []),
                 ("starved", {"fleet": {"verdict_ok": {"local": False}}}, 2, "INVALID", []),
                 ("restart", {"stack": {"restart": True}}, 2, "INVALID", ["R7"]),
                 ("stack up fails", {"stack": {"fail_up": True}}, 3, "ERROR",
                  ["R1", "R10", "R2", "R3", "R4", "R7"])]
        for name, kw, rc, verdict, gates in cases:
            with self.subTest(name):
                run, _ = self.check(CliWorld(self, **kw), rc, verdict, gates)
                self.assertEqual(run["scorer_verdict"], {"PASS": "PASS", "FAIL": "FAIL", "INVALID": (
                    "PASS" if gates else "INVALID"), "ERROR": None}[verdict])

    def test_a_scorer_that_cannot_reach_prometheus_is_an_error(self):
        cli = CliWorld(self, hook=lambda c: setattr(c.tools, "transport", _refused))
        _, err = self.check(cli, 3, "ERROR", ["R10"])
        self.assertIn("error: score: the scorer exited 3 without a result", err)

    def test_a_dirty_tree_is_refused_unless_allowed_and_then_capped(self):
        cli = CliWorld(self, tree={"dirty": True})
        run, err = self.check(cli, 3, "ERROR", ["R1", "R10", "R2", "R3", "R4", "R7", "R8", "R9"])
        self.assertIn("--allow-dirty", err)
        self.assertEqual((cli.r.h.stack.calls, cli.r.h.fleet.calls), ([], []))
        cli = CliWorld(self, tree={"dirty": True})
        run, _ = self.check(cli, 2, "INVALID", ["R9"], "--out-dir", cli.out, "--allow-dirty")
        self.assertEqual(run["scorer_verdict"], "PASS")

    def test_refused_scenarios_exit_3_and_create_no_run_folder(self):
        for old, new, needle in (("mode: gate", "mode: report", "report mode"),
                                 ("expect: null", "expect: {verdict: PASS, invalid_gates: []}", "never expects PASS"),
                                 ("target: local", "target: cluster", "run.target")):
            with self.subTest(new):
                cli = CliWorld(self, text=ut.scenario_text().replace(old, new, 1))
                rc, out, err = cli.main()
                self.assertEqual((rc, out, cli.factory_calls), (3, "", 0))
                self.assertIn(needle, err)
                self.assertFalse(os.path.exists(cli.out))

    def test_a_reused_run_folder_exits_3_and_leaves_the_first_run_untouched(self):
        cli = CliWorld(self)
        self.assertEqual(cli.main()[0], 0)
        with open(os.path.join(cli.dir, "run.json"), "rb") as fh:
            first = fh.read()
        cli.clock.target = fakes.FakeClock(wall=CLI_WALL)
        rc, _, err = cli.main()
        self.assertEqual((rc, cli.factory_calls), (3, 1))
        self.assertIn("already exists", err)
        with open(os.path.join(cli.dir, "run.json"), "rb") as fh:
            self.assertEqual(fh.read(), first)

    def test_an_in_repo_out_dir_from_the_environment_exits_3(self):
        cli = CliWorld(self)
        rc, _, err = cli.main("--allow-dirty", environ={"CQ_RUNS_DIR": os.path.join(cli.repo, "runs")})
        self.assertEqual((rc, cli.factory_calls, os.listdir(cli.repo)), (3, 0, []))
        self.assertIn("outside the repository", err)


def _refused(url, body, headers):
    raise OSError("connection refused")


def _sigterm(*args):
    os.kill(os.getpid(), signal.SIGTERM)


def _once(obj, name, when=lambda *a: True, signum=signal.SIGTERM):
    fired = []

    def before(*args):
        if not fired and when(*args):
            fired.append(1)
            os.kill(os.getpid(), signum)
    _wrap(obj, name, before=before)
    return fired


class SigtermInEveryPhase(unittest.TestCase):
    """One SIGTERM, delivered for real through the CLI's handler, at a point inside each phase."""

    BEFORE_TEARDOWN = {
        "preflight": (lambda c: _once(c.tree, "dirty"), "preflight", ()),
        "stack": (lambda c: _once(c.r.h.stack, "up"), "stack_ready", ("preflight",)),
        "join": (lambda c: _once(c.r.h.fleet, "joined"), "hold_start", ("join_start",)),
        "steady": (lambda c: _once(c.r.h.fleet, "poll", lambda: 975.0 <= c.r.h.clock.wall < sq.HS), "hold_start",
                   ("join_start",)),
        "hold": (lambda c: _once(c.r.h.fleet, "poll", lambda: c.r.h.clock.wall >= 1200.0), "hold_end",
                 ("hold_start",)),
        "settle-out": (lambda c: _once(c.r.h.fleet, "poll", lambda: c.r.h.clock.wall >= sq.HE + 5), "scored",
                       ("hold_end", "collected")),
    }
    AFTER_TEARDOWN_STARTS = {
        "a stop": lambda c: _once(c.r.h.fleet, "stop"),
        "collect": lambda c: _once(c.r.h.fleet, "collect"),
        "collector": lambda c: _once(c.tools, "run", lambda argv: "cq_collect.py" in argv[1]),
        "score": lambda c: _once(c.tools, "run", lambda argv: "--prom-url" in argv),
        "down": lambda c: _once(c.r.h.stack, "down"),
    }

    def setUp(self):
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            self.addCleanup(signal.signal, sig, signal.signal(sig, lambda *a: None))

    def interrupted_run(self, hook):
        cli = CliWorld(self, hook=hook)
        rc = cli.main()[0]
        run = cli.run_json()
        self.assertEqual((cli.fired, rc), ([1], run["exit_code"]))
        assert_consistent(self, cli.dir, run)
        self.assertTrue(run["interrupted"] or run["error"])
        return cli, rc, run, {e["type"] for e in cli.lines()}, [e["proc_id"] for e in cli.lines()
                                                                 if e["type"] == "launched"]

    def test_a_sigterm_before_teardown_tears_down_and_exits_invalid(self):
        for phase, (hook, absent, present) in self.BEFORE_TEARDOWN.items():
            with self.subTest(phase):
                cli, rc, run, kinds, launched = self.interrupted_run(hook)
                self.assertLessEqual(set(present), kinds)
                self.assertNotIn(absent, kinds)
                self.assertEqual((rc, run["verdict"], run["interrupted"], run["error"]), (2, "INVALID", True, None))
                self.assertEqual(cli.r.h.stack.calls[-1:], [("down",)] if phase != "preflight" else [])
                self.assertEqual(sorted(cli.stops()), sorted(launched))
                self.assertNotIn("scored", kinds)
                self.assertIsNone(cli.r.result())
                if phase == "settle-out":
                    self.assertLess(run["timeline"]["teardown_start"], sq.HE + cq_runner.TEARDOWN_DELAY_S)
                    self.assertEqual(run["scorer_verdict"], None)
                    self.assertIn("R1", cli.failed(run))

    def test_a_sigterm_after_teardown_starts_stops_every_process_and_is_never_scored_after_it(self):
        for phase, hook in self.AFTER_TEARDOWN_STARTS.items():
            with self.subTest(phase):
                cli, rc, run, kinds, launched = self.interrupted_run(hook)
                self.assertEqual((rc, run["verdict"], run["error"], run["teardown_problems"]), (2, "INVALID", None, []))
                self.assertLessEqual({"teardown_start", "collected"}, kinds)
                self.assertEqual(sorted(cli.stops()), sorted(launched))
                self.assertEqual("scored" in kinds, phase == "down")

    def test_sigint_and_sighup_are_handled_like_sigterm(self):
        for signum in (signal.SIGINT, signal.SIGHUP):
            with self.subTest(signal=signum.name):
                cli, rc, run, kinds, launched = self.interrupted_run(
                    lambda c, signum=signum: _once(c.r.h.fleet, "poll", lambda: c.r.h.clock.wall >= 1200.0, signum))
                self.assertEqual((rc, run["verdict"], run["signals"]), (2, "INVALID", [signum.name]))
                self.assertEqual(sorted(cli.stops()), sorted(launched))
                self.assertNotIn("hold_end", kinds)


class SignalGaps(unittest.TestCase):
    """The integration pass's known gaps: one real SIGTERM through the CLI at a point the first draft mishandled."""

    def setUp(self):
        self.addCleanup(signal.signal, signal.SIGTERM, signal.signal(signal.SIGTERM, lambda *a: None))

    def assert_all_stopped_and_collected(self, cli):
        launched = [e["proc_id"] for e in cli.lines() if e["type"] == "launched"]
        self.assertEqual(sorted(cli.stops()), sorted(launched))
        self.assertIn("collected", {e["type"] for e in cli.lines()})
        self.assertNotIn("scored", {e["type"] for e in cli.lines()})
        self.assertEqual((cli.run_json()["teardown_problems"], cli.run_json()["signals"]), ([], ["SIGTERM"]))

    def test_gap1_a_sigterm_inside_finish_still_writes_run_json_and_exits_2(self):
        gates = cq_runner.Runner.gates

        def interrupted(runner):
            _sigterm()
            return gates(runner)
        cli = CliWorld(self)
        with mock.patch.object(cq_runner.Runner, "gates", interrupted):
            rc = cli.main()[0]
        self.assertEqual((rc, cli.run_json()["verdict"], cli.run_json()["interrupted"]), (2, "INVALID", True))

    def test_gap2_a_sigterm_at_the_teardown_stack_state_still_stops_every_process(self):
        cli = CliWorld(self, hook=lambda c: _once(c.r.h.stack, "state", lambda: c.r.h.stack.states >= 1))
        self.assertEqual(cli.main()[0], 2)
        self.assert_all_stopped_and_collected(cli)

    def test_gap3_a_sigterm_between_two_stops_still_stops_the_rest(self):
        def hook(c):
            def after(out, proc):
                if proc["proc_id"] == "rust-pub":
                    _once(c.r.h.clock, "now")
                return out
            _wrap(c.r.h.fleet, "stop", after=after)
        cli = CliWorld(self, hook=hook)
        self.assertEqual(cli.main()[0], 2)
        self.assert_all_stopped_and_collected(cli)

    def test_gap4_a_sigterm_in_the_teardown_git_check_still_collects(self):
        cli = CliWorld(self, hook=lambda c: _once(c.tree, "commit", lambda: c.r.h.clock.wall >= 1600.0))
        self.assertEqual(cli.main()[0], 2)
        self.assert_all_stopped_and_collected(cli)


class EventWindowGap(unittest.TestCase):
    EVENTS = ("[{at: hold+5m, select: {role: viewers, count: 1}, action: leave}, "
              "{at: hold+565s, select: {role: probe-ref, count: 1}, action: leave}]")

    def test_gap5_an_event_pushed_past_hold_end_minus_30_s_never_passes(self):
        for delay, gates in ((299.0, ["R6"]), (330.0, ["R1", "R6"])):
            with self.subTest(delay=delay):
                r = WorldRun(self, random.Random("late-event"), probes=11, events=self.EVENTS)

                def slow(out, ev, r=r, delay=delay):
                    if ev["event_id"] == "e0":
                        r.h.clock.advance(delay)
                    return out
                _wrap(r.h.fleet, "apply", after=slow)
                run = r.run()
                late = r.h.lines("event")[1]["t_issued"]
                self.assertGreater(late, r.h.mark("hold_start") + r.h.plan["steps"][0]["hold_s"] - 30)
                self.assertNotEqual(run["verdict"], "PASS", (late, run["gates"]))
                self.assertLessEqual(set(gates), set(r.h.failed()), run["gates"])

if __name__ == "__main__":
    unittest.main()
