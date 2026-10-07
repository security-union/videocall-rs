#!/usr/bin/env python3
"""Fail the build when a cluster's Prometheus rule groups do not PARSE (#2734).

Usage: check_prometheus_rules_parse.py [values.yaml ...]   (default: tracked)
Env:   REQUIRE_PROMTOOL=1      no container runtime is a failure, not a skip
       PROMTOOL_SKIP_RUNTIME=1 static layer only
       PROMTOOL_IMAGES=a,b     override the pinned images
Exit:  0 parses  1 does not parse, or promtool required and unavailable  2 usage
"""
from __future__ import annotations

import importlib.util
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent

_PARITY = SCRIPT_DIR / "check_prometheus_alert_parity.py"
_spec = importlib.util.spec_from_file_location("alert_parity", _PARITY)
parity = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(parity)

# Every file against every image: the guarded groups must be identical, so an
# expression parsing on one server and not another is drift. v2.47.0 = us-east
# Chart.lock 25.0.0's appVersion; v3.11.2 = the live hcl-daily server (#2732).
# ~870 MB on the runner's disk between them, pulled once and cached.
PINNED_IMAGES = (
    "prom/prometheus:v2.47.0",
    "prom/prometheus:v3.11.2",
)

# Classified by probing each candidate through 3.11.2; last_over_time is core.
EXPERIMENTAL_FUNCTIONS = (
    "double_exponential_smoothing",
    "first_over_time",
    "info",
    "limit_ratio",
    "limitk",
    "mad_over_time",
    "sort_by_label",
    "sort_by_label_desc",
    "ts_of_first_over_time",
    "ts_of_last_over_time",
    "ts_of_max_over_time",
    "ts_of_min_over_time",
)

DEFAULT_FILES = tuple(REPO_ROOT / c.values for c in parity.CLUSTERS)
RULE_TESTS_DIR = REPO_ROOT / "helm" / "global" / "prometheus-rule-tests"


def rule_subtrees(text):
    """{serverFiles key: dedented standalone rule file}."""
    out = {}
    for key, body in parity.server_files_children(text).items():
        if key == "prometheus.yml":
            continue
        if not any(ln.strip() == "groups:" for ln in body):
            continue
        kept = [ln for ln in body if ln.strip()]
        if not kept:
            continue
        shift = min(len(ln) - len(ln.lstrip()) for ln in kept)
        out[key] = "\n".join(ln[shift:].rstrip() for ln in body) + "\n"
    return out


def _denied_call_pattern():
    """Longest first, left word boundary: else ts_of_first_over_time reports as first_over_time."""
    names = sorted(EXPERIMENTAL_FUNCTIONS, key=len, reverse=True)
    return re.compile(
        r"(?<![A-Za-z0-9_])(%s)\s*\(" % "|".join(re.escape(n) for n in names)
    )


def static_problems(label, rule_text):
    problems = []
    pattern = _denied_call_pattern()
    alert = "<unknown>"
    for lineno, line in enumerate(rule_text.splitlines(), start=1):
        stripped = line.strip()
        if stripped.startswith("- alert:"):
            alert = stripped.split(":", 1)[1].strip()
        if "expr:" not in stripped:
            continue
        for fn in dict.fromkeys(pattern.findall(stripped)):
            problems.append(
                "%s:%d alert %r uses %s(), which Prometheus 3.x gates behind "
                "--enable-feature=promql-experimental-functions and 2.x does "
                "not have. ONE such rule aborts the whole rule-file load, so "
                "every alert in the file stops evaluating and a fresh server "
                "refuses to start." % (label, lineno, alert, fn)
            )
    return problems


def runtime() -> str | None:
    if os.environ.get("PROMTOOL_SKIP_RUNTIME") == "1":
        return None
    for exe in ("docker", "podman"):
        path = shutil.which(exe)
        if not path:
            continue
        probe = subprocess.run(
            [path, "version", "--format", "{{.Server.Version}}"],
            capture_output=True,
            text=True,
        )
        if probe.returncode == 0:
            return path
    return None


def expected_rule_count(rule_text):
    return sum(
        1
        for ln in rule_text.splitlines()
        if ln.strip().startswith("- alert:") or ln.strip().startswith("- record:")
    )


SUCCESS_LINE = re.compile(r"^\s*SUCCESS:\s*(\d+)\s+rules found\s*$", re.M)


def promtool_verdict(image, name, expected, returncode, output):
    """promtool in v2.47.0 exits 0 and prints NOTHING for a file it cannot
    parse, so a pass needs one `SUCCESS: <expected> rules found`, not exit 0."""
    if returncode != 0:
        if "permission denied" in output:
            return (
                "%s %s: ENVIRONMENT FAILURE, not a rule defect. promtool could "
                "not stat the mounted file, so it never read the PromQL. The "
                "bind-mounted dir or the file is unreadable to the container "
                "user -- check the 0755/0644 chmod in stage_rules() and the "
                "--user flag in promtool_argv().\n%s" % (image, name, output.strip())
            )
        return "%s %s: promtool exit %d\n%s" % (image, name, returncode, output.strip())
    found = SUCCESS_LINE.findall(output)
    if len(found) != 1:
        return (
            "%s %s: promtool exited 0 but printed %d 'SUCCESS: N rules found' "
            "line(s), expected exactly 1. This image reports an unparseable "
            "file by staying SILENT, so no output means the rules did NOT "
            "load.\n%s" % (image, name, len(found), output.strip() or "(no output)")
        )
    if int(found[0]) != expected:
        return (
            "%s %s: promtool loaded %s rules, the file declares %d. Rules were "
            "dropped or the extractor is reading the wrong subtree.\n%s"
            % (image, name, found[0], expected, output.strip())
        )
    return None


def stage_rules(workdir, files_by_label):
    """Stage into the bind mount readable by the container's uid 65534:
    mkdtemp gives 0700 and a strict umask 0600, which Linux enforces (#2711)."""
    entries = []
    problems = []
    os.chmod(workdir, 0o755)
    for label, rule_text in sorted(files_by_label.items()):
        name = label.replace("/", "__")
        dest = workdir / name
        dest.write_text(rule_text)
        os.chmod(dest, 0o644)
        expected = expected_rule_count(rule_text)
        if expected == 0:
            problems.append(
                "%s: declares no alerting or recording rules. A rule file "
                "with nothing in it would pass promtool vacuously." % label
            )
            continue
        entries.append((name, expected))
    return entries, problems


def promtool_argv(exe, workdir, image, name, command=("check", "rules")):
    """Run as the invoking user; promtool reads one file and needs no home."""
    argv = [exe, "run", "--rm", "--entrypoint", "promtool"]
    if hasattr(os, "getuid"):
        argv += ["--user", "%d:%d" % (os.getuid(), os.getgid())]
    argv += ["-v", "%s:/rules" % workdir, image, *command, "/rules/%s" % name]
    return argv


def rule_test_files():
    return sorted(RULE_TESTS_DIR.glob("*.test.yaml"))


def stage_rule_tests(workdir, files_by_label, tests):
    by_cluster = {}
    for label, rule_text in files_by_label.items():
        cluster, key = label.split("::", 1)
        by_cluster.setdefault(cluster, {})[key] = rule_text
    runs = []
    for cluster, rules in sorted(by_cluster.items()):
        d = workdir / ("tests__" + cluster)
        d.mkdir()
        os.chmod(d, 0o755)
        for key, rule_text in rules.items():
            (d / key).write_text(rule_text)
            os.chmod(d / key, 0o644)
        header = "rule_files:\n" + "".join("  - %s\n" % k for k in sorted(rules))
        for test in tests:
            (d / test.name).write_text(header + test.read_text())
            os.chmod(d / test.name, 0o644)
            runs.append("%s/%s" % (d.name, test.name))
    return runs


def rule_test_verdict(image, name, returncode, output):
    if returncode == 0 and "SUCCESS" in output:
        return None
    return "%s %s: promtool test rules FAILED (exit %d)\n%s" % (
        image, name, returncode, output.strip() or "(no output)")


def promtool_problems(exe, images, files_by_label):
    """Layer 2: promtool per file per image, so verdicts attribute."""
    workdir = Path(tempfile.mkdtemp(prefix=".promtool-tmp-", dir=str(REPO_ROOT)))
    try:
        entries, problems = stage_rules(workdir, files_by_label)
        for image in images:
            print("  %s: promtool check rules on %d file(s)" % (image, len(entries)))
            for name, expected in entries:
                res = subprocess.run(
                    promtool_argv(exe, workdir, image, name),
                    capture_output=True,
                    text=True,
                )
                bad = promtool_verdict(
                    image, name, expected, res.returncode, res.stdout + res.stderr
                )
                if bad:
                    problems.append(bad)
        runs = stage_rule_tests(workdir, files_by_label, rule_test_files())
        for image in images:
            print("  %s: promtool test rules on %d staged test(s)" % (image, len(runs)))
            for name in runs:
                res = subprocess.run(
                    promtool_argv(exe, workdir, image, name, ("test", "rules")),
                    capture_output=True,
                    text=True,
                )
                bad = rule_test_verdict(image, name, res.returncode, res.stdout + res.stderr)
                if bad:
                    problems.append(bad)
    finally:
        shutil.rmtree(workdir, ignore_errors=True)
    return problems


def main(argv):
    paths = [Path(p) for p in argv[1:]] or list(DEFAULT_FILES)
    if not paths:
        print("ERROR: no files to check.", file=sys.stderr)
        return 2

    files_by_label = {}
    failures = []
    for p in paths:
        if not p.is_file():
            print("ERROR: file not found: %s" % p, file=sys.stderr)
            return 2
        subtrees = rule_subtrees(p.read_text())
        if not subtrees:
            failures.append(
                "%s: no serverFiles key holding alert groups. Either the file "
                "stopped shipping rules or this guard stopped finding them." % p
            )
            continue
        cluster = parity.cluster_for(p)
        for key, rule_text in subtrees.items():
            label = "%s::%s" % (cluster.key if cluster else p.parent.parent.name, key)
            files_by_label[label] = rule_text
            failures.extend(static_problems(label, rule_text))

    if not rule_test_files():
        failures.append("%s: no *.test.yaml rule unit tests found." % RULE_TESTS_DIR)

    images = tuple(
        i.strip() for i in os.environ.get("PROMTOOL_IMAGES", "").split(",") if i.strip()
    ) or PINNED_IMAGES
    required = os.environ.get("REQUIRE_PROMTOOL") == "1"

    exe = runtime()
    if exe is None:
        message = (
            "SKIP: promtool check NOT run -- no working container runtime "
            "(docker/podman). The static experimental-function check above DID "
            "run over %d rule file(s); PromQL syntax beyond that is UNVERIFIED."
            % len(files_by_label)
        )
        if required:
            failures.append(message.replace("SKIP", "FAIL", 1) + " REQUIRE_PROMTOOL=1 is set.")
        else:
            print(message, file=sys.stderr)
    elif files_by_label:
        failures.extend(promtool_problems(exe, images, files_by_label))

    if failures:
        print("Prometheus rule PARSE check FAILED.\n")
        for f in failures:
            print(f)
            print()
        print(
            "A rule file that does not parse loads ZERO groups: a fresh server "
            "refuses to start and crashloops (exit 2 on 3.11.2, exit 1 on "
            "2.47.0), and a running one keeps its previous rule set and logs "
            "'error loading rules, previous rule set restored'.",
            file=sys.stderr,
        )
        return 1

    print(
        "Prometheus rule PARSE OK: %d rule file(s) parse under %s."
        % (len(files_by_label), ", ".join(images) if exe else "the static check only")
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
