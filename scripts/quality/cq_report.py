"""Markdown rendering of a call quality score result."""

import re

DIM_LABELS = {
    "A": "Audio: concealed share of talker audio",
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
        out += ["", f"Dimension A status: {_md(s['dimension_A_status'])}."]
        if s["invalid_reasons"]:
            out.append(f"Blocking unmeasured gates: {_md(', '.join(s['invalid_reasons']))}.")
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
        out += ["### Participants (receivers and talkers)", "",
                "| User | Sessions in hold | New in hold | Migrations | Unplanned reconnects | Transport | Max presence gap (s) |",
                "|---|---|---|---|---|---|---|"]
        for user, st in sorted(s["stability"].items()):
            out.append(f"| {_md(user)} | {st['sessions_in_hold']} | {st['new_sessions_in_hold']} | "
                       f"{_fmt(st['migrations'])} | "
                       f"{_fmt(st['unplanned_reconnects'])} | {_md(s['transport'].get(user, '–'))} | "
                       f"{_fmt(st['max_presence_gap_s'])} |")
        out += ["", "### Diagnostics (never gated)", ""]
        for k, v in s["diagnostics"].items():
            shown = _fmt(v) if not isinstance(v, list) else (_md(", ".join(map(str, v))) or "none")
            out.append(f"- {_md(k)}: {shown}")
        out += [f"- layer mix: {_md(s['layer_mix'])}", f"- server: {_md(s['server_diagnostics'])}"]
    if command_line:
        out += ["", "## Reproduction", "", "```", command_line.replace("`", "'"), "```"]
    return "\n".join(out) + "\n"
