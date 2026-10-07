"""Fake clock, stack, fleet, tree and tools for driving cq_runner without Docker (#2914 PR-4 tests)."""

import contextlib
import io
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import call_quality_score  # noqa: E402
import cq_collect  # noqa: E402
import cq_manifest  # noqa: E402
from cq_runner import AdapterError, EventOutcome, Exit, Joined, Stamp  # noqa: E402

NETWORK = {"profile": "none", "shaped": False, "direction": "none", "shaper": "none", "params": {}}
COMMIT = "0123abcd0123abcd0123abcd0123abcd0123abcd"


class FakeClock:
    def __init__(self, wall=900.0, mono=50.0, max_sleeps=None):
        self.wall, self.mono, self.jumps = wall, mono, []
        self.sleeps, self.max_sleeps = 0, max_sleeps

    def now(self):
        return Stamp(self.wall, self.mono)

    def sleep(self, seconds):
        self.sleeps += 1
        if self.max_sleeps is not None and self.sleeps > self.max_sleeps:
            raise AssertionError(f"more than {self.max_sleeps} sleeps")
        if seconds > 0:
            self.advance(seconds)

    def advance(self, seconds):
        self.wall += seconds
        self.mono += seconds
        for at, delta in [j for j in self.jumps if self.wall >= j[0]]:
            self.wall += delta
            self.jumps.remove((at, delta))


class FakeTree:
    def __init__(self, clock, root, commit=COMMIT, dirty=False, dirty_from=None, commit_from=None, clean_from=None):
        self.clock, self.root, self._commit, self._dirty = clock, root, commit, dirty
        self.dirty_from, self.commit_from, self.clean_from = dirty_from, commit_from, clean_from

    def toplevel(self):
        return self.root

    def commit(self):
        moved = self.commit_from is not None and self.clock.wall >= self.commit_from
        return "fedcba9" + self._commit[7:] if moved else self._commit

    def dirty(self):
        if self.clean_from is not None and self.clock.wall >= self.clean_from:
            return False
        return self._dirty or (self.dirty_from is not None and self.clock.wall >= self.dirty_from)


class FakeStack:
    def __init__(self, clock, up_s=40.0, selector='up{job="relay-ws"}', prom_url="http://prom.invalid",
                 restart=False, problems=(), fail_up=False, images=None):
        self.clock, self.up_s, self.selector, self.url = clock, up_s, selector, prom_url
        self.restart, self.problems, self.fail_up = restart, list(problems), fail_up
        self._images = images if images is not None else {k: f"sha256:{k}" for k in cq_collect.IMAGE_KEYS}
        self.calls, self.states = [], 0

    def preflight(self, plan):
        return self.problems

    def up(self, plan, mode):
        self.calls.append(("up", mode))
        if self.fail_up:
            raise AdapterError("compose up exited 1")
        self.clock.advance(self.up_s)

    def state(self):
        self.states += 1
        return {"relay-ws": {"started_at": "2026-10-06T00:00:00Z",
                             "restart_count": 1 if self.restart and self.states > 1 else 0}}

    def images(self):
        return dict(self._images)

    def prom_url(self):
        return self.url

    def scrape_up_selector(self):
        return self.selector

    def down(self):
        self.calls.append(("down",))


class FakeFleet:
    def __init__(self, clock, plan, *, join_delay=6.0, rust_join_delay=1.0, media_delay=16.0, never_join=(),
                 extra_joined=(), crash=None, crash_leave_ts=False, event_results=None, confirm_delay=0.5,
                 stop_s=5.0, stop_fails=(), exit_time=None, hosts=("local",), verdict_ok=None,
                 missing_verdicts=(), extra_verdicts=(), window_shift=0.0, facts=None, interrupt_at=None,
                 leave_exit_delay=1.0, silent_crash=False, confirm_skew=0.0, leave_exit_code=0, crash_code=137):
        self.clock, self.plan = clock, plan
        self.parts = {p["user_id"]: p for p in plan["participants"]}
        self.procs = {p["proc_id"]: p for p in plan["processes"]}
        self.join_delay, self.rust_join_delay, self.media_delay = join_delay, rust_join_delay, media_delay
        self.never_join, self.extra_joined = set(never_join), list(extra_joined)
        self.crash, self.crash_leave_ts = dict(crash or {}), crash_leave_ts
        self.event_results, self.confirm_delay = dict(event_results or {}), confirm_delay
        self.stop_s, self.stop_fails, self.exit_time = stop_s, set(stop_fails), dict(exit_time or {})
        self.hosts, self.verdict_ok = list(hosts), dict(verdict_ok or {})
        self.missing_verdicts, self.extra_verdicts = set(missing_verdicts), list(extra_verdicts)
        self.window_shift, self.facts, self.interrupt_at = window_shift, facts, interrupt_at
        self.leave_exit_delay, self.silent_crash, self.confirm_skew = leave_exit_delay, silent_crash, confirm_skew
        self.leave_exit_code, self.crash_code = leave_exit_code, crash_code
        self.launched, self.exited, self.reported, self.left, self.scheduled = {}, {}, set(), {}, {}
        self.calls = []

    def _interrupt(self):
        if self.interrupt_at is not None and self.clock.wall >= self.interrupt_at:
            self.interrupt_at = None
            raise KeyboardInterrupt

    def clock_facts(self):
        return self.facts if self.facts is not None else {"sync": "unknown",
                                                          "per_node": [{"node": "local", "skew_ms": 0}]}

    def launch(self, proc):
        self.calls.append(("launch", proc["proc_id"]))
        self.launched[proc["proc_id"]] = self.clock.wall

    def _join_ts(self, uid):
        proc = self.parts[uid]["process"]
        delay = self.rust_join_delay if self.parts[uid]["fleet"] == "rust" else self.join_delay
        delay = delay.get(uid, 6.0) if isinstance(delay, dict) else delay
        return None if uid in self.never_join or proc not in self.launched else self.launched[proc] + delay

    def _media(self, proc):
        return self.launched[proc] + self.media_delay if self.procs[proc]["fleet"] == "rust" else None

    def joined(self):
        self._interrupt()
        now, out = self.clock.wall, {}
        for uid in self.parts:
            ts = self._join_ts(uid)
            if ts is not None and ts <= now:
                media = self._media(self.parts[uid]["process"])
                out[uid] = Joined(ts, media if media is not None and media <= now else None)
        if self.launched:
            out.update({u: Joined(min(self.launched.values()) + 1) for u in self.extra_joined})
        return out

    def apply(self, ev):
        self.calls.append(("apply", ev["event_id"]))
        issued = self.clock.wall
        result = self.event_results.get(ev["event_id"], "ok")
        self.clock.advance(self.confirm_delay)
        if result != "ok":
            return EventOutcome(None, result)
        if ev["action"] == "leave":
            for uid in ev["participants"]:
                self.left[uid] = issued + 0.25
            for pid, proc in self.procs.items():
                if all(u in self.left for u in proc["participants"]):
                    self.scheduled.setdefault(pid, self.clock.wall + self.leave_exit_delay)
        return EventOutcome(self.clock.wall + self.confirm_skew, "ok")

    def _due(self, reaping=False):
        now = self.clock.wall
        due = [(t, pid, self.crash_code) for pid, t in self.crash.items()
               if pid in self.launched and t <= now and (reaping or not self.silent_crash)]
        due += [(t, pid, self.leave_exit_code) for pid, t in self.scheduled.items() if t <= now]
        return sorted(d for d in due if d[1] not in self.exited)

    def poll(self):
        self._interrupt()
        out = []
        for t, pid, code in self._due():
            self.exited[pid] = (t, code)
            out.append(Exit(pid, t, code))
        return out

    def stop(self, proc):
        pid = proc["proc_id"]
        self.calls.append(("stop", pid))
        if pid in self.stop_fails:
            raise AdapterError("container did not stop")
        for t, due, code in self._due(reaping=True):
            if due == pid:
                self.exited[pid] = (t, code)
                return Exit(pid, self.exit_time.get(pid, t), code)
        self.clock.advance(self.stop_s)
        t = self.clock.wall - 0.1
        self.exited[pid] = (t, 0)
        return Exit(pid, self.exit_time.get(pid, t), 0)

    def generator_hosts(self):
        return list(self.hosts)

    def generator_verdicts(self, hold_start, hold_end):
        out = [{"host": h, "ok": self.verdict_ok.get(h, True), "detail": "RESOURCE_OK", "starved": False,
                "noEvidence": False, "reasons": [],
                "window": {"from": hold_start, "to": hold_end + self.window_shift, "unit": "epoch_seconds"}}
               for h in self.hosts if h not in self.missing_verdicts]
        return out + [dict(out[0], **v) for v in self.extra_verdicts] if out else out

    def _record(self, uid, ended):
        p = self.parts[uid]
        crashed = self.crash.get(p["process"])
        join = self._join_ts(uid)
        if uid in self.left:
            leave = self.left[uid]
        elif crashed is not None:
            leave = crashed if self.crash_leave_ts else None
        else:
            leave = None if p["fleet"] == "browser" or ended is None else ended - 0.4
        rec = {"user_id": uid, "fleet": p["fleet"], "role": p["role"] if p["fleet"] == "browser" else (
            "talker" if p["talker"] else "viewer"), "observer": p["observer"], "talker": p["talker"],
            "publishes": dict(p["publishes"]), "network": dict(NETWORK),
            "transport_intended": self.procs[p["process"]].get("transport", "websocket"),
            "join_ts": join, "leave_ts": leave}
        return rec, ("dropped: relay closed" if crashed is not None and leave is not None else
                     "left" if uid in self.left else None)

    def collect(self, run_dir):
        self.calls.append(("collect",))
        rust = [p for p in self.plan["processes"] if p["fleet"] == "rust" and p["proc_id"] in self.launched]
        for k, proc in enumerate(rust):
            pid, launched = proc["proc_id"], self.launched[proc["proc_id"]]
            exit_t = self.exited.get(pid, (None, None))[0]
            ended = None if pid in self.crash or exit_t is None else exit_t - 0.05
            parts = []
            for uid in proc["participants"]:
                rec, outcome = self._record(uid, ended)
                parts.append(dict(rec, instance_id=f"i-{uid}", **({"outcome": outcome} if outcome else {})))
            _write(os.path.join(run_dir, "rust", f"participants-{k}.json"), {
                "kind": cq_collect.RUST_KIND, "manifest_schema": cq_manifest.SCHEMA,
                "meeting_id": self.plan["meeting_id"], "id_prefix": self.plan["run_id"],
                "started_at": launched + 0.5, "media_started_at": self._media(pid), "ended_at": ended,
                "planned": len(parts), "participants": parts})
        for proc in (p for p in self.plan["processes"] if p["fleet"] == "browser" and p["proc_id"] in self.launched):
            uid = proc["participants"][0]
            rec, outcome = self._record(uid, None)
            rec.update(placement=None, stagger_ms=None)
            _write(os.path.join(run_dir, "probes", str(proc["probe_index"]), "participants", f"bot-{uid}.json"),
                   {"schema": cq_collect.BROWSER_SCHEMA, "bot_id": f"bot-{proc['proc_id']}", "outcome": outcome,
                    "unverified": [], "participant": rec})


def _write(path, obj):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(obj, fh)


class InProcessTools:
    def __init__(self, transport=None, mutate=None):
        self.transport, self.mutate, self.calls = transport, mutate, []

    def run(self, argv):
        script, args = os.path.basename(argv[1]), list(argv[2:])
        if self.mutate is not None:
            args = self.mutate(script, args)
        self.calls.append((script, args))
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                if script == "cq_collect.py":
                    rc = cq_collect.main(args)
                elif script == "call_quality_score.py":
                    rc = call_quality_score.main(args, transport=self.transport, environ={})
                else:
                    rc = 3
            except SystemExit as exc:
                rc = exc.code
        return rc, out.getvalue(), err.getvalue()
