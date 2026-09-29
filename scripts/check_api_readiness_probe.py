#!/usr/bin/env python3
"""Fail when the meeting-api or videocall-ui Deployment loses its readiness
probe, or probes a port the Service does not send traffic to (#2831).

Renders both bare charts and every HCL deploy step that installs them, with
that step's own --set block.

Usage:
  python3 scripts/check_api_readiness_probe.py               # the tracked tree
  python3 scripts/check_api_readiness_probe.py --repo-root D # an alternate tree

Exit codes:
  0  every render passes
  1  a render lost the probe, points it at the wrong port, or failed
  2  helm is missing, or a deploy step could not be extracted
"""
from __future__ import annotations

import re
import shutil
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_wt_service_affinity as wt  # noqa: E402
from check_deploy_env_parity import ExtractorError, step_body  # noqa: E402

DEFAULT_REPO_ROOT = Path(__file__).resolve().parent.parent

CHARTS = {
    "meeting-api": {"path": "/version", "step": "Deploy Meeting API"},
    "videocall-ui": {"path": "/version.json", "step": "Deploy Dioxus UI"},
}
WORKFLOWS = (
    "daily-deploy-hcl.yaml",
    "daily-deploy-labsworkspace.yaml",
    "daily-deploy-ascend.yaml",
    "pr-deploy-reusable-hcl.yaml",
)
SHELL_VAR_STUB = {
    "PG_PASSWORD": "stub",
    "OAUTH_CLIENT_ID": "stub",
    "OAUTH_CLIENT_SECRET": "stub",
    "PR_NUM": "1",
    "SLOT": "1",
}

SOURCE_RE = re.compile(r"^# Source: \S+/templates/(deployment|service)\.yaml$", re.MULTILINE)
CONTAINER_RE = re.compile(r"^        - ", re.MULTILINE)
CONTAINER_PORT_RE = re.compile(r"^ +- name: (\S+)\n +containerPort: (\d+)$", re.MULTILINE)
LISTEN_ADDR_RE = re.compile(r"^ +- name: LISTEN_ADDR\n +value: ['\"]?[^'\"\n]*:(\d+)['\"]?$", re.MULTILINE)
TARGET_PORT_RE = re.compile(r"^ +targetPort: (\S+)$", re.MULTILINE)


def block(text, key):
    """The lines nested under the first `key:` line, or None."""
    m = re.search(rf"^( *){re.escape(key)}:\n", text, re.MULTILINE)
    if not m:
        return None
    indent = len(m.group(1))
    body = []
    for line in text[m.end():].splitlines():
        if line.strip() and len(line) - len(line.lstrip()) <= indent:
            break
        body.append(line)
    return "\n".join(body)


def field(text, key):
    m = re.search(rf"^ +{re.escape(key)}: ['\"]?([^'\"\n]+?)['\"]?$", text or "", re.MULTILINE)
    return m.group(1) if m else None


def resolve(port, deployment):
    if port is None or port.isdigit():
        return int(port) if port else None
    for name, number in CONTAINER_PORT_RE.findall(deployment):
        if name == port:
            return int(number)
    return None


def split_docs(stdout):
    docs = {}
    marks = list(SOURCE_RE.finditer(stdout))
    for i, m in enumerate(marks):
        end = marks[i + 1].start() if i + 1 < len(marks) else len(stdout)
        docs.setdefault(m.group(1), []).append(stdout[m.end():end])
    return docs


def problems_for(chart, where, rendered):
    if rendered.returncode != 0:
        return [f"{where}: helm template failed: {rendered.stderr.strip()}"]
    docs = split_docs(rendered.stdout)
    if len(docs.get("deployment", [])) != 1 or len(docs.get("service", [])) != 1:
        return [f"{where}: expected one Deployment and one Service document, found {docs.keys()}."]
    deployment, service = docs["deployment"][0], docs["service"][0]
    if len(CONTAINER_RE.findall(block(deployment, "containers") or "")) != 1:
        return [f"{where}: expected exactly one container."]
    spec = CHARTS[chart]
    problems = []

    probe = block(block(deployment, "readinessProbe") or "", "httpGet")
    if not probe:
        return [f"{where}: no readinessProbe.httpGet; helm --wait cannot see a failed start."]
    path = field(probe, "path")
    if path != spec["path"]:
        problems.append(f"{where}: readiness path {path!r}, expected {spec['path']!r}.")

    raw_port = field(probe, "port")
    probe_port = resolve(raw_port, deployment)
    target_match = TARGET_PORT_RE.search(service)
    target = resolve(target_match.group(1) if target_match else None, deployment)
    if probe_port is None or probe_port != target:
        problems.append(
            f"{where}: readiness probe port {raw_port!r} -> {probe_port}, "
            f"but the Service targets {target}."
        )
    if chart == "meeting-api":
        m = LISTEN_ADDR_RE.search(deployment)
        listen = int(m.group(1)) if m else 8081
        if probe_port != listen:
            problems.append(f"{where}: readiness probe port {probe_port}, LISTEN_ADDR port {listen}.")
    return problems


def show_only():
    return ["--show-only", "templates/deployment.yaml", "--show-only", "templates/service.yaml"]


def main(argv):
    repo_root = DEFAULT_REPO_ROOT
    if argv:
        if len(argv) != 2 or argv[0] != "--repo-root":
            print("usage error: check_api_readiness_probe.py [--repo-root DIR]", file=sys.stderr)
            return 2
        repo_root = Path(argv[1]).resolve()
    if shutil.which("helm") is None:
        print("ERROR: helm is not on PATH; nothing was rendered.", file=sys.stderr)
        return 2

    wt.SHELL_VAR_STUB.update(SHELL_VAR_STUB)
    problems = []
    rendered = 0
    try:
        if not WORKFLOWS:
            raise ExtractorError("WORKFLOWS is empty; refusing to pass vacuously.")
        for chart, spec in CHARTS.items():
            targets = [("bare chart " + chart, f"helm/{chart}", show_only())]
            for name in WORKFLOWS:
                path = repo_root / ".github" / "workflows" / name
                if not path.is_file():
                    raise ExtractorError(f"workflow file not found: {path}")
                body = step_body(path.read_text(), spec["step"], name)
                step_chart, args = wt.step_helm_args(body, f"{name} {chart}")
                if step_chart != f"helm/{chart}":
                    raise ExtractorError(f"{name}: {spec['step']!r} installs {step_chart}")
                targets.append((f"{name} {chart}", step_chart, args + show_only()))
            for where, path, args in targets:
                problems += problems_for(chart, where, wt.render(repo_root, where, path, args))
                rendered += 1
    except ExtractorError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2

    if problems:
        print("ERROR: readiness probe drift (#2831):", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    print(f"OK: {rendered} renders carry a readinessProbe on the Service's target port.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
