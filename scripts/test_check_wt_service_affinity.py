#!/usr/bin/env python3
"""Regression tests for check_wt_service_affinity.py (#2727).

Each test breaks exactly one thing the guard protects, in a copy of the tree,
and asserts the guard notices. Every mutation asserts that it applied, so an
edit that silently matched nothing cannot read as a passing arm.
"""
from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("check_wt_service_affinity.py")
ROOT = SCRIPT.parent.parent
WORKFLOWS = (
    ".github/workflows/daily-deploy-hcl.yaml",
    ".github/workflows/daily-deploy-labsworkspace.yaml",
    ".github/workflows/daily-deploy-ascend.yaml",
)
LOADBALANCER = "helm/rustlemania-webtransport/templates/loadbalancer.yaml"
VALIDATE = "helm/rustlemania-webtransport/templates/validate-values.yaml"
CHART_VALUES = "helm/rustlemania-webtransport/values.yaml"
HCL_WORKFLOW = WORKFLOWS[0]


def run(repo_root):
    return subprocess.run(
        [sys.executable, str(SCRIPT), "--repo-root", str(repo_root)],
        capture_output=True,
        text=True,
        cwd=str(ROOT),
    )


class WtServiceAffinityGuardTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        shutil.copytree(ROOT / "helm", self.tmp / "helm")
        (self.tmp / ".github" / "workflows").mkdir(parents=True)
        for rel in WORKFLOWS:
            shutil.copy(ROOT / rel, self.tmp / rel)

    def mutate(self, rel, old, new):
        path = self.tmp / rel
        text = path.read_text()
        self.assertIn(old, text, f"{rel}: mutation source not present")
        path.write_text(text.replace(old, new, 1))

    def test_the_unmutated_copy_passes(self):
        result = run(self.tmp)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("OK:", result.stdout)

    def test_a_service_without_a_traffic_policy_is_caught(self):
        self.mutate(LOADBALANCER, "  externalTrafficPolicy: {{ . }}\n", "")
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no externalTrafficPolicy", result.stderr)

    def test_a_service_without_client_affinity_is_caught(self):
        self.mutate(LOADBALANCER, "  sessionAffinity: ClientIP\n", "")
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("sessionAffinity=absent", result.stderr)

    def test_a_shortened_affinity_timeout_is_caught(self):
        self.mutate(
            CHART_VALUES,
            "sessionAffinityTimeoutSeconds: 86400",
            "sessionAffinityTimeoutSeconds: 10800",
        )
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("timeoutSeconds=10800", result.stderr)

    def test_a_k3s_cluster_flipped_to_local_is_caught(self):
        self.mutate(
            HCL_WORKFLOW,
            "--set service.externalTrafficPolicy=Cluster",
            "--set service.externalTrafficPolicy=Local",
        )
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("externalTrafficPolicy=Local, expected Cluster", result.stderr)
        self.assertIn("klipper-lb MASQUERADEs", result.stderr)

    def test_a_do_wrapper_flipped_to_local_is_caught(self):
        self.mutate(
            "helm/global/us-east/webtransport/values.yaml",
            "externalTrafficPolicy: Cluster",
            "externalTrafficPolicy: Local",
        )
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("externalTrafficPolicy=Local, expected Cluster", result.stderr)
        self.assertIn("Flip it together with the guard", result.stderr)

    def test_a_removed_replica_guard_is_caught(self):
        self.mutate(VALIDATE, "{{- if gt (int .Values.replicaCount) 1 }}", "{{- if false }}")
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("the bare chart rendered with replicaCount=2", result.stderr)
        # The wrapper renders the guard in the SUBCHART value scope, which a
        # bare-chart render does not exercise.
        self.assertIn(
            "helm/global/us-east/webtransport rendered with replicaCount=2",
            result.stderr,
        )

    def test_a_removed_autoscaling_guard_is_caught(self):
        self.mutate(
            VALIDATE,
            "{{- if and .Values.autoscaling.enabled (gt (int .Values.autoscaling.maxReplicas) 1) }}",
            "{{- if false }}",
        )
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("an autoscaling ceiling above 1", result.stderr)

    def test_a_removed_timeout_range_guard_is_caught(self):
        self.mutate(VALIDATE, "{{- if eq $affinity \"ClientIP\" }}", "{{- if false }}")
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("a client-affinity timeout above the API maximum", result.stderr)

    def test_a_removed_policy_enum_guard_is_caught(self):
        self.mutate(
            VALIDATE,
            '{{- if and $policy (not (has $policy (list "Local" "Cluster"))) }}',
            "{{- if false }}",
        )
        result = run(self.tmp)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("a mis-cased externalTrafficPolicy", result.stderr)

    def test_a_missing_workflow_is_an_extractor_error(self):
        (self.tmp / HCL_WORKFLOW).unlink()
        result = run(self.tmp)
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("workflow file not found", result.stderr)

    def test_a_step_without_a_helm_command_is_an_extractor_error(self):
        self.mutate(
            HCL_WORKFLOW,
            "helm upgrade --install videocall-webtransport helm/rustlemania-webtransport/",
            "echo skipped",
        )
        result = run(self.tmp)
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("no `helm upgrade --install", result.stderr)

    def test_an_unstubbed_shell_variable_is_an_extractor_error(self):
        self.mutate(
            HCL_WORKFLOW,
            "--set tlsSecret=webtransport-tls",
            "--set tlsSecret=${SOME_UNSTUBBED_VAR}",
        )
        result = run(self.tmp)
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("SOME_UNSTUBBED_VAR", result.stderr)

    def test_a_tree_without_wrappers_is_an_extractor_error(self):
        shutil.rmtree(self.tmp / "helm" / "global")
        result = run(self.tmp)
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("refusing to pass", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
