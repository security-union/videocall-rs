#!/usr/bin/env python3
"""Regression tests for check_dashboard_configmap_keys.py (#2985)."""
from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("check_dashboard_configmap_keys.py")
ROOT = SCRIPT.parent.parent


def entry(key, source=None):
    return f'  {key}: |-\n    {{{{ .Files.Get "dashboards/{source or key}" | nindent 4 }}}}\n'


class Guard(unittest.TestCase):
    def tree(self, files, entries):
        tmp = Path(tempfile.mkdtemp(prefix="cm-keys-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        (tmp / "helm/grafana/dashboards").mkdir(parents=True)
        (tmp / "helm/grafana/templates").mkdir(parents=True)
        for name in files:
            (tmp / "helm/grafana/dashboards" / name).write_text("{}")
        (tmp / "helm/grafana/templates/dashboards-configmap.yaml").write_text(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  labels:\n    grafana_dashboard: \"1\"\ndata:\n"
            + "".join(entries))
        return tmp

    def run_guard(self, root):
        return subprocess.run([sys.executable, str(SCRIPT), "--root", str(root)], capture_output=True, text=True)

    def assert_flags(self, root, needle):
        res = self.run_guard(root)
        self.assertEqual(res.returncode, 1, res.stdout + res.stderr)
        self.assertIn(needle, res.stdout)

    def test_every_dashboard_shipped_passes(self):
        root = self.tree(["a.json", "b.json", "e2e-scoreboard.json"], [entry("a.json"), entry("b.json")])
        res = self.run_guard(root)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)

    def test_an_unlisted_dashboard_fails(self):
        self.assert_flags(self.tree(["a.json", "new.json"], [entry("a.json")]), "new.json: no `new.json: |-` key")

    def test_a_key_reading_another_file_fails(self):
        self.assert_flags(self.tree(["a.json", "b.json"], [entry("a.json"), entry("b.json", "a.json")]),
                          "key b.json reads 'a.json'")

    def test_a_key_without_files_get_fails(self):
        self.assert_flags(self.tree(["a.json"], ["  a.json: |-\n    {}\n"]), "key a.json reads None")

    def test_a_key_for_a_missing_file_fails(self):
        self.assert_flags(self.tree(["a.json"], [entry("a.json"), entry("gone.json")]),
                          "key gone.json reads a file that does not exist")

    def test_an_exempt_dashboard_shipped_through_the_configmap_fails(self):
        self.assert_flags(self.tree(["e2e-scoreboard.json"], [entry("e2e-scoreboard.json")]),
                          "e2e-scoreboard.json is exempt")

    def test_no_dashboards_is_a_usage_error(self):
        res = self.run_guard(self.tree([], []))
        self.assertEqual(res.returncode, 2, res.stdout + res.stderr)

    def test_the_real_tree_passes(self):
        res = self.run_guard(ROOT)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=1)
