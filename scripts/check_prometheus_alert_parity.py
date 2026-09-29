#!/usr/bin/env python3
"""Fail the build when the shared Prometheus alert groups drift between the
three cluster values files, or when a cluster writes them somewhere Prometheus
never reads (#2713).

Three things are checked.

1. PARITY. us-east, hcl and hcl-daily-deployment must define the same relay,
   quality and resource alerts. Indentation, comments and blank lines are
   normalised away -- us-east nests its groups one level deeper, under a
   `prometheus:` subchart key, and that is cosmetic.

2. SELECTORS. The one allowed difference is the container name, which really
   does differ: every cluster deploys the relay charts under its own
   `fullnameOverride`, so the same container is `webtransport-us-east` on
   us-east and `videocall-webtransport` on both HCL clusters. Those names are
   not taken on trust -- each is verified against the deploy source that sets
   it, and each container regex is match-tested against the names it must
   cover. A selector naming a container no cluster deploys is a failure, which
   is the point: `rustlemania-webtransport` was registered here once and
   matched nothing on any cluster, so the rule using it was dead.

3. RULE-FILE WIRING. A values file may only put alert groups under a
   `serverFiles` key that the Prometheus server will actually load: either one
   named in its own `serverFiles."prometheus.yml".rule_files` override, or one
   of the upstream chart's defaults. The HCL overlays shipped their groups
   under `alert_rules.yml` with no override, and the chart default list does
   not contain it, so the live server loaded zero rule groups.

Usage:
  python3 scripts/check_prometheus_alert_parity.py            # the tracked files
  python3 scripts/check_prometheus_alert_parity.py A B C      # explicit files

Exit codes:
  0  every check passes
  1  drift, a missing group, a selector that matches nothing, or unloadable rules
  2  usage error (fewer than two files to compare, or an unknown file)
"""
from __future__ import annotations

import difflib
import re
import sys
from pathlib import Path

GUARDED_GROUPS = (
    "videocall_relay_alerts",
    "videocall_quality_alerts",
    "videocall_resource_alerts",
    # #2734: the one group that was unguarded, and the one that broke.
    "videocall_client_stability_alerts",
)

REPO_ROOT = Path(__file__).resolve().parent.parent

# `rule_files` entries the upstream prometheus-community/prometheus chart ships
# by default. A `serverFiles` key renders to /etc/config/<key>, so only these
# names are loaded when a values file does not override the list. Confirmed
# against the running hcl-daily server's /api/v1/status/config.
CHART_DEFAULT_RULE_FILES = (
    "recording_rules.yml",
    "alerting_rules.yml",
    "rules",
    "alerts",
)


class Cluster:
    """One cluster's prometheus values file and the relay containers it targets.

    `wt`/`ws` are the container names that cluster's deploy produces. The chart
    names a container after `rustlemania.fullname`, which returns
    `fullnameOverride` put through `trunc 63 | trimSuffix "-"` -- so for a name
    that is already short and has no trailing dash (asserted below) the override
    reaches the container verbatim.
    """

    def __init__(self, key, values, wt, ws, cpu_set, mem_set, name_sources):
        self.key = key
        self.values = values
        self.wt = wt
        self.ws = ws
        self.cpu_set = cpu_set
        self.mem_set = mem_set
        self.name_sources = name_sources


CLUSTERS = [
    Cluster(
        key="us-east",
        values="helm/global/us-east/prometheus/values.yaml",
        wt="webtransport-us-east",
        ws="websocket-us-east",
        cpu_set=".*-us-east|metrics-api.*|nats",
        mem_set=".*-us-east|metrics-api.*|nats|prometheus-server",
        name_sources=(
            "helm/global/us-east/webtransport/values.yaml",
            "helm/global/us-east/websocket/values.yaml",
        ),
    ),
    Cluster(
        key="hcl",
        values="helm/global/hcl/prometheus/values.yaml",
        wt="videocall-webtransport",
        ws="videocall-websocket",
        cpu_set="videocall-.*|metrics-api.*|nats",
        mem_set="videocall-.*|metrics-api.*|nats|prometheus-server",
        name_sources=(".github/workflows/daily-deploy-hcl.yaml",),
    ),
    Cluster(
        key="hcl-daily-deployment",
        values="helm/global/hcl-daily-deployment/prometheus/values.yaml",
        wt="videocall-webtransport",
        ws="videocall-websocket",
        cpu_set="videocall-.*|metrics-api.*|nats",
        mem_set="videocall-.*|metrics-api.*|nats|prometheus-server",
        name_sources=(".github/workflows/daily-deploy-hcl.yaml",),
    ),
]

BY_VALUES = {c.values: c for c in CLUSTERS}
DEFAULT_FILES = tuple(REPO_ROOT / c.values for c in CLUSTERS)

CPU_TOKEN = "<CPU_SET>"
MEM_TOKEN = "<MEMORY_SET>"


def cluster_for(path):
    p = str(path).replace("\\", "/")
    for values, cluster in BY_VALUES.items():
        if p.endswith(values):
            return cluster
    return None


# --------------------------------------------------------------------------
# 1. parity
# --------------------------------------------------------------------------
def extract_group(text, group):
    lines = text.splitlines()
    start = None
    indent = 0
    pattern = re.compile(r"^(\s*)- name:\s*%s\s*$" % re.escape(group))
    for i, line in enumerate(lines):
        m = pattern.match(line)
        if m:
            start = i
            indent = len(m.group(1))
            break
    if start is None:
        return None

    end = len(lines)
    for j in range(start + 1, len(lines)):
        line = lines[j]
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if len(line) - len(line.lstrip()) <= indent:
            end = j
            break
    return lines[start:end]


def normalise(block, cluster):
    """Strip comments, blanks and the block's common indent, then replace this
    cluster's own container selectors with cluster-neutral tokens. The
    substitution is keyed on the cluster, so a spelling belonging to a
    different cluster is NOT normalised away and surfaces as drift."""
    kept = [ln for ln in block if ln.strip() and not ln.lstrip().startswith("#")]
    if not kept:
        return []
    shift = min(len(ln) - len(ln.lstrip()) for ln in kept)
    subs = [
        ('container=~"%s"' % cluster.mem_set, MEM_TOKEN),
        ('container=~"%s"' % cluster.cpu_set, CPU_TOKEN),
    ]
    out = []
    for ln in kept:
        ln = ln[shift:].rstrip()
        for spelling, token in subs:
            ln = ln.replace(spelling, token)
        out.append(ln)
    return out


# --------------------------------------------------------------------------
# 2. selectors really name deployed containers
# --------------------------------------------------------------------------
def fullname_overrides(text):
    """Every fullnameOverride value a deploy source sets, in either spelling:
    a helm values key, or a `--set` on a helm upgrade line."""
    found = set()
    for m in re.finditer(r"""fullnameOverride:\s*["']?([\w.\-]+)["']?""", text):
        found.add(m.group(1))
    for m in re.finditer(r"""--set\s+fullnameOverride=([\w.\-]+)""", text):
        found.add(m.group(1))
    return found


def check_selectors(cluster, text):
    problems = []
    declared = set()
    for src in cluster.name_sources:
        f = REPO_ROOT / src
        if not f.is_file():
            problems.append(
                "%s: deploy source %s is missing, cannot verify container names."
                % (cluster.key, src)
            )
            continue
        declared |= fullname_overrides(f.read_text())

    for role, name in (("webtransport", cluster.wt), ("websocket", cluster.ws)):
        if name not in declared:
            problems.append(
                "%s: container %r is not set as a fullnameOverride in %s, so no "
                "deploy produces it. A selector naming it matches nothing."
                % (cluster.key, name, ", ".join(cluster.name_sources))
            )
        # helm applies `trunc 63 | trimSuffix "-"`; assert that is the identity
        # here rather than reimplementing it.
        if len(name) > 63 or name.endswith("-"):
            problems.append(
                "%s: %s name %r is truncated or dash-trimmed by helm, so the "
                "container name will not equal the override." % (cluster.key, role, name)
            )

    # PromQL regex matchers are fully anchored, so fullmatch is the right test.
    # #2727 un-excluded the WT relay from ContainerCPUHigh, so cpu_set must match
    # BOTH relays or the WT container silently loses CPU alerting.
    for name in (cluster.wt, cluster.ws):
        if not re.fullmatch(cluster.cpu_set, name):
            problems.append(
                "%s: cpu_set %r does not match container %r."
                % (cluster.key, cluster.cpu_set, name)
            )
    for name in (cluster.wt, cluster.ws):
        if not re.fullmatch(cluster.mem_set, name):
            problems.append(
                "%s: mem_set %r does not match container %r."
                % (cluster.key, cluster.mem_set, name)
            )

    for needed in (
        'container=~"%s"' % cluster.cpu_set,
        'container=~"%s"' % cluster.mem_set,
    ):
        if needed not in text:
            problems.append(
                "%s: expected selector %s is absent from %s; the cluster table "
                "and the values file disagree."
                % (cluster.key, needed, cluster.values)
            )
    return problems


# --------------------------------------------------------------------------
# 3. the rules are written where Prometheus will load them
# --------------------------------------------------------------------------
def server_files_children(text):
    """Direct child keys of `serverFiles:`, each with its own subtree lines."""
    lines = text.splitlines()
    start = None
    base = 0
    for i, line in enumerate(lines):
        if line.strip() == "serverFiles:":
            start = i
            base = len(line) - len(line.lstrip())
            break
    if start is None:
        return {}

    children = {}
    current = None
    for line in lines[start + 1:]:
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        indent = len(line) - len(line.lstrip())
        if indent <= base:
            break
        m = re.match(r"^\s*([\w.\-]+):\s*\|?-?\s*$", line)
        if indent == base + 2 and m:
            current = m.group(1)
            children[current] = []
        elif current is not None:
            children[current].append(line)
    return children


def check_rule_files(path, text):
    children = server_files_children(text)
    if not children:
        return ["%s: no serverFiles: block found." % path]

    loaded = list(CHART_DEFAULT_RULE_FILES)
    override = None
    for line in children.get("prometheus.yml", []):
        if re.match(r"^\s*rule_files:\s*$", line):
            override = []
            continue
        if override is not None:
            m = re.match(r"""^\s*-\s*["']?([\w./\-]+)["']?\s*$""", line)
            if m:
                override.append(m.group(1).rsplit("/", 1)[-1])
            elif line.strip() and not line.lstrip().startswith("-"):
                break
    if override:
        loaded = override

    problems = []
    for key, body in children.items():
        if key == "prometheus.yml":
            continue
        if not any(re.match(r"^\s*groups:\s*$", ln) for ln in body):
            continue
        if key not in loaded:
            problems.append(
                "%s: alert groups are under serverFiles key %r, which renders to "
                "/etc/config/%s -- not in the rule_files this server loads (%s). "
                "Prometheus will read ZERO groups from it. Use one of those "
                "names, or override serverFiles.\"prometheus.yml\".rule_files."
                % (path, key, key, ", ".join(loaded))
            )
    return problems


# --------------------------------------------------------------------------
def main(argv):
    paths = [Path(p) for p in argv[1:]] or list(DEFAULT_FILES)
    if len(paths) < 2:
        print("ERROR: need at least two files to compare.", file=sys.stderr)
        return 2

    texts = []
    clusters = []
    for p in paths:
        if not p.is_file():
            print("ERROR: file not found: %s" % p, file=sys.stderr)
            return 1
        c = cluster_for(p)
        if c is None:
            print(
                "ERROR: %s is not a known cluster values file. Add it to "
                "CLUSTERS before guarding it." % p,
                file=sys.stderr,
            )
            return 2
        texts.append(p.read_text())
        clusters.append(c)

    failures = []
    for p, text, c in zip(paths, texts, clusters):
        failures.extend(check_selectors(c, text))
        failures.extend(check_rule_files(str(p), text))

    base_path, base_text, base_cluster = paths[0], texts[0], clusters[0]
    for group in GUARDED_GROUPS:
        base_block = extract_group(base_text, group)
        if base_block is None:
            failures.append("MISSING group %s in %s" % (group, base_path))
            continue
        base_norm = normalise(base_block, base_cluster)

        for other_path, other_text, other_cluster in zip(
            paths[1:], texts[1:], clusters[1:]
        ):
            other_block = extract_group(other_text, group)
            if other_block is None:
                failures.append("MISSING group %s in %s" % (group, other_path))
                continue
            other_norm = normalise(other_block, other_cluster)
            if other_norm == base_norm:
                continue
            diff = "\n".join(
                difflib.unified_diff(
                    base_norm,
                    other_norm,
                    fromfile="%s :: %s" % (base_path, group),
                    tofile="%s :: %s" % (other_path, group),
                    lineterm="",
                )
            )
            failures.append("DRIFT in group %s:\n%s" % (group, diff))

    if failures:
        print("Prometheus alert-group parity FAILED.\n")
        for f in failures:
            print(f)
            print()
        print(
            "The guarded groups must be identical across clusters apart from "
            "the container selectors in CLUSTERS, every selector must name a "
            "container some deploy actually produces, and the groups must live "
            "under a serverFiles key the server loads.",
            file=sys.stderr,
        )
        return 1

    print(
        "Prometheus alert-group parity OK: %s identical across %d file(s); "
        "selectors match deployed containers; rule groups sit under a "
        "serverFiles key the server reads. This checks the KEY, not the "
        "PromQL -- check_prometheus_rules_parse.py parses the expressions."
        % (", ".join(GUARDED_GROUPS), len(paths))
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
