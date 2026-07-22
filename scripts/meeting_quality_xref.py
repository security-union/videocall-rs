#!/usr/bin/env python3
"""meeting_quality_xref — deep investigative meeting-analysis tool for videocall-rs.

This is the Phase-2 "deep" layer that sits behind scripts/parse_meeting_console_logs.sh
(the quick-triage front end). Where the bash script emits COUNTS, this tool builds
produce/consume timelines, cross-references them with the relay pod split and
Prometheus, and runs a deterministic ANOMALY RULE ENGINE (R1-R14) that reproduces
findings which previously took hours of manual grepping.

Spec: ~/work/notebook/videocall-rs/plans/2026-06-26-meeting-quality-xref-tool-spec.md
README: scripts/meeting_quality_xref.README.md

Design guard-rails (each encodes a real mistake — see §3 of the spec):
  1. Prometheus is ALWAYS anchored at the meeting epoch (`@ <end_epoch>`), never "now".
     A near-empty result WARNS ("did you anchor at the meeting epoch?"), never concludes
     "no data" — per-peer client GaugeVecs go stale ~5 min after the meeting.
  2. navigator.connection (preamble `network=`) is UNRELIABLE — printed but never the sole
     basis for a bandwidth finding (R9 is a guard, not a rule).
  3. WT and WS are TWO separate ChatServer instances ALWAYS. Pod assignment is a
     first-class column; #1202 cross-pod base-pin bites at replicaCount:1.
  4. LAYER_SWITCH (rendered) != LAYER_PREFERENCE (requested). SWITCH is authoritative for
     "what a receiver got"; PREFERENCE is intent (rate-limited/capped).
  5. Counters need deltas (increase()); head_age is UNBOUNDED; media_kind="camera" for
     encoder_active_layers but ="video" for received_layer.
  6. NEGATIVE claims ("X never happens") are backed by a real scan, not an assumption.
  7. Client logs truncate early — detect per-user last-ts; don't read "events stopped" as
     "behaviour stopped". Corroborate duration with Prometheus (full window).
  8. IBA naming: never a country; use names / "IBA team".

Stdlib only. Python 3.8+.
"""

import argparse
import gzip
import json
import math
import os
import re
import statistics
import subprocess
import sys
import urllib.parse
import urllib.request
from collections import Counter, defaultdict
from datetime import datetime, timezone

# ===========================================================================
# Environment configuration (cluster kubeconfig + Prometheus endpoint/auth)
# ===========================================================================
ENVIRONMENTS = {
    "hcl-daily": {
        "kubeconfig": os.path.expanduser("~/vc-k3s-config.yaml"),
        "namespace": "videocall",
        "api_instance": "videocall-api",
        "wt_instance": "videocall-webtransport",
        "ws_instance": "videocall-websocket",
        "prom_url": "https://prometheus.videocall.fnxlabs.com",
        "prom_auth": None,  # no auth
    },
    "ascend": {
        # conceptcar7 / CC7. NOTE: ~/ascend.yaml is STALE — use ascend-cluster-config.
        "kubeconfig": os.path.expanduser("~/ascend/ascend-cluster-config"),
        "namespace": "videocall",
        "api_instance": "videocall-api",
        "wt_instance": "videocall-webtransport",
        "ws_instance": "videocall-websocket",
        "prom_url": "https://prometheus.ops.ascend.fnxlabs.com",
        "prom_auth": ("env", "ASCEND_OPS_USER", "ASCEND_OPS_PASSWORD"),  # from ~/.ascend-env
    },
}

# ===========================================================================
# Rule thresholds (tunable — each is a documented default from the spec)
# ===========================================================================
TH = {
    "periodic_min_events": 6,          # R2: need >= this many to call something periodic
    "periodic_modal_frac": 0.60,       # R2: >= this fraction of inter-arrivals share one gap
    "periodic_cv": 0.15,               # R2: OR stdev/mean below this (single-cadence case)
    "oscillation_shed_count": 5,       # R3: > this many sheds in the meeting => oscillation
    "send_queue_bytes": 500_000,       # R4: WS buffered_amount over this => uplink saturation
    "concealment_pct": 15.0,           # R5: audio_concealment_pct over this => audible breakup
    "protective_cycles": 10,           # R6: > this many ENTERED/EMERGENCY => thrash
    "freshness_head_age_ms": 5000,     # R7: held-last-good head_age over this => bad freeze
    "freshness_skip_count": 20,        # R7: > this many freshness_skips for a sender => starvation
    "high_rtt_ms": 200.0,              # R14: baseline/active RTT over this => high-RTT env
    "low_cores": 4,                    # R13: cores < this => main-thread-stall risk
}

SEVERITY_ORDER = {"CRITICAL": 0, "HIGH": 1, "MEDIUM": 2, "LOW": 3, "INFO": 4}


# ===========================================================================
# Time helpers
# ===========================================================================
def iso_to_epoch(ts):
    """ISO8601 Z -> epoch seconds (float)."""
    try:
        return datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
    except Exception:
        return None


def fmt_ts(epoch):
    if epoch is None:
        return "??:??:??"
    return datetime.fromtimestamp(epoch, tz=timezone.utc).strftime("%H:%M:%S")


def fmt_clock(epoch):
    if epoch is None:
        return "????-??-?? ??:??:??Z"
    return datetime.fromtimestamp(epoch, tz=timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")


def promql_label(val):
    r"""Escape a string for use inside a PromQL double-quoted label matcher.
    A display_name containing `"` or `\` would otherwise produce malformed PromQL."""
    return str(val).replace("\\", "\\\\").replace('"', '\\"')


# ===========================================================================
# Log line parsers — every pattern is a free-text phrase in the `msg` field.
# Ported / extended from scripts/parse_meeting_console_logs.sh PATTERN INVENTORY.
# When a client emitter changes, UPDATE THESE in the same PR.
# ===========================================================================
RE = {
    "elected": re.compile(r"Elected connection (ws_0|wt_0)"),
    "session_assigned": re.compile(r"SESSION_ASSIGNED received on connection \w+:\s*(\d+)"),
    "participant": re.compile(r"PARTICIPANT_JOINED.*?display_name=([^,]*), session=(\d+)"),
    # AQ_STATUS: video_tier=medium(4) audio_tier=high(0) target_fps=0 target_bitrate=400
    #            encoder_queue_depth=0 active_layers=1 union_cap=none
    "aq_status": re.compile(
        r"AQ_STATUS:\s*video_tier=(\w+)\((\d+)\)\s*audio_tier=(\w+)\((\d+)\)"
        r".*?target_bitrate=(\d+).*?active_layers=(\d+)\s*union_cap=(\w+)"
    ),
    # LAYER_SWITCH session_id=N kind=video from=0 to=1 site=tick constrained=false highest_available=1
    "layer_switch": re.compile(
        r"LAYER_SWITCH session_id=(\d+) kind=(\w+) from=(\d+) to=(\d+)"
        r" site=(\w+) constrained=(\w+) highest_available=(\d+)"
    ),
    # LAYER_SWITCH_FRESHNESS_SKIP session_id=N kind=video ms_since_switch=1488 head_age_ms=1808 ...
    "layer_switch_freshness": re.compile(
        r"LAYER_SWITCH_FRESHNESS_SKIP session_id=(\d+) kind=(\w+) ms_since_switch=(\d+) head_age_ms=(\d+)"
    ),
    # Sending LAYER_PREFERENCE packet: first 2 of 2 entry(ies): [(NNN, Video, 1), (MMM, Video, 1)]
    "layer_pref": re.compile(r"Sending LAYER_PREFERENCE packet:.*?\[(.*)\]\s*$"),
    "layer_pref_tuple": re.compile(r"\((\d+),\s*(\w+),\s*(\d+)\)"),
    # Simulcast layer change: active 1->2 (reason=restore) | [0] 480x360 ~400kbps ACTIVE | ...
    "simulcast": re.compile(r"Simulcast layer change: active (\d+)->(\d+) \(reason=([a-z-]+)\)"),
    # [JITTER_BUFFER] freshness_skip A->B: head_age=1806ms dropped=1 keyframe_seq=none (held last-good) escalated=false
    "freshness": re.compile(
        r"freshness_skip (\d+)->(\d+):\s*head_age=(\d+)ms.*?keyframe_seq=(\S+).*?escalated=(\w+)"
    ),
    "keyframe_req": re.compile(r"Sending KEYFRAME_REQUEST to .*?\(session (\d+)\) for (\w+)"),
    # Host render: video=true prev=false mic=true prev=false screen=false prev=false
    "host_render": re.compile(
        r"Host render: video=(\w+) prev=\w+ mic=(\w+) prev=\w+ screen=(\w+)"
    ),
    "camera_on": re.compile(r"Host render: camera ON|CameraEncoder::start"),
    "camera_off": re.compile(r"camera OFF|CameraEncoder: stopped"),
    # ProtectiveMode: ENTERED trigger=audio_buffer median_fps=44.0 ... audio_buffer_ms=729 ...
    "protective": re.compile(
        r"ProtectiveMode:\s*(ENTERED|EXITED|EMERGENCY)(?:\s+trigger=(\w+))?"
        r"(?:.*?audio_buffer_ms=(\d+))?"
    ),
    "audio_tier": re.compile(r"audio tier updated to '(\w+)' \((\d+)kbps"),
    "mic_layers": re.compile(r"MicrophoneEncoder: effective audio (?:simulcast )?layers = (\d+)"),
    "congestion_ceiling": re.compile(r"congestion ceiling (?:cut to|->) (\d+)"),
    # Updated audio health (buffer: 660ms) for peer: 12175... (from current_user)
    "audio_health": re.compile(r"audio health \(buffer:\s*(\d+)ms\) for peer:\s*(\d+)"),
    # WebSocket backpressure: dropping 667 byte packet (buffered: 1048955 bytes, threshold: 1048576 bytes)
    "ws_backpressure": re.compile(
        r"WebSocket backpressure: dropping (\d+) byte packet \(buffered:\s*(\d+) bytes"
    ),
    "uplink_sat": re.compile(r"uplink saturation detected.*?(\d+) slow"),
    "decode_visibility": re.compile(r"Peer (\d+) decode visibility changed:\s*(\w+) -> (\w+)"),
    "baseline_rtt": re.compile(r"Baseline RTT for re-election monitoring:\s*([\d.]+)ms"),
    "reelection": re.compile(r"RTT degradation threshold reached|Re-election triggered"),
    "connection_lost": re.compile(r"Connection lost|No valid connections"),
    "capability_ceiling": re.compile(r"simulcast capability ceiling:\s*(\d+) layer"),
    # SIMULCAST: publishing 3 video layers (default ON) ...
    "publish_layers": re.compile(r"publishing (\d+) video layers"),
    # CPU-overload watchdog: main-thread drift 539ms exceeded threshold 500ms — ...
    "cpu_watchdog": re.compile(r"CPU-overload watchdog: main-thread drift (\d+)ms exceeded"),
    # preamble fields
    "pre_cores": re.compile(r"cores=(\d+)"),
    "pre_mem": re.compile(r"memory=([^;]+)"),
    "pre_cap": re.compile(r"capability_score=(\d+)"),
    "pre_net": re.compile(r"network=([^;]+)"),
    "pre_gpu": re.compile(r"gpu=([^;]+)"),
    "pre_platform": re.compile(r"platform=([^;(]+)"),
    "pre_appver": re.compile(r"appVersion=([^;]+)"),
    "pre_disp": re.compile(r"displayName=([^;]+)"),
    "pre_battery": re.compile(r"battery=([^;]+)"),
}


# ===========================================================================
# Data model
# ===========================================================================
class Participant:
    def __init__(self, email):
        self.email = email
        self.display_name = None
        self.own_sessions = set()      # SESSION_ASSIGNED ids (own)
        self.pods = []                 # [(epoch, "wt"|"ws")]
        self.specs = {}                # preamble dict
        self.first_ts = None
        self.last_ts = None
        self.events = []               # normalized events (dicts)

    @property
    def pod(self):
        """Dominant pod (last elected wins; that is the live transport)."""
        return self.pods[-1][1] if self.pods else None

    @property
    def own_session(self):
        return next(iter(self.own_sessions)) if self.own_sessions else None


class Meeting:
    def __init__(self, room, date, env):
        self.room = room
        self.date = date
        self.env = env
        self.participants = {}          # email -> Participant
        self.session_to_name = {}       # session_id(str) -> display_name (global)
        self.session_to_email = {}      # session_id(str) -> email (own sessions only)
        self.first_ts = None
        self.last_ts = None

    def name_for(self, sid):
        sid = str(sid)
        return self.session_to_name.get(sid, f"session:{sid[:6]}…")


# ===========================================================================
# Log loading
# ===========================================================================
def discover_users(log_dir):
    users = {}
    for fn in os.listdir(log_dir):
        if not fn.endswith(".log.gz"):
            continue
        m = re.match(r"(.+?)_(\d+)_(\d+)\.log\.gz$", fn)
        if not m:
            continue
        email, _session_ms, _chunk = m.group(1), m.group(2), m.group(3)
        users.setdefault(email, []).append(fn)
    # Sort by (session_ms, chunk) NUMERICALLY, not lexicographically. Chunk suffixes are
    # zero-padded to 5 digits today (…_00010 sorts fine as a string), but the regex accepts
    # unpadded numbers, and load order feeds `Participant.pod` (last-appended election wins) →
    # a wrong order could yield a stale pod and mis-fire the R1 cross-pod diagnosis. Numeric
    # keying is correct regardless of padding.
    def _key(fn):
        mm = re.match(r"(.+?)_(\d+)_(\d+)\.log\.gz$", fn)
        return (int(mm.group(2)), int(mm.group(3))) if mm else (0, 0)
    for email in users:
        users[email].sort(key=_key)
    return users


def parse_preamble(msg):
    out = {}
    for key, rx in (
        ("cores", "pre_cores"), ("memory", "pre_mem"), ("capability_score", "pre_cap"),
        ("network", "pre_net"), ("gpu", "pre_gpu"), ("platform", "pre_platform"),
        ("appVersion", "pre_appver"), ("displayName", "pre_disp"), ("battery", "pre_battery"),
    ):
        m = RE[rx].search(msg)
        if m:
            out[key] = m.group(1).strip()
    return out


def load_participant(meeting, email, files, log_dir):
    p = Participant(email)
    for fn in files:
        path = os.path.join(log_dir, fn)
        try:
            fh = gzip.open(path, "rt", errors="replace")
        except Exception as e:
            sys.stderr.write(f"WARN: cannot open {fn}: {e}\n")
            continue
        with fh:
            # A truncated/partial .log.gz (tab-closed mid-flush, or a disk-full write on the
            # log-writer — see the 2026-07-15 ENOSPC event) ends before its DEFLATE
            # end-of-stream marker. `errors="replace"` only covers text decode, NOT a
            # truncated compressed stream, so iterating raises EOFError/BadGzipFile from the C
            # layer. Accumulate line-by-line so the lines decompressed before the bad tail are
            # kept, then WARN and move on — one bad chunk must not abort the whole analysis
            # (guard-rail #7: truncation is expected, not fatal).
            lines = []
            fh_iter = iter(fh)
            while True:
                try:
                    lines.append(next(fh_iter))
                except StopIteration:
                    break
                except (EOFError, OSError, gzip.BadGzipFile) as e:
                    sys.stderr.write(
                        f"WARN: truncated/corrupt gzip {fn} ({e}); using {len(lines)} lines read before the bad tail\n"
                    )
                    break
            for line in lines:
                line = line.strip()
                if not line:
                    continue
                try:
                    o = json.loads(line)
                except Exception:
                    continue
                msg = o.get("msg", "")
                epoch = iso_to_epoch(o.get("ts", ""))
                level = o.get("level", "")
                if epoch is not None:
                    p.first_ts = epoch if p.first_ts is None else min(p.first_ts, epoch)
                    p.last_ts = epoch if p.last_ts is None else max(p.last_ts, epoch)
                if level == "preamble":
                    p.specs = parse_preamble(msg)
                    if p.specs.get("displayName"):
                        p.display_name = p.specs["displayName"]
                    continue
                # A line with an unparseable/missing ts can't be placed on a timeline or sorted
                # (guard-rail #7: truncated/corrupted gzip tails produce these). Identity-bearing
                # lines (SESSION_ASSIGNED, PARTICIPANT_JOINED, Elected) are still worth recording
                # even without a ts, so let those through; everything else needs a real ts.
                if epoch is None and not (
                    "SESSION_ASSIGNED" in msg or "PARTICIPANT_JOINED" in msg
                    or "Elected connection" in msg
                ):
                    continue
                _classify(meeting, p, epoch, msg)
    return p


def _ev(p, epoch, kind, **fields):
    e = {"ts": epoch, "kind": kind}
    e.update(fields)
    p.events.append(e)
    return e


def _classify(meeting, p, epoch, msg):
    """Match a single log line against all extractors -> normalized events."""
    m = RE["elected"].search(msg)
    if m:
        pod = "wt" if m.group(1) == "wt_0" else "ws"
        p.pods.append((epoch, pod))
        return
    m = RE["session_assigned"].search(msg)
    if m:
        p.own_sessions.add(m.group(1))
        meeting.session_to_email[m.group(1)] = p.email
        return
    m = RE["participant"].search(msg)
    if m:
        name, sid = m.group(1).strip(), m.group(2)
        # display_name in PARTICIPANT_JOINED is often a short name; keep first seen.
        meeting.session_to_name.setdefault(sid, name)
        return
    m = RE["aq_status"].search(msg)
    if m:
        union = m.group(7)
        _ev(p, epoch, "aq",
            video_tier=m.group(1), video_tier_n=int(m.group(2)),
            audio_tier=m.group(3), target_bitrate=int(m.group(5)),
            active_layers=int(m.group(6)),
            union_cap=(None if union == "none" else int(union)))
        return
    m = RE["layer_switch"].search(msg)
    if m:
        _ev(p, epoch, "layer_switch",
            sender=m.group(1), media=m.group(2),
            frm=int(m.group(3)), to=int(m.group(4)), site=m.group(5),
            constrained=(m.group(6) == "true"), highest_available=int(m.group(7)))
        return
    m = RE["layer_switch_freshness"].search(msg)
    if m:
        _ev(p, epoch, "layer_switch_freshness",
            sender=m.group(1), media=m.group(2),
            ms_since_switch=int(m.group(3)), head_age_ms=int(m.group(4)))
        return
    m = RE["layer_pref"].search(msg)
    if m:
        tuples = [(t[0], t[1], int(t[2])) for t in RE["layer_pref_tuple"].findall(m.group(1))]
        _ev(p, epoch, "layer_pref", entries=tuples)
        return
    m = RE["simulcast"].search(msg)
    if m:
        _ev(p, epoch, "simulcast",
            frm=int(m.group(1)), to=int(m.group(2)), reason=m.group(3))
        return
    m = RE["freshness"].search(msg)
    if m:
        _ev(p, epoch, "freshness",
            receiver=m.group(1), sender=m.group(2),
            head_age_ms=int(m.group(3)),
            keyframe_seq=m.group(4), escalated=(m.group(5) == "true"))
        return
    m = RE["keyframe_req"].search(msg)
    if m:
        _ev(p, epoch, "keyframe_req", sender=m.group(1), media=m.group(2))
        return
    m = RE["host_render"].search(msg)
    if m:
        _ev(p, epoch, "host_render",
            video=(m.group(1) == "true"), mic=(m.group(2) == "true"),
            screen=(m.group(3) == "true"))
        return
    m = RE["protective"].search(msg)
    if m:
        _ev(p, epoch, "protective",
            state=m.group(1), trigger=m.group(2),
            audio_buffer_ms=(int(m.group(3)) if m.group(3) else None))
        return
    m = RE["audio_tier"].search(msg)
    if m:
        _ev(p, epoch, "audio_tier", tier=m.group(1), kbps=int(m.group(2)))
        return
    m = RE["mic_layers"].search(msg)
    if m:
        _ev(p, epoch, "mic_layers", layers=int(m.group(1)))
        return
    m = RE["audio_health"].search(msg)
    if m:
        _ev(p, epoch, "audio_health", buffer_ms=int(m.group(1)), peer=m.group(2))
        return
    m = RE["ws_backpressure"].search(msg)
    if m:
        _ev(p, epoch, "ws_backpressure",
            dropped_bytes=int(m.group(1)), buffered=int(m.group(2)))
        return
    m = RE["uplink_sat"].search(msg)
    if m:
        _ev(p, epoch, "uplink_sat", slow_events=int(m.group(1)))
        return
    m = RE["decode_visibility"].search(msg)
    if m:
        _ev(p, epoch, "decode_visibility",
            peer=m.group(1), frm=m.group(2), to=m.group(3))
        return
    m = RE["baseline_rtt"].search(msg)
    if m:
        _ev(p, epoch, "baseline_rtt", rtt_ms=float(m.group(1)))
        return
    if RE["reelection"].search(msg):
        _ev(p, epoch, "reelection")
        return
    if RE["connection_lost"].search(msg):
        _ev(p, epoch, "connection_lost")
        return
    m = RE["capability_ceiling"].search(msg)
    if m:
        _ev(p, epoch, "capability_ceiling", layers=int(m.group(1)))
        return
    m = RE["publish_layers"].search(msg)
    if m:
        _ev(p, epoch, "publish_layers", layers=int(m.group(1)))
        return
    m = RE["cpu_watchdog"].search(msg)
    if m:
        _ev(p, epoch, "cpu_watchdog", drift_ms=int(m.group(1)))
        return


def load_meeting(log_dir, room, date, env):
    meeting = Meeting(room, date, env)
    users = discover_users(log_dir)
    if not users:
        sys.stderr.write(f"ERROR: no *.log.gz files found in {log_dir}\n")
        sys.exit(2)
    for email, files in sorted(users.items()):
        p = load_participant(meeting, email, files, log_dir)
        meeting.participants[email] = p
        if p.first_ts is not None:
            meeting.first_ts = p.first_ts if meeting.first_ts is None else min(meeting.first_ts, p.first_ts)
        if p.last_ts is not None:
            meeting.last_ts = p.last_ts if meeting.last_ts is None else max(meeting.last_ts, p.last_ts)
    # Backfill display_name / session->name from own sessions where PARTICIPANT_JOINED missed it.
    for p in meeting.participants.values():
        for sid in p.own_sessions:
            if p.display_name:
                meeting.session_to_name.setdefault(sid, p.display_name)
    return meeting


def events_of(p, kind):
    return [e for e in p.events if e["kind"] == kind]


# ===========================================================================
# Prometheus client — ALWAYS anchored at the meeting epoch (guard-rail #1)
# ===========================================================================
class PromClient:
    def __init__(self, env_cfg, end_epoch, lookback_min, enabled=True, insecure=False):
        self.cfg = env_cfg
        self.end_epoch = int(end_epoch)
        self.lookback = max(1, int(lookback_min))
        self.enabled = enabled and bool(env_cfg.get("prom_url"))
        self.insecure = insecure
        self.warnings = []
        self._auth_header = None
        auth = env_cfg.get("prom_auth")
        if auth and auth[0] == "env":
            user = os.environ.get(auth[1]) or self._from_ascend_env(auth[1])
            pw = os.environ.get(auth[2]) or self._from_ascend_env(auth[2])
            if user and pw:
                import base64
                tok = base64.b64encode(f"{user}:{pw}".encode()).decode()
                self._auth_header = f"Basic {tok}"
            else:
                self.warnings.append(
                    f"Prom basic-auth creds ({auth[1]}/{auth[2]}) not found in env or ~/.ascend-env"
                )

    @staticmethod
    def _from_ascend_env(key):
        path = os.path.expanduser("~/.ascend-env")
        if not os.path.exists(path):
            return None
        for line in open(path):
            line = line.strip()
            if line.startswith(key + "=") or line.startswith("export " + key + "="):
                v = line.split("=", 1)[1].strip().strip('"').strip("'")
                return v
        return None

    def query(self, expr):
        """Instant query, ALWAYS anchored at end_epoch (guard-rail #1)."""
        if not self.enabled:
            return None
        url = self.cfg["prom_url"].rstrip("/") + "/api/v1/query"
        data = urllib.parse.urlencode({"query": expr, "time": str(self.end_epoch)}).encode()
        req = urllib.request.Request(url, data=data, method="POST")
        req.add_header("Content-Type", "application/x-www-form-urlencoded")
        if self._auth_header:
            req.add_header("Authorization", self._auth_header)
        try:
            import ssl
            # TLS verification is ON by default. The Ascend query carries Basic-auth ops
            # creds, so an unverified channel would expose them to a MITM presenting a spoofed
            # cert (flagged by both reviewers). Only disable verification when the operator
            # explicitly passes --insecure-tls (e.g. a self-signed internal endpoint).
            ctx = ssl.create_default_context()
            if self.insecure:
                ctx.check_hostname = False
                ctx.verify_mode = ssl.CERT_NONE
            with urllib.request.urlopen(req, timeout=20, context=ctx) as resp:
                payload = json.loads(resp.read().decode())
        except Exception as e:
            self.warnings.append(f"Prom query failed ({expr[:60]}…): {e}")
            return None
        if payload.get("status") != "success":
            self.warnings.append(f"Prom query non-success ({expr[:60]}…): {payload.get('error')}")
            return None
        result = payload.get("data", {}).get("result", [])
        if not result:
            # GUARD-RAIL #1: never conclude "no data" from an empty instant query.
            self.warnings.append(
                f"EMPTY result for `{expr[:70]}…` — did you anchor at the meeting epoch? "
                f"(anchored @ {self.end_epoch}; client GaugeVecs go stale ~5min after the call)"
            )
        return result

    def lb(self):
        """Range-vector lookback string spanning the whole meeting, e.g. '78m'."""
        return f"{self.lookback}m"


# ===========================================================================
# Finding model
# ===========================================================================
class Finding:
    def __init__(self, rule, severity, title, evidence=None, drill=None, subject=None):
        self.rule = rule
        self.severity = severity
        self.title = title
        self.subject = subject          # the participant/email this is about
        self.evidence = evidence or []  # list of strings
        self.drill = drill or []        # list of strings (drill-down lines)


def label(meeting, p):
    nm = p.display_name or meeting.session_to_name.get(p.own_session or "", None) or p.email
    return nm


# ===========================================================================
# Rule engine (D) — each rule a small function over events + Prom + pod map.
# ===========================================================================
# Human-readable name for each rule id, so reports are self-documenting and a
# reader never has to know the R# shorthand cold (the summary table renders
# "R7 (keyframe-starvation freeze)", not a bare "R7"). Keep in sync with the
# R1-R14 table in meeting_quality_xref.README.md when rules change.
RULE_NAMES = {
    "R1": "cross-pod base-pin (#1202)",
    "R2": "periodic-tick alarm (not a user action)",
    "R3": "layer oscillation (shed/restore flap)",
    "R4": "WS send-side HOL / uplink saturation",
    "R5": "audio concealment (breakup)",
    "R6": "ProtectiveMode thrash",
    "R7": "keyframe-starvation freeze",
    "R9": "navigator.connection guard (UNRELIABLE)",
    "R10": "re-election / connection instability",
    "R12": "camera-state contradiction",
    "R13": "low-core device",
    "R14": "high-RTT environment",
}


def rule_name(rule_id):
    """Human label for a rule id; falls back to the bare id if unmapped."""
    return RULE_NAMES.get(rule_id, rule_id)


def rule_R1_cross_pod_base_pin(meeting, prom):
    """#1202 FLAGSHIP: publisher's union_cap stuck at 1 (base) while consumers on a
    DIFFERENT pod decode the stream and never see > L0."""
    findings = []
    pods = {e: p.pod for e, p in meeting.participants.items() if p.pod}
    multi_pod = len(set(pods.values())) > 1
    for email, p in meeting.participants.items():
        aq = events_of(p, "aq")
        if not aq:
            continue
        union_vals = [e["union_cap"] for e in aq if e["union_cap"] is not None]
        active_vals = [e["active_layers"] for e in aq]
        if not union_vals:
            continue
        max_union = max(union_vals)
        # Pinned to base if union_cap never rose above 1 AND active stuck at 1.
        pinned = (max_union <= 1 and max(active_vals) <= 1)
        if not pinned:
            continue
        # Distinguish the #1202 deadlock (INVOLUNTARY pin) from a legitimately single-layer
        # publisher. Two independent "device could publish more" signals:
        #   - capability ceiling > 1 (the device is allowed >1 layer), AND/OR
        #   - "publishing N video layers" with N > 1 (the encoder was configured for >1).
        # If a device is GENUINELY 1-layer (ceiling==1 or publish==1), it is not the cross-pod bug.
        ceil_vals = [c["layers"] for c in events_of(p, "capability_ceiling")]
        capability_ceiling = max(ceil_vals) if ceil_vals else None
        pub_vals = [c["layers"] for c in events_of(p, "publish_layers")]
        publish_layers = max(pub_vals) if pub_vals else None
        if capability_ceiling == 1 or publish_layers == 1:
            continue  # genuinely a 1-layer device/config — not the cross-pod bug
        involuntary = (capability_ceiling is not None and capability_ceiling > 1) or \
                      (publish_layers is not None and publish_layers > 1)
        # Did anyone on a DIFFERENT pod decode this publisher's video?
        my_pod = p.pod
        cross_pod_consumers = []
        for oe, op in meeting.participants.items():
            if oe == email or op.pod == my_pod or not op.pod:
                continue
            decoded = any(
                ls["sender"] in p.own_sessions and ls["media"] == "video"
                for ls in events_of(op, "layer_switch")
            )
            froze_on = any(
                fr["sender"] in p.own_sessions for fr in events_of(op, "freshness")
            )
            if decoded or froze_on:
                cross_pod_consumers.append((op, decoded, froze_on))
        if not cross_pod_consumers:
            continue
        # Confirm the consumers only ever saw L0 for this publisher (highest_available==0).
        max_ha = 0
        for op, _d, _f in cross_pod_consumers:
            for ls in events_of(op, "layer_switch"):
                if ls["sender"] in p.own_sessions and ls["media"] == "video":
                    max_ha = max(max_ha, ls["highest_available"])
        # Severity: a confirmed-involuntary pin spanning both pods is the CRITICAL #1202 case.
        # If we can't confirm the pin is involuntary (no ceiling/publish line logged), downgrade
        # to MEDIUM and hedge the title — do NOT assert #1202 on an unverified cause (review fix).
        if involuntary and multi_pod:
            sev = "CRITICAL"
        elif involuntary:
            sev = "HIGH"
        else:
            sev = "MEDIUM"
        names = ", ".join(label(meeting, c[0]) for c in cross_pod_consumers)
        # Only claim "decoded only at L0" when that is actually true; otherwise report the layer.
        if max_ha == 0:
            decode_clause = f"consumers on the other pod ({names}) decoded only base video (L0)"
        else:
            decode_clause = (f"consumers on the other pod ({names}) reached at most L{max_ha} "
                             f"(partial — base-pin not absolute)")
        pin_clause = ("#1202 cross-pod base-pin" if involuntary
                      else "possible cross-pod base-pin (pin cause UNVERIFIED — no capability/"
                           "publish-layers line logged)")
        ceil_str = (f"{capability_ceiling} layer(s)" if capability_ceiling is not None else "?")
        pub_str = (f"{publish_layers} layer(s)" if publish_layers is not None else "?")
        f = Finding(
            "R1", sev,
            f"{pin_clause}: {label(meeting,p)} (pod={my_pod}) published only base video "
            f"(union_cap≤1, active=1) while {decode_clause}.",
            subject=email,
            evidence=[
                f"{label(meeting,p)} pod={my_pod}, own_session(s)={sorted(p.own_sessions)}",
                f"union_cap samples: max={max_union}, n={len(union_vals)} (AQ_STATUS)",
                f"active_layers: max={max(active_vals)}",
                f"capability ceiling={ceil_str}, configured publish={pub_str} → pin is "
                + ("INVOLUNTARY (device/config allows >1 layer) ⇒ #1202" if involuntary
                   else "of UNVERIFIED cause (no ceiling/publish line) — treat as a lead, not a verdict"),
                f"cross-pod consumers (pod != {my_pod}): {names}",
                f"consumers' max highest_available for this publisher: {max_ha}",
            ],
        )
        # Drill: union_cap timeline + pod map
        drill = [f"union_cap / active_layers timeline for {label(meeting,p)}:"]
        last = None
        for e in aq:
            key = (e["active_layers"], e["union_cap"])
            if key != last:
                drill.append(f"  {fmt_ts(e['ts'])} active_layers={e['active_layers']} "
                             f"union_cap={e['union_cap']} video_tier={e['video_tier']}")
                last = key
        drill.append("pod assignment (proves the split):")
        for oe, op in meeting.participants.items():
            drill.append(f"  {label(meeting,op):28s} pod={op.pod}")
        f.drill = drill
        findings.append(f)
    return findings


def _intervals(epochs):
    epochs = sorted(e for e in epochs if e is not None)
    return [round(epochs[i + 1] - epochs[i], 3) for i in range(len(epochs) - 1)]


def rule_R2_periodic_tick(meeting, prom):
    """Detect machine-cadence repeats (NOT user actions). The 5.0s LAYER_PREFERENCE
    chooser tick that was mistaken for pinning is the canonical case."""
    findings = []
    watch = {
        "layer_pref": "LAYER_PREFERENCE re-publish",
        "keyframe_req": "KEYFRAME_REQUEST",
    }
    for email, p in meeting.participants.items():
        for kind, human in watch.items():
            evs = events_of(p, kind)
            gaps = _intervals([e["ts"] for e in evs])
            if len(evs) < TH["periodic_min_events"] or not gaps:
                continue
            # Two interleaved sub-cadences inflate CV, so prefer a modal-gap fraction.
            buckets = Counter(round(g * 2) / 2 for g in gaps)  # 0.5s resolution
            modal_gap, modal_n = buckets.most_common(1)[0]
            modal_frac = modal_n / len(gaps)
            mean = statistics.mean(gaps)
            cv = (statistics.pstdev(gaps) / mean) if mean else 99
            is_periodic = (modal_frac >= TH["periodic_modal_frac"] and modal_gap > 0) or \
                          (cv < TH["periodic_cv"])
            if not is_periodic:
                continue
            period = modal_gap if modal_frac >= TH["periodic_modal_frac"] else round(mean, 2)
            findings.append(Finding(
                "R2", "INFO",
                f"{human} fires every ~{period}s for {label(meeting,p)} "
                f"({len(evs)} events) — periodic tick/loop, NOT a user action.",
                subject=email,
                evidence=[
                    f"n={len(evs)} events, modal gap {modal_gap}s = {modal_frac:.0%} of intervals, "
                    f"mean={mean:.2f}s cv={cv:.2f}",
                    "Pin/viewport actions emit NO log — do not read this cadence as user behaviour.",
                ],
            ))
    return findings


def rule_R3_layer_oscillation(meeting, prom):
    """shed-under-load <-> restore flapping on the publisher's encoder."""
    findings = []
    for email, p in meeting.participants.items():
        sim = events_of(p, "simulcast")
        sheds = [e for e in sim if e["reason"] == "shed-under-load"]
        restores = [e for e in sim if e["reason"] == "restore"]
        if len(sheds) <= TH["oscillation_shed_count"]:
            continue
        # Sub-classify the cause.
        cap_ceiling = events_of(p, "capability_ceiling")
        uplink = events_of(p, "uplink_sat")
        cpu_wd = events_of(p, "cpu_watchdog")
        ws_bp = events_of(p, "ws_backpressure")
        cap_score = p.specs.get("capability_score")
        # Report ALL contributing signals (a publisher can be both CPU- and uplink-bound);
        # ordered most-to-least dominant.
        signals = []
        if ws_bp:
            signals.append(f"WS uplink saturation ({len(ws_bp)} backpressure drops → see R4)")
        if uplink:
            signals.append("WT uplink-saturation (slow-ready events logged)")
        if cpu_wd:
            max_drift = max(e["drift_ms"] for e in cpu_wd)
            signals.append(f"CPU main-thread drift watchdog ({len(cpu_wd)}×, max {max_drift}ms)")
        if cap_ceiling and any(c["layers"] < 3 for c in cap_ceiling):
            signals.append("CPU/capability ceiling")
        cause = "; ".join(signals) if signals else "unknown"
        # Prom corroboration: cpu_throttled gauge + stddev_over_time(encoder_active_layers[camera])
        prom_note = ""
        if prom and prom.enabled and p.display_name:
            dn = promql_label(p.display_name)
            # Scope by meeting_id (both metrics carry it; verified metrics.rs:1008/1061) so a
            # same-display_name participant in another concurrent meeting can't contaminate the
            # CPU-hunting corroboration for this one.
            mid = promql_label(meeting.room)
            res = prom.query(
                f'stddev_over_time(videocall_encoder_active_layers'
                f'{{meeting_id="{mid}",media_kind="camera",display_name="{dn}"}}[{prom.lb()}] @ {prom.end_epoch})'
            )
            if res:
                try:
                    sd = float(res[0]["value"][1])
                    prom_note = f"Prom stddev_over_time(encoder_active_layers[camera])={sd:.2f} (>0 ⇒ hunting)"
                except Exception:
                    pass
            thr = prom.query(
                f'max_over_time(videocall_client_cpu_throttled'
                f'{{meeting_id="{mid}",display_name="{dn}"}}[{prom.lb()}] @ {prom.end_epoch})'
            )
            if thr:
                try:
                    tv = float(thr[0]["value"][1])
                    prom_note += f"  cpu_throttled max={tv:.0f}"
                except Exception:
                    pass
        f = Finding(
            "R3", "MEDIUM",
            f"Layer oscillation on {label(meeting,p)}: {len(sheds)} shed / {len(restores)} restore "
            f"({cause}).",
            subject=email,
            evidence=[
                f"sheds={len(sheds)} restores={len(restores)} cap_score={cap_score} cause={cause}",
            ] + ([prom_note] if prom_note else []),
        )
        drill = [f"simulcast layer-change timeline for {label(meeting,p)}:"]
        for e in sim:
            drill.append(f"  {fmt_ts(e['ts'])} {e['frm']}->{e['to']} ({e['reason']})")
        f.drill = drill
        findings.append(f)
    return findings


def rule_R4_send_side_hol(meeting, prom):
    """WS send-side head-of-line blocking: buffered_amount near the 1MB cliff + drops.
    Audio is FIFO behind video on the single TCP socket."""
    findings = []
    for email, p in meeting.participants.items():
        bp = events_of(p, "ws_backpressure")
        if not bp:
            continue
        max_buf = max(e["buffered"] for e in bp)
        total_dropped = sum(e["dropped_bytes"] for e in bp)
        # Fire only on a genuine send-side saturation signal: buffered_amount reached the
        # threshold OR packets were actually dropped. (`bp` is already non-empty here, so the
        # prior `len(bp) == 0` conjunct was dead and the threshold was never consulted. The WS
        # emitter only logs above the 1MB cliff so this rarely changes the outcome, but the
        # guard now expresses the intended condition.) Concealment victims are attributed in R5;
        # R4 stays on the send side.
        if max_buf < TH["send_queue_bytes"] and total_dropped == 0:
            continue
        f = Finding(
            "R4", "HIGH",
            f"WS send-side HOL / uplink saturation: {label(meeting,p)} "
            f"buffered up to {max_buf/1000:.0f}KB, {len(bp)} dropped packets "
            f"({total_dropped/1000:.0f}KB) — audio HOL-blocked behind video on the shared TCP socket "
            f"(WS only).",
            subject=email,
            evidence=[
                f"max buffered_amount={max_buf} bytes (cliff=1048576), drop events={len(bp)}, "
                f"pod={p.pod}",
                "WS multiplexes ALL media on ONE TCP socket — audio is strictly FIFO behind video.",
            ],
        )
        drill = [f"WebSocket backpressure timeline for {label(meeting,p)}:"]
        # Sample buffered_amount progression.
        step = max(1, len(bp) // 12)
        for e in bp[::step]:
            drill.append(f"  {fmt_ts(e['ts'])} buffered={e['buffered']} dropped={e['dropped_bytes']}B")
        f.drill = drill
        findings.append(f)
    return findings


def _resolve_peer(meeting, token):
    """A concealment/received_layer label `from_peer` can be an email OR a session_id.
    Resolve to a display name; return (name, email-or-None)."""
    if token is None:
        return ("?", None)
    if "@" in token:  # email
        p = meeting.participants.get(token)
        return ((p.display_name if p and p.display_name else token), token)
    # session id
    return (meeting.name_for(token), meeting.session_to_email.get(token))


def rule_R5_concealment_by_source(meeting, prom):
    """audio_concealment_pct > 15% names the bad uplink. Needs Prom.

    Label semantics VERIFIED on the cluster (do NOT trust intuition here):
      - `to_peer`       = the SOURCE peer's session_id (whose audio is being concealed)
      - `reporter_name` = the RECEIVER reporting the concealment
      - `from_peer`     = the receiver's OWN email (NOT the source) — counter-intuitive
    Use avg_over_time (sustained concealment), not max_over_time (transient 100% spikes).
    Aggregate by `to_peer` to attribute the bad UPLINK to its owner."""
    findings = []
    if not (prom and prom.enabled):
        return findings
    # Per (to_peer, reporter) sustained average — names each receiver that heard the source badly.
    # MUST scope to this meeting: `videocall_audio_concealment_pct` carries a `meeting_id` label
    # (== room name; verified metrics.rs:1071) and persists ~5min, so an UNSCOPED query pulls in
    # any other live meeting on the same Prometheus in the lookback window and would fabricate
    # HIGH "source uplink fault" findings against foreign sessions. Scope it, same as the relay
    # section already does for its room-labeled series.
    _mid = promql_label(meeting.room)
    res = prom.query(
        f'avg by (to_peer, reporter_name)'
        f'(avg_over_time(videocall_audio_concealment_pct{{meeting_id="{_mid}"}}[{prom.lb()}] @ {prom.end_epoch}))'
    )
    if not res:
        return findings
    # Group by RESOLVED person (a reconnect gives one user two sessions — collapse them).
    by_src = defaultdict(dict)   # (src_name, src_email) -> {receiver: worst_pct}
    by_rx = defaultdict(dict)    # receiver -> {src_name: worst_pct}
    for series in res:
        try:
            val = float(series["value"][1])
        except Exception:
            continue
        if val <= TH["concealment_pct"]:
            continue
        m = series.get("metric", {})
        src_sid = m.get("to_peer")
        rx = m.get("reporter_name") or "?"
        if not src_sid or src_sid == "none":
            continue
        src_name, src_email = _resolve_peer(meeting, src_sid)
        # self-pair guard (#5): a receiver hearing itself is 0% by construction; skip if matched.
        if src_name == rx:
            continue
        prev = by_src[(src_name, src_email)].get(rx, 0)
        by_src[(src_name, src_email)][rx] = max(prev, val)
        by_rx[rx][src_name] = max(by_rx[rx].get(src_name, 0), val)

    # Discriminate UPLINK-source (heard badly by >= 2 receivers) from RECEIVER-downlink
    # (one receiver hears MANY sources badly — the loss is on that receiver's link).
    downlink_receivers = {rx for rx, srcs in by_rx.items() if len(srcs) >= 3}
    for (src_name, src_email), per_rx in sorted(by_src.items(), key=lambda kv: -max(kv[1].values())):
        # receivers attributable to a known bad downlink don't prove a source uplink fault
        real_rx = {rx: v for rx, v in per_rx.items() if rx not in downlink_receivers}
        if len(real_rx) < 2:
            continue  # not a source-uplink problem; covered by the receiver-downlink finding below
        worst = max(real_rx.values())
        findings.append(Finding(
            "R5", "HIGH",
            f"Audio breakup, SOURCE={src_name}: heard concealed by {len(real_rx)} receiver(s), "
            f"up to {worst:.0f}% synthesized (sustained avg) — source uplink fault.",
            subject=src_email,
            evidence=[f"{src_name} heard by {rx} at {v:.0f}% avg concealment"
                      for rx, v in sorted(real_rx.items(), key=lambda x: -x[1])],
        ))
    for rx in sorted(downlink_receivers, key=lambda r: -max(by_rx[r].values())):
        srcs = by_rx[rx]
        worst = max(srcs.values())
        findings.append(Finding(
            "R5", "MEDIUM",
            f"Audio breakup at RECEIVER {rx}: hears {len(srcs)} different sources concealed "
            f"(up to {worst:.0f}%) — receiver downlink fault, not a source uplink fault.",
            subject=None,
            evidence=[f"{rx} hears {s} at {v:.0f}% avg concealment"
                      for s, v in sorted(srcs.items(), key=lambda x: -x[1])],
        ))
    return findings


def rule_R6_protective_thrash(meeting, prom):
    findings = []
    for email, p in meeting.participants.items():
        pm = events_of(p, "protective")
        if not pm:
            continue
        entered = [e for e in pm if e["state"] == "ENTERED"]
        emergency = [e for e in pm if e["state"] == "EMERGENCY"]
        cycles = len(entered) + len(emergency)
        if cycles <= TH["protective_cycles"]:
            continue
        triggers = Counter(e["trigger"] for e in entered if e["trigger"])
        top_trigger = triggers.most_common(1)[0][0] if triggers else "?"
        meaning = ("receiver audio-jitter starvation" if top_trigger == "audio_buffer"
                   else "decode/main-thread pressure" if top_trigger == "fps" else "?")
        findings.append(Finding(
            "R6", "MEDIUM",
            f"ProtectiveMode thrash on {label(meeting,p)}: {len(entered)} ENTERED / "
            f"{len(emergency)} EMERGENCY (top trigger={top_trigger} → {meaning}).",
            subject=email,
            evidence=[
                f"ENTERED={len(entered)} EMERGENCY={len(emergency)} EXITED="
                f"{len([e for e in pm if e['state']=='EXITED'])}",
                f"trigger histogram: {dict(triggers)}",
            ],
        ))
    return findings


def rule_R7_keyframe_starvation(meeting, prom):
    """held-last-good freshness_skip with no keyframe + high head_age => keyframe-starved freeze.
    Attribute by SENDER (the publisher whose keyframe never arrived)."""
    findings = []
    # Aggregate freshness_skip by sender across ALL receivers.
    by_sender = defaultdict(list)   # sender_sid -> list of (head_age, receiver_email, ts)
    for email, p in meeting.participants.items():
        for e in events_of(p, "freshness"):
            if e["keyframe_seq"] != "none":
                continue
            by_sender[e["sender"]].append((e["head_age_ms"], email, e["ts"]))
    for sender, lst in by_sender.items():
        if len(lst) <= TH["freshness_skip_count"]:
            continue
        max_age = max(a for a, _, _ in lst)
        rxs = Counter(e for _, e, _ in lst)
        sender_name = meeting.name_for(sender)
        sev = "HIGH" if max_age >= TH["freshness_head_age_ms"] else "MEDIUM"
        # Receiver names
        rx_summary = ", ".join(
            f"{label(meeting, meeting.participants[e]) if e in meeting.participants else e}={n}"
            for e, n in rxs.most_common()
        )
        f = Finding(
            "R7", sev,
            f"Keyframe-starved freeze, sender={sender_name}: {len(lst)} held-last-good "
            f"freshness_skips across receivers, max head_age={max_age}ms.",
            subject=meeting.session_to_email.get(sender),
            evidence=[
                f"sender session={sender} ({sender_name})",
                f"freshness_skip(keyframe_seq=none) per receiver: {rx_summary}",
                f"max head_age={max_age}ms (UNBOUNDED — not capped at 1800ms)",
            ],
        )
        # Drill: head_age progression + KEYFRAME_REQUEST cadence toward this sender.
        drill = [f"head_age progression toward {sender_name} (held-last-good):"]
        for age, e, ts in sorted(lst, key=lambda x: (x[2] is None, x[2]))[:20]:
            who = label(meeting, meeting.participants[e]) if e in meeting.participants else e
            drill.append(f"  {fmt_ts(ts)} {who} head_age={age}ms")
        # KEYFRAME_REQUEST cadence toward this sender
        kf = []
        for email, p in meeting.participants.items():
            for e in events_of(p, "keyframe_req"):
                if e["sender"] == sender:
                    kf.append(e["ts"])
        if kf:
            gaps = _intervals(kf)
            drill.append(f"KEYFRAME_REQUEST toward {sender_name}: {len(kf)} requests, "
                         f"median gap {statistics.median(gaps) if gaps else 0:.1f}s")
        f.drill = drill
        findings.append(f)
    return findings


def rule_R9_navigator_connection_guard(meeting, prom):
    """GUARD (not a rule): print preamble network= but LABEL IT UNRELIABLE; never emit a
    bandwidth finding from it alone."""
    findings = []
    lines = []
    for email, p in meeting.participants.items():
        net = p.specs.get("network")
        if net:
            lines.append(f"{label(meeting,p):28s} network={net}  (pod={p.pod})")
    if not lines:
        return findings
    findings.append(Finding(
        "R9", "INFO",
        "navigator.connection preamble values (UNRELIABLE — never attribute bandwidth from these).",
        evidence=lines + [
            "Corroborate with active_server_rtt / multi-second downstream gaps / a speed test.",
            "Real-world miss: a '1.7Mbps' preamble vs a 551Mbps speed test (same machine).",
        ],
    ))
    return findings


def rule_R10_reelection(meeting, prom):
    findings = []
    for email, p in meeting.participants.items():
        re_count = len(events_of(p, "reelection"))
        lost = len(events_of(p, "connection_lost"))
        if re_count == 0 and lost == 0:
            continue
        sev = "HIGH" if lost > 0 else "LOW"
        findings.append(Finding(
            "R10", sev,
            f"Connection instability on {label(meeting,p)}: {re_count} re-election trigger(s), "
            f"{lost} connection-lost event(s).",
            subject=email,
            evidence=[f"re-election triggers={re_count}, connection-lost={lost}, pod={p.pod}"],
        ))
    return findings


def rule_R12_camera_state_crosscheck(meeting, prom):
    """Camera-state contradiction guard: full-session host_render scan + peer LAYER_SWITCH
    kind=video cross-check. Flags 'looks audio-only but peers decoded video'."""
    findings = []
    for email, p in meeting.participants.items():
        hr = events_of(p, "host_render")
        ever_video_on = any(e["video"] for e in hr)
        # Peer cross-check: did anyone decode this participant's video?
        peers_saw_video = False
        for op in meeting.participants.values():
            if op.email == email:
                continue
            if any(ls["sender"] in p.own_sessions and ls["media"] == "video"
                   for ls in events_of(op, "layer_switch")):
                peers_saw_video = True
                break
        if peers_saw_video and not ever_video_on and hr:
            findings.append(Finding(
                "R12", "MEDIUM",
                f"Camera-state contradiction for {label(meeting,p)}: host_render never shows "
                f"video=true, but peers decoded their video (LAYER_SWITCH kind=video). "
                f"Do NOT report as audio-only — log likely truncated before camera-on.",
                subject=email,
                evidence=[
                    f"host_render video=true count: 0 of {len(hr)}",
                    "peer cross-check: at least one peer pulled LAYER_SWITCH kind=video for this session",
                ],
            ))
    return findings


def rule_R13_implausible_rtt_stall(meeting, prom):
    findings = []
    for email, p in meeting.participants.items():
        cores = p.specs.get("cores")
        if cores is None:
            continue
        try:
            cores_n = int(cores)
        except Exception:
            continue
        if cores_n >= TH["low_cores"]:
            continue
        findings.append(Finding(
            "R13", "LOW",
            f"Low-core device: {label(meeting,p)} has {cores_n} cores — main-thread stall / "
            f"decode starvation risk.",
            subject=email,
            evidence=[f"cores={cores_n} capability_score={p.specs.get('capability_score')}"],
        ))
    return findings


def rule_R14_high_rtt(meeting, prom):
    """High-RTT environment from client Baseline RTT (log-derivable; Prom corroborates)."""
    findings = []
    for email, p in meeting.participants.items():
        rtts = [e["rtt_ms"] for e in events_of(p, "baseline_rtt")]
        if not rtts:
            continue
        max_rtt = max(rtts)
        if max_rtt <= TH["high_rtt_ms"]:
            continue
        note = ""
        if "cato" in (p.specs.get("network") or "").lower():
            note = " (Cato/SASE — possible egress-PoP hairpin; suggest ifconfig.co/json)"
        findings.append(Finding(
            "R14", "MEDIUM",
            f"High-RTT environment: {label(meeting,p)} baseline RTT up to {max_rtt:.0f}ms{note}.",
            subject=email,
            evidence=[f"baseline RTT samples: max={max_rtt:.0f}ms n={len(rtts)} pod={p.pod}"],
        ))
    return findings


ALL_RULES = [
    rule_R1_cross_pod_base_pin,
    rule_R2_periodic_tick,
    rule_R3_layer_oscillation,
    rule_R4_send_side_hol,
    rule_R5_concealment_by_source,
    rule_R6_protective_thrash,
    rule_R7_keyframe_starvation,
    rule_R9_navigator_connection_guard,
    rule_R10_reelection,
    rule_R12_camera_state_crosscheck,
    rule_R13_implausible_rtt_stall,
    rule_R14_high_rtt,
]


def run_rules(meeting, prom):
    findings = []
    for rule in ALL_RULES:
        try:
            findings.extend(rule(meeting, prom))
        except Exception as e:
            sys.stderr.write(f"WARN: rule {rule.__name__} raised: {e}\n")
    findings.sort(key=lambda f: (SEVERITY_ORDER.get(f.severity, 9), f.rule))
    return findings


# ===========================================================================
# Timeline builders (A produce, B consume)
# ===========================================================================
def bucket_index(ts, start, bucket):
    if ts is None or start is None:
        return None
    return int((ts - start) // bucket)


def produce_timeline(meeting, p, bucket):
    """Per-bucket produce summary: max active_layers, union_cap, video tier, sheds, audio tier."""
    start = meeting.first_ts
    rows = defaultdict(lambda: {"active": None, "union": None, "tier": None,
                                "sheds": 0, "restores": 0, "audio": None})
    for e in events_of(p, "aq"):
        b = bucket_index(e["ts"], start, bucket)
        if b is None:
            continue
        r = rows[b]
        r["active"] = e["active_layers"] if r["active"] is None else max(r["active"], e["active_layers"])
        if e["union_cap"] is not None:
            r["union"] = e["union_cap"] if r["union"] is None else max(r["union"], e["union_cap"])
        r["tier"] = e["video_tier"]
    for e in events_of(p, "simulcast"):
        b = bucket_index(e["ts"], start, bucket)
        if b is None:
            continue
        if e["reason"] == "shed-under-load":
            rows[b]["sheds"] += 1
        elif e["reason"] == "restore":
            rows[b]["restores"] += 1
    for e in events_of(p, "audio_tier"):
        b = bucket_index(e["ts"], start, bucket)
        if b is None:
            continue
        rows[b]["audio"] = f"{e['tier']}({e['kbps']})"
    return rows


def consume_matrix(meeting, p):
    """receiver p's view per sender: max highest_available, max rendered (to=), freshness count."""
    senders = defaultdict(lambda: {"max_ha": 0, "max_to": 0, "freshness": 0, "media": set()})
    for e in events_of(p, "layer_switch"):
        s = senders[e["sender"]]
        s["max_ha"] = max(s["max_ha"], e["highest_available"])
        s["max_to"] = max(s["max_to"], e["to"])
        s["media"].add(e["media"])
    for e in events_of(p, "freshness"):
        senders[e["sender"]]["freshness"] += 1
    return senders


# ===========================================================================
# Renderers
# ===========================================================================
def render_markdown(meeting, findings, prom, args):
    out = []
    span_min = ((meeting.last_ts or 0) - (meeting.first_ts or 0)) / 60.0
    out.append(f"# Meeting quality cross-reference — `{meeting.room}` ({meeting.date}, env={meeting.env})")
    out.append("")
    out.append(f"- Window: {fmt_clock(meeting.first_ts)} → {fmt_clock(meeting.last_ts)} "
               f"({span_min:.0f} min)")
    out.append(f"- Participants: {len(meeting.participants)}")
    appver = next((p.specs.get("appVersion") for p in meeting.participants.values()
                   if p.specs.get("appVersion")), "?")
    out.append(f"- Build: {appver}")
    # Pod split (first-class — guard-rail #3)
    pod_split = Counter(p.pod for p in meeting.participants.values() if p.pod)
    out.append(f"- Pod split: " + ", ".join(f"{v}×{k}" for k, v in pod_split.items()) +
               ("  ⚠️ ROOM SPANS BOTH PODS (WT/WS = two ChatServer instances → #1202 risk)"
                if len(pod_split) > 1 else ""))
    # Truncation detection (guard-rail #7)
    trunc = []
    for p in meeting.participants.values():
        if p.last_ts is not None and meeting.last_ts is not None and \
           (meeting.last_ts - p.last_ts) > 300:
            trunc.append(f"{label(meeting,p)} (logs stop {fmt_ts(p.last_ts)}, "
                         f"{(meeting.last_ts - p.last_ts)/60:.0f}min before meeting end)")
    if trunc:
        out.append(f"- ⚠️ Log truncation: " + "; ".join(trunc) +
                   " — corroborate duration with Prometheus, don't read 'events stopped' as 'behaviour stopped'.")
    out.append("")

    # Anomaly summary table (severity-sorted, at TOP)
    out.append("## Anomaly summary")
    out.append("")
    if not findings:
        out.append("_No anomalies fired._")
    else:
        out.append("_Rule = the anomaly-engine check that fired; each is named "
                   "inline (see the R1-R14 table in meeting_quality_xref.README.md)._")
        out.append("")
        out.append("| Sev | Rule | Finding |")
        out.append("|-----|------|---------|")
        for f in findings:
            out.append(f"| {f.severity} | {f.rule} ({rule_name(f.rule)}) | {f.title} |")
    out.append("")

    # Participant / pod / identity table
    out.append("## Participants (pod / session / device)")
    out.append("")
    out.append("| Participant | Pod | Own session | Cores | cap_score | network (UNRELIABLE) |")
    out.append("|---|---|---|---|---|---|")
    for email, p in sorted(meeting.participants.items()):
        out.append(f"| {label(meeting,p)} | {p.pod or '?'} | "
                   f"{(p.own_session or '?')[:10]}… | {p.specs.get('cores','?')} | "
                   f"{p.specs.get('capability_score','?')} | {p.specs.get('network','?')} |")
    out.append("")

    if args.produce or args.all:
        out.append("## A. Produce timelines (per user)")
        out.append("")
        for email, p in sorted(meeting.participants.items()):
            rows = produce_timeline(meeting, p, args.bucket)
            if not rows:
                continue
            out.append(f"### {label(meeting,p)} (pod={p.pod})")
            out.append("")
            out.append(f"| t+ ({args.bucket}s bkt) | video_tier | active | union_cap | shed | restore | audio |")
            out.append("|---|---|---|---|---|---|---|")
            # Collapse runs of identical (tier, active, union) state — only show transitions and
            # any bucket with a shed/restore. Keeps the table to meaningful changes.
            prev_state = object()
            for b in sorted(rows):
                r = rows[b]
                state = (r["tier"], r["active"], r["union"])
                has_activity = (r["sheds"] or r["restores"] or r["audio"])
                if state == prev_state and not has_activity:
                    continue
                prev_state = state
                out.append(f"| {b*args.bucket//60}m{b*args.bucket%60:02d}s | {r['tier'] or '-'} | "
                           f"{r['active'] if r['active'] is not None else '-'} | "
                           f"{r['union'] if r['union'] is not None else '-'} | "
                           f"{r['sheds'] or '-'} | {r['restores'] or '-'} | {r['audio'] or '-'} |")
            out.append("")

    if args.consume or args.all:
        out.append("## B. Consume matrix (receiver → sender)")
        out.append("")
        for email, p in sorted(meeting.participants.items()):
            mat = consume_matrix(meeting, p)
            if not mat:
                continue
            out.append(f"### {label(meeting,p)} pulled from:")
            out.append("")
            out.append("| Sender | media | max highest_available | max rendered (to) | freshness_skips |")
            out.append("|---|---|---|---|---|")
            for sender, s in sorted(mat.items(), key=lambda kv: -kv[1]["freshness"]):
                out.append(f"| {meeting.name_for(sender)} | {','.join(sorted(s['media']))} | "
                           f"{s['max_ha']} | {s['max_to']} | {s['freshness']} |")
            out.append("")

    if args.relay or args.all:
        out.append("## C. Relay / pod cross-reference")
        out.append("")
        render_relay_section(meeting, prom, out)

    # D. anomalies already summarised; here we give the full evidence
    if args.anomalies or args.all:
        out.append("## D. Anomaly detail + E. drill-down")
        out.append("")
        if not findings:
            out.append("_No anomalies fired._")
        for f in findings:
            out.append(f"### [{f.severity}] {f.rule} — {f.title}")
            out.append("")
            for ev in f.evidence:
                out.append(f"- {ev}")
            if f.drill:
                out.append("")
                out.append("<details><summary>drill-down</summary>")
                out.append("")
                out.append("```")
                out.extend(f.drill)
                out.append("```")
                out.append("")
                out.append("</details>")
            out.append("")

    # Prom warnings (guard-rail #1)
    if prom and prom.warnings:
        out.append("## Prometheus notes / warnings")
        out.append("")
        for w in prom.warnings:
            out.append(f"- ⚠️ {w}")
        out.append("")

    return "\n".join(out)


def render_relay_section(meeting, prom, out):
    out.append("**Pod assignment (authoritative, from `Elected connection`):**")
    out.append("")
    out.append("| Participant | Pod |")
    out.append("|---|---|")
    for email, p in sorted(meeting.participants.items()):
        out.append(f"| {label(meeting,p)} | {p.pod or '?'} |")
    out.append("")
    if not (prom and prom.enabled):
        out.append("_Relay Prometheus metrics skipped (--no-prom or no endpoint)._")
        out.append("")
        return
    room = promql_label(meeting.room)
    lb = prom.lb()
    ep = prom.end_epoch
    # Metric names + labels VERIFIED on the cluster (guard-rail #6). relay_layer_* use label
    # `room` (= meeting NAME). The drop counter has had several names across builds and may
    # have NO live series for a window — query several and report which exist. scheduler-lag is
    # a HISTOGRAM (videocall_relay_scheduler_lag_ms_{sum,count}), NOT a gauge.
    queries = {
        "relay_layer_filtered_total":
            f'sum(increase(relay_layer_filtered_total{{room="{room}"}}[{lb}] @ {ep}))',
        "relay_layer_forwarded_by_layer_total":
            f'sum by (layer_id)(increase(relay_layer_forwarded_by_layer_total{{room="{room}"}}[{lb}] @ {ep}))',
        "relay_packet_drops_total (by reason)":
            f'sum by (drop_reason)(increase(relay_packet_drops_total{{room="{room}"}}[{lb}] @ {ep}))',
        # Process-global relay counter: labels are only {transport, kind} (metrics.rs:1833),
        # NO meeting_id, so it CANNOT be meeting-scoped — it's a relay-wide health signal, read
        # as such (not attributed to this room).
        "videocall_outbound_channel_drops_total (Ascend drop name, relay-wide)":
            f'sum(increase(videocall_outbound_channel_drops_total[{lb}] @ {ep}))',
        # Client-side, carries meeting_id (metrics.rs:773) — MUST scope, else another concurrent
        # meeting's WS drops get attributed here (same contamination class as R5).
        "videocall_websocket_drops (client-side WS drops, by reporter)":
            f'sum by (display_name)(videocall_websocket_drops{{meeting_id="{room}"}} @ {ep})',
        "videocall_relay_scheduler_lag_ms (avg, histogram)":
            f'sum(rate(videocall_relay_scheduler_lag_ms_sum[{lb}] @ {ep}))'
            f'/sum(rate(videocall_relay_scheduler_lag_ms_count[{lb}] @ {ep}))',
    }
    out.append("**Relay Prometheus (anchored @ meeting epoch):**")
    out.append("")
    for human, q in queries.items():
        res = prom.query(q)
        out.append(f"- `{human}`:")
        if not res:
            out.append("    - (no live series in window — metric may be renamed/reset; not 'zero traffic')")
            continue
        for series in res:
            m = series.get("metric", {})
            labels = ",".join(f"{k}={v}" for k, v in m.items()) or "total"
            try:
                val = float(series["value"][1])
                # increase() extrapolates to fractional counts; round anything >=100 to a whole
                # number with separators (it's a count), keep small values (ms lag) at 2 decimals.
                # `:.3g` rendered large counts as unreadable 1.28e+05.
                disp = f"{val:,.0f}" if abs(val) >= 100 else f"{val:.2f}"
                out.append(f"    - {labels}: {disp}")
            except Exception:
                out.append(f"    - {labels}: {series['value']}")
    out.append("")


def render_json(meeting, findings, prom, args):
    obj = {
        "room": meeting.room,
        "date": meeting.date,
        "env": meeting.env,
        "window": {"start": meeting.first_ts, "end": meeting.last_ts},
        # Key by email (the unique identity), NOT the display name — two participants named
        # "Tony" would otherwise collide and one would be silently dropped from JSON output.
        # The display name is preserved as a field.
        "participants": {
            p.email: {
                "display_name": label(meeting, p),
                "pod": p.pod,
                "own_sessions": sorted(p.own_sessions),
                "specs": p.specs,
                "first_ts": p.first_ts,
                "last_ts": p.last_ts,
            } for p in meeting.participants.values()
        },
        "findings": [
            {"rule": f.rule, "severity": f.severity, "title": f.title,
             "subject": f.subject, "evidence": f.evidence, "drill": f.drill}
            for f in findings
        ],
        "prom_warnings": prom.warnings if prom else [],
    }
    return json.dumps(obj, indent=2)


# ===========================================================================
# Log auto-pull (kubectl/tar) — guard-rail: only pull if not already present
# ===========================================================================
def ensure_logs(env, room, date, log_dir):
    if os.path.isdir(log_dir) and any(f.endswith(".log.gz") for f in os.listdir(log_dir)):
        return log_dir
    cfg = ENVIRONMENTS[env]
    os.makedirs(log_dir, exist_ok=True)
    sys.stderr.write(f"Pulling console logs for {room}/{date} from {env}…\n")
    # Find the API pod.
    kc = cfg["kubeconfig"]
    ns = cfg["namespace"]
    get_pod = subprocess.run(
        ["kubectl", "--kubeconfig", kc, "get", "pods", "-n", ns,
         "-l", f"app.kubernetes.io/instance={cfg['api_instance']}",
         "-o", "jsonpath={.items[0].metadata.name}"],
        capture_output=True, text=True)
    pod = get_pod.stdout.strip()
    if not pod:
        sys.stderr.write(f"ERROR: no API pod found ({get_pod.stderr.strip()}). "
                         f"Pull logs manually per spec §1.\n")
        sys.exit(2)
    # Pipe kubectl-exec(tar czf -) into tar xzf - WITHOUT a shell: room/date/log_dir are
    # CLI/config values, so a shell=True pipeline would execute any shell metacharacters in
    # them. Wiring the pipe by hand also lets us observe the SOURCE tar's exit code, which a
    # shell pipeline (no pipefail under /bin/sh) would mask behind the receiving tar's 0.
    src = subprocess.Popen(
        ["kubectl", "--kubeconfig", kc, "exec", pod, "-n", ns, "--",
         "tar", "czf", "-", "-C", f"/data/console-logs/{room}/{date}", "."],
        stdout=subprocess.PIPE)
    dst = subprocess.Popen(
        ["tar", "xzf", "-", "-C", log_dir],
        stdin=src.stdout)
    src.stdout.close()  # let src receive SIGPIPE if dst exits
    dst_rc = dst.wait()
    src_rc = src.wait()
    if src_rc != 0 or dst_rc != 0:
        sys.stderr.write(f"ERROR: log pull failed (kubectl/tar rc={src_rc}, extract rc={dst_rc}).\n")
        sys.exit(2)
    return log_dir


# ===========================================================================
# CLI
# ===========================================================================
def main():
    ap = argparse.ArgumentParser(
        description="Deep investigative meeting-quality cross-reference (produce/consume/relay "
                    "timelines + R1-R14 anomaly rules).")
    ap.add_argument("--room", required=True, help="meeting room name (e.g. infra, meeting_sync)")
    ap.add_argument("--date", required=True, help="YYYY-MM-DD")
    ap.add_argument("--env", default="hcl-daily", choices=list(ENVIRONMENTS),
                    help="cluster: hcl-daily (fnxlabs) or ascend (conceptcar7)")
    ap.add_argument("--log-dir", default=None,
                    help="override log dir (default /tmp/console-logs/<room>/<date>)")
    ap.add_argument("--end-epoch", type=int, default=None,
                    help="meeting end epoch (s) for Prom anchoring; default = last log ts")
    ap.add_argument("--bucket", type=int, default=30, help="timeline bucket size in seconds")
    ap.add_argument("--produce", action="store_true")
    ap.add_argument("--consume", action="store_true")
    ap.add_argument("--relay", action="store_true")
    ap.add_argument("--anomalies", action="store_true")
    ap.add_argument("--all", action="store_true", help="all sections (default if none selected)")
    ap.add_argument("--drill", default=None, help="show drill-down only for this rule id (e.g. R7)")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--no-prom", action="store_true", help="skip all Prometheus queries")
    ap.add_argument("--insecure-tls", action="store_true",
                    help="disable TLS cert verification for Prometheus (only for a self-signed "
                         "internal endpoint; NOT recommended for the auth'd Ascend endpoint)")
    args = ap.parse_args()

    if not (args.produce or args.consume or args.relay or args.anomalies):
        args.all = True

    log_dir = args.log_dir or f"/tmp/console-logs/{args.room}/{args.date}"
    ensure_logs(args.env, args.room, args.date, log_dir)

    meeting = load_meeting(log_dir, args.room, args.date, args.env)

    end_epoch = args.end_epoch or int(meeting.last_ts or 0)
    span_min = math.ceil(((meeting.last_ts or 0) - (meeting.first_ts or 0)) / 60.0) + 2
    prom = PromClient(ENVIRONMENTS[args.env], end_epoch, span_min,
                      enabled=not args.no_prom, insecure=args.insecure_tls)

    findings = run_rules(meeting, prom)

    if args.drill:
        findings = [f for f in findings if f.rule == args.drill]

    if args.json:
        print(render_json(meeting, findings, prom, args))
    else:
        print(render_markdown(meeting, findings, prom, args))


if __name__ == "__main__":
    main()
