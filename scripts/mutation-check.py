#!/usr/bin/env python3
"""Run a declared set of mutations against a test suite and report which die.

    scripts/mutation-check.py run    <spec>.json [--only NAME] [-v]
    scripts/mutation-check.py verify <spec>.json [--sha REV]

`run` applies each mutation, runs the suite, and asserts the NAMED tests die — so a
mutation that kills some other test is an error, not a pass. `verify` asserts claims
against COMMITTED blobs, which is the check to use after a fix round; `--sha` audits
an older commit.

WHY A TOOL AND NOT A SHELL LOOP. Each guard below exists because the hand-written
equivalent returned a WRONG answer that a reviewer then had to catch. Do not remove
one because it looks paranoid:

  * dirty tree refused, restores by explicit SHA — `git checkout -- <path>` once
    discarded an uncommitted fix, which then shipped.
  * mutations are structured data, occurrence count asserted — a shell delimiter
    split a spec on the `:` inside `value: "10"`, making every mutation a no-op.
  * no parseable summary is ERROR, never SURVIVED; node version and baseline count
    asserted — a suite that never ran printed nothing, and "0 failures" was read as
    "0 killed".
  * `verify` reads committed blobs only — verification once read the working tree
    after a restore, confirming the file it had just reverted.
  * `invalidate_pycache` — when the tool mutates a Python file its own tests import,
    a cached `.pyc` outlived the restore and one mutation reported the PREVIOUS
    mutation's failure signature. A wrong verdict that did not reproduce.

Spec format: see `--help` on `run`, or `scripts/mutation-check-self-spec.json` (which
mutates this file) and `e2e/bots-app/mutation-spec.json`. Unknown keys are rejected,
so a typo cannot silently drop an assertion.

Runners: `vitest`, `cargo`, `unittest`.

"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

KILLED, SURVIVED, ERROR = "KILLED", "SURVIVED", "ERROR"


SPEC_TOP = {"runner", "cwd", "command", "node", "baseline_tests", "mutations",
            "committed_claims", "_comment"}
SPEC_MUT = {"name", "file", "find", "replace", "occurrences", "expect_kills"}
SPEC_CLAIM = {"file", "present", "absent"}


def validate_spec(spec: dict, cmd: str) -> None:
    """Reject unknown keys outright.

    A typo like `expect_kill` or `occurences` would otherwise be ignored, silently
    dropping the assertion it was meant to make — a verifier that quietly checks
    less than its spec says is worse than no verifier.
    """
    errs = [f"unknown top-level key: {k!r}" for k in spec if k not in SPEC_TOP]
    if cmd == "run":
        for req in ("runner", "command", "baseline_tests", "mutations"):
            if req not in spec:
                errs.append(f"missing required key for `run`: {req!r}")
        for i, m in enumerate(spec.get("mutations", [])):
            errs += [f"mutations[{i}]: unknown key {k!r}" for k in m if k not in SPEC_MUT]
            errs += [f"mutations[{i}]: missing {r!r}" for r in
                     ("name", "file", "find", "replace", "expect_kills") if r not in m]
    for i, c in enumerate(spec.get("committed_claims", [])):
        errs += [f"committed_claims[{i}]: unknown key {k!r}" for k in c if k not in SPEC_CLAIM]
        if "file" not in c or not ({"present", "absent"} & set(c)):
            errs.append(f"committed_claims[{i}]: needs `file` plus `present` or `absent`")
    if errs:
        sys.exit("INVALID SPEC:\n" + "".join(f"    {e}\n" for e in errs))


def repo_root() -> Path:
    out = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
    )
    return Path(out.stdout.strip())


def git(*args: str, cwd: Path) -> str:
    return subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, check=True
    ).stdout


def classify(expect_kills: list[str], failed: list[str]) -> tuple[str, str]:
    """Decide KILLED / SURVIVED / ERROR from the expected and actual failures.

    Extracted from the run loop so it can be tested directly: this is the step
    that could most easily manufacture a false KILLED.

    `expect_kills` entries are substrings, which is deliberate (real test names are
    long) but means a mutation could kill some OTHER test whose name happens to
    contain the substring. Two mitigations: EVERY expected entry must match, and any
    unexpected extra failures are surfaced in the detail so a mutation that is
    broader than intended is visible rather than silently green.
    """
    if not expect_kills:
        return ERROR, "spec declares no expect_kills — nothing to assert"
    matched = {e for e in expect_kills if any(e.lower() in f.lower() for f in failed)}
    missing = [e for e in expect_kills if e not in matched]
    if not failed:
        return SURVIVED, ""
    if missing:
        return ERROR, f"tests failed but not the named one(s); missing {missing}, got {failed}"
    unexpected = [f for f in failed if not any(e.lower() in f.lower() for e in expect_kills)]
    detail = f"also killed (mutation may be broader than intended): {unexpected}" if unexpected else ""
    return KILLED, detail


@dataclass
class Result:
    name: str
    verdict: str
    killed: list[str]
    detail: str = ""


class Runner:
    """Wraps the suite command and parses its output into (passed, failed_names).

    Returning `None` for the parse is load-bearing: it means the runner did not
    produce a result at all (bad node version, compile error, crash). Callers
    MUST treat that as ERROR — the failure mode that once turned "never ran" into
    "nothing was killed".
    """

    def __init__(self, spec: dict, root: Path, verbose: bool):
        self.kind = spec["runner"]
        self.cwd = root / spec.get("cwd", ".")
        self.command = spec["command"]
        self.node = spec.get("node")
        self.timeout_s = spec.get("timeout_s", 900)
        self.verbose = verbose
        if self.kind not in ("vitest", "cargo", "unittest"):
            sys.exit(f"unsupported runner: {self.kind!r} (expected vitest, cargo or unittest)")

    def _shell(self) -> list[str]:
        cmd = shlex.join(self.command)
        if self.node:
            # nvm is a shell function, so it cannot be exec'd directly. Sourcing it
            # here is exactly the step that was forgotten by hand.
            cmd = (
                'export NVM_DIR="${NVM_DIR:-$HOME/.config/nvm}"; '
                '. "$NVM_DIR/nvm.sh" >/dev/null 2>&1; '
                f"nvm use {self.node} >/dev/null 2>&1; "
                'echo "MUTATION_CHECK_NODE=$(node -v)"; ' + cmd
            )
        return ["bash", "-lc", cmd]

    def run(self) -> tuple[int | None, list[str], str]:
        # PYTHONDONTWRITEBYTECODE: a mutated .py must not leave a cached .pyc that a
        # LATER run could load after the restore. Combined with invalidate_pycache()
        # below this closes a non-deterministic wrong-verdict path — the worst
        # outcome for a verifier, since it does not reproduce.
        env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}
        try:
            # Bounded: a hung suite (vitest awaiting a socket, a cargo deadlock) would
            # otherwise stall the tool indefinitely behind a blank screen.
            proc = subprocess.run(
                self._shell(), cwd=self.cwd, capture_output=True, text=True, env=env,
                timeout=self.timeout_s,
            )
        except subprocess.TimeoutExpired:
            return None, [], f"suite exceeded timeout_s={self.timeout_s}"
        out = proc.stdout + proc.stderr
        if self.verbose:
            print(out)
        if self.node:
            m = re.search(r"MUTATION_CHECK_NODE=v(\d+)", out)
            if not m:
                return None, [], "could not determine node version"
            if m.group(1) != str(self.node).lstrip("v"):
                return None, [], f"wrong node: got v{m.group(1)}, spec wants {self.node}"
        parser = {"vitest": self._parse_vitest, "cargo": self._parse_cargo,
                  "unittest": self._parse_unittest}[self.kind]
        passed, failed = parser(out)
        if passed is None:
            tail = "\n".join(out.strip().splitlines()[-6:])
            return None, [], f"no parseable test summary; tail:\n{tail}"
        return passed, failed, ""

    @staticmethod
    def _parse_vitest(out: str) -> tuple[int | None, list[str]]:
        # "Tests  931 passed (931)" | "Tests  1 failed | 930 passed (931)"
        m = re.search(r"^\s*Tests\s+(?:(\d+) failed \| )?(\d+) passed", out, re.M)
        if not m:
            return None, []
        names = [
            n.strip()
            for n in re.findall(r"^\s+×\s+(.*?)(?:\s+\d+ms)?\s*$", out, re.M)
        ]
        return int(m.group(2)), names

    @staticmethod
    def _parse_unittest(out: str) -> tuple[int | None, list[str]]:
        # "Ran 25 tests in 0.049s" then "OK" or "FAILED (failures=2, errors=1)".
        # `Ran N` is the only reliable total; the trailer carries the breakdown.
        m = re.search(r"^Ran (\d+) tests? in ", out, re.M)
        if not m:
            return None, []
        total = int(m.group(1))
        names = re.findall(r"^(?:FAIL|ERROR): (\S+)", out, re.M)
        return total - len(names), names

    @staticmethod
    def _parse_cargo(out: str) -> tuple[int | None, list[str]]:
        # "test result: ok. 117 passed; 0 failed" (possibly several targets)
        totals = re.findall(r"^test result: \w+\.\s+(\d+) passed", out, re.M)
        if not totals:
            return None, []
        names = re.findall(r"^test (\S+) \.\.\. FAILED", out, re.M)
        return sum(int(t) for t in totals), names


def assert_clean(root: Path, files: list[str]) -> None:
    """Refuse to touch a dirty tree.

    This is the single most important guard in the tool. Restores use
    `git checkout <sha> -- <path>`, which cannot distinguish a mutation from a
    real edit — so an uncommitted fix in any target file would be destroyed.
    """
    dirty = [
        line[3:]
        for line in git("status", "--porcelain", cwd=root).splitlines()
        if line[3:] in files
    ]
    if dirty:
        sys.exit(
            "REFUSING TO RUN — uncommitted changes in target files:\n"
            + "".join(f"    {d}\n" for d in dirty)
            + "\nRestores use `git checkout <sha> -- <path>`, which would DISCARD them.\n"
            "Commit (or stash) first, then re-run. This guard exists because that\n"
            "exact sequence silently dropped a fix that then shipped."
        )


def invalidate_pycache(path: Path) -> None:
    """Remove cached bytecode for `path`.

    Python's cache is keyed on source mtime+size, so a mutation that happens to
    preserve both — or a `git checkout` restore whose mtime lands inside the same
    granularity — can leave a subprocess importing the PREVIOUS version. Observed
    in practice while this tool mutated itself: a mutation reported the failure
    signature of the mutation before it.
    """
    if path.suffix != ".py":
        return
    for pyc in (path.parent / "__pycache__").glob(f"{path.stem}.*.pyc"):
        pyc.unlink(missing_ok=True)


def preflight(root: Path, muts: list[dict]) -> None:
    """Check every anchor BEFORE running anything.

    Without this, a stale anchor or a renamed file is only discovered when that
    mutation's turn arrives — after the baseline and every preceding suite run. Cheap
    on a 0.6s spec, ~9 minutes on a workspace-scoped cargo spec, for a defect
    detectable in under a millisecond.
    """
    errs = []
    cache: dict[str, str] = {}
    for i, m in enumerate(muts):
        path = root / m["file"]
        if m["file"] not in cache:
            if not path.is_file():
                errs.append(f"{m['name']!r}: {m['file']} does not exist")
                continue
            cache[m["file"]] = path.read_text()
        text = cache[m["file"]]
        want = m.get("occurrences", 1)
        found = text.count(m["find"])
        if found != want:
            errs.append(
                f"{m['name']!r}: anchor appears {found}x in {m['file']}, spec expects "
                f"{want}x — {'code moved?' if found == 0 else 'ambiguous'}"
            )
        if m["find"] == m["replace"]:
            errs.append(f"{m['name']!r}: find == replace, mutation would be a no-op")
    if errs:
        sys.exit("SPEC DOES NOT MATCH THE TREE:\n" + "".join(f"    {e}\n" for e in errs))


def apply_mutation(path: Path, find: str, replace: str, want: int) -> None:
    text = path.read_text()
    found = text.count(find)
    if found != want:
        raise RuntimeError(
            f"anchor appears {found}x, spec expects {want}x — refusing to guess "
            f"which one to mutate (anchor: {find!r})"
        )
    mutated = text.replace(find, replace, 1)
    if mutated == text:
        raise RuntimeError("replacement produced an identical file — mutation would be a no-op")
    path.write_text(mutated)


def cmd_run(spec: dict, root: Path, only: str | None, verbose: bool) -> int:
    muts = spec["mutations"]
    if only:
        muts = [m for m in muts if only.lower() in m["name"].lower()]
        if not muts:
            sys.exit(f"--only {only!r} matched no mutation")
    files = sorted({m["file"] for m in muts})
    assert_clean(root, files)
    preflight(root, muts)
    sha = git("rev-parse", "HEAD", cwd=root).strip()
    runner = Runner(spec, root, verbose)

    # Serial BY DESIGN, not by omission: mutations rewrite files in the single shared
    # working tree and the runner reads that whole tree, so even mutations to different
    # files cannot safely overlap. Parallelising needs one `git worktree` per job, which
    # costs a fresh node_modules or target/ per job — against sub-10s specs, nil payoff.
    print(f"HEAD {sha[:8]}  ·  {len(muts)} mutation(s)  ·  runner={runner.kind}")
    print("baseline …", end=" ", flush=True)
    passed, failed, err = runner.run()
    if passed is None:
        sys.exit(f"\nBASELINE DID NOT RUN — {err}")
    if failed:
        sys.exit(f"\nBASELINE IS RED ({len(failed)} failing) — fix before mutating:\n  " + "\n  ".join(failed))
    expected = spec["baseline_tests"]
    if passed != expected:
        sys.exit(
            f"\nBASELINE COUNT MISMATCH — ran {passed}, spec says {expected}.\n"
            "Update `baseline_tests` deliberately; a drifting count usually means\n"
            "the command is scoped differently than you think."
        )
    print(f"{passed} passed\n")

    results: list[Result] = []
    for m in muts:
        path = root / m["file"]
        original = path.read_text()
        print(f"  {m['name']} …", end=" ", flush=True)
        try:
            apply_mutation(path, m["find"], m["replace"], m.get("occurrences", 1))
            invalidate_pycache(path)
            passed, failed, err = runner.run()
            if passed is None:
                results.append(Result(m["name"], ERROR, [], err))
                print(f"{ERROR} ({err.splitlines()[0]})")
                continue
            verdict, detail = classify(m.get("expect_kills", []), failed)
            results.append(Result(m["name"], verdict, failed, detail))
            suffix = f" by {len(failed)} test(s)" if verdict == KILLED else ""
            print(f"{verdict}{suffix}" + (f" — {detail}" if detail else ""))
        except RuntimeError as exc:
            results.append(Result(m["name"], ERROR, [], str(exc)))
            print(f"{ERROR} ({exc})")
        finally:
            # Explicit SHA, not the index: restores exactly what was committed even
            # if something else moved the index mid-run.
            subprocess.run(
                ["git", "checkout", sha, "--", m["file"]], cwd=root, check=False,
                capture_output=True,
            )
            invalidate_pycache(path)
            if path.read_text() != original:
                # Not just a warning: a dirty tree after the run invalidates every
                # later verdict, so it must reach the exit code.
                results.append(Result(m["name"] + " [restore]", ERROR, [],
                                      f"{m['file']} did not restore cleanly — check `git diff`"))
                print(f"    ERROR: {m['file']} did not restore cleanly — check `git diff`")

    bad = [r for r in results if r.verdict != KILLED]
    print()
    for r in results:
        mark = "ok" if r.verdict == KILLED else "!!"
        print(f"  [{mark}] {r.verdict:<8} {r.name}" + (f"  — {r.detail}" if r.detail else ""))
    print(f"\n{len(results) - len(bad)}/{len(results)} killed")
    if bad:
        print("\nA SURVIVOR means the test does not guard what it claims.")
        print("An ERROR means the run is inconclusive — fix the harness, do not interpret it.")
    return 1 if bad else 0


def cmd_verify(spec: dict, root: Path, rev: str) -> int:
    """Assert claims against COMMITTED blobs, never the working tree.

    `rev` defaults to HEAD but accepts any commit, so a past commit can be
    audited after the fact ("would this claim have caught what shipped?").
    """
    claims = spec.get("committed_claims", [])
    if not claims:
        sys.exit("spec has no `committed_claims` to verify")
    sha = git("rev-parse", rev, cwd=root).strip()
    print(f"verifying {len(claims)} claim(s) against committed blobs at {sha[:8]}\n")
    failures = 0
    for c in claims:
        blob = subprocess.run(
            ["git", "show", f"{sha}:{c['file']}"], cwd=root, capture_output=True, text=True
        )
        if blob.returncode != 0:
            print(f"  [!!] {c['file']} — not present at {sha[:8]}")
            failures += 1
            continue
        if "present" in c:
            ok = c["present"] in blob.stdout
            what = f"present: {c['present']!r}"
        else:
            ok = c["absent"] not in blob.stdout
            what = f"absent:  {c['absent']!r}"
        print(f"  [{'ok' if ok else '!!'}] {c['file']} — {what}")
        failures += 0 if ok else 1
    print(f"\n{len(claims) - failures}/{len(claims)} claims hold")
    return 1 if failures else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run", help="apply mutations and assert the named tests die")
    r.add_argument("spec", type=Path)
    r.add_argument("--only", help="substring: run just the matching mutation(s)")
    r.add_argument("-v", "--verbose", action="store_true", help="stream runner output")
    v = sub.add_parser("verify", help="assert claims about COMMITTED content")
    v.add_argument("spec", type=Path)
    v.add_argument("--sha", default="HEAD", help="commit to audit (default HEAD)")
    args = ap.parse_args()

    spec = json.loads(args.spec.read_text())
    validate_spec(spec, args.cmd)
    root = repo_root()
    if args.cmd == "run":
        return cmd_run(spec, root, args.only, args.verbose)
    return cmd_verify(spec, root, args.sha)


if __name__ == "__main__":
    sys.exit(main())
