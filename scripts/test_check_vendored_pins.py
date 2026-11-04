#!/usr/bin/env python3
"""Regression tests for check-vendored-pins.sh, run against temporary vendor
trees through CHECK_VENDORED_PINS_ROOT."""
import os
import subprocess
import tempfile
import unittest

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                      "check-vendored-pins.sh")

PACKAGE = '[package]\nname = "alpha"\nversion = "1.2.3"\nedition = "2021"\n'
PINS = '[package]\nname = "vendored-advisories"\n\n[dependencies]\nalpha = "{pin}"\n\n[workspace]\n'


def write(root, rel, text):
    path = os.path.join(root, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(text)


def run(root):
    env = dict(os.environ, CHECK_VENDORED_PINS_ROOT=root)
    return subprocess.run(["bash", SCRIPT], env=env, capture_output=True, text=True)


class CheckVendoredPins(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        self.addCleanup(self.tmp.cleanup)

    def test_matching_pin_passes(self):
        write(self.root, "vendor/alpha/Cargo.toml", PACKAGE)
        write(self.root, "vendor/advisories/Cargo.toml", PINS.format(pin="=1.2.3"))
        r = run(self.root)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("OK: vendor/alpha 1.2.3", r.stdout)

    def test_mismatched_pin_fails(self):
        write(self.root, "vendor/alpha/Cargo.toml", PACKAGE)
        write(self.root, "vendor/advisories/Cargo.toml", PINS.format(pin="=1.2.4"))
        r = run(self.root)
        self.assertEqual(r.returncode, 1)
        self.assertIn("pins '=1.2.4'", r.stderr)

    def test_dependency_version_is_not_the_package_version(self):
        # A [package] without `version` must not borrow one from a later table.
        write(self.root, "vendor/alpha/Cargo.toml",
              '[package]\nname = "alpha"\n\n[dependencies.x]\nversion = "1.2.3"\n')
        write(self.root, "vendor/advisories/Cargo.toml", PINS.format(pin="=1.2.3"))
        r = run(self.root)
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("no [package] version", r.stderr)

    def test_empty_vendor_dir_exits_2(self):
        write(self.root, "vendor/advisories/Cargo.toml", PINS.format(pin="=1.2.3"))
        r = run(self.root)
        self.assertEqual(r.returncode, 2)
        self.assertIn("refusing to pass vacuously", r.stderr)


if __name__ == "__main__":
    unittest.main()
