#!/usr/bin/env python3
"""Fail when a Grafana dashboard JSON is not shipped by the dashboards ConfigMap (#2985).

Usage: check_dashboard_configmap_keys.py [--root DIR]
Exit:  0 every dashboard is shipped  1 a dashboard is missing or mis-keyed  2 usage
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DASHBOARDS = "helm/grafana/dashboards"
CONFIGMAP = "helm/grafana/templates/dashboards-configmap.yaml"
EXEMPT = {"e2e-scoreboard.json": "scripts/deploy-e2e-scoreboard.sh deploys it through the Grafana API"}
KEY = re.compile(r"^  ([^\s:]+):\s*\|-?\s*$")
FILES_GET = re.compile(r'\.Files\.Get\s+"dashboards/([^"]+)"')


def keys(text):
    lines = text.splitlines()
    out = {}
    for i, line in enumerate(lines):
        m = KEY.match(line)
        if m:
            nxt = FILES_GET.search(lines[i + 1]) if i + 1 < len(lines) else None
            out[m.group(1)] = nxt.group(1) if nxt else None
    return out


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    args = parser.parse_args(argv)
    root = args.root.resolve()
    try:
        shipped = keys((root / CONFIGMAP).read_text(encoding="utf-8"))
    except OSError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    files = {p.name for p in (root / DASHBOARDS).glob("*.json")}
    if not files:
        print(f"error: no dashboard under {root / DASHBOARDS}", file=sys.stderr)
        return 2
    problems = [f"{name}: no `{name}: |-` key in {CONFIGMAP}" for name in sorted(files - set(shipped) - set(EXEMPT))]
    for key, source in sorted(shipped.items()):
        if source != key:
            problems.append(f"{CONFIGMAP}: key {key} reads {source!r}, not dashboards/{key}")
        elif source not in files:
            problems.append(f"{CONFIGMAP}: key {key} reads a file that does not exist (renders empty)")
        elif key in EXEMPT:
            problems.append(f"{CONFIGMAP}: {key} is exempt ({EXEMPT[key]}) and must not be shipped here")
    for line in problems:
        print(line)
    if problems:
        return 1
    print(f"OK: {len(shipped)} of {len(files)} dashboards shipped, {len(files) - len(shipped)} exempt")
    return 0


if __name__ == "__main__":
    sys.exit(main())
