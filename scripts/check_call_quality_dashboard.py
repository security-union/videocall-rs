#!/usr/bin/env python3
"""Fail when the Call Quality dashboard drifts from the scorer's definitions (#2985).

Every dimension query must equal the expression derived here from scripts/quality/cq_score.py
and its config; scripts/test_call_quality_dashboard_semantics.py checks those expressions
against the scorer on synthetic series.

Usage: check_call_quality_dashboard.py [--dashboard PATH] [--config OVERRIDE.json]
Exit:  0 consistent  1 drift  2 usage or unreadable input
"""
from __future__ import annotations

import argparse
import json
import math
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent
sys.path.insert(0, str(HERE / "quality"))

import cq_score  # noqa: E402

DASHBOARD = REPO_ROOT / "helm/grafana/dashboards/call-quality.json"
DIMS = ("A", "V", "Q", "S")
ROLES = ("series", "hold", "p95", "k", "table")
MEETING = 'meeting_id="$meeting"'
MEETING_QUERY = f"label_values({cq_score.M_PEER_INFO}, meeting_id)"
LOOKBACK = "5m"
HOUR_S = 3600
TAG = re.compile(r"\bcq:(?:([AVQS]):(series|hold|p95|k)|(table))\b")
STR = r""""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`[^`]*`"""
TOKEN = re.compile(STR + r"|\[[^\]]*\]|\$\{?\w+\}?|\{(?:" + STR + r"""|[^}"'`])*\}"""
                   r"|[A-Za-z_:][A-Za-z0-9_:]*|[0-9.]+(?:[eE][-+]?[0-9]+)?[A-Za-z0-9]*|\S")
MATCHER = re.compile(r"\s*([A-Za-z_]\w*)\s*(=~|!~|!=|=)\s*(" + STR + r")\s*(?:,|$)")
LABEL_LISTS = {"by", "without", "on", "ignoring", "group_left", "group_right"}
NOT_SELECTORS = {"and", "or", "unless", "bool", "offset", "atan2", "inf", "nan"}
SUBQUERY_STEP = re.compile(r"\[[^\]:]*:\s*([^\]]*?)\s*\]")
OFFSET = re.compile(r"\boffset\s+(-?\d+[a-z]+)")
GRAFANA_BUILTIN = {"type": "grafana", "uid": "-- Grafana --"}
OTHER_QUANTILE = re.compile(r"\b(?:quantile_over_time|histogram_quantile)\s*\(")


def selectors(expr):
    """[(metric name or None, [(label, op, quoted value)] or None if unparsable)] for every vector selector."""
    out, tokens = [], [m.group() for m in TOKEN.finditer(expr)]
    i = 0
    while i < len(tokens):
        tok, nxt = tokens[i], tokens[i + 1] if i + 1 < len(tokens) else ""
        if tok in LABEL_LISTS:
            i = tokens.index(")", i) + 1 if nxt == "(" and ")" in tokens[i:] else i + 1
            continue
        named = re.fullmatch(r"[A-Za-z_:][A-Za-z0-9_:]*", tok) and tok.lower() not in NOT_SELECTORS
        if named and nxt not in ("(", "by", "without"):
            out.append((tok, matchers(nxt) if nxt.startswith("{") else []))
            i += 2 if nxt.startswith("{") else 1
            continue
        if tok.startswith("{"):
            out.append((None, matchers(tok)))
        i += 1
    return out


def matchers(braces):
    body, out = braces[1:-1].strip().rstrip(","), []
    pos = 0
    while pos < len(body):
        m = MATCHER.match(body, pos)
        if not m:
            return None
        out.append(m.groups())
        pos = m.end()
    return out


def num(value):
    return f"{value:g}"


def normalised(expr):
    return re.sub(r"\s+", "", expr)


def sel(metric, *matchers):
    return metric + "{" + ",".join((MEETING,) + matchers) + "}"


def canonical(cfg):
    """{(dim, role): PromQL} built from the scorer's metric names and config."""
    cv = cq_score.cv
    step = f"{num(cv(cfg, 'sampling', 'scrape_step_s'))}s"
    nominal = num(cv(cfg, "dimensions", "nominal_talker_pps"))
    full = num(cv(cfg, "dimensions", "expand_ops_full_scale"))
    near = num(cv(cfg, "dimensions", "near_frozen_fps"))
    obs, parts = 'from_peer=~"$obs"', 'peer_id=~"$participants"'
    pair = "session_id, from_peer, to_peer"
    expand, pps = sel(cq_score.M_EXPAND, obs), sel(cq_score.M_PPS, obs)
    listen, fps = sel(cq_score.M_CAN_LISTEN, obs), sel(cq_score.M_FPS, obs)
    freeze = sel(cq_score.M_FREEZE, obs)
    sent, reelect = sel(cq_score.M_SENT, parts), sel(cq_score.M_REELECT, 'result="proceeded"')

    observed = sel(cq_score.M_SENT, 'peer_id=~"$obs"')
    births = (f"min by (session_id) (min_over_time(timestamp({observed})[{LOOKBACK}:{step}] @ end() offset $__range)"
              f" or min_over_time(timestamp({observed})[$__range:{step}] @ end()))")

    alive = f"max by (peer_id, session_id) ({observed}) * 0 + on (session_id) group_left () {births}"
    newest_session = f"topk by (peer_id) (1, {alive})"

    def newest(qty):
        return f"({qty} and on (session_id) {newest_session})"

    info = sel(cq_score.M_PEER_INFO, 'peer_id=~"$talkers"')
    talkers = f'max by (to_peer) (label_replace({info}, "to_peer", "$1", "session_id", "(.*)"))'
    gap = f"clamp_min(1 - {pps} / {nominal}, 0)"
    bad = f"((clamp_max({expand}, {full}) / {full} > {gap}) or {gap})"
    a_sample = newest(f"((({bad} and on ({pair}) {listen} == 1) or on ({pair}) ({bad} * 0 + 1))"
                      f" * on (to_peer) group_left () {talkers})")

    prev = f"last_over_time({freeze}[{LOOKBACK}] offset {step})"
    inc = newest(f"(({freeze} - {prev}) >= 0 or {freeze} * 1)")
    decoding = newest(f"({num(cv(cfg, 'sampling', 'scrape_step_s'))} * ({fps} > bool 0))")
    exposure = f'max by (from_peer, to_peer) (label_replace({inc}, "term", "freeze", "", "") or {decoding})'
    decoded = newest(f"({fps} > bool 0)")
    near_frozen = newest(f"(({fps} > bool 0) * ({fps} < bool {near}))")

    every = f"max by (peer_id, session_id) (last_over_time({sent}[$__range]))"
    before = f"max by (peer_id, session_id) (last_over_time({sent}[$__range] offset $__range))"
    new = f"(count by (peer_id) ({every} unless on (session_id) {before}) or count by (peer_id) ({every}) * 0)"
    joined = (f"(count by (peer_id) ({every}) * 0 + 1 unless on (peer_id) count by (peer_id) ({before})"
              f" or count by (peer_id) ({every}) * 0)")
    proceeded = (f"(sum by (peer_id) ((last_over_time({reelect}[$__range]) - ({reelect} offset $__range"
                 f" or last_over_time({reelect}[$__range]) * 0)) * on (session_id) group_left (peer_id)"
                 f" {every}) or count by (peer_id) ({every}) * 0)")

    def per_observer(dim, win):
        if dim == "A":
            return (f"sum by (from_peer) (sum_over_time({a_sample}[{win}:{step}]))\n/\n"
                    f"sum by (from_peer) (count_over_time({a_sample}[{win}:{step}]))")
        if dim == "V":
            return (f"sum by (from_peer) (sum_over_time({inc}[{win}:{step}]))\n/\n"
                    f"(sum by (from_peer) (sum_over_time({exposure}[{win}:{step}])) > 0)")
        if dim == "Q":
            return (f"sum by (from_peer) (sum_over_time({near_frozen}[{win}:{step}]))"
                    f"\n/\n(sum by (from_peer) (sum_over_time({decoded}[{win}:{step}])) > 0)")
        return f"clamp_min({new} - {joined} - {proceeded}, 0)\n/ ($__range_s / {HOUR_S})"

    out = {}
    for dim in DIMS:
        hold = per_observer(dim, "$__range")
        red = num(cfg["bands"][dim]["red"])
        key = "peer_id" if dim == "S" else "from_peer"
        if dim != "S":
            out[(dim, "series")] = per_observer(dim, "$__interval")
        out[(dim, "hold")] = f"sort_desc(\n{hold}\n)"
        out[(dim, "p95")] = f"quantile(0.95,\n{hold}\n)"
        out[(dim, "k")] = f"sum(\n{hold}\n>= bool {red}\n)"
        out[(dim, "table")] = f'max by (participant) (label_replace(\n{hold}\n, "participant", "$1", "{key}", "(.*)"))'
    return out


def datasources(node, path="dashboard"):
    if isinstance(node, dict):
        for k, v in node.items():
            if k == "datasource":
                yield path, v
            yield from datasources(v, f"{path}.{k}")
    elif isinstance(node, list):
        for i, v in enumerate(node):
            yield from datasources(v, f"{path}[{i}]")


def table_transforms(panel):
    ts = panel.get("transformations") or []
    if [t.get("id") for t in ts] != ["merge", "organize"]:
        return [f"transformations {[t.get('id') for t in ts]} != ['merge', 'organize']"]
    want = {f"Value #{t.get('refId')}": t.get("refId") for t in panel.get("targets", [])}
    got = ts[1].get("options", {}).get("renameByName")
    return [] if got == want else [f"renameByName {got} != {want}"]


def thresholds_for(dim, role, cfg):
    red = cfg["bands"][dim]["red"]
    if role == "k":
        return [1, cq_score.cv(cfg, "quality_gates", "k_red_fail")]
    return [math.nextafter(red, math.inf) if role == "p95" else red]


def panels(node):
    for p in node.get("panels", []) or []:
        yield p
        yield from panels(p)


def tagged_targets(dash):
    """(panel, target, dim, role) for every target of a panel carrying a cq: tag."""
    for p in panels(dash):
        for dim, role, table in TAG.findall(p.get("description") or ""):
            for t in p.get("targets", []):
                yield p, t, (t.get("refId") if table else dim), (role or "table")


def scorer_families(cfg):
    stub = {"meeting_id": "m"}
    step = {"join_start": 0.0, "hold_start": 60.0, "hold_end": 600.0}
    return {name for _, expr, _, _ in cq_score.step_queries(stub, step, cfg, None)
            for name, _ in selectors(expr) if name}


def panel_thresholds(panel, dim, role):
    if role != "table":
        return panel.get("fieldConfig", {}).get("defaults", {}).get("thresholds")
    for o in panel.get("fieldConfig", {}).get("overrides", []):
        if o.get("matcher") == {"id": "byName", "options": dim}:
            for prop in o.get("properties", []):
                if prop.get("id") == "thresholds":
                    return prop.get("value")
    return None


def check(dash, cfg):
    problems = []
    step = cq_score.cv(cfg, "sampling", "scrape_step_s")
    fetched = scorer_families(cfg)
    want = canonical(cfg)
    seen = set()

    refresh = re.fullmatch(r"(\d+)([smhd])", str(dash.get("refresh") or "0s"))
    if not refresh or 0 < int(refresh[1]) * {"s": 1, "m": 60, "h": 3600, "d": 86400}[refresh[2]] < step:
        problems.append(f"refresh {dash.get('refresh')!r}: must be off or at least {step}s")
    variables = {v.get("name"): v for v in dash.get("templating", {}).get("list", [])}
    meeting = variables.get("meeting", {})
    query = meeting.get("query")
    if meeting.get("type") != "query" or (query.get("query") if isinstance(query, dict) else query) != MEETING_QUERY:
        problems.append(f"$meeting must be a query variable over {MEETING_QUERY}")

    for path, node in datasources(dash):
        if isinstance(node, str) or (isinstance(node, dict) and node.get("uid") and node != GRAFANA_BUILTIN):
            problems.append(f"{path}: datasource {node!r} is not portable: use a type with no uid or name")

    for p in panels(dash):
        where = f"panel {p.get('id')} ({p.get('title')})"
        for t in p.get("targets", []):
            expr = t.get("expr", "")
            found = selectors(expr)
            for name, labels in found:
                what = name or "{...}"
                if name is None or any(label == "__name__" for label, _, _ in labels or []):
                    problems.append(f"{where}: {what} selector must name its metric literally")
                elif name not in fetched:
                    problems.append(f"{where}: {name} is not fetched by cq_score.step_queries")
                if [m for m in labels or [] if m[0] == "meeting_id"] != [("meeting_id", "=", '"$meeting"')]:
                    problems.append(f"{where}: {what} selector must carry exactly {MEETING}")
            if {n for n, _ in found} & set(cq_score.PAIR_METRICS) and "by (from_peer)" not in expr:
                problems.append(f"{where}: per-pair series must be reduced by (from_peer)")
            for s in SUBQUERY_STEP.findall(expr):
                if s != f"{step}s":
                    problems.append(f"{where}: subquery step {s or '(default)'} != scrape_step_s {step}s")
            for o in OFFSET.findall(expr):
                if o != f"{step}s":
                    problems.append(f"{where}: offset {o} != scrape_step_s {step}s")
            if OTHER_QUANTILE.search(expr):
                problems.append(f"{where}: only quantile(0.95, ...) across observers is the scorer's p95")

    for p in panels(dash):
        if any(table for _, _, table in TAG.findall(p.get("description") or "")):
            problems += [f"panel {p.get('id')} ({p.get('title')}): {x}" for x in table_transforms(p)]
    for p, t, dim, role in tagged_targets(dash):
        where = f"panel {p.get('id')} ({p.get('title')}) target {t.get('refId')}"
        if (dim, role) not in want:
            problems.append(f"{where}: no scorer-derived expression for {dim} {role}")
            continue
        seen.add((dim, role))
        if normalised(t.get("expr", "")) != normalised(want[(dim, role)]):
            problems.append(f"{where}: {dim} {role} query differs from the scorer-derived expression")
        steps = [s.get("value") for s in (panel_thresholds(p, dim, role) or {}).get("steps", [])
                 if s.get("value") is not None]
        if steps != thresholds_for(dim, role, cfg):
            problems.append(f"{where}: thresholds {steps} != {thresholds_for(dim, role, cfg)} "
                            f"(bands.{dim}.red, p95 > red, k_red_fail)")
    for key in sorted(set(want) - seen):
        problems.append(f"no panel tagged cq:{key[0]}:{key[1]}" if key[1] != "table"
                        else f"the cq:table panel has no {key[0]} target")

    texts = [p.get("options", {}).get("content", "") for p in panels(dash) if p.get("type") == "text"]
    pending = any(cq_score.LATENCY_PENDING in t for t in texts)
    if cq_score.latency_metric(cfg) is None and not pending:
        problems.append(f"no text panel says {cq_score.LATENCY_PENDING!r} while the latency metric is unset")
    elif cq_score.latency_metric(cfg) is not None and pending:
        problems.append("latency_gate.audio_delay_metric is configured but the dashboard still says L is pending")
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dashboard", type=Path, default=DASHBOARD)
    parser.add_argument("--config", help="JSON deep-merged over scripts/quality/call_quality_config.json")
    args = parser.parse_args(argv)
    try:
        dash = json.loads(args.dashboard.read_text(encoding="utf-8"))
        cfg = cq_score.load_config(args.config)
    except (OSError, ValueError, cq_score.ConfigError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    if not isinstance(dash, dict):
        print(f"error: {args.dashboard} is not a dashboard object", file=sys.stderr)
        return 2
    problems = check(dash, cfg)
    for line in problems:
        print(f"{args.dashboard.name}: {line}")
    if problems:
        return 1
    print(f"OK: {args.dashboard.name} matches the scorer's sources, constants and bands")
    return 0


if __name__ == "__main__":
    sys.exit(main())
