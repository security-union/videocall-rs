#!/usr/bin/env python3
"""Fail when an alert rule or Grafana dashboard queries a metric nothing registers (#2922).

A query on an unregistered name evaluates empty, which reads as "healthy".

Usage: check_dashboard_metric_refs.py [--root DIR]
Exit:  0 every reference resolves  1 an unregistered metric is referenced  2 usage
"""
from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

RULE_GLOBS = (
    "docker/monitoring/prometheus/alert_rules.yml",
    "helm/global/*/prometheus/values.yaml",
)
DASHBOARD_GLOBS = (
    "helm/grafana/dashboards/*.json",
)
PRUNED_DIRS = {".git", ".claude", "target", "node_modules", "dist"}
QUERY_KEYS = {"expr", "query", "definition"}
HISTOGRAM_SUFFIXES = ("_bucket", "_sum", "_count")

TOKEN = re.compile(r"(?<![A-Za-z0-9_:$])((?:videocall|relay|meeting)_[A-Za-z0-9_]+)")
STRING_LITERAL = re.compile(r'"(?:[^"\\]|\\.)*"|\'(?:[^\'\\]|\\.)*\'')
REGISTER_CALL = re.compile(r"\bregister_\w+!\s*\(")
# A collector names its metric in the first argument of one of these.
CONSTRUCTOR_CALL = re.compile(r"\b(?:Desc|Opts|HistogramOpts)::new\s*\(")
RUST_STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')
# `const NAMES: [(&str, ...); N] = [("metric_name", ...), ...]` feeding a collector.
STR_TUPLE_TABLE = re.compile(r"\b(?:const|static)\s+\w+\s*:\s*\[\s*\(\s*&(?:'static\s+)?str\b")
TUPLE_FIRST_STRING = re.compile(r'\(\s*"((?:[^"\\]|\\.)*)"')
LABEL_ARRAY = re.compile(r"(?:&|vec!)\[([^\]]*)\]")
EXPR_LINE = re.compile(r"^(\s*)(?:-\s+)?expr:\s*(.*)$")


def rust_files(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in PRUNED_DIRS]
        for name in filenames:
            if name.endswith(".rs"):
                yield Path(dirpath) / name


def call_body(text, open_idx, open_ch="(", close_ch=")"):
    """Text between the bracket at `open_idx` and its balancing close, skipping strings."""
    depth, i = 0, open_idx
    while i < len(text):
        ch = text[i]
        if ch == '"':
            i += 1
            while i < len(text) and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
        elif ch == open_ch:
            depth += 1
        elif ch == close_ch:
            depth -= 1
            if depth == 0:
                return text[open_idx + 1 : i]
        i += 1
    return text[open_idx + 1 :]


def table_names(text):
    """First string of each tuple in a `[(&str, ...)]` const/static table."""
    names = set()
    for m in STR_TUPLE_TABLE.finditer(text):
        assign = text.find("= [", m.end())
        if assign < 0:
            continue
        body = call_body(text, assign + 2, "[", "]")
        names.update(TUPLE_FIRST_STRING.findall(body))
    return names


def registered(root):
    """(metric names, label names) declared by `register_*!` calls and by
    `Desc::new` / `Opts::new` / `HistogramOpts::new`: a literal first argument, or,
    when the name is not a literal, the `[(&str, ...)]` tables in the same file."""
    names, labels = set(), set()
    for path in rust_files(root):
        text = path.read_text(encoding="utf-8", errors="replace")
        computed_name = False
        for pattern, literal_first in ((REGISTER_CALL, False), (CONSTRUCTOR_CALL, True)):
            for m in pattern.finditer(text):
                body = call_body(text, m.end() - 1)
                first = RUST_STRING.search(body)
                if not literal_first or body.lstrip().startswith('"'):
                    if first:
                        names.add(first.group(1))
                else:
                    computed_name = True
                for array in LABEL_ARRAY.findall(body):
                    labels.update(RUST_STRING.findall(array))
        if computed_name:
            names.update(table_names(text))
    return names, labels


def rule_expressions(text):
    """Every `expr:` value in a Prometheus rule file or a chart's embedded rules."""
    lines = text.splitlines()
    out = []
    for i, line in enumerate(lines):
        m = EXPR_LINE.match(line)
        if not m:
            continue
        indent, value = len(m.group(1)), m.group(2).strip()
        if value[:1] not in ("|", ">"):
            if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
                value = value[1:-1]
            out.append(value)
            continue
        block = []
        for nxt in lines[i + 1 :]:
            if nxt.strip() and len(nxt) - len(nxt.lstrip()) <= indent:
                break
            block.append(nxt)
        out.append("\n".join(block))
    return out


def dashboard_expressions(node):
    if isinstance(node, dict):
        for key, value in node.items():
            if key in QUERY_KEYS and isinstance(value, str):
                yield value
            else:
                yield from dashboard_expressions(value)
    elif isinstance(node, list):
        for item in node:
            yield from dashboard_expressions(item)


def referenced(expr):
    return set(TOKEN.findall(STRING_LITERAL.sub('""', expr)))


def resolves(token, names, labels):
    if token in names or token in labels:
        return True
    return any(
        token.endswith(s) and token[: -len(s)] in names for s in HISTOGRAM_SUFFIXES
    )


def expressions_by_file(root):
    for pattern in RULE_GLOBS:
        for path in sorted(root.glob(pattern)):
            yield path, rule_expressions(path.read_text(encoding="utf-8"))
    for pattern in DASHBOARD_GLOBS:
        for path in sorted(root.glob(pattern)):
            doc = json.loads(path.read_text(encoding="utf-8"))
            yield path, list(dashboard_expressions(doc))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    args = parser.parse_args(argv)
    root = args.root.resolve()

    names, labels = registered(root)
    if not names:
        print(f"error: no registered metric under {root}", file=sys.stderr)
        return 2

    scanned, dead = 0, []
    for path, exprs in expressions_by_file(root):
        scanned += 1
        for token in sorted(set().union(*map(referenced, exprs)) if exprs else ()):
            if not resolves(token, names, labels):
                dead.append((path.relative_to(root), token))
    if scanned == 0:
        print(f"error: no alert rule or dashboard file under {root}", file=sys.stderr)
        return 2

    for path, token in dead:
        print(f"{path}: {token} is not registered")
    if dead:
        return 1
    print(f"OK: {scanned} files, every referenced metric is registered")
    return 0


if __name__ == "__main__":
    sys.exit(main())
