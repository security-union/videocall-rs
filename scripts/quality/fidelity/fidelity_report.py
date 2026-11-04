import collections
import html
import json
import math
import os
import re
import secrets
import time

from fidelity_tolerance import EXEMPT, FAIL, NOT_RUN, PASS, _fmt, _pct, sizing_line

LAYER_KINDS = ("video layer 0", "video layer 1", "video layer 2")
VIDEO_NOT_RUN = "uplink bytes/s and keyframe size"
SCORED, NOT_MEASURED, NOT_APPLICABLE = "scored", "not measured", "n/a"
CLASS_ORDER = ("talker", "camera", "receive")
PATH_TEXT = re.compile(r"""(?<!\w)'[^']*/[^']*'|(?<!\w)"[^"]*/[^"]*"|(?<![\w./<-])(?:(?:~|\.{0,2})/\S+"""
                       r"""|(?![A-Z0-9_/]+(?![\w./-]))(?=[\d./-]*[A-Za-z_])[\w.-]+(?:/[\w.-]+){2,}/?)""")
RATIO_SPAN = 4

Axis = collections.namedtuple("Axis", "key group label measure kinds not_run")
AXES = (
    Axis("audio_pkts", "How often", "Audio packets", "uplink packets/s", ("audio",), ()),
    Axis("video_pkts", "How often", "Video packets", "uplink packets/s", LAYER_KINDS, ()),
    Axis("status", "How often", "Status reports", "uplink packets/s", ("heartbeat", "HEALTH", "RTT"), ()),
    Axis("key_rate", "How often", "Keyframe rate", "keyframe rate", LAYER_KINDS, ()),
    Axis("audio_data", "How much", "Audio data", "uplink bytes/s", ("audio",), ()),
    Axis("video_data", "How much", "Video data", "uplink bytes/s", LAYER_KINDS, (VIDEO_NOT_RUN,)),
    Axis("key_size", "How much", "Keyframe size", "keyframe size p50", LAYER_KINDS, (VIDEO_NOT_RUN,)),
    Axis("video_recv", "How much", "Video received", "downlink bytes/s", ("all",), ()),
    Axis("resend", "Reacting", "Resend requests", "uplink packets/s", ("KEYFRAME_REQUEST",), ()),
    Axis("bad_net", "Reacting", "Adapts to a bad network", "impairment arm", ("-",), ("impairment arm",)),
)
GROUPS = ("How often", "How much", "Reacting")
SAME_SOURCE = {"video_data", "key_size"}
TOP_LAYER_ONLY = {"key_size"}
WEAK_BELOW = 70
DIFFERENCE = {
    "audio_pkts": ("send more audio packets", "send fewer audio packets"),
    "video_pkts": ("send more video packets", "send fewer video packets"),
    "status": ("send more status reports", "send fewer status reports"),
    "key_rate": ("send keyframes more often", "send keyframes less often"),
    "audio_data": ("send more audio data", "send less audio data"),
    "video_data": ("send more video data", "send less video data"),
    "key_size": ("send larger keyframes", "send smaller keyframes"),
    "video_recv": ("receive more data", "receive less data"),
    "resend": ("send more KEYFRAME_REQUEST packets", "send fewer KEYFRAME_REQUEST packets"),
}
VERDICT_TEXT = {
    "FAIL": "The run is valid, and at least one row is outside the tolerance.",
    "INCOMPLETE": "The run is valid with no FAIL, but some rows were not measured.",
    "INVALID": "The comparison itself went wrong, so the numbers below are indicative only.",
    "ERROR": "The run did not end cleanly, so the numbers below are indicative only.",
}


def closeness(bot, browser):
    lo, hi = sorted((abs(bot), abs(browser)))
    return 100.0 if hi == 0 else 100.0 * lo / hi


def role_order(rows):
    roles = []
    for r in rows:
        if r["role"] != "room" and r["role"] not in roles:
            roles.append(r["role"])

    def key(role):
        m = re.search(r"\((\w+), n=", role)
        cls = m.group(1) if m else ""
        return (CLASS_ORDER.index(cls) if cls in CLASS_ORDER else len(CLASS_ORDER), role)
    return sorted(roles, key=key)


def axis_score(axis, rows, role):
    mine = [i for i, r in enumerate(rows) if r["role"] in (role, "room")]
    gaps = [i for i in mine if rows[i]["result"] == NOT_RUN
            and rows[i]["measure"] in axis.not_run + (axis.measure, "every measure")]
    if gaps:
        return {"state": NOT_MEASURED, "score": None, "rows": gaps, "note": rows[gaps[0]]["note"]}
    used = [i for i in mine if rows[i]["measure"] == axis.measure and rows[i]["kind"] in axis.kinds
            and rows[i]["bot"] is not None and rows[i]["browser"] is not None]
    if used and axis.key in TOP_LAYER_ONLY:
        used = [max(used, key=lambda i: rows[i]["kind"])]
    if used:
        bot = sum(rows[i]["bot"] for i in used)
        browser = sum(rows[i]["browser"] for i in used)
        return {"state": SCORED, "score": closeness(bot, browser), "bot": bot, "browser": browser,
                "unit": rows[used[0]]["unit"], "rows": used, "note": ""}
    return {"state": NOT_APPLICABLE, "score": None, "rows": [], "note": "neither side sends this"}


def mean(values):
    values = [v for v in values if v is not None]
    return sum(values) / len(values) if values else None


def summarise(cells):
    groups = {g: mean([c["score"] for a, c in zip(AXES, cells) if a.group == g]) for g in GROUPS}
    return {"overall": mean([c["score"] for c in cells]), "groups": groups}


def scores(result):
    rows = result["rows"]
    roles = role_order(rows)
    per_role = {role: [axis_score(a, rows, role) for a in AXES] for role in roles}
    combined = []
    for n, a in enumerate(AXES):
        cells = [per_role[role][n] for role in roles]
        scored = [c for c in cells if c["state"] == SCORED]
        if scored:
            state = SCORED
        elif any(c["state"] == NOT_MEASURED for c in cells):
            state = NOT_MEASURED
        else:
            state = NOT_APPLICABLE
        note = next((c["note"] for c in cells if c["state"] == NOT_MEASURED), "")
        combined.append({"state": state, "score": mean([c["score"] for c in scored]), "rows": sorted(
            {i for c in cells for i in c["rows"]}), "roles": len(scored), "note": note,
            "unmeasured": sum(c["state"] == NOT_MEASURED for c in cells)})
    used = {i for cells in per_role.values() for c in cells for i in c["rows"]}
    unscored = [i for i, r in enumerate(rows) if i not in used]
    return {"roles": roles, "per_role": per_role, "combined": combined, "summary": summarise(combined),
            "role_summary": {role: summarise(per_role[role]) for role in roles}, "unscored": unscored}


def headline(combined):
    ranked = sorted(((c["score"], a.label) for a, c in zip(AXES, combined) if c["state"] == SCORED),
                    key=lambda t: (-t[0], t[1]))
    if not ranked:
        return "Nothing was scored."
    best = ranked[:2]
    worst = [t for t in sorted(ranked, key=lambda t: (t[0], t[1]))[:3] if t not in best]
    text = "Closest to the browser: " + ", ".join(f"{label.lower()} ({s:.0f})" for s, label in best) + "."
    if worst:
        text += " Furthest: " + ", ".join(f"{label.lower()} ({s:.0f})" for s, label in worst) + "."
    missing = [a.label.lower() for a, c in zip(AXES, combined) if c["state"] == NOT_MEASURED]
    if missing:
        text += " Not measured: " + ", ".join(missing) + "."
    return text


STARVED = re.compile(r"^(\S+): video layer (\d) at ([\d.]+) packets/s, under the quiet baseline ([\d.]+)")
REASONS = (
    ("host", re.compile(r"^(host resources over the hold|no affirmative host resource verdict)")),
    ("capture", re.compile(r"^(capture:|the capture |stopping the capture|tcpdump)")),
    ("coverage", re.compile(r"^(\S+): traffic in (\d+%) of the hold")),
    ("layers", re.compile(r"^(\S+): published video layers")),
    ("foreign", re.compile(r"user\(s\) outside this run")),
    ("unidentified", re.compile(r"no decoded user id")),
    ("exited", re.compile(r"^(\S+) exited during the hold")),
    ("commits", re.compile(r"the stack runs|uncommitted product changes|checkout could not be read|git diff of")),
    ("silent", re.compile(r"received nothing|median traffic of")),
    ("mix", re.compile(r"no browser of its class|has no Rust role|nothing was compared")),
)


def plain_reasons(problems):
    """One line per known cause, starved browsers first, then by first appearance; an unknown reason as written."""
    found, unknown = collections.OrderedDict(), []
    for p in problems:
        m = STARVED.match(p)
        cause = "starved" if m else next((name for name, rx in REASONS if rx.search(p)), None)
        if cause is None:
            found.setdefault("other", [])
            unknown.append(p)
            continue
        found.setdefault(cause, []).append(m or next(rx for name, rx in REASONS if name == cause).search(p))
    fixed = {
        "host": "The machine was not shown to have spare CPU over the test, so starved browsers can't be ruled out"
                + (" (its CPU monitor missed readings)." if any("tick outside" in p for p in problems) else "."),
        "capture": "The traffic recording at the server is incomplete, so some counts may be short.",
        "foreign": "Someone outside the test used the server during it.",
        "unidentified": "Some traffic during the test could not be tied to anyone, so someone outside it may have "
                        "used the server.",
        "commits": "The server and the test tool may not be running the same code.",
        "silent": "Some bots and the browsers they are compared with received nothing during the test.",
        "mix": "The room's mix of bots and browsers does not allow a like-for-like comparison.",
    }
    lines = []
    for cause in sorted(found, key=lambda c: c != "starved"):
        hits = found[cause]
        names = ", ".join(m.group(1) for m in hits) if cause in ("coverage", "layers", "exited") else ""
        one = len(hits) == 1
        if cause == "starved":
            lines.append(starved_line(hits))
        elif cause == "coverage":
            lines.append(f"{names} had traffic in only {', '.join(m.group(2) for m in hits)} of the test.")
        elif cause == "layers":
            lines.append(f"{names} did not send every video layer {'it' if one else 'they'} should have.")
        elif cause == "exited":
            lines.append(f"{names} stopped during the test.")
        elif cause == "other":
            lines += unknown
        else:
            lines.append(fixed[cause])
    return lines


def starved_line(hits):
    starved = collections.OrderedDict()
    for m in hits:
        starved.setdefault(m.group(1), []).append(m.groups()[1:])
    same = len({tuple((h[0], h[2]) for h in layers) for layers in starved.values()}) == 1
    detail = "; ".join(f"{name} sent {'/'.join(h[1] for h in layers)} on video layers {'/'.join(h[0] for h in layers)}"
                       + ("" if same else f" (quiet: at least {'/'.join(h[2] for h in layers)})")
                       for name, layers in starved.items())
    if same:
        layers = next(iter(starved.values()))
        detail = (f"a quiet machine sends at least {'/'.join(h[2] for h in layers)} video packets/s on "
                  f"video layers {'/'.join(h[0] for h in layers)}; {detail}")
    return ("The reference browsers were starved of CPU: they sent fewer video packets per second than on a quiet "
            f"machine, so the yardstick itself is off ({detail}).")


def no_paths(text):
    return PATH_TEXT.sub("<path>", text)


def esc(value):
    return html.escape(str(value), quote=True)


def css_class(result):
    return re.sub(r"[^a-z]+", "-", str(result).lower()).strip("-")


def score_text(score):
    return "-" if score is None else f"{score:.0f}"


def value_text(v, unit):
    return f"{_fmt(v)} {unit}".strip()


def radar(cells, name):
    cx, cy, r, ring = 75.0, 50.0, 34.0, 41.0
    n = len(AXES)

    def at(i, v):
        a = -math.pi / 2 + i * 2 * math.pi / n
        return cx + math.cos(a) * r * v / 100, cy + math.sin(a) * r * v / 100

    def poly(values):
        return " ".join(f"{x:.2f},{y:.2f}" for x, y in (at(i, v) for i, v in values))
    svg = ['<svg viewBox="0 0 150 100" aria-hidden="true" focusable="false">']
    for v in (25, 50, 75):
        svg.append(f'<polygon class="grid" points="{poly((i, v) for i in range(n))}"/>')
    for i, c in enumerate(cells):
        x, y = at(i, 100)
        spoke = "spoke" if c["state"] == SCORED else "spoke off"
        svg.append(f'<line class="{spoke}" x1="{cx}" y1="{cy}" x2="{x:.2f}" y2="{y:.2f}"/>')
    svg.append(f'<polygon class="ref" points="{poly((i, 100) for i in range(n))}"/>')
    pts = [(i, max(c["score"], 1.5)) for i, c in enumerate(cells) if c["state"] == SCORED]
    if len(pts) >= 3:
        svg.append(f'<polygon class="bot" points="{poly(pts)}"/>')
    elif len(pts) == 2:
        svg.append(f'<polyline class="bot" points="{poly(pts)}"/>')
    for i, v in pts:
        x, y = at(i, v)
        svg.append(f'<circle class="dot" cx="{x:.2f}" cy="{y:.2f}" r="1.4"/>')
    svg.append("</svg>")
    labels = []
    for i, (a, c) in enumerate(zip(AXES, cells)):
        ang = -math.pi / 2 + i * 2 * math.pi / n
        left, top = (cx + math.cos(ang) * ring) / 150 * 100, (cy + math.sin(ang) * ring)
        tx, ty = -50 + 50 * math.cos(ang), -50 + 50 * math.sin(ang)
        value = score_text(c["score"]) if c["state"] == SCORED else c["state"]
        cls = "rl" if c["state"] == SCORED else "rl off"
        labels.append(f'<span class="{cls}" style="left:{left:.1f}%;top:{top:.1f}%;transform:translate('
                      f'{tx:.0f}%,{ty:.0f}%)"><span class="rl-num">{i + 1}</span><span class="rl-name">'
                      f'{esc(a.label)}</span> <b>{esc(value)}</b></span>')
    desc = "; ".join(f"{a.label} {score_text(c['score']) if c['state'] == SCORED else c['state']}"
                     for a, c in zip(AXES, cells))
    return (f'<div class="radar" role="img" aria-label="{esc(name)} closeness to the browser out of 100: '
            f'{esc(desc)}">{"".join(svg)}{"".join(labels)}</div>')


def axis_list(cells, rows):
    out = ['<ol class="axes">']
    for i, (a, c) in enumerate(zip(AXES, cells)):
        if c["state"] == SCORED:
            unit = rows[c["rows"][0]]["unit"] if c["rows"] else ""
            detail = (f"bot {esc(value_text(c['bot'], unit))} · browser {esc(value_text(c['browser'], unit))}"
                      if "bot" in c else f"mean of {c['roles']} role{'s' if c['roles'] != 1 else ''}"
                      + (f"; {c['unmeasured']} not measured" if c["unmeasured"] else ""))
            bar = f'<span class="meter"><i style="width:{max(c["score"], 0.5):.1f}%"></i></span>'
            score = f'<b class="num">{score_text(c["score"])}</b>'
        else:
            detail = esc(c["note"] or c["state"])
            bar, score = '<span class="meter off"></span>', f'<b class="num off">{esc(c["state"])}</b>'
        out.append(f'<li><span class="ax-num">{i + 1}</span><span class="ax-name">{esc(a.label)}<small>'
                   f'{esc(a.group)}</small></span>{bar}{score}<span class="ax-detail">{detail}</span></li>')
    out.append("</ol>")
    return "".join(out)


def ratio_pos(bot, browser):
    if browser == 0:
        return 50.0 if bot == 0 else 100.0
    if bot == 0:
        return 0.0
    x = max(-RATIO_SPAN, min(RATIO_SPAN, math.log2(bot / browser)))
    return 50 + x / RATIO_SPAN * 50


def bar(row):
    if row["result"] == NOT_RUN or row["bot"] is None or row["browser"] is None:
        return '<span class="track off"><em>not run</em></span>'
    lo, hi = ratio_pos(1 - row["tol"], 1), ratio_pos(1 + row["tol"], 1)
    pos = ratio_pos(row["bot"], row["browser"])
    left, width = min(pos, 50), abs(pos - 50)
    return (f'<span class="track"><span class="band" style="left:{lo:.1f}%;width:{hi - lo:.1f}%"></span>'
            f'<span class="mid"></span><span class="fill {css_class(row["result"])}" '
            f'style="left:{left:.1f}%;width:{width:.1f}%"></span><span class="pin {css_class(row["result"])}" '
            f'style="left:clamp(7px,{pos:.1f}%,calc(100% - 7px))"></span></span>')


def chip(result):
    return f'<span class="chip {css_class(result)}"><i></i>{esc(result)}</span>'


def row_table(rows, roles):
    ticks = "".join(f'<span style="left:{ratio_pos(v, 1):.1f}%">{t}</span>' for v, t in (
        (1 / 8, "⅛×"), (1 / 4, "¼×"), (1 / 2, "½×"), (1, "same"), (2, "2×"), (4, "4×"), (8, "8×")))
    out = []
    for role in roles + ["room"]:
        mine = [r for r in rows if r["role"] == role]
        if not mine:
            continue
        counts = collections.Counter(r["result"] for r in mine)
        tally = " · ".join(f"{counts[k]} {k}" for k in (FAIL, PASS, EXEMPT, NOT_RUN) if counts[k])
        title = "The whole room" if role == "room" else role
        out.append(f'<details class="role" open><summary><span>{esc(title)}</span><small>{esc(tally)}</small>'
                   f'</summary><div class="rows"><div class="r head"><span>Kind</span><span class="ticks">{ticks}'
                   '</span><span>Bot</span><span>Browser</span><span>Diff</span><span>Result</span></div>')
        section = None
        for r in mine:
            if (r["measure"], r["reference"]) != section:
                section = (r["measure"], r["reference"])
                ref = "" if r["reference"] == "-" else f" · against {r['reference']}"
                tol = "" if r["tol"] is None else f" · tolerance ±{r['tol']:.0%}"
                out.append(f'<div class="m">{esc(r["measure"])}{esc(ref)}{esc(tol)}</div>')
            note = f'<span class="note">{esc(r["note"])}</span>' if r["note"] else ""
            unit = f' <small>{esc(r["unit"])}</small>' if r["unit"] else ""
            out.append(f'<div class="r"><span class="k">{esc(r["kind"])}</span>{bar(r)}'
                       f'<span class="v vb"><small class="lab">bot </small>{esc(_fmt(r["bot"]))}{unit}</span>'
                       f'<span class="v vw"><small class="lab">browser </small>{esc(_fmt(r["browser"]))}{unit}</span>'
                       f'<span class="v d">{esc(_pct(r["diff"]))}</span>{chip(r["result"])}{note}</div>')
        out.append("</div></details>")
    return "".join(out)


def video_share(measures):
    parts = (measures or {}).get("participants") or {}
    total = sum(p.get("down_total", {}).get("Bps", 0) for p in parts.values())
    video = sum(v.get("Bps", 0) for p in parts.values() for k, v in p.get("down", {}).items()
                if k.startswith("video_L"))
    return video / total if total else None


def mapping_table(rows, unscored, share):
    out = ['<table class="map"><thead><tr><th>Axis</th><th>Table rows added up</th></tr></thead><tbody>']
    for a in AXES:
        kinds = "top video layer" if a.key in TOP_LAYER_ONLY else ", ".join(a.kinds)
        src = f"{a.measure}: {kinds}" if a.kinds != ("-",) else f"room: {a.measure}"
        extra = " (same source geometry)" if a.key in SAME_SOURCE else ""
        if a.key == "video_recv" and share is not None:
            extra = f" (video was {share:.0%} of all downlink in this run)"
        out.append(f"<tr><td>{esc(a.group)} · {esc(a.label)}</td><td>{esc(src)}{esc(extra)}</td></tr>")
    out.append("</tbody></table>")
    if unscored:
        listed = collections.OrderedDict()
        for i in unscored:
            r = rows[i]
            listed.setdefault((r["measure"], r["kind"]), []).append(r["result"])
        items = ""
        for (measure, kind), results in listed.items():
            items += f"<li>{esc(measure)}: {esc(kind)} ({esc(', '.join(sorted(set(results))))})</li>"
        out.append(f'<p>Rows on no axis: they are in the bars above and in the verdict, not in the score.</p>'
                   f'<ul class="plain">{items}</ul>')
    return "".join(out)


def differences(s):
    """(axis label, sentence) per axis weak for a role, weakest first."""
    found = []
    for n, a in enumerate(AXES):
        cells = [(role.split(" (")[0], s["per_role"][role][n]) for role in s["roles"]]
        weak = [(name, c) for name, c in cells if c["state"] == SCORED and int(score_text(c["score"])) < WEAK_BELOW]
        if not weak:
            continue
        clauses = []
        for side, what in zip((True, False), DIFFERENCE[a.key]):
            names = [name for name, c in weak if (c["bot"] > c["browser"]) == side]
            if names:
                clauses.append(f"{', '.join(names)} {what} than browsers")
        found.append((min(c["score"] for _, c in weak), a.label, "; ".join(clauses)))
    return [(label, text) for _, label, text in sorted(found, key=lambda t: t[:2])]


def render(doc, run=None):
    run = run or {}
    result, measures, facts = doc["result"], doc.get("measures") or {}, doc.get("facts") or {}
    rows, verdict = result["rows"], result["verdict"]
    s = scores(result)
    indicative = verdict in ("INVALID", "ERROR")
    window = measures.get("window") or run.get("window")
    when = time.strftime("%d %b %Y, %H:%M UTC", time.gmtime(window[0])) if window else "unknown"
    hold = f"{window[1] - window[0]:.0f} s" if window else "unknown"
    plan, fid = run.get("plan") or {}, run.get("fidelity") or {}
    setup = []
    if fid.get("browser_simulcast_max_layers"):
        setup.append(f"browsers asked for {fid['browser_simulcast_max_layers']} video layers")
    if plan.get("video_mode"):
        setup.append(f"Rust video_mode {plan['video_mode']}")
    if plan.get("bot_diagnostics"):
        setup.append(f"bot DIAGNOSTICS {plan['bot_diagnostics']}")
    counts = result.get("counts", {})
    tally = ", ".join(f"{counts.get(k, 0)} {k}" for k in (FAIL, PASS, EXEMPT, NOT_RUN))
    banner_title = {
        "INVALID": "These numbers can't be trusted",
        "ERROR": "This run did not finish cleanly",
        "FAIL": "Valid run: some rows are outside the tolerance",
        "INCOMPLETE": "Valid run: no failures, but not everything was measured",
    }.get(verdict, verdict)
    reasons = [no_paths(t) for t in plain_reasons(result.get("problems", []))]
    if verdict == "ERROR" and result.get("interrupted"):
        reasons.insert(0, f"The run ended in ERROR: {no_paths(result['interrupted'])}")
    raw = "".join(f"<li>{esc(no_paths(p))}</li>" for p in result.get("problems", []))
    banner = (f'<section class="banner {css_class(verdict)}" role="status"><p class="verdict">{chip(verdict)}'
              f'<span>{esc(tally)}</span></p><h2>{esc(banner_title)}</h2><p>{esc(VERDICT_TEXT.get(verdict, ""))}</p>'
              + (f'<ul>{"".join(f"<li>{esc(t)}</li>" for t in reasons)}</ul>' if reasons else "")
              + f'<p class="sizing"><b>Usable for sizing:</b> {esc(sizing_line(result))}</p>'
              + (f'<details><summary>The harness\'s own words</summary><ul class="raw">{raw}</ul></details>'
                 if raw else "") + "</section>")
    tag = '<span class="tag">indicative</span>' if indicative else ""
    summ = s["summary"]
    groups = "".join(f'<div class="tile"><span class="g">{esc(g)}</span>'
                     + (f'<b class="num">{score_text(summ["groups"][g])}</b>' if summ["groups"][g] is not None
                        else '<b class="num off">not measured</b>')
                     + f'<small>{esc(", ".join(a.label.lower() for a in AXES if a.group == g))}</small></div>'
                     for g in GROUPS)
    tabs, panels, css = [], [], []
    views = [("all", "All roles", s["combined"], summ)] + [
        (f"r{i}", role, s["per_role"][role], s["role_summary"][role]) for i, role in enumerate(s["roles"])]
    for n, (vid, name, cells, sm) in enumerate(views):
        checked = " checked" if n == 0 else ""
        tabs.append(f'<input type="radio" name="view" id="v-{vid}"{checked}><label for="v-{vid}">{esc(name)}</label>')
        css.append(f"#v-{vid}:checked ~ .panels .p-{vid}{{display:grid}}")
        overall = score_text(sm["overall"])
        what = ("each axis is the mean of the roles with a score" if vid == "all" else
                "this role's rows only")
        panels.append(f'<div class="panel p-{vid}"><div>{radar(cells, name)}<p class="fine">Overall for '
                      f'{esc(name)}: <b>{overall}</b>; {esc(what)}. A dotted spoke marks an axis with no score.</p>'
                      f'</div>{axis_list(cells, rows)}</div>')
    head_meta = [f"{when}", f"hold {hold}",
                 f"harness {(facts.get('harness_commit') or '?')[:12]}",
                 f"stack {(facts.get('stack_commit') or '?')[:12]}"] + setup
    share = video_share(measures)
    differs = "".join(f"<li><b>{esc(label)}</b>: {esc(text)}.</li>" for label, text in differences(s))
    if differs:
        differs = (f'<section class="means"><h2>Where the bots differ {tag}</h2><ul class="plain">{differs}</ul>'
                   f'<p class="fine">Axes scoring under {WEAK_BELOW} for a role.</p></section>')
    body_class = "indicative" if indicative else ""
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Fidelity {esc(doc.get('run_id', ''))}</title>
<style>{STYLE}{"".join(css)}</style>
</head>
<body class="{body_class}">
<main>
<header>
<p class="eyebrow">Bot vs browser fidelity</p>
<h1>{esc(doc.get('run_id', 'fidelity run'))}</h1>
<p class="meta">{" · ".join(esc(m) for m in head_meta)}</p>
<p class="meta">Reference: the browsers of each class in this room (canvas-clock camera, not a real camera).</p>
</header>
{banner}
<section class="score">
<div class="hero"><span class="g">Overall closeness {tag}</span><b class="big num">{score_text(summ['overall'])}</b>
<small>out of 100 · higher is closer to the browser</small></div>
<div class="tiles">{groups}</div>
<p class="headline">{esc(headline(s['combined']))}</p>
</section>
<section>
<h2>Where the bots are close, and where not</h2>
<p class="legend"><span><i class="sw ref"></i>Browser (100 on every axis)</span><span><i class="sw bot"></i>Rust bot
</span></p>
<div class="tabs">{"".join(tabs)}<div class="panels">{"".join(panels)}</div></div>
</section>
{differs}
<section>
<h2>Every row of the table</h2>
<p class="fine">Each bar is the bot's value against the browser's on a ratio scale: the centre line is "same", the
shaded band is the tolerance, left means the bot's value is lower. Values beyond 16× either way sit at the edge.</p>
{row_table(rows, s['roles'])}
</section>
<footer>
<h2>How the score is computed</h2>
<p>Each table row is already a role's median over its participants (keyframe sizes: over all its keyframes). An axis
adds up the bot values of its rows and the browser values of the same rows, then scores smaller ÷ larger × 100 (100
when both are zero, 0 when only one is). NOT RUN rows make an axis "not measured"; an axis with no rows for a role
is "n/a" (neither side sends it). "All roles" averages each axis over the roles with a score; the overall and group
scores average the scored axes, unweighted. The score is a summary; the "Usable for sizing" line says whether a
bot-only scale run at this commit can be used for sizing.</p>
{mapping_table(rows, s['unscored'], share)}
</footer>
</main>
</body>
</html>
"""


STYLE = """
:root{--bg:#F3F5F8;--surface:#FFFFFF;--fg:#172033;--muted:#586378;--line:#D9DFE8;--grid:#CCD4E0;
--ref:#1F63C2;--bot:#C4410C;--pass:#0A7F0A;--fail:#C42F2F;--exempt:#7C8597;--band:rgba(10,127,10,.14);
--warn-bg:#FFF1EE;--warn-line:#E4A493;color-scheme:light}
@media (prefers-color-scheme:dark){:root{--bg:#0F1522;--surface:#161E2F;--fg:#E8ECF3;--muted:#9EA9BC;
--line:#27324A;--grid:#2E3A55;--ref:#5A92E2;--bot:#E66A35;--pass:#3DBB3D;--fail:#F06A6A;--exempt:#8A93A6;
--band:rgba(61,187,61,.18);--warn-bg:#2A1A1A;--warn-line:#6E3B33;
color-scheme:dark}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,-apple-system,"Segoe UI",Roboto,
Arial,sans-serif}
main{max-width:1080px;margin:0 auto;padding:32px 16px 64px;display:flex;flex-direction:column;gap:40px}
h1{font-size:clamp(24px,4vw,36px);line-height:1.15;margin:0;overflow-wrap:anywhere}
h2{font-size:clamp(20px,3vw,26px);margin:0 0 12px}
section,footer{min-width:0}
.eyebrow{margin:0 0 6px;font-size:13px;font-weight:700;letter-spacing:.1em;text-transform:uppercase;color:var(--bot)}
.meta{margin:6px 0 0;color:var(--muted);font-size:14px}
.num{font-variant-numeric:tabular-nums}
.fine{font-size:14px;color:var(--muted)}
.banner{border:1px solid var(--line);border-left:6px solid var(--muted);background:var(--surface);
border-radius:12px;padding:20px 22px}
.banner.invalid,.banner.error,.banner.fail{border-color:var(--warn-line);border-left-color:var(--fail);
background:var(--warn-bg)}
.banner h2{margin:6px 0}
.banner p{margin:6px 0}
.banner ul{margin:8px 0;padding-left:20px}
.banner li{margin:4px 0}
.verdict{display:flex;gap:12px;align-items:center;flex-wrap:wrap;color:var(--muted);font-size:14px}
.raw{font-size:13px;color:var(--muted);overflow-wrap:anywhere}
summary{cursor:pointer}
summary:focus-visible,label:focus-visible,input:focus-visible+label{outline:2px solid var(--ref);outline-offset:3px}
.chip{display:inline-flex;align-items:center;gap:6px;font-size:13px;font-weight:700;white-space:nowrap}
.chip i{width:10px;height:10px;border-radius:50%;background:var(--exempt)}
.chip.pass i{background:var(--pass)}.chip.fail i,.chip.invalid i,.chip.error i{background:var(--fail)}
.chip.not-run i{background:none;border:2px dashed var(--exempt)}.chip.incomplete i{background:var(--bot)}
.score{display:grid;gap:16px;grid-template-columns:1fr}
.hero,.tile{background:var(--surface);border:1px solid var(--line);border-radius:12px;padding:18px 20px;
display:flex;flex-direction:column;gap:4px;min-width:0}
.g{font-size:13px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:var(--muted)}
.big{font-size:clamp(56px,10vw,88px);line-height:1}
.tile b{font-size:40px;line-height:1.1}.tile b.off{font-size:20px;line-height:44px;color:var(--muted)}
.tile small,.hero small{color:var(--muted);font-size:13px}
.tiles{display:grid;gap:12px;grid-template-columns:repeat(auto-fit,minmax(180px,1fr))}
.headline{margin:0;font-size:18px;font-weight:600}
.hero .g{display:flex;flex-wrap:wrap;gap:4px 8px;align-items:center}
.tag{padding:1px 8px;border-radius:999px;border:1px solid var(--fail);
color:var(--fail);font-size:11px;letter-spacing:.06em}
.indicative .big,.indicative .tile b,.indicative .axes .num{color:var(--muted)}
.indicative .radar .bot{stroke-dasharray:3 2}
.legend{display:flex;flex-wrap:wrap;gap:6px 20px;font-size:14px;color:var(--muted);margin:0 0 12px}
.legend span{display:inline-flex;align-items:center;gap:8px}
.sw{width:18px;height:4px;border-radius:2px;display:inline-block}.sw.ref{background:var(--ref)}
.sw.bot{background:var(--bot)}
.tabs>input{position:absolute;opacity:0;pointer-events:none}
.tabs>label{display:inline-flex;align-items:center;min-height:44px;padding:0 14px;margin:0 6px 8px 0;
border:1px solid var(--line);border-radius:999px;background:var(--surface);cursor:pointer;font-size:14px;
transition:background .15s,border-color .15s}
.tabs>label:hover{border-color:var(--muted)}
.tabs>input:checked+label{border-color:var(--fg);font-weight:700}
.panel{display:none;gap:20px;grid-template-columns:1fr;background:var(--surface);border:1px solid var(--line);
border-radius:12px;padding:16px;margin-top:4px}
.radar{position:relative;width:100%;max-width:640px;margin:0 auto;aspect-ratio:3/2}
.radar svg{position:absolute;inset:0;width:100%;height:100%;overflow:visible}
.radar .grid{fill:none;stroke:var(--grid);stroke-width:.3}
.radar .spoke{stroke:var(--grid);stroke-width:.3}.radar .spoke.off{stroke-dasharray:1 1}
.radar .ref{fill:none;stroke:var(--ref);stroke-width:.7;stroke-linejoin:round}
.radar .bot{fill:var(--bot);fill-opacity:.18;stroke:var(--bot);stroke-width:.7;stroke-linejoin:round}
.radar .dot{fill:var(--bot);stroke:var(--surface);stroke-width:.4}
.rl{position:absolute;font-size:13px;line-height:1.2;text-align:center;max-width:110px;color:var(--fg)}
.rl b{display:block;font-size:15px}
.rl.off{color:var(--muted)}.rl.off b{font-size:12px;font-weight:600}
.rl-num{display:none}
.axes{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:2px}
.axes li{display:grid;grid-template-columns:24px minmax(0,1fr) 60px;gap:2px 10px;align-items:center;
padding:8px 0;border-bottom:1px solid var(--line)}
.ax-num{font-size:12px;color:var(--muted);text-align:center}
.ax-name{font-weight:600}.ax-name small{display:block;font-weight:400;font-size:12px;color:var(--muted)}
.meter{grid-column:2;height:8px;border-radius:4px;background:var(--line);overflow:hidden}
.meter i{display:block;height:100%;background:var(--bot);border-radius:4px}
.meter.off{background:repeating-linear-gradient(135deg,var(--line) 0 4px,transparent 4px 8px)}
.axes .num{grid-column:3;grid-row:1/3;font-size:20px;text-align:right}
.axes .num.off{font-size:11px;font-weight:600;color:var(--muted)}
.ax-detail{grid-column:2/4;font-size:13px;color:var(--muted)}
.role{background:var(--surface);border:1px solid var(--line);border-radius:12px;margin:0 0 12px;padding:0 16px}
.role>summary{display:flex;flex-wrap:wrap;justify-content:space-between;gap:4px 12px;padding:14px 0;
font-weight:700;min-height:44px;align-items:center}
.role>summary small{font-weight:400;color:var(--muted)}
.rows{padding-bottom:12px}
.r{display:grid;grid-template-columns:minmax(0,1fr) auto auto;grid-template-areas:"k d c" "t t t" "b w w";
gap:4px 12px;align-items:center;padding:8px 0;border-top:1px solid var(--line);font-size:14px}
.r .k{grid-area:k;font-weight:600}.r .track{grid-area:t}.r .d{grid-area:d;text-align:right}
.r .chip{grid-area:c}.r .vb{grid-area:b}.r .vw{grid-area:w;text-align:right}
.r .note{grid-column:1/-1;font-size:12px;color:var(--muted)}
.r .v{font-variant-numeric:tabular-nums}.r small{color:var(--muted)}
.r.head{display:none}
.m{padding:12px 0 4px;font-size:13px;font-weight:700;color:var(--muted)}
.track{position:relative;display:block;height:18px;border-radius:4px;background:var(--bg);overflow:hidden}
.track.off{background:repeating-linear-gradient(135deg,var(--line) 0 4px,transparent 4px 8px)}
.track.off em{position:absolute;left:8px;top:0;font-size:12px;line-height:18px;color:var(--muted)}
.band{position:absolute;top:0;bottom:0;background:var(--band)}
.mid{position:absolute;left:50%;top:0;bottom:0;width:2px;margin-left:-1px;background:var(--ref)}
.fill{position:absolute;top:5px;height:8px;background:var(--exempt);opacity:.55}
.fill.pass{background:var(--pass)}.fill.fail{background:var(--fail)}
.pin{position:absolute;top:2px;width:14px;height:14px;margin-left:-7px;border-radius:50%;background:var(--exempt);
border:2px solid var(--surface)}
.pin.pass{background:var(--pass)}.pin.fail{background:var(--fail)}
.ticks{position:relative;display:block;height:16px;font-size:11px;color:var(--muted)}
.ticks span{position:absolute;transform:translateX(-50%);white-space:nowrap}
.means{background:var(--surface);border:1px solid var(--line);border-radius:12px;padding:20px 22px}
.means p{margin:0 0 12px}
.plain{margin:0;padding-left:20px}.plain li{margin:4px 0}
footer{border-top:1px solid var(--line);padding-top:24px;font-size:14px}
footer h2{font-size:20px}
.map{width:100%;border-collapse:collapse;font-size:13px;margin:12px 0}
.map th,.map td{text-align:left;vertical-align:top;padding:6px 8px;border-bottom:1px solid var(--line)}
.map th{color:var(--muted)}
@media (max-width:559px){.rl{max-width:none}.rl-name{display:none}
.rl-num{display:inline-block;font-size:11px;color:var(--muted);margin-right:2px}.rl b{display:inline;font-size:13px}}
@media (min-width:760px){.score{grid-template-columns:minmax(220px,300px) 1fr}.headline{grid-column:1/-1}
.r{grid-template-columns:176px minmax(0,1fr) 92px 92px 64px 92px;grid-template-areas:none}
.r .k,.r .track,.r .d,.r .chip,.r .vb,.r .vw{grid-area:auto}
.r .lab{position:absolute;width:1px;height:1px;overflow:hidden;clip-path:inset(50%);white-space:nowrap}
.r.head{display:grid;font-size:12px;color:var(--muted);border-top:0}
.r .v,.r .d{text-align:right}}
@media (min-width:1000px){.panel{grid-template-columns:minmax(0,1.4fr) minmax(300px,1fr);align-items:start}
.panel>div{position:sticky;top:16px}}
@media (prefers-reduced-motion:reduce){*{transition:none!important}}
"""


def write(run_dir):
    with open(os.path.join(run_dir, "fidelity.json"), encoding="utf-8") as fh:
        doc = json.load(fh)
    with open(os.path.join(run_dir, "run.json"), encoding="utf-8") as fh:
        run = json.load(fh)
    page = render(doc, run)
    path = os.path.join(run_dir, "report.html")
    tmp = f"{path}.{secrets.token_hex(8)}.tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o666)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(page)
        os.replace(tmp, path)
    except BaseException:
        os.remove(tmp)
        raise
    return path
