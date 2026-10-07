"""Run manifest loading and validation (schema: docs/quality-at-scale/CALL_QUALITY_SCORING.md section 5.2)."""

import json
import math
import re

SCHEMA = "call-quality-run-manifest/v1"
SCHEMA_MAJOR_PREFIX = "call-quality-run-manifest/v"

FLEETS = ("browser", "rust", "human")
CLOCK_SYNC = ("chrony", "ntp", "none", "unknown")
DIRECTIONS = ("none", "egress", "ingress", "both")
SHAPERS = ("none", "netem", "netsim", "region")
TRANSPORTS = ("auto", "websocket", "webtransport")
EVENT_ACTIONS = ("netem", "leave", "rejoin", "outage", "mute", "unmute", "camera-off", "camera-on")
RUN_ID_RE = re.compile(r"^[a-z0-9-]{1,40}$")


class ManifestError(Exception):
    def __init__(self, errors):
        super().__init__("invalid run manifest:\n  " + "\n  ".join(errors))
        self.errors = errors


def load_manifest(path):
    with open(path, encoding="utf-8") as fh:
        text = fh.read()
    if path.endswith((".yaml", ".yml")):
        try:
            import yaml
        except ImportError as exc:
            raise ManifestError(["YAML manifests need PyYAML; install it or use JSON"]) from exc
        m = yaml.safe_load(text)
    else:
        m = json.loads(text)
    bad = non_finite_paths(m)
    if bad:
        raise ManifestError([f"{p}: NaN and infinity are not numbers here" for p in bad])
    return m


def non_finite_paths(obj, path="$"):
    """JSON paths of NaN/Infinity values, which json.load and YAML accept and every comparison treats as False."""
    if isinstance(obj, float) and not math.isfinite(obj):
        return [path]
    if isinstance(obj, dict):
        return [p for k, v in obj.items() for p in non_finite_paths(v, f"{path}.{k}")]
    if isinstance(obj, list):
        return [p for i, v in enumerate(obj) for p in non_finite_paths(v, f"{path}[{i}]")]
    return []


def _is_num(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool)


class _Checker:
    def __init__(self):
        self.errors = []

    def err(self, path, msg):
        self.errors.append(f"{path}: {msg}")

    def obj(self, parent, key, path, required=True):
        if key not in parent:
            if required:
                self.err(f"{path}.{key}", "required field missing")
            return None
        val = parent[key]
        if not isinstance(val, dict):
            self.err(f"{path}.{key}", "must be an object")
            return None
        return val

    def typed(self, parent, key, path, kind, required=True, nullable=False, choices=None):
        if key not in parent:
            if required:
                self.err(f"{path}.{key}", "required field missing")
            return None
        val = parent[key]
        if val is None:
            if not nullable:
                self.err(f"{path}.{key}", "must not be null")
            return None
        ok = {
            "str": isinstance(val, str) and val != "",
            "num": _is_num(val),
            "bool": isinstance(val, bool),
            "list": isinstance(val, list),
        }[kind]
        if not ok:
            self.err(f"{path}.{key}", f"must be a {'non-empty string' if kind == 'str' else kind}")
            return None
        if choices is not None and val not in choices:
            self.err(f"{path}.{key}", f"must be one of {', '.join(choices)} (got {val!r})")
            return None
        return val


def validate_manifest(m):
    """Return a list of human-readable errors; empty means valid."""
    c = _Checker()
    if not isinstance(m, dict):
        return ["$: manifest must be a JSON object"]
    for p in non_finite_paths(m):
        c.err(p, "NaN and infinity are not numbers here")
    schema = c.typed(m, "schema", "$", "str")
    if schema is not None and schema != SCHEMA:
        if schema.startswith(SCHEMA_MAJOR_PREFIX):
            c.err("$.schema", f"unsupported major version {schema!r}; this scorer reads {SCHEMA!r}")
        else:
            c.err("$.schema", f"expected {SCHEMA!r}")
    run_id = c.typed(m, "run_id", "$", "str")
    if run_id is not None and not RUN_ID_RE.match(run_id):
        c.err("$.run_id", "must match [a-z0-9-]{1,40}")
    c.typed(m, "meeting_id", "$", "str")
    c.typed(m, "environment", "$", "str")

    code = c.obj(m, "code", "$")
    if code is not None:
        c.typed(code, "commit", "$.code", "str")
        images = c.obj(code, "images", "$.code")
        for k, v in (images or {}).items():
            if not isinstance(v, str) or not v:
                c.err(f"$.code.images.{k}", "must be a non-empty string")

    scenario = c.obj(m, "scenario", "$")
    if scenario is not None:
        c.typed(scenario, "file", "$.scenario", "str")
        c.typed(scenario, "sha256", "$.scenario", "str")

    clock = c.obj(m, "clock", "$")
    if clock is not None:
        c.typed(clock, "sync", "$.clock", "str", choices=CLOCK_SYNC)
        c.typed(clock, "max_abs_skew_ms", "$.clock", "num", required=False, nullable=True)
        c.typed(clock, "measured_at", "$.clock", "num", required=False, nullable=True)

    step_ids = _validate_steps(c, m)
    _validate_events(c, m, step_ids)
    _validate_participants(c, m, step_ids)
    return c.errors


def _validate_steps(c, m):
    steps = c.typed(m, "steps", "$", "list")
    ids = []
    if steps is None:
        return ids
    if not steps:
        c.err("$.steps", "must contain at least one step")
    headlines = 0
    prev_hold_end = None
    for i, s in enumerate(steps):
        p = f"$.steps[{i}]"
        if not isinstance(s, dict):
            c.err(p, "must be an object")
            continue
        sid = c.typed(s, "step_id", p, "str")
        if sid is not None:
            if sid in ids:
                c.err(f"{p}.step_id", f"duplicate step_id {sid!r}")
            ids.append(sid)
        n = c.typed(s, "n_target", p, "num")
        if n is not None and (int(n) != n or n < 1):
            c.err(f"{p}.n_target", "must be a positive integer")
        js = c.typed(s, "join_start", p, "num")
        hs = c.typed(s, "hold_start", p, "num")
        he = c.typed(s, "hold_end", p, "num")
        if None not in (js, hs, he):
            if not js <= hs < he:
                c.err(p, "require join_start <= hold_start < hold_end")
            if prev_hold_end is not None and js < prev_hold_end:
                c.err(p, "steps must be ordered: join_start must not precede the previous step's hold_end")
            prev_hold_end = he
        hl = c.typed(s, "headline", p, "bool")
        if hl:
            headlines += 1
    if steps and headlines != 1:
        c.err("$.steps", f"exactly one step must have headline=true (found {headlines})")
    return ids


def _validate_events(c, m, step_ids):
    events = c.typed(m, "events", "$", "list", required=False)
    for i, e in enumerate(events or []):
        p = f"$.events[{i}]"
        if not isinstance(e, dict):
            c.err(p, "must be an object")
            continue
        c.typed(e, "at", p, "num")
        sid = c.typed(e, "step_id", p, "str")
        if sid is not None and sid not in step_ids:
            c.err(f"{p}.step_id", f"unknown step_id {sid!r}")
        c.typed(e, "action", p, "str", choices=EVENT_ACTIONS)
        c.typed(e, "participants", p, "list")
        if "params" in e and not isinstance(e["params"], dict):
            c.err(f"{p}.params", "must be an object")


def _validate_participants(c, m, step_ids):
    parts = c.typed(m, "participants", "$", "list")
    if parts is None:
        return
    if not parts:
        c.err("$.participants", "must contain at least one participant")
    seen = set()
    for i, pt in enumerate(parts):
        p = f"$.participants[{i}]"
        if not isinstance(pt, dict):
            c.err(p, "must be an object")
            continue
        uid = c.typed(pt, "user_id", p, "str", nullable=True)
        if uid is not None:
            if uid in seen:
                c.err(f"{p}.user_id", f"duplicate user_id {uid!r} (gate G-V8)")
            seen.add(uid)
        fleet = c.typed(pt, "fleet", p, "str", choices=FLEETS)
        c.typed(pt, "role", p, "str")
        observer = c.typed(pt, "observer", p, "bool")
        if observer and fleet == "rust":
            c.err(f"{p}.observer", "Rust bots cannot be observers (their receive-side values are synthetic)")
        c.typed(pt, "talker", p, "bool")
        pub = c.obj(pt, "publishes", p)
        if pub is not None:
            for k in ("camera", "mic", "screen"):
                c.typed(pub, k, f"{p}.publishes", "bool")
        net = c.obj(pt, "network", p)
        if net is not None:
            c.typed(net, "profile", f"{p}.network", "str")
            c.typed(net, "shaped", f"{p}.network", "bool")
            c.typed(net, "direction", f"{p}.network", "str", required=False, choices=DIRECTIONS)
            c.typed(net, "shaper", f"{p}.network", "str", required=False, choices=SHAPERS)
            if "params" in net and not isinstance(net["params"], dict):
                c.err(f"{p}.network.params", "must be an object")
        c.typed(pt, "transport_intended", p, "str", choices=TRANSPORTS)
        placement = pt.get("placement")
        if placement is not None:
            if not isinstance(placement, dict):
                c.err(f"{p}.placement", "must be an object or null")
            else:
                ordinal = c.typed(placement, "ordinal", f"{p}.placement", "num", required=False, nullable=True)
                if ordinal is not None and (int(ordinal) != ordinal or ordinal < 0):
                    c.err(f"{p}.placement.ordinal", "must be a non-negative integer")
                c.typed(placement, "node", f"{p}.placement", "str", required=False, nullable=True)
        stagger = c.typed(pt, "stagger_ms", p, "num", required=False, nullable=True)
        if stagger is not None and stagger < 0:
            c.err(f"{p}.stagger_ms", "must be >= 0")
        c.typed(pt, "join_ts", p, "num")
        c.typed(pt, "leave_ts", p, "num", required=False, nullable=True)
        steps = c.typed(pt, "steps", p, "list")
        for s in steps or []:
            if s not in step_ids:
                c.err(f"{p}.steps", f"unknown step_id {s!r}")


def require_valid(m):
    errors = validate_manifest(m)
    if errors:
        raise ManifestError(errors)
    return m


def synthesize_real_meeting_manifest(meeting_id, start, end, user_ids):
    """Minimal manifest for a real meeting (section 5.2): one step, everyone a human observer."""
    return {
        "schema": SCHEMA,
        "run_id": "real-meeting",
        "meeting_id": meeting_id,
        "environment": "real-meeting",
        "code": {"commit": "unknown", "images": {}},
        "scenario": {"file": "none", "sha256": "none"},
        "clock": {"sync": "unknown", "max_abs_skew_ms": None, "measured_at": None},
        "steps": [{"step_id": "window", "n_target": max(1, len(user_ids)), "join_start": start,
                   "hold_start": start, "hold_end": end, "headline": True}],
        "events": [],
        "participants": [
            {"user_id": u, "fleet": "human", "role": "participant", "observer": True, "talker": True,
             "publishes": {"camera": True, "mic": True, "screen": False},
             "network": {"profile": "none", "shaped": False, "direction": "none", "shaper": "none"},
             "transport_intended": "auto", "join_ts": start, "leave_ts": None, "steps": ["window"]}
            for u in sorted(user_ids)
        ],
    }
