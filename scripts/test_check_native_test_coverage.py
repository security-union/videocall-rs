#!/usr/bin/env python3
"""Regression tests for check_native_test_coverage.py (issue #2446).

A guard that cannot fail is worse than none, so these drive the real module --
not a re-implementation of its rules -- over synthetic crate trees.
"""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "check_native_test_coverage",
    Path(__file__).resolve().with_name("check_native_test_coverage.py"),
)
guard = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(guard)


def write_crate(root: Path, body: str) -> Path:
    src = root / "src"
    src.mkdir(parents=True, exist_ok=True)
    (src / "lib.rs").write_text(body, encoding="utf-8")
    return src


def cargo_list(*names: str) -> str:
    lines = [f"tests::{name}: test" for name in names]
    lines += ["", f"{len(names)} tests, 0 benchmarks"]
    return "\n".join(lines)


NATIVE_ONLY = """
#[cfg(test)]
mod tests {
    #[test]
    fn alpha() {}

    #[test]
    fn beta() {}
}
"""

ONE_WASM_GATED = """
#[cfg(test)]
mod tests {
    #[test]
    fn alpha() {}

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn beta_never_runs() {}
}
"""

COMMENT_MENTIONS_TEST_ATTR = """
#[cfg(test)]
mod tests {
    // A plain `#[test]` here would be compiled out; see #2446.
    /// Prose mentioning #[test] must not be counted.
    #[test]
    fn alpha() {}
}
"""

INTERVENING_ATTRIBUTE = """
#[cfg(test)]
mod tests {
    #[test]
    #[should_panic]
    fn alpha() {}
}
"""


class CheckNativeTestCoverage(unittest.TestCase):
    def run_guard(self, body: str, *listed: str):
        with tempfile.TemporaryDirectory() as tmp:
            src = write_crate(Path(tmp), body)
            return guard.check(str(src), cargo_list(*listed))

    def test_matched_pair_passes(self):
        code, report = self.run_guard(NATIVE_ONLY, "alpha", "beta")
        self.assertEqual(code, 0, "\n".join(report))
        self.assertIn("OK", "\n".join(report))

    def test_wasm_gated_test_fails_and_is_named(self):
        code, report = self.run_guard(ONE_WASM_GATED, "alpha")
        text = "\n".join(report)
        self.assertEqual(code, 1, text)
        self.assertIn("beta_never_runs", text)
        self.assertIn("2446", text)

    def test_comment_mentioning_test_attribute_is_not_counted(self):
        # If prose were counted the guard would fail on a clean tree and get muted.
        code, report = self.run_guard(COMMENT_MENTIONS_TEST_ATTR, "alpha")
        self.assertEqual(code, 0, "\n".join(report))

    def test_blind_source_scan_is_an_error_not_a_pass(self):
        code, report = self.run_guard(NATIVE_ONLY, "alpha", "beta", "gamma")
        text = "\n".join(report)
        self.assertEqual(code, 2, text)
        self.assertIn("gone blind", text)

    def test_source_scan_finds_fn_name_below_intervening_attributes(self):
        with tempfile.TemporaryDirectory() as tmp:
            src = write_crate(Path(tmp), INTERVENING_ATTRIBUTE)
            found = guard.source_tests(str(src))
        self.assertEqual([entry[2] for entry in found], ["alpha"])

    def test_listed_tests_ignores_the_trailing_summary_line(self):
        self.assertEqual(guard.listed_tests(cargo_list("alpha", "beta")), ["alpha", "beta"])


if __name__ == "__main__":
    unittest.main()
