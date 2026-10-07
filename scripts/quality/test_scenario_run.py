#!/usr/bin/env python3
"""Unit tests for the scenario runner core with fake adapters and a stub scorer (#2914 PR-4)."""

import contextlib
import errno
import gc
import hashlib
import io
import itertools
import json
import os
import random
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import unittest.mock
import warnings

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cq_collect  # noqa: E402
import cq_runner  # noqa: E402
import cq_runner_fakes as fakes  # noqa: E402
import cq_scenario  # noqa: E402
import cq_score  # noqa: E402
import scenario_run  # noqa: E402

RUN_ID = "cqtest-20261006t1200-0123abc"
QUALITY = [{"gate": g, "status": "not_measured" if g == "G-Q5" else "pass"} for g in cq_runner.QUALITY_GATES]
SCENARIO = '''\
schema: scale-scenario/v1
run: {{id_prefix: cqtest, seed: 7, target: local, fidelity_waivers: []}}
population:
  - {{role: probe-ref, fleet: browser, count: {probes}, media: {{camera: on, mic: muted}}, join_stagger: {stagger}}}
  - {{role: talkers, fleet: rust, count: 1, media: {{camera: on, mic: continuous}}}}
  - {{role: viewers, fleet: rust, count: {viewers}, media: {{camera: off, mic: off}},
      receive: {{pin_video_layer: 0, viewport_visible_count: 1}}}}
steps: [{{id: s1, hold: {hold}, headline: true}}]
events: {events}
scoring: {{mode: gate, relay_path_exemptions: {{}}, expect: {expect}}}
'''
LEAVE_VIEWER = "[{at: hold+5m, select: {role: viewers, count: 1}, action: leave}]"
LEAVE_PROBE = "[{at: hold+5m, select: {role: probe-ref, count: 1}, action: leave}]"


def scenario_text(probes=10, viewers=2, stagger="1s", hold="10m", events="[]", expect="null"):
    return SCENARIO.format(probes=probes, viewers=viewers, stagger=stagger, hold=hold, events=events, expect=expect)


def make_plan(**kw):
    return cq_scenario.compile_scenario(cq_scenario.parse_yaml(scenario_text(**kw)), run_id=RUN_ID,
                                        scenario_file="external:t.yaml", scenario_sha256="0" * 64)


def stub_result(config_path, verdict="PASS", gv5="pass", drop_config=False, validity=None, steps=None,
                bad_sha=False, bad_overrides=False, extra=()):
    cfg = cq_score.load_config(None if drop_config else config_path)
    statuses = {g: "not_measured" if g in ("G-V2", "G-V4") else "pass" for g in cq_runner.VALIDITY_GATES}
    statuses["G-V5"] = gv5
    statuses.update(validity or {})
    sha = hashlib.sha256(json.dumps(cfg, sort_keys=True).encode()).hexdigest()
    return {"verdict": verdict, "config_overrides": {} if bad_overrides else cq_score.config_overrides(cfg),
            "run": {"config_sha256": "0" * 64 if bad_sha else sha},
            "steps": [{"step_id": "s1", "headline": True, "verdict": verdict, "invalid_reasons": [],
                       "validity": [{"gate": g, "status": st} for g, st in statuses.items() if st is not None]
                       + list(extra)}]
            if steps is None else steps}


def config_set(section, key, value):
    load = cq_score.load_config

    def loaded(path=None):
        cfg = load(path)
        cfg[section][key]["value"] = value
        return cfg
    return unittest.mock.patch.object(cq_score, "load_config", loaded)


class StubScorer(fakes.InProcessTools):
    """The real collector and --validate-only; the scoring call answers `verdict` with the forced config echoed and
    every validity gate pass, except G-V2 and G-V4 not_measured, as the real scorer does under the forced config."""

    def __init__(self, verdict="PASS", rc=None, gv5=None, drop_config=False, mutate=None, quality=QUALITY, step=None,
                 **result):
        super().__init__(mutate=mutate)
        self.verdict, self.rc, self.gv5, self.drop_config = verdict, rc, gv5, drop_config
        self.quality, self.step, self.result = list(quality), dict(step or {}), result

    def run(self, argv):
        if os.path.basename(argv[1]) != "call_quality_score.py" or "--prom-url" not in argv:
            return super().run(argv)
        args = self.mutate("call_quality_score.py", argv[2:]) if self.mutate else argv[2:]
        self.calls.append(("score", args))
        arg = dict(zip(args[::2], args[1::2]))
        with open(arg["--generator-verdict"], encoding="utf-8") as fh:
            gv5 = self.gv5 if self.gv5 is not None else ("pass" if json.load(fh)["ok"] is True else "fail")
        os.makedirs(arg["--out-dir"], exist_ok=True)
        with open(os.path.join(arg["--out-dir"], "result.json"), "w", encoding="utf-8") as fh:
            result = stub_result(arg["--config"], self.verdict, gv5, self.drop_config, **self.result)
            for step in result["steps"]:
                step["quality_gates"] = self.quality
                step.update(self.step)
            json.dump(result, fh)
        return (cq_runner.SCORER_EXIT[self.verdict] if self.rc is None else self.rc), "", ""


class Harness:
    def __init__(self, tc, plan=None, fleet=None, stack=None, tree=None, tools=None, allow_dirty=False,
                 stack_mode="up", keep_stack=False, **plan_kw):
        tmp = tempfile.mkdtemp()
        tc.addCleanup(shutil.rmtree, tmp)
        self.tc, self.repo = tc, os.path.join(tmp, "repo")
        os.makedirs(self.repo)
        self.plan = plan or make_plan(**plan_kw)
        self.clock = fakes.FakeClock()
        self.fleet = fakes.FakeFleet(self.clock, self.plan, **(fleet or {}))
        self.stack = fakes.FakeStack(self.clock, **(stack or {}))
        self.tree = fakes.FakeTree(self.clock, self.repo, **(tree or {}))
        self.tools = tools or StubScorer()
        self.dir = cq_runner.prepare_run_dir(os.path.join(tmp, "runs"), self.plan["run_id"], self.repo)
        self.runner = cq_runner.Runner(self.plan, b"scenario", self.dir, stack=self.stack, fleet=self.fleet,
                                       tree=self.tree, clock=self.clock, tools=self.tools, allow_dirty=allow_dirty,
                                       stack_mode=stack_mode, keep_stack=keep_stack)

    def run(self):
        try:
            self.result = self.runner.run()
        except KeyboardInterrupt:
            self.tc.fail("KeyboardInterrupt escaped Runner.run")
        return self.result

    def lines(self, kind=None):
        lines = cq_collect.read_events(os.path.join(self.dir, cq_collect.EVENTS_FILE))
        return [e for e in lines if kind is None or e["type"] == kind]

    def gate(self, name):
        return next(g for g in self.result["gates"] if g["gate"] == name)

    def failed(self):
        return sorted(g["gate"] for g in self.result["gates"] if not g["ok"])

    def proc(self, pid):
        return next(p for p in self.plan["processes"] if p["proc_id"] == pid)

    def uid(self, role, k=0):
        return [p["user_id"] for p in self.plan["participants"] if p["role"] == role][k]

    def mark(self, kind):
        return self.result["timeline"][kind]

    def log(self, name):
        with open(os.path.join(self.dir, "logs", f"{name}.log"), encoding="utf-8") as fh:
            return fh.read()


class HealthyRun(unittest.TestCase):

    def test_a_healthy_run_follows_the_phase_order_and_every_gate_passes(self):
        h = Harness(self)
        run = h.run()
        self.assertEqual((run["verdict"], run["exit_code"], h.failed(), run["error"]), ("PASS", 0, [], None))
        kinds = [e["type"] for e in h.lines()]
        firsts = [k for k, _ in itertools.groupby(kinds)]
        self.assertEqual(firsts, ["preflight", "stack_ready", "join_start", "launched", "joined", "hold_start",
                                  "hold_end", "teardown_start", "stopped", "collected", "scored", "done"])
        self.assertEqual(h.mark("join_start"), 940.0)
        self.assertEqual(h.mark("hold_start"), 1000.0)
        self.assertEqual(h.mark("hold_end"), 1600.0)
        self.assertEqual(h.mark("teardown_start"), 1630.0)
        self.assertTrue(os.path.exists(os.path.join(h.dir, "manifest.json")))
        self.assertEqual(h.stack.calls, [("up", "up"), ("down",)])

    def test_every_line_has_a_type_a_finite_wall_and_a_clock_reading(self):
        h = Harness(self, events=LEAVE_VIEWER)
        h.run()
        for e in h.lines():
            with self.subTest(e["type"]):
                self.assertTrue(cq_runner._num(e["wall"]))
                if e["type"] == "stopped":
                    self.assertTrue(cq_runner._num(e["logged"]["mono"]))
                    self.assertNotIn("mono", e)
                else:
                    self.assertTrue(cq_runner._num(e["mono"]))

    def test_the_run_folder_layout(self):
        h = Harness(self)
        h.run()
        for rel in ("scenario.yaml", cq_collect.PLAN_FILE, cq_collect.EVENTS_FILE, cq_collect.IMAGES_FILE,
                    "stack/state-ready.json", "stack/state-teardown.json", "rust/participants-0.json",
                    "probes/0/participants", "generator-verdict.json", "manifest.json", "score/config.json",
                    "score/result.json", "run.json", "commands.txt", "logs/collector.log", "logs/validate.log",
                    "logs/scorer.log"):
            self.assertTrue(os.path.exists(os.path.join(h.dir, rel)), rel)
        with open(os.path.join(h.dir, "run.json"), encoding="utf-8") as fh:
            self.assertEqual(json.load(fh)["verdict"], "PASS")

    def test_commands_carry_no_host_paths(self):
        h = Harness(self)
        h.runner.argv = ["scripts/quality/scenario_run.py", "--scenario", "x.yaml"]
        h.run()
        with open(os.path.join(h.dir, "commands.txt"), encoding="utf-8") as fh:
            text = fh.read()
        self.assertNotIn(h.dir, text)
        self.assertNotIn(HERE, text)
        self.assertNotIn(sys.executable + " ", text)
        self.assertEqual(text.splitlines()[0], "scripts/quality/scenario_run.py --scenario x.yaml")
        self.assertIn("python3 scripts/quality/call_quality_score.py --manifest '$RUN/manifest.json' --prom-url",
                      text)

    def test_the_plan_written_is_the_compiled_plan(self):
        h = Harness(self)
        h.run()
        with open(os.path.join(h.dir, cq_collect.PLAN_FILE), encoding="utf-8") as fh:
            self.assertEqual(json.load(fh), h.plan)


class Timeline(unittest.TestCase):

    def test_hold_and_teardown_respect_the_settle_rules_over_random_timings(self):
        for seed in range(12):
            rng = random.Random(seed)
            with self.subTest(seed=seed):
                plan = make_plan(stagger=f"{rng.randint(0, 3)}s", hold=f"{rng.randint(60, 900)}s",
                                 viewers=rng.randint(1, 3))
                h = Harness(self, plan=plan, fleet={"join_delay": rng.uniform(0.5, 40),
                                                    "rust_join_delay": rng.uniform(0.5, 5),
                                                    "media_delay": rng.uniform(5, 60)})
                h.run()
                joined = h.lines("joined")
                hs, he, td = h.mark("hold_start"), h.mark("hold_end"), h.mark("teardown_start")
                self.assertGreaterEqual(hs, max(e["join_ts"] for e in joined) + 30 + cq_runner.JOIN_SETTLE_S)
                self.assertGreaterEqual(hs, max(e["media_started_at"] for e in joined if e["media_started_at"])
                                        + cq_runner.MEDIA_SETTLE_S)
                self.assertGreaterEqual(he, hs + plan["steps"][0]["hold_s"])
                self.assertGreaterEqual(td, he + cq_runner.TEARDOWN_DELAY_S)
                self.assertEqual(h.result["verdict"], "PASS", h.result["gates"])

    def test_hold_marks_are_the_reached_times_not_the_planned_ones(self):
        h = Harness(self)
        h.clock.sleep = lambda s: h.clock.advance(s + 0.375) if s > 0 else None
        target = h.runner.hold_start_target
        h.runner.hold_start_target = lambda: target() if h.runner.s.phase == "steady" else None
        h.run()
        hs = next(e["wall"] for e in h.lines("hold_start"))
        self.assertGreater(hs, target())
        self.assertEqual(h.mark("hold_start"), hs)
        self.assertGreaterEqual(h.mark("hold_end"), hs + 600)
        self.assertGreater(h.mark("hold_end"), target() + 600)

    def test_events_are_issued_at_their_hold_offset_once_each(self):
        h = Harness(self, events="[{at: hold+1m, select: {role: probe-ref, count: 1}, action: unmute}, "
                                 "{at: hold+2m, select: {role: probe-ref, count: 1}, action: mute}]")
        h.run()
        lines = h.lines("event")
        self.assertEqual([(e["event_id"], e["action"], e["t_issued"]) for e in lines],
                         [("e0", "unmute", 1060.0), ("e1", "mute", 1120.0)])
        self.assertEqual([e["t_confirmed"] for e in lines], [1060.5, 1120.5])
        self.assertEqual(h.result["verdict"], "PASS")

    def test_probes_launch_staggered_then_rust(self):
        h = Harness(self, stagger="2s")
        h.run()
        launched = [(e["proc_id"], e["wall"]) for e in h.lines("launched")]
        self.assertEqual([w for _, w in launched[:10]], [940.0 + 2 * k for k in range(10)])
        self.assertEqual([p for p, _ in launched[10:]], ["rust-pub", "rust-role-viewers"])

    def test_teardown_stops_every_rust_process_before_any_probe(self):
        h = Harness(self)
        h.run()
        stops = [c[1] for c in h.fleet.calls if c[0] == "stop"]
        self.assertEqual(stops[:2], ["rust-pub", "rust-role-viewers"])
        self.assertEqual(sorted(stops[2:]), [f"probe-{k:02d}" for k in range(10)])
        first_stop = h.fleet.calls.index(("stop", "rust-pub"))
        self.assertLess(max(i for i, c in enumerate(h.fleet.calls) if c[0] == "apply") if any(
            c[0] == "apply" for c in h.fleet.calls) else -1, first_stop)


class StoppedLines(unittest.TestCase):

    def test_a_crash_seen_only_at_reap_is_stamped_at_its_exit_time(self):
        crash = 1600.0 - 5
        h = Harness(self, fleet={"crash": {"rust-role-viewers": crash}, "silent_crash": True})
        run = h.run()
        line = next(e for e in h.lines("stopped") if e["proc_id"] == "rust-role-viewers")
        self.assertEqual(line["wall"], crash)
        self.assertGreater(line["logged"]["wall"], h.mark("teardown_start"))
        self.assertNotEqual(run["verdict"], "PASS")
        self.assertIn("R5", h.failed())
        self.assertIn("R3", h.failed())
        self.assertIn(f"{h.uid('viewers')}: left at {crash:.3f}", h.log("collector"))

    def test_a_crash_seen_by_a_poll_is_stamped_at_its_exit_time_not_the_poll(self):
        crash = 1101.3
        h = Harness(self, fleet={"crash": {"probe-03": crash}})
        h.run()
        line = next(e for e in h.lines("stopped") if e["proc_id"] == "probe-03")
        self.assertEqual(line["wall"], crash)
        self.assertEqual(line["logged"]["wall"], 1105.0)
        self.assertIn("probe-03", h.gate("R5")["detail"])
        self.assertEqual(h.result["verdict"], "INVALID")

    def test_an_unknown_or_impossible_exit_time_writes_no_stopped_line(self):
        for exit_time in (None, float("nan"), 99999.0, 100.0, "1700"):
            with self.subTest(exit_time=exit_time):
                h = Harness(self, fleet={"exit_time": {"rust-pub": exit_time}})
                h.run()
                self.assertNotIn("rust-pub", [e["proc_id"] for e in h.lines("stopped")])
                self.assertIn("rust-pub: exit time", h.gate("R5")["detail"])
                self.assertIn("no stopped line names it", h.log("collector"))
                self.assertEqual(h.result["verdict"], "INVALID")

    def test_an_exit_time_up_to_the_slack_after_the_reap_is_kept(self):
        for exit_time, kept in ((1635.0 + cq_runner.EXIT_SLACK_S, True), (1636.25, False)):
            with self.subTest(exit_time=exit_time):
                h = Harness(self, fleet={"exit_time": {"rust-pub": exit_time}})
                h.run()
                walls = [e["wall"] for e in h.lines("stopped") if e["proc_id"] == "rust-pub"]
                self.assertEqual(walls, [exit_time] if kept else [])

    def test_a_failed_stop_writes_no_line_and_still_stops_the_rest(self):
        h = Harness(self, fleet={"stop_fails": ["rust-pub"]})
        h.run()
        self.assertEqual(len([c for c in h.fleet.calls if c[0] == "stop"]), 12)
        self.assertIn("rust-pub: stop failed", h.gate("R5")["detail"])
        self.assertEqual(h.result["verdict"], "INVALID")

    def test_a_planned_leave_exit_is_not_an_unplanned_exit(self):
        h = Harness(self, events=LEAVE_VIEWER)
        h.run()
        exit_line = next(e for e in h.lines("stopped") if e["proc_id"] == "rust-leave-00")
        self.assertLess(exit_line["wall"], h.mark("teardown_start"))
        self.assertTrue(h.gate("R5")["ok"], h.gate("R5"))
        self.assertEqual(h.result["verdict"], "PASS")

    def test_a_leaver_that_exits_with_an_error_after_its_leave_is_unplanned(self):
        for code in (137, None):
            with self.subTest(code=code):
                h = Harness(self, events=LEAVE_VIEWER, fleet={"leave_exit_code": code})
                h.run()
                self.assertFalse(h.gate("R5")["ok"])
                self.assertTrue(h.gate("R5")["detail"].startswith("rust-leave-00"), h.gate("R5")["detail"])

    def test_an_exit_at_teardown_start_is_not_an_unplanned_exit(self):
        h = Harness(self, fleet={"crash": {"probe-04": 1630.0}})
        h.run()
        self.assertEqual(h.mark("teardown_start"), 1630.0)
        self.assertEqual([e["wall"] for e in h.lines("stopped") if e["proc_id"] == "probe-04"], [1630.0])
        self.assertTrue(h.gate("R5")["ok"], h.gate("R5")["detail"])

    def test_an_exit_before_its_confirmed_leave_is_unplanned(self):
        h = Harness(self, events=LEAVE_VIEWER, fleet={"crash": {"rust-leave-00": 1299.0}, "crash_code": 0})
        h.run()
        self.assertIn("rust-leave-00", h.gate("R5")["detail"])

    def test_a_clean_exit_without_a_leave_is_unplanned(self):
        h = Harness(self, fleet={"crash": {"probe-04": 1300.0}, "crash_code": 0})
        h.run()
        self.assertEqual(h.gate("R5")["detail"], "probe-04")


class ClockFacts(unittest.TestCase):

    def test_max_abs_skew_ms_is_always_set_from_per_node(self):
        rng = random.Random(3)
        for _ in range(50):
            skews = [rng.uniform(-30000, 30000) for _ in range(rng.randint(1, 3))]
            nodes = [rng.choice(["local", "ci", "local"]) for _ in skews]
            facts = {"sync": "ntp", "max_abs_skew_ms": 0, "per_node": [
                {"node": n, "skew_ms": s} for n, s in zip(nodes, skews)]}
            block = cq_runner.clock_block(facts, "local", 900.0)
            self.assertEqual(block["max_abs_skew_ms"], max(abs(s) for s in skews))
            self.assertEqual(set(block), {"sync", "max_abs_skew_ms", "measured_at", "per_node"})

    def test_the_preflight_line_carries_max_abs_skew_ms(self):
        h = Harness(self, fleet={"facts": {"sync": "unknown", "per_node": [{"node": "local", "skew_ms": -12.5}]}})
        h.run()
        clock = h.lines("preflight")[0]["clock"]
        self.assertEqual(clock, {"sync": "unknown", "max_abs_skew_ms": 12.5, "measured_at": 900.0,
                                 "per_node": [{"node": "local", "skew_ms": -12.5}]})

    def test_only_node_and_skew_are_copied(self):
        block = cq_runner.clock_block({"sync": "ntp", "per_node": [{"node": "local", "skew_ms": 1, "host": "x9"}]},
                                      "local", 1.0)
        self.assertEqual(block["per_node"], [{"node": "local", "skew_ms": 1}])

    def test_reserved_and_environment_names_are_accepted(self):
        for node in ("local", "ci", "ci-smoke"):
            cq_runner.clock_block({"sync": "unknown", "per_node": [{"node": node, "skew_ms": 0}]}, "ci-smoke", 1.0)

    def test_a_host_name_is_refused_before_anything_starts_and_never_written(self):
        h = Harness(self, fleet={"facts": {"sync": "ntp", "per_node": [{"node": "build-host-7", "skew_ms": 0}]}})
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("a host name is never written", run["error"])
        self.assertEqual(h.stack.calls, [])
        self.assertEqual(h.fleet.calls, [])
        for root, _, files in os.walk(h.dir):
            for name in files:
                with open(os.path.join(root, name), encoding="utf-8") as fh:
                    self.assertNotIn("build-host-7", fh.read(), name)

    def test_bad_clock_facts_are_refused(self):
        for facts in ({"sync": "gps", "per_node": [{"node": "local", "skew_ms": 0}]},
                      {"sync": "ntp", "per_node": []}, {"sync": "ntp"}, None,
                      {"sync": "ntp", "per_node": [{"node": "local", "skew_ms": float("nan")}]},
                      {"sync": "ntp", "per_node": [{"node": "local", "skew_ms": True}]},
                      {"sync": "ntp", "per_node": [{"node": "local", "skew_ms": 30000.5}]},
                      {"sync": "ntp", "per_node": [{"node": "local", "skew_ms": -30001}]}):
            with self.subTest(facts=facts):
                with self.assertRaises(cq_runner.Refused):
                    cq_runner.clock_block(facts, "local", 1.0)

    def test_skew_exactly_at_the_cap_is_accepted(self):
        block = cq_runner.clock_block({"sync": "ntp", "per_node": [{"node": "local", "skew_ms": -30000}]},
                                      "local", 1.0)
        self.assertEqual(block["max_abs_skew_ms"], 30000)


class GeneratorVerdict(unittest.TestCase):
    W = (1000.0, 1600.0)
    ALLOWED = {"a", "b"}

    def verdict(self, host, ok=True, window=W):
        return {"host": host, "ok": ok, "detail": "RESOURCE_OK", "window": {"from": window[0], "to": window[1]}}

    def test_all_hosts_ok_for_this_hold_is_ok(self):
        out = cq_runner.compose_generator_verdict(["a", "b"], [self.verdict("a"), self.verdict("b")], self.W,
                                                  self.ALLOWED)
        self.assertEqual((out["ok"], out["hosts"], out["window"]), (True, ["a", "b"], {"from": 1000.0, "to": 1600.0}))

    def test_anything_missing_extra_or_off_window_is_not_ok(self):
        cases = {
            "one host starved": (["a", "b"], [self.verdict("a"), self.verdict("b", ok=False)], "b: not ok"),
            "ok is truthy, not True": (["a"], [dict(self.verdict("a"), ok=1)], "a: not ok"),
            "a host with no verdict": (["a", "b"], [self.verdict("a")], "b: no verdict for the hold"),
            "two verdicts for a host": (["a"], [self.verdict("a"), self.verdict("a")], "a: more than one verdict"),
            "an undeclared host": (["a"], [self.verdict("a"), self.verdict("b")], "names an undeclared"),
            "a host that is not a reserved name": (["a", "build-host-7"], [self.verdict("a"),
                                                                           self.verdict("build-host-7")],
                                                   "the name is not written"),
            "a verdict without a host": (["a"], [self.verdict("a"), {"ok": True}], "names no host"),
            "another window": (["a"], [self.verdict("a", window=(1000.0, 1590.0))], "window is not the hold"),
            "another start": (["a"], [self.verdict("a", window=(1000.5, 1600.0))], "window is not the hold"),
            "no window": (["a"], [{"host": "a", "ok": True}], "window is not the hold"),
            "no hosts declared": ([], [], "no load-generator host declared"),
            "verdicts not a list": (["a"], None, "names no host"),
            "hosts not a list": ("a", [self.verdict("a")], "no load-generator host declared"),
        }
        for name, (hosts, verdicts, needle) in cases.items():
            with self.subTest(name):
                out = cq_runner.compose_generator_verdict(hosts, verdicts, self.W, self.ALLOWED)
                self.assertIs(out["ok"], False)
                self.assertIn(needle, out["detail"])
                self.assertNotIn("build-host-7", json.dumps(out))

    def test_the_runner_writes_the_composed_verdict_for_the_hold(self):
        h = Harness(self, fleet={"hosts": ["local", "ci"], "verdict_ok": {"ci": False}})
        h.run()
        with open(os.path.join(h.dir, "generator-verdict.json"), encoding="utf-8") as fh:
            gv = json.load(fh)
        self.assertEqual((gv["ok"], gv["window"]), (False, {"from": 1000.0, "to": 1600.0}))
        self.assertIn("ci: not ok", gv["detail"])
        score = next(a for s, a in h.tools.calls if s == "score")
        self.assertEqual(score[score.index("--generator-verdict") + 1], os.path.join(h.dir, "generator-verdict.json"))

    def test_a_host_name_is_never_written_to_the_verdict(self):
        h = Harness(self, fleet={"hosts": ["local", "build-host-7"]})
        h.run()
        with open(os.path.join(h.dir, "generator-verdict.json"), encoding="utf-8") as fh:
            gv = json.load(fh)
        self.assertEqual((gv["ok"], gv["hosts"]), (False, ["local"]))
        for root, _, files in os.walk(h.dir):
            for name in files:
                with open(os.path.join(root, name), encoding="utf-8") as fh:
                    self.assertNotIn("build-host-7", fh.read(), name)

    def test_the_runner_asks_for_exactly_the_reached_hold(self):
        h = Harness(self, fleet={"window_shift": -0.001})
        h.run()
        with open(os.path.join(h.dir, "generator-verdict.json"), encoding="utf-8") as fh:
            self.assertFalse(json.load(fh)["ok"])


class VerdictComposition(unittest.TestCase):

    def test_the_runner_only_downgrades(self):
        names = [f"R{i}" for i in range(1, 11)]
        for scorer in ("PASS", "FAIL", "INVALID", None, "REPORT"):
            for failing in itertools.chain.from_iterable(itertools.combinations(names, k) for k in (0, 1, 2)):
                for error in (None, "boom"):
                    gates = [{"gate": n, "ok": n not in failing, "detail": ""} for n in names]
                    v = cq_runner.compose_verdict(scorer, gates, error)
                    with self.subTest(scorer=scorer, failing=failing, error=error):
                        self.assertGreaterEqual(cq_runner.RANK[v], cq_runner.RANK.get(scorer, 2))
                        self.assertEqual(v == "PASS", scorer == "PASS" and not failing and error is None)
                        if failing:
                            self.assertGreaterEqual(cq_runner.RANK[v], cq_runner.RANK["INVALID"])
                        if error:
                            self.assertEqual(v, "ERROR")

    def test_a_scorer_pass_with_any_runner_fault_is_never_pass(self):
        faults = {
            "missing join": {"fleet": {"never_join": ["X"]}},
            "early exit": {"fleet": {"crash": {"probe-04": 1300.0}}},
            "silent last-scrape crash": {"fleet": {"crash": {"rust-pub": 1592.0}, "silent_crash": True}},
            "failed event": {"events": LEAVE_PROBE, "fleet": {"event_results": {"e0": "http-500"}}},
            "timed-out event": {"events": LEAVE_VIEWER, "fleet": {"event_results": {"e0": "timeout"}}},
            "starved generator": {"fleet": {"verdict_ok": {"local": False}}},
            "missing generator verdict": {"fleet": {"hosts": ["local", "ci"], "missing_verdicts": ["ci"]}},
            "clock jump": {"jump": (1200.0, 5.0)},
            "no hold_end": {"fleet": {"interrupt_at": 1300.0}},
            "dirty tree": {"tree": {"dirty": True}, "allow_dirty": True},
            "tree changed": {"tree": {"dirty_from": 1500.0}},
            "HEAD moved": {"tree": {"commit_from": 1500.0}},
            "stack restart": {"stack": {"restart": True}},
            "extra participant": {"fleet": {"extra_joined": ["stranger@bots-app.local"]}},
            "unknown exit time": {"fleet": {"exit_time": {"probe-00": None}}},
            "stop failed": {"fleet": {"stop_fails": ["probe-01"]}},
            "confirmation from the future": {"events": LEAVE_VIEWER, "fleet": {"confirm_skew": 30.0}},
        }
        for name, f in faults.items():
            with self.subTest(name):
                fleet = dict(f.get("fleet", {}))
                plan = make_plan(events=f.get("events", "[]"))
                if fleet.get("never_join") == ["X"]:
                    fleet["never_join"] = [plan["participants"][3]["user_id"]]
                h = Harness(self, plan=plan, fleet=fleet, stack=f.get("stack"), tree=f.get("tree"),
                            allow_dirty=f.get("allow_dirty", False))
                if "jump" in f:
                    h.clock.jumps.append(f["jump"])
                run = h.run()
                self.assertNotEqual(run["verdict"], "PASS", run["gates"])
                self.assertNotEqual(run["exit_code"], 0)

    def test_a_scorer_exit_that_disagrees_with_its_result_is_an_error(self):
        h = Harness(self, tools=StubScorer(verdict="FAIL", rc=0))
        self.assertEqual(h.run()["verdict"], "ERROR")
        self.assertIn("disagrees", h.result["error"])

    def test_a_scorer_pass_listing_a_failing_gate_is_an_error(self):
        h = Harness(self, fleet={"verdict_ok": {"local": False}})
        self.assertEqual(h.run()["verdict"], "ERROR")
        self.assertIn("s1: G-V5 is ['fail']", h.result["error"])

    def test_a_scorer_pass_listing_a_failing_quality_gate_is_an_error(self):
        quality = [dict(g, status="fail") if g["gate"] == "G-Q4" else g for g in QUALITY]
        h = Harness(self, tools=StubScorer(quality=quality))
        self.assertEqual(h.run()["verdict"], "ERROR")
        self.assertEqual(h.result["error"],
                         "score: the scorer's PASS is incomplete or contradictory: s1: G-Q4 is ['fail']")

    def test_a_scorer_pass_without_every_validity_gate_passing_is_an_error(self):
        cases = {"a gate missing": {"validity": {"G-V9": None}},
                 "G-V3 not measured": {"validity": {"G-V3": "not_measured"}},
                 "G-V7 not applicable": {"validity": {"G-V7": "not_applicable"}},
                 "another step": {"steps": [{"step_id": "s2", "validity": []}]},
                 "no steps": {"steps": []},
                 "a gate twice": {"extra": [{"gate": "G-V1", "status": "pass"}]}}
        for name, kw in cases.items():
            with self.subTest(name):
                h = Harness(self, tools=StubScorer(**kw))
                self.assertEqual(h.run()["verdict"], "ERROR")
                self.assertIn("the scorer's PASS is incomplete or contradictory", h.result["error"])

    def test_a_self_contradicting_scorer_pass_is_an_error(self):
        q = {g["gate"]: g["status"] for g in QUALITY}
        cases = {"step INVALID": {"step": {"verdict": "INVALID", "invalid_reasons": ["G-Q7"]}},
                 "step INVALID without reasons": {"step": {"verdict": "INVALID"}},
                 "step PASS with invalid_reasons": {"step": {"invalid_reasons": ["G-Q7"]}},
                 "no invalid_reasons": {"step": {"invalid_reasons": None}},
                 "G-Q7 not measured": {"quality": dict(q, **{"G-Q7": "not_measured"})},
                 "G-Q1 bogus": {"quality": dict(q, **{"G-Q1": "bogus"})},
                 "G-Q8 missing": {"quality": {g: st for g, st in q.items() if g != "G-Q8"}},
                 "G-Q2 twice": {"quality": dict(q, **{"G-Q2b": "pass"})}}
        for name, kw in cases.items():
            if "quality" in kw:
                kw = {"quality": [{"gate": g[:4], "status": st} for g, st in kw["quality"].items()]}
            with self.subTest(name):
                h = Harness(self, tools=StubScorer(**kw))
                self.assertEqual((h.run()["verdict"], h.result["scorer_verdict"]), ("ERROR", None))
                self.assertIn("the scorer's PASS is incomplete or contradictory", h.result["error"])

    def test_g_q5_may_be_unmeasured_only_while_latency_is_waived(self):
        with unittest.mock.patch.object(cq_score, "latency_metric", return_value="audio_delay_ms"):
            h = Harness(self)
            self.assertEqual(h.run()["verdict"], "ERROR")
            self.assertIn("G-Q5 is ['not_measured']", h.result["error"])
            measured = [dict(g, status="pass") for g in QUALITY]
            self.assertEqual(Harness(self, tools=StubScorer(quality=measured)).run()["verdict"], "PASS")
        with config_set("latency_gate", "allowed_unmeasured", False):
            h = Harness(self)
            self.assertEqual(h.run()["verdict"], "ERROR")
            self.assertIn("G-Q5 is ['not_measured']", h.result["error"])

    def test_g_q9_may_be_disabled_only_while_the_config_disables_it(self):
        disabled = [dict(g, status="disabled") if g["gate"] == "G-Q9" else g for g in QUALITY]
        h = Harness(self, tools=StubScorer(quality=disabled))
        self.assertEqual(h.run()["verdict"], "ERROR")
        self.assertIn("G-Q9 is ['disabled']", h.result["error"])
        with config_set("quality_gates", "reconnect_gate_enabled", False):
            self.assertEqual(Harness(self, tools=StubScorer(quality=disabled)).run()["verdict"], "PASS")

    def test_g_v2_and_g_v4_measured_still_pass(self):
        h = Harness(self, tools=StubScorer(validity={"G-V2": "pass", "G-V4": "pass"}))
        self.assertEqual(h.run()["verdict"], "PASS")

    def test_a_scorer_error_exit_is_an_error(self):
        h = Harness(self, tools=StubScorer(rc=3))
        self.assertEqual(h.run()["verdict"], "ERROR")

    def test_the_scorer_fail_is_kept(self):
        h = Harness(self, tools=StubScorer(verdict="FAIL"))
        self.assertEqual((h.run()["verdict"], h.result["exit_code"], h.failed()), ("FAIL", 1, []))

    def test_validate_only_failing_on_a_collected_manifest_is_an_error(self):
        def mutate(script, args):
            return args + ["--no-such-flag"] if "--validate-only" in args else args
        h = Harness(self, tools=StubScorer(mutate=mutate))
        self.assertEqual(h.run()["verdict"], "ERROR")
        self.assertIn("a collector bug", h.result["error"])
        self.assertFalse([c for c in h.tools.calls if c[0] == "score"])


class Gates(unittest.TestCase):

    def test_r1_an_interrupt_mid_hold_tears_down_collects_a_partial_and_does_not_score(self):
        h = Harness(self, fleet={"interrupt_at": 1200.0})
        run = h.run()
        self.assertEqual((run["verdict"], run["interrupted"]), ("INVALID", True))
        self.assertIn("R1", h.failed())
        self.assertNotIn("hold_end", run["timeline"])
        self.assertTrue(os.path.exists(os.path.join(h.dir, "manifest.partial.json")))
        self.assertFalse(os.path.exists(os.path.join(h.dir, "manifest.json")))
        self.assertFalse([c for c in h.tools.calls if c[0] == "score"])
        self.assertEqual(len([c for c in h.fleet.calls if c[0] == "stop"]), 12)
        self.assertEqual(h.stack.calls[-1], ("down",))

    def test_r1_reads_the_timeline_on_disk(self):
        h = Harness(self)
        h.run()
        path = os.path.join(h.dir, cq_collect.EVENTS_FILE)
        lines = h.lines()
        with open(path, "w", encoding="utf-8") as fh:
            fh.writelines(json.dumps(dict(e, wall=1599.0) if e["type"] == "teardown_start" else e) + "\n"
                          for e in lines)
        self.assertFalse(next(g for g in h.runner.gates() if g["gate"] == "R1")["ok"])

    def test_r1_an_interrupt_after_hold_end_is_not_scored(self):
        h = Harness(self, fleet={"interrupt_at": 1610.0})
        run = h.run()
        self.assertEqual((run["verdict"], run["exit_code"], run["scorer_verdict"]), ("INVALID", 2, None))
        self.assertIn("R1", h.failed())
        self.assertIn("hold_end", run["timeline"])
        self.assertFalse([c for c in h.tools.calls if c[0] == "score"])

    def test_r2_a_participant_that_never_joins_aborts_at_the_join_budget(self):
        plan = make_plan()
        missing = plan["participants"][4]["user_id"]
        h = Harness(self, plan=plan, fleet={"never_join": [missing]})
        run = h.run()
        self.assertEqual(run["verdict"], "INVALID")
        self.assertIn(f"not joined: {missing}", run["aborted"])
        budget = 940.0 + plan["steps"][0]["join_window_s"] + cq_runner.JOIN_BUDGET_EXTRA_S
        self.assertEqual(h.mark("teardown_start"), budget)
        self.assertIn("R2", h.failed())
        self.assertTrue(os.path.exists(os.path.join(h.dir, "manifest.partial.json")))
        self.assertFalse([c for c in h.tools.calls if c[0] == "score"])

    def test_r2_a_rust_participant_without_media_has_not_joined(self):
        h = Harness(self, fleet={"media_delay": 10_000.0})
        run = h.run()
        self.assertIn("not joined", run["aborted"])
        self.assertNotIn(h.uid("talkers"), [u for e in h.lines("joined") for u in e["participants"]])

    def test_r2_an_extra_participant_fails(self):
        h = Harness(self, fleet={"extra_joined": ["stranger@bots-app.local"]})
        h.run()
        self.assertFalse(h.gate("R2")["ok"])
        self.assertIn("extra ['stranger@bots-app.local']", h.gate("R2")["detail"])

    def test_a_process_exit_during_the_join_aborts(self):
        h = Harness(self, fleet={"crash": {"probe-02": 945.0}})
        run = h.run()
        self.assertIn("exited during the join: probe-02", run["aborted"])
        self.assertLess(h.mark("teardown_start"), 960.0)

    def test_r4_a_manifest_changed_before_scoring_fails(self):
        def mutate(script, args):
            if "--prom-url" in args:
                with open(args[args.index("--manifest") + 1], "a", encoding="utf-8") as fh:
                    fh.write(" ")
            return args
        h = Harness(self, tools=StubScorer(mutate=mutate))
        h.run()
        self.assertEqual(h.failed(), ["R4"])
        self.assertEqual(h.result["verdict"], "INVALID")

    def test_r6_a_failed_event_is_written_once_and_fails(self):
        h = Harness(self, events=LEAVE_PROBE, fleet={"event_results": {"e0": "http-500"}})
        h.run()
        lines = h.lines("event")
        self.assertEqual([(e["result"], e["t_confirmed"]) for e in lines], [("http-500", None)])
        self.assertIn("R6", h.failed())
        self.assertIn("was not confirmed", h.log("collector"))

    def test_r6_a_confirmation_outside_issue_and_reply_is_unconfirmed(self):
        for skew in (30.0, -5.0, -0.504):
            with self.subTest(skew=skew):
                h = Harness(self, events=LEAVE_VIEWER, fleet={"confirm_skew": skew})
                h.run()
                self.assertTrue(h.lines("event")[0]["result"].startswith("unconfirmed"))
                self.assertIn("R6", h.failed())

    def test_r6_an_adapter_error_on_an_event_is_a_failed_event(self):
        h = Harness(self, events=LEAVE_VIEWER)

        def boom(ev):
            raise cq_runner.AdapterError("control API unreachable")
        h.fleet.apply = boom
        h.run()
        self.assertEqual(h.lines("event")[0]["result"], "error: control API unreachable")
        self.assertIn("R6", h.failed())

    def test_r7_a_restarted_stack_container_fails(self):
        h = Harness(self, stack={"restart": True})
        h.run()
        self.assertEqual(h.failed(), ["R7"])

    def test_r8_a_clock_jump_fails_and_small_drift_passes(self):
        for delta, ok in ((5.0, False), (-1.5, False), (0.9, True)):
            with self.subTest(delta=delta):
                h = Harness(self)
                h.clock.jumps.append((1200.0, delta))
                h.run()
                self.assertEqual(h.gate("R8")["ok"], ok, h.gate("R8"))

    def test_r9_a_dirty_tree_is_refused_without_allow_dirty(self):
        h = Harness(self, tree={"dirty": True})
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertTrue(run["error"].startswith("preflight: the tree is dirty"), run["error"])
        self.assertIn("--allow-dirty", run["error"])
        self.assertEqual((h.stack.calls, h.fleet.calls), ([], []))

    def test_r9_a_dirty_tree_caps_a_scorer_pass_at_invalid(self):
        h = Harness(self, tree={"dirty": True}, allow_dirty=True)
        run = h.run()
        self.assertEqual((run["verdict"], run["scorer_verdict"], h.failed()), ("INVALID", "PASS", ["R9"]))
        self.assertTrue(h.lines("preflight")[0]["dirty"])

    def test_r9_a_tree_dirty_at_preflight_and_clean_later_still_fails(self):
        h = Harness(self, tree={"dirty": True, "clean_from": 1300.0}, allow_dirty=True)
        h.run()
        self.assertEqual(h.failed(), ["R9"])

    def test_r9_a_tree_that_changes_or_moves_during_the_run_fails(self):
        for tree in ({"dirty_from": 1300.0}, {"commit_from": 1300.0}):
            with self.subTest(tree=tree):
                h = Harness(self, tree=tree)
                h.run()
                self.assertEqual(h.failed(), ["R9"])

    def test_r10_a_scorer_that_ignored_the_forced_config_fails(self):
        h = Harness(self, tools=StubScorer(drop_config=True))
        h.run()
        self.assertEqual(h.failed(), ["R10"])

    def test_r10_g_v5_not_measured_fails(self):
        h = Harness(self, tools=StubScorer(gv5="not_measured"))
        h.run()
        self.assertEqual(h.failed(), ["R10"])

    def test_r10_a_g_v5_that_contradicts_the_generator_verdict_fails(self):
        for fleet, gv5 in (({"verdict_ok": {"local": False}}, "pass"), ({}, "fail")):
            with self.subTest(gv5=gv5):
                h = Harness(self, fleet=fleet, tools=StubScorer(gv5=gv5))
                h.run()
                self.assertEqual(h.failed(), ["R10"])

    def test_r10_each_echo_alone_is_checked(self):
        for kw in ({"bad_sha": True}, {"bad_overrides": True}):
            with self.subTest(**kw):
                h = Harness(self, tools=StubScorer(**kw))
                h.run()
                self.assertEqual(h.failed(), ["R10"])

    def test_r10_a_result_without_steps_fails(self):
        h = Harness(self, tools=StubScorer(verdict="FAIL", steps=[]))
        self.assertEqual((h.run()["verdict"], h.failed()), ("INVALID", ["R10"]))

    def test_r7_an_empty_stack_state_at_both_ends_fails(self):
        h = Harness(self)
        h.stack.state = dict
        h.run()
        self.assertEqual(h.failed(), ["R7"])

    def test_r5_a_launched_process_with_no_exit_fails(self):
        h = Harness(self)
        h.run()
        del h.runner.s.exits["probe-04"]
        r5 = next(g for g in h.runner.gates() if g["gate"] == "R5")
        self.assertEqual((r5["ok"], r5["detail"]), (False, "probe-04: never stopped"))

    def test_a_wall_step_between_two_events_does_not_delay_the_second(self):
        h = Harness(self, events="[{at: hold+1m, select: {role: probe-ref, count: 1}, action: unmute}, "
                                 "{at: hold+2m, select: {role: probe-ref, count: 1}, action: mute}]")
        h.clock.jumps.append((1060.2, -3600.0))
        h.run()
        s = h.runner.s
        self.assertEqual(s.issued_mono["e1"] - s.hold_mono["hold_start"], 120.0)
        self.assertTrue(h.gate("R6")["ok"], h.gate("R6"))

    def test_r6_an_event_issued_late_fails(self):
        for delay, ok in ((4.0, True), (299.0, False)):
            with self.subTest(delay=delay):
                h = Harness(self, events="[{at: hold+1m, select: {role: probe-ref, count: 1}, action: unmute}, "
                                         "{at: hold+62s, select: {role: probe-ref, count: 1}, action: mute}]")
                apply = h.fleet.apply

                def slow(ev, apply=apply, h=h, delay=delay):
                    if ev["event_id"] == "e0":
                        h.clock.advance(delay)
                    return apply(ev)
                h.fleet.apply = slow
                h.run()
                self.assertEqual(h.gate("R6")["ok"], ok, h.gate("R6"))
                if not ok:
                    self.assertIn("e1 issued at hold+359.5 s, planned hold+62 s", h.gate("R6")["detail"])

    def test_r6_an_event_after_the_last_two_scrapes_fails_whatever_the_plan_says(self):
        plan = make_plan(events=LEAVE_PROBE, probes=11)
        plan["events"][0]["at_offset_s"] = plan["steps"][0]["hold_s"] - 29
        h = Harness(self, plan=plan)
        h.run()
        self.assertIn("e0 issued at hold+571.0 s", h.gate("R6")["detail"])
        self.assertFalse(h.gate("R6")["ok"])

    def test_r1_a_hold_stretched_by_a_slow_event_fails(self):
        h = Harness(self, events=LEAVE_PROBE, probes=11)
        apply = h.fleet.apply

        def slow(ev):
            h.clock.advance(330.0)
            return apply(ev)
        h.fleet.apply = slow
        h.run()
        self.assertIn("R1", h.failed())
        self.assertIn("hold 630.5 s on the monotonic clock, planned 600 s + 5 s", h.gate("R1")["detail"])

    def test_r1_a_hold_within_the_slack_passes(self):
        h = Harness(self, events="[{at: hold+565s, select: {role: probe-ref, count: 1}, action: unmute}]")
        apply = h.fleet.apply

        def slow(ev):
            h.clock.advance(39.0)
            return apply(ev)
        h.fleet.apply = slow
        h.run()
        self.assertTrue(h.gate("R1")["ok"], h.gate("R1"))

    def test_a_small_backward_step_inside_an_event_call_is_confirmed_no_earlier_than_issued(self):
        for when in ("before the reply is stamped", "after the reply is stamped"):
            with self.subTest(when):
                h = Harness(self, events=LEAVE_VIEWER, fleet={"confirm_delay": 0.004})
                apply = h.fleet.apply

                def stepped(ev, apply=apply, h=h, when=when):
                    if when.startswith("before"):
                        h.clock.wall -= 0.008
                        return apply(ev)
                    out = apply(ev)
                    h.clock.wall -= 0.008
                    return out
                h.fleet.apply = stepped
                run = h.run()
                line = h.lines("event")[0]
                self.assertEqual(line["result"], "ok", line)
                self.assertGreaterEqual(line["t_confirmed"], line["t_issued"])
                self.assertEqual(line["t_confirmed"], line["t_issued"] + (0 if when.startswith("before") else 0.004))
                self.assertEqual(run["verdict"], "PASS", run["gates"])

    def test_the_forced_config_is_strict_and_loads(self):
        h = Harness(self, stack={"selector": 'up{job=~"relay-.*"}'})
        h.run()
        path = os.path.join(h.dir, "score", "config.json")
        cfg = cq_score.load_config(path)
        self.assertEqual(cq_score.cv(cfg, "validity_gates", "allowed_not_measured"), ["G-V2", "G-V4"])
        self.assertEqual(cq_score.cv(cfg, "validity_gates", "scrape_up_selector"), 'up{job=~"relay-.*"}')
        score = next(a for s, a in h.tools.calls if s == "score")
        self.assertEqual(score[score.index("--config") + 1], path)


def report_joins(h, uid, fn):
    joined = h.fleet.joined

    def wrapped():
        out = joined()
        j = fn(out.get(uid)) if h.fleet.launched else None
        if j is not None:
            out[uid] = j
        return out
    h.fleet.joined = wrapped


class JoinTimes(unittest.TestCase):

    def test_a_join_or_media_time_in_milliseconds_aborts_the_join(self):
        for field, k in (("join_ts", 0), ("media_started_at", -1)):
            with self.subTest(field):
                h = Harness(self)
                h.clock.max_sleeps = 5000
                uid = h.plan["participants"][k]["user_id"]
                report_joins(h, uid, lambda j: j and fakes.Joined(
                    **dict(vars(j), **{field: getattr(j, field) * 1000 if getattr(j, field) else None})))
                run = h.run()
                self.assertEqual((run["verdict"], run["error"]), ("INVALID", None))
                self.assertRegex(run["aborted"], f"^{uid}: {field} [0-9.e+]+ is later than the runner's clock$")
                self.assertEqual(sorted(c[1] for c in h.fleet.calls if c[0] == "stop"),
                                 sorted(p["proc_id"] for p in h.plan["processes"]))
                self.assertEqual(h.stack.calls[-1], ("down",))

    def test_a_join_time_up_to_the_slack_ahead_is_kept(self):
        for ahead, verdict in ((31.0, "PASS"), (31.5, "INVALID")):
            with self.subTest(ahead=ahead):
                h = Harness(self)
                uid = h.plan["participants"][0]["user_id"]
                report_joins(h, uid, lambda j: fakes.Joined(h.clock.wall + ahead))
                self.assertEqual(h.run()["verdict"], verdict, h.result["aborted"])
                self.assertEqual("later than the runner's clock" in (h.result["aborted"] or ""), verdict == "INVALID")

    def test_a_join_without_a_time_is_ignored_until_it_has_one(self):
        h = Harness(self)
        uid = [p["user_id"] for p in h.plan["participants"] if p["fleet"] == "browser"][-1]
        report_joins(h, uid, lambda j: j or fakes.Joined(None))
        run = h.run()
        self.assertEqual((run["verdict"], run["error"]), ("PASS", None))
        line = next(e for e in h.lines("joined") if e["participants"] == [uid])
        self.assertTrue(cq_runner._num(line["join_ts"]))


class AbortPaths(unittest.TestCase):

    def test_a_failing_stack_state_at_teardown_still_stops_and_collects(self):
        h = Harness(self)
        states = iter([{"relay-ws": {"restart_count": 0}}])

        def state():
            try:
                return next(states)
            except StopIteration:
                raise cq_runner.AdapterError("docker inspect failed") from None
        h.stack.state = state
        run = h.run()
        self.assertEqual(len([c for c in h.fleet.calls if c[0] == "stop"]), 12)
        self.assertIn(("collect",), h.fleet.calls)
        self.assertEqual((run["verdict"], h.failed(), run["teardown_problems"]),
                         ("INVALID", ["R7"], ["stack state: docker inspect failed"]))

    def test_a_failing_tree_at_teardown_still_collects_and_scores(self):
        h = Harness(self, tree={"commit_from": 1630.0})
        h.tree.commit = lambda: (_ for _ in ()).throw(cq_runner.AdapterError("git gone")) \
            if h.clock.wall >= 1630.0 else fakes.COMMIT
        run = h.run()
        self.assertEqual((run["verdict"], run["scorer_verdict"], h.failed()), ("INVALID", "PASS", ["R9"]))

    def test_a_backward_wall_step_does_not_stretch_the_hold(self):
        for events in ("[]", LEAVE_VIEWER):
            with self.subTest(events=events):
                h = Harness(self, events=events)
                h.clock.jumps.append((1200.0, -3600.0))
                h.run()
                mono = {e["type"]: e["mono"] for e in h.lines() if e["type"] in ("hold_start", "hold_end")}
                self.assertEqual(mono["hold_end"] - mono["hold_start"], 600.0)
                self.assertLess(h.clock.mono - 50.0, 900.0)
                self.assertIn("R8", h.failed())

    def test_a_stack_that_fails_to_start_is_an_error_with_no_bots(self):
        h = Harness(self, stack={"fail_up": True})
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("compose up exited 1", run["error"])
        self.assertEqual(h.fleet.calls, [])
        self.assertEqual(h.stack.calls, [("up", "up"), ("down",)])

    def test_stack_preflight_problems_are_refused_before_up(self):
        h = Harness(self, stack={"problems": ["port 3001 busy"]})
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("port 3001 busy", run["error"])
        self.assertEqual(h.stack.calls, [])

    def test_a_missing_scrape_up_selector_is_refused(self):
        for selector in (None, "", "  "):
            with self.subTest(selector=selector):
                h = Harness(self, stack={"selector": selector})
                self.assertEqual(h.run()["verdict"], "ERROR")
                self.assertEqual(h.stack.calls, [])

    def test_a_reused_or_kept_stack_is_left_running(self):
        for kw in ({"stack_mode": "reuse"}, {"keep_stack": True}):
            with self.subTest(**kw):
                h = Harness(self, **kw)
                h.run()
                self.assertNotIn(("down",), h.stack.calls)

    def test_any_exception_from_one_stop_still_stops_every_other_process(self):
        for exc in (RuntimeError("boom"), OSError("docker gone"), KeyboardInterrupt()):
            with self.subTest(exc=type(exc).__name__):
                h = Harness(self)
                stop = h.fleet.stop

                def first_fails(proc, exc=exc, stop=stop):
                    if proc["proc_id"] == "rust-pub":
                        h.fleet.calls.append(("stop", "rust-pub"))
                        raise exc
                    return stop(proc)
                h.fleet.stop = first_fails
                run = h.run()
                self.assertEqual(len([c for c in h.fleet.calls if c[0] == "stop"]), 12)
                self.assertIn(("collect",), h.fleet.calls)
                self.assertIn("rust-pub: stop failed", h.gate("R5")["detail"])
                self.assertEqual(run["interrupted"], isinstance(exc, KeyboardInterrupt))
                self.assertEqual(h.stack.calls[-1], ("down",))

    def test_a_launch_that_fails_after_starting_is_still_stopped(self):
        h = Harness(self)
        launch = h.fleet.launch

        def fails(proc):
            launch(proc)
            if proc["proc_id"] == "probe-03":
                raise cq_runner.AdapterError("healthz timed out")
        h.fleet.launch = fails
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertEqual([c[1] for c in h.fleet.calls if c[0] == "stop"], [f"probe-0{k}" for k in range(4)])

    def test_a_failing_down_still_writes_run_json(self):
        h = Harness(self)

        def down():
            raise RuntimeError("compose down hung")
        h.stack.down = down
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("down: RuntimeError: compose down hung", run["error"])
        with open(os.path.join(h.dir, "run.json"), encoding="utf-8") as fh:
            self.assertEqual(json.load(fh)["verdict"], "ERROR")

    def test_an_internal_error_still_tears_down(self):
        h = Harness(self)

        def boom():
            raise ZeroDivisionError("bug")
        h.runner.hold_start_target = boom
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("internal error ZeroDivisionError", run["error"])
        self.assertEqual(len([c for c in h.fleet.calls if c[0] == "stop"]), 12)
        self.assertFalse([c for c in h.tools.calls if c[0] == "score"])


def stops(h):
    return [c[1] for c in h.fleet.calls if c[0] == "stop"]


def enospc():
    return OSError(errno.ENOSPC, "No space left on device", "/home/someone/runs/x")


class TeardownUnderFaults(unittest.TestCase):
    EXCEPTIONS = (lambda: enospc(), lambda: RuntimeError("docker SDK bug"), lambda: KeyboardInterrupt())

    def check(self, h, run):
        self.assertEqual(sorted(stops(h)), sorted(p["proc_id"] for p in h.plan["processes"]))
        self.assertIn(("collect",), h.fleet.calls)
        self.assertEqual(h.stack.calls[-1], ("down",))
        self.assertNotEqual(run["verdict"], "PASS")
        with open(os.path.join(h.dir, "run.json"), encoding="utf-8") as fh:
            self.assertEqual(json.load(fh)["verdict"], run["verdict"])

    def test_an_events_write_that_fails_in_teardown_still_stops_every_process(self):
        for kind in ("teardown_start", "stopped"):
            for make in self.EXCEPTIONS:
                exc = make()
                with self.subTest(kind=kind, exc=type(exc).__name__):
                    h = Harness(self)
                    log_write = cq_runner.EventLog.write

                    def failing(log, k, wall=None, kind=kind, exc=exc, **f):
                        if k == kind:
                            raise exc
                        return log_write(log, k, wall=wall, **f)
                    with unittest.mock.patch.object(cq_runner.EventLog, "write", failing):
                        run = h.run()
                    self.check(h, run)
                    self.assertTrue(any(p.startswith(("teardown_start line:", "stopped line for"))
                                        for p in run["teardown_problems"]), run["teardown_problems"])
                    self.assertNotIn("/home/someone", json.dumps(run))
                    self.assertEqual(run["interrupted"], isinstance(exc, KeyboardInterrupt))

    def test_any_exception_from_a_teardown_adapter_call_still_stops_every_process(self):
        for target in ("stack state", "git HEAD", "git status"):
            for make in self.EXCEPTIONS:
                exc = make()
                with self.subTest(target=target, exc=type(exc).__name__):
                    h = Harness(self)
                    obj, name = {"stack state": (h.stack, "state"), "git HEAD": (h.tree, "commit"),
                                 "git status": (h.tree, "dirty")}[target]
                    fn = getattr(obj, name)

                    def failing(fn=fn, exc=exc, h=h):
                        if h.runner.s.phase == "teardown":
                            raise exc
                        return fn()
                    setattr(obj, name, failing)
                    run = h.run()
                    self.check(h, run)
                    self.assertTrue(run["teardown_problems"][0].startswith(target + ":"), run["teardown_problems"])
                    self.assertIn("R7" if target == "stack state" else "R9", h.failed())

    def test_a_stop_that_returns_another_process_exit_still_stops_that_process(self):
        h = Harness(self)
        stop = h.fleet.stop

        def crossed(proc):
            out = stop(proc)
            return cq_runner.Exit("rust-role-viewers", out.exited_at, 0) if proc["proc_id"] == "rust-pub" else out
        h.fleet.stop = crossed
        run = h.run()
        self.check(h, run)
        self.assertIn("rust-pub: stop failed (it returned the exit of 'rust-role-viewers')", h.gate("R5")["detail"])
        self.assertEqual(run["teardown_problems"],
                         ["stop rust-pub: stop failed (it returned the exit of 'rust-role-viewers')"])

    def test_a_run_json_that_cannot_be_written_is_an_error(self):
        h = Harness(self)
        dump = cq_runner._dump

        def failing(path, obj):
            if path.endswith("run.json"):
                raise enospc()
            return dump(path, obj)
        with unittest.mock.patch.object(cq_runner, "_dump", failing):
            run = h.run()
        self.assertEqual((run["verdict"], run["exit_code"]), ("ERROR", 3))
        self.assertEqual(run["error"], "finish: run.json not written (OSError: No space left on device)")

    def test_a_done_line_or_commands_file_that_cannot_be_written_is_an_error(self):
        for what in ("done", "commands"):
            with self.subTest(what):
                h = Harness(self)
                if what == "done":
                    log_write = cq_runner.EventLog.write

                    def failing(log, k, wall=None, **f):
                        if k == "done":
                            raise enospc()
                        return log_write(log, k, wall=wall, **f)
                    patch = unittest.mock.patch.object(cq_runner.EventLog, "write", failing)
                else:
                    patch = unittest.mock.patch.object(cq_runner.Runner, "_commands", side_effect=enospc())
                with patch:
                    run = h.run()
                with open(os.path.join(h.dir, "run.json"), encoding="utf-8") as fh:
                    self.assertEqual((json.load(fh)["verdict"], run["exit_code"]), ("ERROR", 3))

    def test_no_os_error_path_reaches_run_json(self):
        def hold_end(h):
            log_write = cq_runner.EventLog.write

            def failing(log, k, wall=None, **f):
                if k == "hold_end":
                    raise OSError(errno.ENOSPC, "No space left on device", log.path)
                return log_write(log, k, wall=wall, **f)
            return unittest.mock.patch.object(cq_runner.EventLog, "write", failing)

        def adapter(obj, name, when):
            def prep(h):
                fn = getattr(obj(h), name)

                def failing(*a):
                    if when(*a):
                        raise OSError(errno.EACCES, "Permission denied", "/home/someone/.docker/config.json")
                    return fn(*a)
                setattr(obj(h), name, failing)
                return contextlib.nullcontext()
            return prep

        def events_broken(how):
            def prep(h):
                down = h.stack.down

                def broken():
                    path = os.path.join(h.dir, cq_collect.EVENTS_FILE)
                    if how == "directory":
                        os.remove(path)
                        os.mkdir(path)
                    else:
                        with open(path, "a", encoding="utf-8") as fh:
                            fh.write("{not json\n")
                    return down()
                h.stack.down = broken
                return contextlib.nullcontext()
            return prep
        cases = {"hold_end line": (hold_end, "hold: internal error OSError: No space left on device"),
                 "stop": (adapter(lambda h: h.fleet, "stop", lambda p: p["proc_id"] == "rust-pub"), None),
                 "down": (adapter(lambda h: h.stack, "down", lambda: True), "down: PermissionError: Permission denied"),
                 "events.jsonl a directory": (events_broken("directory"), "gates: internal error collector error:\n  "
                                              "events.jsonl: [Errno 21] Is a directory: 'events.jsonl'"),
                 "events.jsonl a bad line": (events_broken("line"), "gates: internal error collector error:\n  "
                                             "events.jsonl:")}
        for name, (prep, error) in cases.items():
            with self.subTest(name):
                h = Harness(self)
                with prep(h):
                    run = h.run()
                with open(os.path.join(h.dir, "run.json"), encoding="utf-8") as fh:
                    text = fh.read()
                self.assertNotIn("/home/someone", text)
                self.assertNotIn(h.dir, text)
                self.assertNotEqual(run["verdict"], "PASS")
                if error:
                    self.assertTrue(run["error"].startswith(error), run["error"])
                else:
                    self.assertIn("rust-pub: stop failed (PermissionError: Permission denied)", h.gate("R5")["detail"])

    def test_a_collector_error_names_the_run_folder_as_run_in_its_log(self):
        h = Harness(self)
        collect = h.fleet.collect

        def broken(run_dir):
            collect(run_dir)
            with open(os.path.join(run_dir, "rust", "participants-0.json"), "w", encoding="utf-8") as fh:
                fh.write("{")
        h.fleet.collect = broken
        self.assertNotEqual(h.run()["verdict"], "PASS")
        self.assertNotIn(h.dir, h.log("collector"))
        self.assertIn("$RUN/rust/participants-0.json: Expecting property name", h.log("collector"))

    def test_a_tool_traceback_names_the_checkout_as_scripts_quality_in_its_log(self):
        class Traceback(StubScorer):
            def run(self, argv):
                rc, out, err = super().run(argv)
                return rc, out, err + f'  File "{HERE}/cq_collect.py", line 1\n'
        h = Harness(self, tools=Traceback())
        h.run()
        self.assertNotIn(HERE, h.log("collector"))
        self.assertIn('File "scripts/quality/cq_collect.py", line 1', h.log("collector"))

    def test_r10_names_no_path_when_the_verdict_file_is_gone(self):
        h = Harness(self)
        h.run()
        os.remove(os.path.join(h.dir, "generator-verdict.json"))
        r10 = next(g for g in h.runner.gates() if g["gate"] == "R10")
        self.assertEqual((r10["ok"], r10["detail"]), (False, "cannot read the forced config or the generator "
                                                            "verdict: FileNotFoundError: No such file or directory"))

    def test_a_tool_that_times_out_or_cannot_start_logs_no_argv_url_or_path(self):
        argv = [sys.executable, "x.py", "--prom-url", "http://u:secret@prom.example:9090", "--out-dir", "/home/u/r"]
        for exc, want in ((subprocess.TimeoutExpired(argv, 900), "TimeoutExpired: no exit after 900 s"),
                          (FileNotFoundError(errno.ENOENT, "No such file or directory", "/home/u/py"),
                           "FileNotFoundError: No such file or directory")):
            with self.subTest(type(exc).__name__):
                with unittest.mock.patch.object(cq_runner.subprocess, "run", side_effect=exc):
                    self.assertEqual(cq_runner.SubprocessTools().run(argv), (3, "", want))

    def test_a_git_call_that_hangs_is_an_adapter_error(self):
        hang = subprocess.TimeoutExpired("git", 1)
        with unittest.mock.patch.object(cq_runner.subprocess, "run", side_effect=hang), \
                self.assertRaisesRegex(cq_runner.AdapterError, "git rev-parse HEAD timed out"):
            cq_runner.GitTree("/nonexistent").commit()

    def test_a_plan_with_two_processes_of_one_id_is_refused_before_anything_starts(self):
        plan = make_plan()
        plan["processes"][-1]["proc_id"] = plan["processes"][-2]["proc_id"]
        h = Harness(self, plan=plan)
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("two processes one proc_id", run["error"])
        self.assertEqual((h.stack.calls, h.fleet.calls), ([], []))


class Signals(unittest.TestCase):

    def signal_at(self, h, obj, name, when=lambda *a: True, signum=signal.SIGTERM, times=1):
        fn, fired = getattr(obj, name), []

        def wrapped(*args):
            if len(fired) < times and when(*args):
                fired.append(1)
                h.runner.on_signal(signum)
            return fn(*args)
        setattr(obj, name, wrapped)
        return fired

    def check_unscored(self, h, run, fired):
        self.assertEqual(fired, [1])
        self.assertEqual(sorted(stops(h)), sorted(p["proc_id"] for p in h.plan["processes"]))
        self.assertIn(("collect",), h.fleet.calls)
        self.assertIn("collected", [e["type"] for e in h.lines()])
        self.assertFalse([c for c in h.tools.calls if c[0] == "score"])
        self.assertEqual((run["verdict"], run["exit_code"], run["interrupted"], run["scorer_verdict"]),
                         ("INVALID", 2, True, None))
        self.assertIn("R1", h.failed())
        self.assertEqual(h.stack.calls[-1], ("down",))

    def test_a_signal_after_collect_and_before_scoring_starts_is_never_scored(self):
        h = Harness(self)
        guarded = h.runner._guarded

        def signal_then(fn, mode):
            if fn == h.runner._score:
                h.runner.on_signal(signal.SIGTERM)
            return guarded(fn, mode)
        h.runner._guarded = signal_then
        run = h.run()
        self.assertEqual((run["verdict"], run["scorer_verdict"], run["signals"]), ("INVALID", None, ["SIGTERM"]))
        self.assertEqual([c[0] for c in h.tools.calls], ["cq_collect.py"])

    def test_a_signal_before_the_run_starts_is_recorded_and_nothing_starts(self):
        h = Harness(self)
        try:
            h.runner.on_signal(signal.SIGTERM)
        except KeyboardInterrupt:
            self.fail("a signal before the run raised out of on_signal")
        run = h.run()
        self.assertEqual((run["verdict"], run["exit_code"], run["interrupted"], run["signals"]),
                         ("INVALID", 2, True, ["SIGTERM"]))
        self.assertEqual((h.stack.calls, h.fleet.calls), ([], []))
        self.assertTrue(os.path.exists(os.path.join(h.dir, "run.json")))

    def test_a_signal_mid_hold_raises_and_the_run_is_torn_down_unscored(self):
        h = Harness(self)
        fired = self.signal_at(h, h.fleet, "poll", lambda: h.clock.wall >= 1200.0)
        run = h.run()
        self.check_unscored(h, run, fired)
        self.assertNotIn("hold_end", run["timeline"])
        self.assertEqual(run["signals"], ["SIGTERM"])

    def test_a_signal_in_the_settle_out_after_hold_end_is_not_scored(self):
        h = Harness(self)
        fired = self.signal_at(h, h.fleet, "poll", lambda: h.clock.wall >= 1605.0)
        run = h.run()
        self.check_unscored(h, run, fired)
        self.assertIn("hold_end", run["timeline"])
        self.assertTrue(os.path.exists(os.path.join(h.dir, "manifest.json")))

    def test_a_signal_anywhere_in_teardown_is_recorded_and_every_process_is_stopped(self):
        points = {"teardown stack state": lambda h: (h.stack, "state", lambda: h.runner.s.phase == "teardown"),
                  "first stop": lambda h: (h.fleet, "stop", lambda proc: True),
                  "between two stops": lambda h: (h.fleet, "stop", lambda proc: proc["proc_id"] == "probe-03"),
                  "git HEAD": lambda h: (h.tree, "commit", lambda: h.runner.s.phase == "teardown"),
                  "git status": lambda h: (h.tree, "dirty", lambda: h.runner.s.phase == "teardown"),
                  "first stop after an adapter's KeyboardInterrupt": lambda h: (h.fleet, "stop", lambda proc: True)}
        for name, point in points.items():
            for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
                with self.subTest(name, signal=signum.name):
                    h = Harness(self, fleet={"interrupt_at": 1200.0} if "adapter" in name else None)
                    obj, attr, when = point(h)
                    fired = self.signal_at(h, obj, attr, when, signum)
                    run = h.run()
                    self.check_unscored(h, run, fired)
                    self.assertEqual(run["signals"], [signum.name])
                    self.assertEqual(run["teardown_problems"], [])
                    self.assertEqual(len(h.lines("stopped")), len(h.plan["processes"]))

    def test_a_signal_during_collect_is_recorded_and_collect_finishes(self):
        h = Harness(self)
        fired = self.signal_at(h, h.fleet, "collect")
        run = h.run()
        self.check_unscored(h, run, fired)
        self.assertEqual(run["collector_exit"], cq_collect.EXIT_OK)

    def test_a_signal_while_scoring_raises_and_the_scorer_never_runs(self):
        h = Harness(self)
        fired = self.signal_at(h, h.tools, "run", lambda argv: "--validate-only" in argv)
        run = h.run()
        self.check_unscored(h, run, fired)

    def test_a_signal_while_a_tool_runs_ends_the_tool_at_once(self):
        h = Harness(self)
        self.addCleanup(signal.setitimer, signal.ITIMER_REAL, 0)
        self.addCleanup(signal.signal, signal.SIGALRM, signal.signal(signal.SIGALRM, h.runner.on_signal))
        h.runner._signal_mode = "raise"
        signal.setitimer(signal.ITIMER_REAL, 0.2)
        start = time.monotonic()
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", ResourceWarning)
            with self.assertRaises(KeyboardInterrupt) as cm:
                cq_runner.SubprocessTools().run([sys.executable, "-c", "import time; time.sleep(60)"])
            del cm
            gc.collect()
        self.assertLess(time.monotonic() - start, 10)
        self.assertEqual(h.runner.s.signals, ["SIGALRM"])

    def test_a_signal_while_the_scored_line_is_written_leaves_the_run_unscored(self):
        h = Harness(self)
        log_write = cq_runner.EventLog.write

        def signalled(log, k, wall=None, **f):
            if k == "scored":
                h.runner.on_signal(signal.SIGTERM)
            return log_write(log, k, wall=wall, **f)
        with unittest.mock.patch.object(cq_runner.EventLog, "write", signalled):
            run = h.run()
        self.assertEqual((run["scorer_verdict"], run["verdict"], run["exit_code"]), (None, "INVALID", 2))
        self.assertIn("R1", h.failed())

    def test_a_signal_after_scoring_is_listed_and_caps_the_verdict_only_before_the_final_gates(self):
        for where, verdict in (("down", "INVALID"), ("commands.txt", "PASS")):
            with self.subTest(where):
                h = Harness(self)
                obj, name = (h.stack, "down") if where == "down" else (h.runner, "_commands")
                fired = self.signal_at(h, obj, name)
                run = h.run()
                self.assertEqual(fired, [1])
                self.assertEqual((run["scorer_verdict"], run["verdict"], run["signals"]),
                                 ("PASS", verdict, ["SIGTERM"]))
                self.assertEqual("R1" in h.failed(), verdict == "INVALID")

    def test_a_terminal_signal_to_the_process_group_never_reaches_a_tool(self):
        script = "\n".join([
            "import os, signal, sys, threading, time",
            "sys.path.insert(0, sys.argv[1])",
            "import cq_runner, test_scenario_run",
            "runner = cq_runner.Runner(test_scenario_run.make_plan(), b'', '/nonexistent', stack=None, fleet=None,"
            " tree=None)",
            "runner._signal_mode = 'record'",
            "signal.signal(signal.SIGINT, runner.on_signal)",
            "signal.signal(signal.SIGHUP, runner.on_signal)",
            "def terminal():",
            "    time.sleep(0.5)",
            "    os.killpg(0, signal.SIGINT)",
            "    os.killpg(0, signal.SIGHUP)",
            "threading.Thread(target=terminal, daemon=True).start()",
            "rc, out, err = cq_runner.SubprocessTools().run(",
            "    [sys.executable, '-c', 'import time; time.sleep(3); print(\"tool done\")'])",
            "print(rc, out.strip(), sorted(runner.s.signals))"])
        p = subprocess.run([sys.executable, "-c", script, HERE], capture_output=True, text=True, timeout=60,
                           start_new_session=True)
        self.assertEqual(p.stdout.strip(), "0 tool done ['SIGHUP', 'SIGINT']", p.stderr)

    def test_every_child_the_runner_starts_gets_its_own_session_and_a_timeout(self):
        seen = []
        real = subprocess.run

        def run(*a, **kw):
            seen.append((kw.get("start_new_session"), kw.get("timeout")))
            return real(*a, **kw)
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        with unittest.mock.patch.object(cq_runner.subprocess, "run", run):
            cq_runner.SubprocessTools().run([sys.executable, "-c", "pass"])
            with self.assertRaises(cq_runner.AdapterError):
                cq_runner.GitTree(tmp).commit()
        self.assertEqual(seen, [(True, cq_runner.TOOL_TIMEOUT_S)] * 2)

    def test_a_signal_in_finish_still_writes_run_json_and_caps_the_verdict(self):
        for where in ("before R1", "after R1"):
            with self.subTest(where):
                h = Harness(self)
                target = "gates" if where == "before R1" else "_strict_config_echoed"
                fn, fired = getattr(cq_runner.Runner, target), []

                def wrapped(runner, fn=fn, fired=fired):
                    if not fired:
                        fired.append(1)
                        runner.on_signal(signal.SIGTERM)
                    return fn(runner)
                with unittest.mock.patch.object(cq_runner.Runner, target, wrapped):
                    run = h.run()
                self.assertEqual(fired, [1])
                with open(os.path.join(h.dir, "run.json"), encoding="utf-8") as fh:
                    on_disk = json.load(fh)
                self.assertEqual((on_disk["verdict"], on_disk["interrupted"], run["exit_code"]), ("INVALID", True, 2))
                self.assertIn("R1", h.failed())


class Expectation(unittest.TestCase):
    OK = [{"gate": "R1", "ok": True, "detail": ""}]

    def result(self, failing, statuses=None, verdict="INVALID", invalid_reasons=()):
        validity = [{"gate": g, "status": "fail"} for g in failing]
        validity += [{"gate": g, "status": s} for g, s in (statuses or {"G-V1": "pass"}).items()]
        return {"verdict": verdict, "steps": [{"headline": True, "validity": validity,
                                               "invalid_reasons": list(invalid_reasons)}]}

    def test_met_only_when_the_failing_set_matches_exactly(self):
        expect = {"verdict": "INVALID", "invalid_gates": ["G-V7"]}
        self.assertEqual(cq_runner.check_expectation(expect, self.result(["G-V7"]), self.OK),
                         {"met": True, "problems": []})
        for failing in ([], ["G-V7", "G-V11"], ["G-V11"]):
            with self.subTest(failing=failing):
                self.assertFalse(cq_runner.check_expectation(expect, self.result(failing), self.OK)["met"])

    def test_not_met_for_another_verdict_an_unmeasured_gate_a_runner_gate_or_no_result(self):
        expect = {"verdict": "INVALID", "invalid_gates": ["G-V7"]}
        cases = [(self.result(["G-V7"], verdict="PASS"), self.OK),
                 (self.result(["G-V7"], {"G-V3": "not_measured"}), self.OK),
                 (self.result(["G-V7"], invalid_reasons=["G-V3"]), self.OK),
                 (self.result(["G-V7"]), [{"gate": "R5", "ok": False, "detail": "x"}]),
                 (None, self.OK)]
        for result, gates in cases:
            self.assertFalse(cq_runner.check_expectation(expect, result, gates)["met"])
        self.assertTrue(cq_runner.check_expectation(expect, self.result(["G-V7"], {"G-V2": "not_measured"}),
                                                    self.OK)["met"])

    def test_not_met_when_the_run_ends_in_error(self):
        h = Harness(self, expect="{verdict: INVALID, invalid_gates: [G-V7]}",
                    tools=StubScorer(verdict="INVALID", validity={"G-V7": "fail"}))

        def down():
            raise cq_runner.AdapterError("compose down exited 1")
        h.stack.down = down
        run = h.run()
        self.assertEqual((run["verdict"], run["error"]), ("ERROR", "down: compose down exited 1"))
        self.assertEqual(run["expectation"], {"met": False, "problems": ["run error: down: compose down exited 1"]})
        expect = {"verdict": "INVALID", "invalid_gates": ["G-V7"]}
        self.assertEqual(cq_runner.check_expectation(expect, self.result(["G-V7"]), self.OK, "down: x"),
                         {"met": False, "problems": ["run error: down: x"]})

    def test_no_expectation_is_none_in_run_json(self):
        h = Harness(self)
        self.assertIsNone(h.run()["expectation"])


class RunFolder(unittest.TestCase):

    def setUp(self):
        real = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, real)
        self.tmp = os.path.join(real, "link")
        os.symlink(real, self.tmp)
        self.repo = os.path.join(self.tmp, "repo")
        os.makedirs(self.repo)

    def test_an_out_dir_inside_the_repo_is_refused(self):
        for out in (self.repo, os.path.join(self.repo, "runs")):
            with self.subTest(out=out):
                with self.assertRaisesRegex(cq_runner.Refused, "outside the repository"):
                    cq_runner.prepare_run_dir(out, RUN_ID, self.repo)
        self.assertEqual(os.listdir(self.repo), [])

    def test_a_sibling_with_the_repo_as_a_name_prefix_is_outside(self):
        run_dir = cq_runner.prepare_run_dir(self.repo + "-runs", RUN_ID, self.repo)
        self.assertEqual(run_dir, os.path.join(os.path.realpath(self.repo + "-runs"), RUN_ID))

    def test_an_existing_run_folder_is_never_reused(self):
        out = os.path.join(self.tmp, "runs")
        cq_runner.prepare_run_dir(out, RUN_ID, self.repo)
        with self.assertRaisesRegex(cq_runner.Refused, "already exists"):
            cq_runner.prepare_run_dir(out, RUN_ID, self.repo)


class GitTreeAdapter(unittest.TestCase):

    def test_clean_dirty_and_untracked(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@bots-app.local", GIT_COMMITTER_NAME="t",
                   GIT_COMMITTER_EMAIL="t@bots-app.local")
        for cmd in (["init", "-q"], ["commit", "-q", "--allow-empty", "-m", "x"]):
            subprocess.run(["git", "-C", tmp, *cmd], check=True, env=env, capture_output=True)
        tree = cq_runner.GitTree(tmp)
        self.assertEqual(os.path.realpath(tree.toplevel()), os.path.realpath(tmp))
        self.assertRegex(tree.commit(), r"^[0-9a-f]{40}$")
        self.assertFalse(tree.dirty())
        with open(os.path.join(tmp, "new.txt"), "w", encoding="utf-8") as fh:
            fh.write("x")
        self.assertTrue(tree.dirty())

    def test_a_missing_git_is_an_adapter_error(self):
        empty = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, empty)
        with unittest.mock.patch.dict(os.environ, {"PATH": empty}), \
                self.assertRaisesRegex(cq_runner.AdapterError, "^git rev-parse HEAD: FileNotFoundError: No such file"):
            cq_runner.GitTree(empty).commit()

    def test_git_stderr_reaches_the_error_without_its_paths(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        with self.assertRaises(cq_runner.AdapterError) as cm:
            cq_runner.GitTree(os.path.join(tmp, "gone")).commit()
        self.assertNotIn(tmp, str(cm.exception))
        self.assertIn("<path>", str(cm.exception))
        self.assertEqual(cq_runner._no_paths("fatal: detected dubious ownership in repository at '/home/u/my repo'\n"
                                             "\tgit config --global --add safe.directory /home/u/repo"),
                         "fatal: detected dubious ownership in repository at <path>\n"
                         "\tgit config --global --add safe.directory <path>")

    def test_not_a_repository_is_an_adapter_error(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        with self.assertRaises(cq_runner.AdapterError):
            cq_runner.GitTree(tmp).commit()


class Cli(unittest.TestCase):

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp)
        self.repo = os.path.join(self.tmp, "repo")
        os.makedirs(self.repo)
        self.scenario = os.path.join(self.repo, "scenarios", "t.yaml")
        os.makedirs(os.path.dirname(self.scenario))
        with open(self.scenario, "w", encoding="utf-8") as fh:
            fh.write(scenario_text())
        self.clock = fakes.FakeClock(wall=1791288000.0)
        self.tree = fakes.FakeTree(self.clock, self.repo)

    def main(self, *argv, adapters=None, tools=None):
        err, out = io.StringIO(), io.StringIO()
        try:
            with contextlib.redirect_stderr(err), contextlib.redirect_stdout(out):
                rc = scenario_run.main(["--scenario", self.scenario, *argv], adapters=adapters, tree=self.tree,
                                       clock=self.clock, tools=tools, environ={})
        except KeyboardInterrupt:
            self.fail("KeyboardInterrupt escaped scenario_run.main")
        return rc, out.getvalue(), err.getvalue()

    def test_compile_only_prints_the_plan(self):
        rc, out, _ = self.main("--compile-only")
        plan = json.loads(out)
        self.assertEqual((rc, plan["run_id"], plan["scenario"]["file"]), (0, RUN_ID, os.path.join("scenarios", "t.yaml")))
        with open(self.scenario, "rb") as fh:
            self.assertEqual(plan["scenario"]["sha256"], hashlib.sha256(fh.read()).hexdigest())

    def test_a_scenario_outside_the_repo_is_labelled_without_its_path(self):
        self.assertEqual(scenario_run.scenario_label("/elsewhere/dir/x.yaml", self.repo), "external:x.yaml")

    def test_refusals_and_usage_errors_exit_3(self):
        with open(self.scenario, "a", encoding="utf-8") as fh:
            fh.write("ramp: []\n")
        self.assertEqual(self.main("--compile-only")[0], 3)
        self.assertEqual(self.main()[0], 3)
        err = io.StringIO()
        with contextlib.redirect_stderr(err), self.assertRaises(SystemExit) as cm:
            scenario_run.main(["--stack", "maybe"])
        self.assertEqual(cm.exception.code, 3)

    def test_a_target_mismatch_or_no_adapters_exit_3(self):
        self.assertEqual(self.main("--target", "ci")[0], 3)
        rc, _, err = self.main("--out-dir", os.path.join(self.tmp, "runs"))
        self.assertEqual(rc, 3)
        self.assertIn("no adapters for target local", err)
        self.assertFalse(os.path.exists(os.path.join(self.tmp, "runs")))

    def test_an_out_dir_inside_the_repo_exits_3(self):
        rc, _, err = self.main("--out-dir", os.path.join(self.repo, "runs"), adapters={"local": self.factory})
        self.assertEqual(rc, 3)
        self.assertIn("outside the repository", err)

    def factory(self, plan):
        self.clock.wall, self.clock.mono = 900.0, 50.0
        self.fleet = fakes.FakeFleet(self.clock, plan)
        return fakes.FakeStack(self.clock), self.fleet

    def test_a_run_with_injected_adapters_returns_the_verdict_exit_code(self):
        sigs = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
        before = [signal.getsignal(s) for s in sigs]
        seen = {}

        def factory(plan):
            stack, fleet = self.factory(plan)
            launch = fleet.launch

            def look(proc):
                seen.update({s: signal.getsignal(s) for s in sigs})
                return launch(proc)
            fleet.launch = look
            return stack, fleet
        rc, _, err = self.main("--out-dir", os.path.join(self.tmp, "runs"), adapters={"local": factory},
                               tools=StubScorer())
        self.assertEqual(rc, 0, err)
        self.assertIn(f"{RUN_ID}: PASS", err)
        self.assertEqual({s: getattr(h, "__name__", None) for s, h in seen.items()},
                         {s: "on_signal" for s in sigs})
        self.assertEqual([signal.getsignal(s) for s in sigs], before)

    def test_a_run_json_that_cannot_be_written_exits_3(self):
        dump = cq_runner._dump

        def failing(path, obj):
            if path.endswith("run.json"):
                raise OSError(errno.ENOSPC, "No space left on device")
            return dump(path, obj)
        with unittest.mock.patch.object(cq_runner, "_dump", failing):
            rc, _, err = self.main("--out-dir", os.path.join(self.tmp, "runs"), adapters={"local": self.factory},
                                   tools=StubScorer())
        self.assertEqual(rc, 3)
        self.assertIn("ERROR", err)
        self.assertIn("run.json not written", err)
        with open(os.path.join(self.tmp, "runs", RUN_ID, "scenario.yaml"), encoding="utf-8") as fh:
            self.assertEqual(fh.read(), scenario_text())

    def test_sigterm_mid_hold_tears_down_and_exits_invalid(self):
        self.addCleanup(signal.signal, signal.SIGTERM, signal.signal(signal.SIGTERM, lambda *a: None))

        def factory(plan):
            stack, fleet = self.factory(plan)
            poll = fleet.poll

            def poll_and_signal():
                if self.clock.wall >= 1300.0 and not getattr(fleet, "signalled", False):
                    fleet.signalled = True
                    os.kill(os.getpid(), signal.SIGTERM)
                return poll()
            fleet.poll = poll_and_signal
            self.stack = stack
            return stack, fleet
        rc, _, err = self.main("--out-dir", os.path.join(self.tmp, "runs"), adapters={"local": factory},
                               tools=StubScorer())
        self.assertEqual(rc, 2, err)
        self.assertEqual(len([c for c in self.fleet.calls if c[0] == "stop"]), 12)
        self.assertEqual(self.stack.calls[-1], ("down",))
        with open(os.path.join(self.tmp, "runs", RUN_ID, "run.json"), encoding="utf-8") as fh:
            self.assertTrue(json.load(fh)["interrupted"])

    def test_a_malformed_scenario_value_exits_3_not_a_traceback(self):
        for old, new in (("camera: on", "camera: [on]"), ("target: local", "target: [local]"),
                         ("events: []", "events: [{at: hold+1m, select: {role: [p], count: 1}, action: leave}]"),
                         ("events: []", "events: [{at: hold+1m, select: {role: viewers, count: 1}, action: [x]}]")):
            with self.subTest(new=new):
                with open(self.scenario, "w", encoding="utf-8") as fh:
                    fh.write(scenario_text().replace(old, new, 1))
                rc, _, err = self.main("--compile-only")
                self.assertEqual(rc, 3)
                self.assertIn("wrong shape", err)

    def test_a_missing_git_exits_3(self):
        empty = os.path.join(self.tmp, "bin")
        os.makedirs(empty)
        err = io.StringIO()
        with unittest.mock.patch.dict(os.environ, {"PATH": empty}), contextlib.redirect_stderr(err):
            rc = scenario_run.main(["--scenario", self.scenario, "--compile-only"], clock=self.clock, environ={})
        self.assertEqual(rc, 3)
        self.assertIn("FileNotFoundError: No such file or directory", err.getvalue())

    def test_a_deeply_nested_scenario_exits_3(self):
        with open(self.scenario, "w", encoding="utf-8") as fh:
            fh.write("a: " + "[" * 20000 + "]" * 20000 + "\n")
        rc, _, err = self.main("--compile-only")
        self.assertEqual(rc, 3)
        self.assertIn("nested too deeply", err)

    def test_an_unexpected_exception_exits_3(self):
        def boom():
            raise RuntimeError("boom")
        self.tree.commit = boom
        rc, _, err = self.main("--compile-only")
        self.assertEqual((rc, err), (3, "internal error: RuntimeError: boom\n"))

    def test_no_run_folder_file_names_a_user_path(self):
        outside = os.path.join(self.tmp, "elsewhere", "t.yaml")
        os.makedirs(os.path.dirname(outside))
        shutil.copy(self.scenario, outside)
        runs = os.path.join(self.tmp, "runs")
        for k, (scenario, flags, line) in enumerate((
                (self.scenario, ["--out-dir", runs],
                 "scripts/quality/scenario_run.py --scenario scenarios/t.yaml --out-dir '$RUN/..'"),
                (outside, ["--out-dir=" + runs, "--allow-dirty"],
                 "scripts/quality/scenario_run.py --scenario '$RUN/scenario.yaml' '--out-dir=$RUN/..' --allow-dirty"))):
            with self.subTest(scenario=scenario):
                self.scenario, self.clock.wall = scenario, 1791288000.0 + 60 * k
                rc, _, err = self.main(*flags, adapters={"local": self.factory}, tools=StubScorer())
                self.assertEqual(rc, 0, err)
                run_dir = os.path.join(runs, sorted(os.listdir(runs))[-1])
                with open(os.path.join(run_dir, "commands.txt"), encoding="utf-8") as fh:
                    self.assertEqual(fh.read().splitlines()[0], line)
                for base, _, files in os.walk(run_dir):
                    for name in files:
                        with open(os.path.join(base, name), encoding="utf-8") as fh:
                            self.assertNotIn(self.tmp, fh.read(), name)

    def test_the_default_out_dir_is_outside_the_checkout(self):
        self.assertEqual(scenario_run.default_out_dir({"CQ_RUNS_DIR": "/srv/runs"}), "/srv/runs")
        self.assertNotEqual(os.path.commonpath([scenario_run.default_out_dir({}), os.path.dirname(HERE)]),
                            os.path.dirname(HERE))


if __name__ == "__main__":
    unittest.main()
