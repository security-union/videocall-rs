"""Unit tests for mutation-check.py.

Run: `python3 -m unittest discover -s scripts -p 'test_*.py'`
(stdlib unittest, matching scripts/test_meeting_quality_xref.py — pytest is not a
dependency of this repo.)

A verifier with no tests of its own is the one thing this tool exists to prevent, so
the cases below are the ones where a wrong answer would manufacture false
confidence: a KILLED that should be ERROR, a SURVIVED that should be ERROR, an
unparseable run read as a result, and a mutation that silently did not apply.
"""

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

# Hyphenated filename, so import by path rather than by module name.
_spec = importlib.util.spec_from_file_location(
    "mutation_check", Path(__file__).with_name("mutation-check.py")
)
mc = importlib.util.module_from_spec(_spec)
# Register before exec: @dataclass resolves `cls.__module__` through sys.modules, so a
# path-loaded module that skips this raises AttributeError on Python 3.10.
sys.modules["mutation_check"] = mc
_spec.loader.exec_module(mc)


class ClassifyTest(unittest.TestCase):
    """`classify` is the step most able to produce a false KILLED."""

    def test_all_expected_failures_present_is_killed(self):
        self.assertEqual(mc.classify(["wants_a"], ["test_wants_a_thing"])[0], mc.KILLED)

    def test_nothing_failed_is_survived(self):
        self.assertEqual(mc.classify(["wants_a"], [])[0], mc.SURVIVED)

    def test_wrong_test_died_is_error_not_killed(self):
        # The mutation broke something, but NOT what the spec named. Reporting this
        # as KILLED would credit the test with a guard it does not provide.
        verdict, detail = mc.classify(["wants_a"], ["some_unrelated_test"])
        self.assertEqual(verdict, mc.ERROR)
        self.assertIn("not the named one", detail)

    def test_partial_match_is_error(self):
        verdict, _ = mc.classify(["wants_a", "wants_b"], ["test_wants_a"])
        self.assertEqual(verdict, mc.ERROR)

    def test_extra_failures_are_surfaced_not_hidden(self):
        # Still KILLED (the named test did die), but a mutation that also broke four
        # other tests is probably broader than intended and must not be silently green.
        verdict, detail = mc.classify(["wants_a"], ["test_wants_a", "collateral_one"])
        self.assertEqual(verdict, mc.KILLED)
        self.assertIn("collateral_one", detail)

    def test_empty_expect_kills_is_error_never_killed(self):
        # A spec that asserts nothing must not be able to report success.
        verdict, detail = mc.classify([], ["anything"])
        self.assertEqual(verdict, mc.ERROR)
        self.assertIn("no expect_kills", detail)

    def test_substring_matching_is_case_insensitive(self):
        self.assertEqual(mc.classify(["WANTS_A"], ["test_wants_a"])[0], mc.KILLED)


class ParseVitestTest(unittest.TestCase):
    def test_all_passed(self):
        passed, failed = mc.Runner._parse_vitest("      Tests  931 passed (931)\n")
        self.assertEqual((passed, failed), (931, []))

    def test_some_failed_reports_names(self):
        out = "     × defaults the cap 12ms\n      Tests  1 failed | 930 passed (931)\n"
        passed, failed = mc.Runner._parse_vitest(out)
        self.assertEqual(passed, 930)
        self.assertEqual(failed, ["defaults the cap"])

    def test_no_summary_returns_none(self):
        # The failure that once turned "never ran" into "nothing was killed".
        self.assertEqual(mc.Runner._parse_vitest("bash: npx: command not found")[0], None)

    def test_zero_tests_is_a_result_not_a_failure_to_parse(self):
        passed, _ = mc.Runner._parse_vitest("      Tests  0 passed (0)\n")
        self.assertEqual(passed, 0)


class ParseCargoTest(unittest.TestCase):
    def test_sums_multiple_targets(self):
        out = (
            "test result: ok. 117 passed; 0 failed; 0 ignored\n"
            "test result: ok. 5 passed; 0 failed; 0 ignored\n"
        )
        self.assertEqual(mc.Runner._parse_cargo(out)[0], 122)

    def test_failed_names_extracted(self):
        out = (
            "test inbound_stats::tests::a_pin ... FAILED\n"
            "test result: FAILED. 116 passed; 1 failed; 0 ignored\n"
        )
        passed, failed = mc.Runner._parse_cargo(out)
        self.assertEqual(passed, 116)
        self.assertEqual(failed, ["inbound_stats::tests::a_pin"])

    def test_compile_error_returns_none(self):
        out = "error[E0308]: mismatched types\nerror: could not compile `bot`"
        self.assertEqual(mc.Runner._parse_cargo(out)[0], None)


class ApplyMutationTest(unittest.TestCase):
    def setUp(self):
        fd, self.path = tempfile.mkstemp()
        os.close(fd)
        self.p = Path(self.path)

    def tearDown(self):
        self.p.unlink(missing_ok=True)

    def test_applies_and_changes_the_file(self):
        self.p.write_text('value: "10"\n')
        mc.apply_mutation(self.p, 'value: "10"', 'value: "6"', 1)
        self.assertEqual(self.p.read_text(), 'value: "6"\n')

    def test_missing_anchor_raises(self):
        self.p.write_text("nothing here\n")
        with self.assertRaises(RuntimeError) as cm:
            mc.apply_mutation(self.p, "absent", "x", 1)
        self.assertIn("0x", str(cm.exception))

    def test_ambiguous_anchor_raises_rather_than_guessing(self):
        # Mutating the wrong occurrence is a silent no-op for the assertion.
        self.p.write_text("dup\ndup\n")
        with self.assertRaises(RuntimeError) as cm:
            mc.apply_mutation(self.p, "dup", "x", 1)
        self.assertIn("refusing to guess", str(cm.exception))

    def test_noop_replacement_raises(self):
        self.p.write_text("same\n")
        with self.assertRaises(RuntimeError) as cm:
            mc.apply_mutation(self.p, "same", "same", 1)
        self.assertIn("no-op", str(cm.exception))


class ValidateSpecTest(unittest.TestCase):
    """A typo'd key must fail loudly, not silently drop the assertion."""

    def _valid(self):
        return {
            "runner": "vitest",
            "command": ["true"],
            "baseline_tests": 1,
            "mutations": [{"name": "n", "file": "f", "find": "a", "replace": "b",
                           "expect_kills": ["t"]}],
        }

    def test_valid_spec_passes(self):
        mc.validate_spec(self._valid(), "run")  # must not raise

    def test_typod_mutation_key_is_rejected(self):
        spec = self._valid()
        spec["mutations"][0]["expect_kill"] = ["t"]   # singular typo
        with self.assertRaises(SystemExit) as cm:
            mc.validate_spec(spec, "run")
        self.assertIn("expect_kill", str(cm.exception))

    def test_typod_occurrences_key_is_rejected(self):
        spec = self._valid()
        spec["mutations"][0]["occurences"] = 2        # misspelling
        with self.assertRaises(SystemExit):
            mc.validate_spec(spec, "run")

    def test_missing_required_key_is_rejected(self):
        spec = self._valid()
        del spec["baseline_tests"]
        with self.assertRaises(SystemExit):
            mc.validate_spec(spec, "run")

    def test_claim_needs_present_or_absent(self):
        with self.assertRaises(SystemExit):
            mc.validate_spec({"committed_claims": [{"file": "f"}]}, "verify")


class InvalidatePycacheTest(unittest.TestCase):
    """Guards the non-deterministic wrong-verdict path: stale bytecode after a restore."""

    def test_removes_cached_bytecode_for_the_mutated_module(self):
        with tempfile.TemporaryDirectory() as d:
            src = Path(d) / "victim.py"
            src.write_text("x = 1\n")
            cache = Path(d) / "__pycache__"
            cache.mkdir()
            stale = cache / "victim.cpython-310.pyc"
            stale.write_bytes(b"stale")
            mc.invalidate_pycache(src)
            self.assertFalse(stale.exists(), "a stale .pyc must not survive a mutation")

    def test_ignores_non_python_files(self):
        with tempfile.TemporaryDirectory() as d:
            yaml = Path(d) / "manifest.yaml"
            yaml.write_text("a: 1\n")
            mc.invalidate_pycache(yaml)  # must not raise

    def test_tolerates_a_missing_pycache_dir(self):
        with tempfile.TemporaryDirectory() as d:
            src = Path(d) / "victim.py"
            src.write_text("x = 1\n")
            mc.invalidate_pycache(src)  # must not raise


class PreflightTest(unittest.TestCase):
    """Anchors must be checked BEFORE any suite run, not when their turn arrives."""

    def _tree(self, d, content='value: "10"\n'):
        root = Path(d)
        (root / "target.yaml").write_text(content)
        return root

    def _mut(self, **over):
        m = {"name": "m", "file": "target.yaml", "find": 'value: "10"',
             "replace": 'value: "6"', "expect_kills": ["t"]}
        m.update(over)
        return m

    def test_valid_spec_passes(self):
        with tempfile.TemporaryDirectory() as d:
            mc.preflight(self._tree(d), [self._mut()])  # must not raise

    def test_stale_anchor_fails_before_running_anything(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(SystemExit) as cm:
                mc.preflight(self._tree(d), [self._mut(find="moved away")])
            self.assertIn("code moved?", str(cm.exception))

    def test_ambiguous_anchor_fails(self):
        with tempfile.TemporaryDirectory() as d:
            root = self._tree(d, 'dup\ndup\n')
            with self.assertRaises(SystemExit) as cm:
                mc.preflight(root, [self._mut(find="dup", replace="x")])
            self.assertIn("ambiguous", str(cm.exception))

    def test_missing_file_is_reported_not_raised_as_traceback(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(SystemExit) as cm:
                mc.preflight(self._tree(d), [self._mut(file="gone.yaml")])
            self.assertIn("does not exist", str(cm.exception))

    def test_noop_mutation_fails(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(SystemExit) as cm:
                mc.preflight(self._tree(d), [self._mut(replace='value: "10"')])
            self.assertIn("no-op", str(cm.exception))

    def test_missing_expect_kills_is_rejected_by_the_schema(self):
        # classify() treats an empty expect_kills as an unconditional ERROR, so a spec
        # missing it can NEVER pass — it must be refused up front, not after N runs.
        spec = {"runner": "vitest", "command": ["true"], "baseline_tests": 1,
                "mutations": [{"name": "n", "file": "f", "find": "a", "replace": "b"}]}
        with self.assertRaises(SystemExit) as cm:
            mc.validate_spec(spec, "run")
        self.assertIn("expect_kills", str(cm.exception))


class DirtyTreeGuardTest(unittest.TestCase):
    """The guard whose absence let a `git checkout --` discard a shipped fix."""

    def test_refuses_when_a_target_file_is_dirty(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            run = lambda *a: subprocess.run(a, cwd=root, check=True, capture_output=True)
            run("git", "init", "-q")
            run("git", "config", "user.email", "t@t")
            run("git", "config", "user.name", "t")
            (root / "target.txt").write_text("committed\n")
            run("git", "add", "target.txt")
            run("git", "commit", "-qm", "init")
            (root / "target.txt").write_text("UNCOMMITTED WORK\n")
            with self.assertRaises(SystemExit) as cm:
                mc.assert_clean(root, ["target.txt"])
            self.assertIn("REFUSING TO RUN", str(cm.exception))

    def test_allows_a_clean_target(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            run = lambda *a: subprocess.run(a, cwd=root, check=True, capture_output=True)
            run("git", "init", "-q")
            run("git", "config", "user.email", "t@t")
            run("git", "config", "user.name", "t")
            (root / "target.txt").write_text("committed\n")
            run("git", "add", "target.txt")
            run("git", "commit", "-qm", "init")
            mc.assert_clean(root, ["target.txt"])  # must not raise


if __name__ == "__main__":
    unittest.main()
