#!/usr/bin/env python3
"""Regression tests for check_api_readiness_probe.py (#2831).

Each test breaks one thing in a copy of the tree and asserts the guard notices.
"""
from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("check_api_readiness_probe.py")
ROOT = SCRIPT.parent.parent
API_VALUES = "helm/meeting-api/values.yaml"
API_DEPLOYMENT = "helm/meeting-api/templates/deployment.yaml"
UI_DEPLOYMENT = "helm/videocall-ui/templates/deployment.yaml"
PROBE_TEMPLATE = "          readinessProbe:\n            {{- toYaml .Values.readinessProbe | nindent 12 }}\n"


def run(repo_root):
    return subprocess.run(
        [sys.executable, str(SCRIPT), "--repo-root", str(repo_root)],
        capture_output=True,
        text=True,
        cwd=str(ROOT),
    )


class ApiReadinessProbeGuardTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        shutil.copytree(ROOT / "helm", self.tmp / "helm")
        shutil.copytree(ROOT / ".github" / "workflows", self.tmp / ".github" / "workflows")

    def mutate(self, rel, old, new):
        path = self.tmp / rel
        text = path.read_text()
        self.assertIn(old, text, f"{rel}: mutation source not present")
        path.write_text(text.replace(old, new, 1))

    def assert_caught(self, needle):
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(needle, result.stderr)

    def test_the_unmutated_copy_passes(self):
        result = run(self.tmp)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("OK:", result.stdout)

    def test_meeting_api_without_a_probe_is_caught(self):
        self.mutate(API_DEPLOYMENT, PROBE_TEMPLATE, "")
        self.assert_caught("no readinessProbe")

    def test_videocall_ui_without_a_probe_is_caught(self):
        self.mutate(UI_DEPLOYMENT, PROBE_TEMPLATE, "")
        self.assert_caught("no readinessProbe")

    def test_a_probe_on_the_wrong_port_is_caught(self):
        self.mutate(API_VALUES, "    path: /version\n    port: 8081\n", "    path: /version\n    port: 8080\n")
        self.assert_caught("but the Service targets 8081")

    def test_one_deploy_step_that_nulls_the_probe_is_caught(self):
        self.mutate(
            ".github/workflows/daily-deploy-ascend.yaml",
            '            --set "env[0].name=DATABASE_URL" \\\n',
            '            --set readinessProbe=null \\\n            --set "env[0].name=DATABASE_URL" \\\n',
        )
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("daily-deploy-ascend.yaml meeting-api: no readinessProbe", result.stderr)
        self.assertEqual(result.stderr.count("  - "), 1, result.stderr)

    def test_a_probe_on_the_wrong_path_is_caught(self):
        self.mutate(API_VALUES, "    path: /version\n", "    path: /healthz\n")
        self.assert_caught("readiness path '/healthz'")

    def test_a_listen_addr_that_moves_off_the_probe_port_is_caught(self):
        self.mutate(
            ".github/workflows/daily-deploy-hcl.yaml",
            '--set "env[11].value=0.0.0.0:8081"',
            '--set "env[11].value=0.0.0.0:9000"',
        )
        self.assert_caught("LISTEN_ADDR port 9000")


if __name__ == "__main__":
    unittest.main()
