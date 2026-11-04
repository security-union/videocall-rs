"""Per-participant, per-kind rates over the hold window from decoded relay WS messages."""

import collections
import csv
import gzip
import json
import math
import re

KINDS = ("video_L0", "video_L1", "video_L2", "audio", "heartbeat", "HEALTH", "KEYFRAME_REQUEST", "LAYER_PREFERENCE",
         "VIEWPORT", "RTT", "DIAGNOSTICS", "other")
MEDIA_KINDS = {"AUDIO": "audio", "HEARTBEAT": "heartbeat", "RTT": "RTT", "KEYFRAME_REQUEST": "KEYFRAME_REQUEST"}
WRAPPER_KINDS = ("HEALTH", "VIEWPORT", "LAYER_PREFERENCE", "DIAGNOSTICS")
VIDEO_LAYERS = 3
MIN_COVERAGE = 0.95
MIN_LAYER_PPS = 1.0
MAX_UNIDENTIFIED_SHARE = 0.001
CLASSES = ("talker", "camera", "receive")
ROOM_SERIES = ("relay_room_bytes_total", "relay_keyframe_requests_total", "relay_nats_in_messages_total",
               "relay_nats_out_messages_total")


def kind_of(row):
    if row["op"] != "binary":
        return "other"
    pt = row["ptype"]
    if pt == "MEDIA":
        mt = row["mtype"]
        if mt == "VIDEO":
            layer = int(row["layer"] or 0)
            return f"video_L{layer}" if 0 <= layer < VIDEO_LAYERS else "other"
        return MEDIA_KINDS.get(mt, "other")
    return pt if pt in WRAPPER_KINDS else "other"


def class_of(participant):
    if participant["talker"]:
        return "talker"
    return "camera" if participant["publishes"]["camera"] else "receive"


def local_part(user):
    return user.split("@", 1)[0]


def map_connections(conns, participants):
    """({conn id: participant name}, {conn id: user}) by each connection's majority uplink user id."""
    by_user = {local_part(p["user_id"]): p["name"] for p in participants}
    ours, foreign = {}, {}
    for c in conns:
        if c.get("not_ws") or not c.get("users"):
            continue
        user = local_part(max(c["users"], key=c["users"].get))
        if user in by_user:
            ours[c["id"]] = by_user[user]
        else:
            foreign[c["id"]] = user
    return ours, foreign


def percentile(values, q):
    """Nearest-rank percentile of a non-empty list."""
    s = sorted(values)
    return s[max(0, math.ceil(q * len(s)) - 1)]


def _rates(acc, dur):
    return {k: {"pps": n / dur, "Bps": b / dur} for k, (b, n) in sorted(acc.items())}


def geometry(src, key_dims):
    """WxH from the video metadata's source size; else the top video layer's keyframe size (never upscaled)."""
    if src:
        return src.most_common(1)[0][0]
    for layer in sorted(key_dims, reverse=True):
        if key_dims[layer]:
            return key_dims[layer].most_common(1)[0][0]
    return None


def measure(rows, conns, participants, window, expected_layers):
    """Measures for every planned participant from message rows (dicts with the ws_capture columns)."""
    t0, t1 = window
    dur = t1 - t0
    if dur <= 0:
        raise ValueError(f"empty hold window {window}")
    conn_name, foreign = map_connections(conns, participants)
    up = collections.defaultdict(lambda: collections.defaultdict(lambda: [0, 0]))
    down = collections.defaultdict(lambda: collections.defaultdict(lambda: [0, 0]))
    keys = collections.defaultdict(lambda: collections.defaultdict(list))
    src = collections.defaultdict(collections.Counter)
    dims = collections.defaultdict(lambda: collections.defaultdict(collections.Counter))
    seconds = collections.defaultdict(set)
    foreign_seen, unmapped, unidentified, in_hold = set(), 0, 0, 0
    for r in rows:
        t = float(r["t"])
        if not t0 <= t < t1:
            continue
        in_hold += 1
        name = conn_name.get(int(r["conn"]))
        if name is None:
            unmapped += 1
            if int(r["conn"]) in foreign:
                foreign_seen.add(foreign[int(r["conn"])])
            else:
                unidentified += 1
            continue
        k, nb = kind_of(r), int(r["ws_bytes"])
        acc = (up if r["dir"] == "u" else down)[name][k]
        acc[0] += nb
        acc[1] += 1
        seconds[name].add(int(t))
        if r["dir"] == "u" and k.startswith("video_L"):
            if r.get("src_w"):
                src[name][f"{r['src_w']}x{r['src_h']}"] += 1
            if r["ftype"] == "key":
                keys[name][int(k[-1])].append(nb)
                if r.get("w") not in (None, "", "0"):
                    dims[name][int(k[-1])][f"{r['w']}x{r['h']}"] += 1
    out, problems = {}, []
    for p in participants:
        name = p["name"]
        u, d = _rates(up[name], dur), _rates(down[name], dur)
        layers = sorted(int(k[-1]) for k, v in u.items() if k.startswith("video_L") and v["pps"] >= MIN_LAYER_PPS)
        coverage = min(1.0, len(seconds[name]) / max(1, round(dur)))
        out[name] = {
            "fleet": p["fleet"], "role": p["role"], "class": class_of(p), "coverage": round(coverage, 4),
            "up": u, "down": d,
            "up_total": {"pps": sum(v["pps"] for v in u.values()), "Bps": sum(v["Bps"] for v in u.values())},
            "down_total": {"pps": sum(v["pps"] for v in d.values()), "Bps": sum(v["Bps"] for v in d.values())},
            "video_layers": layers, "geometry": geometry(src[name], dims[name]),
            "keyframes": {str(layer): {"per_s": len(s) / dur, "sizes": s} for layer, s in sorted(keys[name].items())},
        }
        if coverage < MIN_COVERAGE:
            problems.append(f"{name}: traffic in {coverage:.0%} of the hold's seconds (needs {MIN_COVERAGE:.0%}); it "
                            "joined late, left or was never captured")
        want = expected_layers.get(name)
        if want and layers != list(range(want)):
            problems.append(f"{name}: published video layers {layers} at >= {MIN_LAYER_PPS:g} pkt/s, expected "
                            f"{list(range(want))}")
    if foreign_seen:
        problems.append(f"{len(foreign_seen)} user(s) outside this run used the relay during the hold")
    if unidentified > MAX_UNIDENTIFIED_SHARE * in_hold:
        problems.append(f"{unidentified} of the hold's {in_hold} messages ran on connections with no decoded user id: "
                        f"over {MAX_UNIDENTIFIED_SHARE:.1%}, so traffic outside the run is not ruled out")
    return {"window": [t0, t1], "duration_s": dur, "participants": out, "unmapped_messages_in_window": unmapped,
            "unidentified_messages_in_window": unidentified, "problems": problems}


def read_rows(path):
    with gzip.open(path, "rt", newline="") as fh:
        yield from csv.DictReader(fh)


def measure_files(messages_csv, conns_json, participants, window, expected_layers):
    with open(conns_json, encoding="utf-8") as fh:
        conns = json.load(fh)
    return measure(read_rows(messages_csv), conns, participants, window, expected_layers)


SAMPLE_RE = re.compile(r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{[^}]*\})?\s+(\S+)$")
METRIC_NAME_RE = re.compile(r"[a-zA-Z_:][a-zA-Z0-9_:]*")
LABEL_RE = re.compile(r'\s*([a-zA-Z_]\w*)="((?:[^"\\]|\\.)*)"\s*,?')
SAMPLE_TAIL_RE = re.compile(r"(?:\s+[^\s{}]+){0,2}\s*")


def parse_metrics(lines):
    out = {}
    for ln in lines:
        m = SAMPLE_RE.match(ln.strip())
        if m:
            try:
                out[m.group(1) + (m.group(2) or "")] = float(m.group(3))
            except ValueError:
                continue
    return out


def in_room(series, room):
    """Whether a sample line or series key has no room label or exactly this one; malformed labels are False."""
    name = METRIC_NAME_RE.match(series)
    if name is None:
        return False
    pos = name.end()
    if series.startswith("{", pos):
        pos += 1
        while not series.startswith("}", pos):
            label = LABEL_RE.match(series, pos)
            if label is None or (label.group(1) == "room" and label.group(2) != room):
                return False
            pos = label.end()
        pos += 1
    return SAMPLE_TAIL_RE.fullmatch(series, pos) is not None


def room_lines(lines, room):
    """The relay_* lines of this room and of no other room."""
    return [ln for ln in lines if ln.startswith("relay_") and in_room(ln, room)]


def relay_deltas(samples, room, window):
    """Per-second rates of the room-level relay series over the window, from (t, {series: value}) samples."""
    t0, t1 = window
    usable = [(t, v) for t, v in samples if v]
    if len(usable) < 2:
        return None
    a = min(usable, key=lambda s: abs(s[0] - t0))
    b = min(usable, key=lambda s: abs(s[0] - t1))
    if b[0] - a[0] <= 0:
        return None
    out = {}
    for key, vb in sorted(b[1].items()):
        name = key.split("{", 1)[0]
        if name not in ROOM_SERIES or not in_room(key, room):
            continue
        out[key] = (vb - a[1].get(key, 0.0)) / (b[0] - a[0])
    return {"from": a[0], "to": b[0], "per_s": out}


def cadence_problems(measures, min_fps):
    """Browser publishers whose video layer N ran under min_fps[N] packets/s: a starved encoder, not a reference."""
    out = []
    for name, p in sorted(measures["participants"].items()):
        if p["fleet"] != "browser" or not p["video_layers"]:
            continue
        for layer, floor in enumerate(min_fps or ()):
            got = p["up"].get(f"video_L{layer}", {}).get("pps", 0.0)
            if got < floor:
                out.append(f"{name}: video layer {layer} at {got:.2f} packets/s, under the quiet baseline {floor:g} "
                           "(fidelity.browser_min_video_fps); the browser was starved")
    return out
