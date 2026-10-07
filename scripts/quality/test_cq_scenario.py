#!/usr/bin/env python3
"""Tests for the scenario file loader and compiler (#2914 PR-4)."""

import copy
import os
import sys
import time
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cq_collect  # noqa: E402
import cq_manifest  # noqa: E402
import cq_scenario  # noqa: E402

RUN_ID = "cqlocal-20261006t1200-0123abc"
DESIGN_EXAMPLE = '''\
schema: scale-scenario/v1
run:  { id_prefix: cqlocal, seed: 42, target: local, fidelity_waivers: [] }
population:
  - { role: probe-ref, fleet: browser, count: 10, media: { camera: off, mic: muted },
      network: none, transport: auto, join_stagger: 3s }
  - { role: talkers, fleet: rust, count: 2, media: { camera: on, mic: continuous, video_layers: 3 } }
  - { role: cameras, fleet: rust, count: 4, media: { camera: on, mic: muted, video_layers: 3 } }
  - { role: viewers, fleet: rust, count: 14, media: { camera: off, mic: off },
      receive: { pin_video_layer: 0, viewport_visible_count: 3 } }
steps:  [ { id: n30, hold: 10m, headline: true } ]
events: [ { at: hold+5m, select: { role: viewers, count: 1 }, action: leave } ]
scoring:
  mode: gate
  relay_path_exemptions: {}
  expect: null
'''


def compile_doc(doc, run_id=RUN_ID):
    return cq_scenario.compile_scenario(doc, run_id=run_id, scenario_file="scripts/quality/scenarios/x.yaml",
                                        scenario_sha256="0" * 64)


def base():
    return cq_scenario.parse_yaml(DESIGN_EXAMPLE)


class DesignExample(unittest.TestCase):

    def setUp(self):
        self.plan = compile_doc(base())

    def test_the_plan_is_what_the_collector_reads(self):
        self.assertEqual(cq_collect._check_plan(self.plan), [])
        self.assertEqual((self.plan["environment"], self.plan["meeting_id"]), ("local", f"scale-{RUN_ID}"))
        self.assertTrue(cq_manifest.RUN_ID_RE.match(self.plan["run_id"]))

    def test_participants_carry_the_planned_roles_and_media(self):
        parts = self.plan["participants"]
        self.assertEqual(len(parts), 30)
        browser = [p for p in parts if p["fleet"] == "browser"]
        self.assertEqual(len(browser), 10)
        self.assertTrue(all(p["observer"] and not p["talker"] and p["user_id"].endswith("@bots-app.local")
                            and p["publishes"] == {"camera": False, "mic": False, "screen": False} for p in browser))
        talkers = [p for p in parts if p["talker"]]
        self.assertEqual([p["role"] for p in talkers], ["talkers", "talkers"])
        self.assertTrue(all(p["publishes"] == {"camera": True, "mic": True, "screen": False} for p in talkers))
        self.assertTrue(all(not p["observer"] for p in parts if p["fleet"] == "rust"))
        self.assertEqual(parts[10]["user_id"], f"{RUN_ID}-talkers-00")
        self.assertEqual(len({p["user_id"] for p in parts}), 30)
        self.assertTrue(all(p["steps"] == ["n30"] for p in parts))

    def test_n_target_excludes_the_planned_final_leave(self):
        step = self.plan["steps"][0]
        self.assertEqual((step["n_target"], step["hold_s"], step["headline"]), (29, 600, True))
        self.assertEqual(step["join_window_s"], cq_scenario.JOIN_WINDOW_BASE_S + 3 * 9)

    def test_the_planned_leave_picks_the_last_viewer_at_its_offset(self):
        self.assertEqual(self.plan["events"], [{"event_id": "e0", "at_offset_s": 300, "action": "leave",
                                                "participants": [f"{RUN_ID}-viewers-13"], "params": {}}])

    def test_rust_processes_share_one_roster_and_isolate_the_leaver(self):
        rust = [p for p in self.plan["processes"] if p["fleet"] == "rust"]
        self.assertEqual([(p["proc_id"], len(p["participants"]), p["roster_offset"]) for p in rust],
                         [("rust-pub", 6, 0), ("rust-role-viewers", 13, 6), ("rust-leave-00", 1, 19)])
        self.assertTrue(all(p["run_size"] == 20 for p in rust))
        self.assertEqual(rust[1]["receive"], {"pin_video_layer": 0, "viewport_visible_count": 3})
        self.assertEqual(rust[2]["receive"], rust[1]["receive"])
        self.assertEqual(rust[0]["video_layers"], 3)
        probes = [p for p in self.plan["processes"] if p["fleet"] == "browser"]
        self.assertEqual([p["probe_index"] for p in probes], list(range(10)))
        self.assertTrue(all(p["join_stagger_s"] == 3 for p in probes))
        every = [u for p in self.plan["processes"] for u in p["participants"]]
        self.assertEqual(sorted(every), sorted(p["user_id"] for p in self.plan["participants"]))
        for p in self.plan["participants"]:
            self.assertIn(p["user_id"], next(q for q in self.plan["processes"]
                                             if q["proc_id"] == p["process"])["participants"])


class StrictYaml(unittest.TestCase):

    def test_on_off_yes_no_and_sexagesimal_stay_strings(self):
        doc = cq_scenario.parse_yaml("a: on\nb: off\nc: yes\nd: no\ne: 10:00\nf: 1.5\ng: .nan\nh: true\ni: 7\nj:\n")
        self.assertEqual(doc, {"a": "on", "b": "off", "c": "yes", "d": "no", "e": "10:00", "f": "1.5",
                               "g": ".nan", "h": True, "i": 7, "j": None})

    def test_a_duplicate_key_is_refused(self):
        with self.assertRaisesRegex(cq_scenario.ScenarioError, "duplicate key 'count'"):
            cq_scenario.parse_yaml("count: 1\ncount: 2\n")

    def test_a_non_string_key_is_refused(self):
        with self.assertRaisesRegex(cq_scenario.ScenarioError, "not a string"):
            cq_scenario.parse_yaml("1: x\n")

    def test_python_tags_are_refused(self):
        with self.assertRaises(cq_scenario.ScenarioError):
            cq_scenario.parse_yaml("a: !!python/object/apply:os.system ['true']\n")

    def test_explicit_tags_other_than_str_seq_map_and_null_are_refused(self):
        for text in ("a: !!bool yes", "a: !!int 0x1F", "a: !!int 1_000", "a: !!float 1.5", "a: !!set {x}",
                     "a: !!timestamp 2026-10-06", "a: !!binary aGk=", "a: !!omap [{x: 1}]", "a: !local x",
                     "!!map {a: !!bool true}"):
            with self.subTest(text):
                with self.assertRaisesRegex(cq_scenario.ScenarioError, "explicit tag .* is refused"):
                    cq_scenario.parse_yaml(text + "\n")
        self.assertEqual(cq_scenario.parse_yaml("!!map {a: !!str 5, b: !!seq [1], c: !!null '', d: ! yes}\n"),
                         {"a": "5", "b": [1], "c": None, "d": "yes"})

    def test_signed_and_underscored_integers_stay_strings(self):
        self.assertEqual(cq_scenario.parse_yaml("a: +5\nb: 1_000\nc: 01\nd: -3\n"),
                         {"a": "+5", "b": "1_000", "c": "01", "d": -3})

    def test_a_deeply_nested_document_is_a_scenario_error(self):
        with self.assertRaisesRegex(cq_scenario.ScenarioError, "nested too deeply"):
            cq_scenario.parse_yaml("a: " + "[" * 20000 + "]" * 20000 + "\n")

    def test_missing_pyyaml_is_a_scenario_error(self):
        with mock.patch.dict(sys.modules, {"yaml": None}):
            with self.assertRaisesRegex(cq_scenario.ScenarioError, "requirements-runner.txt"):
                cq_scenario.parse_yaml("a: 1\n")


def edit(fn):
    doc = base()
    fn(doc)
    return doc


def role(doc, name):
    return next(r for r in doc["population"] if r["role"] == name)


REFUSALS = [
    ("camera-on event", lambda d: d["events"].append({"at": "hold+1m", "select": {"role": "probe-ref", "count": 1},
                                                      "action": "camera-on"}), "camera-on is refused: G-V13"),
    ("camera-off event", lambda d: d["events"][0].update(action="camera-off"), "camera-off is refused: G-V13"),
    ("outage event", lambda d: d["events"][0].update(action="outage"), "outage is refused: G-V13"),
    ("netem event", lambda d: d["events"][0].update(action="netem"), "netem is refused"),
    ("rejoin event", lambda d: d["events"][0].update(action="rejoin"),
     "rejoin is refused: a v1 limit: the collector allows one record per user id"),
    ("unknown action", lambda d: d["events"][0].update(action="dance"), "must be one of leave, mute, unmute"),
    ("screen share", lambda d: role(d, "probe-ref")["media"].update(screen="on"), "screen share is refused"),
    ("mute of a Rust talker", lambda d: d["events"].append(
        {"at": "hold+1m", "select": {"role": "talkers", "count": 1}, "action": "mute"}),
     "Rust bot has no runtime control"),
    ("mute of a browser talker", lambda d: (
        d["population"].append({"role": "btalk", "fleet": "browser", "count": 1,
                                "media": {"camera": "off", "mic": "continuous"}}),
        d["events"].append({"at": "hold+1m", "select": {"role": "btalk", "count": 1}, "action": "mute"})),
     "of declared talker"),
    ("unmute of a Rust viewer", lambda d: d["events"].append(
        {"at": "hold+1m", "select": {"role": "viewers", "count": 1}, "action": "unmute"}),
     "Rust bot has no runtime control"),
    ("more than 30 camera publishers", lambda d: role(d, "cameras").update(count=29),
     "31 camera publishers; more than 30 makes the step INVALID by construction (G-V12)"),
    ("Rust webtransport", lambda d: role(d, "talkers").update(transport="webtransport"), "webtransport is invalid"),
    ("netem network", lambda d: role(d, "probe-ref").update(network="lossy_mobile"), "'lossy_mobile' is refused"),
    ("cameras without a talker", lambda d: d["population"].remove(role(d, "talkers")),
     "rust-pub: a Rust process with cameras but no talker sends no video"),
    ("a planned leave of a Rust camera", lambda d: d["events"][0]["select"].update(role="cameras"),
     "rust-leave-00: a Rust process with cameras but no talker"),
    ("mixed publisher video layers", lambda d: role(d, "cameras")["media"].update(video_layers=1),
     "their video_layers must match"),
    ("receive on a publisher", lambda d: role(d, "cameras").update(receive={"pin_video_layer": 0}),
     "only a viewer role"),
    ("browser receive", lambda d: role(d, "probe-ref").update(receive={"pin_video_layer": 0}),
     "only Rust roles take a receive config"),
    ("browser video layers", lambda d: role(d, "probe-ref")["media"].update(video_layers=3),
     "only Rust roles take video_layers"),
    ("report mode", lambda d: d["scoring"].update(mode="report"), "only gate is supported"),
    ("an expected PASS", lambda d: d["scoring"].update(expect={"verdict": "PASS", "invalid_gates": []}),
     "never expects PASS"),
    ("an INVALID expectation with no gates", lambda d: d["scoring"].update(
        expect={"verdict": "INVALID", "invalid_gates": []}), "non-empty exactly when verdict is INVALID"),
    ("a malformed gate id", lambda d: d["scoring"].update(expect={"verdict": "INVALID", "invalid_gates": ["GV7"]}),
     "distinct G-V gate ids"),
    ("three relay exemptions", lambda d: d["scoring"].update(relay_path_exemptions={
        k: "why" for k in ("relay_layer_filtered_total", "relay_viewport_filtered_total",
                           "relay_keyframe_requests_total")}), "at most 2 known relay counters"),
    ("an exemption without a reason", lambda d: d["scoring"].update(
        relay_path_exemptions={"relay_layer_filtered_total": " "}), "each with a reason"),
    ("cluster target", lambda d: d["run"].update(target="cluster"), "cluster runs come with PR-8"),
    ("two steps", lambda d: d["steps"].append({"id": "n2", "hold": "5m", "headline": False}),
     "exactly one step (D4)"),
    ("a short hold", lambda d: d["steps"][0].update(hold="59s"), "at least 60 s"),
    ("a non-headline step", lambda d: d["steps"][0].update(headline=False), "the only step is the headline step"),
    ("an event at hold start", lambda d: d["events"][0].update(at="hold+0s"), "within [15 s, hold - 35 s]"),
    ("an event in the last 35 s", lambda d: d["events"][0].update(at="hold+566s"), "within [15 s, hold - 35 s]"),
    ("an event before the hold", lambda d: d["events"][0].update(at="join+1m"), "must be hold+<duration>"),
    ("an unpaired mute", lambda d: d["events"].append(
        {"at": "hold+1m", "select": {"role": "probe-ref", "count": 1}, "action": "mute"}),
     "mute of cqlocal-20261006t1200-0123abc-probe-ref-09@bots-app.local is not paired"),
    ("a second unmute", lambda d: d["events"].extend([
        {"at": "hold+1m", "select": {"role": "probe-ref", "count": 1}, "action": "unmute"},
        {"at": "hold+2m", "select": {"role": "probe-ref", "count": 1}, "action": "unmute"}]), "unmute of"),
    ("too many selected", lambda d: d["events"][0]["select"].update(count=15),
     "15 of role viewers requested, 14 still present"),
    ("a viewer selected after the role has left", lambda d: d["events"].append(
        {"at": "hold+6m", "select": {"role": "viewers", "count": 14}, "action": "leave"}),
     "14 of role viewers requested, 13 still present"),
    ("an unknown role", lambda d: d["events"][0]["select"].update(role="ghosts"), "needs a declared role"),
    ("a zero count", lambda d: role(d, "viewers").update(count=0), "count: must be a positive integer"),
    ("a boolean count", lambda d: role(d, "viewers").update(count=True), "count: must be a positive integer"),
    ("duplicate roles", lambda d: role(d, "cameras").update(role="talkers"), "role names must be unique"),
    ("an unknown top-level key", lambda d: d.update(ramp=[]), "scenario.ramp: unknown key"),
    ("an unknown role key", lambda d: role(d, "viewers").update(placement="node-a"), "placement: unknown key"),
    ("a wrong schema", lambda d: d.update(schema="scale-scenario/v0"), "schema: must be scale-scenario/v1"),
    ("a camera spelled true", lambda d: role(d, "cameras")["media"].update(camera=True), "must be on or off"),
    ("a bad stagger", lambda d: role(d, "probe-ref").update(join_stagger="3 s"), "must be a duration"),
    ("a bad pin", lambda d: role(d, "viewers")["receive"].update(pin_video_layer=3), "must be 0, 1 or 2"),
    ("a bad waiver", lambda d: d["run"].update(fidelity_waivers=[""]), "non-empty strings"),
    ("a zero event count", lambda d: d["events"][0]["select"].update(count=0),
     "needs a declared role and a positive count"),
    ("a negative viewport count", lambda d: role(d, "viewers")["receive"].update(viewport_visible_count=-1),
     "viewport_visible_count: must be a non-negative integer"),
    ("four video layers", lambda d: role(d, "talkers")["media"].update(video_layers=4), "must be 1, 2 or 3"),
    ("a staggered Rust role", lambda d: role(d, "viewers").update(join_stagger="1s"),
     "only browser roles are staggered"),
    ("video layers on a camera-off Rust role", lambda d: role(d, "viewers")["media"].update(video_layers=1),
     "a role with camera off publishes no video layers"),
    ("an unknown browser transport", lambda d: role(d, "probe-ref").update(transport="quic"),
     "probe-ref].transport: must be one of"),
    ("a headline of 1", lambda d: d["steps"][0].update(headline=1), "the only step is the headline step"),
    ("a string seed", lambda d: d["run"].update(seed="42"), "run.seed: must be an integer"),
    ("a repeated expected gate", lambda d: d["scoring"].update(
        expect={"verdict": "INVALID", "invalid_gates": ["G-V7", "G-V7"]}), "distinct G-V gate ids"),
    ("a bad join window", lambda d: d["steps"][0].update(join_window="soon"), "join_window: must be a duration"),
]


class Refusals(unittest.TestCase):

    def test_the_unedited_example_compiles(self):
        compile_doc(base())

    def test_each_refusal_names_its_reason(self):
        for name, fn, needle in REFUSALS:
            with self.subTest(name):
                with self.assertRaises(cq_scenario.ScenarioError) as cm:
                    compile_doc(edit(fn))
                self.assertTrue(any(needle in e for e in cm.exception.errors), cm.exception.errors)

    def test_a_paired_mute_of_an_observer_is_accepted(self):
        plan = compile_doc(edit(lambda d: d["events"].extend([
            {"at": "hold+1m", "select": {"role": "probe-ref", "count": 1}, "action": "unmute"},
            {"at": "hold+2m", "select": {"role": "probe-ref", "count": 1}, "action": "mute"}])))
        self.assertEqual([(e["action"], e["at_offset_s"]) for e in plan["events"]],
                         [("unmute", 60), ("mute", 120), ("leave", 300)])
        self.assertEqual(plan["events"][0]["participants"], plan["events"][1]["participants"])

    def test_event_offsets_at_the_bounds_are_accepted(self):
        for at in ("hold+15s", "hold+565s"):
            with self.subTest(at):
                compile_doc(edit(lambda d: d["events"][0].update(at=at)))

    def test_exactly_two_relay_exemptions_are_accepted(self):
        compile_doc(edit(lambda d: d["scoring"].update(relay_path_exemptions={
            "relay_layer_filtered_total": "why", "relay_viewport_filtered_total": "why"})))

    def test_a_hold_of_exactly_60_s_is_accepted(self):
        plan = compile_doc(edit(lambda d: (d["steps"][0].update(hold="60s"), d["events"][0].update(at="hold+20s"))))
        self.assertEqual(plan["steps"][0]["hold_s"], 60)

    def test_compiling_is_linear_in_the_participant_count(self):
        doc = edit(lambda d: role(d, "probe-ref").update(count=20000))
        start = time.monotonic()
        plan = compile_doc(doc)
        self.assertLess(time.monotonic() - start, 1.0)
        self.assertEqual(plan["participants"][-2]["process"], "rust-role-viewers")
        self.assertEqual(plan["participants"][19999]["process"], "probe-19999")

    def test_exactly_30_camera_publishers_is_accepted(self):
        compile_doc(edit(lambda d: role(d, "cameras").update(count=28)))

    def test_a_rust_talker_may_leave_alone(self):
        plan = compile_doc(edit(lambda d: d["events"][0]["select"].update(role="talkers")))
        leaver = next(p for p in plan["processes"] if p["proc_id"] == "rust-leave-00")
        self.assertEqual(leaver["participants"], [f"{RUN_ID}-talkers-01"])

    def test_role_names_that_spell_another_process_id_still_get_their_own_process(self):
        def names(d):
            role(d, "viewers").update(role="pub")
            d["population"].append({"role": "leave-00", "fleet": "rust", "count": 2,
                                    "media": {"camera": "off", "mic": "off"}})
            d["events"][0]["select"].update(role="pub")
        plan = compile_doc(edit(names))
        ids = [p["proc_id"] for p in plan["processes"]]
        self.assertEqual(len(ids), len(set(ids)), ids)
        self.assertLessEqual({"rust-pub", "rust-role-pub", "rust-role-leave-00", "rust-leave-00"}, set(ids))
        every = [u for p in plan["processes"] for u in p["participants"]]
        self.assertEqual(sorted(every), sorted(p["user_id"] for p in plan["participants"]))

    def test_two_processes_with_one_name_are_refused(self):
        with mock.patch.object(cq_scenario, "PROC_ROLE", "rust-{}"), \
                self.assertRaisesRegex(cq_scenario.ScenarioError, "two processes are named rust-pub"):
            compile_doc(edit(lambda d: role(d, "viewers").update(role="pub")))

    def test_a_bad_run_id_is_refused(self):
        with self.assertRaisesRegex(cq_scenario.ScenarioError, "must match"):
            compile_doc(base(), run_id="CQ_local")


class RunId(unittest.TestCase):

    def test_run_id_is_prefix_utc_minute_and_short_sha(self):
        self.assertEqual(cq_scenario.derive_run_id("cqlocal", 1791288000.0, "0123abcd" * 5),
                         "cqlocal-20261006t1200-0123abc")

    def test_the_longest_prefix_fits_the_manifest_run_id(self):
        rid = cq_scenario.derive_run_id("a" * 12, 4102444799.0, "f" * 40)
        self.assertTrue(cq_manifest.RUN_ID_RE.match(rid), rid)

    def test_bad_prefixes_and_commits_are_refused(self):
        for prefix, commit in (("CQ", "abcdef0"), ("a" * 13, "abcdef0"), ("cq-x", "abcdef0"), (None, "abcdef0"),
                               ("cq", "xyz"), ("cq", "abc")):
            with self.subTest(prefix=prefix, commit=commit):
                with self.assertRaises(cq_scenario.ScenarioError):
                    cq_scenario.derive_run_id(prefix, 1791288000.0, commit)


class Durations(unittest.TestCase):

    def test_units(self):
        self.assertEqual([cq_scenario.duration_s(t) for t in ("3s", "10m", "1h", "500ms", "10", "1.5s", 5, "-1s")],
                         [3, 600, 3600, 0.5, None, None, None, None])

    def test_compiling_does_not_mutate_the_document(self):
        doc = base()
        snapshot = copy.deepcopy(doc)
        compile_doc(doc)
        self.assertEqual(doc, snapshot)


if __name__ == "__main__":
    unittest.main()
