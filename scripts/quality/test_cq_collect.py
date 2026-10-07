#!/usr/bin/env python3
"""Tests for the run-manifest collector. The fixture is built from the 2026-10-01 local run (cqlocal1)."""

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

import call_quality_score  # noqa: E402
import cq_collect  # noqa: E402
import cq_manifest  # noqa: E402
import cq_score  # noqa: E402

FIXTURE = os.path.join(HERE, "testdata", "cq_collect_20261001")
PROBE = "probe0@bots-app.local"
PROBE_RECORD = os.path.join("probes", "0", "participants", "c8d6004c-4137-4ea9-97f8-9011385ae1f2.json")
RUST = os.path.join("rust", "participants-0.json")
JS, HS, HE, TEARDOWN = 1790867520.0, 1790867665.0, 1790868085.0, 1790868115.0
LAST = 1790868174.335030849
NETEM_REASON = "set by POST /netem at 2026-10-01T15:15:25.000Z; the shaping params are not read back"
ALL = [PROBE, "alice", "bot-002", "bot-003", "bot-004", "bot-005", "bot-006"]
STEP_S = 15


def json_diff(a, b, path="$"):
    """JSON paths where a and b differ."""
    if isinstance(a, dict) and isinstance(b, dict):
        return {d for k in set(a) | set(b)
                for d in (json_diff(a[k], b[k], f"{path}.{k}") if k in a and k in b else {f"{path}.{k}"})}
    if isinstance(a, list) and isinstance(b, list) and len(a) == len(b):
        return {d for i, (x, y) in enumerate(zip(a, b)) for d in json_diff(x, y, f"{path}[{i}]")}
    return set() if a == b else {path}


class RunDir:
    def __init__(self, tc):
        tmp = tempfile.mkdtemp()
        tc.addCleanup(shutil.rmtree, tmp)
        self.path = os.path.join(tmp, "run")
        shutil.copytree(FIXTURE, self.path)

    def edit(self, rel, fn):
        p = os.path.join(self.path, rel)
        with open(p, encoding="utf-8") as fh:
            obj = json.load(fh)
        fn(obj)
        with open(p, "w", encoding="utf-8") as fh:
            json.dump(obj, fh)

    def rust(self, uid, **changes):
        def apply(f):
            next(p for p in f["participants"] if p["user_id"] == uid).update(changes)
        self.edit(RUST, apply)

    def probe(self, **changes):
        self.edit(PROBE_RECORD, lambda r: r["participant"].update(changes))

    def plan_participant(self, uid, **changes):
        self.edit(cq_collect.PLAN_FILE,
                  lambda pl: next(p for p in pl["participants"] if p["user_id"] == uid).update(changes))

    def events(self, keep=lambda e: True, add=()):
        p = os.path.join(self.path, cq_collect.EVENTS_FILE)
        with open(p, encoding="utf-8") as fh:
            lines = [json.loads(line) for line in fh if line.strip()]
        lines = [e for e in lines if keep(e)] + list(add)
        with open(p, "w", encoding="utf-8") as fh:
            fh.writelines(json.dumps(e) + "\n" for e in sorted(lines, key=lambda e: e["wall"]))

    def collect(self):
        return cq_collect.collect(self.path)

    def cli(self):
        with contextlib.redirect_stderr(io.StringIO()):
            return cq_collect.main(["--run-dir", self.path])


def event(action, uid, t_issued, t_confirmed, result="ok"):
    return {"type": "event", "wall": t_confirmed or t_issued, "action": action, "participants": [uid], "params": {},
            "t_issued": t_issued, "t_confirmed": t_confirmed, "result": result}


def by_uid(manifest):
    return {p["user_id"]: p for p in manifest["participants"]}


class Reproduces20261001(unittest.TestCase):
    def test_matches_the_hand_built_manifest_except_network_params_and_the_node_name(self):
        got = RunDir(self).collect()
        with open(os.path.join(FIXTURE, "expected_manifest.json"), encoding="utf-8") as fh:
            expected = json.load(fh)
        self.assertEqual(got.missing, [])
        self.assertEqual(json_diff(got.manifest, expected),
                         {"$.participants[0].network.params", "$.clock.per_node[0].node"})
        self.assertEqual(got.manifest["clock"]["per_node"], [{"node": "local", "skew_ms": 0.0}])
        self.assertEqual(got.manifest["participants"][0]["network"]["params"], {})

    def test_written_manifest_passes_the_scorer_validate_only_entry_point(self):
        run = RunDir(self)
        self.assertEqual(run.cli(), cq_collect.EXIT_OK)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = call_quality_score.main(["--manifest", os.path.join(run.path, "manifest.json"), "--validate-only"])
        self.assertEqual(rc, 0, out.getvalue())
        self.assertIn("manifest OK: 7 participants", out.getvalue())

    def test_the_confirmed_mute_is_written_at_t_issued(self):
        events = RunDir(self).collect().manifest["events"]
        self.assertEqual([(e["action"], e["at"]) for e in events], [("mute", 1790867561.964954976)])


class Partial(unittest.TestCase):
    def assertPartial(self, run, *names):
        got = run.collect()
        self.assertEqual(got.manifest["schema"], cq_collect.PARTIAL_SCHEMA)
        for name in names:
            self.assertTrue(any(name in m for m in got.missing), (name, got.missing))
        self.assertTrue(any("schema" in e for e in cq_manifest.validate_manifest(got.manifest)))
        return got

    def test_a_missing_record_is_partial_and_named(self):
        run = RunDir(self)
        run.edit(RUST, lambda f: f.update(participants=f["participants"][:2] + f["participants"][3:], planned=5))
        self.assertPartial(run, "bot-003: no participant record")

    def test_a_null_join_ts_is_partial_and_named(self):
        run = RunDir(self)
        run.rust("bot-004", join_ts=None, leave_ts=None)
        got = self.assertPartial(run, "bot-004: join_ts is null")
        self.assertNotIn("bot-004", by_uid(got.manifest))

    def test_a_missing_hold_end_is_partial(self):
        run = RunDir(self)
        run.events(keep=lambda e: e["type"] != "hold_end")
        self.assertPartial(run, "no hold_end")

    def test_a_missing_teardown_start_is_partial(self):
        run = RunDir(self)
        run.events(keep=lambda e: e["type"] != "teardown_start")
        self.assertPartial(run, "no teardown_start")

    def test_cli_writes_only_the_partial_and_removes_a_stale_manifest(self):
        run = RunDir(self)
        self.assertEqual(run.cli(), cq_collect.EXIT_OK)
        run.edit(RUST, lambda f: f.update(participants=f["participants"][:2] + f["participants"][3:], planned=5))
        self.assertEqual(run.cli(), cq_collect.EXIT_PARTIAL)
        self.assertFalse(os.path.exists(os.path.join(run.path, "manifest.json")))
        self.assertTrue(os.path.exists(os.path.join(run.path, "manifest.partial.json")))
        with open(os.path.join(run.path, "missing.txt"), encoding="utf-8") as fh:
            self.assertIn("bot-003", fh.read())

    def test_cli_error_removes_a_stale_manifest(self):
        run = RunDir(self)
        self.assertEqual(run.cli(), cq_collect.EXIT_OK)
        run.probe(user_id=None)
        self.assertEqual(run.cli(), cq_collect.EXIT_ERROR)
        self.assertFalse(os.path.exists(os.path.join(run.path, "manifest.json")))


class Errors(unittest.TestCase):
    def assertCollectError(self, run, *fragments):
        with self.assertRaises(cq_collect.CollectError) as ctx:
            run.collect()
        text = "\n".join(ctx.exception.errors)
        for f in fragments:
            self.assertIn(f, text)
        self.assertEqual(run.cli(), cq_collect.EXIT_ERROR)
        return text

    def test_a_record_outside_the_plan_is_an_error(self):
        run = RunDir(self)
        run.edit(RUST, lambda f: f["participants"].append(dict(f["participants"][1], user_id="bot-099")))
        self.assertCollectError(run, "bot-099", "not in the plan")

    def test_a_null_user_id_is_an_error(self):
        run = RunDir(self)
        run.probe(user_id=None)
        self.assertCollectError(run, "c8d6004c-4137-4ea9-97f8-9011385ae1f2.json: user_id is null")

    def test_a_user_id_clash_is_an_error(self):
        run = RunDir(self)
        shutil.copytree(os.path.join(run.path, "probes", "0"), os.path.join(run.path, "probes", "1"))
        self.assertCollectError(run, f"{PROBE}: two records", "G-V8")

    def test_a_rust_file_from_another_meeting_is_an_error(self):
        run = RunDir(self)
        run.edit(RUST, lambda f: f.update(meeting_id="other"))
        self.assertCollectError(run, "is not this run's 'cqlocal1'")

    def test_observed_publishes_must_match_the_plan(self):
        run = RunDir(self)
        run.rust("bot-006", publishes={"camera": False, "mic": False, "screen": False})
        self.assertCollectError(run, "bot-006: observed publishes")

    def test_an_unverified_publishes_field_is_an_error(self):
        run = RunDir(self)
        run.edit(PROBE_RECORD, lambda r: r["unverified"].append({"field": "publishes.mic", "reason": "not found"}))
        self.assertCollectError(run, f"{PROBE}: publishes.mic is unverified")

    def test_an_unknown_network_is_an_error(self):
        run = RunDir(self)
        run.probe(network={"profile": "unknown", "shaped": None, "direction": None, "shaper": None, "params": {}})
        self.assertCollectError(run, f"{PROBE}: applied network is unknown")

    def test_plan_and_record_must_agree_on_talker_and_role(self):
        run = RunDir(self)
        run.plan_participant("bot-002", talker=True)
        run.probe(role="viewers")
        self.assertCollectError(run, "bot-002: record talker False", f"{PROBE}: record role 'viewers'")

    def test_an_event_naming_an_unplanned_participant_is_an_error(self):
        run = RunDir(self)
        run.events(add=[event("leave", "nobody", HS + 60, HS + 61)])
        self.assertCollectError(run, "names nobody, not in the plan")

    def test_an_event_outside_every_step_is_an_error(self):
        run = RunDir(self)
        run.events(add=[event("mute", PROBE, JS - 60, JS - 59)])
        self.assertCollectError(run, "outside every step window")

    def test_a_missing_image_id_is_an_error(self):
        run = RunDir(self)
        run.edit(cq_collect.IMAGES_FILE, lambda i: i.pop("rust_bot"))
        self.assertCollectError(run, "no image id for rust_bot")

    def test_a_node_without_an_alias_is_an_error(self):
        run = RunDir(self)
        run.probe(placement={"ordinal": 0, "node": "aks-pool-123"})
        text = self.assertCollectError(run, f"{PROBE}: placement.node has no alias")
        self.assertNotIn("aks-pool-123", text)


    def test_an_unconfirmed_event_is_an_error(self):
        run = RunDir(self)
        run.events(add=[event("mute", PROBE, JS + 10, None, result="timeout")])
        self.assertCollectError(run, f"mute for {PROBE} was not confirmed (result 'timeout')")

    def test_a_failed_event_is_an_error_even_with_a_confirmation_time(self):
        run = RunDir(self)
        run.events(add=[event("mute", PROBE, JS + 10, JS + 11, result="failed")])
        self.assertCollectError(run, f"mute for {PROBE} was not confirmed (result 'failed')")

    def test_the_result_goes_through_the_scorer_validator(self):
        run = RunDir(self)
        run.edit(cq_collect.PLAN_FILE, lambda pl: pl.update(run_id="Not_A_Run_Id"))
        self.assertCollectError(run, "$.run_id: must match")


def stopped(uid, wall):
    return {"type": "stopped", "wall": wall, "participants": uid if isinstance(uid, list) else [uid],
            "exit_code": 137}


class Clock(unittest.TestCase):
    """Record timestamps must fall inside the run's own window, so a skewed bot clock cannot pass."""

    def assertOutside(self, run, *fragments):
        with self.assertRaises(cq_collect.CollectError) as ctx:
            run.collect()
        text = "\n".join(ctx.exception.errors)
        for f in fragments + ("outside the run window",):
            self.assertIn(f, text)

    def test_a_join_ts_before_join_start_is_an_error(self):
        run = RunDir(self)
        run.probe(join_ts=JS - 60)
        self.assertOutside(run, f"{PROBE}: join_ts", PROBE_RECORD)

    def test_a_leave_ts_after_the_last_event_is_an_error(self):
        run = RunDir(self)
        run.rust("bot-003", leave_ts=LAST + 60)
        self.assertOutside(run, "bot-003: leave_ts", RUST)

    def test_a_rust_file_started_before_join_start_is_an_error(self):
        run = RunDir(self)
        run.edit(RUST, lambda f: f.update(started_at=JS - 300))
        self.assertOutside(run, "participants-0.json: started_at", "participants alice, bot-002")

    def test_a_media_started_at_outside_the_window_is_an_error(self):
        run = RunDir(self)
        run.edit(RUST, lambda f: f.update(media_started_at=JS - 60))
        self.assertOutside(run, "participants-0.json: media_started_at")

    def test_a_declared_skew_above_30_s_is_an_error(self):
        for skew in (30001, -30001):
            run = RunDir(self)
            run.events(keep=lambda e: e["type"] != "preflight",
                       add=[{"type": "preflight", "wall": JS - 20, "commit": "abc", "dirty": False,
                             "clock": {"sync": "ntp", "max_abs_skew_ms": skew}}])
            with self.assertRaises(cq_collect.CollectError) as ctx:
                run.collect()
            self.assertIn(f"max_abs_skew_ms {skew} exceeds 30000", "\n".join(ctx.exception.errors))
        run = RunDir(self)
        run.events(keep=lambda e: e["type"] != "preflight",
                   add=[{"type": "preflight", "wall": JS - 20, "commit": "abc", "dirty": False,
                         "clock": {"sync": "ntp", "max_abs_skew_ms": 30000}}])
        run.collect()

    def test_a_negative_declared_skew_widens_the_tolerance_too(self):
        run = RunDir(self)
        run.probe(join_ts=JS - 25)
        run.events(keep=lambda e: e["type"] != "preflight",
                   add=[{"type": "preflight", "wall": JS - 20, "commit": "abc", "dirty": False,
                         "clock": {"sync": "ntp", "max_abs_skew_ms": -24000}}])
        self.assertEqual(by_uid(run.collect().manifest)[PROBE]["join_ts"], JS - 25)

    def test_the_tolerance_is_two_seconds(self):
        run = RunDir(self)
        run.probe(join_ts=JS - 1.9)
        run.collect()
        run.probe(join_ts=JS - 2.1)
        self.assertOutside(run, f"{PROBE}: join_ts")

    def test_a_declared_skew_widens_the_tolerance(self):
        run = RunDir(self)
        run.probe(join_ts=JS - 25)
        run.events(keep=lambda e: e["type"] != "preflight",
                   add=[{"type": "preflight", "wall": JS - 20, "commit": "abc", "dirty": False,
                         "clock": {"sync": "ntp", "max_abs_skew_ms": 24000}}])
        self.assertEqual(by_uid(run.collect().manifest)[PROBE]["join_ts"], JS - 25)


class LeaveTs(unittest.TestCase):
    def test_a_probe_dead_after_its_final_leave_has_no_leave_ts_key(self):
        run = RunDir(self)
        run.events(add=[event("leave", PROBE, HS + 90, HS + 91), stopped(PROBE, HS + 100)])
        self.assertNotIn("leave_ts", by_uid(run.collect().manifest)[PROBE])

    def assertUnplanned(self, run, uid, gone):
        with self.assertRaises(cq_collect.CollectError) as ctx:
            run.collect()
        self.assertTrue(any(e.startswith(f"{uid}: left at {gone:.3f}") and "unplanned exit" in e
                            for e in ctx.exception.errors), ctx.exception.errors)

    def test_a_probe_dead_before_teardown_without_a_final_leave_is_an_error(self):
        run = RunDir(self)
        run.events(add=[stopped(PROBE, HE - 5)])
        self.assertUnplanned(run, PROBE, HE - 5)

    def test_a_leave_more_than_one_scrape_after_the_death_does_not_cover_it(self):
        run = RunDir(self)
        run.events(add=[stopped(PROBE, HS + 100), event("leave", PROBE, HS + 100 + STEP_S + 1, HS + 117)])
        self.assertUnplanned(run, PROBE, HS + 100)

    def test_a_leave_followed_by_a_rejoin_does_not_cover_a_later_death(self):
        run = RunDir(self)
        run.events(add=[event("leave", PROBE, HS + 90, HS + 91), event("rejoin", PROBE, HS + 120, HS + 121),
                        stopped(PROBE, HS + 200)])
        self.assertUnplanned(run, PROBE, HS + 200)

    def test_a_rust_leave_ts_before_teardown_without_a_final_leave_is_an_error(self):
        run = RunDir(self)
        run.rust("bot-002", leave_ts=TEARDOWN - 1, outcome="dropped: relay closed")
        self.assertUnplanned(run, "bot-002", TEARDOWN - 1)

    def test_a_participant_no_stopped_line_names_is_an_error(self):
        for uid in (PROBE, "bot-002"):
            run = RunDir(self)
            run.events(keep=lambda e: e["type"] != "stopped",
                       add=[stopped([u for u in ALL if u != uid], LAST)])
            with self.assertRaises(cq_collect.CollectError) as ctx:
                run.collect()
            self.assertEqual([e for e in ctx.exception.errors if "stopped" in e],
                             [f"{uid}: no stopped line names it, so when its process exited is unknown"])

    def test_a_probe_stopped_after_teardown_keeps_null(self):
        run = RunDir(self)
        run.events(add=[{"type": "stopped", "wall": TEARDOWN + 40, "participants": [PROBE], "exit_code": 137}])
        self.assertIsNone(by_uid(run.collect().manifest)[PROBE]["leave_ts"])

    def test_an_unclean_rust_exit_drops_only_the_unrecorded_leave_ts(self):
        run = RunDir(self)
        run.edit(RUST, lambda f: f.update(ended_at=None))
        run.rust("bot-005", leave_ts=None)
        parts = by_uid(run.collect().manifest)
        self.assertNotIn("leave_ts", parts["bot-005"])
        self.assertEqual(parts["alice"]["leave_ts"], 1790868171.8104837)

    def test_the_rust_instance_id_and_outcome_are_dropped(self):
        run = RunDir(self)
        run.rust("bot-002", outcome="dropped: relay closed")
        self.assertFalse({"instance_id", "outcome"} & set(by_uid(run.collect().manifest)["bot-002"]))


class TalkerUnmutedBeforeHold(unittest.TestCase):
    """D34."""

    def test_a_talker_muted_at_hold_start_is_an_error(self):
        run = RunDir(self)
        run.events(add=[event("mute", "alice", JS + 60, JS + 61)])
        with self.assertRaises(cq_collect.CollectError) as ctx:
            run.collect()
        self.assertIn("alice: declared talker is not unmuted strictly before hold_start of step n7 (D34)",
                      ctx.exception.errors)

    def test_an_unmute_confirmed_exactly_at_hold_start_is_an_error(self):
        run = RunDir(self)
        run.events(add=[event("mute", "alice", JS + 60, JS + 61), event("unmute", "alice", HS - 5, HS)])
        with self.assertRaises(cq_collect.CollectError) as ctx:
            run.collect()
        self.assertTrue(any("alice" in e and "D34" in e for e in ctx.exception.errors))

    def test_an_unmute_confirmed_before_hold_start_is_accepted(self):
        run = RunDir(self)
        run.events(add=[event("mute", "alice", JS + 60, JS + 61), event("unmute", "alice", HS - 5, HS - 1)])
        events = run.collect().manifest["events"]
        self.assertEqual([e["at"] for e in events if e["participants"] == ["alice"]], [JS + 60, HS - 1])


class MidHoldLeave(unittest.TestCase):
    """D35: a mid-hold leave_ts is planned only when a declared final leave covers it."""

    def resolve(self, manifest, uid):
        step = manifest["steps"][0]
        flags = []
        part = by_uid(manifest)[uid]
        resolved = cq_score._resolve_leaves({uid: part}, {}, flags, manifest["events"], step["hold_end"], STEP_S)
        return part, resolved[uid], flags

    def test_an_unconfirmed_leave_never_covers_a_mid_hold_leave_ts(self):
        run = RunDir(self)
        run.rust("bot-002", leave_ts=HS + 100, outcome="dropped: relay closed")
        run.events(add=[event("leave", "bot-002", HS + 99, None, result="timeout")])
        with self.assertRaises(cq_collect.CollectError) as ctx:
            run.collect()
        text = "\n".join(ctx.exception.errors)
        self.assertIn("leave for bot-002 was not confirmed", text)
        self.assertIn(f"bot-002: left at {HS + 100:.3f}", text)

    def test_a_confirmed_final_leave_is_written_at_t_issued_and_covers_it(self):
        run = RunDir(self)
        run.rust("bot-002", leave_ts=HS + 100)
        run.events(add=[event("leave", "bot-002", HS + 99, HS + 101)])
        manifest = run.collect().manifest
        leaves = [e for e in manifest["events"] if e["action"] == "leave"]
        self.assertEqual([(e["at"], e["step_id"], e["participants"]) for e in leaves], [(HS + 99, "n7", ["bot-002"])])
        _, resolved, flags = self.resolve(manifest, "bot-002")
        self.assertEqual(resolved["leave_ts"], HS + 100)
        self.assertEqual(flags, [])


class RoundZeroReview(unittest.TestCase):
    def errors(self, run):
        with self.assertRaises(cq_collect.CollectError) as ctx:
            run.collect()
        return "\n".join(ctx.exception.errors)

    def test_a_teardown_start_before_hold_end_is_an_error(self):
        run = RunDir(self)
        run.events(keep=lambda e: e["type"] != "teardown_start", add=[{"type": "teardown_start", "wall": HE - 30}])
        self.assertIn(f"teardown_start {HE - 30:.3f} precedes hold_end {HE:.3f} of step n7", self.errors(run))

    def test_a_teardown_start_at_hold_end_is_accepted(self):
        run = RunDir(self)
        run.events(keep=lambda e: e["type"] != "teardown_start", add=[{"type": "teardown_start", "wall": HE}])
        run.collect()

    def test_two_teardown_starts_are_an_error(self):
        run = RunDir(self)
        run.events(add=[{"type": "teardown_start", "wall": TEARDOWN + 10}])
        self.assertIn("2 teardown_start lines; exactly one is allowed", self.errors(run))

    def test_a_rust_file_with_fewer_records_than_planned_is_an_error(self):
        run = RunDir(self)
        run.edit(RUST, lambda f: f.update(planned=9))
        self.assertIn("participants-0.json: planned 9 but 6 participant records", self.errors(run))

    def test_a_rust_file_without_planned_is_an_error(self):
        run = RunDir(self)
        run.edit(RUST, lambda f: f.pop("planned"))
        self.assertIn("participants-0.json: planned None but 6 participant records", self.errors(run))

    def test_a_rust_talker_needs_media_strictly_before_hold_start(self):
        for media in (None, HS, HS + 14):
            run = RunDir(self)
            run.edit(RUST, lambda f: f.update(media_started_at=media))
            self.assertIn(f"alice: media_started_at {media} in", self.errors(run))
        run = RunDir(self)
        run.edit(RUST, lambda f: f.update(media_started_at=HS - 1))
        run.collect()

    def test_an_unverified_network_is_an_error(self):
        run = RunDir(self)
        run.probe(network={"profile": "3g", "shaped": True, "direction": "egress", "shaper": "netsim", "params": {}})
        run.edit(PROBE_RECORD, lambda r: r["unverified"].append(
            {"field": "network", "reason": "netsim applies only in a client built with --features netsim"}))
        self.assertIn(f"{PROBE}: applied network is unverified: netsim applies", self.errors(run))

    def test_an_undeclared_rejoin_is_an_error(self):
        run = RunDir(self)
        run.edit(PROBE_RECORD, lambda r: r["unverified"].append({"field": "rejoin", "reason": "ctl network change"}))
        self.assertIn("records a rejoin that no confirmed rejoin event declares", self.errors(run))
        run.events(add=[event("leave", PROBE, HS + 60, HS + 61), event("rejoin", PROBE, HS + 70, HS + 71)])
        run.collect()

    def netem_record(self, run, reason):
        run.edit(PROBE_RECORD, lambda r: r["unverified"].append({"field": "network", "reason": reason}))

    def test_a_confirmed_netem_excuses_its_unverified_network(self):
        run = RunDir(self)
        self.netem_record(run, NETEM_REASON)
        run.events(add=[dict(event("netem", PROBE, HS + 60, HS + 61), params={"op": "clear"})])
        manifest = run.collect().manifest
        self.assertEqual([e["action"] for e in manifest["events"]], ["mute", "netem"])
        self.assertEqual(by_uid(manifest)[PROBE]["network"]["shaped"], False)

    def test_a_netem_reason_needs_a_netem_event_naming_the_participant(self):
        for names in ([], ["alice"]):
            run = RunDir(self)
            self.netem_record(run, NETEM_REASON)
            run.events(add=[event("netem", u, HS + 60, HS + 61) for u in names])
            self.assertIn(f"{PROBE}: applied network is unverified: {NETEM_REASON}", self.errors(run))

    def test_only_a_netem_event_excuses_the_netem_reason(self):
        run = RunDir(self)
        self.netem_record(run, NETEM_REASON)
        run.events(add=[event("mute", PROBE, HS + 59, HS + 61)])
        self.assertIn(f"{PROBE}: applied network is unverified: {NETEM_REASON}", self.errors(run))

    def test_a_netem_event_excuses_only_the_netem_reason(self):
        run = RunDir(self)
        self.netem_record(run, NETEM_REASON + "; netsim uplink 3g also requested on top of it")
        run.events(add=[event("netem", PROBE, HS + 60, HS + 61)])
        self.assertIn(f"{PROBE}: applied network is unverified: {NETEM_REASON}; netsim", self.errors(run))

    def test_another_participants_rejoin_does_not_excuse_a_rejoin(self):
        run = RunDir(self)
        run.edit(PROBE_RECORD, lambda r: r["unverified"].append({"field": "rejoin", "reason": "ctl network change"}))
        run.events(add=[event("leave", "bot-002", HS + 60, HS + 61), event("rejoin", "bot-002", HS + 70, HS + 71)])
        self.assertIn(f"{PROBE}: {PROBE_RECORD} records a rejoin", self.errors(run).replace(run.path + os.sep, ""))

    def test_an_outcome_without_a_leave_ts_is_an_error(self):
        run = RunDir(self)
        run.edit(PROBE_RECORD, lambda r: r.update(outcome="crashed"))
        self.assertIn(f"{PROBE}: outcome 'crashed' in", self.errors(run))

    def test_a_non_numeric_wall_is_an_error(self):
        for wall in ("1790867520", None, True):
            run = RunDir(self)
            run.events(add=[{"type": "teardown_start", "wall": 0.0}])
            path = os.path.join(run.path, cq_collect.EVENTS_FILE)
            with open(path, encoding="utf-8") as fh:
                text = fh.read().replace('"wall": 0.0', f'"wall": {json.dumps(wall)}')
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(text)
            self.assertIn("each line needs a string 'type' and a finite numeric 'wall'", self.errors(run))

    def test_a_confirmed_event_with_a_non_numeric_t_issued_is_an_error(self):
        run = RunDir(self)
        run.events(add=[dict(event("mute", PROBE, JS + 10, JS + 11), t_issued="soon")])
        self.assertIn(f"mute for {PROBE} needs a numeric t_issued no later than t_confirmed", self.errors(run))


class RoundOneReview(unittest.TestCase):
    def errors(self, run):
        with self.assertRaises(cq_collect.CollectError) as ctx:
            run.collect()
        self.assertEqual(run.cli(), cq_collect.EXIT_ERROR)
        return "\n".join(ctx.exception.errors).replace(run.path + os.sep, "")

    def test_every_confirmed_action_needs_t_issued_no_later_than_t_confirmed(self):
        for action, t_issued in (("unmute", None), ("unmute", "garbage"), ("unmute", JS + 12), ("mute", JS + 12),
                                 ("unmute", JS + 11.001)):
            run = RunDir(self)
            run.events(add=[dict(event(action, PROBE, JS + 10, JS + 11), t_issued=t_issued)])
            self.assertIn(f"{action} for {PROBE} needs a numeric t_issued no later than t_confirmed", self.errors(run))

    def test_t_issued_equal_to_t_confirmed_is_accepted(self):
        run = RunDir(self)
        run.events(add=[event("unmute", PROBE, JS + 10, JS + 10)])
        run.collect()

    def test_an_unknown_line_type_does_not_widen_the_clock_window(self):
        year = 365 * 86400
        run = RunDir(self)
        run.events(add=[{"type": "heartbeat", "wall": LAST + year}])
        run.rust("bot-003", leave_ts=LAST + year - 10)
        self.assertIn("bot-003: leave_ts", self.errors(run))

    def test_a_stop_mid_hold_counts_even_when_leave_ts_is_after_teardown(self):
        run = RunDir(self)
        run.events(add=[stopped("bot-002", HS + 100)])
        self.assertIn(f"bot-002: left at {HS + 100:.3f}", self.errors(run))

    def two_steps(self, run):
        run.edit(cq_collect.PLAN_FILE, lambda pl: pl["steps"].insert(
            0, {"step_id": "warm", "n_target": 1, "headline": False}))
        run.events(keep=lambda e: e["type"] != "join_start",
                   add=[{"type": "join_start", "wall": JS, "step_id": "warm"},
                        {"type": "hold_start", "wall": JS + 40, "step_id": "warm"},
                        {"type": "hold_end", "wall": JS + 45, "step_id": "warm"},
                        {"type": "join_start", "wall": JS + 45, "step_id": "n7"}])

    def test_the_clock_window_opens_at_the_first_step_join_start(self):
        run = RunDir(self)
        self.two_steps(run)
        self.assertEqual(by_uid(run.collect().manifest)[PROBE]["join_ts"], 1790867530.291027)
        run.probe(join_ts=JS - 60)
        self.assertIn(f"{PROBE}: join_ts", self.errors(run))

    def test_an_event_with_no_participants_is_an_error(self):
        run = RunDir(self)
        run.events(add=[dict(event("mute", PROBE, JS + 10, JS + 11), participants=[])])
        self.assertIn("mute needs a non-empty list of user ids", self.errors(run))

    def test_a_user_id_planned_twice_is_an_error(self):
        run = RunDir(self)
        run.edit(cq_collect.PLAN_FILE, lambda pl: pl["participants"].append(dict(pl["participants"][1])))
        self.assertIn("plan: user_id 'alice' planned twice", self.errors(run))

    def test_a_rust_file_with_a_wrong_or_absent_kind_is_an_error(self):
        for change in (lambda f: f.update(kind="rust-bot"), lambda f: f.pop("kind")):
            run = RunDir(self)
            run.edit(RUST, change)
            self.assertIn(f"{RUST}: not a rust-bot-participants file", self.errors(run))

    def test_a_non_numeric_leave_ts_is_an_error(self):
        run = RunDir(self)
        run.rust("bot-002", leave_ts="late")
        self.assertIn(f"bot-002: leave_ts in {RUST} must be a finite number or null", self.errors(run))

    def test_a_non_numeric_join_ts_is_an_error_not_a_partial(self):
        run = RunDir(self)
        run.rust("bot-002", join_ts="early")
        self.assertIn(f"bot-002: join_ts in {RUST} must be a finite number or null", self.errors(run))

    def test_malformed_shapes_exit_3_not_a_traceback(self):
        def plan_part(**kw):
            return lambda run: run.plan_participant("alice", **kw)

        def mark_step_id(v):
            return lambda run: run.events(keep=lambda e: e["type"] != "hold_end",
                                          add=[{"type": "hold_end", "wall": HE, "step_id": v}])

        def append_bytes(data):
            def apply(run):
                with open(os.path.join(run.path, cq_collect.EVENTS_FILE), "ab") as fh:
                    fh.write(data)
            return apply
        cases = [
            (plan_part(steps=None), "plan participants[1]: user_id must be a string and steps a list"),
            (plan_part(user_id=["alice"]), "plan participants[1]: user_id must be a string and steps a list"),
            (lambda run: run.edit(cq_collect.PLAN_FILE, lambda pl: pl.update(steps=True)),
             "plan: steps must be a list"),
            (lambda run: run.edit(cq_collect.PLAN_FILE, lambda pl: pl.update(participants=True)),
             "plan: participants must be a list"),
            (lambda run: run.edit(cq_collect.PLAN_FILE, lambda pl: pl["steps"][0].update(step_id=["n7"])),
             "plan steps[0]: needs a string step_id"),
            (mark_step_id(["n7"]), "events: hold_end needs a string step_id"),
            (mark_step_id({"id": "n7"}), "events: hold_end needs a string step_id"),
            (lambda run: run.edit(PROBE_RECORD, lambda r: r.update(unverified=True)), "unverified must be a list"),
            (lambda run: run.edit(cq_collect.PLAN_FILE, lambda pl: pl.update(node_aliases=True)),
             "plan: node_aliases must map node names to alias strings"),
            (lambda run: run.edit(cq_collect.PLAN_FILE, lambda pl: pl.update(node_aliases={"a": 1})),
             "plan: node_aliases must map node names to alias strings"),
            (lambda run: run.events(add=[{"type": "heartbeat", "wall": 10 ** 400}]),
             "each line needs a string 'type' and a finite numeric 'wall'"),
            (lambda run: run.rust("bot-002", join_ts=10 ** 400),
             f"bot-002: join_ts in {RUST} must be a finite number or null"),
            (lambda run: run.probe(placement={"node": []}), f"{PROBE}: placement.node must be a string or null"),
            (append_bytes(b'{"type": "heartbeat", "wall": 1790868100.0, "note": "\xff"}\n'),
             f"{cq_collect.EVENTS_FILE}: 'utf-8' codec can't decode byte 0xff"),
        ]
        for apply, msg in cases:
            run = RunDir(self)
            apply(run)
            self.assertIn(msg, self.errors(run))

    def test_a_nan_in_a_partial_manifest_is_an_error(self):
        run = RunDir(self)
        run.events(keep=lambda e: e["type"] not in ("preflight", "hold_end"),
                   add=[{"type": "preflight", "wall": JS - 20, "commit": "abc", "dirty": False,
                         "clock": {"sync": "ntp", "max_abs_skew_ms": float("nan")}}])
        self.assertIn("$.clock.max_abs_skew_ms: NaN and infinity are not numbers here", self.errors(run))

    def test_a_stopped_line_must_name_planned_user_ids(self):
        for parts, msg in (([{}], "stopped needs a list of user ids"), (["nobody"], "stopped names nobody")):
            run = RunDir(self)
            run.events(add=[stopped(parts, LAST)])
            self.assertIn(msg, self.errors(run))

    def test_a_non_object_publishes_is_an_error(self):
        run = RunDir(self)
        run.rust("alice", publishes="on")
        self.assertIn("alice: observed publishes on differ", self.errors(run))

    def test_the_fleet_must_match_the_records_source(self):
        run = RunDir(self)
        run.rust("bot-002", fleet="browser")
        run.plan_participant("bot-002", fleet="browser")
        self.assertIn(f"bot-002: record fleet 'browser' but {RUST} is a rust record", self.errors(run))


class AntonioRoundOne(unittest.TestCase):
    def errors(self, run):
        with self.assertRaises(cq_collect.CollectError) as ctx:
            run.collect()
        return "\n".join(ctx.exception.errors).replace(run.path + os.sep, "")

    def per_node(self, run, per_node, aliases):
        run.edit(cq_collect.PLAN_FILE, lambda pl: pl.update(node_aliases=aliases))
        run.events(keep=lambda e: e["type"] != "preflight",
                   add=[{"type": "preflight", "wall": JS - 20, "commit": "abc", "dirty": False,
                         "clock": {"sync": "ntp", "max_abs_skew_ms": 0, "per_node": per_node}}])

    def test_a_usage_error_exits_3_not_the_partial_code(self):
        for argv in ([], ["--run-dir", "x", "--bogus"]):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as ctx:
                cq_collect.main(argv)
            self.assertEqual(ctx.exception.code, cq_collect.EXIT_ERROR)

    def test_a_missing_out_dir_exits_3_and_is_not_created(self):
        run = RunDir(self)
        out = os.path.join(run.path, "no-such-dir")
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as ctx:
            cq_collect.main(["--run-dir", run.path, "--out-dir", out])
        self.assertEqual(ctx.exception.code, cq_collect.EXIT_ERROR)
        self.assertFalse(os.path.exists(out))

    def test_clock_per_node_is_aliased(self):
        run = RunDir(self)
        self.per_node(run, [{"node": "aks-pool-7", "skew_ms": 1.0}, {"node": "node-b", "skew_ms": 0.5}],
                      {"aks-pool-7": "node-a", "aks-pool-8": "node-b"})
        self.assertEqual(run.collect().manifest["clock"]["per_node"],
                         [{"node": "node-a", "skew_ms": 1.0}, {"node": "node-b", "skew_ms": 0.5}])

    def test_reserved_node_names_need_no_alias(self):
        for node, environment in (("local", "local"), ("ci", "local"), ("ci-smoke", "ci-smoke")):
            run = RunDir(self)
            self.per_node(run, [{"node": node, "skew_ms": 0}], {})
            run.edit(cq_collect.PLAN_FILE, lambda pl: pl.update(environment=environment))
            self.assertEqual(run.collect().manifest["clock"]["per_node"], [{"node": node, "skew_ms": 0}])

    def test_any_other_unmapped_node_name_is_an_error(self):
        run = RunDir(self)
        self.per_node(run, [{"node": "wsl-host-7", "skew_ms": 0}], {})
        text = self.errors(run)
        self.assertIn("a preflight clock.per_node node has no alias", text)
        self.assertNotIn("wsl-host-7", text)

    def test_a_netem_reason_with_an_impossible_date_is_an_error(self):
        bad = "set by POST /netem at 2026-13-45T25:61:61.000Z; the shaping params are not read back"
        run = RunDir(self)
        run.edit(PROBE_RECORD, lambda r: r["unverified"].append({"field": "network", "reason": bad}))
        run.events(add=[event("netem", PROBE, HS + 60, HS + 61)])
        self.assertIn(f"{PROBE}: applied network is unverified: {bad}", self.errors(run))
        self.assertEqual(run.cli(), cq_collect.EXIT_ERROR)

    def test_an_unaliased_or_malformed_per_node_is_an_error(self):
        for per_node, msg in (([{"node": "aks-pool-9", "skew_ms": 1.0}], "per_node node has no alias"),
                              ([{"skew_ms": 1.0}], "per_node must be a list of objects with a string node"),
                              ("local", "per_node must be a list of objects with a string node")):
            run = RunDir(self)
            self.per_node(run, per_node, {"aks-pool-7": "node-a"})
            text = self.errors(run)
            self.assertIn(msg, text)
            self.assertNotIn("aks-pool-9", text)

    def test_a_netem_reason_stamped_outside_the_event_window_is_an_error(self):
        late = "set by POST /netem at 2026-10-01T15:20:00.000Z; the shaping params are not read back"
        run = RunDir(self)
        run.edit(PROBE_RECORD, lambda r: r["unverified"].append({"field": "network", "reason": late}))
        run.events(add=[event("netem", PROBE, HS + 60, HS + 61)])
        self.assertIn(f"{PROBE}: applied network is unverified: {late}", self.errors(run))

    def test_a_netem_reason_within_the_clock_tolerance_is_excused(self):
        run = RunDir(self)
        reason = "set by POST /netem at 2026-10-01T15:15:27.900Z; the shaping params are not read back"
        run.edit(PROBE_RECORD, lambda r: r["unverified"].append({"field": "network", "reason": reason}))
        run.events(add=[event("netem", PROBE, HS + 60, HS + 61)])
        run.collect()

    def test_a_browser_record_moved_into_a_rust_file_is_an_error(self):
        run = RunDir(self)
        with open(os.path.join(run.path, PROBE_RECORD), encoding="utf-8") as fh:
            probe = json.load(fh)["participant"]
        os.remove(os.path.join(run.path, PROBE_RECORD))
        run.edit(RUST, lambda f: f.update(participants=f["participants"] + [probe], planned=7))
        self.assertIn(f"{PROBE}: record fleet 'browser' but {RUST} is a rust record", self.errors(run))


class Code(unittest.TestCase):
    def test_a_dirty_tree_marks_the_commit(self):
        run = RunDir(self)
        run.events(keep=lambda e: e["type"] != "preflight",
                   add=[{"type": "preflight", "wall": JS - 20, "commit": "abc", "dirty": True,
                         "clock": {"sync": "unknown"}}])
        self.assertEqual(run.collect().manifest["code"]["commit"], "abc-dirty")


if __name__ == "__main__":
    unittest.main()
