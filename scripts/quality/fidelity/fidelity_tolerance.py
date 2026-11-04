"""Bot-vs-browser tolerance table, evaluated per Rust role against browsers of its class."""

import collections
import statistics

from fidelity_measures import CLASSES, KINDS, percentile

PKT_TOL = 0.15
BYTES_TOL = 0.25
KEYFRAME_TOL = 0.30
DOWNLINK_TOL = 0.25
IMPAIR_TOL = 0.30
ROOM_TOL = 0.20
MINOR_SHARE = 0.05
WS_WRITE_BUFFER_B = 32 * 1024
LAYERS = ("0", "1", "2")
PASS, FAIL, EXEMPT, NOT_RUN = "PASS", "FAIL", "EXEMPT", "NOT RUN"
EXIT = {"PASS": 0, "FAIL": 1, "INCOMPLETE": 2, "ERROR": 3, "INVALID": 4}
VIDEO_KINDS = tuple(f"video_L{n}" for n in LAYERS)
KIND_LABEL = {**{f"video_L{n}": f"video layer {n}" for n in LAYERS}, "RTT": "RTT"}
NEVER_MINOR = {"KEYFRAME_REQUEST"}
NOT_RUN_ROWS = (
    ("impairment arm", IMPAIR_TOL, "not measured: this harness does not run an impairment arm"),
    ("room level", ROOM_TOL, "relay room bytes, NATS deliveries and relay keyframe requests need a bot-only and a "
     "browser-only room of the same population; this run is one mixed room"),
)
SIZING = (("uplink bytes/s", ("video layer",)), ("downlink bytes/s", ("all",)), ("uplink packets/s", ("KEYFRAME",)))


def rel_diff(bot, ref):
    """(bot - ref) / ref; 0.0 when both are zero, None when only the reference is zero."""
    if ref == 0:
        return 0.0 if bot == 0 else None
    return (bot - ref) / ref


def judge(bot, ref, tol, minor=False):
    diff = rel_diff(bot, ref)
    if diff is not None and abs(diff) <= tol + 1e-9:
        return diff, PASS
    return diff, EXEMPT if minor else FAIL


def median(values):
    return statistics.median(values) if values else 0.0


def _row(rows, role, ref, measure, kind, unit, bot, browser, tol, result, diff, note=""):
    rows.append({"role": role, "reference": ref, "measure": measure, "kind": kind, "unit": unit, "bot": bot,
                 "browser": browser, "tol": tol, "diff": diff, "result": result, "note": note})


def _per_kind(rows, role, ref_name, bots, refs):
    def kind_value(group, k):
        return median([p["up"].get(k, {}).get("pps", 0.0) for p in group])

    bot_total = sum(kind_value(bots, k) for k in KINDS)
    ref_total = sum(kind_value(refs, k) for k in KINDS)
    for k in KINDS:
        b, r = kind_value(bots, k), kind_value(refs, k)
        if b == 0 and r == 0:
            continue
        minor = k not in NEVER_MINOR and (bot_total == 0 or b / bot_total < MINOR_SHARE) and (
            ref_total == 0 or r / ref_total < MINOR_SHARE)
        diff, res = judge(b, r, PKT_TOL, minor)
        _row(rows, role, ref_name, "uplink packets/s", KIND_LABEL.get(k, k), "pkt/s", b, r, PKT_TOL, res, diff,
             "under 5 % of the role's packets on both sides" if res == EXEMPT else "")


def _bytes(rows, role, ref_name, bots, refs, kinds):
    for k in kinds:
        b = median([p["up"].get(k, {}).get("Bps", 0.0) for p in bots])
        r = median([p["up"].get(k, {}).get("Bps", 0.0) for p in refs])
        if b == 0 and r == 0:
            continue
        diff, res = judge(b, r, BYTES_TOL)
        _row(rows, role, ref_name, "uplink bytes/s", KIND_LABEL.get(k, k), "kbps", b * 8 / 1000, r * 8 / 1000,
             BYTES_TOL, res, diff)


def _keyframe_sizes(rows, role, ref_name, bots, refs):
    for layer in LAYERS:
        bs = [s for p in bots for s in p["keyframes"].get(layer, {}).get("sizes", [])]
        rs = [s for p in refs for s in p["keyframes"].get(layer, {}).get("sizes", [])]
        if not bs and not rs:
            continue
        for q, label in ((0.5, "keyframe size p50"), (0.9, "keyframe size p90")):
            b = percentile(bs, q) if bs else 0
            r = percentile(rs, q) if rs else 0
            diff, res = judge(b, r, KEYFRAME_TOL)
            side_ok = (b > WS_WRITE_BUFFER_B) == (r > WS_WRITE_BUFFER_B)
            note = "" if side_ok else "on the other side of the 32 KiB WS write-buffer threshold"
            _row(rows, role, ref_name, label, f"video layer {layer}", "KB", b / 1000, r / 1000, KEYFRAME_TOL,
                 res if side_ok else FAIL, diff, note)


def _keyframe_rates(rows, role, ref_name, bots, refs):
    for layer in LAYERS:
        b = median([p["keyframes"].get(layer, {}).get("per_s", 0.0) for p in bots])
        r = median([p["keyframes"].get(layer, {}).get("per_s", 0.0) for p in refs])
        if b == 0 and r == 0:
            continue
        diff, res = judge(b, r, KEYFRAME_TOL)
        _row(rows, role, ref_name, "keyframe rate", f"video layer {layer}", "/s", b, r, KEYFRAME_TOL, res, diff)


def _names(group):
    return "+".join(sorted({p["role"] for p in group})) + f" (n={len(group)})"


def evaluate(measures, problems=(), receive=None):
    """Rows and verdict. INVALID beats FAIL beats INCOMPLETE (a NOT RUN row) beats PASS. receive: {Rust role: note
    for its downlink row}."""
    parts, receive = measures["participants"], receive or {}
    rows, problems = [], list(problems) + list(measures.get("problems", []))
    rust_roles = sorted({p["role"] for p in parts.values() if p["fleet"] == "rust"})
    for role in rust_roles:
        bots = [p for p in parts.values() if p["fleet"] == "rust" and p["role"] == role]
        cls = bots[0]["class"]
        refs = [p for p in parts.values() if p["fleet"] == "browser" and p["class"] == cls]
        label = f"{role} ({cls}, n={len(bots)})"
        if not refs:
            _row(rows, label, "-", "every measure", "-", "", None, None, None, NOT_RUN, None,
                 f"no browser of class {cls} in the room")
            problems.append(f"Rust role {role} ({cls}) has no browser of its class to compare")
            continue
        _per_kind(rows, label, _names(refs), bots, refs)
        _bytes(rows, label, _names(refs), bots, refs, ("audio",))
        if cls != "receive":
            geo = collections.Counter(p.get("geometry") for p in bots).most_common(1)[0][0]
            same = [p for p in refs if geo is not None and p.get("geometry") == geo]
            if same:
                ref_geo = f"{_names(same)} {geo}"
                _bytes(rows, label, ref_geo, bots, same, VIDEO_KINDS)
                _keyframe_sizes(rows, label, ref_geo, bots, same)
            else:
                _row(rows, label, _names(refs), "uplink bytes/s and keyframe size", "video", "", None, None,
                     BYTES_TOL, NOT_RUN, None, f"no browser {cls} with a {geo} source; the browsers have "
                     f"{', '.join(sorted({str(p.get('geometry')) for p in refs}))}")
            _keyframe_rates(rows, label, _names(refs), bots, refs)
        b = median([p["down_total"]["Bps"] for p in bots])
        r = median([p["down_total"]["Bps"] for p in refs])
        if b == 0 and r == 0:
            _row(rows, label, _names(refs), "downlink bytes/s", "all", "kbps", 0.0, 0.0, DOWNLINK_TOL, NOT_RUN, None,
                 "no downlink traffic on either side")
            problems.append(f"Rust role {role} and its browsers received nothing in the hold")
        else:
            diff, res = judge(b, r, DOWNLINK_TOL)
            _row(rows, label, _names(refs), "downlink bytes/s", "all", "kbps", b * 8 / 1000, r * 8 / 1000,
                 DOWNLINK_TOL, res, diff, receive.get(role, ""))
    for cls in CLASSES:
        if any(p["fleet"] == "browser" and p["class"] == cls for p in parts.values()) and not any(
                p["fleet"] == "rust" and p["class"] == cls for p in parts.values()):
            problems.append(f"browser class {cls} has no Rust role to compare")
    if not any(r["result"] in (PASS, FAIL, EXEMPT) for r in rows):
        if any(r["measure"] != "every measure" for r in rows):
            problems.append("nothing was compared: the median traffic of the Rust roles and their browsers was zero")
        else:
            problems.append("nothing was compared: the room needs at least one Rust role and a browser of its class")
    for measure, tol, note in NOT_RUN_ROWS:
        _row(rows, "room", "-", measure, "-", "", None, None, tol, NOT_RUN, None, note)
    counts = {k: sum(1 for r in rows if r["result"] == k) for k in (PASS, FAIL, EXEMPT, NOT_RUN)}
    blockers = sorted({f"{m} {r['kind'] if r['kind'] != 'all' else ''}".strip() for r in rows if r["result"] == FAIL
                       for m, kinds in SIZING if r["measure"] == m and r["kind"].startswith(kinds)})
    if problems:
        verdict = "INVALID"
    elif counts[FAIL]:
        verdict = "FAIL"
    elif counts[NOT_RUN]:
        verdict = "INCOMPLETE"
    else:
        verdict = "PASS"
    return {"verdict": verdict, "exit_code": EXIT[verdict], "counts": counts, "problems": problems,
            "sizing_blockers": blockers, "rows": rows}


def _fmt(v):
    if v is None:
        return "-"
    return f"{v:.3g}" if abs(v) < 10 else f"{v:.1f}" if abs(v) < 1000 else f"{v:.0f}"


def _pct(v):
    return "-" if v is None else f"{v:+.0%}" if abs(v) < 10 else f"{v:+.1e}"


def table_lines(result, markdown=False):
    head = ["Rust role", "Browser", "Measure", "Kind", "Bot", "Browser", "Unit", "Diff", "Tol", "Result", "Note"]
    body = [[r["role"], r["reference"], r["measure"], r["kind"], _fmt(r["bot"]), _fmt(r["browser"]), r["unit"],
             _pct(r["diff"]), "-" if r["tol"] is None else f"±{r['tol']:.0%}", r["result"], r["note"]]
            for r in result["rows"]]
    if markdown:
        return ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)] + [
            "| " + " | ".join(c.replace("|", "/") for c in row) + " |" for row in body]
    widths = [max(len(x[i]) for x in [head] + body) for i in range(len(head) - 1)]
    return ["  ".join(c.ljust(w) for c, w in zip(row[:-1], widths)) + "  " + row[-1] for row in [head] + body]


def sizing_line(result):
    if result["verdict"] in ("INVALID", "ERROR"):
        return f"no, the run is {result['verdict']}"
    gaps = sorted({f"{r['measure']} for {r['role'].split(' (')[0]}" for r in result["rows"]
                   if r["result"] == NOT_RUN and r["role"] != "room"})
    not_run = "NOT RUN: " + "; ".join(gaps)
    if result["sizing_blockers"]:
        return "no, FAIL on " + "; ".join(result["sizing_blockers"]) + (f"; {not_run}" if gaps else "")
    if gaps:
        return f"not shown, {not_run}"
    return "not blocked by video bytes, downlink or KEYFRAME_REQUEST"


def render(result, header, markdown=False):
    c = result["counts"]
    lines = list(header) + [
        f"Verdict: {result['verdict']} ({c[FAIL]} FAIL, {c[PASS]} PASS, {c[EXEMPT]} EXEMPT, {c[NOT_RUN]} NOT RUN)",
        "Usable for sizing: " + sizing_line(result),
        ""] + table_lines(result, markdown)
    if result["problems"]:
        lines += ["", "Why the run is INVALID:"] + [f"- {p}" for p in result["problems"]]
    return "\n".join(lines) + "\n"


def retest(a, b, tol=0.05):
    """Per-participant agreement of two runs' measures on every reported rate (test-retest)."""
    rows = []
    for name in sorted(set(a["participants"]) & set(b["participants"])):
        pa, pb = a["participants"][name], b["participants"][name]
        for d in ("up", "down"):
            ta, tb = pa[f"{d}_total"], pb[f"{d}_total"]
            for field, unit, scale in (("pps", "pkt/s", 1), ("Bps", "kbps", 8 / 1000)):
                diff, res = judge(tb[field], ta[field], tol)
                rows.append({"participant": name, "direction": d, "kind": "total", "unit": unit,
                             "a": ta[field] * scale, "b": tb[field] * scale, "diff": diff, "result": res})
                for k in KINDS:
                    va = pa[d].get(k, {}).get(field, 0.0)
                    vb = pb[d].get(k, {}).get(field, 0.0)
                    if va == 0 and vb == 0:
                        continue
                    minor = (va / ta[field] < MINOR_SHARE if ta[field] else True) and (
                        vb / tb[field] < MINOR_SHARE if tb[field] else True)
                    diff, res = judge(vb, va, tol, minor)
                    rows.append({"participant": name, "direction": d, "kind": KIND_LABEL.get(k, k), "unit": unit,
                                 "a": va * scale, "b": vb * scale, "diff": diff, "result": res})
    missing = sorted(set(a["participants"]) ^ set(b["participants"]))
    silent = sorted(n for m in (a, b) for n, p in m["participants"].items()
                    if not p["up_total"]["pps"] or not p["down_total"]["pps"])
    agree = bool(rows) and not missing and not silent and all(r["result"] != FAIL for r in rows)
    return {"agree": agree, "missing": missing, "silent": silent, "tol": tol, "rows": rows}
