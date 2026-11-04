"""Unit tests for mutation-check.py.

Run: `python3 -m unittest discover -s scripts -p 'test_*.py'`
(stdlib unittest, matching scripts/test_meeting_quality_xref.py — pytest is not a
dependency of this repo.)

A verifier with no tests of its own is the one thing this tool exists to prevent, so
the cases below are the ones where a wrong answer would manufacture false
confidence: a KILLED that should be ERROR, a SURVIVED that should be ERROR, an
unparseable run read as a result, and a mutation that silently did not apply.
"""

import contextlib
import importlib.util
import io
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
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

    def test_a_spec_may_set_timeout_s(self):
        spec = self._valid()
        spec["timeout_s"] = 5
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


TOOL = Path(__file__).with_name("mutation-check.py")


def _git_repo(d):
    root = Path(d)
    run = lambda *a: subprocess.run(a, cwd=root, check=True, capture_output=True, text=True)
    run("git", "init", "-q")
    run("git", "config", "user.email", "t@t")
    run("git", "config", "user.name", "t")
    return root, run


def _commit(root, run, files, msg):
    for name, content in files.items():
        (root / name).write_text(content if isinstance(content, str) else json.dumps(content))
    run("git", "add", "-A")
    run("git", "commit", "-qm", msg)
    return run("git", "rev-parse", "HEAD").stdout.strip()


def _mut(name, file, find, kills, replace="X"):
    return {"name": name, "file": file, "find": find, "replace": replace, "expect_kills": kills}


LIB = (
    "def untouched():\n    return 1\n\n\n"
    "def widened(x):\n" + "".join(f"    v{i} = x + {i}\n" for i in range(7)) + "    return v6\n"
)
TESTS = "def test_alpha():\n    assert True\n\n\ndef test_beta():\n    assert True\n"
CONF = "".join(f"key{i} = {i}\n" for i in range(1, 21))


class SelectOnlyTest(unittest.TestCase):
    MUTS = [_mut("alpha one", "f", "a", ["t"]), _mut("beta two", "f", "b", ["t"]),
            _mut("gamma", "f", "c", ["t"])]

    def test_repeated_only_runs_the_union(self):
        picked = mc.select({"mutations": self.MUTS}, Path("."), "", ["alpha", "BETA"], None)
        self.assertEqual([m["name"] for m, _ in picked], ["alpha one", "beta two"])

    def test_a_filter_matching_nothing_is_an_error_even_beside_one_that_matches(self):
        with self.assertRaises(SystemExit) as cm:
            mc.select({"mutations": self.MUTS}, Path("."), "", ["alpha", "zzz"], None)
        self.assertIn("ERROR", str(cm.exception))
        self.assertIn("zzz", str(cm.exception))

    def test_no_filter_selects_every_mutation(self):
        picked = mc.select({"mutations": self.MUTS}, Path("."), "", [], None)
        self.assertEqual(len(picked), 3)


class ChangedSinceTest(unittest.TestCase):
    def setUp(self):
        self._d = tempfile.TemporaryDirectory()
        self.root, self.run = _git_repo(self._d.name)
        self.spec = {"runner": "unittest", "command": ["python3", "-c", "open('RAN', 'w')"],
                     "baseline_tests": 1,
                     "mutations": [
                         _mut("untouched_def", "lib.py", "return 1", ["test_alpha"]),
                         _mut("widened_def", "lib.py", "return v6", ["test_alpha"]),
                         _mut("conf_near", "conf.txt", "key10 = 10", ["test_alpha"]),
                         _mut("conf_far", "conf.txt", "key2 = 2", ["test_alpha"]),
                         _mut("beta_named", "conf.txt", "key20 = 20", ["test_beta"]),
                         _mut("edited_entry", "conf.txt", "key19 = 19", ["test_alpha"]),
                     ]}
        self.base = _commit(self.root, self.run, {"lib.py": LIB, "test_lib.py": TESTS,
                                                  "conf.txt": CONF, "spec.json": self.spec},
                            "base")

    def tearDown(self):
        self._d.cleanup()

    def _change(self, **files):
        _commit(self.root, self.run, files, "fix round")

    def _picked(self):
        sel = mc.select_changed_since(self.spec, self.root, "spec.json", self.base)
        return {self.spec["mutations"][i]["name"]: why for i, why in sel.items()}

    def test_unchanged_tree_selects_nothing(self):
        self.assertEqual(self._picked(), {})

    def test_a_new_and_an_edited_spec_entry_are_selected(self):
        self.spec["mutations"][5]["replace"] = "Y"
        self.spec["mutations"].append(_mut("brand_new", "conf.txt", "key3 = 3", ["test_alpha"]))
        self._change(**{"spec.json": self.spec})
        picked = self._picked()
        self.assertEqual(set(picked), {"edited_entry", "brand_new"})
        self.assertIn("(a) spec entry changed", picked["edited_entry"])
        self.assertIn("(a) new spec entry", picked["brand_new"])

    def test_a_python_change_widens_to_the_whole_enclosing_function(self):
        self._change(**{"lib.py": LIB.replace("v0 = x + 0", "v0 = x - 0")})
        picked = self._picked()
        self.assertEqual(set(picked), {"widened_def"})
        self.assertTrue(any("def widened" in w for w in picked["widened_def"]))

    def test_a_non_python_change_selects_anchors_within_the_margin_only(self):
        self._change(**{"conf.txt": CONF.replace("key12 = 12", "key12 = 99")})
        picked = self._picked()
        self.assertEqual(set(picked), {"conf_near"})
        self.assertTrue(picked["conf_near"][0].startswith("(b)"))

    def test_a_pure_deletion_counts_as_a_change_beside_it(self):
        self._change(**{"conf.txt": CONF.replace("key11 = 11\n", "")})
        self.assertEqual(set(self._picked()), {"conf_near"})

    def test_a_vanished_anchor_is_selected_for_preflight_to_report(self):
        self._change(**{"conf.txt": CONF.replace("key2 = 2", "key2 = 7")})
        self.assertIn("(b) anchor appears 0x, spec expects 1x — preflight will report it",
                      self._picked()["conf_far"])

    def test_a_newly_duplicated_anchor_is_selected_for_preflight_to_report(self):
        self._change(**{"lib.py": LIB.replace("    return v6\n", "    return 1\n")})
        self.assertIn("(b) anchor appears 2x, spec expects 1x — preflight will report it",
                      self._picked()["untouched_def"])

    def test_a_deleted_named_test_selects_its_mutations(self):
        self._change(**{"test_lib.py": TESTS.split("\n\n\ndef test_beta")[0] + "\n"})
        picked = self._picked()
        self.assertEqual(set(picked), {"beta_named"})
        self.assertEqual(picked["beta_named"],
                         ["(c) test 'test_beta' changed or removed: was test_lib.py:5"])

    def test_a_skip_decorator_on_a_named_test_selects_it(self):
        self._change(**{"test_lib.py": TESTS.replace("def test_beta", "@unittest.skip('x')\ndef test_beta")})
        self.assertEqual(self._picked(), {"beta_named": ["(c) test 'test_beta' changed at test_lib.py:5"]})

    def test_a_skip_decorator_on_the_enclosing_class_selects_it(self):
        cls = "import unittest\n\n\nclass T(unittest.TestCase):\n    def test_gamma(self):\n        pass\n"
        self.spec["mutations"].append(_mut("gamma_named", "conf.txt", "key5 = 5", ["test_gamma"]))
        self.base = _commit(self.root, self.run, {"test_cls.py": cls, "spec.json": self.spec}, "cls")
        self._change(**{"test_cls.py": cls.replace("class T", "@unittest.skip('x')\nclass T")})
        self.assertEqual(self._picked(), {"gamma_named": ["(c) test 'test_gamma' changed at test_cls.py:4"]})

    def test_a_fall_in_baseline_tests_selects_every_entry(self):
        self.spec["baseline_tests"] = 0
        self._change(**{"spec.json": self.spec})
        picked = self._picked()
        self.assertEqual(len(picked), 6)
        self.assertEqual(picked["conf_far"], ["baseline_tests fell 1→0: a test may have dropped out "
                                              "of discovery — every entry selected"])

    def test_a_rise_in_baseline_tests_selects_nothing_extra(self):
        self.spec["baseline_tests"] = 2
        self._change(**{"spec.json": self.spec})
        self.assertEqual(self._picked(), {})

    def test_each_run_key_change_selects_every_entry(self):
        base_spec = json.loads(json.dumps(self.spec))
        for key, value in (("runner", "cargo"), ("cwd", "sub"), ("node", "20")):
            with self.subTest(key=key):
                self.spec = {**base_spec, key: value}
                self._change(**{"spec.json": self.spec})
                picked = self._picked()
                self.assertEqual(len(picked), 6)
                self.assertTrue(picked["conf_far"][0].startswith(f"spec `{key}` changed since"))

    def test_a_python_file_that_did_not_parse_at_rev_selects_every_entry(self):
        self.base = _commit(self.root, self.run, {"extra.py": "def x(:\n"}, "broken")
        self._change(**{"extra.py": "def x():\n    pass\n"})
        self.assertEqual(self._picked()["conf_far"],
                         [f"extra.py does not parse at {self.base[:8]} — every entry selected"])

    def test_a_change_exactly_the_margin_after_the_anchor_selects_it(self):
        self._change(**{"conf.txt": CONF.replace("key13 = 13", "key13 = 99")})
        self.assertIn("conf_near", self._picked())

    def test_a_changed_decorator_widens_to_the_decorated_def(self):
        lib2 = "@functools.lru_cache(maxsize=1)\ndef cached():\n" + "    a = 1\n" * 4 + "    return a\n"
        self.spec["mutations"].append(_mut("decorated", "lib2.py", "return a", ["test_alpha"]))
        self.base = _commit(self.root, self.run, {"lib2.py": lib2, "spec.json": self.spec}, "lib2")
        self._change(**{"lib2.py": lib2.replace("maxsize=1", "maxsize=2")})
        self.assertEqual(set(self._picked()), {"decorated"})

    def test_a_changed_run_command_selects_every_entry(self):
        self.spec["command"] = ["python3", "-m", "unittest"]
        self._change(**{"spec.json": self.spec})
        picked = self._picked()
        self.assertEqual(len(picked), 6)
        self.assertTrue(picked["conf_far"][0].startswith("spec `command` changed since"))

    def test_a_python_file_that_does_not_parse_selects_every_entry(self):
        self._change(**{"lib.py": LIB.replace("def untouched():", "def untouched(:")})
        picked = self._picked()
        self.assertEqual(len(picked), 6)
        self.assertEqual(picked["conf_far"], ["lib.py does not parse now — every entry selected"])

    def test_quoted_and_non_ascii_file_names_are_read(self):
        files = {"a file.py": "def f():\n    a = 1\n" + "    a += 1\n" * 6 + "    return a\n",
                 "l\u00edb.txt": CONF}
        self.spec["mutations"] += [_mut("spaced", "a file.py", "return a", ["test_alpha"]),
                                   _mut("accented", "l\u00edb.txt", "key10 = 10", ["test_alpha"])]
        self.base = _commit(self.root, self.run, {**files, "spec.json": self.spec}, "names")
        self._change(**{"a file.py": files["a file.py"].replace("a = 1", "a = 2"),
                        "l\u00edb.txt": CONF.replace("key12 = 12", "key12 = 99")})
        picked = self._picked()
        self.assertEqual(set(picked), {"spaced", "accented"})
        self.assertIn("(b) anchor at a file.py:9 is in changed def f", picked["spaced"])

    def test_a_change_in_a_nested_def_widens_to_the_outermost(self):
        text = "def outer():\n    a = 1\n\n    def inner():\n        return 2\n\n    return a\n"
        self.assertEqual(mc.changed_regions("m.py", text, {5}), [(1, 7, "def outer")])

    def test_an_edited_named_test_selects_its_mutations(self):
        self._change(**{"test_lib.py": TESTS.replace("def test_beta():\n    assert True",
                                                     "def test_beta():\n    assert 1")})
        picked = self._picked()
        self.assertEqual(set(picked), {"beta_named"})
        self.assertTrue(picked["beta_named"][0].startswith("(c) test 'test_beta'"))

    def test_an_unresolvable_rev_is_an_error(self):
        with self.assertRaises(SystemExit) as cm:
            mc.select_changed_since(self.spec, self.root, "spec.json", "no-such-rev")
        self.assertIn("does not resolve", str(cm.exception))

    def test_a_spec_missing_at_the_rev_is_an_error(self):
        with self.assertRaises(SystemExit) as cm:
            mc.select_changed_since(self.spec, self.root, "other.json", self.base)
        self.assertIn("cannot read the spec", str(cm.exception))

    def _cli(self, *args):
        return subprocess.run([sys.executable, str(TOOL), "run", "spec.json", *args],
                              cwd=self.root, capture_output=True, text=True)

    def test_list_prints_names_reasons_and_count_and_runs_nothing(self):
        self.spec["mutations"].append(_mut("brand_new", "conf.txt", "key3 = 3", ["test_alpha"]))
        self._change(**{"spec.json": self.spec,
                        "conf.txt": CONF.replace("key12 = 12", "key12 = 99")})
        out = self._cli("--changed-since", self.base, "--only", "conf_far", "--list")
        self.assertEqual(out.returncode, 0, out.stderr)
        for want in ("conf_near", "conf_far", "brand_new", "--only 'conf_far'",
                     "(a) new spec entry", "3/7 mutation(s) selected"):
            self.assertIn(want, out.stdout)
        self.assertNotIn("untouched_def", out.stdout)
        self.assertFalse((self.root / "RAN").exists())

    def test_repeated_only_on_the_cli_lists_the_union(self):
        out = self._cli("--only", "conf_near", "--only", "conf_far", "--list")
        self.assertIn("2/6 mutation(s) selected", out.stdout)

    def test_an_empty_targeted_set_runs_nothing_and_passes(self):
        out = self._cli("--changed-since", "HEAD")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("0 mutation(s) selected", out.stdout)


class CargoAttributeTest(unittest.TestCase):
    def test_removing_a_test_attribute_with_the_body_unchanged_selects_it(self):
        self._drop_attribute("#[test]\n")

    def test_removing_a_test_attribute_above_a_blank_line_selects_it(self):
        self._drop_attribute("#[test]\n\n")

    def _drop_attribute(self, attr):
        with tempfile.TemporaryDirectory() as d:
            root, run = _git_repo(d)
            (root / "src").mkdir()
            tests = "".join(f"{attr}fn test_{n}() {{\n    assert!(true);\n}}\n\n" for n in ("keep", "guard"))
            spec = {"runner": "cargo", "command": ["cargo", "test"], "baseline_tests": 2,
                    "mutations": [_mut("guard", "src/lib.rs", "1 }", ["tests::test_guard"])]}
            base = _commit(root, run, {"src/lib.rs": "pub fn f() -> i32 { 1 }\n" + "\n" * 30,
                                       "src/tests.rs": tests, "spec.json": spec}, "base")
            _commit(root, run, {"src/tests.rs": tests.replace(attr + "fn test_guard", "fn test_guard")},
                    "drop attribute")
            picked = mc.select_changed_since(spec, root, "spec.json", base)
            self.assertEqual(list(picked), [0])
            self.assertTrue(picked[0][0].startswith("(c) test 'tests::test_guard' changed or removed"))


class TestSpansTest(unittest.TestCase):
    def test_cargo_maps_the_last_path_segment_to_the_fn_body(self):
        text = "mod tests {\n    #[test]\n    fn the_pin() {\n        let a = { 1 };\n" \
               "        assert_eq!(a, 1);\n    }\n\n    fn other() {}\n}\n"
        self.assertEqual(mc.test_spans("cargo", "x.rs", text, "x::tests::the_pin"), [(2, 6)])

    def test_a_brace_inside_a_string_does_not_end_the_span(self):
        text = 'fn the_pin() {\n    let s = "}";\n    // }\n    run(s);\n}\n'
        self.assertEqual(mc.test_spans("cargo", "x.rs", text, "the_pin"), [(1, 5)])

    def test_cargo_span_reaches_comment_lines_above_the_fn(self):
        text = "#[test]\n// why\nfn the_pin() {\n}\n"
        self.assertEqual(mc.test_spans("cargo", "x.rs", text, "the_pin"), [(1, 4)])

    def test_cargo_fn_without_a_body_gets_the_margin(self):
        self.assertEqual(mc.test_spans("cargo", "x.rs", "fn the_pin();\n", "the_pin"),
                         [(1, 1 + mc.CONTEXT_MARGIN)])

    def test_vitest_modifiers_on_the_title_line_still_match(self):
        text = "test.concurrent('does the thing', async () => {\n  expect(1).toBe(1);\n});\n"
        self.assertEqual(mc.test_spans("vitest", "x.test.ts", text, "does the thing"), [(1, 3)])

    def test_unittest_span_covers_a_multi_line_class_header(self):
        text = "class T(\n    unittest.TestCase,\n):\n    def test_gamma(self):\n        pass\n"
        self.assertEqual(mc.test_spans("unittest", "t.py", text, "test_gamma"), [(4, 5), (1, 3)])

    def test_a_multi_line_anchor_spans_all_its_lines(self):
        text = "a\nb\nc\nd\n"
        self.assertEqual(mc.anchor_span(text, "b\nc"), (2, 3))
        self.assertEqual(mc.anchor_span(text, "b\nc\n"), (2, 3))

    def test_vitest_maps_the_last_title_segment_to_the_call_body(self):
        text = "describe('suite', () => {\n  it('does the thing', () => {\n" \
               "    expect(1).toBe(1);\n  });\n});\n"
        self.assertEqual(mc.test_spans("vitest", "x.test.ts", text, "suite > does the thing"),
                         [(2, 4)])


class ProcessGroupTest(unittest.TestCase):
    def _run_and_reap(self, d, script, fail_with=None, timeout_s=60, gate=False):
        pidfile = Path(d) / "pid"
        script = script.format(f"echo $! > {pidfile}.tmp && mv {pidfile}.tmp {pidfile}")
        runner = mc.Runner({"runner": "unittest", "command": ["bash", "-c", script],
                            "timeout_s": timeout_s}, Path(d), False)
        real_wait = subprocess.Popen.wait

        def once_ready(proc, timeout=None):
            if timeout is None:
                return real_wait(proc)
            deadline = time.monotonic() + 10
            while not pidfile.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            if fail_with is not None:
                raise fail_with
            return real_wait(proc, timeout)

        started = time.monotonic()
        try:
            if fail_with is None and not gate:
                return runner.run()
            with mock.patch.object(subprocess.Popen, "wait", once_ready):
                return runner.run()
        finally:
            self.assertLess(time.monotonic() - started, 15, "run() waited on the grandchild")
            pid = int(pidfile.read_text())
            try:
                deadline = time.monotonic() + 5
                while _alive(pid) and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertFalse(_alive(pid), "the suite's grandchild outlived run()")
            finally:
                if _alive(pid):
                    os.kill(pid, signal.SIGKILL)

    def test_a_timed_out_suite_leaves_no_grandchild_running(self):
        with tempfile.TemporaryDirectory() as d:
            passed, _, err = self._run_and_reap(d, "sleep 30 & {}; wait", timeout_s=0.3, gate=True)
        self.assertIsNone(passed)
        self.assertIn("timeout", err)

    def test_an_interrupted_suite_leaves_no_grandchild_running(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(KeyboardInterrupt):
                self._run_and_reap(d, "sleep 30 & {}; wait", KeyboardInterrupt())

    def test_a_finished_suite_leaves_no_grandchild_running(self):
        with tempfile.TemporaryDirectory() as d:
            self._run_and_reap(d, "sleep 30 >/dev/null 2>&1 & {}; echo 'Ran 1 test in 0s'")

    def test_suite_stderr_reaches_the_error_and_verbose_output(self):
        runner = mc.Runner({"runner": "unittest", "command": ["bash", "-c", "echo boom-on-stderr >&2"]},
                           Path("."), True)
        shown = io.StringIO()
        with contextlib.redirect_stdout(shown):
            passed, _, err = runner.run()
        self.assertIsNone(passed)
        self.assertIn("boom-on-stderr", err)
        self.assertIn("boom-on-stderr", shown.getvalue())

    def test_a_grandchild_holding_stdout_does_not_hold_a_finished_suite(self):
        with tempfile.TemporaryDirectory() as d:
            passed, _, err = self._run_and_reap(d, "sleep 30 & {}; echo 'Ran 1 test in 0s'")
        self.assertEqual(passed, 1, err)


class SignalBoundaryTest(unittest.TestCase):
    """A real signal delivered at each cleanup boundary, in process."""

    SIGS = (signal.SIGTERM, signal.SIGHUP)

    def setUp(self):
        self._old = {sig: signal.signal(sig, mc._exit_on_signal) for sig in self.SIGS}
        if hasattr(mc, "_PENDING"):
            mc._PENDING.clear()

    def tearDown(self):
        for sig, handler in self._old.items():
            signal.signal(sig, handler)
        if hasattr(mc, "_PENDING"):
            mc._PENDING.clear()

    def _grandchild_runner(self, d):
        pidfile = Path(d) / "pid"
        script = f"sleep 30 & echo $! > {pidfile}.tmp && mv {pidfile}.tmp {pidfile}; wait"
        return pidfile, mc.Runner({"runner": "unittest", "command": ["bash", "-c", script],
                                   "timeout_s": 10}, Path(d), False)

    def _assert_reaped(self, pidfile):
        pid = int(pidfile.read_text())
        try:
            deadline = time.monotonic() + 5
            while _alive(pid) and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertFalse(_alive(pid), "the suite's grandchild outlived the signal")
        finally:
            if _alive(pid):
                os.kill(pid, signal.SIGKILL)

    @staticmethod
    def _await(pidfile):
        deadline = time.monotonic() + 10
        while not pidfile.exists() and time.monotonic() < deadline:
            time.sleep(0.01)

    def test_a_signal_between_popen_and_the_try_still_kills_the_group(self):
        real_popen = subprocess.Popen
        with tempfile.TemporaryDirectory() as d:
            pidfile, runner = self._grandchild_runner(d)

            def popen_then_signal(*a, **k):
                proc = real_popen(*a, **k)
                self._await(pidfile)
                os.kill(os.getpid(), signal.SIGTERM)
                return proc

            started = time.monotonic()
            with self.assertRaises(SystemExit) as cm:
                with mock.patch.object(subprocess, "Popen", popen_then_signal):
                    runner.run()
            self.assertLess(time.monotonic() - started, 5, "the pending signal waited for timeout_s")
            self._assert_reaped(pidfile)
            self.assertEqual(cm.exception.code, 128 + signal.SIGTERM)

    def test_a_second_signal_during_the_group_kill_does_not_abort_it(self):
        real_wait, real_killpg = subprocess.Popen.wait, os.killpg
        with tempfile.TemporaryDirectory() as d:
            pidfile, runner = self._grandchild_runner(d)

            def wait_then_signal(proc, timeout=None):
                if timeout is not None:
                    self._await(pidfile)
                    os.kill(os.getpid(), signal.SIGTERM)
                return real_wait(proc, timeout)

            def killpg_after_sighup(pgid, sig):
                os.kill(os.getpid(), signal.SIGHUP)
                return real_killpg(pgid, sig)

            with self.assertRaises(SystemExit) as cm:
                with mock.patch.object(subprocess.Popen, "wait", wait_then_signal), \
                        mock.patch.object(os, "killpg", killpg_after_sighup):
                    runner.run()
            self._assert_reaped(pidfile)
            self.assertEqual(cm.exception.code, 128 + signal.SIGTERM)

    def test_a_signal_during_the_restore_does_not_leave_the_mutation(self):
        real_run = subprocess.run
        with tempfile.TemporaryDirectory() as d:
            root, run = _git_repo(d)
            spec = {"runner": "unittest", "command": ["bash", "-c", "echo 'Ran 1 test in 0.0s'"],
                    "baseline_tests": 1,
                    "mutations": [_mut("m1", "target.txt", "ORIGINAL", ["x"], replace="MUTATED"),
                                  _mut("m2", "other.txt", "ORIGINAL", ["x"], replace="MUTATED")]}
            _commit(root, run, {"target.txt": "ORIGINAL\n", "other.txt": "ORIGINAL\n",
                                "spec.json": spec}, "base")

            def signal_before_checkout(args, *a, **k):
                if list(args[:2]) == ["git", "checkout"]:
                    os.kill(os.getpid(), signal.SIGTERM)
                return real_run(args, *a, **k)

            shown = io.StringIO()
            with self.assertRaises(SystemExit) as cm, contextlib.redirect_stdout(shown):
                with mock.patch.object(subprocess, "run", signal_before_checkout):
                    mc.cmd_run(spec, root, spec["mutations"], False)
            self.assertEqual(cm.exception.code, 128 + signal.SIGTERM)
            self.assertEqual((root / "target.txt").read_text(), "ORIGINAL\n")
            self.assertNotIn("m2", shown.getvalue(), "a mutation started after the signal")

    def test_two_pending_signals_exit_with_the_first(self):
        with self.assertRaises(SystemExit) as cm:
            with mc.deferred_signals():
                os.kill(os.getpid(), signal.SIGTERM)
                os.kill(os.getpid(), signal.SIGHUP)
        self.assertEqual(cm.exception.code, 128 + signal.SIGTERM)

    def test_with_nothing_to_clean_up_a_signal_exits_at_once(self):
        with self.assertRaises(SystemExit) as cm:
            mc._exit_on_signal(signal.SIGHUP, None)
        self.assertEqual(cm.exception.code, 128 + signal.SIGHUP)


class SignalTest(unittest.TestCase):
    """SIGTERM, SIGHUP or `timeout` mid-mutation: suite group killed, file restored."""

    def _signal_mid_mutation(self, sig, via_timeout=False):
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as aux:
            root, run = _git_repo(d)
            pidfile = Path(aux) / "pid"
            script = (f"if grep -q MUTATED target.txt; then sleep 30 & echo $! > {pidfile}.tmp && "
                      f"mv {pidfile}.tmp {pidfile}; wait; else echo 'Ran 1 test in 0.0s'; fi")
            spec = {"runner": "unittest", "command": ["bash", "-c", script], "baseline_tests": 1,
                    "mutations": [_mut("m", "target.txt", "ORIGINAL", ["x"], replace="MUTATED")]}
            _commit(root, run, {"target.txt": "ORIGINAL\n", "spec.json": spec}, "base")
            cmd = ["timeout", "60"] if via_timeout else []
            proc = subprocess.Popen(cmd + [sys.executable, str(TOOL), "run", "spec.json"], cwd=root,
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 10
                while not pidfile.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(pidfile.exists(), "the mutation run never started")
                proc.send_signal(sig)
                self.assertEqual(proc.wait(timeout=15), 128 + signal.SIGTERM if via_timeout else 128 + sig)
                pid = int(pidfile.read_text())
                deadline = time.monotonic() + 5
                while _alive(pid) and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertEqual({"grandchild alive": _alive(pid),
                                  "target.txt": (root / "target.txt").read_text()},
                                 {"grandchild alive": False, "target.txt": "ORIGINAL\n"})
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()
                if pidfile.exists() and _alive(int(pidfile.read_text())):
                    os.kill(int(pidfile.read_text()), signal.SIGKILL)

    def test_sigterm_kills_the_suite_and_restores_the_file(self):
        self._signal_mid_mutation(signal.SIGTERM)

    def test_sighup_kills_the_suite_and_restores_the_file(self):
        self._signal_mid_mutation(signal.SIGHUP)

    def test_sigint_kills_the_suite_and_restores_the_file(self):
        self._signal_mid_mutation(signal.SIGINT)

    @unittest.skipUnless(shutil.which("timeout"), "needs GNU timeout")
    def test_gnu_timeout_kills_the_suite_and_restores_the_file(self):
        self._signal_mid_mutation(signal.SIGTERM, via_timeout=True)


def _alive(pid):
    out = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True)
    return out.returncode == 0 and not out.stdout.strip().startswith("Z")


if __name__ == "__main__":
    unittest.main()
