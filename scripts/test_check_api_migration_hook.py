#!/usr/bin/env python3
"""Regression tests for check_api_migration_hook.py (#2831).

Each test breaks one thing in a copy of the tree and asserts the guard notices.
"""
from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("check_api_migration_hook.py")
ROOT = SCRIPT.parent.parent
JOB = "helm/meeting-api/templates/migrate-job.yaml"
API_VALUES = "helm/meeting-api/values.yaml"
E2E_COMPOSE = "docker/docker-compose.e2e.yaml"


def run(repo_root):
    return subprocess.run(
        [sys.executable, str(SCRIPT), "--repo-root", str(repo_root)],
        capture_output=True,
        text=True,
        cwd=str(ROOT),
    )


class MigrationHookGuardTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        for rel in ("helm", ".github/workflows", "docker"):
            shutil.copytree(ROOT / rel, self.tmp / rel)
        shutil.copy(ROOT / "Dockerfile.meeting-api", self.tmp / "Dockerfile.meeting-api")

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

    def test_a_missing_job_is_caught(self):
        (self.tmp / JOB).unlink()
        self.assert_caught("expected one migrate Job")

    def test_a_post_upgrade_hook_is_caught(self):
        self.mutate(JOB, "pre-install,pre-upgrade", "post-install,post-upgrade")
        self.assert_caught("expected pre-install,pre-upgrade")

    def test_a_job_that_does_not_run_dbmate_is_caught(self):
        self.mutate(JOB, '["/app/dbmate/startup.sh"]', '["meeting-api"]')
        self.assert_caught("does not run dbmate")

    def test_a_job_with_its_own_database_url_is_caught(self):
        self.mutate(
            JOB,
            "{{- toYaml .Values.env | nindent 12 }}",
            "- name: DATABASE_URL\n              value: postgres://elsewhere/db",
        )
        self.assert_caught("differs from the Deployment's")

    def test_a_deadline_over_the_helm_timeout_is_caught(self):
        self.mutate(JOB, "activeDeadlineSeconds: 240", "activeDeadlineSeconds: 600")
        self.assert_caught("must be under helm --timeout")

    def test_a_job_with_a_different_image_is_caught(self):
        self.mutate(
            JOB,
            'image: "{{ .Values.image.repository }}:{{ .Values.image.tag }}"',
            "image: securityunion/videocall-meeting-api:latest",
        )
        self.assert_caught("migrate Job image ['securityunion/videocall-meeting-api:latest'] differs")

    def test_a_job_without_resources_is_caught(self):
        self.mutate(JOB, "{{- toYaml .Values.migrate.resources | nindent 12 }}", "{}")
        self.assert_caught("no resources.requests for ['cpu', 'memory']")

    def test_a_job_without_memory_limit_is_caught(self):
        self.mutate(API_VALUES, '      cpu: "250m"\n      memory: "128Mi"\n', '      cpu: "250m"\n')
        self.assert_caught("no resources.limits for ['memory']")

    def test_a_migrate_pod_with_the_instance_label_is_caught(self):
        self.mutate(
            JOB,
            "        app.kubernetes.io/component: migrate\n    spec:",
            "        app.kubernetes.io/component: migrate\n        app.kubernetes.io/instance: {{ .Release.Name }}\n    spec:",
        )
        self.assert_caught("migrate pod carries app.kubernetes.io/instance")

    def test_a_job_without_a_lock_timeout_is_caught(self):
        self.mutate(JOB, '              value: "-c lock_timeout=5s"\n', '              value: ""\n')
        self.assert_caught("no PGOPTIONS lock_timeout")

    def test_a_dockerfile_cmd_that_migrates_is_caught(self):
        self.mutate(
            "Dockerfile.meeting-api",
            'CMD [ "meeting-api" ]',
            'CMD [ "/bin/bash", "-c", "/app/dbmate/startup.sh && meeting-api" ]',
        )
        self.assert_caught("still runs migrations in the app container")

    def test_a_compose_service_that_skips_migrations_is_caught(self):
        self.mutate(E2E_COMPOSE, '"/app/dbmate/startup.sh && /app/docker/e2e-backend.sh', '"/app/docker/e2e-backend.sh')
        self.assert_caught("docker-compose.e2e.yaml service meeting-api")


if __name__ == "__main__":
    unittest.main()
