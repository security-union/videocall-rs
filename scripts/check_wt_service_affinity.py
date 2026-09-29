#!/usr/bin/env python3
"""Fail the build when the WT relay Service loses its client-affinity settings,
or when the one-replica constraint of #1202 stops being enforced (#2727).

Renders the bare chart, every helm/global/*/webtransport wrapper with the
subchart vendored fresh, and every daily-deploy relay step with that step's own
--set block; asserts the three Service fields on each, and that every values
guard still rejects what it was written to reject. Why each value is what it is,
and why externalTrafficPolicy is not uniform across clusters, is in
docs/hcl-daily-operations.md section 4.

Usage:
  python3 scripts/check_wt_service_affinity.py               # the tracked tree
  python3 scripts/check_wt_service_affinity.py --repo-root D # an alternate tree

Exit codes:
  0  every check passes
  1  a Service lost a field, a cluster drifted, or a guard stopped firing
  2  helm is missing, or the extractor found no deploy step to render
"""
from __future__ import annotations

import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from check_deploy_env_parity import ExtractorError, step_body  # noqa: E402

DEFAULT_REPO_ROOT = Path(__file__).resolve().parent.parent

CHART = "helm/rustlemania-webtransport"
WRAPPER_GLOB = "helm/global/*/webtransport"
CLUSTERS = ("hcl", "labsworkspace", "ascend")
RELAY_STEPS = {
    "webtransport": "Deploy WebTransport Server",
    "websocket": "Deploy WebSocket Server",
}

EXPECT_AFFINITY = "ClientIP"
EXPECT_TIMEOUT = 86400
EXPECT_POLICY = "Cluster"

POLICY_RATIONALE = (
    "Every WT target pins Cluster while the #1202 replica guard holds. Local's "
    "benefits -- source-address preservation for the affinity match, and a "
    "node-aware cloud health check -- all need a second replica, while at one "
    "replica Local narrows serving to the node holding the pod and costs a "
    "partial-loss window on each reschedule. Flip it together with the guard, "
    "not on its own."
)

PROXIED_CLUSTERS = {
    "hcl": "k3s ServiceLB: klipper-lb MASQUERADEs every packet in POSTROUTING",
    "labsworkspace": "k3s ServiceLB: klipper-lb MASQUERADEs every packet in POSTROUTING",
    "ascend": "an off-cluster NGINX QUIC proxy reaches the relay at a pinned node port",
}

SHELL_VAR_STUB = {
    "TAG": "affinity-check",
    "REGISTRY": "registry.invalid/videocall",
    "NAMESPACE": "videocall",
    "REGISTRY_VAR": "registry.invalid/videocall",
}

HELM_CHART_RE = re.compile(r"^\s*helm upgrade --install\s+\S+\s+(\S+?)/?\s*\\?$", re.MULTILINE)
SET_ARG_RE = re.compile(r'^\s*--set(-string)?\s+"?([^"\n]+?)"?\s*\\?$', re.MULTILINE)
SHELL_VAR_RE = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-[^}]*)?\}")

SERVICE_DOC_RE = re.compile(r"#\s*Source:\s*\S*templates/loadbalancer\.yaml")
POLICY_RE = re.compile(r"^  externalTrafficPolicy:\s*(\S+)\s*$", re.MULTILINE)
AFFINITY_RE = re.compile(r"^  sessionAffinity:\s*(\S+)\s*$", re.MULTILINE)
TIMEOUT_RE = re.compile(r"^      timeoutSeconds:\s*(\d+)\s*$", re.MULTILINE)


class Rendered:
    """One `helm template` invocation and what it produced."""

    def __init__(self, where, returncode, stdout, stderr):
        self.where = where
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def helm(repo_root, args):
    return subprocess.run(
        ["helm", *args],
        cwd=repo_root,
        capture_output=True,
        text=True,
        check=False,
    )


def render(repo_root, where, chart, extra):
    proc = helm(repo_root, ["template", "affinity-check", str(chart), *extra])
    return Rendered(where, proc.returncode, proc.stdout, proc.stderr)


def workflow_text(repo_root, cluster):
    path = repo_root / ".github" / "workflows" / f"daily-deploy-{cluster}.yaml"
    if not path.is_file():
        raise ExtractorError(f"workflow file not found: {path}")
    return path.read_text(), path.name


def substitute(value, where):
    """Replace the shell variables a deploy step expands before helm sees them."""
    missing = []

    def repl(match):
        name = match.group(1)
        if name not in SHELL_VAR_STUB:
            missing.append(name)
            return match.group(0)
        return SHELL_VAR_STUB[name]

    out = SHELL_VAR_RE.sub(repl, value)
    if missing:
        raise ExtractorError(
            f"{where}: --set value {value!r} expands ${{{missing[0]}}}, which this "
            f"guard has no stub for. Add it to SHELL_VAR_STUB rather than letting "
            f"the render carry a literal dollar sign."
        )
    if "$" in out:
        raise ExtractorError(
            f"{where}: --set value {out!r} still carries a dollar sign after "
            f"substitution, so the render would not match what the step deploys."
        )
    return out


def step_helm_args(body, where):
    """The chart path and the --set arguments of one deploy step."""
    chart = HELM_CHART_RE.search(body)
    if not chart:
        raise ExtractorError(
            f"{where}: no `helm upgrade --install <release> <chart>` line found; "
            f"refusing to render a step this guard cannot read."
        )
    args = []
    for string_flag, arg in SET_ARG_RE.findall(body):
        args += ["--set-string" if string_flag else "--set", substitute(arg, where)]
    if not args:
        raise ExtractorError(f"{where}: extracted no --set arguments.")
    return chart.group(1), args


def service_problems(rendered, expect_policy):
    """The three Service fields, read out of the rendered loadbalancer.yaml doc."""
    where = rendered.where
    if rendered.returncode != 0:
        return [f"{where}: helm template failed: {rendered.stderr.strip()}"]

    docs = [d for d in re.split(r"^---\s*$", rendered.stdout, flags=re.MULTILINE)
            if SERVICE_DOC_RE.search(d)]
    if len(docs) != 1:
        return [
            f"{where}: expected exactly one rendered loadbalancer.yaml document, "
            f"found {len(docs)}. Without it every field check below passes vacuously."
        ]
    doc = docs[0]
    problems = []

    policy = POLICY_RE.search(doc)
    if not policy:
        problems.append(
            f"{where}: the Service has no externalTrafficPolicy. The API default "
            f"matches the value expected here, so nothing breaks today -- but the "
            f"template has lost the plumbing the #1202 flip depends on, and the "
            f"deployed policy is no longer visible in the manifest."
        )
    elif policy.group(1) != expect_policy:
        problems.append(
            f"{where}: externalTrafficPolicy={policy.group(1)}, expected "
            f"{expect_policy}. {POLICY_RATIONALE}"
        )

    affinity = AFFINITY_RE.search(doc)
    if not affinity or affinity.group(1) != EXPECT_AFFINITY:
        got = affinity.group(1) if affinity else "absent"
        problems.append(
            f"{where}: sessionAffinity={got}, expected {EXPECT_AFFINITY}. Without "
            f"it a NAT rebind is a fresh conntrack entry and kube-proxy repicks a "
            f"backend that holds no state for the QUIC connection."
        )

    timeout = TIMEOUT_RE.search(doc)
    if not timeout:
        problems.append(
            f"{where}: sessionAffinityConfig.clientIP.timeoutSeconds is absent."
        )
    elif int(timeout.group(1)) != EXPECT_TIMEOUT:
        problems.append(
            f"{where}: sessionAffinityConfig.clientIP.timeoutSeconds="
            f"{timeout.group(1)}, expected {EXPECT_TIMEOUT}. kube-proxy stamps the "
            f"entry once per new connection and does not refresh it while one is "
            f"open, so the value spans a meeting, not an idle gap."
        )
    return problems


def vendored_wrapper(repo_root, wrapper, tmp):
    """A copy of one helm/global wrapper with the subchart packaged fresh.

    `helm dependency build` reaches for every configured repository; packaging
    the local subchart produces the same tarball without the network call.
    """
    dest = tmp / f"{wrapper.parent.name}-{wrapper.name}"
    shutil.copytree(wrapper, dest)
    shutil.rmtree(dest / "charts", ignore_errors=True)
    (dest / "charts").mkdir()
    proc = helm(repo_root, ["package", CHART, "-d", str(dest / "charts")])
    if proc.returncode != 0:
        raise ExtractorError(
            f"helm package {CHART} failed for {wrapper}: {proc.stderr.strip()}"
        )
    return dest


CASES = (
    ("replicaCount=2", "#1202", "replicaCount=2"),
    (
        "autoscaling.enabled=true,autoscaling.maxReplicas=2",
        "#1202",
        "an autoscaling ceiling above 1",
    ),
    (
        "service.sessionAffinityTimeoutSeconds=86401",
        "86400",
        "a client-affinity timeout above the API maximum",
    ),
    (
        "service.externalTrafficPolicy=local",
        "Local or Cluster",
        "a mis-cased externalTrafficPolicy",
    ),
)


def guard_problems(repo_root, chart, where, prefix=""):
    """Each values guard must still reject what it was written to reject.

    Wrappers are checked under the subchart key: the guard reads the subchart's
    own .Values scope, which a bare-chart render does not exercise.
    """
    problems = []
    for overrides, needle, label in CASES:
        args = []
        for override in overrides.split(","):
            args += ["--set", f"{prefix}{override}"]
        out = render(repo_root, f"{where} guard[{label}]", chart, args)
        if out.returncode == 0:
            problems.append(
                f"{where} rendered with {label}; the values guard that must "
                f"reject it is gone or unreachable."
            )
        elif needle not in out.stderr:
            problems.append(
                f"{where} rejected {label} but the message does not mention "
                f"{needle!r}, so an operator cannot tell why: {out.stderr.strip()}"
            )
    return problems


def main(argv):
    repo_root = DEFAULT_REPO_ROOT
    if argv:
        if len(argv) != 2 or argv[0] != "--repo-root":
            print(
                "usage error: check_wt_service_affinity.py [--repo-root DIR]",
                file=sys.stderr,
            )
            return 2
        repo_root = Path(argv[1]).resolve()
        if not (repo_root / CHART).is_dir():
            print(f"usage error: {repo_root} has no {CHART}", file=sys.stderr)
            return 2

    if shutil.which("helm") is None:
        print(
            "ERROR: helm is not on PATH, so nothing was rendered. Refusing to "
            "report success.",
            file=sys.stderr,
        )
        return 2

    problems = []
    rendered = 0
    try:
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            problems += service_problems(
                render(repo_root, "bare chart", CHART, []), EXPECT_POLICY
            )
            rendered += 1

            wrappers = sorted(repo_root.glob(WRAPPER_GLOB))
            if not wrappers:
                raise ExtractorError(
                    f"no wrapper charts matched {WRAPPER_GLOB}; refusing to pass "
                    f"vacuously."
                )
            for wrapper in wrappers:
                copy = vendored_wrapper(repo_root, wrapper, tmp)
                where = str(wrapper.relative_to(repo_root))
                problems += service_problems(
                    render(repo_root, where, copy, []), EXPECT_POLICY
                )
                rendered += 1
                problems += guard_problems(
                    repo_root, copy, where, prefix=f"{Path(CHART).name}."
                )

            for cluster in CLUSTERS:
                text, name = workflow_text(repo_root, cluster)
                for service, step_name in RELAY_STEPS.items():
                    where = f"{name} {service}"
                    body = step_body(text, step_name, name)
                    chart, args = step_helm_args(body, where)
                    out = render(repo_root, where, chart, args)
                    rendered += 1
                    if service != "webtransport":
                        if out.returncode != 0:
                            problems.append(
                                f"{where}: helm template failed with the step's own "
                                f"--set block: {out.stderr.strip()}"
                            )
                        continue
                    found = service_problems(out, EXPECT_POLICY)
                    if found and cluster in PROXIED_CLUSTERS:
                        found = [
                            f"{p} This cluster keeps Cluster even after the guard "
                            f"lifts, because {PROXIED_CLUSTERS[cluster]}."
                            for p in found
                        ]
                    problems += found

            problems += guard_problems(repo_root, CHART, "the bare chart")
    except ExtractorError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2

    if problems:
        print("ERROR: WT relay Service affinity drift (#2727):", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1

    print(
        f"OK: {rendered} renders carry sessionAffinity={EXPECT_AFFINITY} at "
        f"{EXPECT_TIMEOUT}s with the expected traffic policy, and the #1202 "
        f"replica guard still fires."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
