#!/usr/bin/env python3
"""Tests for the shared-target hooks (#2914 PR-8a): target hcl-daily, report mode, R11, R12, stack.watch(), load-a."""

import contextlib
import io
import json
import math
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cq_collect  # noqa: E402
import cq_runner  # noqa: E402
import cq_runner_fakes as fakes  # noqa: E402
import cq_scenario  # noqa: E402
import scenario_run  # noqa: E402
import test_scenario_run as tsr  # noqa: E402

SHARED = "hcl-daily"
LOAD_HOST = {"facts": {"sync": "ntp", "per_node": [{"node": "load-a", "skew_ms": 3}]}, "hosts": ["load-a"], "leaks": (),
             "admit": ()}
RUST_ONLY = '''\
schema: scale-scenario/v1
run: {{id_prefix: cqtest, seed: 7, target: {target}, fidelity_waivers: []}}
population:
  - {{role: talkers, fleet: rust, count: 1, media: {{camera: on, mic: continuous}}}}
  - {{role: viewers, fleet: rust, count: 2, media: {{camera: off, mic: off}}}}
steps: [{{id: s1, hold: 10m, headline: true}}]
events: []
scoring: {{mode: {mode}, relay_path_exemptions: {{}}, expect: null}}
'''


class Recording:
    def __init__(self, inner):
        self.inner, self.used = inner, []

    def __getattr__(self, name):
        attr = getattr(self.inner, name)
        if not callable(attr):
            return attr

        def call(*args, **kw):
            self.used.append(name)
            return attr(*args, **kw)
        return call


def mutes(start, every, count):
    acts = [f"{{at: hold+{start + every * k}s, select: {{role: probe-ref, count: 1}}, "
            f"action: {('unmute', 'mute')[k % 2]}}}" for k in range(count)]
    return "[" + ", ".join(acts) + "]"


def slow_poll(h, seconds):
    poll = h.fleet.poll

    def slow():
        h.clock.advance(seconds)
        return poll()
    h.fleet.poll = slow


def unwatched(h):
    marks = {e["type"]: e["mono"] for e in h.lines() if e["type"] in ("join_start", "teardown_start")}
    ends = [marks["join_start"]] + h.stack.watch_ends
    starts = [m for _, m in h.stack.watched] + [marks["teardown_start"]]
    return max(b - a for a, b in zip(ends, starts))


def shared_text(target=SHARED, mode="report", **kw):
    return tsr.scenario_text(**kw).replace("target: local", f"target: {target}").replace("mode: gate", f"mode: {mode}")


def compile_text(text):
    return cq_scenario.compile_scenario(cq_scenario.parse_yaml(text), run_id=tsr.RUN_ID,
                                        scenario_file="external:t.yaml", scenario_sha256="0" * 64)


def refusal(tc, text):
    with tc.assertRaises(cq_scenario.ScenarioError) as cm:
        compile_text(text)
    return "\n".join(cm.exception.errors)


def harness(tc, text=None, stack=None, stack_cls=fakes.FakeSharedStack, fleet=None, tools=None, stack_mode="reuse",
            wrap=lambda x: x):
    h = tsr.Harness(tc, plan=compile_text(text or shared_text()), fleet=dict(LOAD_HOST, **(fleet or {})),
                    tools=tools, stack_mode=stack_mode)
    h.stack = stack_cls(h.clock, **(stack or {}))
    h.runner = cq_runner.Runner(h.plan, b"scenario", h.dir, stack=wrap(h.stack), fleet=wrap(h.fleet), tree=h.tree,
                                clock=h.clock, tools=h.tools, stack_mode=stack_mode)
    return h


class Compile(unittest.TestCase):

    def test_hcl_daily_compiles_as_a_shared_report_mode_plan(self):
        plan = compile_text(shared_text())
        self.assertEqual((plan["target"], plan["environment"], plan["scoring"]["mode"]), (SHARED, SHARED, "report"))
        self.assertIn(SHARED, cq_scenario.SHARED_TARGETS)
        self.assertEqual(cq_collect._check_plan(plan), [])

    def test_report_mode_is_accepted_exactly_on_shared_targets(self):
        for target in cq_scenario.TARGETS:
            for mode in ("gate", "report", "audit"):
                text = shared_text(target=target, mode=mode)
                ok = mode == ("report" if target in cq_scenario.SHARED_TARGETS else "gate")
                with self.subTest(target=target, mode=mode):
                    if ok:
                        self.assertEqual(compile_text(text)["scoring"]["mode"], mode)
                    else:
                        self.assertIn("scoring.mode:", refusal(self, text))

    def test_gate_on_hcl_daily_is_refused_also_by_default(self):
        self.assertIn("only report is allowed", refusal(self, shared_text(mode="gate")))
        doc = cq_scenario.parse_yaml(shared_text())
        del doc["scoring"]
        with self.assertRaises(cq_scenario.ScenarioError) as cm:
            cq_scenario.compile_scenario(doc, run_id=tsr.RUN_ID, scenario_file="x", scenario_sha256="0" * 64)
        self.assertIn("only report is allowed", "\n".join(cm.exception.errors))

    def test_an_unknown_target_lists_the_targets_and_nothing_else(self):
        errors = refusal(self, shared_text(target="k8s")).splitlines()
        self.assertIn("run.target: must be one of local, ci, hcl-daily", errors)

    def test_an_expectation_is_refused_on_a_shared_target(self):
        expect = "{verdict: INVALID, invalid_gates: [G-V7]}"
        self.assertIn("scoring.expect: a shared target is report-only", refusal(self, shared_text(expect=expect)))
        self.assertEqual(compile_text(tsr.scenario_text(expect=expect))["scoring"]["expect"]["verdict"], "INVALID")

    def test_a_shared_target_with_no_probe_is_refused(self):
        self.assertIn("a 0-probe run is refused", refusal(self, RUST_ONLY.format(target=SHARED, mode="report")))
        self.assertEqual(compile_text(RUST_ONLY.format(target="local", mode="gate"))["target"], "local")

    def test_a_shared_target_whose_probes_all_leave_is_refused(self):
        leave = "[{{at: hold+5m, select: {{role: probe-ref, count: {}}}, action: leave}}]"
        self.assertIn("a 0-probe run is refused", refusal(self, shared_text(probes=2, events=leave.format(2))))
        self.assertEqual(len(compile_text(shared_text(probes=2, events=leave.format(1)))["events"]), 1)
        self.assertEqual(len(compile_text(tsr.scenario_text(probes=2, events=leave.format(2)))["events"]), 1)


class ReportOnly(unittest.TestCase):

    def test_a_shared_run_whose_scorer_says_pass_ends_invalid_through_r11(self):
        h = harness(self)
        run = h.run()
        self.assertEqual((run["scorer_verdict"], run["verdict"], run["exit_code"], h.failed(), run["error"]),
                         ("PASS", "INVALID", 2, ["R11"], None))
        self.assertEqual(h.gate("R11")["detail"], "Phase 0 is report-only on a shared stack")
        self.assertEqual((h.gate("R12")["ok"], h.gate("R12")["detail"]), (True, "the room drained"))
        self.assertNotIn(("down",), h.stack.calls)

    def test_r11_caps_every_scorer_verdict_at_invalid(self):
        for verdict in cq_runner.SCORER_EXIT:
            with self.subTest(verdict=verdict):
                h = harness(self, tools=tsr.StubScorer(verdict=verdict))
                run = h.run()
                self.assertEqual((run["scorer_verdict"], run["verdict"]), (verdict, "INVALID"))
                self.assertFalse(h.gate("R11")["ok"])

    def test_a_local_run_has_neither_r11_nor_r12(self):
        h = tsr.Harness(self)
        h.run()
        self.assertEqual([g["gate"] for g in h.result["gates"]], [f"R{k}" for k in range(1, 11)])
        self.assertNotIn("watch", h.result)

    def test_a_shared_target_refuses_a_stack_without_watch_or_drained(self):
        h = harness(self, stack_cls=fakes.FakeStack, wrap=Recording)
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("its stack adapter has no watch(); its stack adapter has no drained()", run["error"])
        self.assertEqual((h.runner.stack.used, h.runner.fleet.used), ([], ["leaks", "leaks"]))
        self.assertEqual([g["gate"] for g in run["gates"] if not g["ok"]][-2:], ["R11", "R12"])


    def test_a_shared_target_refuses_a_stack_mode_other_than_reuse(self):
        h = harness(self, stack_mode="up", wrap=Recording)
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("is shared: it needs --stack reuse, not up", run["error"])
        self.assertEqual((h.runner.stack.used, h.runner.fleet.used), ([], ["leaks", "leaks"]))


class Admission(unittest.TestCase):
    REFUSED = "host 1.00 + probe 3.00 cores > 5.30"

    def test_each_probe_is_admitted_just_before_its_launch_once_rust_media_started(self):
        h = harness(self)
        run = h.run()
        probes = [p["proc_id"] for p in h.plan["processes"] if p["fleet"] == "browser"]
        self.assertEqual([c for c in h.fleet.calls if c[0] in ("launch", "admit")],
                         [("launch", "rust-pub"), ("launch", "rust-role-viewers")]
                         + [c for p in probes for c in (("admit", p), ("launch", p))])
        self.assertEqual([(a["proc_id"], a["admitted"]) for a in run["admission"]], [(p, True) for p in probes])
        media = max(e["media_started_at"] for e in h.lines("joined") if e["media_started_at"])
        self.assertGreaterEqual(h.lines("admission")[0]["wall"], media)

    def test_a_refused_probe_is_left_out_and_recorded_and_the_run_goes_on(self):
        h = harness(self, fleet={"admit": ["probe-03"]})
        uid = h.uid("probe-ref", 3)
        n = len(h.plan["participants"])
        run = h.run()
        self.assertNotIn(("launch", "probe-03"), h.fleet.calls)
        self.assertEqual([a for a in run["admission"] if not a["admitted"]],
                         [{"proc_id": "probe-03", "admitted": False, "detail": self.REFUSED}])
        self.assertEqual([(e["proc_id"], e["admitted"]) for e in h.lines("admission") if not e["admitted"]],
                         [("probe-03", False)])
        with open(os.path.join(h.dir, cq_collect.PLAN_FILE), encoding="utf-8") as fh:
            plan = json.load(fh)
        self.assertNotIn(uid, [p["user_id"] for p in plan["participants"]])
        self.assertNotIn("probe-03", [p["proc_id"] for p in plan["processes"]])
        self.assertEqual(plan["steps"][0]["n_target"], n - 1)
        self.assertEqual(plan["refused"], [{"proc_id": "probe-03", "participants": [uid], "detail": self.REFUSED}])
        self.assertEqual((run["aborted"], run["error"], run["scorer_verdict"], h.failed()),
                         (None, None, "PASS", ["R11"]))

    def test_a_refused_owner_probe_aborts_before_any_probe_launch(self):
        h = harness(self, fleet={"admit": ["probe-00"]})
        run = h.run()
        self.assertEqual(run["aborted"], f"probe-00 was refused admission ({self.REFUSED}), and it owns the meeting, "
                                         "so no later probe could join")
        self.assertFalse([c for c in h.fleet.calls if c[0] == "launch" and c[1].startswith("probe")])

    def test_a_refused_probe_that_an_event_acts_on_aborts(self):
        h = harness(self, text=shared_text(events=mutes(60, 60, 2)), fleet={"admit": ["probe-09"]})
        run = h.run()
        self.assertIn("probe-09 was refused admission", run["aborted"])
        self.assertIn("a scenario event acts on it", run["aborted"])

    def test_slow_refusals_still_get_their_watch_rounds(self):
        refused = [f"probe-{k:02d}" for k in range(1, 16)]
        h = harness(self, text=shared_text(probes=16), fleet={"admit": refused}, stack={"watch_s": 1.0})
        admit = h.fleet.admit

        def slow(proc):
            h.clock.advance(5.0)
            return admit(proc)
        h.fleet.admit = slow
        run = h.run()
        self.assertEqual(len([a for a in run["admission"] if not a["admitted"]]), 15)
        self.assertLessEqual(unwatched(h), cq_runner.WATCH_STARVE_S + 5.0 + 1e-6)

    def test_a_malformed_admission_answer_is_an_error_and_launches_nothing_more(self):
        for answer in (True, (1, "x"), (True, " "), [True, "x"], (True, "x", 1)):
            with self.subTest(answer=answer):
                h = harness(self)
                h.fleet.admit = lambda proc, a=answer: a
                run = h.run()
                self.assertEqual(run["verdict"], "ERROR")
                self.assertIn("fleet admit() must return (admitted, a non-blank detail)", run["error"])
                self.assertFalse([c for c in h.fleet.calls if c[0] == "launch" and c[1].startswith("probe")])

    def test_a_shared_target_refuses_a_fleet_without_admit(self):
        h = harness(self, fleet={"admit": None})
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("its fleet adapter has no admit()", run["error"])
        self.assertNotIn("admission", run)


class Drained(unittest.TestCase):

    def test_drain_problems_fail_r12(self):
        h = harness(self, stack={"drain": ["room scale-x still has 2 sessions after 60 s"]})
        h.run()
        self.assertEqual(h.failed(), ["R11", "R12"])
        self.assertEqual(h.gate("R12")["detail"], "room scale-x still has 2 sessions after 60 s")

    def test_a_failing_or_malformed_drain_check_fails_r12(self):
        for drain in (cq_runner.AdapterError("prometheus timed out"), None, [3], [""], [" "]):
            with self.subTest(drain=drain):
                h = harness(self, stack={"drain": drain})
                h.run()
                self.assertFalse(h.gate("R12")["ok"])
                self.assertTrue(any(p.startswith("drain check: ") for p in h.result["teardown_problems"]))

    def test_the_drain_check_runs_once_after_every_stop(self):
        h = harness(self)
        seen = []
        h.stack.drained = lambda: seen.append(sorted(c[1] for c in h.fleet.calls if c[0] == "stop")) or []
        h.run()
        self.assertEqual(seen, [sorted(p["proc_id"] for p in h.plan["processes"])])
        self.assertTrue(h.gate("R12")["ok"])

    def test_a_local_stack_with_drain_data_gets_r12(self):
        h = tsr.Harness(self)
        h.stack = fakes.FakeSharedStack(h.clock, drain=["not drained"])
        h.runner = cq_runner.Runner(h.plan, b"scenario", h.dir, stack=h.stack, fleet=h.fleet, tree=h.tree,
                                    clock=h.clock, tools=h.tools)
        h.run()
        self.assertEqual((h.failed(), h.gate("R12")["detail"]), (["R12"], "not drained"))


class Watch(unittest.TestCase):

    def test_a_watch_reason_mid_hold_aborts_tears_down_and_fails_r1(self):
        h = harness(self, stack={"abort_at": 1300.0})
        run = h.run()
        self.assertEqual((run["verdict"], run["error"]), ("INVALID", None))
        self.assertEqual(run["aborted"], "stack watch: a real meeting started")
        self.assertIn("R1", h.failed())
        self.assertEqual([e["type"] for e in h.lines() if e["type"] in ("hold_start", "hold_end", "teardown_start")],
                         ["hold_start", "teardown_start"])
        teardown = h.mark("teardown_start")
        self.assertGreaterEqual(teardown, 1300.0)
        self.assertEqual(sorted(c[1] for c in h.fleet.calls if c[0] == "stop"), sorted(h.runner.s.launched))
        self.assertLessEqual(max(w for w, _ in h.stack.watched), teardown)
        self.assertTrue(0 <= h.stack.watched[-1][0] - 1300.0 < 15.0, h.stack.watched[-1])

    def test_a_watch_reason_during_the_join_aborts_before_the_hold(self):
        h = harness(self, stack={"abort_at": 941.0})
        run = h.run()
        self.assertTrue(run["aborted"].startswith("stack watch: "))
        self.assertTrue({"R1", "R2"} <= set(h.failed()), h.failed())
        self.assertLess(len(h.lines("joined")), len(h.plan["participants"]))
        self.assertLessEqual(h.mark("teardown_start"), 940.0 + cq_runner.WATCH_INTERVAL_S)
        self.assertEqual(h.lines("hold_start"), [])

    def test_the_watch_runs_once_per_15_s_and_no_more(self):
        h = harness(self)
        h.run()
        mono = [m for _, m in h.stack.watched]
        gaps = [b - a for a, b in zip(mono, mono[1:])]
        self.assertTrue(gaps and min(gaps) >= 15.0, gaps)
        hs, he = h.runner.s.hold_mono["hold_start"], h.runner.s.hold_mono["hold_end"]
        held = [b - a for a, b in zip(mono, mono[1:]) if hs <= a and b <= he]
        self.assertEqual(sorted(set(held)), [15.0])
        self.assertGreaterEqual(len(held), (he - hs) / 15 - 2)

    def test_a_watch_that_returns_no_list_is_an_error_and_still_tears_down(self):
        for reasons in (None, "x", [1], [""], ["  "]):
            with self.subTest(reasons=reasons):
                h = harness(self)
                h.stack.watch = lambda r=reasons: r
                run = h.run()
                self.assertEqual(run["verdict"], "ERROR")
                self.assertIn("watch() must return a list of non-blank strings", run["error"])
                self.assertTrue(any(c[0] == "stop" for c in h.fleet.calls))


    def test_a_watch_slower_than_the_slack_delays_events_and_hold_end_by_at_most_its_overrun(self):
        for watch_s in (4.0, 5.5, 6.0, 8.0):
            with self.subTest(watch_s=watch_s):
                h = harness(self, text=shared_text(events=tsr.LEAVE_VIEWER), stack={"watch_s": watch_s})
                h.run()
                self.assertEqual(h.failed(), ["R11"])
                hs, he = h.runner.s.hold_mono["hold_start"], h.runner.s.hold_mono["hold_end"]
                late = (he - hs - 600.0, h.runner.s.issued_mono["e0"] - hs - 300.0)
                self.assertTrue(all(0 <= x <= watch_s - 2.0 for x in late), late)
                self.assertGreater(len([m for _, m in h.stack.watched if hs <= m <= he]), 20)

    def test_a_normal_round_after_a_slow_poll_delays_a_target_by_at_most_w_minus_2_plus_p(self):
        for watch_s in (2.5, 4.0, 6.0):
            for poll in (0.3, 0.9):
                with self.subTest(watch_s=watch_s, poll=poll):
                    h = harness(self, text=shared_text(events=tsr.LEAVE_VIEWER), stack={"watch_s": watch_s})
                    slow_poll(h, poll)
                    h.run()
                    hs, he = h.runner.s.hold_mono["hold_start"], h.runner.s.hold_mono["hold_end"]
                    late = (he - hs - 600.0, h.runner.s.issued_mono["e0"] - hs - 300.0)
                    self.assertTrue(all(0 <= x <= max(poll, watch_s - 2.0 + poll) + 1e-6 for x in late), late)
                    self.assertEqual(h.failed(), ["R11"])

    def test_the_watch_interval_runs_from_the_end_of_a_round(self):
        h = harness(self, stack={"watch_s": 16.0})
        h.run()
        starts = [m for _, m in h.stack.watched]
        self.assertGreater(len(starts), 10)
        self.assertGreaterEqual(min(b - a for a, b in zip(h.stack.watch_ends, starts[1:])), 15.0)

    def test_a_watch_within_its_budget_does_not_stretch_the_probe_stagger(self):
        for stagger, gap, watched in (("1900ms", 1.9, False), ("1999ms", 1.999, False), ("2005ms", 2.005, True)):
            with self.subTest(stagger=stagger):
                h = harness(self, text=shared_text(probes=8, stagger=stagger), stack={"watch_s": 2.0})
                h.run()
                probes = sorted(v["launched_at"] for k, v in h.runner.s.launched.items() if k.startswith("probe-"))
                self.assertEqual({round(b - a, 6) for a, b in zip(probes, probes[1:])}, {gap})
                self.assertEqual(any(probes[0] <= w < probes[-1] for w, _ in h.stack.watched), watched)

    def test_a_stack_whose_watch_is_not_callable_is_not_watched(self):
        h = tsr.Harness(self)
        h.stack.watch = 0
        self.assertEqual(h.run()["verdict"], "PASS")


    def test_events_6_s_apart_still_get_a_watch_about_every_15_s(self):
        h = harness(self, text=shared_text(hold="4m", events=mutes(15, 6, 32)))
        h.run()
        self.assertEqual(h.failed(), ["R11"])
        hs, he = h.runner.s.hold_mono["hold_start"], h.runner.s.hold_mono["hold_end"]
        held = [m for _, m in h.stack.watched if hs + 15 <= m <= hs + 201]
        self.assertGreaterEqual(len(held), 10)
        self.assertLessEqual(max(b - a for a, b in zip(held, held[1:])), 15.0 + 6)

    def test_a_staggered_probe_launch_is_watched_and_its_stagger_stays_exact(self):
        h = harness(self, text=shared_text(probes=25, stagger="4s"), stack={"watch_s": 2.0})
        h.run()
        probes = sorted(v["launched_at"] for k, v in h.runner.s.launched.items() if k.startswith("probe-"))
        self.assertEqual({round(b - a, 6) for a, b in zip(probes, probes[1:])}, {4.0})
        self.assertGreaterEqual(len([w for w, _ in h.stack.watched if probes[0] <= w < probes[-1]]), 5)

    def test_a_slow_fleet_poll_does_not_cancel_the_rounds_between_events_3_s_apart(self):
        h = harness(self, text=shared_text(hold="4m", events=mutes(15, 3, 63)), stack={"watch_s": 1.0})
        slow_poll(h, 0.3)
        h.run()
        self.assertEqual(h.failed(), ["R11"])
        hs, he = h.runner.s.hold_mono["hold_start"], h.runner.s.hold_mono["hold_end"]
        self.assertGreaterEqual(len([m for _, m in h.stack.watched if hs <= m <= he]), 10)
        self.assertLessEqual(unwatched(h), 17.0 + 2 * 0.3 + 0.5 + 1e-6)

    def test_no_stretch_goes_unwatched_past_the_ceiling_with_events_2_s_apart(self):
        h = harness(self, text=shared_text(hold="4m", events=mutes(15, 2, 95)),
                    stack={"watch_s": 2.0}, fleet={"confirm_delay": 0.5})
        slow_poll(h, 0.3)
        h.run()
        self.assertEqual(h.failed(), ["R11"])
        self.assertLessEqual(unwatched(h), 30.0 + 0.3 + 0.5 + 1e-6)

    def test_slow_launches_of_many_rust_leavers_stay_under_the_ceiling(self):
        leave = "[{at: hold+5m, select: {role: viewers, count: 10}, action: leave}]"
        h = harness(self, text=shared_text(viewers=10, events=leave), stack={"watch_s": 1.0})
        launch = h.fleet.launch

        def slow(proc):
            h.clock.advance(4.0)
            return launch(proc)
        h.fleet.launch = slow
        h.run()
        self.assertEqual(len([p for p in h.plan["processes"] if p["fleet"] == "rust"]), 11)
        self.assertEqual(h.failed(), ["R11"])
        self.assertLessEqual(unwatched(h), 30.0 + 4.0 + 1e-6)

    def test_the_first_round_comes_within_one_slow_launch_of_its_forced_time(self):
        h = harness(self, text=shared_text(probes=1), stack={"watch_s": 1.0})
        launch = h.fleet.launch

        def slow(proc):
            h.clock.advance(20.0)
            return launch(proc)
        h.fleet.launch = slow
        h.run()
        join = h.lines("join_start")[0]["mono"]
        self.assertLessEqual(h.stack.watched[0][1] - join, 30.0 - 15.0 + 20.0 + 1e-6)

    def test_a_slow_last_probe_launch_gets_its_forced_round_before_a_slow_join_poll(self):
        h = harness(self, text=shared_text(probes=1), stack={"watch_s": 1.0})
        launch, joined, after = h.fleet.launch, h.fleet.joined, []

        def slow_launch(proc):
            if proc["proc_id"].startswith("probe"):
                h.clock.advance(35.0)
                after.append(True)
            return launch(proc)

        def slow_joined():
            if after and after.pop():
                h.clock.advance(20.0)
            return joined()
        h.fleet.launch, h.fleet.joined = slow_launch, slow_joined
        h.run()
        self.assertLessEqual(unwatched(h), 35.0 + 1e-6)

    def test_a_due_round_in_the_join_loop_waits_only_for_fleet_joined(self):
        h = harness(self, stack={"watch_s": 1.0}, fleet={"join_delay": 100.0})
        joined = h.fleet.joined

        def slow():
            h.clock.advance(0.3)
            return joined()
        h.fleet.joined = slow
        h.run()
        hs, starts = h.runner.s.hold_mono["hold_start"], [m for _, m in h.stack.watched]
        late = [b - (a + 15.0) for a, b in zip(h.stack.watch_ends, starts[1:]) if b < hs]
        self.assertGreaterEqual(len(late), 5)
        self.assertLessEqual(max(late), 0.3 + 1e-6, late)

    def test_a_starving_round_over_budget_fails_closed_through_r6(self):
        h = harness(self, text=shared_text(hold="4m", events=mutes(15, 2, 95)), stack={"watch_s": 8.0})
        run = h.run()
        self.assertIn("R6", h.failed())
        self.assertEqual(run["verdict"], "INVALID")

    def test_a_poll_slower_than_the_apply_still_meets_the_ceiling(self):
        cases = ((shared_text(hold="4m", events=mutes(15, 2, 95)), {"confirm_delay": 0.0}),
                 (shared_text(probes=25, stagger="1900ms"), {}))
        for text, fleet in cases:
            with self.subTest(fleet=fleet):
                h = harness(self, text=text, stack={"watch_s": 1.5}, fleet=fleet)
                slow_poll(h, 0.9)
                h.run()
                self.assertEqual(h.failed(), ["R11"])
                self.assertLessEqual(unwatched(h), 30.0 + 0.9 + 1e-6)

    def test_no_stretch_goes_unwatched_past_the_ceiling_in_a_1_5_s_stagger(self):
        h = harness(self, text=shared_text(probes=25, stagger="1500ms"), stack={"watch_s": 1.0})
        slow_poll(h, 0.3)
        h.run()
        self.assertEqual(h.failed(), ["R11"])
        probes = sorted(v["launched_at"] for k, v in h.runner.s.launched.items() if k.startswith("probe-"))
        self.assertGreaterEqual(len([w for w, _ in h.stack.watched if probes[0] <= w < probes[-1]]), 1)
        self.assertLessEqual(unwatched(h), 30.0 + 0.3 + 1e-6)

    def test_a_failing_watch_aborts_and_the_run_is_still_torn_down_and_scored(self):
        h = harness(self, stack={"fail_at": 1605.0})
        run = h.run()
        self.assertEqual((run["verdict"], run["error"], run["scorer_verdict"]), ("INVALID", None, "PASS"))
        self.assertEqual(run["aborted"], "stack watch failed: prometheus timed out")
        self.assertEqual(h.failed(), ["R1", "R11"])
        self.assertEqual((run["watch"]["rounds"], run["watch"]["max_s"]), (len(h.stack.watched), 0.0))
        self.assertTrue(any(c[0] == "stop" for c in h.fleet.calls))

    def test_a_watch_raising_anything_but_adapter_error_is_an_internal_error(self):
        h = harness(self)
        h.stack.watch = lambda: 1 / 0
        run = h.run()
        self.assertEqual((run["verdict"], run["aborted"]), ("ERROR", None))
        self.assertIn("internal error ZeroDivisionError", run["error"])

    def test_watch_at_gives_the_due_time_the_budget_fit_and_the_forced_time(self):
        h = harness(self)
        h.runner.s.watched_mono = 0.0
        times = [h.runner._watch_at(mono, target) for mono, target in ((10.0, 100.0), (20.0, 22.0), (20.0, 21.9))]
        self.assertEqual(times, [15.0, 20.0, 30.0])

    def test_a_round_is_due_from_the_stacks_last_watch_when_the_fleet_watched_later(self):
        h = harness(self)
        h.runner.s.watched_mono, h.stack.watched_mono = 0.0, 10.0
        self.assertEqual(h.runner._watch_at(12.0, 100.0), 25.0)

    def test_no_round_starts_before_it_is_due(self):
        h = harness(self)
        now = h.clock.mono
        h.runner.s.watched_mono = now - 14.7
        h.runner._poll(math.inf)
        h.runner.s.watched_mono = now - 29.7
        h.runner._sleep_until_mono(now)
        self.assertEqual((h.stack.watched, h.clock.mono), ([], now))

    def test_the_first_round_follows_the_rust_launch_and_a_stagger_defers_one_only_to_its_forced_time(self):
        h = harness(self, text=shared_text(probes=12, stagger="1900ms"), stack={"watch_s": 1.0})
        h.run()
        self.assertAlmostEqual(h.stack.watched[0][1] - h.lines("join_start")[0]["mono"], 0.0, places=6)
        gaps = [start - end for (_, start), end in zip(h.stack.watched[1:], h.stack.watch_ends)]
        self.assertGreater(max(gaps), cq_runner.WATCH_INTERVAL_S)
        self.assertLessEqual(max(gaps), cq_runner.WATCH_STARVE_S)

    def test_a_failing_watch_in_the_join_is_an_abort_not_an_error(self):
        run = harness(self, stack={"fail_at": 941.0}).run()
        self.assertEqual((run["verdict"], run["error"], run["scorer_verdict"]), ("INVALID", None, None))
        self.assertTrue(run["aborted"].startswith("stack watch failed: "))

    def test_run_json_records_the_slowest_watch_and_rounds_over_budget(self):
        h = harness(self, stack={"watch_s": [1.0, 8.0, 3.0]})
        h.run()
        with open(os.path.join(h.dir, "run.json"), encoding="utf-8") as fh:
            watch = json.load(fh)["watch"]
        self.assertEqual(watch, {"rounds": len(h.stack.watched), "max_s": 8.0, "over_budget": len(h.stack.watched) - 1})
        self.assertGreater(watch["rounds"], 10)

    def test_a_round_of_exactly_the_budget_is_not_over_budget(self):
        h = harness(self, stack={"watch_s": 2.0})
        h.run()
        self.assertEqual((h.result["watch"]["over_budget"], h.result["watch"]["max_s"]), (0, 2.0))
        self.assertGreater(h.result["watch"]["rounds"], 10)


class LoadHostAlias(unittest.TestCase):

    def test_load_a_is_accepted_as_a_clock_node_and_a_generator_host(self):
        self.assertIn("load-a", cq_collect.RESERVED_NODES)
        block = cq_runner.clock_block(LOAD_HOST["facts"], "local", 1.0)
        self.assertEqual(block["per_node"], [{"node": "load-a", "skew_ms": 3}])
        h = harness(self)
        h.run()
        with open(os.path.join(h.dir, "manifest.json"), encoding="utf-8") as fh:
            self.assertEqual(json.load(fh)["clock"]["per_node"], [{"node": "load-a", "skew_ms": 3}])
        with open(os.path.join(h.dir, "generator-verdict.json"), encoding="utf-8") as fh:
            self.assertEqual((lambda v: (v["ok"], v["hosts"]))(json.load(fh)), (True, ["load-a"]))


class Cli(unittest.TestCase):

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp)
        self.repo, self.runs = os.path.join(self.tmp, "repo"), os.path.join(self.tmp, "runs")
        self.scenario = os.path.join(self.repo, "t.yaml")
        os.makedirs(self.repo)
        with open(self.scenario, "w", encoding="utf-8") as fh:
            fh.write(shared_text())
        self.clock = fakes.FakeClock(wall=1791288000.0)
        self.tree = fakes.FakeTree(self.clock, self.repo)

    def main(self, *argv, adapters=None):
        err, out = io.StringIO(), io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(out):
            rc = scenario_run.main(["--scenario", self.scenario, "--out-dir", self.runs, *argv], adapters=adapters,
                                   tree=self.tree, clock=self.clock, tools=tsr.StubScorer(), environ={})
        return rc, out.getvalue(), err.getvalue()

    def test_hcl_daily_compiles_from_the_cli(self):
        rc, out, _ = self.main("--compile-only")
        self.assertEqual((rc, json.loads(out)["target"]), (0, SHARED))

    def test_hcl_daily_needs_an_explicit_target_and_stack_reuse(self):
        for flags in ((), ("--stack", "reuse"), ("--target", SHARED), ("--target", SHARED, "--stack", "up")):
            with self.subTest(flags=flags):
                rc, _, err = self.main(*flags, adapters={SHARED: self.factory})
                self.assertEqual(rc, 3)
                self.assertIn("pass --target hcl-daily and --stack reuse", err)
                self.assertFalse(os.path.exists(self.runs))

    def factory(self, plan, run_dir=None, stack_mode=None):
        self.clock.wall, self.clock.mono = 900.0, 50.0
        self.stack = fakes.FakeSharedStack(self.clock)
        return self.stack, fakes.FakeFleet(self.clock, plan, **LOAD_HOST)

    def test_hcl_daily_with_an_injected_adapter_exits_invalid_through_r11(self):
        rc, _, err = self.main("--target", SHARED, "--stack", "reuse", adapters={SHARED: self.factory})
        self.assertEqual(rc, 2, err)
        self.assertIn("INVALID (scorer PASS; failed runner gates R11)", err)
        self.assertEqual(self.stack.calls[0], ("up", "reuse"))
        self.assertNotIn(("down",), self.stack.calls)
        self.assertNotIn("refused at admission", err)

    def test_the_summary_line_names_the_probes_refused_at_admission(self):
        def factory(plan, run_dir=None, stack_mode=None):
            self.clock.wall, self.clock.mono = 900.0, 50.0
            return fakes.FakeSharedStack(self.clock), fakes.FakeFleet(self.clock, plan,
                                                                      **dict(LOAD_HOST, admit=["probe-02", "probe-05"]))
        rc, _, err = self.main("--target", SHARED, "--stack", "reuse", adapters={SHARED: factory})
        self.assertEqual(rc, 2, err)
        self.assertIn("failed runner gates R11); probes refused at admission: probe-02, probe-05", err)


if __name__ == "__main__":
    unittest.main()
