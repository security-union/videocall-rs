"""Call quality scoring (docs/quality-at-scale/CALL_QUALITY_SCORING.md sections 2, 3, 9)."""

import bisect
import copy
import json
import math
import os
from collections import defaultdict

from cq_manifest import non_finite_paths
from cq_prom import PromError, any_of_regex, counter_delta, selector

DEFAULT_CONFIG_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "call_quality_config.json")
DIMENSIONS = ("A", "V", "L", "Q", "S")
QUALITY_GATE_DIMS = (("G-Q3", "A"), ("G-Q4", "V"), ("G-Q5", "L"))
LATENCY_PENDING = "latency not gated: #2948 pending"
AUDIO_TERM_NAMES = {"videocall_audio_packets_discarded_total": "discarded audio",
                    "videocall_audio_playout_underrun_ms_total": "playout silence"}

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
M_DISCARDS = "videocall_audio_packets_discarded_total"
M_UNDERRUN = "videocall_audio_playout_underrun_ms_total"
M_BUFFER = "videocall_neteq_audio_buffer_ms"
M_TARGET = "videocall_neteq_target_delay_ms"
M_ACCEL = "videocall_neteq_accelerate_ops_per_sec"
AUDIO_COUNTERS = (M_DISCARDS, M_UNDERRUN)
DELAY_METRICS = (M_BUFFER, M_TARGET, M_ACCEL)
AUDIO_PACKET_MS = 20
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
                 ("dimensions", "nominal_talker_pps"): (50, 50),
                 ("dimensions", "discard_weight"): (1.0, None),
                 ("dimensions", "playout_underrun_weight"): (1.0, None)}
MAX_RELAY_EXEMPTIONS = 2
RUST_HEALTH_INTERVALS_S = (1, 5)
MIN_REQUIRED_DIMENSIONS = {"A", "V", "Q", "S"}


class ConfigError(Exception):
    pass
WT, BOTH = ("webtransport",), ("webtransport", "websocket")
RELAY_COUNTERS = {
    "mailbox_drops": ("relay_inbound_mailbox_drops_total", [], None, (), BOTH),
    "shed_escalations": ("relay_downlink_shed_escalations_total", [], "stage", ("one", "two"), WT),
    "stream_resets": ("relay_outbound_bridge_stream_resets_total", [], "reason", ("write_timeout",), WT),
    "wt_session_closes": ("relay_wt_session_closes_total", [], "reason", (), WT),
    "overflow_cap": ("relay_downlink_stream_overflow_frames_total", [("cause", "=", "cap")], None, (), WT),
    "outbound_channel_drops": ("videocall_outbound_channel_drops_total", [], "kind", ("audio",), BOTH),
    "stream_queue_drops": ("relay_downlink_stream_queue_drops_total", [], "kind", ("other",), WT),
}
SCHED_LAG = {"sched_lag_le50": ("videocall_relay_scheduler_lag_ms_bucket", [("le", "=~", "50(\\.0)?")]),
             "sched_lag_count": ("videocall_relay_scheduler_lag_ms_count", [])}
QUIC = {"_quic_rtt": "videocall_relay_connection_rtt_ms", "_quic_lost": "videocall_relay_connection_path_lost_packets",
        "_quic_sent": "videocall_relay_connection_path_sent_packets",
        "_quic_congestion": "videocall_relay_connection_path_congestion_events"}
CFS_QUERY = ("sum by (pod) (rate(container_cpu_cfs_throttled_periods_total{{{0}}}[1m])) / "
             "sum by (pod) (rate(container_cpu_cfs_periods_total{{{0}}}[1m]))")
SERVER_QUERIES = set(RELAY_COUNTERS) | set(SCHED_LAG) | set(QUIC) | {"_room_sessions", "_session_drops", "_cfs"}
DIAGNOSTIC_SERIES = SERVER_QUERIES | {M_RTT, M_ACTIVE_LAYERS, *DELAY_METRICS}
NOT_COLLECTED, FAILED = "not collected", "query failed"


class FailedQuery(list):
    """An empty result standing for a diagnostic query that failed, so it reads 'query failed', never 0."""


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
    share, pairs, cap = (cv(cfg, "diagnostics", k) for k in
                         ("correlated_freeze_share", "correlated_freeze_min_pairs", "target_delay_cap_ms"))
    if not (_is_num(share) and 0 < share <= 1 and isinstance(pairs, int) and not isinstance(pairs, bool)
            and pairs >= 1 and _is_num(cap) and cap > 0):
        errors.append("diagnostics: correlated_freeze_share in (0, 1], correlated_freeze_min_pairs an integer >= 1 and "
                      "target_delay_cap_ms > 0")
    pods = cv(cfg, "server_diagnostics", "throttle_pod_selector")
    if pods is not None and (not isinstance(pods, str) or not pods.strip() or set(pods) & set("{}\n")):
        errors.append("server_diagnostics.throttle_pod_selector must be null or label matchers without braces")
    if not isinstance(cv(cfg, "latency_gate", "allowed_unmeasured"), bool):
        errors.append("latency_gate.allowed_unmeasured must be true or false")
    if errors:
        raise ConfigError("invalid scorer config:\n  " + "\n  ".join(errors))


def _is_num(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)


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
    hold = (step["hold_start"], step["hold_end"])
    step_s = cv(cfg, "sampling", "scrape_step_s")
    wide = (step["hold_start"] - step_s * math.ceil((step["hold_start"] - step["join_start"]) / step_s),
            step["hold_end"])
    meet = [("meeting_id", "=", m)]
    pairs = pair_metrics(cfg)
    q = [(name, selector(name, meet + obs), *hold) for name in pairs]
    q += [(name, selector(name, meet + obs), *hold) for name in AUDIO_COUNTERS + DELAY_METRICS]
    q.append(("_pps_min", f"min_over_time({selector(M_PPS, meet + obs)}[{2 * cv(cfg, 'sampling', 'scrape_step_s')}s])",
              *hold))
    every_pair_family = selector("", [("__name__", "=~", any_of_regex(pairs))] + meet)
    q += [
        ("_reporters", f"count by (from_peer) ({every_pair_family})", *hold),
        (M_SENT, selector(M_SENT, meet), *wide),
        (M_PEER_INFO, selector(M_PEER_INFO, meet), *wide),
        (M_RTT, selector(M_RTT, meet), *wide),
        (M_REELECT, selector(M_REELECT, meet), *wide),
        (M_SERVER_CONN, selector(M_SERVER_CONN, meet), *wide),
        (M_ACTIVE_LAYERS, selector(M_ACTIVE_LAYERS, meet + [("media_kind", "=~", "camera|video")]), *hold),
        (M_HEALTH, M_HEALTH, *hold),
        ("_session_drops", selector("relay_session_drops_total", [("room", "=", m)]), *hold),
        ("_room_sessions", selector("relay_active_sessions_per_room", [("room", "=", m)]), *hold),
    ]
    for name, (metric, matchers, *_) in {**RELAY_COUNTERS, **SCHED_LAG}.items():
        q.append((name, selector(metric, matchers) if matchers else metric, *hold))
    q += [(name, selector(metric, [("room", "=", m)]), *hold) for name, metric in QUIC.items()]
    pods = cv(cfg, "server_diagnostics", "throttle_pod_selector")
    if pods:
        q.append(("_cfs", CFS_QUERY.format(pods), *hold))
    for metric, matchers in required_relay_paths(cfg).items():
        q.append((metric, selector(metric, [("room", "=", m)] + matchers), *wide))
    for name, key in (("_up", "scrape_up_selector"), ("_restarts", "restarts_selector")):
        expr = cv(cfg, "validity_gates", key)
        if expr:
            q.append((name, expr, *hold))
    return q


def fetch_step_data(client, manifest, step, cfg, observers):
    step_s = cv(cfg, "sampling", "scrape_step_s")
    out = {}
    for name, expr, start, end in step_queries(manifest, step, cfg, observers):
        try:
            out[name] = client.range(expr, start, end, step_s)
        except PromError:
            if name not in DIAGNOSTIC_SERIES:
                raise
            out[name] = FailedQuery()
    return out


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


def _series_increments(samples, start, base):
    """{ts: increase} of one counter after start; a first sample after start is booked against `base`, the value
    the counter had before this series (0 when it was created, None when unknown: that sample books nothing)."""
    out, prev = {}, base
    for ts, val in samples:
        if ts > start and prev is not None:
            out[ts] = val - prev if val >= prev else val
        prev = val
    return out


def _lane_increments(lane, start, upper):
    """lane: [(birth, chain, samples, provably_new)] of one counter across sessions -> ({ts: increase}, [(why, amount)]).
    A first sample after start books, on the lower reading, its rise over the largest earlier value of its chain it
    could continue (nothing without a baseline), on the upper from 0; the amounts where they differ are listed."""
    out, unproven = defaultdict(float), []
    for birth, chain, pts, new in lane:
        first_ts, first = pts[0]
        others = [p for b, c, p, _ in lane if c == chain and b <= birth and p is not pts]
        prior = [v[-1] for v in ([v for t, v in p if t <= first_ts] for p in others) if v]
        if first_ts <= start:
            low = high = None
        elif others:
            fit = [v for v in prior if v <= first]
            low, high = (first if len(prior) < len(others) else max(fit, default=0.0)), 0.0
        else:
            low, high = (0.0, 0.0) if new else (first, 0.0)
        if low:
            unproven.append(("continuation or rebuild" if others else "baseline", low))
        until = min((b for b, *_ in lane if b > birth), default=math.inf)
        for ts, inc in _series_increments(pts, start, high if upper else low).items():
            if ts <= until or upper:
                out[ts] += inc
            if ts > until and inc:
                unproven.append(("after a later session started", inc))
    return out, unproven


def _series_total(samples, start, born_after):
    """Increase of one session-scoped counter over the window, or None when it is unknown: no finite sample, or a
    first sample after start for a counter that existed before it."""
    if not samples or (samples[0][0] > start and not born_after):
        return None
    return sum(_series_increments(samples, start, 0.0 if born_after else None).values())


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
    return {"A_bad": 0.0, "A_exp": 0, "A_loss": 0, "A_terms": defaultdict(float), "V_freeze": 0.0, "V_exp_s": 0.0, "Q_bad": 0, "Q_exp": 0,
            "L": [], "stale": []}


def _dimension_values(acc):
    vals = {}
    if acc["A_exp"]:
        vals["A"] = acc["A_bad"] / acc["A_exp"]
        if acc["A_terms"]:
            vals["A_terms"] = {k: v / acc["A_exp"] for k, v in sorted(acc["A_terms"].items())}
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


def _reelection_increase(series, sessions, start, linger, upper):
    """(booked, unproven) for one re-election result of one user. The count is a page-wide static, exported once
    non-zero, and a re-election that proceeds opens a new session; so a series that appears after its session's
    first report counts from 0, and one present from it continues the previous session when the user's sessions
    are sequential (a lower value is a reload). With tied or concurrent sessions (two tabs) the lower reading
    continues the largest earlier value it could and the upper reading counts it from 0."""
    spans = {s: (ts[0], ts[-1]) for s, ts in sessions.items() if ts}
    booked = unproven = 0.0
    for sid, pts in series.items():
        birth = spans.get(sid, (pts[0][0],))[0]
        first_ts, first = pts[0]
        base = None
        if start < first_ts and birth < first_ts:
            base = 0.0
        elif start < first_ts:
            chain = sorted(sp for s, sp in spans.items() if sp[0] <= birth)
            sequential = len(chain) > 1 and all(a[0] < b[0] and a[1] <= b[0] + linger
                                                for a, b in zip(chain, chain[1:]))
            pred = next((s for s, sp in spans.items() if sequential and sp == chain[-2]), None)
            held = [v for t, v in series.get(pred, ()) if t <= first_ts]
            prev = held[-1] if held else 0.0
            if sequential:
                base = prev if first >= prev else 0.0
            else:
                earlier = [s for s, sp in spans.items() if s != sid and sp[0] <= birth]
                held = [([v for t, v in series.get(s, ()) if t <= first_ts] or [0.0])[-1] for s in earlier]
                low = max((v for v in held if v <= first), default=0.0) if earlier else first
                unproven += low
                base = 0.0 if upper else low
        booked += sum(_series_increments(pts, start, base).values())
    return booked, unproven


def _stability(data, user, user2sess, step, part, away=(), filled=(), rejoins=0, upper=False):
    hs, he = step["hold_start"], step["hold_end"]
    sessions = _sessions_of(data, user)
    new_in_hold = sum(1 for ts in sessions.values() if ts and hs < ts[0] <= he)
    initial_join_in_hold = 1 if (part.get("join_ts") or hs) > hs else 0
    by_result = defaultdict(list)
    for labels, samples in data.get(M_REELECT, []):
        if labels.get("session_id") in user2sess.get(user, set()):
            by_result[labels.get("result", "")].append((labels, [(t, v) for t, v in samples if t <= he]))
    reelect, doubt = {}, {}
    for r, group in by_result.items():
        reelect[r], doubt[r] = _reelection_increase(
            {sid: sorted(smp.items()) for (sid, _, _), smp in _index(group).items() if smp}, sessions, hs,
            METRICS_SESSION_TIMEOUT_S + step.get("scrape_step_s", 15), upper)
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
        "reelections_unproven": {r: v for r, v in sorted(doubt.items()) if v},
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


def _transport(data, sessions, hs):
    if isinstance(data.get(M_RTT), FailedQuery):
        return FAILED
    counts = defaultdict(int)
    for labels, samples in data.get(M_RTT, []):
        n = sum(1 for t, _ in samples if t >= hs)
        if labels.get("session_id") in sessions and n:
            counts[labels.get("server_type") or "unknown"] += n
    return max(counts, key=counts.get) if counts else "unknown"


def _pair_increments(series, sess2user, keep, births, start, conflicts, unproven, upper):
    """(receiver, publisher) -> {ts: increase}. A per-pair client counter lives in the receiver's decoder for one
    publisher session, which may survive the receiver's session change or be rebuilt, so series chain by to_peer."""
    by_cause = defaultdict(list)
    for labels, samples in series:
        by_cause[labels.get("cause", "")].append((labels, samples))
    out = defaultdict(lambda: defaultdict(float))
    for group in by_cause.values():
        lanes = defaultdict(list)
        for (sid, recv, to_peer), samples in _index(group, conflicts).items():
            if keep(recv) and samples:
                pts = sorted(samples.items())
                birth = max(births.get(s, (pts[0][0],))[0] for s in (sid, to_peer))
                lanes[(recv, sess2user.get(to_peer))].append((birth, sid, to_peer, pts))
        for key, lane in lanes.items():
            incs, doubts = _lane_increments(
                [(birth, to_peer, pts, any(births.get(s, (-math.inf,))[0] > start for s in (sid, to_peer)))
                 for birth, sid, to_peer, pts in lane], start, upper)
            for ts, inc in incs.items():
                out[key][ts] += inc
            out[key]
            unproven[key] += doubts
    return out


def _correlated_freezes(freeze_inc, fps_tl, receivers, hs, share, min_pairs):
    """Per receiver, the scrape buckets in which at least `share` of its camera pairs that decoded during the hold
    accrued freeze time; pairs that never decoded neither count nor dilute."""
    out = {}
    for r in sorted(receivers):
        inc = {k: v for k, v in freeze_inc.items() if k[0] == r and k[1]}
        total = sum(sum(v.values()) for v in inc.values())
        together, inside, worst, evaluated = [], 0.0, 0.0, 0
        live = [k for k in inc if any(v > 0 for t, v in fps_tl.get(k, {}).items() if t >= hs)]
        for ts in sorted({t for v in inc.values() for t in v}):
            if len(live) < min_pairs:
                continue
            evaluated += 1
            froze = [k for k in live if inc[k].get(ts, 0) > 0]
            worst = max(worst, len(froze) / len(live))
            if len(froze) >= share * len(live):
                together.append(ts)
                inside += sum(inc[k][ts] for k in froze)
        if inc:
            out[r] = {"buckets_evaluated": evaluated, "buckets_froze_together": together, "max_share_frozen": worst,
                      "freeze_s": total, "freeze_s_in_those_buckets": inside}
    return out


def _audio_delay(tls, keep, cap):
    """Per receiver over its talker pairs: p95 jitter-buffer level, share of samples with the target at the cap,
    mean accelerate ops/s. Receiver jitter-buffer levels, not mouth-to-ear delay."""
    vals = defaultdict(lambda: defaultdict(list))
    for name, tl in tls.items():
        for (r, pub), samples in tl.items():
            if keep(r, pub):
                vals[r][name] += list(samples.values())
    return {r: {"buffer_ms_p95": percentile(v[M_BUFFER], 95),
                "target_delay_at_cap_share": (sum(1 for x in v[M_TARGET] if x >= cap) / len(v[M_TARGET])
                                              if v[M_TARGET] else None),
                "accelerate_ops_per_sec_mean": sum(v[M_ACCEL]) / len(v[M_ACCEL]) if v[M_ACCEL] else None}
            for r, v in sorted(vals.items())}


def _sequential(a, b, linger):
    """b replaced a: it started later, and a ended within the metrics linger of b's start. Anything else (a tie,
    two tabs) is concurrent, and a transport change between them is not a provable switch."""
    return a[0] < b[0] and a[1] <= b[0] + linger


def _switches(rows, linger):
    """rows: [(from, to, server_type)] -> (proven moves [(from_type, to_type, at)], sessions started alongside a
    session on another transport). A change into a session is a switch only when every session that started before
    it forms one sequential chain with it; otherwise the transport it left is unproven. Independent of row order."""
    moves, unproven = [], 0
    for b in rows:
        earlier = sorted(r for r in rows if r[0] <= b[0] and r is not b)
        if not earlier:
            continue
        chain = earlier + [b]
        if all(_sequential(x, y, linger) for x, y in zip(chain, chain[1:])):
            if earlier[-1][2] != b[2]:
                moves.append((earlier[-1][2], b[2], b[0]))
        elif any(r[2] != b[2] and (not _sequential(r, b, linger) or r[0] == earlier[-1][0]) for r in earlier):
            unproven += 1
    return moves, unproven


def _transport_path(rows):
    """'websocket+webtransport -> webtransport': sessions opened in the same scrape form one group."""
    groups = defaultdict(set)
    for r in rows:
        groups[r["from"]].add(r["server_type"])
    path = ["+".join(sorted(g)) for _, g in sorted(groups.items())]
    return " -> ".join(p for i, p in enumerate(path) if not i or p != path[i - 1])


def _transport_timeline(data, sessions, part, hs, he, linger):
    """Each session's server_type over the step in start order, the switches between sequential sessions, the
    changes between concurrent ones (unproven), and what the bots-app record saw over the whole run."""
    recorded = ({"preferred": part.get("transport_preferred"), "observed": part.get("transport_observed"),
                 "fallbacks": part["transport_fallbacks"]} if "transport_fallbacks" in part else None)
    if isinstance(data.get(M_RTT), FailedQuery):
        return {"status": FAILED, "sessions": [], "switches": None, "switches_in_hold": None, "recorded": recorded}
    spans = defaultdict(list)
    for labels, samples in data.get(M_RTT, []):
        if labels.get("session_id") in sessions and samples:
            spans[(labels["session_id"], labels.get("server_type") or "unknown")] += [t for t, _ in samples if t <= he]
    rows = sorted(({"from": min(ts), "to": max(ts), "server_type": st, "session_id": sid}
                   for (sid, st), ts in spans.items() if ts), key=lambda r: (r["from"], r["to"], r["server_type"]))
    moves, unproven = _switches([(r["from"], r["to"], r["server_type"]) for r in rows], linger)
    return {"status": "measured", "sessions": rows, "path": _transport_path(rows), "switches": len(moves),
            "switches_in_hold": sum(1 for _, _, at in moves if hs < at),
            "switches_unproven": unproven, "recorded": recorded}


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
    """Scores the step under the lower reading of every counter whose lineage the samples cannot prove and, when
    there is one, under the upper reading too; a quality gate whose status differs between them is uncertain.
    The lower reading's values are the ones reported."""
    low = _score_step(manifest, step, data, cfg, mode, generator_verdict, split_transport, False)
    low["uncertain_gates"] = []
    if not low["diagnostics"]["counter_lineage_unproven"]:
        return low
    high = {g["gate"]: g for g in _score_step(manifest, step, data, cfg, mode, generator_verdict, split_transport,
                                               True)["quality_gates"]}
    gates = []
    for g in low["quality_gates"]:
        h = high.get(g["gate"], g)
        if h["status"] != g["status"]:
            g = dict(g, status="uncertain", detail=f"{g['status']} on the lower reading of unproven counters, "
                                                   f"{h['status']} on the upper (diagnostics.counter_lineage_unproven)"
                                                   + (f"; {g['detail']}" if g["detail"] else ""))
        gates.append(g)
    low["quality_gates"] = gates
    low["uncertain_gates"] = [g["gate"] for g in gates if g["status"] == "uncertain"]
    low["verdict"], low["invalid_reasons"] = _verdict(mode, low["validity"], gates, low["invalid_reasons"],
                                                      low["headline_cell"])
    return low


def _verdict(mode, validity, quality, blocking, headline_cell):
    """(verdict, invalid_reasons): a gate failing on both readings stands; an uncertain one only blocks a PASS."""
    uncertain = [g["gate"] for g in quality if g["status"] == "uncertain"]
    if mode == "real":
        return "REPORT", blocking
    if any(g["status"] == "fail" for g in validity) or blocking or headline_cell is None:
        return "INVALID", blocking
    if any(g["status"] == "fail" for g in quality):
        return "FAIL", blocking
    if uncertain:
        return "INVALID", uncertain
    return "PASS", blocking


def _score_step(manifest, step, data, cfg, mode, generator_verdict, split_transport, upper):
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
    transport = {u: _transport(data, user2sess.get(u, set()), hs) for u in observers}
    timelines_tr = {u: _transport_timeline(data, user2sess.get(u, set()), in_step[u], hs, he,
                                           METRICS_SESSION_TIMEOUT_S + step_s) for u in in_step}
    for u, tl in sorted(timelines_tr.items()):
        if tl["switches"]:
            flags.append(f"{u}: transport switched {tl['path']} (switches {tl['switches']}, in the hold "
                         f"{tl['switches_in_hold']})")
        if tl.get("switches_unproven"):
            flags.append(f"{u}: {tl['switches_unproven']} session(s) started alongside a session on another "
                         "transport; their transport changes are not counted as switches")

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
    stability = {u: _stability(data, u, user2sess, step, in_step[u], away[u], pipeline_gaps, rejoin_count[u], upper)
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

    unproven = {name: defaultdict(list) for name in AUDIO_COUNTERS + (M_FREEZE,)}
    increments = {name: _pair_increments(data.get(name, []), sess2user, lambda r: r in observers, births, hs, conflicts,
                                         unproven[name], upper) for name in AUDIO_COUNTERS + (M_FREEZE,)}
    bucket_ms = step_s * 1000.0
    w_cut, w_silent = cv(cfg, "dimensions", "discard_weight"), cv(cfg, "dimensions", "playout_underrun_weight")
    coverage = defaultdict(dict)
    for u in sorted(observers) if mode == "run" else ():
        for t in sorted(talkers - {u}):
            terms = coverage[u].setdefault(cell_of(u, t), {term: [0, 0] for term in AUDIO_TERM_NAMES.values()})
            for name in AUDIO_COUNTERS:
                terms[AUDIO_TERM_NAMES[name]][0] += (u, t) in increments[name]
                terms[AUDIO_TERM_NAMES[name]][1] += 1
    coverage = dict(coverage)
    short = {term: [sum(c[term][i] for cells in coverage.values() for c in cells.values()) for i in (0, 1)]
             for term in AUDIO_TERM_NAMES.values()}

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
                        a["A_terms"]["no_sample_or_not_listening"] += 1.0
                    else:
                        conceal, missing = min(ev / full, 1.0), max(0.0, 1.0 - pv / nominal_pps)
                        cut = w_cut * increments[M_DISCARDS].get((r, t), {}).get(ts, 0.0) * AUDIO_PACKET_MS / bucket_ms
                        silent = w_silent * increments[M_UNDERRUN].get((r, t), {}).get(ts, 0.0) / bucket_ms
                        bad = min(1.0, max(max(conceal, missing) + silent, cut))
                        a["A_terms"]["concealment" if conceal >= missing else "missing_packets"] += max(conceal, missing)
                        a["A_terms"]["discarded"] += cut
                        a["A_terms"]["playout_silence"] += silent
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

    for recv, pub in sorted(set(freeze_tl) | set(increments[M_FREEZE]), key=str):
        accrued = increments[M_FREEZE].get((recv, pub), {})
        delta = sum(accrued.values())
        fps = fps_tl.get((recv, pub), {})
        exposure = sum(max(accrued.get(t, 0.0), step_s if fps.get(t, 0) > 0 else 0.0)
                       for t in set(accrued) | {t for t in fps if hs < t <= he})
        a = acc[cell_of(recv, pub if pub in shaped else None)][recv]
        a["V_freeze"] += delta
        a["V_exp_s"] += exposure
        if delta > diag["max_freeze"][0]:
            diag["max_freeze"] = (delta, f"{recv} <- {pub}")

    stale_tl = timelines(idx[M_STALE])
    for (recv, pub), fps in sorted(fps_tl.items(), key=lambda kv: str(kv[0])):
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
    rtt_failed = isinstance(data.get(M_RTT), FailedQuery)
    if split_transport and rtt_failed:
        flags.append("--split-transport: no per-transport cells, the RTT query failed")
    elif split_transport:
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
            "transport_mix": FAILED if rtt_failed else _count(transport.get(u, "unknown") for u in per_user),
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
    verdict, _ = _verdict(mode, validity, quality, blocking_unmeasured, headline_key)

    return {
        "step_id": sid,
        "n_target": step["n_target"],
        "headline": step["headline"],
        "window": [hs, he],
        "join_start": step["join_start"],
        "verdict": verdict,
        "notices": ([LATENCY_PENDING] if latency_waived else [])
        + [f"dimension A: {term} measured for {got} of {want} observer-talker pairs; A leaves it out where it is "
           "missing" for term, (got, want) in short.items() if got < want]
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
        "transport_timeline": timelines_tr,
        "layer_mix": _layer_mix(data),
        "server_diagnostics": _server_diagnostics(data, hs, sess2user, births,
                                                  cv(cfg, "server_diagnostics", "throttle_pod_selector")),
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
            "audio_counter_coverage": coverage,
            "counter_lineage_unproven": sorted(f"{r} <- {p}: {name} {why} ({v:g})" for name, by in unproven.items()
                                               for (r, p), doubts in by.items() for why, v in doubts)
            + sorted(f"{u}: re-election {r} not provably from this hold ({v:g})" for u, st in stability.items()
                     for r, v in st["reelections_unproven"].items()),
            "correlated_freeze": _correlated_freezes(
                increments[M_FREEZE], fps_tl, observers, hs, cv(cfg, "diagnostics", "correlated_freeze_share"),
                cv(cfg, "diagnostics", "correlated_freeze_min_pairs")),
            "audio_delay": FAILED if any(isinstance(data.get(name), FailedQuery) for name in DELAY_METRICS)
            else _audio_delay({name: timelines(_index(data.get(name, []))) for name in DELAY_METRICS},
                              lambda r, pub: mode == "real" or pub in talkers, cv(cfg, "diagnostics", "target_delay_cap_ms")),
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
    series = data.get(M_ACTIVE_LAYERS, [])
    layers = FAILED if isinstance(series, FailedQuery) else percentile([v for _, smp in series for _, v in smp], 50)
    return {"decoded_fps_p50": percentile(fps, 50), "publisher_active_video_layers_p50": layers}


def _sum_delta(series, start):
    return sum(counter_delta(s, start) for _, s in series) if series else None


def _relay_counter(series, start, label, always, transports, room_failed):
    """Hold increase by 'transport[/label]'. A relay counter exports nothing until its first increment, so an
    absent combination reads 0 only on a transport that carried this room AND exports the counter (another
    combination of it has a series); otherwise absence cannot be told from an image without the counter."""
    if isinstance(series, FailedQuery):
        return {"status": FAILED}
    series = [(labels, samples) for labels, samples in series if samples]
    by = defaultdict(float)
    for labels, samples in series:
        by["/".join([labels.get("transport", "?")] + ([labels.get(label, "?")] if label else []))] += \
            counter_delta(samples, start)
    exported = {labels.get("transport") for labels, _ in series}
    silent = [t for t in transports if t not in exported]
    for t in set(transports) & exported:
        for v in always:
            by.setdefault(f"{t}/{v}", 0.0)
    if room_failed:
        detail = "relay_active_sessions_per_room query failed: zero-fill skipped"
    elif silent:
        detail = f"no series on {', '.join(silent)}: never incremented, or not exported by that relay's image"
    else:
        detail = ""
    if not by:
        return {"status": FAILED if room_failed else NOT_COLLECTED, "detail": detail or "no series"}
    return {"status": "measured", "by": dict(sorted(by.items()))} | ({"detail": detail} if detail else {})


def _quic(data, start, sess2user, births, wt_in_room):
    """Per-session QUIC path stats. lost, sent and congestion events are cumulative gauges GC'd at session end:
    each session adds its own increase, a gauge without a series stays unknown (None), and a session whose first
    sample comes after start although it was born before has an unknown baseline for that sample."""
    if any(isinstance(data.get(name), FailedQuery) for name in QUIC):
        return {"status": FAILED}
    if not any(data.get(name) for name in QUIC):
        return {"status": NOT_COLLECTED, "detail": "relay QUIC path series absent for this room"
                + ("" if wt_in_room else " (no webtransport session of this room was scraped)")}
    per = defaultdict(lambda: {"rtt": [], "lost": [], "sent": [], "congestion_events": []})
    for name, field in (("_quic_lost", "lost"), ("_quic_sent", "sent"), ("_quic_congestion", "congestion_events")):
        for labels, samples in data.get(name, []):
            sid = labels.get("session_id", "?")
            per[sid][field].append(_series_total(samples, start, births.get(sid, (-math.inf,))[0] > start))
    for labels, samples in data.get("_quic_rtt", []):
        per[labels.get("session_id", "?")]["rtt"] += [v for t, v in samples if t >= start]
    by_user = defaultdict(list)
    for sid, st in sorted(per.items()):
        row = {k: (None if not v or None in v else sum(v)) for k, v in st.items() if k != "rtt"}
        row["loss_fraction"] = row["lost"] / row["sent"] if row["lost"] is not None and row["sent"] else None
        by_user[sess2user.get(sid, "?")].append({"session_id": sid, "rtt_ms_median": percentile(st["rtt"], 50), **row})
    rows = [st for sessions in by_user.values() for st in sessions]
    both = [st for st in rows if st["lost"] is not None and st["sent"] is not None]
    medians = [st["rtt_ms_median"] for st in rows if st["rtt_ms_median"] is not None]
    lost, sent = sum(st["lost"] for st in both), sum(st["sent"] for st in both)
    events = [st["congestion_events"] for st in rows if st["congestion_events"] is not None]
    return {"status": "measured", "rtt_ms_median": percentile(medians, 50), "rtt_ms_p95": percentile(medians, 95),
            "lost": lost if both else None, "sent": sent if both else None,
            "loss_fraction": lost / sent if sent else None, "loss_sessions": [len(both), len(rows)],
            "congestion_events": sum(events) if events else None, "by_user": dict(by_user)}


def _throttling(series, pods):
    """CFS throttled share of periods for the target's relay and NATS pods only."""
    if not pods:
        return {"status": NOT_COLLECTED, "detail": "no relay and NATS pod selector for this target"}
    if isinstance(series, FailedQuery):
        return {"status": FAILED, "selector": pods}
    by_pod = {labels.get("pod", "?"): {"max": max(v for _, v in smp), "mean": sum(v for _, v in smp) / len(smp)}
              for labels, smp in series if smp}
    if not by_pod:
        return {"status": NOT_COLLECTED, "selector": pods, "detail": "the selector matched no CFS series"}
    return {"status": "measured", "selector": pods, "max": max(p["max"] for p in by_pod.values()),
            "by_pod": dict(sorted(by_pod.items()))}


def _server_diagnostics(data, start, sess2user, births, pods):
    room_sessions = data.get("_room_sessions", [])
    room_failed = isinstance(room_sessions, FailedQuery)
    in_room = {labels["transport"] for labels, smp in room_sessions
               if labels.get("transport") and any(v > 0 for _, v in smp)}
    out = {name: _relay_counter(data.get(name, []), start, label, always, sorted(in_room & set(emits)), room_failed)
           for name, (_, _, label, always, emits) in RELAY_COUNTERS.items()}
    drops = data.get("_session_drops", [])
    out["session_drops"] = _relay_counter(drops, start, "kind", ("audio",), sorted(in_room), room_failed)
    if out["session_drops"]["status"] == "measured":
        by_user = defaultdict(lambda: defaultdict(float))
        for labels, samples in [(lb, smp) for lb, smp in drops if smp]:
            by_user[sess2user.get(labels.get("session_id"), "?")][labels.get("kind", "?")] += \
                counter_delta(samples, start)
        out["session_drops"]["by_user"] = {u: dict(sorted(k.items())) for u, k in sorted(by_user.items())}
    le50, count = data.get("sched_lag_le50", []), data.get("sched_lag_count", [])
    if isinstance(le50, FailedQuery) or isinstance(count, FailedQuery):
        out["scheduler_late_fraction_gt_50ms"] = {"status": FAILED}
    else:
        le, n = _sum_delta(le50, start), _sum_delta(count, start)
        late = 1 - le / n if le is not None and n else None
        out["scheduler_late_fraction_gt_50ms"] = ({"status": "measured", "value": late}
                                                  if late is not None and math.isfinite(late)
                                                  else {"status": NOT_COLLECTED})
    out["quic"] = _quic(data, start, sess2user, births, "webtransport" in in_room)
    out["cpu_throttling"] = _throttling(data.get("_cfs", []), pods)
    out["room_transports"] = FAILED if room_failed else sorted(in_room)
    out["note"] = ("session_drops and quic are this room's; the other relay counters are relay-wide, so they are "
                   "attributable to this run only while no other room used the relay. An absent counter combination "
                   "reads 0 only on a transport of this room whose relay exports the counter. Reported, never scored "
                   "or gated.")
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


def _fallback_reconciliation(steps, linger):
    """Per participant with a bots-app record: its run-wide fallbacks against the switches away from its preferred
    transport that the health reports show across every scored window."""
    out = {}
    users = {u for s in steps for u, tl in s["transport_timeline"].items() if tl.get("recorded")}
    for u in sorted(users):
        rec = next(s["transport_timeline"][u]["recorded"] for s in steps if u in s["transport_timeline"])
        spans, failed = {}, False
        for s in steps:
            tl = s["transport_timeline"].get(u, {})
            failed |= tl.get("status") == FAILED
            for r in tl.get("sessions", []):
                k = (r["session_id"], r["server_type"])
                lo, hi = spans.get(k, (r["from"], r["to"]))
                spans[k] = (min(lo, r["from"]), max(hi, r["to"]))
        rows = sorted((lo, hi, st) for (_, st), (lo, hi) in spans.items())
        pref = rec["preferred"]
        concurrent = not all(_sequential(a, b, linger) for a, b in zip(rows, rows[1:]))
        seen = (None if failed or concurrent or not rows or pref not in ("webtransport", "websocket") else
                sum(1 for a, b, _ in _switches(rows, linger)[0] if a == pref and b in ("webtransport", "websocket")))
        out[u] = dict(rec, fallbacks_in_health_reports=seen)
    return out


def score_run(manifest, datasets, cfg, mode="run", generator_verdict=None, split_transport=False):
    steps = [score_step(manifest, s, datasets[s["step_id"]], cfg, mode, generator_verdict, split_transport)
             for s in manifest["steps"]]
    headline = next(s for s in steps if s["headline"])
    fallbacks = _fallback_reconciliation(steps, METRICS_SESSION_TIMEOUT_S + cv(cfg, "sampling", "scrape_step_s"))
    unseen = sum(1 for f in fallbacks.values()
                 if f["fallbacks_in_health_reports"] is not None and f["fallbacks"] > f["fallbacks_in_health_reports"])
    return {
        "schema": "call-quality-score/v0",
        "run": {"run_id": manifest["run_id"], "meeting_id": manifest["meeting_id"],
                "environment": manifest["environment"], "commit": manifest["code"]["commit"],
                "bands_version": cfg.get("config_version"), "mode": mode,
                "scrape_step_s": cv(cfg, "sampling", "scrape_step_s")},
        "verdict": headline["verdict"],
        "notices": sorted({n for s in steps for n in s["notices"]}
                          | ({f"transport: {unseen} participant(s) whose bots-app record counts fallbacks from the "
                              "preferred transport that their health reports in the scored windows do not show"}
                             if unseen else set())),
        "transport_fallbacks": fallbacks,
        "config_overrides": config_overrides(cfg),
        "headline_step": headline["step_id"],
        "steps": steps,
    }
