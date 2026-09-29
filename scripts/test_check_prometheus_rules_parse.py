#!/usr/bin/env python3
"""Regression tests for check_prometheus_rules_parse.py (#2734).

Each test breaks one property the guard claims and asserts it notices. Copies
sit at the real relative paths so the guard's cluster lookup resolves them.
"""
from __future__ import annotations

import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("check_prometheus_rules_parse.py")
ROOT = SCRIPT.parent.parent

_spec = importlib.util.spec_from_file_location("rules_parse", SCRIPT)
guard = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(guard)

REL = [c.values for c in guard.parity.CLUSTERS]
# One opt-out, honoured by every test that reaches a container, so the skips and
# the runtime probe agree. `make test-scripts` sets it to keep the fmt job free
# of a docker dependency; the deploy-parity job runs without it.
SKIP_RUNTIME_REQUESTED = os.environ.get("PROMTOOL_SKIP_RUNTIME") == "1"
HAVE_RUNTIME = guard.runtime() is not None

BAD_EXPR = "first_over_time(videocall_client_memory_used_bytes[5m])"
GOOD_EXPR = "(videocall_client_memory_used_bytes offset 5m)"

# SUCCESS on 3.11.2, exit 1 on 2.45.0, SILENT exit 0 on 2.47.0.
FULL_EXPR = (
    "videocall_client_memory_used_bytes / on(meeting_id, session_id, peer_id) "
    "(videocall_client_memory_used_bytes offset 5m) > 2"
)
THREE_X_ONLY_EXPR = "'{\"videocall_client_memory_used_bytes\"} > 2'"


def run(paths, env_extra=None, no_runtime=False):
    env = dict(os.environ)
    env.pop("REQUIRE_PROMTOOL", None)
    env.pop("PROMTOOL_IMAGES", None)
    if env_extra:
        env.update(env_extra)
    empty = None
    if no_runtime:
        empty = tempfile.mkdtemp(prefix="emptypath-")
        env["PATH"] = empty
    try:
        res = subprocess.run(
            [sys.executable, str(SCRIPT)] + [str(p) for p in paths],
            capture_output=True, text=True, env=env, cwd=str(ROOT),
        )
    finally:
        if empty:
            shutil.rmtree(empty, ignore_errors=True)
    return res.returncode, res.stdout, res.stderr


class Sandbox:

    def __enter__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="rules-parse-test-"))
        self.paths = []
        for rel in REL:
            dest = self.tmp / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(ROOT / rel, dest)
            self.paths.append(dest)
        return self

    def __exit__(self, *exc):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def mutate(self, index, old, new):
        p = self.paths[index]
        text = p.read_text()
        assert old in text, "fixture no longer contains %r" % old[:60]
        p.write_text(text.replace(old, new, 1))


class RulesParseGuardTest(unittest.TestCase):
    def test_tracked_files_parse(self):
        rc, out, err = run([])
        self.assertEqual(rc, 0, out + err)
        self.assertIn("PARSE OK", out)

    def test_experimental_function_fails_without_a_runtime(self):
        """The static layer alone must catch it; no runtime is not a free pass."""
        with Sandbox() as s:
            s.mutate(0, GOOD_EXPR, BAD_EXPR)
            rc, out, err = run(s.paths, no_runtime=True)
        self.assertEqual(rc, 1, out + err)
        self.assertIn("first_over_time", out)
        self.assertIn("ClientHeapGrowthHigh", out)

    def test_every_listed_experimental_function_is_rejected(self):
        for fn in guard.EXPERIMENTAL_FUNCTIONS:
            text = "groups:\n  - name: g\n    rules:\n      - alert: A\n        expr: %s(up) > 0\n" % fn
            problems = guard.static_problems("t", text)
            self.assertTrue(problems, "static check missed %s()" % fn)
            self.assertIn(fn, problems[0])

    def test_static_check_is_not_vacuous(self):
        """Emptying the constant must blind the static layer."""
        text = "groups:\n  - name: g\n    rules:\n      - alert: A\n        expr: %s > 0\n" % BAD_EXPR
        self.assertTrue(guard.static_problems("t", text))
        real = guard.EXPERIMENTAL_FUNCTIONS
        try:
            guard.EXPERIMENTAL_FUNCTIONS = ()
            self.assertEqual(guard.static_problems("t", text), [])
        finally:
            guard.EXPERIMENTAL_FUNCTIONS = real

    def test_last_over_time_is_not_denied(self):
        """A substring-matching deny-list would fail this."""
        text = "groups:\n  - name: g\n    rules:\n      - alert: A\n        expr: last_over_time(up[5m]) > 0\n"
        self.assertEqual(guard.static_problems("t", text), [])

    @unittest.skipUnless(HAVE_RUNTIME, "no container runtime")
    def test_promtool_catches_a_syntax_error_the_static_layer_cannot(self):
        with Sandbox() as s:
            s.mutate(0, GOOD_EXPR, "(videocall_client_memory_used_bytes offset")
            rc, out, err = run(s.paths)
        self.assertEqual(rc, 1, out + err)
        self.assertIn("promtool exit", out)

    @unittest.skipUnless(HAVE_RUNTIME, "no container runtime")
    def test_promtool_rejects_the_experimental_function_on_both_images(self):
        with Sandbox() as s:
            s.mutate(0, GOOD_EXPR, BAD_EXPR)
            rc, out, err = run(s.paths)
        self.assertEqual(rc, 1, out + err)
        self.assertIn("first_over_time", out)

    def test_pinned_images_span_both_fleet_majors(self):
        """Kills the drop-an-image arms without needing a runtime."""
        majors = {i.rsplit(":v", 1)[1].split(".")[0] for i in guard.PINNED_IMAGES}
        self.assertEqual(majors, {"2", "3"}, guard.PINNED_IMAGES)

    def test_silent_exit_zero_is_a_failure(self):
        bad = guard.promtool_verdict("img", "f.yml", 21, 0, "Checking /rules/f.yml\n")
        self.assertIsNotNone(bad)
        self.assertIn("SILENT", bad)

    def test_rule_count_mismatch_is_a_failure(self):
        bad = guard.promtool_verdict(
            "img", "f.yml", 21, 0, "Checking\n  SUCCESS: 20 rules found\n"
        )
        self.assertIsNotNone(bad)
        self.assertIn("20", bad)

    def test_two_success_lines_is_a_failure(self):
        two = "  SUCCESS: 21 rules found\n  SUCCESS: 21 rules found\n"
        self.assertIsNotNone(guard.promtool_verdict("img", "f.yml", 21, 0, two))

    def test_matching_success_line_passes(self):
        ok = "Checking /rules/f.yml\n  SUCCESS: 21 rules found\n"
        self.assertIsNone(guard.promtool_verdict("img", "f.yml", 21, 0, ok))

    def test_nonzero_exit_is_a_failure(self):
        bad = guard.promtool_verdict("img", "f.yml", 21, 1, "  FAILED:\nparse error")
        self.assertIsNotNone(bad)
        self.assertIn("parse error", bad)

    @unittest.skipUnless(HAVE_RUNTIME, "no container runtime")
    def test_a_three_x_only_expression_is_caught_by_the_two_x_image(self):
        """3.11.2 accepts it; only a real 2.x image plus the SUCCESS rule catches it."""
        with Sandbox() as s:
            s.mutate(0, FULL_EXPR, THREE_X_ONLY_EXPR)
            rc, out, err = run(s.paths)
        self.assertEqual(rc, 1, out + err)
        self.assertIn("v2.47.0", out)
        self.assertIn("SILENT", out)

    def test_permission_denied_is_an_environment_failure(self):
        bad = guard.promtool_verdict(
            "img", "f.yml", 21, 1,
            "promtool: error: stat /rules/f.yml: permission denied, try --help",
        )
        self.assertIsNotNone(bad)
        self.assertIn("ENVIRONMENT FAILURE", bad)

    def test_staged_rules_are_readable_from_inside_the_container(self):
        tmp = Path(tempfile.mkdtemp(prefix="stage-2733-"))
        # Without a strict umask write_text lands on 0644 and this passes vacuously.
        was = os.umask(0o077)
        try:
            os.chmod(tmp, 0o700)
            entries, problems = guard.stage_rules(
                tmp, {"c::rules.yml": "groups:\n  - name: g\n    rules:\n"
                                      "      - alert: A\n        expr: up\n"}
            )
            self.assertEqual(problems, [])
            self.assertEqual(len(entries), 1)
            self.assertEqual(tmp.stat().st_mode & 0o755, 0o755, "dir not traversable")
            for name, _ in entries:
                mode = (tmp / name).stat().st_mode
                self.assertEqual(mode & 0o044, 0o044, "%s not world-readable" % name)
        finally:
            os.umask(was)
            shutil.rmtree(tmp, ignore_errors=True)

    @unittest.skipUnless(hasattr(os, "getuid"), "POSIX only")
    def test_promtool_argv_runs_as_the_invoking_user(self):
        argv = guard.promtool_argv("docker", Path("/w"), "img", "r.yml")
        self.assertIn("--user", argv)
        self.assertEqual(
            argv[argv.index("--user") + 1], "%d:%d" % (os.getuid(), os.getgid())
        )
        self.assertIn("-v", argv)
        self.assertIn("/w:/rules", argv)

    def test_expected_rule_count_counts_both_rule_kinds(self):
        text = (
            "groups:\n  - name: g\n    rules:\n"
            "      - alert: A\n        expr: up\n"
            "      - record: r\n        expr: up\n"
        )
        self.assertEqual(guard.expected_rule_count(text), 2)

    @unittest.skipIf(SKIP_RUNTIME_REQUESTED, "PROMTOOL_SKIP_RUNTIME=1")
    def test_runtime_is_detected_when_docker_answers(self):
        """Probes docker rather than guard.runtime(), so a runtime() stubbed to
        return None is caught rather than skipped past. Gated on the explicit
        opt-out only, never on HAVE_RUNTIME, which the stub would also flip."""
        if shutil.which("docker") is None:
            self.skipTest("docker not installed")
        probe = subprocess.run(
            ["docker", "version", "--format", "{{.Server.Version}}"],
            capture_output=True, text=True,
        )
        if probe.returncode != 0:
            self.skipTest("docker daemon not responding")
        self.assertIsNotNone(guard.runtime())
        rc, out, err = run([])
        self.assertEqual(rc, 0, out + err)
        self.assertNotIn("the static check only", out)
        self.assertIn("prom/prometheus:", out)

    def test_missing_runtime_is_a_failure_when_required(self):
        rc, out, err = run([], env_extra={"REQUIRE_PROMTOOL": "1"}, no_runtime=True)
        self.assertEqual(rc, 1, out + err)
        self.assertIn("REQUIRE_PROMTOOL=1", out)

    def test_missing_runtime_skips_loudly_when_not_required(self):
        rc, out, err = run([], no_runtime=True)
        self.assertEqual(rc, 0, out + err)
        self.assertIn("SKIP: promtool check NOT run", err)
        self.assertIn("UNVERIFIED", err)

    def test_a_values_file_with_no_rule_groups_fails(self):
        with Sandbox() as s:
            s.mutate(0, "groups:", "notgroups:")
            rc, out, err = run([s.paths[0], s.paths[1]], no_runtime=True)
        self.assertEqual(rc, 1, out + err)
        self.assertIn("no serverFiles key holding alert groups", out)

    def test_extractor_finds_the_real_key_on_every_cluster(self):
        found = {}
        for rel in REL:
            subtrees = guard.rule_subtrees((ROOT / rel).read_text())
            found[rel] = sorted(subtrees)
            self.assertTrue(subtrees, "no rule subtree found in %s" % rel)
            for text in subtrees.values():
                self.assertTrue(text.startswith("groups:"), rel)
        self.assertIn("alerting_rules.yml", found["helm/global/hcl/prometheus/values.yaml"])
        self.assertIn("alert_rules.yml", found["helm/global/us-east/prometheus/values.yaml"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
