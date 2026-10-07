#!/usr/bin/env python3
"""Integration and property tests for the run-manifest collector: the collector CLI feeds the real scorer CLI,
whose Prometheus is answered in-process by the scorer suite's World fixture."""

import contextlib
import copy
import glob
import io
import json
import os
import random
import re
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cq_collect  # noqa: E402
import cq_manifest  # noqa: E402
import test_call_quality_score as sq  # noqa: E402

JOIN, HS, HE, STEP = sq.JOIN, sq.HS, sq.HE, sq.STEP
TEARDOWN = HE + 30
MEETING = "scale-t1"
OBS = [f"o{i:02d}" for i in range(10)]
TALKER = "t1"
SEEDS = range(int(os.environ.get("CQ_PROPERTY_SEEDS", "8")))
NETWORK = {"profile": "none", "shaped": False, "direction": "none", "shaper": "none", "params": {}}
FIXTURE = os.path.join(HERE, "testdata", "cq_collect_20261001")


def _dump(path, obj):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(obj, fh)


def event(action, uid, t_issued, t_confirmed, result="ok"):
    return {"type": "event", "wall": t_confirmed or t_issued, "action": action, "participants": [uid],
            "params": {}, "t_issued": t_issued, "t_confirmed": t_confirmed, "result": result}


class Run:
    """A generated run folder plus the truth its Prometheus world shows (`gone`, `arrived`, `mic_on_at`)."""

    def __init__(self, tc, rng):
        tmp = tempfile.mkdtemp()
        tc.addCleanup(shutil.rmtree, tmp)
        self.dir, self.out = os.path.join(tmp, "run"), os.path.join(tmp, "out")
        os.makedirs(self.out)
        self.viewers = [f"v{i:02d}" for i in range(rng.randint(1, 3))]
        self.gone, self.arrived, self.mic_on_at = {}, {}, None
        aliases = {"pool-a-0": "node-a"} if rng.random() < 0.5 else {}
        joins = {u: round(rng.uniform(JOIN + 1, HS - 31), 3) for u in OBS + [TALKER] + self.viewers}
        self.plan = {"run_id": "prop-t1", "meeting_id": MEETING, "environment": "test",
                     "scenario": {"file": "scenarios/prop.yaml", "sha256": "0" * 64}, "node_aliases": aliases,
                     "steps": [{"step_id": "s1", "n_target": len(joins), "headline": True}], "participants": []}
        self.browser = {}
        for i, u in enumerate(OBS):
            p = self.planned(u, "browser", "observers", True, False, {"camera": True, "mic": False, "screen": False})
            rec = {k: copy.deepcopy(v) for k, v in p.items() if k != "steps"}
            rec.update(network=copy.deepcopy(NETWORK), transport_intended=rng.choice(cq_manifest.TRANSPORTS),
                       join_ts=joins[u], leave_ts=None, placement={"ordinal": i, "node": next(iter(aliases), None)},
                       stagger_ms=rng.choice([None, 250]))
            self.browser[u] = (rng.randint(0, 2), {"schema": cq_collect.BROWSER_SCHEMA, "bot_id": f"bot-{u}",
                                                   "outcome": None, "unverified": [], "participant": rec})
        self.planned(TALKER, "rust", "talkers", False, True, {"camera": True, "mic": True, "screen": False})
        for v in self.viewers:
            self.planned(v, "rust", "viewers", False, False, {"camera": False, "mic": False, "screen": False})
        self.rust = [{"kind": cq_collect.RUST_KIND, "manifest_schema": cq_manifest.SCHEMA, "meeting_id": MEETING,
                      "id_prefix": None, "started_at": JOIN + 0.5, "media_started_at": JOIN + 16,
                      "ended_at": TEARDOWN + 45, "participants": []}
                     for _ in range(rng.randint(1, 2))]
        for p in self.plan["participants"][len(OBS):]:
            rec = {k: copy.deepcopy(p[k]) for k in ("user_id", "fleet", "observer", "talker", "publishes")}
            rec.update(role="talker" if p["talker"] else "viewer", network=copy.deepcopy(NETWORK),
                       transport_intended="websocket", join_ts=joins[p["user_id"]],
                       leave_ts=round(rng.uniform(TEARDOWN + 5, TEARDOWN + 40), 3), instance_id=f"i-{p['user_id']}")
            rng.choice(self.rust)["participants"].append(rec)
        self.events = [
            {"type": "preflight", "wall": JOIN - 20, "commit": "0123abc", "dirty": False,
             "clock": {"sync": "unknown", "max_abs_skew_ms": 0, "per_node": [{"node": "local", "skew_ms": 0}]}},
            {"type": "join_start", "wall": JOIN, "step_id": "s1"},
            {"type": "hold_start", "wall": HS, "step_id": "s1"},
            {"type": "hold_end", "wall": HE, "step_id": "s1"},
            {"type": "teardown_start", "wall": TEARDOWN},
            {"type": "stopped", "wall": TEARDOWN + 50, "participants": list(joins), "exit_code": 0}]
        if rng.random() < 0.5:
            o = rng.choice(OBS)
            self.events += [event("unmute", o, JOIN + 5, JOIN + 6), event("mute", o, JOIN + 8, JOIN + 9)]
        self.images = {k: f"sha256:{k}" for k in cq_collect.IMAGE_KEYS}

    def planned(self, uid, fleet, role, observer, talker, publishes):
        p = {"user_id": uid, "fleet": fleet, "role": role, "observer": observer, "talker": talker,
             "publishes": publishes, "steps": ["s1"]}
        self.plan["participants"].append(p)
        return p

    def uids(self):
        return OBS + [TALKER] + self.viewers

    def plan_of(self, uid):
        return next(p for p in self.plan["participants"] if p["user_id"] == uid)

    def rust_file_of(self, uid):
        return next(f for f in self.rust if any(p["user_id"] == uid for p in f["participants"]))

    def record(self, uid):
        if uid in self.browser:
            return self.browser[uid][1]["participant"]
        return next(p for p in self.rust_file_of(uid)["participants"] if p["user_id"] == uid)

    def remove_record(self, uid):
        if self.browser.pop(uid, None) is None:
            f = self.rust_file_of(uid)
            f["participants"] = [p for p in f["participants"] if p["user_id"] != uid]

    def pick(self, k, rng):
        """A browser observer for even k, a Rust participant for odd k."""
        return rng.choice(OBS if k % 2 == 0 else [TALKER] + self.viewers)

    def shift(self, uid, skew):
        """Moves every timestamp in the file holding uid's record by skew seconds."""
        browser = uid in self.browser
        for rec in [self.record(uid)] if browser else self.rust_file_of(uid)["participants"]:
            for key in ("join_ts", "leave_ts"):
                if rec[key] is not None:
                    rec[key] += skew
        if not browser:
            f = self.rust_file_of(uid)
            for key in ("started_at", "media_started_at", "ended_at"):
                f[key] += skew

    def write(self):
        shutil.rmtree(self.dir, ignore_errors=True)
        _dump(os.path.join(self.dir, cq_collect.PLAN_FILE), self.plan)
        _dump(os.path.join(self.dir, cq_collect.IMAGES_FILE), self.images)
        for k, f in enumerate(self.rust):
            _dump(os.path.join(self.dir, "rust", f"participants-{k}.json"), {"planned": len(f["participants"]), **f})
        for n, (probe, rec) in enumerate(self.browser.values()):
            _dump(os.path.join(self.dir, "probes", str(probe), "participants", f"{n:02d}-{rec['bot_id']}.json"), rec)
        with open(os.path.join(self.dir, cq_collect.EVENTS_FILE), "w", encoding="utf-8") as fh:
            fh.writelines(json.dumps(e) + "\n" for e in sorted(self.events, key=lambda e: e["wall"]))

    def world(self):
        w = sq.healthy_world(OBS, [TALKER], rust_extra=len(self.viewers))
        for v in self.viewers:
            w.presence(v)
        keep = [(u, lambda t, g=g: t < g) for u, g in self.gone.items()]
        keep += [(u, lambda t, a=a: t >= a) for u, a in self.arrived.items()]
        for series in w.series.values():
            for s in series:
                lab = s["metric"]
                for uid, ok in keep:
                    if uid in (lab.get("peer_id"), lab.get("from_peer")) or lab.get("to_peer") == sq.sess(uid):
                        s["values"] = [v for v in s["values"] if ok(v[0])]
        if self.mic_on_at is not None:
            for s in w.series[sq.M_PPS]:
                if s["metric"]["to_peer"] == sq.sess(TALKER):
                    s["values"] = [[t, v if t >= self.mic_on_at else "0"] for t, v in s["values"]]
        return w


def score_cli(manifest_path, world, out_dir):
    shutil.rmtree(out_dir, ignore_errors=True)
    rc, err = sq.main_with_stderr(["--manifest", manifest_path, "--prom-url", "http://prom.invalid", "--out-dir",
                                   out_dir, "--no-pseudonymise"], transport=world.transport([]), environ={})
    path = os.path.join(out_dir, "result.json")
    return rc, (sq.read_json(path) if os.path.exists(path) else None), err


def collect_and_score(run):
    """The runner's path (R3): collector CLI into run.out, then the scorer CLI on whichever manifest is there."""
    run.write()
    err = io.StringIO()
    with contextlib.redirect_stderr(err):
        rc = cq_collect.main(["--run-dir", run.dir, "--out-dir", run.out])
    kind = {cq_collect.EXIT_OK: "manifest", cq_collect.EXIT_PARTIAL: "partial", cq_collect.EXIT_ERROR: "error"}[rc]
    for name in ("manifest.json", "manifest.partial.json"):
        path = os.path.join(run.out, name)
        if os.path.exists(path):
            scorer_rc, result, _ = score_cli(path, run.world(), os.path.join(run.out, "score"))
            return kind, err.getvalue(), result, scorer_rc
    return kind, err.getvalue(), None, None


def manifest_parts(run):
    return {p["user_id"]: p for p in sq.read_json(os.path.join(run.out, "manifest.json"))["participants"]}


def failing(result):
    return "\n".join(f"{g['gate']}: {g.get('detail', '')}" for s in result["steps"]
                     for g in s["validity"] + s["quality_gates"] if g["status"] == "fail")


class NeverPassUnderFaults(unittest.TestCase):
    """Each seed proves its fault-free run PASSes, then reruns the same out dir with one fault injected."""

    def check(self, fault, expect, seeds=SEEDS):
        for k in seeds:
            rng = random.Random(f"{fault.__name__}/{k}")
            run = Run(self, rng)
            kind, err, result, _ = collect_and_score(run)
            self.assertEqual((kind, result and result["verdict"]), ("manifest", "PASS"),
                             (k, err, result and failing(result)))
            self.assertTrue(all("leave_ts" in p and p["leave_ts"] is None
                                for u, p in manifest_parts(run).items() if u in OBS))
            named, post = fault(run, rng, k)
            kind, err, result, scorer_rc = collect_and_score(run)
            with self.subTest(seed=k, named=named):
                self.assertNotEqual(result and result["verdict"], "PASS", result and failing(result))
                self.assertIn(kind, (expect,) if isinstance(expect, str) else expect, err)
                self.assertIn(named, failing(result) if kind == "manifest" else err)
                if kind != "manifest":
                    self.assertFalse(os.path.exists(os.path.join(run.out, "manifest.json")))
                if kind == "partial":
                    self.assertEqual(scorer_rc, sq.cli.EXIT_ERROR)
                if post:
                    post(run, err)

    def test_a_missing_record_is_partial_and_named(self):
        def fault(run, rng, k):
            uid = run.pick(k, rng)
            run.remove_record(uid)
            run.gone[uid] = JOIN
            return f"{uid}: no participant record", None
        self.check(fault, "partial")

    def test_a_null_join_ts_is_partial_and_named(self):
        def fault(run, rng, k):
            uid = run.pick(k, rng)
            run.record(uid).update(join_ts=None, leave_ts=None)
            run.gone[uid] = JOIN
            return f"{uid}: join_ts is null", None
        self.check(fault, "partial")

    def test_a_missing_step_mark_or_teardown_is_partial(self):
        def fault(run, rng, k):
            mark = (cq_collect.STEP_MARKS + ("teardown_start",))[k % 4]
            run.events = [e for e in run.events if e["type"] != mark]
            return f"no {mark}", None
        self.check(fault, "partial")

    def test_a_duplicate_clashing_or_null_user_id_is_an_error(self):
        def fault(run, rng, k):
            uid = run.pick(k // 3, rng)
            how = ("copy", "clash", "null")[k % 3]
            if how == "null":
                run.record(uid)["user_id"] = None
                return "user_id is null; the session cannot be identified (G-V8)", None
            if how == "clash":
                run.record(rng.choice([u for u in run.uids() if u != uid]))["user_id"] = uid
            elif uid in run.browser:
                probe, rec = run.browser[uid]
                run.browser[uid + "#2"] = ((probe + 1) % 3, dict(copy.deepcopy(rec), bot_id="bot-copy"))
            else:
                rng.choice(run.rust)["participants"].append(copy.deepcopy(run.record(uid)))
            return f"{uid}: two records", None
        self.check(fault, "error", range(max(len(SEEDS), 12)))

    def test_an_unconfirmed_planned_leave_is_an_error(self):
        """D35: n_target already excludes the planned leaver, the reply never confirmed it, the bot left anyway."""
        def fault(run, rng, k):
            uid = rng.choice(run.viewers if k % 2 == 0 else OBS)
            at = round(rng.uniform(HS + STEP, HE - 4 * STEP), 3)
            run.events.append(event("leave", uid, at, *((None, "timeout"), (at + 1, "http-500"), (None, "ok"))[k % 3]))
            run.plan["steps"][0]["n_target"] -= 1
            run.record(uid)["leave_ts"] = at + 1
            run.gone[uid] = at + 1
            return uid, None
        self.check(fault, "error")

    def test_a_failed_unmute_of_a_talker_muted_at_join_is_an_error(self):
        def fault(run, rng, k):
            run.plan_of(TALKER)["publishes"]["mic"] = run.record(TALKER)["publishes"]["mic"] = False
            at = round(rng.uniform(JOIN + 1, HS - 10), 3)
            run.events.append(event("unmute", TALKER, at, (None, at + 1)[k % 2], ("timeout", "http-409")[k % 2]))
            run.mic_on_at = HE + 1
            return f"{TALKER}: declared talker is not unmuted strictly before hold_start", None
        self.check(fault, "error")

    def test_a_process_that_died_mid_hold_without_a_final_leave_is_an_error(self):
        def fault(run, rng, k):
            uid = run.pick(k, rng)
            died = round(rng.uniform(HS + STEP, HE - 3 * STEP), 3)
            run.events.append({"type": "stopped", "wall": died, "participants": [uid], "exit_code": 137})
            if uid not in run.browser:
                run.rust_file_of(uid)["ended_at"] = None
                run.record(uid)["leave_ts"] = None
            run.gone[uid] = died
            return f"{uid}: left at {died:.3f}", None
        self.check(fault, "error")

    def test_a_process_that_died_in_the_last_scrape_of_the_hold_is_an_error(self):
        """Without the collector rule this PASSes: the scorer reads a leave within one scrape of hold_end as planned."""
        def fault(run, rng, k):
            uid = run.pick(k, rng)
            died = round(rng.uniform(HE - STEP + 1, HE - 1), 3)
            if uid in run.browser:
                run.events.append({"type": "stopped", "wall": died, "participants": [uid], "exit_code": 137})
            else:
                run.record(uid).update(leave_ts=died, outcome="dropped: relay closed")
            run.gone[uid] = died
            return f"{uid}: left at {died:.3f}", None
        self.check(fault, "error")

    def test_a_teardown_start_before_hold_end_is_an_error(self):
        """Without the collector rule this PASSes: an early teardown_start lets a last-scrape drop pass as teardown."""
        def fault(run, rng, k):
            uid = rng.choice(run.viewers)
            run.events = [e for e in run.events if e["type"] != "teardown_start"]
            run.events.append({"type": "teardown_start", "wall": HE - 30})
            run.record(uid).update(leave_ts=HE - 5, outcome="dropped: relay closed")
            run.gone[uid] = HE - 5
            return f"teardown_start {HE - 30:.3f} precedes hold_end", None
        self.check(fault, "error")

    def test_a_rust_talker_whose_media_starts_after_hold_start_is_an_error(self):
        """D34. Without the collector rule this PASSes: the scorer has no talker-silence signal for 14-30 s."""
        def fault(run, rng, k):
            start = HS + (14, 30)[k % 2]
            run.rust_file_of(TALKER)["media_started_at"] = start
            run.mic_on_at = start
            return f"{TALKER}: media_started_at {start}", None
        self.check(fault, "error")

    def test_a_record_clock_minutes_behind_is_an_error(self):
        """Without the collector rule this PASSes: an early join_ts is never late for G-V15."""
        def fault(run, rng, k):
            uid = rng.choice(OBS)
            run.record(uid)["join_ts"] = round(JOIN + 5 - rng.uniform(60, 600), 3)
            return f"{uid}: join_ts", None
        self.check(fault, "error")

    def test_a_failed_event_with_no_effect_is_an_error(self):
        """Without the collector rule this PASSes: the failed call is dropped and nothing in the data moves."""
        def fault(run, rng, k):
            uid = rng.choice(OBS if k % 2 == 0 else run.viewers)
            action = "mute" if k % 2 == 0 else "leave"
            at = round(rng.uniform(JOIN + 1, HS - 31) if k % 2 == 0 else rng.uniform(HS + STEP, HE - 4 * STEP), 3)
            run.events.append(event(action, uid, at, *((None, "timeout"), (at + 1, "failed"))[k // 2 % 2]))
            return f"{action} for {uid} was not confirmed", None
        self.check(fault, "error")

    def test_a_skewed_record_clock_never_passes(self):
        """Ahead: the record's joins look late. Behind: an on-time join_ts hides a real join inside the hold."""
        def fault(run, rng, k):
            uid = run.pick(k // 2, rng)
            if k % 2 == 0:
                run.shift(uid, round(rng.uniform(HS - 30 - run.record(uid)["join_ts"] + 1, 400), 3))
            else:
                arrived = round(rng.uniform(HS, HS + 200), 3)
                run.record(uid)["join_ts"] = arrived
                run.shift(uid, round(rng.uniform(JOIN + 1, HS - 31), 3) - arrived)
                run.arrived[uid] = arrived
            return uid, None
        self.check(fault, ("manifest", "error"))

    def test_an_extra_unknown_record_is_an_error(self):
        def fault(run, rng, k):
            stranger = f"stranger-{rng.randint(0, 99)}"
            donor = run.pick(k, rng)
            if donor in run.browser:
                probe, rec = copy.deepcopy(run.browser[donor])
                rec["participant"]["user_id"] = stranger
                run.browser[stranger] = (probe, dict(rec, bot_id="bot-stranger"))
            else:
                run.rust_file_of(donor)["participants"].append(dict(copy.deepcopy(run.record(donor)),
                                                                    user_id=stranger))
            return f"{stranger}: record", None
        self.check(fault, "error")

    def test_a_talker_unmuted_at_or_after_hold_start_is_an_error(self):
        """D34: issued before hold_start, confirmed at or after it."""
        def fault(run, rng, k):
            if k % 2 == 0:
                run.plan_of(TALKER)["publishes"]["mic"] = run.record(TALKER)["publishes"]["mic"] = False
            else:
                run.events.append(event("mute", TALKER, JOIN + 10, JOIN + 11))
            confirmed = HS if k % 4 < 2 else round(rng.uniform(HS + 1, HE - 4 * STEP), 3)
            run.events.append(event("unmute", TALKER, round(rng.uniform(HS - 20, HS - 1), 3), confirmed))
            run.mic_on_at = confirmed
            return f"{TALKER}: declared talker is not unmuted strictly before hold_start", None
        self.check(fault, "error")

    def test_a_mid_hold_leave_with_no_event_never_passes(self):
        """D35."""
        def fault(run, rng, k):
            uid = run.pick(k, rng)
            left = round(rng.uniform(HS + STEP, HE - 3 * STEP), 3)
            run.record(uid)["leave_ts"] = left
            run.gone[uid] = left
            return uid, None
        self.check(fault, "error")

    def test_a_record_contradicting_the_plan_or_run_is_an_error(self):
        def contradiction(run, rng, k):
            obs, rust = rng.choice(OBS), rng.choice([TALKER] + run.viewers)
            uid = (obs, rust)[k % 2]
            rec = run.record(uid)
            variants = [
                lambda: rec["publishes"].update(camera=not rec["publishes"]["camera"]) or f"{uid}: observed publishes",
                lambda: run.browser[obs][1]["unverified"].append({"field": "publishes.mic", "reason": "not seen"})
                or f"{obs}: publishes.mic is unverified",
                lambda: rec["network"].update(profile="unknown") or f"{uid}: applied network is unknown",
                lambda: rec["network"].update(shaped=None) or f"{uid}: applied network is unknown",
                lambda: run.record(obs).update(placement={"ordinal": 0, "node": "pool-z-9"})
                or f"{obs}: placement.node has no alias",
                lambda: rec.update(talker=not rec["talker"]) or f"{uid}: record talker",
                lambda: run.record(obs).update(observer=False) or f"{obs}: record observer",
                lambda: run.record(obs).update(role="talkers") or f"{obs}: record role",
                lambda: run.record(rust).update(fleet="browser") or f"{rust}: record fleet",
                lambda: run.rust_file_of(rust).update(meeting_id="other") or f"missing: {rust}: no participant record",
                lambda: run.images.pop("relay_wt") and "no image id for relay_wt",
                lambda: run.events.append(event("mute", "nobody", HS + 60, HS + 61)) or "names nobody, not in the plan",
                lambda: run.events.append(event("camera-off", uid, JOIN - 30, JOIN - 29))
                or f"for {uid} is outside every step window",
                lambda: rec.update(transport_intended="quic") or "transport_intended: must be one of",
                lambda: run.events.append(dict(run.events[0], wall=JOIN - 10)) or "expected one preflight line",
                lambda: run.events[0].pop("commit") and "preflight needs a commit string",
                lambda: run.events.append({"type": "hold_start", "wall": HS + 100, "step_id": "s1"})
                or "hold_start for step 's1' recorded twice",
                lambda: run.events.append({"type": "hold_end", "wall": HE + 5, "step_id": "s9"})
                or "names unknown step 's9'",
            ]
            named = variants[k % len(variants)]()
            return named, (lambda run, err: self.assertNotIn("pool-z-9", err)) if "no alias" in named else None
        self.check(contradiction, "error", range(18))


class ConfirmedEventsStillPass(unittest.TestCase):
    """Controls: the fail-closed rules must not refuse a run whose events were confirmed in time."""

    def run_with(self, add):
        rng = random.Random(add.__name__)
        run = Run(self, rng)
        add(run, rng)
        kind, err, result, _ = collect_and_score(run)
        self.assertEqual((kind, result and result["verdict"]), ("manifest", "PASS"), err or failing(result))
        return run

    def test_a_planned_leave_confirmed_late_is_credited_at_t_issued(self):
        def add(run, rng):
            uid = rng.choice(run.viewers)
            at = round(rng.uniform(HS + STEP, HE - 4 * STEP), 3)
            run.events.append(event("leave", uid, at, at + 2 * STEP))
            run.plan["steps"][0]["n_target"] -= 1
            run.record(uid)["leave_ts"] = at + 1
            run.gone[uid] = at + 1
        self.run_with(add)

    def test_a_process_stopped_after_its_confirmed_final_leave_passes_with_no_leave_ts(self):
        left = []

        def add(run, rng):
            left.append(rng.choice(OBS))
            at = round(rng.uniform(HS + STEP, HE - 4 * STEP), 3)
            run.events += [event("leave", left[0], at, at + 1),
                           {"type": "stopped", "wall": at + 5, "participants": [left[0]], "exit_code": 137}]
            run.plan["steps"][0]["n_target"] -= 1
            run.gone[left[0]] = at + 1
        self.assertNotIn("leave_ts", manifest_parts(self.run_with(add))[left[0]])

    def test_a_talker_unmuted_strictly_before_hold_start_passes(self):
        def add(run, rng):
            run.plan_of(TALKER)["publishes"]["mic"] = run.record(TALKER)["publishes"]["mic"] = False
            run.events.append(event("unmute", TALKER, HS - 3, HS - 1))
            run.mic_on_at = HS - 1
        self.run_with(add)


def fixture_world(m):
    """Healthy series for a manifest's participants at its own times and meeting id."""
    step = m["steps"][0]
    js, hs, he = step["join_start"], step["hold_start"], step["hold_end"]
    parts = m["participants"]
    w = sq.World()
    for p in parts:
        w.presence(p["user_id"], lo=js, hi=he)
    for o in (p["user_id"] for p in parts if p["observer"]):
        for p in (p for p in parts if p["user_id"] != o):
            u = p["user_id"]
            w.pair(sq.M_CAN_LISTEN, o, sq.sess(u), 1, lo=hs, hi=he)
            if p["talker"]:
                w.pair(sq.M_PPS, o, sq.sess(u), 50, lo=hs, hi=he)
                w.pair(sq.M_EXPAND, o, sq.sess(u), 0, lo=hs, hi=he)
            if p["publishes"]["camera"]:
                for name, value in ((sq.M_FPS, 30), (sq.M_STALE, 100), (sq.M_FREEZE, 0)):
                    w.pair(name, o, sq.sess(u), value, lo=hs, hi=he)
    rate = sum(1 / (5 if p["fleet"] == "browser" else sq.BOT_HEALTH_INTERVAL_S) for p in parts)
    w.add(sq.M_HEALTH, {}, lambda t: rate * (t - hs), hs, he)
    for name in sq.RELAY_PATHS:
        extra = {"outcome": "accepted"} if name == "relay_layer_preference_updates_total" else {}
        w.add(name, {"room": m["meeting_id"], **extra}, lambda t: t - js, js, he)
    for series in w.series.values():
        for s in series:
            s["metric"]["meeting_id"] = m["meeting_id"]
    return w


class Reproduces20261001ThroughTheScorer(unittest.TestCase):

    def test_collected_and_hand_built_manifests_score_identically(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        run_dir = os.path.join(tmp, "run")
        shutil.copytree(FIXTURE, run_dir)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(cq_collect.main(["--run-dir", run_dir]), cq_collect.EXIT_OK)
        collected = os.path.join(run_dir, "manifest.json")
        expected = os.path.join(FIXTURE, "expected_manifest.json")
        self.assertEqual(cq_manifest.validate_manifest(sq.read_json(collected)), [])
        world = fixture_world(sq.read_json(expected))
        got = [score_cli(path, world, os.path.join(tmp, name))
               for name, path in (("collected", collected), ("expected", expected))]
        self.assertEqual(got[0][1], got[1][1])
        self.assertEqual((got[0][0], got[0][1]["verdict"]), (sq.cli.EXIT["INVALID"], "INVALID"))
        self.assertIn("G-V7: need >= 10 observers", failing(got[0][1]))

    def test_committed_fixture_has_no_host_paths_internal_hosts_or_personal_data(self):
        pattern = (r"/home/|/tmp/|/Users/|[A-Za-z]:\\\\|localhost|\.internal\b|\.svc\b|hcl|"
                   r"\b\d{1,3}(?:\.\d{1,3}){3}\b|[\w.+-]+@(?!bots-app\.local\b)[\w-]+\.[\w.]+")
        strip = os.path.join(HERE, "..", "sync-strip-blocked-paths.sh")
        if os.path.exists(strip):
            with open(strip, encoding="utf-8") as fh:
                pattern += "|" + re.search(r"^MARKER_PATTERN='([^']+)'$", fh.read(), re.M).group(1)
        banned = re.compile(pattern, re.I)
        files = [f for f in glob.glob(os.path.join(FIXTURE, "**", "*"), recursive=True) if os.path.isfile(f)]
        self.assertGreaterEqual(len(files), 6)
        for path in files:
            with open(path, encoding="utf-8") as fh:
                hits = [m.group(0) for m in banned.finditer(fh.read())]
            self.assertEqual(hits, [], os.path.relpath(path, FIXTURE))


if __name__ == "__main__":
    unittest.main()
