#!/usr/bin/env python3
"""Regression tests for check_prometheus_alert_parity.py (#2713).

Each mutation test breaks exactly the property the guard claims to protect and
asserts the guard notices, so a guard that silently stopped comparing would
fail here rather than pass vacuously.

Copies are laid out under the same relative paths as the real files so the
guard's cluster lookup resolves them; the deploy sources it reads to verify
container names are the real ones in the repo and are never mutated.
"""
from __future__ import annotations

import importlib.util
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("check_prometheus_alert_parity.py")
ROOT = SCRIPT.parent.parent

_spec = importlib.util.spec_from_file_location("alert_parity", SCRIPT)
guard = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(guard)

REL = [c.values for c in guard.CLUSTERS]

LAG_EXPR = (
    "expr: histogram_quantile(0.99, sum by (le, pod) "
    "(rate(videocall_relay_scheduler_lag_ms_bucket[5m]))) > 100"
)


def run(paths):
    return subprocess.run(
        [sys.executable, str(SCRIPT)] + [str(p) for p in paths],
        capture_output=True,
        text=True,
        cwd=str(ROOT),
    )


class ParityGuardTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="alert-parity-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.copies = []
        for rel in REL:
            dst = self.tmp / rel
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / rel, dst)
            self.copies.append(dst)
        self.us, self.hcl, self.daily = self.copies

    def edit(self, path, old, new, count=1):
        text = path.read_text()
        self.assertIn(old, text, "fixture text not found in %s" % path.name)
        path.write_text(text.replace(old, new, count))

    # --- the live assertions --------------------------------------------------
    def test_tracked_files_are_in_parity(self):
        r = run([ROOT / rel for rel in REL])
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("parity OK", r.stdout)

    def test_default_argv_checks_the_tracked_files_from_any_cwd(self):
        r = subprocess.run(
            [sys.executable, str(SCRIPT)],
            capture_output=True,
            text=True,
            cwd=str(self.tmp),
        )
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("parity OK", r.stdout)

    def test_copies_are_in_parity(self):
        r = run(self.copies)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    # --- parity mutations -----------------------------------------------------
    def test_changed_threshold_is_drift(self):
        self.edit(self.hcl, LAG_EXPR, LAG_EXPR.replace("> 100", "> 250"))
        r = run(self.copies)
        self.assertEqual(r.returncode, 1)
        self.assertIn("DRIFT in group videocall_relay_alerts", r.stdout)
        self.assertIn("> 250", r.stdout)

    def test_changed_for_duration_is_drift(self):
        text = self.daily.read_text()
        head, sep, tail = text.partition("- alert: RelaySchedulerLagHigh")
        self.assertTrue(sep)
        self.daily.write_text(head + sep + tail.replace("for: 1m", "for: 30m", 1))
        r = run(self.copies)
        self.assertEqual(r.returncode, 1)
        self.assertIn("for: 30m", r.stdout)

    def test_removed_alert_is_drift(self):
        text = self.hcl.read_text()
        start = text.index("          - alert: RelaySchedulerLagHigh")
        end = text.index("      - name: videocall_quality_alerts")
        self.assertLess(start, end)
        self.hcl.write_text(text[:start] + text[end:])
        r = run(self.copies)
        self.assertEqual(r.returncode, 1)
        self.assertIn("DRIFT", r.stdout)

    def test_added_alert_is_drift(self):
        extra = (
            "          - alert: MadeUpRule\n"
            "            expr: vector(1) > 0\n"
            "            for: 1m\n"
            "            labels:\n"
            "              severity: warning\n"
            "            annotations:\n"
            '              summary: "made up"\n'
        )
        self.edit(
            self.hcl,
            "      - name: videocall_resource_alerts",
            extra + "      - name: videocall_resource_alerts",
        )
        r = run(self.copies)
        self.assertEqual(r.returncode, 1)
        self.assertIn("MadeUpRule", r.stdout)

    def test_missing_resource_group_is_reported(self):
        text = self.hcl.read_text()
        start = text.index("      - name: videocall_resource_alerts")
        end = text.index("      - name: videocall_client_stability_alerts")
        self.assertLess(start, end)
        self.hcl.write_text(text[:start] + text[end:])
        r = run(self.copies)
        self.assertEqual(r.returncode, 1)
        self.assertIn("MISSING group videocall_resource_alerts", r.stdout)

    def test_missing_quality_group_is_reported(self):
        text = self.hcl.read_text()
        start = text.index("      - name: videocall_quality_alerts")
        end = text.index("      - name: videocall_resource_alerts")
        self.assertLess(start, end)
        self.hcl.write_text(text[:start] + text[end:])
        r = run(self.copies)
        self.assertEqual(r.returncode, 1)
        self.assertIn("MISSING group videocall_quality_alerts", r.stdout)

    def test_comment_and_blank_line_differences_are_not_drift(self):
        self.edit(
            self.hcl,
            "          - alert: RelaySchedulerLagHigh",
            "\n          # an explanatory note\n\n          - alert: RelaySchedulerLagHigh",
        )
        r = run(self.copies)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_registered_selector_difference_is_not_drift(self):
        us = self.us.read_text()
        hcl = self.hcl.read_text()
        self.assertIn('container=~".*-us-east|metrics-api.*|nats"', us)
        self.assertIn('container=~"videocall-.*|metrics-api.*|nats"', hcl)
        r = run(self.copies)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_borrowing_another_clusters_container_name_is_caught(self):
        self.edit(
            self.hcl,
            'container=~"videocall-.*|metrics-api.*|nats"',
            'container=~".*-us-east|metrics-api.*|nats"',
        )
        r = run(self.copies)
        self.assertEqual(r.returncode, 1)

    # --- B3: a selector must name a container some deploy produces ------------
    def test_selector_naming_an_undeployed_container_is_reported(self):
        """The exact #2713 miss: `rustlemania-webtransport` was registered as a
        us-east spelling and no cluster deploys it."""
        us_east = [c for c in guard.CLUSTERS if c.key == "us-east"][0]
        bogus = guard.Cluster(
            key=us_east.key,
            values=us_east.values,
            wt="rustlemania-webtransport",
            ws=us_east.ws,
            cpu_set=us_east.cpu_set,
            mem_set=us_east.mem_set,
            name_sources=us_east.name_sources,
        )
        problems = guard.check_selectors(bogus, (ROOT / us_east.values).read_text())
        self.assertTrue(problems)
        self.assertTrue(
            any("is not set as a fullnameOverride" in p for p in problems), problems
        )

    def test_regex_that_misses_the_websocket_container_is_reported(self):
        us_east = [c for c in guard.CLUSTERS if c.key == "us-east"][0]
        bogus = guard.Cluster(
            key=us_east.key,
            values=us_east.values,
            wt=us_east.wt,
            ws=us_east.ws,
            cpu_set="nats",
            mem_set=us_east.mem_set,
            name_sources=us_east.name_sources,
        )
        problems = guard.check_selectors(bogus, (ROOT / us_east.values).read_text())
        # #2727: cpu_set must cover both relays, not only the websocket one.
        for name in (us_east.wt, us_east.ws):
            self.assertTrue(
                any("cpu_set 'nats' does not match container %r" % name in p
                    for p in problems),
                problems,
            )

    def test_real_clusters_pass_the_selector_check(self):
        for c in guard.CLUSTERS:
            problems = guard.check_selectors(c, (ROOT / c.values).read_text())
            self.assertEqual(problems, [], "%s: %s" % (c.key, problems))

    # --- B1: rules must land where the server loads them ----------------------
    UNLOADED = (
        "serverFiles:\n"
        "  alert_rules.yml:\n"
        "    groups:\n"
        "      - name: videocall_relay_alerts\n"
        "        rules: []\n"
    )
    LOADED_BY_DEFAULT = UNLOADED.replace("alert_rules.yml", "alerting_rules.yml")
    LOADED_BY_OVERRIDE = (
        "serverFiles:\n"
        "  prometheus.yml:\n"
        "    rule_files:\n"
        '      - "alert_rules.yml"\n'
        "  alert_rules.yml:\n"
        "    groups:\n"
        "      - name: videocall_relay_alerts\n"
        "        rules: []\n"
    )

    def test_rules_under_an_unloaded_serverfiles_key_is_reported(self):
        problems = guard.check_rule_files("fake.yaml", self.UNLOADED)
        self.assertTrue(problems)
        self.assertIn("alert_rules.yml", problems[0])
        self.assertIn("ZERO groups", problems[0])

    def test_rules_under_a_chart_default_key_pass(self):
        self.assertEqual(guard.check_rule_files("fake.yaml", self.LOADED_BY_DEFAULT), [])

    def test_rules_under_an_overridden_rule_files_key_pass(self):
        self.assertEqual(
            guard.check_rule_files("fake.yaml", self.LOADED_BY_OVERRIDE), []
        )

    def test_real_files_write_rules_where_they_are_loaded(self):
        for c in guard.CLUSTERS:
            problems = guard.check_rule_files(c.values, (ROOT / c.values).read_text())
            self.assertEqual(problems, [], "%s: %s" % (c.key, problems))

    def test_renaming_an_hcl_key_back_to_alert_rules_fails_end_to_end(self):
        self.edit(self.hcl, "\n  alerting_rules.yml:\n", "\n  alert_rules.yml:\n")
        r = run(self.copies)
        self.assertEqual(r.returncode, 1)
        self.assertIn("ZERO groups", r.stdout)

    # --- usage ---------------------------------------------------------------
    def test_single_file_is_a_usage_error(self):
        r = run([self.hcl])
        self.assertEqual(r.returncode, 2)

    def test_unknown_file_is_a_usage_error(self):
        stray = self.tmp / "stray.yaml"
        stray.write_text("serverFiles: {}\n")
        r = run([self.hcl, stray])
        self.assertEqual(r.returncode, 2)
        self.assertIn("not a known cluster values file", r.stderr)

    def test_missing_file_fails(self):
        r = run([self.hcl, self.tmp / "nope.yaml"])
        self.assertEqual(r.returncode, 1)
        self.assertIn("file not found", r.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
