#!/usr/bin/env python3
"""Fail when meeting-api migrations stop running before the new pod.

Every render of helm/meeting-api must carry a pre-install,pre-upgrade hook Job
that runs dbmate with the Deployment's DATABASE_URL and image, a deadline under
the step's --timeout, cpu/memory requests and limits, a PGOPTIONS lock_timeout,
and no instance label on its pod. Dockerfile.meeting-api's CMD must not run
startup.sh; every compose service running meeting-api must run it itself.
Exit 0: pass. 1: a check failed. 2: helm missing or a deploy step not extractable.
"""
from __future__ import annotations

import re
import shutil
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_api_readiness_probe as probe  # noqa: E402
import check_wt_service_affinity as wt  # noqa: E402
from check_deploy_env_parity import ExtractorError, step_body  # noqa: E402

DEFAULT_REPO_ROOT = Path(__file__).resolve().parent.parent
CHART = "helm/meeting-api"
STEP = "Deploy Meeting API"
HELM_DEFAULT_TIMEOUT = 300
COMPOSE_FILES = (
    "docker/docker-compose.yaml",
    "docker/docker-compose.e2e.yaml",
)

HOOK_RE = re.compile(r"^ +['\"]?helm\.sh/hook['\"]?: *['\"]?([^'\"\n]+)", re.MULTILINE)
COMMAND_RE = re.compile(r"^ +command: (.+)$", re.MULTILINE)
IMAGE_RE = re.compile(r"^ +image: ['\"]?([^'\"\n]+)", re.MULTILINE)
INSTANCE_RE = re.compile(r"^ +app\.kubernetes\.io/instance:", re.MULTILINE)
LOCK_TIMEOUT_RE = re.compile(r"^ +- name: PGOPTIONS\n +value: ['\"]?[^\n]*lock_timeout=[1-9]", re.MULTILINE)
DEADLINE_RE = re.compile(r"^  activeDeadlineSeconds: (\d+)$", re.MULTILINE)
DB_URL_RE = re.compile(r"^ +- name: DATABASE_URL\n +value: (.+)$", re.MULTILINE)
TIMEOUT_RE = re.compile(r"--timeout[ =](\d+)([ms])")
CMD_RE = re.compile(r"^(?:CMD|ENTRYPOINT)\b.*$", re.MULTILINE)
SERVICE_RE = re.compile(r"^  ([A-Za-z0-9_.-]+):\s*$", re.MULTILINE)
COMPOSE_COMMAND_RE = re.compile(r"^    command:(.*(?:\n(?: {6,}.*|[ \t]*$))*)", re.MULTILINE)


def kinds(stdout, kind):
    docs = re.split(r"^---\s*$", stdout, flags=re.MULTILINE)
    return [d for d in docs if re.search(rf"^kind: {kind}$", d, re.MULTILINE)]


def step_timeout(body):
    m = TIMEOUT_RE.search(body)
    if not m:
        return HELM_DEFAULT_TIMEOUT
    return int(m.group(1)) * (60 if m.group(2) == "m" else 1)


def job_problems(where, rendered, timeout):
    if rendered.returncode != 0:
        return [f"{where}: helm template failed: {rendered.stderr.strip()}"]
    jobs, deployments = kinds(rendered.stdout, "Job"), kinds(rendered.stdout, "Deployment")
    if len(jobs) != 1 or len(deployments) != 1:
        return [f"{where}: expected one migrate Job and one Deployment, found {len(jobs)} and {len(deployments)}."]
    job, deployment = jobs[0], deployments[0]
    problems = []
    hooks = {h.strip() for m in HOOK_RE.findall(job) for h in m.split(",")}
    if not {"pre-install", "pre-upgrade"} <= hooks:
        problems.append(f"{where}: migrate Job hook is {sorted(hooks)}, expected pre-install,pre-upgrade.")
    commands = COMMAND_RE.findall(job)
    if not any("dbmate" in c or "startup.sh" in c for c in commands):
        problems.append(f"{where}: migrate Job command {commands} does not run dbmate.")
    job_url, dep_url = DB_URL_RE.findall(job), DB_URL_RE.findall(deployment)
    if not job_url or job_url != dep_url:
        problems.append(f"{where}: migrate Job DATABASE_URL {job_url} differs from the Deployment's {dep_url}.")
    deadline = DEADLINE_RE.search(job)
    if not deadline or int(deadline.group(1)) >= timeout:
        problems.append(f"{where}: migrate Job activeDeadlineSeconds must be under helm --timeout {timeout}s.")
    job_image, dep_image = IMAGE_RE.findall(job), IMAGE_RE.findall(deployment)
    if len(job_image) != 1 or job_image != dep_image:
        problems.append(f"{where}: migrate Job image {job_image} differs from the Deployment's {dep_image}.")
    resources = probe.block(job, "resources") or ""
    for kind in ("requests", "limits"):
        section = probe.block(resources, kind) or ""
        missing = [r for r in ("cpu", "memory") if not probe.field(section, r)]
        if missing:
            problems.append(f"{where}: migrate Job has no resources.{kind} for {missing}; a quota rejects it.")
    if not LOCK_TIMEOUT_RE.search(job):
        problems.append(f"{where}: migrate Job has no PGOPTIONS lock_timeout; DDL can stall the serving pod.")
    pod_labels = probe.block(probe.block(job, "template") or "", "labels") or ""
    if INSTANCE_RE.search(pod_labels):
        problems.append(f"{where}: migrate pod carries app.kubernetes.io/instance; instance selectors would pick it.")
    return problems


def dockerfile_problems(repo_root):
    path = repo_root / "Dockerfile.meeting-api"
    cmds = CMD_RE.findall(path.read_text())
    if not cmds:
        return [f"{path.name}: no CMD or ENTRYPOINT found."]
    if any("startup.sh" in c or "dbmate" in c for c in cmds):
        return [f"{path.name}: {cmds[-1]!r} still runs migrations in the app container."]
    return []


def compose_services(text):
    top = re.search(r"^services:\s*$", text, re.MULTILINE)
    if not top:
        return []
    end = re.search(r"^\S", text[top.end():], re.MULTILINE)
    body = text[top.end(): top.end() + end.start() if end else len(text)]
    marks = list(SERVICE_RE.finditer(body))
    return [
        (m.group(1), body[m.end(): marks[i + 1].start() if i + 1 < len(marks) else len(body)])
        for i, m in enumerate(marks)
    ]


def compose_problems(repo_root):
    problems, found = [], 0
    for rel in COMPOSE_FILES:
        path = repo_root / rel
        if not path.is_file():
            raise ExtractorError(f"compose file not found: {path}")
        for name, svc in compose_services(path.read_text()):
            runs_api = (
                name == "meeting-api"
                or "Dockerfile.meeting-api" in svc
                or "videocall-meeting-api" in svc
            )
            if not runs_api:
                continue
            found += 1
            command = COMPOSE_COMMAND_RE.search(svc)
            if not command or "startup.sh" not in command.group(1):
                problems.append(f"{rel} service {name}: command does not run /app/dbmate/startup.sh.")
    if found == 0:
        raise ExtractorError("no compose service runs meeting-api; refusing to pass vacuously.")
    return problems, found


def main(argv):
    repo_root = DEFAULT_REPO_ROOT
    if argv:
        if len(argv) != 2 or argv[0] != "--repo-root":
            print("usage error: check_api_migration_hook.py [--repo-root DIR]", file=sys.stderr)
            return 2
        repo_root = Path(argv[1]).resolve()
    if shutil.which("helm") is None:
        print("ERROR: helm is not on PATH; nothing was rendered.", file=sys.stderr)
        return 2

    wt.SHELL_VAR_STUB.update(probe.SHELL_VAR_STUB)
    problems = []
    try:
        targets = [("bare chart meeting-api", [], HELM_DEFAULT_TIMEOUT)]
        for name in probe.WORKFLOWS:
            path = repo_root / ".github" / "workflows" / name
            if not path.is_file():
                raise ExtractorError(f"workflow file not found: {path}")
            body = step_body(path.read_text(), STEP, name)
            step_chart, args = wt.step_helm_args(body, name)
            if step_chart != CHART:
                raise ExtractorError(f"{name}: {STEP!r} installs {step_chart}")
            targets.append((name, args, step_timeout(body)))
        for where, args, timeout in targets:
            problems += job_problems(where, wt.render(repo_root, where, CHART, args), timeout)
        problems += dockerfile_problems(repo_root)
        compose, services = compose_problems(repo_root)
        problems += compose
    except ExtractorError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2

    if problems:
        print("ERROR: meeting-api migration drift (#2831):", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    print(
        f"OK: {len(targets)} renders carry the migrate hook Job; Dockerfile CMD is migration-free; "
        f"{services} compose services run startup.sh."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
