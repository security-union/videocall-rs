"""Scenario runner core (#2914): phases, events.jsonl, runner gates R1-R10 and verdict composition.

The stack, fleet and source tree are injected adapters; only the scorer can say PASS, the runner only downgrades.
"""

import hashlib
import json
import math
import os
import re
import shlex
import signal
import subprocess
import sys
import time
from dataclasses import dataclass, field
from typing import Protocol

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import call_quality_score  # noqa: E402
import cq_collect  # noqa: E402
import cq_manifest  # noqa: E402
import cq_scenario  # noqa: E402
import cq_score  # noqa: E402

RUN_SCHEMA = "scale-run/v1"
RANK = {"PASS": 0, "FAIL": 1, "INVALID": 2, "ERROR": 3}
SCORER_EXIT = {"PASS": 0, "FAIL": 1, "INVALID": 2}
FORCED_NOT_MEASURED = ["G-V2", "G-V4"]
RESERVED_NODES = cq_collect.RESERVED_NODES
JOIN_SETTLE_S = 15
MEDIA_SETTLE_S = 30
TEARDOWN_DELAY_S = 30
JOIN_BUDGET_EXTRA_S = 120
POLL_S = 5.0
JOIN_POLL_S = 1.0
MAX_CLOCK_DRIFT_S = 1.0
EXIT_SLACK_S = 1.0
JOIN_TIME_SLACK_S = cq_collect.MAX_ABS_SKEW_MS / 1000 + EXIT_SLACK_S
EVENT_SLACK_S = cq_scenario.EVENT_SLACK_S
VALIDITY_GATES = [f"G-V{k}" for k in range(1, 17)]
QUALITY_GATES = ["G-Q1", "G-Q2", "G-Q3", "G-Q4", "G-Q5", "G-Q7", "G-Q8", "G-Q9"]
TOOL_TIMEOUT_S = 900
COLLECT_SCRIPT = os.path.join(HERE, "cq_collect.py")
SCORE_SCRIPT = os.path.join(HERE, "call_quality_score.py")


class Refused(Exception):
    pass


class AdapterError(Exception):
    pass


class Abort(Exception):
    pass


@dataclass(frozen=True)
class Stamp:
    wall: float
    mono: float


@dataclass
class Exit:
    proc_id: str
    exited_at: object
    exit_code: object


@dataclass
class Joined:
    join_ts: object
    media_started_at: object = None


@dataclass
class EventOutcome:
    t_confirmed: object
    result: str


class Stack(Protocol):
    """Raises only AdapterError, whose text reaches run.json; state() and images() are written verbatim to
    stack/state-*.json and stack/images.json (then the manifest). None of it may name a host, address or user path.
    Its subprocesses start in their own session, it never catches KeyboardInterrupt, and every call is time-bounded:
    a terminal signal must not reach its children, and no second signal forces an exit."""

    def preflight(self, plan): ...
    def up(self, plan, mode): ...
    def state(self): ...
    def images(self): ...
    def prom_url(self): ...
    def scrape_up_selector(self): ...
    def down(self): ...


class Fleet(Protocol):
    """Raises only AdapterError, whose text is written verbatim to events.jsonl and run.json, so it may name no host,
    address or user path. Times are epoch seconds; exit and confirmation times are checked against the runner's clock,
    join and media-start times may be no later than it plus JOIN_TIME_SLACK_S, and a planned leave's exit counts as
    planned only with exit code 0. Its subprocesses start in their own session, it never catches KeyboardInterrupt,
    and every call is time-bounded: a terminal signal must not reach its children, and no second signal forces an
    exit."""

    def clock_facts(self): ...
    def launch(self, proc): ...
    def joined(self): ...

    def apply(self, event):
        """EventOutcome whose t_confirmed lies between the call and its return; retries happen inside."""

    def poll(self):
        """Exits since the last poll, each with the process's own exit time, never the time it was noticed."""

    def stop(self, proc):
        """Exit with the process's own exit time (also for a process that had already died), never the reap time."""

    def collect(self, run_dir): ...
    def generator_hosts(self): ...

    def generator_verdicts(self, hold_start, hold_end):
        """One resource-verdict per host for exactly [hold_start, hold_end], each with its host name."""


class Tree(Protocol):
    """Raises only AdapterError, whose text is written verbatim to run.json and so may name no host or user path."""

    def toplevel(self): ...
    def commit(self): ...
    def dirty(self): ...


class SystemClock:
    def now(self):
        return Stamp(time.time(), time.monotonic())

    def sleep(self, seconds):
        if seconds > 0:
            time.sleep(seconds)


class SubprocessTools:
    def run(self, argv):
        try:
            p = subprocess.run(argv, capture_output=True, text=True, timeout=TOOL_TIMEOUT_S, check=False,
                               start_new_session=True)
        except subprocess.TimeoutExpired:
            return 3, "", f"TimeoutExpired: no exit after {TOOL_TIMEOUT_S} s"
        except OSError as exc:
            return 3, "", _describe(exc)
        return p.returncode, p.stdout, p.stderr


class GitTree:
    def __init__(self, root):
        self.root = root

    def _git(self, *args):
        try:
            p = subprocess.run(["git", "-C", self.root, *args], capture_output=True, text=True, check=False,
                               timeout=TOOL_TIMEOUT_S, start_new_session=True)
        except subprocess.TimeoutExpired as exc:
            raise AdapterError(f"git {' '.join(args)} timed out") from exc
        except OSError as exc:
            raise AdapterError(f"git {' '.join(args)}: {_describe(exc)}") from exc
        if p.returncode != 0:
            raise AdapterError(f"git {' '.join(args)} failed: {_no_paths(p.stderr.strip())}")
        return p.stdout

    def toplevel(self):
        return self._git("rev-parse", "--show-toplevel").strip()

    def commit(self):
        return self._git("rev-parse", "HEAD").strip()

    def dirty(self):
        return self._git("status", "--porcelain=v1", "--untracked-files=all") != ""


def _describe(exc):
    if isinstance(exc, (AdapterError, Refused)):
        return str(exc)
    if isinstance(exc, OSError) and exc.strerror:
        return f"{type(exc).__name__}: {exc.strerror}"
    return f"{type(exc).__name__}: {exc}"


def _no_paths(text):
    return re.sub(r"'/[^']*'|\"/[^\"]*\"|(?<![\w.])/\S+", "<path>", text)


def _signal_name(signum):
    try:
        return signal.Signals(signum).name
    except ValueError:
        return str(signum)


def _num(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)


def _sha256(path):
    with open(path, "rb") as fh:
        return hashlib.sha256(fh.read()).hexdigest()


def _dump(path, obj):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(obj, fh, indent=1, allow_nan=False)
        fh.write("\n")
    os.replace(tmp, path)


def prepare_run_dir(out_dir, run_id, repo_root):
    out = os.path.realpath(out_dir)
    root = os.path.realpath(repo_root)
    if os.path.commonpath([out, root]) == root:
        raise Refused("--out-dir must be outside the repository: a run folder inside it dirties the tree (R9)")
    run_dir = os.path.join(out, run_id)
    try:
        os.makedirs(run_dir)
    except FileExistsError as exc:
        raise Refused(f"run folder {run_id} already exists; runs never overwrite one another") from exc
    except OSError as exc:
        raise Refused(f"cannot create the run folder: {exc}") from exc
    return run_dir


def clock_block(facts, environment, measured_at):
    if not isinstance(facts, dict):
        raise Refused("clock facts: must be an object")
    if facts.get("sync") not in cq_manifest.CLOCK_SYNC:
        raise Refused(f"clock facts: sync must be one of {', '.join(cq_manifest.CLOCK_SYNC)}")
    per_node = facts.get("per_node")
    if not isinstance(per_node, list) or not per_node:
        raise Refused("clock facts: per_node must name at least one node")
    allowed = set(RESERVED_NODES) | {environment}
    out = []
    for n in per_node:
        if not isinstance(n, dict) or n.get("node") not in allowed:
            raise Refused(f"clock facts: every per_node node must be one of {', '.join(sorted(allowed))}; "
                          "a host name is never written")
        if not _num(n.get("skew_ms")):
            raise Refused("clock facts: every per_node skew_ms must be a finite number")
        out.append({"node": n["node"], "skew_ms": n["skew_ms"]})
    skew = max(abs(n["skew_ms"]) for n in out)
    if skew > cq_collect.MAX_ABS_SKEW_MS:
        raise Refused(f"clock facts: max skew {skew} ms exceeds {cq_collect.MAX_ABS_SKEW_MS} ms")
    return {"sync": facts["sync"], "max_abs_skew_ms": skew, "measured_at": measured_at, "per_node": out}


def compose_generator_verdict(hosts, verdicts, window, allowed):
    """One verdict per declared host for exactly this hold, ANDed; anything missing or extra is not ok."""
    problems, seen = [], {}
    hosts = list(hosts) if isinstance(hosts, list) else []
    if any(h not in allowed for h in hosts):
        problems.append(f"a load-generator host is not named {', '.join(sorted(allowed))}; the name is not written")
        hosts = [h for h in hosts if h in allowed]
    if not hosts:
        problems.append("no load-generator host declared")
    for v in verdicts if isinstance(verdicts, list) else [None]:
        host = v.get("host") if isinstance(v, dict) else None
        if not isinstance(host, str):
            problems.append("a verdict names no host")
        elif host not in hosts:
            problems.append("a verdict names an undeclared load-generator host")
        elif host in seen:
            problems.append(f"{host}: more than one verdict")
        else:
            seen[host] = v
    for host in hosts:
        v = seen.get(host)
        if v is None:
            problems.append(f"{host}: no verdict for the hold")
            continue
        w = v.get("window")
        if not isinstance(w, dict) or w.get("from") != window[0] or w.get("to") != window[1]:
            problems.append(f"{host}: verdict window is not the hold [{window[0]}, {window[1]}]")
        if v.get("ok") is not True:
            problems.append(f"{host}: not ok")
    details = [f"{h}: {seen[h].get('detail', '')}" for h in hosts if h in seen]
    return {"ok": not problems, "detail": "; ".join(problems + details),
            "window": {"from": window[0], "to": window[1]}, "hosts": sorted(seen)}


def forced_scorer_config(scrape_up_selector, relay_path_exemptions):
    return {"validity_gates": {"allowed_not_measured": {"value": list(FORCED_NOT_MEASURED)},
                               "scrape_up_selector": {"value": scrape_up_selector},
                               "relay_path_exemptions": {"value": dict(relay_path_exemptions)}}}


def collector_argv(run_dir):
    return [sys.executable, COLLECT_SCRIPT, "--run-dir", run_dir]


def validate_argv(manifest):
    return [sys.executable, SCORE_SCRIPT, "--manifest", manifest, "--validate-only"]


def scorer_argv(manifest, prom_url, config, verdict, out_dir):
    return [sys.executable, SCORE_SCRIPT, "--manifest", manifest, "--prom-url", prom_url, "--config", config,
            "--generator-verdict", verdict, "--out-dir", out_dir]


def compose_verdict(scorer_verdict, gates, error=None):
    if error is not None:
        return "ERROR"
    base = scorer_verdict if scorer_verdict in SCORER_EXIT else "INVALID"
    capped = "INVALID" if not gates or any(not g["ok"] for g in gates) else "PASS"
    return max(base, capped, key=RANK.get)


def check_expectation(expect, result, gates, error=None):
    if expect is None:
        return None
    problems = [f"{g['gate']} failed: {g['detail']}" for g in gates if not g["ok"]]
    if error is not None:
        problems.append(f"run error: {error}")
    if result is None:
        return {"met": False, "problems": problems + ["not scored"]}
    if result.get("verdict") != expect["verdict"]:
        problems.append(f"scorer verdict {result.get('verdict')} is not the expected {expect['verdict']}")
    step = next((s for s in result.get("steps", []) if s.get("headline")), {})
    failing = sorted(g["gate"] for g in step.get("validity", []) if g.get("status") == "fail")
    if failing != sorted(expect["invalid_gates"]):
        problems.append(f"failing validity gates {failing} are not exactly {sorted(expect['invalid_gates'])}")
    for g in step.get("validity", []):
        if g.get("status") not in ("pass", "fail") and not (
                g.get("status") == "not_measured" and g.get("gate") in FORCED_NOT_MEASURED):
            problems.append(f"{g.get('gate')} is {g.get('status')}")
    if step.get("invalid_reasons"):
        problems.append(f"blocking not-measured gates: {step['invalid_reasons']}")
    return {"met": not problems, "problems": problems}


class EventLog:
    def __init__(self, path, now):
        self.path, self.now, self.lines = path, now, []
        self.fh = open(path, "a", encoding="utf-8")  # noqa: SIM115 - held for the run, closed by close()

    def write(self, kind, wall=None, **fields):
        st = self.now()
        line = {"type": kind, "wall": st.wall, "mono": st.mono}
        if wall is not None:
            line = {"type": kind, "wall": wall, "logged": {"wall": st.wall, "mono": st.mono}}
        line.update(fields)
        self.fh.write(json.dumps(line, allow_nan=False) + "\n")
        self.fh.flush()
        os.fsync(self.fh.fileno())
        self.lines.append(line)
        return line

    def close(self):
        self.fh.close()


@dataclass
class _State:
    phase: str = "preflight"
    commit: object = None
    dirty_at_preflight: object = None
    dirty_at_teardown: object = None
    commit_at_teardown: object = None
    drift_base: object = None
    max_drift: float = 0.0
    stack_started: bool = False
    stack_ready: object = None
    stack_teardown: object = None
    launched: dict = field(default_factory=dict)
    exits: dict = field(default_factory=dict)
    exit_problems: list = field(default_factory=list)
    joined: dict = field(default_factory=dict)
    final_leave: dict = field(default_factory=dict)
    event_lines: list = field(default_factory=list)
    marks: dict = field(default_factory=dict)
    aborted: object = None
    interrupted: bool = False
    error: object = None
    collector_rc: object = None
    validate_rc: object = None
    scorer_rc: object = None
    manifest_sha: object = None
    manifest_sha_after: object = None
    config_path: object = None
    result: object = None
    commands: list = field(default_factory=list)
    teardown_problems: list = field(default_factory=list)
    signals: list = field(default_factory=list)
    hold_mono: dict = field(default_factory=dict)
    issued_mono: dict = field(default_factory=dict)


class Runner:
    def __init__(self, plan, scenario_bytes, run_dir, *, stack, fleet, tree, clock=None, tools=None,
                 stack_mode="up", keep_stack=False, allow_dirty=False, argv=()):
        self.plan, self.scenario_bytes, self.dir = plan, scenario_bytes, run_dir
        self.stack, self.fleet, self.tree = stack, fleet, tree
        self.clock, self.tools = clock or SystemClock(), tools or SubprocessTools()
        self.stack_mode, self.keep_stack, self.allow_dirty = stack_mode, keep_stack, allow_dirty
        self.argv = list(argv)
        self.step = plan["steps"][0]
        self.s = _State()
        self.log = None
        self._signal_mode = "record"

    def on_signal(self, signum, frame=None):
        """Raises KeyboardInterrupt once, during the phases before teardown or while scoring; otherwise only records.
        A signal before the scored line is written leaves the run unscored; one before the final gates fails R1; a
        later one is listed."""
        self.s.signals.append(_signal_name(signum))
        self.s.interrupted = True
        if self._signal_mode == "raise":
            self._signal_mode = "record"
            raise KeyboardInterrupt

    def path(self, *parts):
        return os.path.join(self.dir, *parts)

    def _now(self):
        st = self.clock.now()
        offset = st.wall - st.mono
        if self.s.drift_base is None:
            self.s.drift_base = offset
        self.s.max_drift = max(self.s.max_drift, abs(offset - self.s.drift_base))
        return st

    def _sleep_until(self, wall):
        start = self._now()
        return self._sleep_until_mono(start.mono + (wall - start.wall))

    def _sleep_until_mono(self, target):
        while True:
            now = self._now()
            if now.mono >= target:
                return now
            self.clock.sleep(min(POLL_S, target - now.mono))
            self._poll()

    def _record_exit(self, ex, reaped):
        proc = self.s.launched.get(getattr(ex, "proc_id", None))
        if proc is None or ex.proc_id in self.s.exits:
            self.s.exit_problems.append(f"exit reported for an unknown or already exited process "
                                        f"{getattr(ex, 'proc_id', None)!r}")
            return
        self.s.exits[ex.proc_id] = ex
        if not _num(ex.exited_at) or not proc["launched_at"] - EXIT_SLACK_S <= ex.exited_at \
                <= reaped.wall + EXIT_SLACK_S:
            self.s.exit_problems.append(f"{ex.proc_id}: exit time {ex.exited_at!r} unknown or outside "
                                        f"[launch, reap]; no stopped line is written")
            return
        self.log.write("stopped", wall=ex.exited_at, proc_id=ex.proc_id, participants=list(proc["participants"]),
                       exit_code=ex.exit_code)

    def _poll(self):
        for ex in self.fleet.poll():
            self._record_exit(ex, self._now())

    def _preflight(self):
        ids = [p["proc_id"] for p in self.plan["processes"]]
        if len(set(ids)) != len(ids):
            raise Refused("the plan gives two processes one proc_id, so one of them would never be stopped")
        _dump(self.path(cq_collect.PLAN_FILE), self.plan)
        with open(self.path("scenario.yaml"), "wb") as fh:
            fh.write(self.scenario_bytes)
        self.log = EventLog(self.path(cq_collect.EVENTS_FILE), self._now)
        self.s.commit = self.tree.commit()
        self.s.dirty_at_preflight = self.tree.dirty()
        if self.s.dirty_at_preflight and not self.allow_dirty:
            raise Refused("the tree is dirty: commit, or pass --allow-dirty to run with the verdict capped at "
                          "INVALID (R9)")
        clock = clock_block(self.fleet.clock_facts(), self.plan["environment"], self._now().wall)
        selector = self.stack.scrape_up_selector()
        if not isinstance(selector, str) or not selector.strip():
            raise Refused("the stack adapter gives no scrape_up_selector, so G-V3 would go unmeasured")
        problems = self.stack.preflight(self.plan)
        if problems:
            raise Refused("stack preflight: " + "; ".join(problems))
        self.log.write("preflight", run_id=self.plan["run_id"], commit=self.s.commit,
                       dirty=self.s.dirty_at_preflight, clock=clock,
                       python=sys.version.split()[0])

    def _stack_up(self):
        self.s.phase = "stack"
        self.s.stack_started = True
        self.stack.up(self.plan, self.stack_mode)
        self.s.stack_ready = self.stack.state()
        _dump(self.path("stack", "state-ready.json"), self.s.stack_ready)
        self.log.write("stack_ready")

    def _launch(self, proc):
        self.s.launched[proc["proc_id"]] = dict(proc, launched_at=self._now().wall)
        self.fleet.launch(proc)
        self.log.write("launched", proc_id=proc["proc_id"], participants=list(proc["participants"]))

    def _join(self):
        self.s.phase = "join"
        sid = self.step["step_id"]
        start = self.log.write("join_start", step_id=sid)
        self.s.marks["join_start"] = start["wall"]
        procs = self.plan["processes"]
        browsers = [p for p in procs if p["fleet"] == "browser"]
        for k, p in enumerate(browsers):
            self._launch(p)
            if k + 1 < len(browsers):
                self._sleep_until_mono(self._now().mono + p["join_stagger_s"])
        for p in (p for p in procs if p["fleet"] == "rust"):
            self._launch(p)
        planned = {p["user_id"]: p for p in self.plan["participants"]}
        deadline = start["mono"] + self.step["join_window_s"] + JOIN_BUDGET_EXTRA_S
        while True:
            reported = self.fleet.joined()
            horizon = self._now().wall + JOIN_TIME_SLACK_S
            for uid, j in sorted(reported.items()):
                if uid in self.s.joined or not _num(j.join_ts) or (
                        planned.get(uid, {}).get("fleet") == "rust" and not _num(j.media_started_at)):
                    continue
                future = [k for k in ("join_ts", "media_started_at") if _num(getattr(j, k)) and getattr(j, k) > horizon]
                if future:
                    raise Abort(f"{uid}: {future[0]} {getattr(j, future[0])} is later than the runner's clock")
                self.s.joined[uid] = j
                self.log.write("joined", participants=[uid], join_ts=j.join_ts, media_started_at=j.media_started_at)
            if self.s.exits:
                raise Abort(f"processes exited during the join: {', '.join(sorted(self.s.exits))}")
            if set(planned) <= set(self.s.joined):
                return
            now = self._now()
            if now.mono >= deadline:
                missing = sorted(set(planned) - set(self.s.joined))
                raise Abort(f"join budget ran out; not joined: {', '.join(missing)}")
            self.clock.sleep(min(JOIN_POLL_S, deadline - now.mono))
            self._poll()

    def hold_start_target(self):
        deadline = cq_score.cv(cq_score.load_config(), "quality_gates", "join_deadline_s")
        joins = [j.join_ts for j in self.s.joined.values()]
        media = [j.media_started_at for j in self.s.joined.values() if _num(j.media_started_at)]
        return max([max(joins) + deadline + JOIN_SETTLE_S] + [m + MEDIA_SETTLE_S for m in media])

    def _hold(self):
        self.s.phase = "steady"
        sid = self.step["step_id"]
        self._sleep_until(self.hold_start_target())
        self.s.phase = "hold"
        hs = self.log.write("hold_start", step_id=sid)
        self.s.marks["hold_start"], self.s.hold_mono["hold_start"] = hs["wall"], hs["mono"]
        for ev in sorted(self.plan["events"], key=lambda e: e["at_offset_s"]):
            self._sleep_until_mono(hs["mono"] + ev["at_offset_s"])
            self._apply(ev)
        self._sleep_until_mono(hs["mono"] + self.step["hold_s"])
        he = self.log.write("hold_end", step_id=sid)
        self.s.marks["hold_end"], self.s.hold_mono["hold_end"] = he["wall"], he["mono"]
        self.s.phase = "settle-out"
        self._sleep_until_mono(he["mono"] + TEARDOWN_DELAY_S)

    def _apply(self, ev):
        issued = self._now()
        t_issued = issued.wall
        self.s.issued_mono[ev["event_id"]] = issued.mono
        try:
            out = self.fleet.apply(ev)
            confirmed, result = out.t_confirmed, out.result
        except AdapterError as exc:
            confirmed, result = None, f"error: {exc}"
        reply = self._now()
        back = max(0.0, (issued.wall - issued.mono) - (reply.wall - reply.mono))
        if result == "ok" and not (_num(confirmed) and t_issued - back <= confirmed <= reply.wall + back):
            confirmed, result = None, f"unconfirmed: t_confirmed {confirmed!r} outside [issued, reply]"
        if result != "ok":
            confirmed = None
        elif confirmed < t_issued:
            confirmed = t_issued
        line = self.log.write("event", event_id=ev["event_id"], action=ev["action"],
                              participants=list(ev["participants"]), params=dict(ev["params"]),
                              t_issued=t_issued, t_confirmed=confirmed, result=result)
        self.s.event_lines.append(line)
        if result == "ok" and ev["action"] == "leave":
            for uid in ev["participants"]:
                self.s.final_leave[uid] = t_issued

    def _teardown_fault(self, what, exc):
        self.s.interrupted = self.s.interrupted or isinstance(exc, KeyboardInterrupt)
        self.s.teardown_problems.append(f"{what}: {_describe(exc)}")

    def _try(self, what, fn):
        try:
            return fn()
        except BaseException as exc:  # noqa: B036 - teardown never stops early
            self._teardown_fault(what, exc)
            return None

    def _unrecorded(self, pid, why):
        self.s.exits.setdefault(pid, Exit(pid, None, None))
        self.s.exit_problems.append(f"{pid}: {why}; no stopped line is written")

    def _teardown(self):
        self.s.phase = "teardown"
        if self.s.stack_ready is not None:
            self.s.stack_teardown = self._try("stack state", self.stack.state)
        line = self._try("teardown_start line", lambda: self.log.write("teardown_start"))
        if line is not None:
            self.s.marks["teardown_start"] = line["wall"]
        order = [p for p in self.plan["processes"] if p["fleet"] == "rust"]
        order += [p for p in self.plan["processes"] if p["fleet"] == "browser"]
        for p in order:
            pid = p["proc_id"]
            if pid not in self.s.launched or pid in self.s.exits:
                continue
            try:
                ex = self.fleet.stop(self.s.launched[pid])
            except BaseException as exc:  # noqa: B036 - every process must get its stop
                self._teardown_fault(f"stop {pid}", exc)
                self._unrecorded(pid, f"stop failed ({_describe(exc)})")
                continue
            if getattr(ex, "proc_id", None) != pid:
                why = f"stop failed (it returned the exit of {getattr(ex, 'proc_id', None)!r})"
                self.s.teardown_problems.append(f"stop {pid}: {why}")
                self._unrecorded(pid, why)
                continue
            try:
                self._record_exit(ex, self._now())
            except BaseException as exc:  # noqa: B036 - the next process still gets its stop
                self._teardown_fault(f"stopped line for {pid}", exc)
                self._unrecorded(pid, f"exit not recorded ({_describe(exc)})")
        self.s.commit_at_teardown = self._try("git HEAD", self.tree.commit)
        self.s.dirty_at_teardown = self._try("git status", self.tree.dirty)

    def _collect(self):
        self.s.phase = "collect"
        os.makedirs(self.path("logs"), exist_ok=True)
        self.fleet.collect(self.dir)
        _dump(self.path(cq_collect.IMAGES_FILE), self.stack.images())
        if self.s.stack_teardown is not None:
            _dump(self.path("stack", "state-teardown.json"), self.s.stack_teardown)
        if "hold_end" in self.s.marks:
            window = (self.s.marks["hold_start"], self.s.marks["hold_end"])
            verdict = compose_generator_verdict(self.fleet.generator_hosts(), self.fleet.generator_verdicts(*window),
                                                window, set(RESERVED_NODES) | {self.plan["environment"]})
            _dump(self.path("generator-verdict.json"), verdict)
        self.s.collector_rc = self._tool("collector", collector_argv(self.dir))
        manifest = self.path("manifest.json")
        if self.s.collector_rc == cq_collect.EXIT_OK and os.path.exists(manifest):
            self.s.manifest_sha = _sha256(manifest)
        self.log.write("collected", collector_exit=self.s.collector_rc, manifest_sha256=self.s.manifest_sha)

    def _tool(self, name, argv):
        self.s.commands.append(argv)
        rc, out, err = self.tools.run(argv)
        with open(self.path("logs", f"{name}.log"), "w", encoding="utf-8") as fh:
            fh.write(f"exit {rc}\n--- stdout\n{out}\n--- stderr\n{err}\n".replace(self.dir, "$RUN")
                     .replace(HERE, "scripts/quality"))
        return rc

    def _score(self):
        if self.s.interrupted:
            raise KeyboardInterrupt
        self.s.phase = "score"
        manifest = self.path("manifest.json")
        self.s.validate_rc = self._tool("validate", validate_argv(manifest))
        if self.s.validate_rc != 0:
            raise AdapterError(f"call_quality_score.py --validate-only exited {self.s.validate_rc} on a manifest the "
                               "collector accepted: a collector bug")
        self.s.config_path = self.path("score", "config.json")
        _dump(self.s.config_path, forced_scorer_config(self.stack.scrape_up_selector(),
                                                       self.plan["scoring"]["relay_path_exemptions"]))
        self.s.scorer_rc = self._tool("scorer", scorer_argv(manifest, self.stack.prom_url(), self.s.config_path,
                                                            self.path("generator-verdict.json"),
                                                            self.path("score")))
        report = self.path("score", "report.md")
        if os.path.exists(report):
            with open(report, encoding="utf-8") as fh:
                text = fh.read()
            with open(report + ".tmp", "w", encoding="utf-8") as fh:
                fh.write(text.replace(self.dir, "$RUN"))
            os.replace(report + ".tmp", report)
        self.s.manifest_sha_after = _sha256(manifest) if os.path.exists(manifest) else None
        result_path = self.path("score", "result.json")
        if self.s.scorer_rc not in SCORER_EXIT.values() or not os.path.exists(result_path):
            raise AdapterError(f"the scorer exited {self.s.scorer_rc} without a result")
        with open(result_path, encoding="utf-8") as fh:
            result = json.load(fh)
        if SCORER_EXIT.get(result.get("verdict")) != self.s.scorer_rc:
            raise AdapterError(f"scorer exit {self.s.scorer_rc} disagrees with result.json verdict "
                               f"{result.get('verdict')!r}")
        incomplete = self._incomplete_pass(result)
        if incomplete:
            raise AdapterError(f"the scorer's PASS is incomplete or contradictory: {'; '.join(incomplete)}")
        self.log.write("scored", scorer_exit=self.s.scorer_rc, verdict=result["verdict"])
        self.s.result = result

    def _incomplete_pass(self, result):
        if result.get("verdict") != "PASS":
            return []
        steps = result.get("steps")
        if not isinstance(steps, list) or [st.get("step_id") for st in steps] != [self.step["step_id"]]:
            return [f"steps {[st.get('step_id') for st in steps or []]} are not the planned {[self.step['step_id']]}"]
        cfg = cq_score.load_config(self.s.config_path)
        latency_waived = cq_score.latency_metric(cfg) is None and cq_score.cv(cfg, "latency_gate",
                                                                            "allowed_unmeasured")
        waived = {"validity": dict.fromkeys(FORCED_NOT_MEASURED, "not_measured"),
                  "quality_gates": {"G-Q5": "not_measured"} if latency_waived else {}}
        if not cq_score.cv(cfg, "quality_gates", "reconnect_gate_enabled"):
            waived["quality_gates"]["G-Q9"] = "disabled"
        problems = []
        for st in steps:
            if st.get("verdict") != "PASS" or st.get("invalid_reasons") != []:
                problems.append(f"{st['step_id']}: step verdict {st.get('verdict')!r}, invalid_reasons "
                                f"{st.get('invalid_reasons')!r}")
            for key, want in (("validity", VALIDITY_GATES), ("quality_gates", QUALITY_GATES)):
                status = {}
                for g in st.get(key, []):
                    status.setdefault(g.get("gate"), []).append(g.get("status"))
                if sorted(status, key=str) != sorted(want):
                    problems.append(f"{st['step_id']}: {key} {sorted(status, key=str)} are not {want}")
                for gate, seen in sorted(status.items(), key=str):
                    if seen not in (["pass"], [waived[key].get(gate, "pass")]):
                        problems.append(f"{st['step_id']}: {gate} is {seen}")
        return problems

    def gates(self):
        s, gates = self.s, []

        def gate(name, ok, detail):
            gates.append({"gate": name, "ok": bool(ok), "detail": detail})

        events = self.path(cq_collect.EVENTS_FILE)
        try:
            lines = cq_collect.read_events(events) if os.path.exists(events) else []
        except cq_collect.CollectError as exc:
            raise AdapterError(str(exc).replace(events, cq_collect.EVENTS_FILE)) from exc
        order = [e["type"] for e in lines if e["type"] in ("hold_start", "hold_end", "teardown_start")]
        walls = [e["wall"] for e in lines if e["type"] in ("hold_start", "hold_end", "teardown_start")]
        complete = order == ["hold_start", "hold_end", "teardown_start"] and walls == sorted(walls)
        mono = {e["type"]: e.get("mono") for e in lines if e["type"] in ("hold_start", "hold_end")}
        held = mono["hold_end"] - mono["hold_start"] if complete and all(map(_num, mono.values())) else None
        on_time = held is not None and held <= self.step["hold_s"] + EVENT_SLACK_S
        gate("R1", complete and on_time and not s.interrupted and s.aborted is None,
             f"hold_start, hold_end, teardown_start in order; the hold lasted {held:.1f} s"
             if complete and on_time and not (s.interrupted or s.aborted)
             else f"timeline {order}; hold {held} s on the monotonic clock, planned {self.step['hold_s']} s "
                  f"+ {EVENT_SLACK_S} s; interrupted {s.interrupted}; aborted {s.aborted}")
        planned = {p["user_id"] for p in self.plan["participants"]}
        joined = {u for e in lines if e["type"] == "joined" for u in e["participants"]}
        gate("R2", joined == planned, f"missing {sorted(planned - joined)}; extra {sorted(joined - planned)}"
             if joined != planned else f"{len(joined)} joined")
        gate("R3", s.collector_rc == cq_collect.EXIT_OK and s.manifest_sha is not None and s.validate_rc == 0,
             f"collector exit {s.collector_rc}, validate-only exit {s.validate_rc}")
        gate("R4", s.manifest_sha is not None and s.manifest_sha == s.manifest_sha_after,
             f"collected {s.manifest_sha}, scored {s.manifest_sha_after}")
        teardown = s.marks.get("teardown_start", math.inf)
        unplanned = sorted(pid for pid, ex in s.exits.items()
                           if _num(ex.exited_at) and ex.exited_at < teardown and not (ex.exit_code == 0 and all(
                               s.final_leave.get(u, math.inf) <= ex.exited_at
                               for u in s.launched[pid]["participants"])))
        never = sorted(set(s.launched) - set(s.exits))
        r5 = unplanned + [f"{p}: never stopped" for p in never] + s.exit_problems
        gate("R5", not r5, "; ".join(r5) or "no unplanned exits")
        ok_events = {e["event_id"] for e in s.event_lines if e["result"] == "ok" and _num(e["t_confirmed"])}
        planned_events = [e["event_id"] for e in self.plan["events"]]
        last = self.step["hold_s"] - 2 * cq_score.cv(cq_score.load_config(), "sampling", "scrape_step_s")
        late = [f"{e['event_id']} issued at hold+{s.issued_mono[e['event_id']] - s.hold_mono['hold_start']:.1f} s, "
                f"planned hold+{e['at_offset_s']} s" for e in self.plan["events"]
                if e["event_id"] in s.issued_mono and s.issued_mono[e["event_id"]] - s.hold_mono["hold_start"]
                > min(e["at_offset_s"] + EVENT_SLACK_S, last)]
        gate("R6", set(planned_events) == ok_events and len(s.event_lines) == len(planned_events) and not late,
             "; ".join([f"{len(ok_events)} of {len(planned_events)} planned events confirmed"] + late))
        gate("R7", bool(s.stack_ready) and s.stack_ready == s.stack_teardown,
             "stack containers unchanged" if bool(s.stack_ready) and s.stack_ready == s.stack_teardown
             else "a stack container restarted, or the stack state is unknown")
        gate("R8", s.drift_base is not None and s.max_drift <= MAX_CLOCK_DRIFT_S,
             f"wall - mono drifted {s.max_drift:.3f} s (limit {MAX_CLOCK_DRIFT_S} s)")
        clean = s.dirty_at_preflight is False and s.dirty_at_teardown is False and s.commit == s.commit_at_teardown
        gate("R9", clean, f"dirty at preflight {s.dirty_at_preflight}, at teardown {s.dirty_at_teardown}, "
             f"HEAD unchanged {s.commit == s.commit_at_teardown}")
        gate("R10", *self._strict_config_echoed())
        return gates

    def _strict_config_echoed(self):
        r = self.s.result
        if r is None or self.s.config_path is None:
            return False, "not scored"
        try:
            cfg = cq_score.load_config(self.s.config_path)
            with open(self.path("generator-verdict.json"), encoding="utf-8") as fh:
                want = "pass" if json.load(fh).get("ok") is True else "fail"
        except (OSError, ValueError, cq_score.ConfigError) as exc:
            return False, f"cannot read the forced config or the generator verdict: {_describe(exc)}"
        sha = hashlib.sha256(json.dumps(cfg, sort_keys=True).encode()).hexdigest()
        if r.get("config_overrides") != cq_score.config_overrides(cfg) or r.get("run", {}).get("config_sha256") != sha:
            return False, "result.json does not echo the forced scorer config"
        wrong = [s.get("step_id") for s in r.get("steps", [])
                 if next((g.get("status") for g in s.get("validity", []) if g.get("gate") == "G-V5"), None) != want]
        if wrong or not r.get("steps"):
            return False, f"G-V5 is not {want}, as generator-verdict.json says, in steps {wrong}"
        return True, f"forced allowed_not_measured {FORCED_NOT_MEASURED}; G-V5 {want} as generator-verdict.json says"

    def _phases(self):
        if self.s.interrupted:
            raise KeyboardInterrupt
        self._preflight()
        self._stack_up()
        self._join()
        self._hold()

    def _guarded(self, fn, mode):
        try:
            try:
                self._signal_mode = mode
                fn()
            except Abort as exc:
                self.s.aborted = str(exc)
            except (Refused, AdapterError) as exc:
                self.s.error = self.s.error or f"{self.s.phase}: {_describe(exc)}"
            except Exception as exc:  # noqa: BLE001
                self.s.error = self.s.error or f"{self.s.phase}: internal error {_describe(exc)}"
            self._signal_mode = "record"
        except BaseException as exc:  # noqa: B036 - teardown still runs
            self._signal_mode = "record"
            if isinstance(exc, KeyboardInterrupt):
                self.s.interrupted = True
            else:
                self.s.error = self.s.error or f"{self.s.phase}: {_describe(exc)}"

    def run(self):
        self._guarded(self._phases, "raise")
        launched_any = bool(self.s.launched)
        if self.log is not None and (launched_any or self.s.stack_started):
            self._teardown()
        if launched_any:
            self._guarded(self._collect, "record")
            if self.s.manifest_sha is not None and self.s.error is None:
                self._guarded(self._score, "raise")
        self._down()
        return self._finish()

    def _down(self):
        if self.s.stack_started and self.stack_mode == "up" and not self.keep_stack:
            try:
                self.stack.down()
            except BaseException as exc:  # noqa: B036 - run.json is still written
                self.s.error = self.s.error or f"down: {_describe(exc)}"

    def _gates_or_error(self):
        try:
            return self.gates() if self.log is not None else []
        except Exception as exc:  # noqa: BLE001
            self.s.error = self.s.error or f"gates: internal error {_describe(exc)}"
            return []

    def _record(self, gates):
        scorer_verdict = (self.s.result or {}).get("verdict")
        verdict = compose_verdict(scorer_verdict, gates, self.s.error)
        return {"schema": RUN_SCHEMA, "run_id": self.plan["run_id"], "verdict": verdict, "exit_code": RANK[verdict],
                "scorer_verdict": scorer_verdict, "gates": gates, "phase": self.s.phase, "error": self.s.error,
                "aborted": self.s.aborted, "interrupted": self.s.interrupted, "signals": list(self.s.signals),
                "collector_exit": self.s.collector_rc, "teardown_problems": self.s.teardown_problems,
                "manifest_sha256": self.s.manifest_sha, "timeline": dict(self.s.marks),
                "fidelity_waivers": self.plan["fidelity_waivers"],
                "expectation": check_expectation(self.plan["scoring"]["expect"], self.s.result, gates, self.s.error),
                "versions": {"python": sys.version.split()[0], "commit": self.s.commit}}

    def _commands(self):
        with open(self.path("commands.txt"), "w", encoding="utf-8") as fh:
            for argv in ([self.argv] if self.argv else []) + self.s.commands:
                argv = ["python3" if a == sys.executable else str(a) for a in call_quality_score.redacted_argv(argv)]
                fh.write(shlex.join(a.replace(self.dir, "$RUN").replace(HERE, "scripts/quality") for a in argv) + "\n")

    def _finish(self):
        seen = self.s.interrupted
        gates = self._gates_or_error()
        if self.s.interrupted != seen:
            gates = self._gates_or_error()
        steps = [("commands.txt", self._commands)]
        if self.log is not None:
            steps += [("events.jsonl done line",
                       lambda: self.log.write("done", verdict=self._record(gates)["verdict"])),
                      ("events.jsonl", self.log.close)]
        for what, fn in steps:
            try:
                fn()
            except BaseException as exc:  # noqa: B036 - run.json is written next
                self.s.error = self.s.error or f"finish: {what} not written ({_describe(exc)})"
        run = self._record(gates)
        try:
            _dump(self.path("run.json"), run)
        except BaseException as exc:  # noqa: B036 - the exit code still says ERROR
            self.s.error = self.s.error or f"finish: run.json not written ({_describe(exc)})"
            run = self._record(gates)
        return run
