#!/usr/bin/env python3
"""Fail the build when a daily-deploy relay step drifts in the env vars, env
indices or resources it passes to helm (#2715).

`--set env[N]` replaces the whole list, so a relay step's `--set` block is the
complete environment that relay runs with: a var the step omits is absent at
runtime, not inherited from the chart. check-deploy-workflow-parity.sh compares
step names only and could not see that.

Checks, each tied to a defect that shipped:
  EXTRACTION  both relay steps present, >= MIN_ENV_NAMES env names each
  CONTIGUITY  env indices are exactly 0..N-1 (a hole renders `- null`)
  NAMES       every relay step carries REQUIRED_ENV_NAMES
  VALUE       SERVICE_TYPE names the transport the step deploys
  RESOURCES   all four resources.{requests,limits}.{cpu,memory} are set, and
              the WT step clears WT_CPU_FLOORS and WT_MEMORY_REQUEST_CEILING
  PORTS       WT service.port, LISTEN_URL and the UI's webTransportHost agree
  COMMAS      no --set value carries an unescaped comma for helm to split on

Parity: hcl and labsworkspace must agree exactly, per service. Ascend is
compared against hcl through ASCEND_ALLOWED; anything outside it is drift.

Usage:
  python3 scripts/check_deploy_env_parity.py           # the tracked workflows
  python3 scripts/check_deploy_env_parity.py A B C     # explicit files

Exit codes:
  0  every check passes
  1  drift, a missing var, a hole in the env indices, or missing resources
  2  usage error, or the extractor found no steps / too few env names
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# Keyed by the cluster token in daily-deploy-<cluster>.yaml.
CLUSTERS = ("hcl", "labsworkspace", "ascend")
REFERENCE = "hcl"
STRICT_PEERS = ("labsworkspace",)

SERVICES = {
    "webtransport": "Deploy WebTransport Server",
    "websocket": "Deploy WebSocket Server",
}

REQUIRED_ENV_NAMES = (
    "SERVICE_TYPE",
    "REGION",
    "SERVER_STATS_INTERVAL_SECS",
    "MEMBERSHIP_MIRROR_ENABLED",
)

REQUIRED_RESOURCE_KEYS = (
    "requests.cpu",
    "requests.memory",
    "limits.cpu",
    "limits.memory",
)

# Floors for the WT relay only. #2727 shards it across available_parallelism()
# arbiters, which FLOORS the cgroup CPU quota to whole cores -- hence
# WHOLE_CORE_KEYS below. hcl and labsworkspace give it requests.cpu 500m; the
# FLOOR stays 200m so it cannot force that on ascend, whose node budget is not
# verified here. See docs/DEPLOYMENT_CONFIG_MAP.md.
WT_CPU_FLOORS = {
    "limits.cpu": ("2000m", "a lower ceiling floors available_parallelism() and "
                            "disables #2727 session sharding"),
    "requests.cpu": ("200m", "a smaller request leaves the relay less CFS weight "
                             "when the node is contended"),
}

# hcl-daily and labsworkspace are single shared nodes; a request above measured
# use takes capacity from every other tenant. Ascend is not held to it.
WT_MEMORY_REQUEST_CEILING = ("128Mi", ("hcl", "labsworkspace"))

# Must be whole cores: available_parallelism() rounds the quota DOWN, so a
# fractional limit clears the floor and still wastes the remainder. SCOPE: this
# guard reads only the daily-deploy workflows, so the helm/global overlays are
# outside it -- us-east's 3500m is fractional and unchecked here by design.
WHOLE_CORE_KEYS = ("limits.cpu",)

QUANTITY_SUFFIX = {
    "": 1, "m": 1e-3, "k": 1e3, "M": 1e6, "G": 1e9, "T": 1e12, "P": 1e15, "E": 1e18,
    "Ki": 2 ** 10, "Mi": 2 ** 20, "Gi": 2 ** 30,
    "Ti": 2 ** 40, "Pi": 2 ** 50, "Ei": 2 ** 60,
}

# The smallest real relay step (websocket) declares 9 names. Under this, the
# extractor lost the block rather than a step having shrunk.
MIN_ENV_NAMES = 8

# Ascend's allowed deviations from the hcl env-name set. Add a name only with
# the reason it cannot match; drop the entry when the deviation goes.
ASCEND_ALLOWED = {
    # Inert everywhere -- no actix-api source reads it. #2715 removed it from
    # the HCL clusters; ascend was out of that issue's scope.
    "webtransport": {"TOKIO_BLOCKING_THREADS"},
    "websocket": set(),
}

ENV_NAME_RE = re.compile(
    r'--set(?:-string)?\s+"env\[(\d+)\]\.name=([A-Za-z_][A-Za-z0-9_]*)"'
)
RESOURCE_RE = re.compile(
    r"--set\s+resources\.(requests|limits)\.(cpu|memory)=(\S+?)\s*\\?$", re.MULTILINE
)
ENV_VALUE_RE = re.compile(r'--set(?:-string)?\s+"env\[(\d+)\]\.value=(\S*?)"')
STEP_RE = re.compile(r"^      - name: (.+)$", re.MULTILINE)
SERVICE_PORT_RE = re.compile(r"--set\s+service\.port=(\d+)")
SERVICE_NODEPORT_RE = re.compile(r"--set\s+service\.nodePort=(\d+)")
UI_HOST_RE = re.compile(r'--set\s+"runtimeConfig\.webTransportHost=(\S+?)"')
UI_STEP = "Deploy Dioxus UI"
SET_ARG_RE = re.compile(r'^\s*--set(?:-string)?\s+"?([^"\n]+?)"?\s*\\?$', re.MULTILINE)
UNESCAPED_COMMA_RE = re.compile(r"(?<!\\),")


class ExtractorError(Exception):
    """The workflow did not parse the way this guard assumes it does."""


def workflow_path(cluster, overrides):
    if cluster in overrides:
        return overrides[cluster]
    return REPO_ROOT / ".github" / "workflows" / f"daily-deploy-{cluster}.yaml"


def step_body(text, step_name, where):
    """The run-block of one step: everything up to the next 6-space step."""
    starts = [m for m in STEP_RE.finditer(text) if m.group(1).strip() == step_name]
    if len(starts) != 1:
        raise ExtractorError(
            f"{where}: expected exactly one step named {step_name!r}, found {len(starts)}"
        )
    start = starts[0]
    nxt = STEP_RE.search(text, start.end())
    return text[start.end() : nxt.start() if nxt else len(text)]


def env_names(body, where, service):
    """Ordered (index, name) pairs, validated for contiguity."""
    pairs = [(int(i), n) for i, n in ENV_NAME_RE.findall(body)]
    if len(pairs) < MIN_ENV_NAMES:
        raise ExtractorError(
            f"{where} {service}: extracted only {len(pairs)} env names "
            f"(expected >= {MIN_ENV_NAMES}) -- refusing to pass vacuously"
        )
    return pairs


def contiguity_problems(pairs, where, service):
    got = [i for i, _ in pairs]
    want = list(range(len(got)))
    if got != want:
        return [
            f"{where} {service}: env indices are {got}, expected {want}. "
            f"A skipped index leaves a null in the helm list and the "
            f"Deployment is rejected."
        ]
    return []


def quantity(text):
    """Kubernetes quantity -> float, so 1Gi and 1024Mi compare equal."""
    m = re.fullmatch(r"(\d+(?:\.\d+)?)([A-Za-z]*)", text)
    if not m or m.group(2) not in QUANTITY_SUFFIX:
        return None
    return float(m.group(1)) * QUANTITY_SUFFIX[m.group(2)]


def resource_problems(body, where, service, cluster):
    found = {f"{kind}.{res}": val for kind, res, val in RESOURCE_RE.findall(body)}
    missing = [k for k in REQUIRED_RESOURCE_KEYS if k not in found]
    if missing:
        return [
            f"{where} {service}: no --set for resources.{', resources.'.join(missing)}. "
            f"The step would inherit the chart default instead."
        ]
    if service != "webtransport":
        return []
    problems = []
    for key, (floor, why) in WT_CPU_FLOORS.items():
        got, qgot, qfloor = found[key], quantity(found[key]), quantity(floor)
        if qgot is None:
            problems.append(
                f"{where} {service}: resources.{key}={got} is an unparseable quantity, "
                f"so the CPU floor cannot be checked."
            )
        elif qgot < qfloor:
            problems.append(
                f"{where} {service}: resources.{key}={got} is below the {floor} floor -- "
                f"{why}."
            )
        elif key in WHOLE_CORE_KEYS and abs(qgot - round(qgot)) > 1e-9:
            problems.append(
                f"{where} {service}: resources.{key}={got} is not a whole number of cores. "
                f"available_parallelism() rounds the cgroup quota DOWN, so the remainder "
                f"buys no parallelism (#2727)."
            )
    ceiling, capped = WT_MEMORY_REQUEST_CEILING
    got = found["requests.memory"]
    if cluster in capped:
        q = quantity(got)
        if q is None:
            problems.append(
                f"{where} {service}: resources.requests.memory={got} is an unparseable "
                f"quantity, so the memory ceiling cannot be checked."
            )
        elif q > quantity(ceiling):
            problems.append(
                f"{where} {service}: resources.requests.memory={got} is above the "
                f"{ceiling} ceiling for this shared single-node cluster (#2842)."
            )
    return problems


def comma_problems(body, where, service):
    """Helm splits a --set value on unescaped commas and keeps only the first.

    `--set "env[1].value=warn,quinn=warn"` renders `RUST_LOG: warn` and turns the
    rest into junk top-level values helm accepts without complaint. `--set-string`
    splits too; escaping as `\\,` is the fix. Verified by rendering both.
    """
    problems = []
    for arg in SET_ARG_RE.findall(body):
        key, sep, value = arg.partition("=")
        if not sep or not UNESCAPED_COMMA_RE.search(value):
            continue
        problems.append(
            f"{where} {service}: --set {key} has an unescaped comma in its value "
            f"({value!r}). Helm would keep only {value.split(',')[0]!r} and scatter "
            f"the rest as top-level values. Escape them as \\,."
        )
    return problems


def listen_url_port(body, pairs):
    """The port inside the step's LISTEN_URL value, or None."""
    idx = [i for i, n in pairs if n == "LISTEN_URL"]
    if not idx:
        return None
    value = dict(ENV_VALUE_RE.findall(body)).get(str(idx[0]))
    m = re.search(r":(\d+)$", value or "")
    return int(m.group(1)) if m else None


def port_coherence_problems(text, body, pairs, where):
    """service.port, LISTEN_URL and the UI's webTransportHost must agree.

    The UI dials the node port when one is pinned (ascend proxies UDP through
    HAProxy) and the Service port otherwise.
    """
    svc = SERVICE_PORT_RE.search(body)
    if not svc:
        return [f"{where} webtransport: no --set service.port=... in the step."]
    svc_port = int(svc.group(1))
    problems = []

    listen = listen_url_port(body, pairs)
    if listen is None:
        problems.append(
            f"{where} webtransport: could not read a port out of LISTEN_URL."
        )
    elif listen != svc_port:
        problems.append(
            f"{where} webtransport: service.port={svc_port} but LISTEN_URL listens on "
            f"{listen}. The Service would forward to a port the relay is not bound to."
        )

    node = SERVICE_NODEPORT_RE.search(body)
    dialled = int(node.group(1)) if node else svc_port
    host = UI_HOST_RE.search(text)
    if not host:
        problems.append(f"{where}: no --set runtimeConfig.webTransportHost=... found.")
        return problems
    m = re.search(r":(\d+)$", host.group(1))
    ui_port = int(m.group(1)) if m else 443
    if ui_port != dialled:
        problems.append(
            f"{where}: the UI dials {host.group(1)} (port {ui_port}) but the relay is "
            f"reachable on {dialled}. A WT step that fails mid-job leaves the Service, "
            f"the pod and the UI disagreeing; this is the check that catches it."
        )
    return problems


def service_type_problems(body, pairs, where, service):
    """SERVICE_TYPE must name the transport the step actually deploys."""
    idx = [i for i, n in pairs if n == "SERVICE_TYPE"]
    if not idx:
        return []  # the required-name check reports this
    values = dict(ENV_VALUE_RE.findall(body))
    got = values.get(str(idx[0]))
    if got is None:
        return [
            f"{where} {service}: SERVICE_TYPE is declared at env[{idx[0]}] but no "
            f"matching --set env[{idx[0]}].value=... was found."
        ]
    if got != service:
        return [
            f"{where} {service}: SERVICE_TYPE={got!r}, expected {service!r}. "
            f"The relay would publish its diagnostics under the wrong service_type."
        ]
    return []


def collect(cluster, overrides):
    path = workflow_path(cluster, overrides)
    if not path.is_file():
        raise ExtractorError(f"workflow file not found: {path}")
    text = path.read_text()
    where = path.name
    out = {}
    problems = []
    for service, step_name in SERVICES.items():
        body = step_body(text, step_name, where)
        pairs = env_names(body, where, service)
        out[service] = [n for _, n in pairs]
        problems += contiguity_problems(pairs, where, service)
        problems += resource_problems(body, where, service, cluster)
        problems += comma_problems(body, where, service)
        problems += service_type_problems(body, pairs, where, service)
        if service == "webtransport":
            problems += port_coherence_problems(text, body, pairs, where)
        missing = [n for n in REQUIRED_ENV_NAMES if n not in out[service]]
        if missing:
            problems.append(
                f"{where} {service}: missing required env {', '.join(missing)}. "
                f"`--set env[N]` replaces the whole list, so an omitted var is "
                f"absent at runtime, not inherited from the chart."
            )
        dupes = sorted({n for n in out[service] if out[service].count(n) > 1})
        if dupes:
            problems.append(
                f"{where} {service}: env name declared more than once: {', '.join(dupes)}."
            )
    return out, problems


def parity_problems(sets):
    problems = []
    for service in SERVICES:
        ref = set(sets[REFERENCE][service])
        for peer in STRICT_PEERS:
            peer_set = set(sets[peer][service])
            extra = sorted(peer_set - ref)
            missing = sorted(ref - peer_set)
            if extra or missing:
                problems.append(
                    f"{peer} {service} env names have drifted from {REFERENCE}: "
                    f"extra={extra or '[]'} missing={missing or '[]'}"
                )
        if "ascend" in sets:
            allowed = ASCEND_ALLOWED.get(service, set())
            a = set(sets["ascend"][service])
            extra = sorted(a - ref - allowed)
            missing = sorted(ref - a - allowed)
            if extra or missing:
                problems.append(
                    f"ascend {service} env names differ from {REFERENCE} outside the "
                    f"allowlist {sorted(allowed) or '[]'}: "
                    f"extra={extra or '[]'} missing={missing or '[]'}"
                )
            stale = sorted(allowed & ref)
            if stale:
                problems.append(
                    f"ASCEND_ALLOWED[{service}] still lists {stale}, but {REFERENCE} "
                    f"now sets them too -- drop the entry so the guard stays strict."
                )
    return problems


def main(argv):
    overrides = {}
    for raw in argv:
        p = Path(raw)
        m = re.fullmatch(r"daily-deploy-(.+)\.yaml", p.name)
        if not m or m.group(1) not in CLUSTERS:
            print(
                f"usage error: {raw} is not a known daily-deploy workflow "
                f"(expected daily-deploy-<{'|'.join(CLUSTERS)}>.yaml)",
                file=sys.stderr,
            )
            return 2
        overrides[m.group(1)] = p

    sets = {}
    problems = []
    try:
        for cluster in CLUSTERS:
            sets[cluster], found = collect(cluster, overrides)
            problems += found
    except ExtractorError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2

    problems += parity_problems(sets)

    if problems:
        print("ERROR: daily-deploy relay env drift (#2715):", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        return 1

    counts = ", ".join(
        f"{c} {s}={len(sets[c][s])}" for c in CLUSTERS for s in SERVICES
    )
    print(f"OK: relay env names, indices and resources agree ({counts}).")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
