"""Scenario file v1 (scale-scenario/v1): strict YAML loading and compilation into the resolved plan (#2914)."""

import collections
import math
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import cq_manifest  # noqa: E402
import cq_score  # noqa: E402

SCHEMA = "scale-scenario/v1"
PLAN_SCHEMA = "scale-scenario-plan/v1"
TARGETS = {"local": "local", "ci": "ci-smoke"}
TOP_KEYS = {"schema", "run", "population", "steps", "events", "scoring"}
RUN_KEYS = {"id_prefix", "seed", "target", "fidelity_waivers"}
ROLE_KEYS = {"role", "fleet", "count", "media", "network", "transport", "join_stagger", "receive"}
MEDIA_KEYS = {"camera", "mic", "video_layers"}
RECEIVE_KEYS = {"pin_video_layer", "viewport_visible_count"}
STEP_KEYS = {"id", "hold", "headline", "join_window"}
EVENT_KEYS = {"at", "select", "action"}
SELECT_KEYS = {"role", "count"}
SCORING_KEYS = {"mode", "relay_path_exemptions", "expect"}
EXPECT_KEYS = {"verdict", "invalid_gates"}
CAMERA = {"on": True, "off": False}
MICS = ("continuous", "muted", "off")
ACTIONS = ("leave", "mute", "unmute")
REFUSED_ACTIONS = {
    "camera-on": "G-V13 makes it INVALID in scorer v1",
    "camera-off": "G-V13 makes it INVALID in scorer v1",
    "outage": "G-V13 makes it INVALID in scorer v1",
    "netem": "probes run with host networking on local and ci targets; netem is cluster only",
    "rejoin": "a v1 limit: the collector allows one record per user id, and bots-app has no rejoin control",
}
ID_PREFIX_RE = re.compile(r"^[a-z0-9]{1,12}$")
NAME_RE = re.compile(r"^[a-z][a-z0-9-]{0,23}$")
GATE_RE = re.compile(r"^G-V[0-9]{1,2}$")
DURATION_RE = re.compile(r"^([0-9]{1,6})(ms|s|m|h)$")
AT_RE = re.compile(r"^hold\+([0-9]{1,6}(?:ms|s|m|h))$")
UNITS = {"ms": 0.001, "s": 1, "m": 60, "h": 3600}
MIN_HOLD_S = 60
EVENT_SLACK_S = 5
PROC_PUB = "rust-pub"
PROC_ROLE = "rust-role-{}"
PROC_LEAVE = "rust-leave-{:02d}"
JOIN_WINDOW_BASE_S = 60
BROWSER_DOMAIN = "bots-app.local"
EXPLICIT_TAGS = {f"tag:yaml.org,2002:{t}" for t in ("str", "seq", "map", "null")}


class ScenarioError(Exception):
    def __init__(self, errors):
        super().__init__("scenario refused:\n  " + "\n  ".join(errors))
        self.errors = errors


def _strict_loader():
    try:
        import yaml
    except ImportError as exc:
        raise ScenarioError(["PyYAML is required: pip install -r scripts/quality/requirements-runner.txt"]) from exc

    class Loader(yaml.SafeLoader):
        def construct_mapping(self, node, deep=False):
            seen = set()
            for key_node, _ in node.value:
                key = self.construct_object(key_node, deep=deep)
                if not isinstance(key, str):
                    raise yaml.constructor.ConstructorError(None, None, f"mapping key {key!r} is not a string",
                                                            key_node.start_mark)
                if key in seen:
                    raise yaml.constructor.ConstructorError(None, None, f"duplicate key {key!r}",
                                                            key_node.start_mark)
                seen.add(key)
            return super().construct_mapping(node, deep)

        def compose_node(self, parent, index):
            event = self.peek_event()
            tag = getattr(event, "tag", None)
            if tag not in (None, "!") and tag not in EXPLICIT_TAGS:
                raise yaml.composer.ComposerError(None, None, f"explicit tag {tag} is refused", event.start_mark)
            return super().compose_node(parent, index)

    keep = {"tag:yaml.org,2002:null"}
    Loader.yaml_implicit_resolvers = {
        ch: [(tag, rx) for tag, rx in rs if tag in keep]
        for ch, rs in yaml.SafeLoader.yaml_implicit_resolvers.items()}
    Loader.add_implicit_resolver("tag:yaml.org,2002:bool", re.compile(r"^(?:true|false)$"), list("tf"))
    Loader.add_implicit_resolver("tag:yaml.org,2002:int", re.compile(r"^-?(?:0|[1-9][0-9]*)$"),
                                 list("-0123456789"))
    return yaml, Loader


def parse_yaml(text):
    yaml, loader = _strict_loader()
    try:
        return yaml.load(text, Loader=loader)  # noqa: S506 - a SafeLoader subclass
    except yaml.YAMLError as exc:
        raise ScenarioError([f"YAML: {exc}"]) from exc
    except RecursionError as exc:
        raise ScenarioError(["YAML: nested too deeply"]) from exc


def load_scenario(path):
    try:
        with open(path, "rb") as fh:
            raw = fh.read()
        text = raw.decode("utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        raise ScenarioError([f"{path}: {exc}"]) from exc
    return parse_yaml(text), raw


def duration_s(text):
    m = DURATION_RE.fullmatch(text) if isinstance(text, str) else None
    return None if m is None else int(m.group(1)) * UNITS[m.group(2)]


def _int(v):
    return isinstance(v, int) and not isinstance(v, bool)


class _Compiler:
    def __init__(self):
        self.errors = []

    def err(self, msg):
        self.errors.append(msg)

    def keys(self, obj, allowed, where, required=()):
        if not isinstance(obj, dict):
            self.err(f"{where}: must be a mapping")
            return False
        for k in sorted(set(obj) - allowed):
            self.err(f"{where}.{k}: unknown key")
        for k in required:
            if k not in obj:
                self.err(f"{where}.{k}: required")
        return all(k in obj for k in required)

    def role(self, i, r):
        where = f"population[{i}]"
        if not self.keys(r, ROLE_KEYS, where, ("role", "fleet", "count", "media")):
            return None
        name, fleet, count, media = r["role"], r["fleet"], r["count"], r["media"]
        if not isinstance(name, str) or not NAME_RE.fullmatch(name):
            self.err(f"{where}.role: must match {NAME_RE.pattern}")
            return None
        where = f"population[{name}]"
        if fleet not in ("browser", "rust"):
            self.err(f"{where}.fleet: must be browser or rust")
            return None
        if not _int(count) or count < 1:
            self.err(f"{where}.count: must be a positive integer")
            return None
        if "screen" in (media if isinstance(media, dict) else {}):
            self.err(f"{where}.media.screen: screen share is refused (G-V13 makes it INVALID in scorer v1)")
            media = {k: v for k, v in media.items() if k != "screen"}
        if not self.keys(media, MEDIA_KEYS, f"{where}.media", ("camera", "mic")):
            return None
        camera, mic = CAMERA.get(media["camera"]), media["mic"]
        if camera is None:
            self.err(f"{where}.media.camera: must be on or off")
        if mic not in MICS:
            self.err(f"{where}.media.mic: must be one of {', '.join(MICS)}")
        if camera is None or mic not in MICS:
            return None
        network = r.get("network", "none")
        if network != "none":
            self.err(f"{where}.network: {network!r} is refused; local and ci targets cannot shape one participant")
        out = {"role": name, "fleet": fleet, "count": count, "camera": camera, "mic": mic,
               "talker": mic == "continuous"}
        if fleet == "browser":
            self._browser(r, media, where, out)
        else:
            self._rust(r, media, where, out)
        return out

    def _browser(self, r, media, where, out):
        transport = r.get("transport", "auto")
        if transport not in cq_manifest.TRANSPORTS:
            self.err(f"{where}.transport: must be one of {', '.join(cq_manifest.TRANSPORTS)}")
        stagger = duration_s(r.get("join_stagger", "0s"))
        if stagger is None:
            self.err(f"{where}.join_stagger: must be a duration such as 3s")
        if "receive" in r:
            self.err(f"{where}.receive: only Rust roles take a receive config")
        if "video_layers" in media:
            self.err(f"{where}.media.video_layers: only Rust roles take video_layers")
        out.update(transport=transport, join_stagger_s=stagger or 0)

    def _rust(self, r, media, where, out):
        transport = r.get("transport", "websocket")
        if transport != "websocket":
            self.err(f"{where}.transport: Rust bots run websocket only (webtransport is invalid until R11)")
        if "join_stagger" in r:
            self.err(f"{where}.join_stagger: only browser roles are staggered")
        layers = media.get("video_layers", 3 if out["camera"] else None)
        if out["camera"] and (not _int(layers) or not 1 <= layers <= 3):
            self.err(f"{where}.media.video_layers: must be 1, 2 or 3")
        if not out["camera"] and "video_layers" in media:
            self.err(f"{where}.media.video_layers: a role with camera off publishes no video layers")
        receive = r.get("receive")
        if receive is not None:
            if out["camera"] or out["mic"] != "off":
                self.err(f"{where}.receive: only a viewer role (camera off, mic off) takes a receive config, "
                         "because publishers share one process")
            elif self.keys(receive, RECEIVE_KEYS, f"{where}.receive"):
                pin = receive.get("pin_video_layer")
                if pin is not None and (not _int(pin) or not 0 <= pin <= 2):
                    self.err(f"{where}.receive.pin_video_layer: must be 0, 1 or 2")
                vis = receive.get("viewport_visible_count")
                if vis is not None and (not _int(vis) or vis < 0):
                    self.err(f"{where}.receive.viewport_visible_count: must be a non-negative integer")
        out.update(transport="websocket", video_layers=layers if out["camera"] else None, receive=receive or {})


def _plan_participants(roles, run_id):
    parts = []
    for r in roles:
        for k in range(r["count"]):
            name = f"{r['role']}-{k:02d}"
            uid = f"{run_id}-{name}" + (f"@{BROWSER_DOMAIN}" if r["fleet"] == "browser" else "")
            parts.append({"user_id": uid, "name": name, "fleet": r["fleet"], "role": r["role"],
                          "observer": r["fleet"] == "browser", "talker": r["talker"],
                          "publishes": {"camera": r["camera"], "mic": r["mic"] == "continuous", "screen": False}})
    return parts


def _events(c, events, roles, parts, hold_s, step_s):
    by_role = {r["role"]: [p for p in parts if p["role"] == r["role"]] for r in roles}
    left, mic, out, parsed = set(), {p["user_id"]: p["publishes"]["mic"] for p in parts}, [], []
    if not isinstance(events, list):
        c.err("events: must be a list")
        return out
    for i, e in enumerate(events):
        where = f"events[{i}]"
        if not c.keys(e, EVENT_KEYS, where, ("at", "select", "action")):
            continue
        action, m = e["action"], AT_RE.fullmatch(e["at"]) if isinstance(e["at"], str) else None
        if action in REFUSED_ACTIONS:
            c.err(f"{where}.action: {action} is refused: {REFUSED_ACTIONS[action]}")
            continue
        if action not in ACTIONS:
            c.err(f"{where}.action: must be one of {', '.join(ACTIONS)}")
            continue
        offset = duration_s(m.group(1)) if m else None
        last = 2 * step_s + EVENT_SLACK_S
        if offset is None or not step_s <= offset <= hold_s - last:
            c.err(f"{where}.at: must be hold+<duration> within [{step_s} s, hold - {last} s]")
            continue
        sel = e["select"]
        if not c.keys(sel, SELECT_KEYS, f"{where}.select", ("role", "count")):
            continue
        if sel["role"] not in by_role or not _int(sel["count"]) or sel["count"] < 1:
            c.err(f"{where}.select: needs a declared role and a positive count")
            continue
        parsed.append((offset, i, action, sel))
    for offset, i, action, sel in sorted(parsed, key=lambda x: (x[0], x[1])):
        where = f"events[{i}]"
        present = [p for p in by_role[sel["role"]] if p["user_id"] not in left]
        if sel["count"] > len(present):
            c.err(f"{where}.select: {sel['count']} of role {sel['role']} requested, {len(present)} still present")
            continue
        chosen = present[-sel["count"]:]
        for p in chosen:
            uid = p["user_id"]
            if action == "leave":
                left.add(uid)
            elif p["fleet"] == "rust":
                c.err(f"{where}: {action} of Rust participant {uid} is refused; the Rust bot has no runtime control")
            elif p["talker"]:
                c.err(f"{where}: {action} of declared talker {uid} inside the hold is refused (G-V13, D34)")
            elif mic[uid] == (action == "unmute"):
                c.err(f"{where}: {action} of {uid} is not paired; its mic is already "
                      f"{'on' if mic[uid] else 'off'}")
            else:
                mic[uid] = action == "unmute"
        out.append({"event_id": f"e{len(out)}", "at_offset_s": offset, "action": action,
                    "participants": [p["user_id"] for p in chosen], "params": {}})
    return out


def _processes(c, roles, parts, leavers):
    procs, rust_order = [], []
    by_name = {r["role"]: r for r in roles}
    for k, p in enumerate(q for q in parts if q["fleet"] == "browser"):
        role = by_name[p["role"]]
        procs.append({"proc_id": f"probe-{k:02d}", "fleet": "browser", "participants": [p["user_id"]],
                      "probe_index": k, "transport": role["transport"], "join_stagger_s": role["join_stagger_s"]})
    rust = {r["role"]: r for r in roles if r["fleet"] == "rust"}
    publishers = [r for r in rust.values() if r["camera"] or r["mic"] != "off"]
    if len({r["video_layers"] for r in publishers if r["camera"]}) > 1:
        c.err("population: Rust publisher roles share one process, so their video_layers must match")
    groups = [(PROC_PUB, [p for p in parts if p["role"] in {r["role"] for r in publishers}
                            and p["user_id"] not in leavers], {})]
    groups += [(PROC_ROLE.format(r["role"]),
                [p for p in parts if p["role"] == r["role"] and p["user_id"] not in leavers],
                r["receive"]) for r in rust.values() if r not in publishers]
    groups += [(PROC_LEAVE.format(k), [p], rust[p["role"]]["receive"])
               for k, p in enumerate(p for p in parts if p["user_id"] in leavers and p["fleet"] == "rust")]
    for proc_id, members, receive in groups:
        if not members:
            continue
        if any(p["publishes"]["camera"] for p in members) and not any(p["talker"] for p in members):
            c.err(f"{proc_id}: a Rust process with cameras but no talker sends no video (PERF-B M3)")
        cams = [rust[p["role"]]["video_layers"] for p in members if p["publishes"]["camera"]]
        procs.append({"proc_id": proc_id, "fleet": "rust", "participants": [p["user_id"] for p in members],
                      "roster_offset": len(rust_order), "video_layers": cams[0] if cams else None,
                      "receive": dict(receive)})
        rust_order += [p["user_id"] for p in members]
    for p in procs:
        if p["fleet"] == "rust":
            p["run_size"] = len(rust_order)
    ids = collections.Counter(p["proc_id"] for p in procs)
    for dup in sorted(i for i, n in ids.items() if n > 1):
        c.err(f"processes: two processes are named {dup}; each must get its own stop")
    return procs


def _scoring(c, scoring):
    scoring = {"mode": "gate", "relay_path_exemptions": {}, "expect": None, **(scoring or {})}
    c.keys(scoring, SCORING_KEYS, "scoring")
    if scoring["mode"] != "gate":
        c.err("scoring.mode: only gate is supported on local and ci targets (report mode is cluster Phase 0)")
    ex = scoring["relay_path_exemptions"]
    if not isinstance(ex, dict) or len(ex) > cq_score.MAX_RELAY_EXEMPTIONS or any(
            k not in cq_score.RELAY_PATHS or not isinstance(v, str) or not v.strip() for k, v in ex.items()):
        c.err(f"scoring.relay_path_exemptions: at most {cq_score.MAX_RELAY_EXEMPTIONS} known relay counters, "
              "each with a reason")
    expect = scoring["expect"]
    if expect is not None and c.keys(expect, EXPECT_KEYS, "scoring.expect", ("verdict", "invalid_gates")):
        if expect["verdict"] not in ("INVALID", "FAIL"):
            c.err("scoring.expect.verdict: must be INVALID or FAIL; a scenario never expects PASS")
        gates = expect["invalid_gates"]
        if not isinstance(gates, list) or not all(isinstance(g, str) and GATE_RE.fullmatch(g) for g in gates) \
                or len(set(gates)) != len(gates) or (expect["verdict"] == "INVALID") != bool(gates):
            c.err("scoring.expect.invalid_gates: distinct G-V gate ids, non-empty exactly when verdict is INVALID")
    return scoring


def compile_scenario(doc, *, run_id, scenario_file, scenario_sha256):
    try:
        return _compile(doc, run_id, scenario_file, scenario_sha256)
    except (TypeError, AttributeError, KeyError) as exc:
        raise ScenarioError([f"scenario: a value has the wrong shape ({type(exc).__name__}: {exc})"]) from exc


def _compile(doc, run_id, scenario_file, scenario_sha256):
    c = _Compiler()
    if not c.keys(doc, TOP_KEYS, "scenario", ("schema", "run", "population", "steps")):
        raise ScenarioError(c.errors)
    if doc["schema"] != SCHEMA:
        c.err(f"schema: must be {SCHEMA}")
    run = doc["run"]
    if c.keys(run, RUN_KEYS, "run", ("id_prefix", "seed", "target")):
        if run["target"] not in TARGETS:
            c.err(f"run.target: must be one of {', '.join(TARGETS)} (cluster runs come with PR-8)")
        if not _int(run["seed"]):
            c.err("run.seed: must be an integer")
        waivers = run.get("fidelity_waivers", [])
        if not isinstance(waivers, list) or not all(isinstance(w, str) and w.strip() for w in waivers):
            c.err("run.fidelity_waivers: must be a list of non-empty strings")
    if not cq_manifest.RUN_ID_RE.match(run_id or ""):
        c.err(f"run_id {run_id!r}: must match [a-z0-9-]{{1,40}}")
    pop = doc["population"]
    roles = [c.role(i, r) for i, r in enumerate(pop)] if isinstance(pop, list) and pop else []
    if not roles:
        c.err("population: must be a non-empty list")
    names = [r["role"] for r in roles if r]
    if len(set(names)) != len(names):
        c.err("population: role names must be unique")
    steps = doc["steps"]
    if not isinstance(steps, list) or len(steps) != 1:
        c.err("steps: v1 runs exactly one step (D4)")
        raise ScenarioError(c.errors)
    step = steps[0]
    if c.errors or None in roles or not c.keys(step, STEP_KEYS, "steps[0]", ("id", "hold", "headline")):
        raise ScenarioError(c.errors)
    if not isinstance(step["id"], str) or not NAME_RE.fullmatch(step["id"]):
        c.err(f"steps[0].id: must match {NAME_RE.pattern}")
    if step["headline"] is not True:
        c.err("steps[0].headline: the only step is the headline step")
    step_s = cq_score.cv(cq_score.load_config(), "sampling", "scrape_step_s")
    hold_s = duration_s(step["hold"])
    if hold_s is None or hold_s < MIN_HOLD_S:
        c.err(f"steps[0].hold: must be a duration of at least {MIN_HOLD_S} s")
        raise ScenarioError(c.errors)
    parts = _plan_participants(roles, run_id)
    n_browser = sum(1 for p in parts if p["fleet"] == "browser")
    stagger = max((r.get("join_stagger_s", 0) for r in roles), default=0)
    join_window = duration_s(step["join_window"]) if "join_window" in step \
        else JOIN_WINDOW_BASE_S + stagger * max(0, n_browser - 1)
    if join_window is None:
        c.err("steps[0].join_window: must be a duration")
    cameras = sum(1 for p in parts if p["publishes"]["camera"])
    if cameras > cq_score.CANVAS_LIMIT:
        c.err(f"population: {cameras} camera publishers; more than {cq_score.CANVAS_LIMIT} makes the step INVALID "
              "by construction (G-V12)")
    events = _events(c, doc.get("events") or [], roles, parts, hold_s, step_s)
    leavers = {u for e in events if e["action"] == "leave" for u in e["participants"]}
    processes = _processes(c, roles, parts, leavers)
    scoring = _scoring(c, doc.get("scoring"))
    if c.errors:
        raise ScenarioError(c.errors)
    owner = {u: q["proc_id"] for q in reversed(processes) for u in q["participants"]}
    for p in parts:
        p["steps"] = [step["id"]]
        p["process"] = owner[p["user_id"]]
    return {
        "plan_schema": PLAN_SCHEMA, "run_id": run_id, "meeting_id": f"scale-{run_id}",
        "environment": TARGETS[run["target"]], "target": run["target"], "seed": run["seed"],
        "fidelity_waivers": list(run.get("fidelity_waivers", [])),
        "scenario": {"file": scenario_file, "sha256": scenario_sha256}, "node_aliases": {},
        "steps": [{"step_id": step["id"], "n_target": len(parts) - len(leavers), "headline": True,
                   "hold_s": hold_s, "join_window_s": join_window}],
        "participants": parts, "processes": processes, "events": events, "scoring": scoring,
    }


def derive_run_id(id_prefix, wall, commit):
    if not isinstance(id_prefix, str) or not ID_PREFIX_RE.fullmatch(id_prefix):
        raise ScenarioError([f"run.id_prefix: must match {ID_PREFIX_RE.pattern}"])
    if not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{7,40}", commit) or not math.isfinite(wall):
        raise ScenarioError(["run_id: needs a hex commit and a finite wall time"])
    return f"{id_prefix}-{time.strftime('%Y%m%dt%H%M', time.gmtime(wall))}-{commit[:7]}"
