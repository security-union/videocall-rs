#!/usr/bin/env python3
"""Build a call-quality-run-manifest/v1 from a scenario run folder (#2914; runner design §2).

Run-folder inputs:
  scenario.resolved.json          plan: run_id, meeting_id, environment, scenario {file, sha256},
                                  steps [{step_id, n_target, headline}], node_aliases {node: alias},
                                  participants [{user_id, fleet, role, observer, talker, publishes, steps}]
  events.jsonl                    one object per line, each with a string "type" and a finite "wall" (epoch s);
                                  lines of other types are checked for that and otherwise ignored. Read here:
                                    preflight       commit, dirty, clock (copied to the manifest; its
                                                    max_abs_skew_ms widens the record clock tolerance)
                                    join_start | hold_start | hold_end    step_id
                                    event           action, participants (user ids), params,
                                                    t_issued, t_confirmed, result ("ok" when confirmed)
                                    stopped         one line per process exit, naming every participant the
                                                    process ran; wall is the exit time. Once every step mark
                                                    and teardown_start is present, every participant needs one.
                                    teardown_start  exactly one, at or after every hold_end
                                  The clock window ends at the latest teardown_start or stopped wall.
  rust/participants-*.json        Rust bot --participants-out files (with planned and media_started_at)
  probes/*/participants/*.json    bots-app participant records
  stack/images.json               ui, relay_ws, relay_wt, bots_app, rust_bot

Writes manifest.json (exit 0), or manifest.partial.json and missing.txt (exit 2); exit 3 on a collector or usage
error.
Stdlib only.
"""

import argparse
import contextlib
import glob
import json
import math
import os
import re
import sys
from dataclasses import dataclass, field
from datetime import datetime

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import cq_manifest  # noqa: E402
import cq_score  # noqa: E402

PLAN_FILE = "scenario.resolved.json"
EVENTS_FILE = "events.jsonl"
IMAGES_FILE = os.path.join("stack", "images.json")
RUST_GLOB = os.path.join("rust", "participants-*.json")
BROWSER_GLOB = os.path.join("probes", "*", "participants", "*.json")
RUST_KIND = "rust-bot-participants"
BROWSER_SCHEMA = "bots-app-participant-record/v0"
PARTIAL_SCHEMA = cq_manifest.SCHEMA + "-partial"
IMAGE_KEYS = ("ui", "relay_ws", "relay_wt", "bots_app", "rust_bot")
STEP_MARKS = ("join_start", "hold_start", "hold_end")
AT_CONFIRMED = ("unmute",)
CLOCK_TOLERANCE_S = 2.0
MAX_ABS_SKEW_MS = 30000
RESERVED_NODES = ("local", "ci")
RECORD_TIMES = ("join_ts", "leave_ts")
FILE_TIMES = ("started_at", "ended_at", "media_started_at")
NETEM_UNVERIFIED = re.compile(r"set by POST /netem at (\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d{3})?Z); "
                              r"the shaping params are not read back")
EXIT_OK, EXIT_PARTIAL, EXIT_ERROR = 0, 2, 3


class CollectError(Exception):
    def __init__(self, errors):
        super().__init__("collector error:\n  " + "\n  ".join(errors))
        self.errors = errors


@dataclass
class Collected:
    """`missing` non-empty means `manifest` is partial and must never be scored."""
    manifest: dict
    missing: list = field(default_factory=list)


@dataclass
class _Record:
    source: str
    part: dict
    unclean_exit: bool
    unverified: list
    outcome: object = None
    file_times: dict = field(default_factory=dict)
    fleet: str = "browser"


def _num(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool) and abs(v) <= sys.float_info.max


def _read_json(path):
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except (OSError, ValueError) as exc:
        raise CollectError([f"{path}: {exc}"]) from exc


def read_events(path):
    out = []
    try:
        with open(path, encoding="utf-8") as fh:
            lines = fh.readlines()
    except (OSError, UnicodeDecodeError) as exc:
        raise CollectError([f"{path}: {exc}"]) from exc
    for n, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            e = json.loads(line)
        except ValueError as exc:
            raise CollectError([f"{path}:{n}: {exc}"]) from exc
        if not isinstance(e, dict) or not isinstance(e.get("type"), str) or not _num(e.get("wall")):
            raise CollectError([f"{path}:{n}: each line needs a string 'type' and a finite numeric 'wall'"])
        out.append(e)
    return out


def _str_list(v):
    return isinstance(v, list) and all(isinstance(x, str) for x in v)


def _check_plan(plan):
    errors = []
    if not isinstance(plan, dict):
        return ["plan: must be an object"]
    for k in ("run_id", "meeting_id", "environment", "scenario", "steps", "participants"):
        if k not in plan:
            errors.append(f"plan: {k} missing")
    for k in ("steps", "participants"):
        if not isinstance(plan.get(k), list):
            errors.append(f"plan: {k} must be a list")
    aliases = plan.get("node_aliases", {})
    if not isinstance(aliases, dict) or not all(isinstance(v, str) for v in aliases.values()):
        errors.append("plan: node_aliases must map node names to alias strings")
    if errors:
        return errors
    seen = set()
    for i, p in enumerate(plan["participants"]):
        missing = [k for k in ("user_id", "fleet", "role", "observer", "talker", "publishes", "steps")
                   if not isinstance(p, dict) or k not in p]
        if missing:
            errors.append(f"plan participants[{i}]: missing {', '.join(missing)}")
        elif not isinstance(p["user_id"], str) or not _str_list(p["steps"]):
            errors.append(f"plan participants[{i}]: user_id must be a string and steps a list of step ids")
        elif p["user_id"] in seen:
            errors.append(f"plan: user_id {p['user_id']!r} planned twice")
        else:
            seen.add(p["user_id"])
    for i, s in enumerate(plan["steps"]):
        if not isinstance(s, dict) or any(k not in s for k in ("step_id", "n_target", "headline")) \
                or not isinstance(s["step_id"], str):
            errors.append(f"plan steps[{i}]: needs a string step_id, n_target and headline")
    return errors


def _load_records(run_dir, meeting_id, errors):
    recs = []
    for path in sorted(glob.glob(os.path.join(run_dir, RUST_GLOB))):
        f = _read_json(path)
        if not isinstance(f, dict) or f.get("kind") != RUST_KIND or not isinstance(f.get("participants"), list):
            errors.append(f"{path}: not a {RUST_KIND} file")
            continue
        if f.get("meeting_id") != meeting_id:
            errors.append(f"{path}: meeting_id {f.get('meeting_id')!r} is not this run's {meeting_id!r}")
            continue
        planned, n = f.get("planned"), len(f["participants"])
        if not isinstance(planned, int) or isinstance(planned, bool) or planned != n:
            errors.append(f"{path}: planned {planned!r} but {n} participant records")
        times = {k: f[k] for k in FILE_TIMES if f.get(k) is not None}
        recs += [_Record(path, p, f.get("ended_at") is None, [], p.get("outcome") if isinstance(p, dict) else None,
                         times, "rust") for p in f["participants"]]
    for path in sorted(glob.glob(os.path.join(run_dir, BROWSER_GLOB))):
        r = _read_json(path)
        if not isinstance(r, dict) or r.get("schema") != BROWSER_SCHEMA or not isinstance(r.get("participant"), dict):
            errors.append(f"{path}: not a {BROWSER_SCHEMA} record")
            continue
        unverified = r.get("unverified", [])
        if not isinstance(unverified, list):
            errors.append(f"{path}: unverified must be a list")
            continue
        recs.append(_Record(path, r["participant"], False, unverified, r.get("outcome")))
    return recs


def _match(recs, planned, errors):
    by_uid = {}
    for rec in recs:
        uid = rec.part.get("user_id") if isinstance(rec.part, dict) else None
        if not isinstance(uid, str) or not uid:
            errors.append(f"{rec.source}: user_id is null; the session cannot be identified (G-V8)")
        elif uid in by_uid:
            errors.append(f"{uid}: two records, {by_uid[uid].source} and {rec.source} (G-V8)")
        elif uid not in planned:
            errors.append(f"{uid}: record {rec.source} is not in the plan")
        else:
            by_uid[uid] = rec
    return by_uid


def _step_windows(plan_steps, events, missing, errors):
    marks = {}
    for e in events:
        if e["type"] in STEP_MARKS:
            if not isinstance(e.get("step_id"), str):
                errors.append(f"events: {e['type']} needs a string step_id")
                continue
            key = (e["step_id"], e["type"])
            if key in marks:
                errors.append(f"events: {e['type']} for step {key[0]!r} recorded twice")
            marks[key] = e["wall"]
    ids = {s["step_id"] for s in plan_steps}
    for sid, kind in sorted(k for k in marks if k[0] not in ids):
        errors.append(f"events: {kind} names unknown step {sid!r}")
    steps = []
    for s in plan_steps:
        row = {"step_id": s["step_id"], "n_target": s["n_target"]}
        for kind in STEP_MARKS:
            if (s["step_id"], kind) in marks:
                row[kind] = marks[(s["step_id"], kind)]
            else:
                missing.append(f"step {s['step_id']}: no {kind} in {EVENTS_FILE}")
        row["headline"] = s["headline"]
        steps.append(row)
    return steps


def _events(events, steps, planned, errors):
    """An event that was not confirmed did not happen as planned, so the run did not follow its scenario."""
    complete = all(k in s for s in steps for k in STEP_MARKS)
    out = []
    for e in (e for e in events if e["type"] == "event"):
        action, parts = e.get("action"), e.get("participants")
        if action not in cq_manifest.EVENT_ACTIONS:
            errors.append(f"events: unknown action {action!r}")
            continue
        if not isinstance(parts, list) or not parts or not all(isinstance(u, str) for u in parts):
            errors.append(f"events: {action} needs a non-empty list of user ids")
            continue
        unknown = [u for u in parts if u not in planned]
        if unknown:
            errors.append(f"events: {action} names {', '.join(unknown)}, not in the plan")
            continue
        if e.get("result") != "ok" or not _num(e.get("t_confirmed")):
            errors.append(f"events: {action} for {', '.join(parts)} was not confirmed (result {e.get('result')!r})")
            continue
        t_issued, t_confirmed = e.get("t_issued"), e["t_confirmed"]
        if not _num(t_issued) or t_issued > t_confirmed:
            errors.append(f"events: {action} for {', '.join(parts)} needs a numeric t_issued no later than t_confirmed")
            continue
        at = t_confirmed if action in AT_CONFIRMED else t_issued
        sid = next((s["step_id"] for s in steps if complete and s["join_start"] <= at <= s["hold_end"]), None)
        if sid is None:
            if complete:
                errors.append(f"events: {action} at {at} for {', '.join(parts)} is outside every step window")
            continue
        out.append({"at": at, "step_id": sid, "action": action, "participants": list(parts),
                    "params": e.get("params", {})})
    return sorted(out, key=lambda e: e["at"])


def _first_stops(events, planned, errors):
    stops = {}
    for e in (e for e in events if e["type"] == "stopped"):
        parts = e.get("participants")
        if not isinstance(parts, list) or not all(isinstance(u, str) for u in parts):
            errors.append("events: stopped needs a list of user ids")
            continue
        unknown = [u for u in parts if u not in planned]
        if unknown:
            errors.append(f"events: stopped names {', '.join(unknown)}, not in the plan")
        for u in parts:
            stops[u] = min(stops.get(u, math.inf), e["wall"])
    return stops


def _check_clocks(recs, window, errors):
    """Every record and file timestamp must lie in [first join_start, last teardown_start or stopped] +- tolerance."""
    lo, hi = window
    files = {}
    for rec in recs:
        for key in RECORD_TIMES:
            v = rec.part.get(key)
            if _num(v) and not lo <= v <= hi:
                errors.append(f"{rec.part['user_id']}: {key} {v} in {rec.source} is outside the run window "
                              f"[{lo:.3f}, {hi:.3f}]: a skewed clock?")
        files.setdefault(rec.source, (rec.file_times, []))[1].append(rec.part["user_id"])
    for source, (times, uids) in files.items():
        for key, v in times.items():
            if not _num(v):
                errors.append(f"{source}: {key} {v!r} is not a finite number")
            elif not lo <= v <= hi:
                errors.append(f"{source}: {key} {v} is outside the run window [{lo:.3f}, {hi:.3f}]: a skewed "
                              f"clock? (participants {', '.join(uids)})")


def _final_leave(events, uid):
    """`at` of the participant's last confirmed leave with no later rejoin, else None."""
    final = None
    for e in events:
        if uid in e["participants"] and e["action"] in ("leave", "rejoin"):
            final = e["at"] if e["action"] == "leave" else None
    return final


def _departure(uid, rec, stop, teardown, final_leave, cover_s, errors):
    """D35: an exit before teardown_start needs a confirmed final leave issued at most one scrape after it."""
    if stop is None:
        errors.append(f"{uid}: no stopped line names it, so when its process exited is unknown")
        return
    leave = rec.part.get("leave_ts")
    gone = stop if leave is None else min(leave, stop)
    if gone < teardown and (final_leave is None or final_leave > gone + cover_s):
        errors.append(f"{uid}: left at {gone:.3f} ({rec.source}), before teardown_start, with no confirmed final "
                      "leave: an unplanned exit, which D35 counts as a drop")


def _netem_windows(events, uid):
    return [(e["t_issued"], e["t_confirmed"]) for e in events
            if e["type"] == "event" and e.get("action") == "netem" and e.get("result") == "ok"
            and isinstance(e.get("participants"), list) and uid in e["participants"]
            and _num(e.get("t_issued")) and _num(e.get("t_confirmed"))]


def _netem_excused(reason, windows, tol):
    """The netem reason, stamped inside [t_issued, t_confirmed] +- tol of a confirmed netem naming this bot."""
    m = NETEM_UNVERIFIED.fullmatch(str(reason))
    if m is None:
        return False
    try:
        t = datetime.fromisoformat(m.group(1).replace("Z", "+00:00")).timestamp()
    except ValueError:
        return False
    return any(lo - tol <= t <= hi + tol for lo, hi in windows)


def _participant(planned, rec, aliases, died, declared, netem, errors):
    uid, p = planned["user_id"], rec.part
    for k in ("fleet", "observer", "talker"):
        if p.get(k) != planned[k]:
            errors.append(f"{uid}: record {k} {p.get(k)!r} differs from the plan's {planned[k]!r}")
    if p.get("fleet") != rec.fleet:
        errors.append(f"{uid}: record fleet {p.get('fleet')!r} but {rec.source} is a {rec.fleet} record")
    if p.get("fleet") == "browser" and p.get("role") not in (None, planned["role"]):
        errors.append(f"{uid}: record role {p.get('role')!r} differs from the plan's {planned['role']!r}")
    if p.get("publishes") != planned["publishes"]:
        errors.append(f"{uid}: observed publishes {p.get('publishes')} differ from the plan's {planned['publishes']}")
    for u in rec.unverified:
        if isinstance(u, dict) and str(u.get("field", "")).startswith("publishes"):
            errors.append(f"{uid}: {u['field']} is unverified: {u.get('reason')}")
    net = p.get("network")
    why = [u.get("reason") for u in rec.unverified if isinstance(u, dict) and u.get("field") == "network"]
    unexcused = [w for w in why if not _netem_excused(w, *netem)]
    if not isinstance(net, dict) or net.get("profile") == "unknown" or not isinstance(net.get("shaped"), bool):
        errors.append(f"{uid}: applied network is unknown" + (f": {why[0]}" if why else ""))
    elif unexcused:
        errors.append(f"{uid}: applied network is unverified: {unexcused[0]}")
    if "rejoin" not in declared and any(isinstance(u, dict) and u.get("field") == "rejoin" for u in rec.unverified):
        errors.append(f"{uid}: {rec.source} records a rejoin that no confirmed rejoin event declares")
    if rec.outcome is not None and p.get("leave_ts") is None:
        errors.append(f"{uid}: outcome {rec.outcome!r} in {rec.source} but no leave_ts")
    out = {"user_id": uid, "fleet": planned["fleet"], "role": planned["role"], "observer": planned["observer"],
           "talker": planned["talker"], "publishes": p.get("publishes"), "network": net,
           "transport_intended": p.get("transport_intended")}
    if "placement" in p:
        placement = p["placement"]
        if isinstance(placement, dict) and placement.get("node") is not None:
            node = placement["node"]
            alias = aliases.get(node) if isinstance(node, str) else None
            if not isinstance(node, str):
                errors.append(f"{uid}: placement.node must be a string or null")
            elif alias is None:
                errors.append(f"{uid}: placement.node has no alias in the plan's node_aliases")
            placement = dict(placement, node=alias)
        out["placement"] = placement
    if "stagger_ms" in p:
        out["stagger_ms"] = p["stagger_ms"]
    out["join_ts"] = p["join_ts"]
    if p.get("leave_ts") is not None or not died:
        out["leave_ts"] = p.get("leave_ts")
    out["steps"] = list(planned["steps"])
    return out


def _talkers_unmuted(participants, steps, events, errors):
    """D34: a declared talker's mic is on strictly before hold_start (join state, then mute/unmute events)."""
    for s in (s for s in steps if "hold_start" in s):
        for p in (p for p in participants if p["talker"] and s["step_id"] in p["steps"]):
            on = isinstance(p["publishes"], dict) and p["publishes"].get("mic") is True
            for e in (e for e in events if e["action"] in ("mute", "unmute") and p["user_id"] in e["participants"]):
                if e["at"] < s["hold_start"]:
                    on = e["action"] == "unmute"
            if not on:
                errors.append(f"{p['user_id']}: declared talker is not unmuted strictly before hold_start of step "
                              f"{s['step_id']} (D34)")


def _rust_talkers_media(parts, by_uid, steps, errors):
    """D34 for Rust talkers: the file's media_started_at is strictly before each hold_start."""
    for p in (p for p in parts if p["talker"] and p["fleet"] == "rust"):
        rec = by_uid[p["user_id"]]
        media = rec.file_times.get("media_started_at")
        for s in (s for s in steps if "hold_start" in s and s["step_id"] in p["steps"]):
            if not _num(media) or media >= s["hold_start"]:
                errors.append(f"{p['user_id']}: media_started_at {media} in {rec.source} is not strictly before "
                              f"hold_start of step {s['step_id']} (D34)")


def _code_and_clock(run_dir, events, errors):
    pre = [e for e in events if e["type"] == "preflight"]
    if len(pre) != 1:
        errors.append(f"events: expected one preflight line, found {len(pre)}")
        return {"commit": None, "images": {}}, None
    commit, dirty, clock = pre[0].get("commit"), pre[0].get("dirty"), pre[0].get("clock")
    if not isinstance(commit, str) or not commit or not isinstance(dirty, bool):
        errors.append("events: preflight needs a commit string and a boolean dirty")
    elif dirty:
        commit += "-dirty"
    if not isinstance(clock, dict):
        errors.append("events: preflight needs a clock object")
    images = _read_json(os.path.join(run_dir, IMAGES_FILE))
    if not isinstance(images, dict):
        errors.append(f"{IMAGES_FILE}: must be an object")
        images = {}
    absent = [k for k in IMAGE_KEYS if not isinstance(images.get(k), str) or not images[k]]
    if absent:
        errors.append(f"{IMAGES_FILE}: no image id for {', '.join(absent)}")
    return {"commit": commit, "images": {k: images[k] for k in IMAGE_KEYS if k in images}}, clock


def _aliased_clock(clock, aliases, reserved, errors):
    """per_node carries aliases only: a raw node is mapped; an alias or a reserved name is kept; else an error."""
    if not isinstance(clock, dict) or "per_node" not in clock:
        return clock
    per_node = clock["per_node"]
    if not isinstance(per_node, list) or not all(isinstance(n, dict) and isinstance(n.get("node"), str)
                                                 for n in per_node):
        errors.append("events: preflight clock.per_node must be a list of objects with a string node")
        return clock
    out = []
    for n in per_node:
        if n["node"] in aliases:
            n = dict(n, node=aliases[n["node"]])
        elif n["node"] not in aliases.values() and n["node"] not in reserved:
            errors.append("events: a preflight clock.per_node node has no alias in the plan's node_aliases")
        out.append(n)
    return dict(clock, per_node=out)


def collect(run_dir):
    """Returns a validated manifest, or a partial one with `missing`; raises CollectError on a contradiction."""
    plan = _read_json(os.path.join(run_dir, PLAN_FILE))
    plan_errors = _check_plan(plan)
    if plan_errors:
        raise CollectError(plan_errors)
    events = read_events(os.path.join(run_dir, EVENTS_FILE))
    errors, missing = [], []
    planned = {p["user_id"]: p for p in plan["participants"]}
    code, clock = _code_and_clock(run_dir, events, errors)
    steps = _step_windows(plan["steps"], events, missing, errors)
    teardowns = [e["wall"] for e in events if e["type"] == "teardown_start"]
    if not teardowns:
        missing.append(f"no teardown_start in {EVENTS_FILE}")
    elif len(teardowns) > 1:
        errors.append(f"events: {len(teardowns)} teardown_start lines; exactly one is allowed")
    teardown = teardowns[0] if len(teardowns) == 1 else None
    for st in (st for st in steps if teardown is not None and st.get("hold_end", -math.inf) > teardown):
        errors.append(f"events: teardown_start {teardown:.3f} precedes hold_end {st['hold_end']:.3f} of step "
                      f"{st['step_id']}")
    by_uid = _match(_load_records(run_dir, plan["meeting_id"], errors), planned, errors)
    stops = _first_stops(events, planned, errors)
    aliases = plan.get("node_aliases") or {}
    clock = _aliased_clock(clock, aliases, RESERVED_NODES + (plan["environment"],), errors)
    skew = clock.get("max_abs_skew_ms") if isinstance(clock, dict) else None
    if _num(skew) and abs(skew) > MAX_ABS_SKEW_MS:
        errors.append(f"events: preflight max_abs_skew_ms {skew} exceeds {MAX_ABS_SKEW_MS}, the 30 s stale-packet "
                      "guard")
    tol = CLOCK_TOLERANCE_S + (abs(skew) / 1000 if _num(skew) else 0)
    joins = [e["wall"] for e in events if e["type"] == "join_start"]
    ends = [e["wall"] for e in events if e["type"] in ("teardown_start", "stopped")]
    if joins and ends:
        _check_clocks(by_uid.values(), (min(joins) - tol, max(ends) + tol), errors)
    manifest_events = _events(events, steps, planned, errors)
    timeline = teardown is not None and all(k in s for s in steps for k in STEP_MARKS)
    cover_s = cq_score.cv(cq_score.load_config(), "sampling", "scrape_step_s")
    parts = []
    for uid, pl in planned.items():
        rec = by_uid.get(uid)
        bad = [k for k in RECORD_TIMES if rec is not None and rec.part.get(k) is not None
               and not _num(rec.part.get(k))]
        if rec is None:
            missing.append(f"{uid}: no participant record")
        elif bad:
            errors.append(f"{uid}: {', '.join(bad)} in {rec.source} must be a finite number or null")
        elif not _num(rec.part.get("join_ts")):
            missing.append(f"{uid}: join_ts is null in {rec.source}")
        else:
            stop = stops.get(uid)
            if timeline:
                _departure(uid, rec, stop, teardown, _final_leave(manifest_events, uid), cover_s, errors)
            died = rec.unclean_exit or (stop is not None and stop < (math.inf if teardown is None else teardown))
            declared = {e["action"] for e in manifest_events if uid in e["participants"]}
            parts.append(_participant(pl, rec, aliases, died, declared, (_netem_windows(events, uid), tol), errors))
    _talkers_unmuted(parts, steps, manifest_events, errors)
    _rust_talkers_media(parts, by_uid, steps, errors)
    manifest = {"schema": cq_manifest.SCHEMA, "run_id": plan["run_id"], "meeting_id": plan["meeting_id"],
                "environment": plan["environment"], "code": code, "scenario": plan["scenario"], "clock": clock,
                "steps": steps, "events": manifest_events, "participants": parts}
    errors += [f"{path}: NaN and infinity are not numbers here" for path in cq_manifest.non_finite_paths(manifest)]
    if errors:
        raise CollectError(errors + [f"missing: {m}" for m in missing])
    if missing:
        return Collected(dict(manifest, schema=PARTIAL_SCHEMA, missing=missing), missing)
    invalid = cq_manifest.validate_manifest(manifest)
    if invalid:
        raise CollectError(invalid)
    return Collected(manifest)


def _write(path, text):
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        fh.write(text)
    os.replace(tmp, path)


def clear_outputs(out_dir):
    for name in ("manifest.json", "manifest.partial.json", "missing.txt"):
        with contextlib.suppress(FileNotFoundError):
            os.remove(os.path.join(out_dir, name))


def write_outputs(result, out_dir):
    body = json.dumps(result.manifest, indent=1, allow_nan=False) + "\n"
    if result.missing:
        _write(os.path.join(out_dir, "manifest.partial.json"), body)
        _write(os.path.join(out_dir, "missing.txt"), "".join(m + "\n" for m in result.missing))
        return EXIT_PARTIAL
    _write(os.path.join(out_dir, "manifest.json"), body)
    return EXIT_OK


class _Parser(argparse.ArgumentParser):
    def error(self, message):
        self.print_usage(sys.stderr)
        self.exit(EXIT_ERROR, f"{self.prog}: error: {message}\n")


def main(argv=None):
    ap = _Parser(description=__doc__.splitlines()[0], allow_abbrev=False)
    ap.add_argument("--run-dir", required=True)
    ap.add_argument("--out-dir", help="an existing folder; default: the run folder")
    args = ap.parse_args(argv)
    if args.out_dir is not None and not os.path.isdir(args.out_dir):
        ap.error(f"--out-dir {args.out_dir} is not a directory")
    out_dir = args.out_dir or args.run_dir
    clear_outputs(out_dir)
    try:
        result = collect(args.run_dir)
    except CollectError as exc:
        print(exc, file=sys.stderr)
        return EXIT_ERROR
    code = write_outputs(result, out_dir)
    for m in result.missing:
        print(f"missing: {m}", file=sys.stderr)
    return code


if __name__ == "__main__":
    sys.exit(main())
