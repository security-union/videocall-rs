"""Markdown rendering of a call quality score result."""

import re

DIM_LABELS = {
    "A": "Audio: unusable share of talker audio",
    "V": "Video: freeze share of decoded tile-time",
    "L": "Latency: p95 audio-timestamp delay (ms)",
    "Q": "Delivered: share of tile-time with 0 < decoded video fps < 5",
    "S": "Stability: unplanned reconnects per hour",
}


MD_SPECIAL = re.compile(r"([\\`*_\[\]<>|~])")


def _md(text):
    """Escape markdown syntax and break @mentions and autolinks in text from labels, the manifest or the CLI."""
    text = MD_SPECIAL.sub(r"\\\1", " ".join(str(text).splitlines()))
    return text.replace("@", "@\u200b").replace("://", ":/\u200b/")


def _code(text):
    return " ".join(str(text).splitlines()).replace("`", "'").replace("@", "@\u200b")


def _fmt(v, digits=4):
    if v is None:
        return "–"
    if isinstance(v, float) and v.is_integer() and abs(v) < 1e15:
        return str(int(v))
    if isinstance(v, float):
        return f"{v:.{digits}g}"
    return _md(v)


def _gate_rows(gates):
    rows = ["| Gate | Status | Value | Detail |", "|---|---|---|---|"]
    for g in gates:
        val = g["value"]
        if isinstance(val, dict):
            val = ", ".join(f"{_md(k)}={_fmt(v)}" for k, v in val.items())
        elif isinstance(val, list):
            val = ", ".join(_md(v) for v in val)
        else:
            val = _fmt(val)
        rows.append(f"| {_md(g['gate'])} | **{_md(g['status'])}** | {val} | {_md(g['detail'])} |")
    return rows


TERMS = ("concealment", "missing_packets", "discarded", "playout_silence", "no_sample_or_not_listening")


def _resolution(step_s):
    step = _fmt(step_s) if step_s else "15"
    return (f"Audio delivery is sampled: packets and concealment per pair are 1 s NetEQ windows read once per {step} s "
            f"scrape, so dimension A sees 1 s in {step} and can miss a stall shorter than {step} s. Discarded packets "
            "and playout silence are cumulative counters, booked in the bucket they fall in rather than sampled. A "
            "does not read them in a bucket it does not score from the counters (for example talker not sending, a "
            "pipeline gap, a declared absence, or a pair missing from a receiver whose per-peer stats are at the cap), "
            "for a pair's first sample when its baseline is unknown, or beyond A's cap of 1 per sample.")


def _term(p, t, cov):
    got, want = cov.get({"discarded": "discarded audio", "playout_silence": "playout silence"}.get(t), (1, 1))
    if not want:
        return "–"
    if not got:
        return "not measured"
    return _fmt(p["A_terms"].get(t, 0.0)) + (f" ({got}/{want} talkers)" if got < want else "")


def _audio_terms(cell, name, coverage):
    rows = [(u, p) for u, p in sorted(cell["participants"].items()) if "A_terms" in p]
    if not rows:
        return []
    out = ["Dimension A by mechanism (share of talker audio). Per sample A = min(1, max(max(concealment, missing "
           "packets) + playout silence, discarded)): playout silence is added to concealment; discards cut audio a "
           "stall already concealed or silenced, so they count only beyond those. The readings below overlap and do not add up to A.", "",
           "| User | A | " + " | ".join(t.replace("_", " ") for t in TERMS) + " |", "|---|---|" + "---|" * len(TERMS)]
    out += [f"| {_md(u)} | {_fmt(p['A'])} | "
            + " | ".join(_term(p, t, coverage.get(u, {}).get(name.split("/")[0], {})) for t in TERMS) + " |"
            for u, p in rows]
    return out + [""]


def _freeze_and_delay(diag):
    out = ["", "### Video tiles that froze together (diagnostic, never gated)", "",
           "| Receiver | Buckets evaluated | Buckets that froze together | Max share frozen | Freeze s | In those buckets |",
           "|---|---|---|---|---|---|"]
    out += [f"| {_md(u)} | {f['buckets_evaluated']} | {len(f['buckets_froze_together'])} | {_fmt(f['max_share_frozen'])} | "
            f"{_fmt(f['freeze_s'])} | {_fmt(f['freeze_s_in_those_buckets'])} |"
            for u, f in diag.get("correlated_freeze", {}).items()]
    out += ["", "### Audio jitter-buffer delay (diagnostic, never gated; not mouth-to-ear latency)", ""]
    delay = diag.get("audio_delay", {})
    if isinstance(delay, str):
        return out + [_md(delay)]
    out += ["| Receiver | Buffer p95 (ms) | Target delay at its cap (share) | Accelerate ops/s (mean) |", "|---|---|---|---|"]
    out += [f"| {_md(u)} | {_fmt(d['buffer_ms_p95'])} | {_fmt(d['target_delay_at_cap_share'])} | "
            f"{_fmt(d['accelerate_ops_per_sec_mean'])} |" for u, d in delay.items()]
    return out


RELAY_LABELS = {
    "shed_escalations": "Shed escalations (stage one: camera video shed; two: session closed)",
    "stream_resets": "WebTransport stream resets",
    "session_drops": "Session drops, this room",
    "outbound_channel_drops": "Outbound channel drops",
    "stream_queue_drops": "Downlink stream-queue drops (audio is kind other)",
    "mailbox_drops": "Inbound mailbox drops",
    "wt_session_closes": "WebTransport session closes",
    "overflow_cap": "Overflow-stream frames at the publisher cap",
}


def _status(entry):
    return entry["status"] + (f" ({entry['detail']})" if entry.get("detail") else "")


def _transport_text(majority, tl):
    tl = tl or {}
    if tl.get("status") == "query failed":
        text = "query failed"
    elif tl.get("sessions"):
        text = tl["path"].replace(" -> ", " → ")
        text += f" (switches {tl['switches']}, in the hold {tl['switches_in_hold']})" if tl["switches"] else ""
        text += (f" ({tl['switches_unproven']} started alongside another transport)"
                 if tl.get("switches_unproven") else "")
    else:
        text = majority or "–"
    rec = tl.get("recorded")
    if rec:
        text += (f"; bots-app record: preferred {rec.get('preferred') or 'unknown'}, last {rec['observed'] or 'none'}, "
                 f"{rec['fallbacks']} fallbacks")
    return text


def _relay_evidence(sd):
    out = ["", "### Relay evidence (never gated)", "",
           "Room transports seen at the relay: "
           + _md(sd["room_transports"] if isinstance(sd["room_transports"], str)
                 else ", ".join(sd["room_transports"]) or "none") + f". {_md(sd['note'])}",
           "", "| Evidence | Value over the hold |", "|---|---|"]
    for key, label in RELAY_LABELS.items():
        e = sd[key]
        shown = (", ".join(f"{_md(k)} {_fmt(v)}" for k, v in e["by"].items())
                 + (f" ({_md(e['detail'])})" if e.get("detail") else "")) if "by" in e else _md(_status(e))
        out.append(f"| {label} | {shown or 'none'} |")
    lag = sd["scheduler_late_fraction_gt_50ms"]
    out.append(f"| Scheduler wakes later than 50 ms (share) | {_fmt(lag['value']) if 'value' in lag else _md(_status(lag))} |")
    q = sd["quic"]
    if q["status"] == "measured":
        out.append(f"| QUIC smoothed RTT over sessions, median / p95 (ms) | {_fmt(q['rtt_ms_median'])} / "
                   f"{_fmt(q['rtt_ms_p95'])} |")
        got, total = q["loss_sessions"]
        out.append(f"| QUIC downlink loss (lost / sent packets, {got} of {total} sessions) | {_fmt(q['loss_fraction'])} "
                   f"({_fmt(q['lost'])} / {_fmt(q['sent'])}); congestion events {_fmt(q['congestion_events'])} |")
    else:
        out.append(f"| QUIC path | {_md(_status(q))} |")
    cfs = sd["cpu_throttling"]
    shown = (", ".join(f"{_md(p)} max {_fmt(v['max'])} mean {_fmt(v['mean'])}" for p, v in cfs["by_pod"].items())
             if cfs["status"] == "measured" else _md(_status(cfs)))
    out.append(f"| CFS throttled share, relay and NATS pods | {shown} |")
    rows = [(u, st["session_id"], st) for u, sessions in q.get("by_user", {}).items() for st in sessions]
    if rows:
        out += ["", "| User | Session | QUIC RTT median (ms) | Lost / sent | Loss | Congestion events |",
                "|---|---|---|---|---|---|"]
        out += [f"| {_md(u)} | {_md(sid)} | {_fmt(st['rtt_ms_median'])} | {_fmt(st['lost'])} / {_fmt(st['sent'])} | "
                f"{_fmt(st['loss_fraction'])} | {_fmt(st['congestion_events'])} |" for u, sid, st in rows]
    drops = sd["session_drops"].get("by_user", {})
    if drops:
        out += ["", "Session drops by receiver: " + "; ".join(
            f"{_md(u)}: " + ", ".join(f"{_md(k)} {_fmt(v)}" for k, v in kinds.items()) for u, kinds in drops.items())]
    return out


def render_markdown(result, command_line=None):
    run = result["run"]
    notices = result.get("notices") or sorted({n for s in result["steps"] for n in s.get("notices", [])})
    out = [f"# Call quality report: `{_code(run['meeting_id'])}` ({_md(run['mode'])})", ""]
    out += [f"> **{_md(n.upper())}**" for n in notices] + ([""] if notices else [])
    overrides = result.get("config_overrides", [])
    if overrides:
        out += ["Config deviations from the shipped defaults:", ""] + [f"- `{_code(o)}`" for o in overrides]
    out += [""] if overrides else []
    out += [
        f"**Verdict: {_md(result['verdict'])}** (headline step `{_code(result['headline_step'])}`). "
        f"Bands `{_code(run['bands_version'])}` (proposals from calibration; Tony signs off). "
        f"Commit `{_code(run['commit'])}`.",
        "",
        "| Step | N | Verdict | Headline cell | Headline n |",
        "|---|---|---|---|---|",
    ]
    for s in result["steps"]:
        head = s["cells"].get(s["headline_cell"] or "UxU")
        out.append(f"| {_md(s['step_id'])}{' (headline)' if s['headline'] else ''} | {_fmt(s['n_target'])} | "
                   f"{_md(s['verdict'])} | {_fmt(s['headline_cell'])} | {head['n'] if head else 0} |")
    for s in result["steps"]:
        out += ["", f"## Step `{_code(s['step_id'])}`: {_md(s['verdict'])}", ""]
        out += [f"- **{_md(n)}**" for n in s.get("notices", [])]
        out += [f"- flag: {_md(f)}" for f in s.get("flags", [])]
        out += ["", "### Validity gates", ""]
        out += _gate_rows(s["validity"])
        out += ["", "### Quality gates", ""]
        out += _gate_rows(s["quality_gates"])
        out += ["", f"Dimension A status: {_md(s['dimension_A_status'])}.",
                "", _resolution(result["run"].get("scrape_step_s"))]
        if s["invalid_reasons"]:
            unmeasured = [g for g in s["invalid_reasons"] if g not in s.get("uncertain_gates", [])]
            if unmeasured:
                out.append(f"Blocking unmeasured gates: {_md(', '.join(unmeasured))}.")
        if s.get("uncertain_gates"):
            out.append("Uncertain gates (they differ between the lower and the upper reading of counters whose "
                       f"lineage the samples cannot prove): {_md(', '.join(s['uncertain_gates']))}.")
        out += ["", "### p95 per dimension per cell", "",
                "| Cell | n | " + " | ".join(DIM_LABELS) + " | staleness ms (diagnostic) |",
                "|---|---|" + "---|" * (len(DIM_LABELS) + 1)]
        for name, row in s["p95_table"].items():
            out.append(f"| {_md(name)} | {s['cells'][name]['n']} | "
                       + " | ".join(_fmt(row[d]) for d in DIM_LABELS) + f" | {_fmt(row['staleness_ms'])} |")
        sp = s["split"]
        out += ["", f"Split rate (unshaped receivers, unshaped talkers): {_fmt(sp['split_rate'])} "
                    f"({sp['split']} split, {sp['healthy']} healthy, {sp['none_received']} with no receiver)."]
        out += ["", "### Cells", ""]
        for name, cell in s["cells"].items():
            missing = cell["missing_required_dimensions"]
            out += [f"**{_md(name)}** (n={cell['n']}, transport mix {_md(cell['transport_mix'])}"
                    + (f", MISSING {_md(', '.join(missing))}" if missing else "")
                    + (", report-only (not gated)" if cell.get("report_only") else "") + ")", "",
                    "| Dimension | n | p50 | p95 | k in red |", "|---|---|---|---|---|"]
            for d, label in DIM_LABELS.items():
                dim = cell["dimensions"][d]
                note = f" ({_md(dim['excluded'])})" if dim.get("excluded") else ""
                out.append(f"| {label}{note} | {dim['n']} | {_fmt(dim['p50'])} | {_fmt(dim['p95'])} | "
                           f"{_fmt(dim['k_red'])} |")
            out.append(f"\nAbsent in this scorer version: {_md(', '.join(cell['absent_dimensions']))}.\n")
            out += _audio_terms(cell, name, s["diagnostics"].get("audio_counter_coverage", {}))
        out += ["### Participants (receivers and talkers)", "",
                "| User | Sessions in hold | New in hold | Migrations | Unplanned reconnects | Transport | Max presence gap (s) |",
                "|---|---|---|---|---|---|---|"]
        for user, st in sorted(s["stability"].items()):
            out.append(f"| {_md(user)} | {st['sessions_in_hold']} | {st['new_sessions_in_hold']} | "
                       f"{_fmt(st['migrations'])} | "
                       f"{_fmt(st['unplanned_reconnects'])} | "
                       f"{_md(_transport_text(s['transport'].get(user), s.get('transport_timeline', {}).get(user)))} | "
                       f"{_fmt(st['max_presence_gap_s'])} |")
        out += ["", "### Diagnostics (never gated)", ""]
        for k, v in s["diagnostics"].items():
            if k in ("correlated_freeze", "audio_delay", "audio_counter_coverage"):
                continue
            shown = _fmt(v) if not isinstance(v, list) else (_md(", ".join(map(str, v))) or "none")
            out.append(f"- {_md(k)}: {shown}")
        out += [f"- layer mix: {_md(s['layer_mix'])}"]
        out += _freeze_and_delay(s["diagnostics"])
        out += _relay_evidence(s["server_diagnostics"])
    fallbacks = result.get("transport_fallbacks") or {}
    if fallbacks:
        out += ["", "## Transport fallbacks: bots-app record against health reports", "",
                "| User | Preferred | Last elected | Fallbacks recorded | Fallbacks in health reports |", "|---|---|---|---|---|"]
        out += [f"| {_md(u)} | {_fmt(f['preferred'])} | {_fmt(f['observed'])} | {f['fallbacks']} | "
                f"{_fmt(f['fallbacks_in_health_reports']) if f['fallbacks_in_health_reports'] is not None else 'unknown'} |"
                for u, f in fallbacks.items()]
    if command_line:
        out += ["", "## Reproduction", "", "```", command_line.replace("`", "'"), "```"]
    return "\n".join(out) + "\n"
