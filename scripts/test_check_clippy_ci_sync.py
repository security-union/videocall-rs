#!/usr/bin/env python3
"""Regression tests for check-clippy-ci-sync.sh, run against a synthetic
workspace. The predicate keys off test attributes and tests/ directories; it
does not read explicit [[test]] targets."""
import os
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                      "check-clippy-ci-sync.sh")

STEP = "cargo clippy -p {} --tests -- -D warnings"

# alpha and beta carry test code and get steps; gamma carries none and gets no
# step, so every per-case file added to gamma decides the verdict on its own.
CRATES = {
    "alpha": "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
    "beta": '#[cfg(all(test, feature = "x"))]\nmod tests {\n    #[test]\n    fn t() {}\n}\n',
    "gamma": "#[derive(Clone)]\npub struct Args;\n\npub const N: u8 = 0;\n",
}

WORKFLOW = """name: Rust Check (HCL)
on:
  pull_request:
    branches: [PR-staging]
jobs:
  fmt:
    steps:
      - run: cargo fmt --all -- --check
  clippy:
    steps:
{steps}
  wasm-check:
    steps:
      - run: cargo check
"""


def yaml_steps(steps):
    """`run:` must sit alone on its line, as in the real workflow — the
    extractor strips `^\\s*run:\\s*`, not a `- run:` list marker."""
    return "".join(f"      - name: step {i}\n        run: {s}\n"
                   for i, s in enumerate(steps))


def write(root, rel, body):
    path = os.path.join(root, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(body)


def workspace(steps, extra_files=(), untracked=()):
    """A git-initialised workspace whose clippy-ci recipe and workflow clippy
    job both carry `steps`. Returns its path; caller removes it."""
    root = tempfile.mkdtemp(prefix="clippy-sync-")
    members = ", ".join(f'"{c}"' for c in CRATES)
    write(root, "Cargo.toml", f'[workspace]\nresolver = "2"\nmembers = [{members}]\n')
    for name, src in CRATES.items():
        write(root, f"{name}/Cargo.toml",
              f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
              '\n[features]\nx = []\n')
        write(root, f"{name}/src/lib.rs", src)
    for path, body in extra_files:
        write(root, path, body)
    write(root, "Makefile", "clippy-ci:\n"
          + "".join(f"\t\t{s}\n" for s in steps) + "\nfmt:\n\t\tcargo fmt --all\n")
    write(root, ".github/workflows/pr-check-rust-hcl.yaml",
          WORKFLOW.format(steps=yaml_steps(steps)))
    subprocess.run(["git", "-C", root, "init", "-q"], check=True, capture_output=True)
    subprocess.run(["git", "-C", root, "add", "-A", "-f"], check=True, capture_output=True)
    for path, body in untracked:
        write(root, path, body)
    return root


class SyncCheckTest(unittest.TestCase):
    def setUp(self):
        self.roots = []

    def tearDown(self):
        for root in self.roots:
            shutil.rmtree(root, ignore_errors=True)

    def guard(self, steps=None, workflow_steps=None, gamma=None, untracked=()):
        """Run the guard over a fixture. `gamma` is one file added to the crate
        that has no step, so its content alone decides the verdict."""
        steps = self.base() if steps is None else steps
        extra = [(f"gamma/src/{gamma[0]}", gamma[1])] if gamma else []
        root = workspace(steps, extra_files=extra, untracked=untracked)
        self.roots.append(root)
        if workflow_steps is not None:
            write(root, ".github/workflows/pr-check-rust-hcl.yaml",
                  WORKFLOW.format(steps=yaml_steps(workflow_steps)))
        return subprocess.run(["bash", SCRIPT], cwd=root, capture_output=True,
                              text=True, timeout=300)

    def assertGreen(self, out, why):
        self.assertEqual(out.returncode, 0, f"{why}\nstdout:{out.stdout}\nstderr:{out.stderr}")

    def assertNames(self, out, crate, why):
        self.assertNotEqual(out.returncode, 0, f"{why}\nstdout:{out.stdout}")
        self.assertIn(crate, out.stderr, f"{why}: did not name {crate}\n{out.stderr}")

    def base(self):
        return ["cargo clippy --all -- -D warnings",
                STEP.format("alpha"), STEP.format("beta")]

    def test_0_wellformed_fixture_is_green(self):
        self.assertGreen(self.guard(), "the fixture itself must pass")

    def test_1_step_without_tests_flag_is_not_coverage(self):
        steps = self.base()
        steps[1] = "cargo clippy -p alpha -- -D warnings"
        self.assertNames(self.guard(steps), "alpha",
                         "a step lacking --tests must not count as coverage")

    def test_2_cfg_all_test_crate_needs_a_step(self):
        self.assertNames(self.guard([s for s in self.base() if "beta" not in s]), "beta",
                         "#[cfg(all(test, ...))] must count as test code")

    def test_3_package_long_form_is_coverage(self):
        steps = self.base()
        steps[1] = "cargo clippy --package alpha --tests -- -D warnings"
        self.assertGreen(self.guard(steps), "--package must be accepted")

    def test_3_p_equals_form_is_coverage(self):
        steps = self.base()
        steps[1] = "cargo clippy -p=alpha --tests -- -D warnings"
        self.assertGreen(self.guard(steps), "-p=NAME must be accepted")

    def test_4_lists_drifting_from_each_other_fails(self):
        out = self.guard(workflow_steps=self.base()[:-1])
        self.assertNotEqual(out.returncode, 0, "byte-identity drift must fail")
        self.assertIn("drifted", out.stderr)

    def test_5_test_text_in_a_string_literal_is_not_test_code(self):
        self.assertGreen(
            self.guard(gamma=("args.rs",
                              '#[clap(long = "debug-send-test-pattern")]\npub const M: u8 = 0;\n')),
            "a kebab-case string literal must not count as a test attribute")

    def test_6_untracked_test_bearing_file_is_ignored(self):
        self.assertGreen(
            self.guard(untracked=[("gamma/src/_leftover.rs", "#[cfg(test)]\nmod x {}\n")]),
            "an untracked leftover must not turn the guard red")

    def test_7_test_attribute_inside_a_comment_is_not_test_code(self):
        self.assertGreen(
            self.guard(gamma=("doc.rs", "/// Driven from a `#[test]` in another crate.\n"
                                        "/* block: #[cfg(test)] mod x {} */\npub const M: u8 = 0;\n")),
            "an attribute inside a comment must not count as test code")

    def test_8_feature_named_testing_is_not_test_code(self):
        self.assertGreen(
            self.guard(gamma=("gate.rs", '#[cfg(feature = "testing")]\npub fn f() {}\n')),
            'a feature literally named "testing" must not count as test code')

    def test_8_wasm_bindgen_test_alone_needs_a_step(self):
        self.assertNames(
            self.guard(gamma=("wasm.rs", "#[wasm_bindgen_test]\nfn t() {}\n")), "gamma",
            "a test attribute suffixed onto an identifier must count as test code")

    def test_9_test_attribute_without_cfg_test_needs_a_step(self):
        self.assertNames(
            self.guard(gamma=("tok.rs", "#[tokio::test]\nasync fn t() {}\n")), "gamma",
            "a #[test] with no #[cfg(test)] in the file must count as test code")


if __name__ == "__main__":
    unittest.main(verbosity=2)
