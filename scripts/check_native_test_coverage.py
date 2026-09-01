#!/usr/bin/env python3
"""Fails when a plain `#[test]` in videocall-client executes in NO CI job (#2446).

`wasm-pack test` discovers only `#[wasm_bindgen_test]` and `cargo test --lib`
runs natively, so a `#[test]` the native build compiles out -- behind a wasm32
cfg, or behind a feature no test step enables -- reads as coverage and provides
none, silently: `cargo test <name>` on one prints "running 0 tests" and exits 0.

Every bare `#[test]` under videocall-client/src must match a test the native
binary lists. Fewer executed is the defect; more executed means the source scan
has gone blind. Usage: [--src-dir DIR] [--list-file FILE]
"""

import argparse
import os
import re
import subprocess
import sys

# Anchored both ends: the crate mentions `#[test]` inside prose in a dozen
# places, and an unanchored match would count those.
TEST_ATTR = re.compile(r"^[ \t]*#\[test\][ \t]*$")
LIST_LINE = re.compile(r"^(?P<path>[A-Za-z0-9_:]+): test$")
FN_DECL = re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z0-9_]+)")

# `netsim` is off by default and its modules carry native-only test suites.
CARGO_LIST_CMD = [
    "cargo",
    "test",
    "-p",
    "videocall-client",
    "--features",
    "netsim",
    "--lib",
    "--",
    "--list",
]

REMEDIES = """Fix each one of these ways:
  - Drop the cfg gate if the test already passes natively (many gates are
    stale: a native fallback was added to the code under test later).
  - Lift the decision under test into a pure fn outside the gate and pin that
    with a native #[test].
  - If it genuinely needs a browser, make it #[wasm_bindgen_test] so
    `wasm-pack test --headless --chrome` runs it."""


def source_tests(src_dir):
    """Return [(relative_path, line_no, fn_name)] for every bare `#[test]` fn."""
    found = []
    for dirpath, _dirnames, filenames in os.walk(src_dir):
        for filename in sorted(filenames):
            if not filename.endswith(".rs"):
                continue
            path = os.path.join(dirpath, filename)
            with open(path, encoding="utf-8") as handle:
                lines = handle.read().split("\n")
            for index, line in enumerate(lines):
                if not TEST_ATTR.match(line):
                    continue
                name = "<unresolved>"
                for follow in lines[index + 1 : index + 13]:
                    match = FN_DECL.match(follow)
                    if match:
                        name = match.group(1)
                        break
                found.append((os.path.relpath(path, src_dir), index + 1, name))
    return sorted(found)


def listed_tests(list_output):
    """Return the leaf fn names cargo reported, one entry per listed test."""
    names = []
    for line in list_output.split("\n"):
        match = LIST_LINE.match(line.strip())
        if match:
            names.append(match.group("path").split("::")[-1])
    return names


def check(src_dir, list_output):
    """Return (exit_code, report_lines)."""
    declared = source_tests(src_dir)
    executed = listed_tests(list_output)
    report = [
        f"declared bare #[test] fns under {src_dir}: {len(declared)}",
        f"tests listed by the native binary:        {len(executed)}",
    ]
    if len(declared) == len(executed):
        report.append("OK: every declared #[test] is reachable natively.")
        return 0, report

    if len(executed) > len(declared):
        report.append(
            "ERROR: the native binary lists MORE tests than this script found in "
            "the source. The source scan has gone blind -- fix TEST_ATTR in "
            "scripts/check_native_test_coverage.py before trusting this gate."
        )
        return 2, report

    executed_names = set(executed)
    report += [
        "",
        f"ERROR: {len(declared) - len(executed)} plain #[test] fn(s) are compiled "
        "out of the native test binary and are discovered by no wasm-pack step "
        "either, so they execute in NO CI job (issue #2446).",
        "",
        "Likely offenders (declared in source, absent from the run):",
    ]
    for path, line_no, name in declared:
        if name not in executed_names:
            report.append(f"  {path}:{line_no}  {name}")
    report += ["", REMEDIES]
    return 1, report


def main(argv=None):
    parser = argparse.ArgumentParser(description="see module docstring")
    parser.add_argument("--src-dir", default=None)
    parser.add_argument("--list-file", default=None)
    args = parser.parse_args(argv)

    src_dir = args.src_dir
    if src_dir is None:
        root = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        src_dir = os.path.join(root, "videocall-client", "src")

    if args.list_file:
        with open(args.list_file, encoding="utf-8") as handle:
            list_output = handle.read()
    else:
        completed = subprocess.run(CARGO_LIST_CMD, capture_output=True, text=True)
        if completed.returncode != 0:
            sys.stderr.write(completed.stdout)
            sys.stderr.write(completed.stderr)
            sys.stderr.write(
                "ERROR: `%s` failed (exit %d). This gate cannot report a pass "
                "on a build it could not run.\n"
                % (" ".join(CARGO_LIST_CMD), completed.returncode)
            )
            return 2
        list_output = completed.stdout

    code, report = check(src_dir, list_output)
    print("\n".join(report))
    return code


if __name__ == "__main__":
    sys.exit(main())
