#!/usr/bin/env python3
"""Run one scale scenario end to end and write its run folder (#2914; design: scenario-runner-design.md).

  scenario_run.py --scenario FILE [--target local|ci] [--stack up|reuse] [--out-dir DIR] [--allow-dirty]
                  [--keep-stack] [--compile-only]

Exit codes follow the scorer: 0 PASS, 1 FAIL, 2 INVALID, 3 ERROR (also a refused scenario or a usage error).
A SIGINT, SIGTERM or SIGHUP before scoring completes (the "scored" line in events.jsonl) ends the run unscored
and INVALID once every bot is stopped
and collected. After scoring and the gates, run.json can still say PASS, with the signal listed under "signals".
Needs PyYAML (scripts/quality/requirements-runner.txt).
"""

import argparse
import hashlib
import json
import os
import signal
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import cq_runner  # noqa: E402
import cq_scenario  # noqa: E402

EXIT_ERROR = 3
ADAPTERS = {}
SIGNALS = ("SIGINT", "SIGTERM", "SIGHUP")


class _Parser(argparse.ArgumentParser):
    def error(self, message):
        self.print_usage(sys.stderr)
        self.exit(EXIT_ERROR, f"{self.prog}: error: {message}\n")


def default_out_dir(environ):
    return environ.get("CQ_RUNS_DIR") or os.path.join(os.path.expanduser("~"), "videocall-scale-runs")


def scenario_label(path, repo_root):
    real, root = os.path.realpath(path), os.path.realpath(repo_root)
    if os.path.commonpath([real, root]) == root:
        return os.path.relpath(real, root)
    return "external:" + os.path.basename(real)


def recorded_argv(argv, label):
    subs = {"--scenario": "$RUN/scenario.yaml" if label.startswith("external:") else label, "--out-dir": "$RUN/.."}
    out, pending = [], None
    for a in argv:
        flag, eq, _ = a.partition("=")
        if pending is not None:
            out.append(subs[pending])
            pending = None
        elif a in subs:
            out.append(a)
            pending = a
        elif eq and flag in subs:
            out.append(f"{flag}={subs[flag]}")
        else:
            out.append(a)
    return out


def build_parser():
    p = _Parser(description=__doc__.splitlines()[0], allow_abbrev=False)
    p.add_argument("--scenario", required=True)
    p.add_argument("--target", choices=sorted(cq_scenario.TARGETS))
    p.add_argument("--stack", choices=("up", "reuse"), default="up")
    p.add_argument("--out-dir")
    p.add_argument("--allow-dirty", action="store_true", help="run a dirty tree; the verdict is capped at INVALID")
    p.add_argument("--keep-stack", action="store_true")
    p.add_argument("--compile-only", action="store_true", help="print the resolved plan and exit")
    return p


def main(argv=None, **deps):
    try:
        return _main(argv, **deps)
    except Exception as exc:  # noqa: BLE001 - exit 1 would read as FAIL
        print(f"internal error: {cq_runner._describe(exc)}", file=sys.stderr)
        return EXIT_ERROR


def _main(argv=None, *, adapters=None, tree=None, clock=None, tools=None, environ=None):
    args = build_parser().parse_args(argv)
    env = os.environ if environ is None else environ
    clock = clock or cq_runner.SystemClock()
    tree = tree or cq_runner.GitTree(os.path.dirname(os.path.abspath(__file__)))
    try:
        doc, raw = cq_scenario.load_scenario(args.scenario)
        root, commit = tree.toplevel(), tree.commit()
        prefix = doc.get("run", {}).get("id_prefix") if isinstance(doc, dict) and isinstance(doc.get("run"), dict) \
            else None
        run_id = cq_scenario.derive_run_id(prefix, clock.now().wall, commit)
        label = scenario_label(args.scenario, root)
        plan = cq_scenario.compile_scenario(doc, run_id=run_id, scenario_file=label,
                                            scenario_sha256=hashlib.sha256(raw).hexdigest())
    except (cq_scenario.ScenarioError, cq_runner.AdapterError) as exc:
        print(exc, file=sys.stderr)
        return EXIT_ERROR
    if args.compile_only:
        print(json.dumps(plan, indent=1))
        return 0
    if args.target is not None and args.target != plan["target"]:
        print(f"--target {args.target} differs from the scenario's run.target {plan['target']}", file=sys.stderr)
        return EXIT_ERROR
    factory = (adapters or ADAPTERS).get(plan["target"])
    if factory is None:
        print(f"no adapters for target {plan['target']} in this build", file=sys.stderr)
        return EXIT_ERROR
    try:
        run_dir = cq_runner.prepare_run_dir(args.out_dir or default_out_dir(env), run_id, root)
        stack, fleet = factory(plan)
    except (cq_runner.Refused, cq_runner.AdapterError) as exc:
        print(exc, file=sys.stderr)
        return EXIT_ERROR
    runner = cq_runner.Runner(plan, raw, run_dir, stack=stack, fleet=fleet, tree=tree, clock=clock, tools=tools,
                              stack_mode=args.stack, keep_stack=args.keep_stack, allow_dirty=args.allow_dirty,
                              argv=["scripts/quality/scenario_run.py"] + recorded_argv(
                                  sys.argv[1:] if argv is None else argv, label))
    previous = {}
    try:
        for name in SIGNALS:
            if hasattr(signal, name):
                previous[name] = signal.signal(getattr(signal, name), runner.on_signal)
        run = runner.run()
    finally:
        for name, handler in previous.items():
            signal.signal(getattr(signal, name), handler)
    failed = [g["gate"] for g in run["gates"] if not g["ok"]]
    print(f"{run['run_id']}: {run['verdict']} (scorer {run['scorer_verdict']}; failed runner gates "
          f"{', '.join(failed) or 'none'}){'; error: ' + run['error'] if run['error'] else ''}", file=sys.stderr)
    return run["exit_code"]


if __name__ == "__main__":
    sys.exit(main())
