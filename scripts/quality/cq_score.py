"""Call quality scoring (docs/quality-at-scale/CALL_QUALITY_SCORING.md sections 2, 3, 9)."""

import bisect
import copy
import json
import math
import os
from collections import defaultdict

from cq_manifest import non_finite_paths
from cq_prom import any_of_regex, counter_delta, merge_max, selector

DEFAULT_CONFIG_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "call_quality_config.json")
DIMENSIONS = ("A", "V", "L", "Q", "S")
QUALITY_GATE_DIMS = (("G-Q3", "A"), ("G-Q4", "V"), ("G-Q5", "L"))
LATENCY_PENDING = "latency not gated: #2948 pending"

M_EXPAND = "videocall_neteq_expand_ops_per_sec"
M_PPS = "videocall_neteq_packets_per_sec"
M_FREEZE = "videocall_video_freeze_seconds_total"
M_STALE = "videocall_video_content_staleness_ms"
M_FPS = "videocall_video_fps"
M_CAN_LISTEN = "videocall_peer_can_listen"
M_SENT = "videocall_client_packets_sent_per_sec"
M_PEER_INFO = "videocall_peer_info"
M_RTT = "videocall_client_active_server_rtt_ms"
M_REELECT = "videocall_client_reelection_total"
M_SERVER_CONN = "videocall_server_connections_active"
M_HEALTH = "videocall_health_reports_total"
M_ACTIVE_LAYERS = "videocall_encoder_active_layers"
PAIR_METRICS = (M_EXPAND, M_PPS, M_FREEZE, M_STALE, M_FPS, M_CAN_LISTEN)
DIM_SOURCES = {"A": (M_PPS, M_EXPAND), "V": (M_FREEZE, M_FPS), "L": ("latency_gate.audio_delay_metric",), "Q": (M_FPS,),
               "S": (M_SENT,)}
SYNCED_CLOCKS = ("chrony", "ntp")
RELAY_PATHS = {
    "relay_layer_filtered_total": [],
    "relay_viewport_filtered_total": [],
    "relay_layer_preference_updates_total": [("outcome", "=", "accepted")],
    "relay_keyframe_requests_total": [],
}
WAIVABLE_GATES = {"G-V2", "G-V3", "G-V4", "G-V5"}
DEPLOYED_PEER_STATS_CAP = 128
CANVAS_LIMIT = 30
METRICS_SESSION_TIMEOUT_S = 30
UNHONOURED_EVENTS = ("camera-off", "camera-on", "outage")
COVERAGE_AMBIGUOUS = "coverage: can't distinguish loss from peer_stats cap"
PIPELINE_GAP_MAX_SHARE = 0.05
RECONNECT_PENDING = "proposal, team decision pending (#2913)"
PIPELINE_GAP_MAX_RUN = 1
CONFIG_BOUNDS = {("sampling", "scrape_step_s"): (15, 15),
                 ("validity_gates", "health_reports_min_ratio"): (0.95, 1.0),
                 ("validity_gates", "health_reports_max_ratio"): (1.0, 1.2),
                 ("validity_gates", "n_min_observers"): (10, None),
                 ("validity_gates", "browser_health_interval_s"): (5, 5),
                 ("quality_gates", "drop_gap_s"): (0, 30),
                 ("quality_gates", "rejoin_grace_s"): (0, 20),
                 ("quality_gates", "join_deadline_s"): (30, 30),
                 ("quality_gates", "k_red_fail"): (1, 2),
                 ("quality_gates", "split_rate_red"): (0, 0.02),
                 ("quality_gates", "per_talker_A_red"): (0, 0.05),
                 ("dimensions", "expand_ops_full_scale"): (1, 100),
                 ("dimensions", "nominal_talker_pps"): (50, 50)}
MAX_RELAY_EXEMPTIONS = 2
RUST_HEALTH_INTERVALS_S = (1, 5)
MIN_REQUIRED_DIMENSIONS = {"A", "V", "Q", "S"}


class ConfigError(Exception):
    pass
SERVER_DIAG = {
    "sched_lag_le50": ("videocall_relay_scheduler_lag_ms_bucket", [("le", "=~", "50(\\.0)?")]),
    "sched_lag_count": ("videocall_relay_scheduler_lag_ms_count", []),
    "mailbox_drops": ("relay_inbound_mailbox_drops_total", []),
    "shed_escalations_stage_two": ("relay_downlink_shed_escalations_total", [("stage", "=", "two")]),
    "wt_session_closes": ("relay_wt_session_closes_total", []),
    "overflow_cap": ("relay_downlink_stream_overflow_frames_total", [("cause", "=", "cap")]),
}
DIAGNOSTIC_SERIES = set(SERVER_DIAG) | {"relay_session_drops_room", M_RTT, M_ACTIVE_LAYERS}


# ---------------------------------------------------------------- config

def deep_merge(base, override):
    out = copy.deepcopy(base)
    for k, v in override.items():
        if isinstance(v, dict) and isinstance(out.get(k), dict):
            out[k] = deep_merge(out[k], v)
        else:
            out[k] = copy.deepcopy(v)
    return out


def load_config(override_path=None):
    with open(DEFAULT_CONFIG_PATH, encoding="utf-8") as fh:
        cfg = json.load(fh)
    if override_path:
        with open(override_path, encoding="utf-8") as fh:
            cfg = deep_merge(cfg, json.load(fh))
    validate_config(cfg)
    return cfg


def validate_config(cfg):
    """Reject overrides that would let missing data pass."""
    errors = [f"{p}: NaN and infinity are not numbers here" for p in non_finite_paths(cfg)]
    extra = set(cv(cfg, "validity_gates", "allowed_not_measured")) - WAIVABLE_GATES
    if extra:
        errors.append(f"allowed_not_measured may only hold {sorted(WAIVABLE_GATES)}; G-Q5 is waived only through "
                      f"latency_gate (got {sorted(extra)})")
    dropped = MIN_REQUIRED_DIMENSIONS - set(cv(cfg, "validity_gates", "required_dimensions"))
    if dropped:
        errors.append(f"required_dimensions must keep {sorted(MIN_REQUIRED_DIMENSIONS)} (missing {sorted(dropped)})")
    exemptions = cv(cfg, "validity_gates", "relay_path_exemptions")
    for (group, key), (lo, hi) in CONFIG_BOUNDS.items():
        value = cv(cfg, group, key)
        if (not isinstance(value, (int, float)) or isinstance(value, bool) or not math.isfinite(value) or value < lo
                or (hi is not None and value > hi)):
            errors.append(f"{group}.{key} must be within [{lo}, {hi if hi is not None else 'inf'}] (got {value!r})")
    if not isinstance(exemptions, dict):
        errors.append("relay_path_exemptions must be an object {counter: reason}")
    elif len(exemptions) > MAX_RELAY_EXEMPTIONS:
        errors.append(f"relay_path_exemptions: at most {MAX_RELAY_EXEMPTIONS} of {len(RELAY_PATHS)} counters "
                      "may be exempted")
    else:
        for path, reason in exemptions.items():
            if path not in RELAY_PATHS:
                errors.append(f"relay_path_exemptions: unknown counter {path!r}")
            if not isinstance(reason, str) or not reason.strip():
                errors.append(f"relay_path_exemptions.{path}: a non-empty reason is required")
    rust_interval = cv(cfg, "validity_gates", "rust_health_interval_s")
    if isinstance(rust_interval, bool) or rust_interval not in RUST_HEALTH_INTERVALS_S:
        errors.append(f"validity_gates.rust_health_interval_s must be one of {sorted(RUST_HEALTH_INTERVALS_S)} "
                      f"(got {cv(cfg, 'validity_gates', 'rust_health_interval_s')!r})")
    if not isinstance(cv(cfg, "quality_gates", "reconnect_gate_enabled"), bool):
        errors.append("quality_gates.reconnect_gate_enabled must be true or false")
    if not isinstance(cv(cfg, "latency_gate", "allowed_unmeasured"), bool):
        errors.append("latency_gate.allowed_unmeasured must be true or false")
    if errors:
        raise ConfigError("invalid scorer config:\n  " + "\n  ".join(errors))


def config_overrides(cfg):
    """'group.key: default -> effective' for every value that differs from the shipped config."""
    with open(DEFAULT_CONFIG_PATH, encoding="utf-8") as fh:
        base = json.load(fh)
    out = []
    for group, entries in sorted(cfg.items()):
        if not isinstance(entries, dict):
            continue
        for key, entry in sorted(entries.items()):
            default = base.get(group, {}).get(key)
            if entry != default:
                out.append(f"{group}.{key}: {json.dumps(default, sort_keys=True)} -> {json.dumps(entry, sort_keys=True)}")
    return out


def required_relay_paths(cfg):
    exempt = cv(cfg, "validity_gates", "relay_path_exemptions")
    return {m: matchers for m, matchers in RELAY_PATHS.items() if m not in exempt}


def cv(cfg, group, key):
    return cfg[group][key]["value"]


def latency_metric(cfg):
    return cv(cfg, "latency_gate", "audio_delay_metric")


def pair_metrics(cfg):
    lat = latency_metric(cfg)
    return PAIR_METRICS + ((lat,) if lat else ())


# ---------------------------------------------------------------- statistics

def percentile(values, q):
    """Linear-interpolation percentile (same as numpy's default); None for no values."""
    s = sorted(values)
    if not s:
        return None
    k = (len(s) - 1) * q / 100.0
    lo, hi = math.floor(k), math.ceil(k)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def summarize_dimension(values_by_user, band, k_red_fail):
    """p50/p95 across participants; band=None means a diagnostic that is never gated."""
    vals = list(values_by_user.values())
    if not vals:
        return {"n": 0, "p50": None, "p95": None, "k_red": None if band is None else 0, "gate_fail": False}
    if not all(math.isfinite(v) for v in vals):
        return {"n": len(vals), "p50": None, "p95": None, "k_red": None, "gate_fail": band is not None,
                "non_finite": sum(1 for v in vals if not math.isfinite(v))}
    p95 = percentile(vals, 95)
    if band is None:
        return {"n": len(vals), "p50": percentile(vals, 50), "p95": p95, "k_red": None, "gate_fail": False}
    k_red = sum(1 for v in vals if v >= band["red"])
    return {
        "n": len(vals),
        "p50": percentile(vals, 50),
        "p95": p95,
        "k_red": k_red,
        "gate_fail": p95 > band["red"] or k_red >= k_red_fail,
    }


# ---------------------------------------------------------------- data fetch

def step_queries(manifest, step, cfg, observers):
    """(name, promql, start, end) for everything one step needs. observers=None means no from_peer filter."""
    m = manifest["meeting_id"]
    obs = [("from_peer", "=~", any_of_regex(observers))] if observers else []
    obs_peer = [("peer_id", "=~", any_of_regex(observers))] if observers else []
    hold = (step["hold_start"], step["hold_end"])
    step_s = cv(cfg, "sampling", "scrape_step_s")
    wide = (step["hold_start"] - step_s * math.ceil((step["hold_start"] - step["join_start"]) / step_s),
            step["hold_end"])
    meet = [("meeting_id", "=", m)]
    pairs = pair_metrics(cfg)
    q = [(name, selector(name, meet + obs), *hold) for name in pairs]
    q.append(("_pps_min", f"min_over_time({selector(M_PPS, meet + obs)}[{2 * cv(cfg, 'sampling', 'scrape_step_s')}s])",
              *hold))
    every_pair_family = selector("", [("__name__", "=~", any_of_regex(pairs))] + meet)
    q += [
        ("_reporters", f"count by (from_peer) ({every_pair_family})", *hold),
        (M_SENT, selector(M_SENT, meet), *wide),
        (M_PEER_INFO, selector(M_PEER_INFO, meet), *wide),
        (M_RTT, selector(M_RTT, meet + obs_peer), *hold),
        (M_REELECT, selector(M_REELECT, meet), *wide),
        (M_SERVER_CONN, selector(M_SERVER_CONN, meet), *wide),
        (M_ACTIVE_LAYERS, selector(M_ACTIVE_LAYERS, meet + [("media_kind", "=~", "camera|video")]), *hold),
        (M_HEALTH, M_HEALTH, *hold),
        ("relay_session_drops_room", selector("relay_session_drops_total", [("room", "=", m)]), *hold),
    ]
    for name, (metric, matchers) in SERVER_DIAG.items():
        q.append((name, selector(metric, matchers) if matchers else metric, *hold))
    for metric, matchers in required_relay_paths(cfg).items():
        q.append((metric, selector(metric, [("room", "=", m)] + matchers), *wide))
    for name, key in (("_up", "scrape_up_selector"), ("_restarts", "restarts_selector")):
        expr = cv(cfg, "validity_gates", key)
        if expr:
            q.append((name, expr, *hold))
    return q


def fetch_step_data(client, manifest, step, cfg, observers):
    step_s = cv(cfg, "sampling", "scrape_step_s")
    return {name: client.range(expr, start, end, step_s)
            for name, expr, start, end in step_queries(manifest, step, cfg, observers)}


# ---------------------------------------------------------------- scoring helpers

def smp_non_finite(samples):
    return getattr(samples, "non_finite", ())


def _index(series, conflicts=None):
    """(session_id, from_peer, to_peer) -> {ts: value}. Repeated series that disagree at a timestamp leave it
    out (missing data) and add the key to `conflicts`."""
    out, clash = {}, defaultdict(set)
    for labels, samples in series:
        key = (labels.get("session_id", ""), labels.get("from_peer", ""), labels.get("to_peer", ""))
        merged = out.setdefault(key, {})
        for ts, val in samples:
            if ts in merged and merged[ts] != val:
                clash[key].add(ts)
            merged[ts] = val
    for key, stamps in clash.items():
        for ts in stamps:
            del out[key][ts]
        if conflicts is not None:
            conflicts.add(key)
    return out


def _increments(samples, window_start):
    """{ts: increase since the previous sample} after window_start; per-sample form of counter_delta."""
    prev, out = 0.0, {}
    for ts, val in samples:
        if ts > window_start:
            out[ts] = val - prev if val >= prev else val
        prev = val
    return out


def _session_births(data):
    """session_id -> (first, last) M_SENT sample; M_SENT is fetched from join_start, before the hold."""
    out = {}
    for labels, samples in data.get(M_SENT, []):
        s = labels.get("session_id")
        if s and samples:
            ts = [t for t, _ in samples]
            first, last = out.get(s, (min(ts), max(ts)))
            out[s] = (min(first, *ts), max(last, *ts))
    return out


def _pair_timelines(index, sess2user, keep, births, unordered):
    """(receiver, publisher) -> {ts: value}; at each timestamp the most recently born receiver session wins,
    because a replaced session's series lingers with stale values until metrics-api reaps it. Sessions are
    ordered by their M_SENT samples, not by the pair samples, which the query clips at hold_start; a key whose
    sessions cannot all be ordered that way is added to `unordered`."""
    groups = defaultdict(list)
    for (sid, recv, to_peer), samples in index.items():
        if keep(recv) and samples:
            groups[(recv, sess2user.get(to_peer))].append((sid, samples))
    out = {}
    for key, lst in groups.items():
        if len(lst) > 1 and any(sid not in births for sid, _ in lst):
            unordered.add(key)
        rank = sorted((((births.get(sid, (min(smp), max(smp))), sid), smp) for sid, smp in lst), key=lambda e: e[0])
        values_at = defaultdict(set)
        for r, smp in rank:
            for t, v in smp.items():
                values_at[(r[0][0], t)].add(v)
        tied = {t for (_, t), vs in values_at.items() if len(vs) > 1}
        if tied:
            unordered.add(key)
        merged = {}
        for _, samples in rank:
            merged.update(samples)
        out[key] = {t: v for t, v in merged.items() if t not in tied}
    return out


def _session_map(data):
    sess2user = {}
    for labels, _ in data.get(M_SERVER_CONN, []):
        if labels.get("session_id") and labels.get("customer_email"):
            sess2user[labels["session_id"]] = labels["customer_email"]
    for name in (M_SENT, M_PEER_INFO):
        for labels, _ in data.get(name, []):
            if labels.get("session_id") and labels.get("peer_id"):
                sess2user[labels["session_id"]] = labels["peer_id"]
    user2sess = defaultdict(set)
    for s, u in sess2user.items():
        user2sess[u].add(s)
    return sess2user, user2sess


def _grid(lo, hi, step_s):
    out, t = [], lo
    while t <= hi + 1e-6:
        out.append(t)
        t += step_s
    return out


def _interval(part, hs, he):
    lo = max(hs, part.get("join_ts") or hs)
    leave = part.get("leave_ts")
    return lo, (min(he, leave) if leave is not None else he)


def _new_acc():
    return {"A_bad": 0.0, "A_exp": 0, "A_loss": 0, "V_freeze": 0.0, "V_exp_s": 0.0, "Q_bad": 0, "Q_exp": 0,
            "L": [], "stale": []}


def _dimension_values(acc):
    vals = {}
    if acc["A_exp"]:
        vals["A"] = acc["A_bad"] / acc["A_exp"]
    if acc["V_exp_s"]:
        vals["V"] = acc["V_freeze"] / acc["V_exp_s"]
    if acc["L"]:
        vals["L"] = percentile(acc["L"], 95)
    if acc["Q_exp"]:
        vals["Q"] = acc["Q_bad"] / acc["Q_exp"]
    if acc["stale"]:
        vals["stale"] = percentile(acc["stale"], 95)
    return vals


def _away_windows(events, user, he):
    """Declared [leave, rejoin) intervals of one user; a leave without a later rejoin runs to hold_end."""
    out, since = [], None
    for at, action in sorted((e["at"], e["action"]) for e in events
                             if e["action"] in ("leave", "rejoin") and user in e["participants"]):
        if action == "leave" and since is None:
            since = at
        elif action == "rejoin" and since is not None:
            out.append((since, at))
            since = None
    if since is not None:
        out.append((since, he))
    return out


def _resolve_leaves(in_step, data, flags, events, he, step_s):
    """A leave_ts before teardown is a planned exit only at the user's final leave event, else a drop (bots
    stamp leave_ts on a relay drop too). Absent leave_ts: the same, judged from the last observed sample, which
    metrics-api keeps until its reaper runs (timeout plus one scrape)."""
    linger = METRICS_SESSION_TIMEOUT_S + step_s
    out = {}
    for u, p in in_step.items():
        final = [lo for lo, hi in _away_windows(events, u, he) if hi == he]
        if "leave_ts" in p:
            left = p["leave_ts"]
            if left is None or left >= he - step_s or any(abs(left - at) <= step_s for at in final):
                out[u] = p
            else:
                out[u] = dict(p, leave_ts=None)
                flags.append(f"{u}: leave_ts {left:.0f} is before teardown and not at a final leave event: "
                             "counted as present (a drop)")
            continue
        seen = [t for ts in _sessions_of(data, u).values() for t in ts if t <= he]
        last = max(seen) if seen else p["join_ts"]
        if last >= he - step_s or any(at - step_s <= last <= at + linger + step_s for at in final):
            out[u] = dict(p, leave_ts=last)
            flags.append(f"{u}: leave_ts absent, treated as left at last observed sample {last:.0f}")
        else:
            out[u] = dict(p, leave_ts=None)
            flags.append(f"{u}: leave_ts absent and last seen at {last:.0f}, before teardown and not at a final "
                         "leave event: counted as present (a drop)")
    return out


def _event_problems(events, parts, in_step, step):
    """Events that the scorer cannot honour make the step INVALID instead of excusing anything."""
    problems = []
    js, hs, he = step["join_start"], step["hold_start"], step["hold_end"]
    for e in events:
        if e["action"] in UNHONOURED_EVENTS:
            problems.append(f"{e['action']} at {e['at']:.0f} is not honoured by scorer v1")
        for u in e["participants"]:
            if u not in in_step:
                problems.append(f"{e['action']} at {e['at']:.0f} names {u}, who is not in this step")
        if not js <= e["at"] <= he:
            problems.append(f"{e['action']} at {e['at']:.0f} is outside the step")
        if e["action"] == "mute" and hs <= e["at"] <= he:
            for u in e["participants"]:
                if parts.get(u, {}).get("talker"):
                    problems.append(f"mute at {e['at']:.0f} on declared continuous talker {u} inside the hold")
    for u in sorted(u for u in in_step if parts[u]["publishes"].get("screen")):
        problems.append(f"screen share by {u} (publishes.screen) is not scored by scorer v1")
    for u in sorted(u for u in in_step if parts[u]["talker"]):
        if _mic_on_since(parts[u], [e for e in events if e["at"] < hs], u, 0) is None:
            problems.append(f"declared talker {u} is muted at hold_start (publishes.mic at join, then mute/unmute "
                            "events)")
    for kinds in (("mute", "unmute"), ("leave", "rejoin")):
        for u in sorted({u for e in events if e["action"] in kinds for u in e["participants"]}):
            joined_muted = kinds[0] == "mute" and u in parts and not parts[u]["publishes"]["mic"]
            state = joined_muted
            for at, action in sorted((e["at"], e["action"]) for e in events
                                     if e["action"] in kinds and u in e["participants"]):
                if (action == kinds[0]) == state:
                    problems.append(f"{action} for {u} at {at:.0f} is not paired"
                                    + (" (publishes.mic is false at join)" if joined_muted else ""))
                state = action == kinds[0]
    return problems


def _mic_on_since(part, events, user, settle_s):
    """When the mic is on from for good: -inf if on at join and never muted, settle_s after the last unmute, or
    None if it ends muted. publishes is the join-time state (D28)."""
    on, since = part["publishes"]["mic"], -math.inf
    for at, action in sorted((e["at"], e["action"]) for e in events
                             if e["action"] in ("mute", "unmute") and user in e["participants"]):
        on = action == "unmute"
        since = at + settle_s
    return since if on else None


def _credited_rejoins(data, events, in_step, step, window, problems):
    """Declared rejoins inside the hold that a new session confirms by starting within [at, at + window]; a
    rejoin with no such session is unhonoured and must not excuse a real reconnect."""
    hs, he = step["hold_start"], step["hold_end"]
    out = {}
    for u in in_step:
        starts = sorted(ts[0] for ts in _sessions_of(data, u).values() if ts)
        credited = 0
        for at in sorted(e["at"] for e in events if e["action"] == "rejoin" and u in e["participants"]):
            hit = next((t for t in starts if at <= t <= at + window), None)
            if hit is None:
                problems.append(f"rejoin for {u} at {at:.0f}: no new session started by {at + window:.0f}")
                continue
            starts.remove(hit)
            credited += hs < at < he
        out[u] = credited
    return out


def _sessions_of(data, user):
    """session_id -> sorted sample timestamps of the user's reporter series."""
    out = defaultdict(set)
    for labels, samples in data.get(M_SENT, []):
        if labels.get("peer_id") == user:
            out[labels.get("session_id")].update(t for t, _ in samples)
    return {s: sorted(ts) for s, ts in out.items()}


def _stability(data, user, user2sess, step, part, away=(), filled=(), rejoins=0):
    hs, he = step["hold_start"], step["hold_end"]
    sessions = _sessions_of(data, user)
    new_in_hold = sum(1 for ts in sessions.values() if ts and hs < ts[0] <= he)
    initial_join_in_hold = 1 if (part.get("join_ts") or hs) > hs else 0
    by_result = defaultdict(list)
    for labels, samples in data.get(M_REELECT, []):
        if labels.get("session_id") in user2sess.get(user, set()):
            by_result[labels.get("result", "")].append([(t, v) for t, v in samples if t <= he])
    reelect = {r: counter_delta(merge_max(lst), hs) for r, lst in by_result.items()}
    proceeded = reelect.get("proceeded", 0.0)
    proceeded = proceeded if math.isfinite(proceeded) else 0.0
    unplanned = max(0.0, new_in_hold - initial_join_in_hold - proceeded - rejoins)
    lo, hi = _interval(part, hs, he)
    pres = {t for ts in sessions.values() for t in ts if lo <= t <= hi} | {t for t in filled if lo <= t <= hi}
    for a, b in away:
        t = max(a, lo)
        while t <= min(b, hi):
            pres.add(t)
            t += 1.0
    pres = sorted(pres)
    if pres:
        max_gap = max([pres[0] - lo, hi - pres[-1]] + [b - a for a, b in zip(pres, pres[1:])])
    else:
        max_gap = hi - lo
    hours = (he - hs) / 3600.0
    return {
        "sessions_in_hold": sum(1 for ts in sessions.values() if any(hs <= t <= he for t in ts)),
        "new_sessions_in_hold": new_in_hold,
        "migrations": proceeded,
        "unplanned_reconnects": unplanned,
        "reelection_failed": reelect.get("failed", 0.0),
        "max_presence_gap_s": max_gap,
        "S": unplanned / hours if hours > 0 else 0.0,
    }


def _reconnect_masked(data, user, step, grid, stab, away):
    """Grid points hidden by a quick unplanned reconnect: the replaced session's last values linger for up to
    METRICS_SESSION_TIMEOUT_S, so from (its last sample - timeout) to the new session's first sample nothing
    measured is fresh. Empty when the user has no unplanned reconnect."""
    if not stab["unplanned_reconnects"]:
        return set()
    hs, he = step["hold_start"], step["hold_end"]
    sessions = sorted((ts[0], ts[-1]) for ts in _sessions_of(data, user).values() if ts)
    out = set()
    for (_, prev_last), (first, _) in zip(sessions, sessions[1:]):
        if not hs < first <= he or any(lo <= first <= hi for lo, hi in away):
            continue
        out |= {t for t in grid if min(prev_last, first) - METRICS_SESSION_TIMEOUT_S <= t < first}
    return out


def _transport(data, sessions):
    counts = defaultdict(int)
    for labels, samples in data.get(M_RTT, []):
        if labels.get("session_id") in sessions:
            counts[labels.get("server_type") or "unknown"] += len(samples)
    return max(counts, key=counts.get) if counts else "unknown"


def _talker_sending(data, talker, ts_grid):
    """Timestamps at which the talker is known to be sending (sender side).

    Without a reporter series for the talker (e.g. bots that send no HEALTH), the manifest
    declaration is taken as-is; with one, timestamps where it reports nothing or 0 are removed.
    """
    sent = {}
    for labels, samples in data.get(M_SENT, []):
        if labels.get("peer_id") == talker:
            for t, v in samples:
                sent[t] = max(v, sent.get(t, 0.0))
    if not sent:
        return set(ts_grid), False
    return {t for t in ts_grid if sent.get(t, 0.0) > 0}, True


# ---------------------------------------------------------------- scoring

def _split_rate(receivers, talkers, iv, heard, sending, grid):
    """Per scrape bucket per transmitting talker: every counted receiver hears it (healthy), none do
    (muted, or a room-wide loss that dimension A scores), or some do and some don't (split).
    heard(r, t, ts) is True, False, or None for a receiver that is not counted at ts."""
    counts = {"healthy": 0, "split": 0, "none_received": 0}
    zero = defaultdict(int)
    for t in sorted(talkers):
        others = sorted(receivers - {t})
        for ts in grid:
            if ts not in sending[t] or not iv[t][0] <= ts <= iv[t][1]:
                continue
            got, missed = [], []
            for r in others:
                if not iv[r][0] <= ts <= iv[r][1]:
                    continue
                h = heard(r, t, ts)
                if h is not None:
                    (got if h else missed).append(r)
            if not got and not missed:
                continue
            if not missed:
                counts["healthy"] += 1
            elif not got:
                counts["none_received"] += 1
            else:
                counts["split"] += 1
                for r in missed:
                    zero[f"{r} <- {t}"] += 1
    classified = counts["healthy"] + counts["split"]
    return dict(counts, split_rate=counts["split"] / classified if classified else None,
                zero_packet_receivers=dict(sorted(zero.items())))


def score_step(manifest, step, data, cfg, mode="run", generator_verdict=None, split_transport=False):
    sid = step["step_id"]
    hs, he = step["hold_start"], step["hold_end"]
    step_s = cv(cfg, "sampling", "scrape_step_s")
    parts = {p["user_id"]: p for p in manifest["participants"] if p["user_id"] is not None}
    flags = []
    events = [e for e in manifest.get("events", []) if e.get("step_id") == sid]
    in_step = _resolve_leaves({u: p for u, p in parts.items() if sid in p["steps"]}, data, flags, events, he, step_s)
    observers = {u for u, p in in_step.items() if p["observer"]}
    talkers = {u for u, p in in_step.items() if p["talker"]}
    reshaped = {u for e in events if e["action"] == "netem" for u in e["participants"]}
    shaped = {u: bool(p["network"]["shaped"]) or u in reshaped for u, p in parts.items()}
    sess2user, user2sess = _session_map(data)
    everyone = set(in_step)
    bands = cfg["bands"]
    k_fail = cv(cfg, "quality_gates", "k_red_fail")
    full = cv(cfg, "dimensions", "expand_ops_full_scale")
    nominal_pps = cv(cfg, "dimensions", "nominal_talker_pps")
    min_pps = cv(cfg, "dimensions", "talker_min_pps")
    frozen_fps = cv(cfg, "dimensions", "near_frozen_fps")
    lat_metric = latency_metric(cfg)
    grid = _grid(hs, he, step_s)

    declared_away = {u: _away_windows(events, u, he) for u in in_step}
    grace = cv(cfg, "quality_gates", "rejoin_grace_s")
    away = {u: [(lo, hi if hi >= he else min(he, hi + grace)) for lo, hi in w] for u, w in declared_away.items()}
    event_problems = _event_problems(events, parts, in_step, step)
    rejoin_count = _credited_rejoins(data, events, in_step, step, grace + step_s, event_problems)

    def is_away(u, ts):
        return any(lo <= ts and (ts < hi or hi >= he) for lo, hi in away.get(u, ()))
    transport = {u: _transport(data, user2sess.get(u, set())) for u in observers}

    def cell_of(recv, pub):
        r = "S" if shaped.get(recv) else "U"
        p = "?" if pub not in shaped else ("S" if shaped[pub] else "U")
        cell = f"{r}x{p}"
        return cell

    conflicts = set()
    idx = {name: _index(data.get(name, []), conflicts) for name in pair_metrics(cfg)}
    acc = defaultdict(lambda: defaultdict(_new_acc))
    talker_acc = defaultdict(lambda: defaultdict(lambda: [0.0, 0]))
    seen_by = defaultdict(set)
    sampled = defaultdict(set)
    entries = defaultdict(set)
    unknown_pubs = set()
    diag = {"max_staleness": (0.0, None), "max_freeze": (0.0, None), "talker_not_sending_samples": 0}
    ambiguous = set()
    canvas_ambiguous = set()
    incomplete = defaultdict(list)

    for i in idx.values():
        for (_, recv, to_peer), samples in i.items():
            if recv in observers:
                seen_by[to_peer].add(recv)
                pub = sess2user.get(to_peer)
                sampled[(recv, pub)].update(samples)
                if pub != recv:
                    for ts in samples:
                        entries[(recv, ts)].add(to_peer)
    seen_ts = {u: sorted(t for ts in _sessions_of(data, u).values() for t in ts) for u in in_step}
    iv_all = {u: _interval(in_step[u], hs, he) for u in in_step}

    up_down = {t for _, smp in data.get("_up", []) for t, v in smp if v == 0}
    health_ts = {t for _, smp in data.get(M_HEALTH, []) for t in [t for t, _ in smp] + list(smp_non_finite(smp))}
    non_finite = sorted((t, name) for name, series in data.items() if name not in DIAGNOSTIC_SERIES
                        for _, smp in series for t in smp_non_finite(smp))
    pipeline_gaps = {ts for ts in grid if ts in up_down or (health_ts and ts not in health_ts)}
    stability = {u: _stability(data, u, user2sess, step, in_step[u], away[u], pipeline_gaps, rejoin_count[u])
                 for u in everyone}
    masked = {u: _reconnect_masked(data, u, step, grid, stability[u], away[u]) for u in everyone}
    linger = METRICS_SESSION_TIMEOUT_S + step_s
    for u, windows in declared_away.items():
        for lo, hi in windows:
            inside = [t for t in seen_ts.get(u, []) if lo + linger + step_s < t < hi - step_s]
            if inside:
                event_problems.append(f"{u} kept reporting inside its declared leave window ({inside[0]:.0f})")

    births = _session_births(data)
    unordered = set()

    def timelines(index):
        return _pair_timelines(index, sess2user, lambda r: r in observers, births, unordered)

    listen = timelines(idx[M_CAN_LISTEN])
    pps_min = timelines(_index(data.get("_pps_min", []), conflicts))
    freeze_tl = timelines(idx[M_FREEZE])
    fps_tl = timelines(idx[M_FPS])

    def at_cap(r, ts):
        return len(entries.get((r, ts), ())) >= DEPLOYED_PEER_STATS_CAP

    def heard(r, t, ts):
        """True/False whether r receives t's audio at ts; None when r is not counted at ts."""
        if (mode == "real" and not entries.get((r, ts))) or ts in pipeline_gaps or is_away(r, ts) or is_away(t, ts):
            return None
        if ts in masked.get(r, ()) or ts in masked.get(t, ()):
            return False
        if ts not in sampled.get((r, t), ()):
            if at_cap(r, ts):
                ambiguous.add((r, t, ts))
                return None
            return False
        if not listen.get((r, t), {}).get(ts):
            return False
        pv = pps_min.get((r, t), {}).get(ts)
        if pv is None:
            pv = audio.get((r, t), {}).get(ts, (None, None))[0]
        return bool(pv)

    unknown_pubs |= {to_peer for (_, recv, to_peer) in idx[M_PPS] if recv in observers and to_peer not in sess2user}
    expand_tl = timelines(idx[M_EXPAND])
    audio = {key: {ts: (pv, expand_tl.get(key, {}).get(ts)) for ts, pv in tl.items()}
             for key, tl in timelines(idx[M_PPS]).items()}

    if mode == "run":
        sending = {}
        for t in talkers:
            mic_since = _mic_on_since(in_step[t], events, t, step_s)
            sending[t] = {ts for ts in grid if not is_away(t, ts) and mic_since is not None and ts >= mic_since}
            diag["talker_not_sending_samples"] += len(set(grid) - _talker_sending(data, t, grid)[0])
    else:
        sending = {t: set(grid) for t in talkers}

    if mode == "run" and not idx[M_PPS]:
        a_status = f"absent (required series {M_PPS} returned nothing)"
    elif mode == "run":
        a_status = "ok"
        for t in sorted(talkers):
            t_lo, t_hi = _interval(in_step[t], hs, he)
            for r in sorted(observers - {t}):
                r_lo, r_hi = _interval(in_step[r], hs, he)
                timeline = audio.get((r, t), {})
                a = acc[cell_of(r, t)][r]
                for ts in grid:
                    if not (max(t_lo, r_lo) <= ts <= min(t_hi, r_hi)):
                        continue
                    if ts not in sending[t] or ts in pipeline_gaps or is_away(r, ts):
                        continue
                    if ts not in sampled.get((r, t), ()) and at_cap(r, ts):
                        ambiguous.add((r, t, ts))
                        continue
                    pv, ev = timeline.get(ts, (None, None))
                    if pv is None or ev is None or listen.get((r, t), {}).get(ts) is None:
                        incomplete["talker pair"].append(f"{r} <- {t} @ {ts:.0f}")
                    deaf = not listen.get((r, t), {}).get(ts) or ts in masked[r] or ts in masked[t]
                    if pv is None or ev is None or deaf:
                        bad = 1.0
                    else:
                        bad = max(min(ev / full, 1.0), max(0.0, 1.0 - pv / nominal_pps))
                    a["A_bad"] += bad
                    a["A_exp"] += 1
                    per_talker = talker_acc[cell_of(r, t)][t]
                    per_talker[0] += bad
                    per_talker[1] += 1
                    if pv is None or deaf or pv < min_pps:
                        a["A_loss"] += 1
    else:
        a_status = "pause-confounded (real meeting: talker pauses count as concealment)"
        for (r, pub), timeline in audio.items():
            a = acc[cell_of(r, pub)][r]
            for ts, (pv, ev) in timeline.items():
                if pv >= min_pps and ev is not None:
                    a["A_bad"] += min(ev / full, 1.0)
                    a["A_exp"] += 1

    if mode == "run":
        for u in sorted(everyone):
            seen = seen_ts.get(u, [])
            lo, hi = _interval(in_step[u], hs, he)
            for ts in grid:
                if lo <= ts <= hi and ts not in pipeline_gaps and not is_away(u, ts):
                    i = bisect.bisect_left(seen, ts - step_s / 2)
                    if i == len(seen) or seen[i] >= ts + step_s / 2:
                        incomplete["presence"].append(f"{u} @ {ts:.0f}")
        present_at = {ts: sum(1 for v in everyone if iv_all[v][0] <= ts <= iv_all[v][1] and not is_away(v, ts))
                      for ts in grid}
        for u in sorted(observers):
            lo, hi = iv_all[u]
            for ts in grid:
                if (lo <= ts <= hi and ts not in pipeline_gaps and not is_away(u, ts) and present_at[ts] > 1
                        and not entries.get((u, ts))):
                    incomplete["observer report"].append(f"{u} @ {ts:.0f}")
        cameras = sorted(u for u, pt in in_step.items() if pt["publishes"].get("camera")) if idx[M_FREEZE] else []
        for p in cameras:
            for r in sorted(observers - {p}):
                lo, hi = max(iv_all[p][0], iv_all[r][0]), min(iv_all[p][1], iv_all[r][1])
                for ts in grid:
                    if (not lo <= ts <= hi or ts in pipeline_gaps or is_away(p, ts) or is_away(r, ts)
                            or not entries.get((r, ts)) or ts in freeze_tl.get((r, p), {})):
                        continue
                    if ts not in listen.get((r, p), {}) and at_cap(r, ts):
                        ambiguous.add((r, p, ts))
                        continue
                    if ts in listen.get((r, p), {}) and len(cameras) - (r in cameras) > CANVAS_LIMIT:
                        canvas_ambiguous.add((r, p, ts))
                        continue
                    a = acc[cell_of(r, p)][r]
                    a["V_freeze"] += step_s
                    a["V_exp_s"] += step_s
                    incomplete["camera pair"].append(f"{r} <- {p} @ {ts:.0f}")

    if mode == "run":
        if non_finite:
            incomplete["non-finite samples"] = [f"{name} @ {t:.0f}" for t, name in non_finite]
        if conflicts:
            incomplete["conflicting duplicate series"] = [f"{s} {r} -> {p}" for s, r, p in sorted(conflicts)]
        if unordered:
            incomplete["session order unknown"] = [f"{r} <- {p}" for r, p in sorted(unordered, key=str)]
        for u in sorted(everyone):
            incomplete["reconnect-masked"] += [f"{u} @ {ts:.0f}" for ts in sorted(masked[u] - pipeline_gaps)]
        if not incomplete["reconnect-masked"]:
            del incomplete["reconnect-masked"]

    split_receivers = {u for u in observers if not shaped.get(u)}
    split_talkers = {t for t in talkers if not shaped.get(t)}
    iv = {u: _interval(in_step[u], hs, he) for u in split_receivers | split_talkers}
    split = _split_rate(split_receivers, split_talkers, iv, heard, sending, grid)

    for (recv, pub), tl in freeze_tl.items():
        delta = counter_delta(sorted(tl.items()), hs)
        fps = fps_tl.get((recv, pub), {})
        accrued = _increments(sorted(tl.items()), hs)
        exposure = sum(max(accrued.get(t, 0.0), step_s if fps.get(t, 0) > 0 else 0.0)
                       for t in set(accrued) | {t for t in fps if hs < t <= he})
        a = acc[cell_of(recv, pub if pub in shaped else None)][recv]
        a["V_freeze"] += delta
        a["V_exp_s"] += exposure
        if delta > diag["max_freeze"][0]:
            diag["max_freeze"] = (delta, f"{recv} <- {pub}")

    stale_tl = timelines(idx[M_STALE])
    for (recv, pub), fps in fps_tl.items():
        a = acc[cell_of(recv, pub)][recv]
        stale = stale_tl.get((recv, pub), {})
        for ts, fv in fps.items():
            if fv <= 0:
                continue
            a["Q_exp"] += 1
            if fv < frozen_fps:
                a["Q_bad"] += 1
            if ts in stale:
                a["stale"].append(stale[ts])
                if stale[ts] > diag["max_staleness"][0]:
                    diag["max_staleness"] = (stale[ts], f"{recv} <- {pub}")

    if lat_metric:
        absolute = mode == "run" and manifest["clock"].get("sync") in SYNCED_CLOCKS
        for (_, recv, to_peer), samples in idx[lat_metric].items():
            vals = [v for t, v in sorted(samples.items()) if hs <= t <= he]
            if recv not in observers or not vals:
                continue
            floor = 0.0 if absolute else min(vals)
            acc[cell_of(recv, sess2user.get(to_peer))][recv]["L"].extend(v - floor for v in vals)

    required = list(cv(cfg, "validity_gates", "required_dimensions"))
    cells = {}
    groups = [(cell, users) for cell, users in sorted(acc.items())]
    if split_transport:
        groups += [(f"{cell}/{t}", {u: a for u, a in users.items() if transport.get(u, "unknown") == t})
                   for cell, users in sorted(acc.items())
                   for t in sorted({transport.get(u, "unknown") for u in users})]
    for cell, users in groups:
        per_user = {}
        loss = {}
        for u, a in users.items():
            vals = _dimension_values(a)
            if not vals:
                continue
            vals["S"] = stability[u]["S"]
            per_user[u] = vals
            if a["A_exp"]:
                loss[u] = a["A_loss"] / a["A_exp"]
        if not per_user:
            continue
        dims = {d: summarize_dimension({u: v[d] for u, v in per_user.items() if d in v}, bands[d], k_fail)
                for d in DIMENSIONS}
        if a_status != "ok":
            dims["A"]["excluded"] = a_status
        usable = {d for d in DIMENSIONS if dims[d]["n"] and not dims[d].get("excluded")}
        cells[cell] = {
            "n": len(per_user),
            "dimensions": dims,
            "diagnostic_dimensions": {"staleness_ms": summarize_dimension(
                {u: v["stale"] for u, v in per_user.items() if "stale" in v}, None, k_fail)},
            "missing_required_dimensions": [d for d in required if d not in usable],
            "absent_dimensions": [d for d in DIMENSIONS if d not in usable] + ["TTFF"],
            "participants": per_user,
            "audio_loss_share": loss,
            "transport_mix": _count(transport.get(u, "unknown") for u in per_user),
        }
        if "/" in cell:
            cells[cell]["report_only"] = True
        else:
            cells[cell]["talker_A"] = {t: b / n for t, (b, n) in sorted(talker_acc[cell].items()) if n}

    headline_key = "UxU" if "UxU" in cells else None
    headline = cells.get(headline_key) if headline_key else None
    reporters = _reporters(data, idx)
    rust_reporters = sorted(r for r in reporters if parts.get(r, {}).get("fleet") == "rust")
    validity = _validity_gates(manifest, step, data, cfg, mode, in_step, parts, observers, talkers, user2sess,
                               seen_by, headline, required, generator_verdict, rust_reporters,
                               (len(ambiguous), len(canvas_ambiguous)), event_problems, sorted(pipeline_gaps), grid,
                               declared_away)
    quality = _quality_gates(step, cfg, mode, in_step, everyone, shaped, stability, data, headline, bands, split)
    quality.append(_reconnect_gate(cfg, mode, everyone, shaped, stability, bands))
    if mode == "run":
        missing = {k: len(v) for k, v in incomplete.items()}
        examples = [x for v in incomplete.values() for x in v[:3]]
        quality.append(_gate("G-Q8", "fail" if missing else "pass", missing,
                             ("missing samples during the hold (counted as loss, never as healthy): "
                              + ", ".join(examples)) if missing else ""))
    else:
        quality.append(_gate("G-Q8", "not_applicable", detail="no roster without a manifest"))
    allowed = set(cv(cfg, "validity_gates", "allowed_not_measured"))
    latency_waived = lat_metric is None and cv(cfg, "latency_gate", "allowed_unmeasured")
    if latency_waived:
        allowed.add("G-Q5")
    blocking_unmeasured = [g["gate"] for g in validity + quality
                           if g["status"] == "not_measured" and g["gate"] not in allowed]

    if mode == "real":
        verdict = "REPORT"
    elif any(g["status"] == "fail" for g in validity) or blocking_unmeasured:
        verdict = "INVALID"
    elif headline is None:
        verdict = "INVALID"
    elif any(g["status"] == "fail" for g in quality):
        verdict = "FAIL"
    else:
        verdict = "PASS"

    return {
        "step_id": sid,
        "n_target": step["n_target"],
        "headline": step["headline"],
        "window": [hs, he],
        "join_start": step["join_start"],
        "verdict": verdict,
        "notices": ([LATENCY_PENDING] if latency_waived else [])
        + ([f"G-Q9 unplanned-reconnect gate: {RECONNECT_PENDING}"] if mode == "run" else [])
        + [f"relay path exempted: {m} ({why})"
           for m, why in sorted(cv(cfg, "validity_gates", "relay_path_exemptions").items()) if mode == "run"],
        "flags": flags,
        "invalid_reasons": blocking_unmeasured,
        "headline_cell": headline_key,
        "validity": validity,
        "quality_gates": quality,
        "dimension_A_status": a_status,
        "p95_table": {c: dict({d: v["dimensions"][d]["p95"] for d in DIMENSIONS},
                              staleness_ms=v["diagnostic_dimensions"]["staleness_ms"]["p95"])
                      for c, v in cells.items()},
        "split": split,
        "cells": cells,
        "stability": stability,
        "transport": transport,
        "layer_mix": _layer_mix(data),
        "server_diagnostics": _server_diagnostics(data, hs, step_s),
        "diagnostics": {
            "note": "Max values and staleness are diagnostics only and never gate (doc sections 2.3, 3.4).",
            "max_staleness_ms": diag["max_staleness"][0],
            "max_staleness_pair": diag["max_staleness"][1],
            "max_freeze_seconds_pair": diag["max_freeze"][0],
            "max_freeze_pair": diag["max_freeze"][1],
            "talker_not_sending_samples": diag["talker_not_sending_samples"],
            "coverage_ambiguous_samples": len(ambiguous) + len(canvas_ambiguous),
            "excluded_reporters": sorted(r for r in reporters if r not in observers),
            "rust_bot_reporters": rust_reporters,
            "unknown_publisher_sessions": sorted(unknown_pubs),
        },
    }


def _reporters(data, idx):
    if "_reporters" in data:
        return {labels.get("from_peer") for labels, _ in data["_reporters"] if labels.get("from_peer")}
    return {k[1] for i in idx.values() for k in i}


def _count(items):
    out = defaultdict(int)
    for it in items:
        out[it] += 1
    return dict(out)


def _layer_mix(data):
    fps = [v for _, samples in data.get(M_FPS, []) for _, v in samples if v > 0]
    layers = [v for _, samples in data.get(M_ACTIVE_LAYERS, []) for _, v in samples]
    return {"decoded_fps_p50": percentile(fps, 50), "publisher_active_video_layers_p50": percentile(layers, 50)}


def _sum_delta(series, start):
    return sum(counter_delta(s, start) for _, s in series) if series else None


def _server_diagnostics(data, start, step_s):
    out = {name: _sum_delta(data.get(name, []), start) for name in SERVER_DIAG}
    le50, count = out.pop("sched_lag_le50"), out.pop("sched_lag_count")
    late = (1 - le50 / count) if le50 is not None and count else None
    out["scheduler_late_fraction_gt_50ms"] = late if late is None or math.isfinite(late) else None
    out["relay_session_drops_room"] = _sum_delta(data.get("relay_session_drops_room", []), start)
    out["note"] = ("All but relay_session_drops_room lack a room label, so they are attributable to this run "
                   "only on a dedicated relay. Reported, not scored or gated in v1 (server-health sub-score deferred).")
    return out


def _gate(gate, status, value=None, detail=""):
    return {"gate": gate, "status": status, "value": value, "detail": detail}


def _validity_gates(manifest, step, data, cfg, mode, in_step, parts, observers, talkers, user2sess, seen_by,
                    headline, required, generator_verdict, rust_reporters, ambiguous, event_problems,
                    pipeline_gaps, grid, away):
    hs, he = step["hold_start"], step["hold_end"]
    out = []
    na = "not_applicable"
    if mode == "real":
        out.append(_gate("G-V1", na, detail="expected reporter count unknown without a manifest"))
    else:
        health = data.get(M_HEALTH, [])
        span = [t for _, smp in health for t, _ in smp if hs <= t <= he]
        t0, t1 = (min(span), max(span)) if span else (hs, he)
        delta = _sum_delta(health, t0) if span else None
        expected = 0.0
        for u, p in in_step.items():
            lo, hi = _interval(p, t0, t1)
            secs = max(0.0, hi - lo) - sum(max(0.0, min(b, hi) - max(a, lo)) for a, b in away.get(u, ()))
            interval = "browser_health_interval_s" if p["fleet"] in ("browser", "human") else "rust_health_interval_s"
            expected += secs / cv(cfg, "validity_gates", interval)
        silent = []
        for u, p in sorted(in_step.items()):
            lo, hi = _interval(p, hs, he)
            if lo <= hi and not any(lo <= t <= hi for ts in _sessions_of(data, u).values() for t in ts):
                silent.append(u)
        if delta is None:
            out.append(_gate("G-V1", "fail", None, f"required series {M_HEALTH} is absent"))
        else:
            ratio = delta / expected if expected > 0 else 0.0
            lo_r = cv(cfg, "validity_gates", "health_reports_min_ratio")
            hi_r = cv(cfg, "validity_gates", "health_reports_max_ratio")
            problems = [] if lo_r <= ratio <= hi_r else [f"ratio outside [{lo_r}, {hi_r}] (another meeting on the "
                                                          "pipeline, or a bot cadence that differs from the config)"]
            if silent:
                problems.append(f"no {M_SENT} sample in the hold from: " + ", ".join(silent))
            out.append(_gate("G-V1", "fail" if problems else "pass", round(ratio, 4),
                             "; ".join(problems) or "valid only if this run is alone on the metrics pipeline"))
    out.append(_gate("G-V2", "not_measured",
                     detail="no stale-discard counter exists yet (#2920); packets > 30 s old are dropped silently"))
    for gate, key, name in (("G-V3", "scrape_up_selector", "_up"), ("G-V4", "restarts_selector", "_restarts")):
        if not cv(cfg, "validity_gates", key):
            out.append(_gate(gate, "not_measured", detail=f"validity_gates.{key} not configured"))
            continue
        series = data.get(name, [])
        if gate == "G-V3":
            vals = [v for _, s in series for _, v in s]
            ok = bool(vals) and min(vals) >= 1
            out.append(_gate(gate, "pass" if ok else "fail", min(vals) if vals else None,
                             "" if vals else "configured selector returned no series"))
        else:
            restarts = _sum_delta(series, hs)
            out.append(_gate(gate, "pass" if restarts == 0 else "fail", restarts,
                             "" if series else "configured selector returned no series"))
    if mode == "real":
        out.append(_gate("G-V5", na, detail="no load generators in a real meeting"))
    elif generator_verdict is None:
        out.append(_gate("G-V5", "not_measured", detail="no --generator-verdict supplied (owned by Discussion B)"))
    else:
        ok = generator_verdict.get("ok") is True
        out.append(_gate("G-V5", "pass" if ok else "fail", ok, generator_verdict.get("detail", "")))
    must_see = sorted((observers | talkers) if mode == "run" else observers)
    hidden = [u for u in must_see
              if not any(seen_by.get(s, set()) - {u} for s in user2sess.get(u, set()))]
    status = "pass" if not hidden else ("fail" if mode == "run" else "info")
    out.append(_gate("G-V6", status, f"{len(must_see) - len(hidden)}/{len(must_see)} visible",
                     ("hidden (no per-pair series from any observer; see #2916): " + ", ".join(hidden))
                     if hidden else ""))
    if mode == "real":
        out += [_gate("G-V7", na, detail="no observer roster without a manifest"),
                _gate("G-V8", na, detail="no identity roster without a manifest"),
                _gate("G-V9", na, detail="dimension A is pause-confounded in real meetings"),
                _gate("G-V10", na, detail="no fleet roster without a manifest"),
                _gate("G-V11", na, detail="relay paths are not a property of a real meeting"),
                _gate("G-V12", na, detail=f"{sum(ambiguous)} ambiguous samples (diagnostic in real meetings)"),
                _gate("G-V13", na, detail="no events without a manifest"),
                _gate("G-V14", na, detail=f"{len(pipeline_gaps)} pipeline-gap grid points excluded"),
                _gate("G-V15", na, detail="no join times without a manifest"),
                _gate("G-V16", na, detail="no n_target without a manifest")]
        return out
    n_min = cv(cfg, "validity_gates", "n_min_observers")
    per_dim = {d: (headline["dimensions"][d]["n"] if headline else 0) for d in required}
    thin = sorted(d for d, n in per_dim.items() if n < n_min)
    out.append(_gate("G-V7", "fail" if thin else "pass", per_dim,
                     f"need >= {n_min} observers in UxU for each required dimension"
                     + (f"; too few for {', '.join(thin)}" if thin else "")))
    unmapped = sorted(u for u in (observers | talkers) if not user2sess.get(u))
    known = set(parts)
    seen_ids = {labels.get("peer_id") for name in (M_SENT, M_PEER_INFO) for labels, _ in data.get(name, [])}
    unexpected = sorted(u for u in seen_ids if u and u not in known)
    unnamed = [f"participants[{i}] ({p['fleet']}/{p['role']})" for i, p in enumerate(manifest["participants"])
               if p["user_id"] is None and step["step_id"] in p["steps"]]
    problems = []
    if unnamed:
        problems.append("user_id never recorded (null in the manifest), so their data cannot be attributed: "
                        + ", ".join(unnamed))
    if unmapped:
        problems.append("no session for: " + ", ".join(unmapped))
    if unexpected:
        problems.append("identities not in the manifest: " + ", ".join(unexpected))
    out.append(_gate("G-V8", "fail" if problems else "pass", len(unmapped) + len(unexpected) + len(unnamed),
                     "; ".join(problems)))
    missing = headline["missing_required_dimensions"] if headline else list(required)
    reasons = [f"{d} (needs {', '.join(DIM_SOURCES[d])})" for d in missing]
    if "A" in missing and not talkers:
        reasons.append("no talker declared in the manifest")
    out.append(_gate("G-V9", "fail" if missing else "pass", missing,
                     ("required dimensions missing in UxU: " + "; ".join(reasons)) if missing else ""))
    out.append(_gate("G-V10", "fail" if rust_reporters else "pass", len(rust_reporters),
                     ("per-pair HEALTH from Rust bots (#2919: session-level only): " + ", ".join(rust_reporters))
                     if rust_reporters else ""))
    deltas = {}
    for m, matchers in required_relay_paths(cfg).items():
        series = [(labels, smp) for labels, smp in data.get(m, [])
                  if all(labels.get(k) == v for k, _, v in matchers)]
        deltas[m] = _sum_delta(series, step["join_start"])
    idle = sorted(m for m, d in deltas.items() if not d)
    out.append(_gate("G-V11", "fail" if idle else "pass", deltas,
                     ("path not exercised (counter stayed 0 or is absent for this room): " + ", ".join(idle))
                     if idle else ""))
    at_cap, canvas = ambiguous
    reasons = []
    if at_cap:
        reasons.append(f"{at_cap} samples missing from a receiver report that held >= {DEPLOYED_PEER_STATS_CAP} peers")
    if canvas:
        reasons.append(f"{canvas} camera samples without a video tracker while the step has more than "
                       f"{CANVAS_LIMIT} camera publishers besides the probe; a probe renders at most "
                       f"{CANVAS_LIMIT} tiles, so such a step (e.g. N=200 with cameras on) is INVALID by "
                       "construction until probes report which tiles they render")
    out.append(_gate("G-V12", "fail" if reasons else "pass", at_cap + canvas,
                     f"{COVERAGE_AMBIGUOUS}: " + "; ".join(reasons) if reasons else ""))
    out.append(_gate("G-V13", "fail" if event_problems else "pass", len(event_problems), "; ".join(event_problems)))
    run = longest = 0
    gaps = set(pipeline_gaps)
    for ts in grid:
        run = run + 1 if ts in gaps else 0
        longest = max(longest, run)
    share = len(gaps) / len(grid) if grid else 0.0
    bad = share > PIPELINE_GAP_MAX_SHARE or longest > PIPELINE_GAP_MAX_RUN
    out.append(_gate("G-V14", "fail" if bad else "pass", {"points": len(gaps), "share": round(share, 4),
                                                          "longest_run": longest},
                     f"failed scrapes (up == 0, or no {M_HEALTH} sample) are excluded from quality "
                     f"gates; budget: share <= {PIPELINE_GAP_MAX_SHARE}, run <= {PIPELINE_GAP_MAX_RUN}"
                     + (f"; at {', '.join(f'{t:.0f}' for t in sorted(gaps)[:5])}" if gaps else "")))
    teardown = he - cv(cfg, "sampling", "scrape_step_s")
    present = sum(1 for u, p in in_step.items()
                  if (p.get("leave_ts") is None or p["leave_ts"] >= teardown)
                  and not any(hi >= he and lo < teardown for lo, hi in away.get(u, ())))
    out.append(_gate("G-V16", "fail" if present < step["n_target"] else "pass", present,
                     f"n_target {step['n_target']}: participants that did not leave for good before hold_end"))
    deadline = cv(cfg, "quality_gates", "join_deadline_s")
    late = sorted(u for u, p in in_step.items() if p["join_ts"] > hs - deadline)
    out.append(_gate("G-V15", "fail" if late else "pass", len(late),
                     f"hold not steady: joined later than hold_start - {deadline} s: " + ", ".join(late) if late else ""))
    return out


def _audio_gate(gate, d, talker_a, red, talker_red):
    worst = max(talker_a.items(), key=lambda kv: kv[1], default=(None, None))
    loud = sorted(t for t, v in talker_a.items() if not v <= talker_red)
    detail = f"red = {red}; per-talker red = {talker_red}"
    if loud:
        detail += "; talkers in red: " + ", ".join(f"{t} ({talker_a[t]:.3f})" for t in loud)
    return _gate(gate, "fail" if d["gate_fail"] or loud else "pass",
                 {"p95": d["p95"], "k_red": d["k_red"], "worst_talker_A": worst[1]}, detail)


def _quality_gates(step, cfg, mode, in_step, members, shaped, stability, data, headline, bands, split):
    out = []
    unshaped_obs = sorted(u for u in members if not shaped.get(u))
    if mode == "real":
        out.append(_gate("G-Q1", "not_applicable", detail="no join times without a manifest"))
    else:
        deadline_s = cv(cfg, "quality_gates", "join_deadline_s")
        late = []
        for u in unshaped_obs:
            deadline = max(in_step[u]["join_ts"], step["join_start"]) + deadline_s
            first = [t for ts in _sessions_of(data, u).values() for t in ts
                     if step["join_start"] <= t <= step["hold_end"]]
            if not first or min(first) > deadline:
                late.append(u)
        out.append(_gate("G-Q1", "fail" if late else "pass", f"{len(unshaped_obs) - len(late)}/{len(unshaped_obs)}",
                         ("missing or late: " + ", ".join(late)) if late else ""))
    drop_gap = cv(cfg, "quality_gates", "drop_gap_s")
    dropped = [u for u in unshaped_obs
               if stability[u]["reelection_failed"] > 0 or stability[u]["max_presence_gap_s"] > drop_gap]
    out.append(_gate("G-Q2", "fail" if dropped else "pass", len(dropped),
                     ("dropped: " + ", ".join(dropped)) if dropped else ""))
    for gate, dim in QUALITY_GATE_DIMS:
        d = headline["dimensions"][dim] if headline else None
        if dim == "L" and latency_metric(cfg) is None:
            out.append(_gate(gate, "not_measured", detail=f"{LATENCY_PENDING} (latency_gate.audio_delay_metric "
                                                          "unset); staleness is a diagnostic only (#1880)"))
        elif d is None or d["n"] == 0:
            status = "not_applicable" if (mode == "real" and dim == "A") else "not_measured"
            out.append(_gate(gate, status, detail=f"no {dim} data in UxU"))
        elif d.get("excluded"):
            out.append(_gate(gate, "not_applicable" if mode == "real" else "not_measured", detail=d["excluded"]))
        elif dim == "A":
            out.append(_audio_gate(gate, d, headline.get("talker_A", {}), bands[dim]["red"],
                                   cv(cfg, "quality_gates", "per_talker_A_red")))
        else:
            out.append(_gate(gate, "fail" if d["gate_fail"] else "pass",
                             {"p95": d["p95"], "k_red": d["k_red"]}, f"red = {bands[dim]['red']}"))
    red = cv(cfg, "quality_gates", "split_rate_red")
    rate = split["split_rate"]
    if rate is None:
        out.append(_gate("G-Q7", "not_measured", detail="no transmitting-talker bucket with a sampled unshaped receiver"))
    else:
        zero = ", ".join(f"{k} ({v})" for k, v in split["zero_packet_receivers"].items())
        out.append(_gate("G-Q7", "fail" if rate > red else "pass", round(rate, 4),
                         f"red = {red}; {split['split']}/{split['split'] + split['healthy']} buckets split"
                         + (f"; zero-packet receivers: {zero}" if zero else "")))
    return out


def _reconnect_gate(cfg, mode, members, shaped, stability, bands):
    if not cv(cfg, "quality_gates", "reconnect_gate_enabled"):
        return _gate("G-Q9", "disabled", detail=f"{RECONNECT_PENDING}; disabled by config")
    values = {u: stability[u]["S"] for u in members if not shaped.get(u)}
    d = summarize_dimension(values, bands["S"], cv(cfg, "quality_gates", "k_red_fail"))
    if not d["n"]:
        return _gate("G-Q9", "not_applicable" if mode == "real" else "not_measured", detail=RECONNECT_PENDING)
    return _gate("G-Q9", "fail" if d["gate_fail"] else "pass", {"p95": d["p95"], "k_red": d["k_red"]},
                 f"unplanned reconnects per participant-hour, unshaped observers and talkers; red = {bands['S']['red']}; "
                 + RECONNECT_PENDING)


def score_run(manifest, datasets, cfg, mode="run", generator_verdict=None, split_transport=False):
    steps = [score_step(manifest, s, datasets[s["step_id"]], cfg, mode, generator_verdict, split_transport)
             for s in manifest["steps"]]
    headline = next(s for s in steps if s["headline"])
    return {
        "schema": "call-quality-score/v0",
        "run": {"run_id": manifest["run_id"], "meeting_id": manifest["meeting_id"],
                "environment": manifest["environment"], "commit": manifest["code"]["commit"],
                "bands_version": cfg.get("config_version"), "mode": mode,
                "scrape_step_s": cv(cfg, "sampling", "scrape_step_s")},
        "verdict": headline["verdict"],
        "notices": sorted({n for s in steps for n in s["notices"]}),
        "config_overrides": config_overrides(cfg),
        "headline_step": headline["step_id"],
        "steps": steps,
    }
