#!/usr/bin/env python3
"""Compare the Call Quality dashboard with a scored run on a live Prometheus (#2985).

Score with --no-pseudonymise so participant ids match.

Usage: call_quality_dashboard_parity.py --manifest run.json --result result.json --prom-url URL
         [--dashboard PATH] [--step STEP_ID] [--auth-bearer-env VAR | --auth-basic-env U:P]
Exit:  0 every value within tolerance  1 outside tolerance or missing  2 usage or query error
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE / "quality"))

import cq_manifest  # noqa: E402
import cq_prom  # noqa: E402
from check_call_quality_dashboard import DASHBOARD, DIMS, tagged_targets  # noqa: E402

ABS_TOL = 0.005
REL_TOL = 0.05


def within(dashboard, scorer):
    return abs(dashboard - scorer) <= max(ABS_TOL, REL_TOL * abs(scorer))


def textbox(value):
    """Grafana's Prometheus escaping of a single-value variable."""
    return value.replace("\\", "\\\\").replace("'", "\\\\'")


def interpolate(expr, variables):
    def sub(m):
        name = m.group(1) or m.group(2)
        return variables[name] if name in variables else m.group(0)
    return re.sub(r"\$\{(\w+)\}|\$(\w+)", sub, expr)


def unshaped(manifest, step):
    netem = {u for e in manifest.get("events", []) if e["step_id"] == step["step_id"] and e["action"] == "netem"
             for u in e["participants"]}
    return sorted(p["user_id"] for p in manifest["participants"] if p["user_id"] and step["step_id"] in p["steps"]
                  and not p["network"]["shaped"] and p["user_id"] not in netem)


def variables_for(manifest, step):
    parts = [p for p in manifest["participants"] if p["user_id"] and step["step_id"] in p["steps"]]
    span = int(round(step["hold_end"] - step["hold_start"]))
    return {
        "participants": textbox(cq_prom.any_of_regex(unshaped(manifest, step))),
        "meeting": manifest["meeting_id"],
        "obs": textbox(cq_prom.any_of_regex({p["user_id"] for p in parts if p["observer"]})),
        "talkers": textbox(cq_prom.any_of_regex({p["user_id"] for p in parts if p["talker"]})),
        "__range": f"{span}s",
        "__range_s": str(span),
    }


def panel_queries(dash):
    out = []
    for _, target, dim, role in tagged_targets(dash):
        if role in ("p95", "table"):
            out.append((dim, role, target["expr"]))
    return out


def evaluate(client, expr, at):
    series = client.range(expr, at, at, 15)
    return {labels.get("participant"): samples[-1][1] for labels, samples in series if samples}


def compare(step_result, measured, s_participants):
    cell = step_result["headline_cell"]
    if not cell:
        return [], ["the scored step has no headline cell"]
    scored = {u: {d: v for d, v in vals.items() if d != "S"}
              for u, vals in step_result["cells"][cell]["participants"].items()}
    for u in s_participants:
        scored.setdefault(u, {})["S"] = step_result["stability"][u]["S"]
    p95 = dict(step_result["p95_table"][cell])
    g_q9 = next((g for g in step_result["quality_gates"] if g["gate"] == "G-Q9"), {})
    p95["S"] = (g_q9.get("value") or {}).get("p95")
    rows, failures = [], []
    for dim in DIMS:
        pairs = [(f"p95 {dim}", p95.get(dim), measured.get((dim, "p95"), {}).get(None))]
        got = measured.get((dim, "table"), {})
        pairs += [(f"{u} {dim}", vals[dim], got.get(u)) for u, vals in sorted(scored.items()) if dim in vals]
        for name, want, have in pairs:
            ok = want is not None and have is not None and within(have, want)
            rows.append((name, want, have, ok))
            if not ok:
                failures.append(f"{name}: dashboard {have!r} vs scorer {want!r}")
    return rows, failures


def main(argv=None, transport=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--manifest", required=True)
    p.add_argument("--result", required=True)
    p.add_argument("--prom-url", required=True)
    p.add_argument("--dashboard", type=Path, default=DASHBOARD)
    p.add_argument("--step", help="step_id (default: the headline step)")
    p.add_argument("--auth-bearer-env", metavar="VAR")
    p.add_argument("--auth-basic-env", metavar="USER_VAR:PASS_VAR")
    args = p.parse_args(argv)
    try:
        manifest = cq_manifest.load_manifest(args.manifest)
        result = json.loads(Path(args.result).read_text(encoding="utf-8"))
        dash = json.loads(args.dashboard.read_text(encoding="utf-8"))
        step_id = args.step or result["headline_step"]
        step = next(s for s in manifest["steps"] if s["step_id"] == step_id)
        step_result = next(s for s in result["steps"] if s["step_id"] == step_id)
        headers = cq_prom.auth_headers(args.auth_bearer_env, args.auth_basic_env)
        client = cq_prom.PromClient(args.prom_url, headers, transport or cq_prom.urllib_transport)
        variables = variables_for(manifest, step)
        measured = {}
        for dim, kind, expr in panel_queries(dash):
            measured[(dim, kind)] = evaluate(client, interpolate(expr, variables), step["hold_end"])
    except (OSError, ValueError, KeyError, StopIteration, cq_manifest.ManifestError, cq_prom.PromError) as exc:
        print(f"error: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 2
    rows, failures = compare(step_result, measured, unshaped(manifest, step))
    print(f"step {step_id}, hold {step['hold_start']:.0f}..{step['hold_end']:.0f}, "
          f"tolerance max({ABS_TOL}, {REL_TOL:.0%} of the scorer value)")
    print(f"{'value':<40} {'scorer':>12} {'dashboard':>12} {'delta':>10}")
    for name, want, have, ok in rows:
        delta = f"{have - want:+.5f}" if want is not None and have is not None else "-"
        fmt = (lambda v: "missing" if v is None else f"{v:.5f}")
        print(f"{name:<40} {fmt(want):>12} {fmt(have):>12} {delta:>10} {'ok' if ok else 'OUTSIDE'}")
    if failures or not rows:
        print(f"FAIL: {len(failures)} value(s) outside tolerance or missing", file=sys.stderr)
        return 1
    print(f"OK: {len(rows)} values within tolerance")
    return 0


if __name__ == "__main__":
    sys.exit(main())
