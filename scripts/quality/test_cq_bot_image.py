#!/usr/bin/env python3
"""Tests for cq_bot_image.py: the image it builds is the one the shared-target preflight accepts."""

import contextlib
import errno
import gzip
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import types
import unittest
import unittest.mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cq_bot_image  # noqa: E402
import cq_local  # noqa: E402
import cq_remote  # noqa: E402
import cq_scenario  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(HERE))
S0 = os.path.join(HERE, "scenarios", "phase0", "s0.yaml")

FAKE_ENGINE = r'''#!/usr/bin/env python3
"""A container engine that keeps image labels the way Dockerfile.bot sets them, in $FAKE_STATE."""
import json, os, re, sys
state_path = os.environ["FAKE_STATE"]
state = json.load(open(state_path)) if os.path.exists(state_path) else {"calls": [], "images": {}}
argv = sys.argv[1:]
state["calls"].append(argv)
out, rc = "", 0
if argv[0] == "build":
    tag = argv[argv.index("-t") + 1]
    labels = dict(a.split("=", 1) for i, a in enumerate(argv) if i and argv[i - 1] == "--label")
    args = dict(a.split("=", 1) for i, a in enumerate(argv) if i and argv[i - 1] == "--build-arg")
    labels[os.environ["REVISION_LABEL"]] = args.get("GIT_SHA", "unknown")
    if os.environ.get("FAKE_NO_LABELS"):
        labels = {}
    state["images"][tag] = labels
    rc = int(os.environ.get("FAKE_BUILD_RC", "0"))
elif argv[:2] == ["image", "inspect"]:
    labels = state["images"].get(argv[-1])
    if labels is None:
        rc = 1
    else:
        out = re.sub(r'\{\{index \.Config\.Labels "([^"]+)"\}\}', lambda m: labels.get(m.group(1), "<no value>"),
                     argv[argv.index("--format") + 1]) + "\n"
elif argv[0] == "run":
    out = "  --deadline <epoch-seconds>  End the run\n  --diagnostics <on|off>  DIAGNOSTICS\n  replay\n"
elif argv[0] == "save":
    out = "archive of " + argv[1]
    rc = int(os.environ.get("FAKE_SAVE_RC", "0"))
json.dump(state, open(state_path, "w"))
sys.stdout.write(out)
sys.exit(rc)
'''


def git(root, *args):
    return subprocess.run(["git", "-C", root, *args], capture_output=True, text=True, check=True).stdout.strip()


class Build(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="cq-bot-image-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.repo = os.path.join(self.tmp, "repo")
        os.makedirs(self.repo)
        git(self.repo, "init", "-q")
        with open(os.path.join(self.repo, "Dockerfile.bot"), "w", encoding="utf-8") as fh:
            fh.write("FROM scratch\n")
        git(self.repo, "add", ".")
        git(self.repo, "-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "x")
        self.commit = git(self.repo, "rev-parse", "HEAD")
        self.engine = os.path.join(self.tmp, "engine")
        with open(self.engine, "w", encoding="utf-8") as fh:
            fh.write(FAKE_ENGINE)
        os.chmod(self.engine, 0o755)
        self.state = os.path.join(self.tmp, "state.json")
        env = dict(os.environ, FAKE_STATE=self.state, REVISION_LABEL=cq_local.REVISION_LABEL)
        patcher = unittest.mock.patch.dict(os.environ, env)
        patcher.start()
        self.addCleanup(patcher.stop)

    def calls(self):
        if not os.path.exists(self.state):
            return []
        with open(self.state, encoding="utf-8") as fh:
            return json.load(fh)["calls"]

    def remote_problems(self, tag):
        """The shared-target preflight's own bot image check, run against the fake engine."""
        ctx = types.SimpleNamespace(docker=self.engine, commit=self.commit, dirty=False, run_flags=lambda: [],
                                    sh=lambda argv, timeout, check=True: subprocess.run(argv, capture_output=True,
                                                                                        text=True))
        ctx.bot_image = cq_local.Local.bot_image.fget(ctx)
        self.assertEqual(ctx.bot_image, tag)
        with open(S0, encoding="utf-8") as fh:
            plan = cq_scenario.compile_scenario(cq_scenario.parse_yaml(fh.read()), run_id="p0s0-20261021t0000-0123abc",
                                                scenario_file="s0.yaml", scenario_sha256="0" * 64)
        return cq_remote.RemoteStack._bot_image_problems(types.SimpleNamespace(ctx=ctx), plan)

    def test_the_built_image_is_the_one_the_remote_preflight_accepts(self):
        tag = cq_bot_image.build(self.repo, self.engine)
        self.assertEqual(tag, "videocall-bot:cq-" + self.commit[:12])
        self.assertEqual(self.calls()[0], cq_local.bot_build_argv(self.engine, self.commit, "clean", tag)[1:])
        self.assertEqual(self.remote_problems(tag), [])

    def test_dockerfile_bot_sets_the_revision_label_the_preflight_reads(self):
        with open(os.path.join(ROOT, "Dockerfile.bot"), encoding="utf-8") as fh:
            self.assertRegex(fh.read(), rf"(?m)^LABEL {re.escape(cq_local.REVISION_LABEL)}=\$\{{GIT_SHA\}}$")

    def test_a_dirty_tree_is_refused_before_any_build(self):
        with open(os.path.join(self.repo, "new.txt"), "w", encoding="utf-8") as fh:
            fh.write("x")
        with self.assertRaisesRegex(cq_bot_image.Refused, "the tree is dirty"):
            cq_bot_image.build(self.repo, self.engine)
        self.assertEqual(self.calls(), [])

    def test_a_failed_build_is_refused_and_nothing_is_saved(self):
        os.environ["FAKE_BUILD_RC"] = "2"
        out = os.path.join(self.tmp, "bot.tar.gz")
        with self.assertRaisesRegex(cq_bot_image.Refused, "build exited 2"):
            cq_bot_image.build(self.repo, self.engine, save=out)
        self.assertFalse(os.path.exists(out))

    def test_an_image_without_the_preflight_labels_is_refused(self):
        os.environ["FAKE_NO_LABELS"] = "1"
        with self.assertRaisesRegex(cq_bot_image.Refused, "does not carry the labels the preflight checks"):
            cq_bot_image.build(self.repo, self.engine)

    def test_save_writes_the_gzipped_archive_of_that_tag(self):
        out = os.path.join(self.tmp, "bot.tar.gz")
        tag = cq_bot_image.build(self.repo, self.engine, save=out)
        with gzip.open(out, "rt", encoding="utf-8") as fh:
            self.assertEqual(fh.read(), "archive of " + tag)
        self.assertEqual(self.calls()[-1], ["save", tag])
        self.assertFalse(os.path.exists(out + ".part"))

    def old_archive(self):
        out = os.path.join(self.tmp, "bot.tar.gz")
        with open(out, "w", encoding="utf-8") as fh:
            fh.write("the previous archive")
        return out

    def assert_kept(self, out):
        with open(out, encoding="utf-8") as fh:
            self.assertEqual(fh.read(), "the previous archive")
        self.assertFalse(os.path.exists(out + ".part"))

    def test_a_failed_export_keeps_the_previous_archive(self):
        out = self.old_archive()
        os.environ["FAKE_SAVE_RC"] = "1"
        with self.assertRaisesRegex(cq_bot_image.Refused, "save exited 1"):
            cq_bot_image.build(self.repo, self.engine, save=out)
        self.assert_kept(out)

    def test_an_export_that_cannot_start_or_write_cleans_up_and_reaps_the_engine(self):
        out = self.old_archive()
        tag = cq_bot_image.build(self.repo, self.engine)
        with unittest.mock.patch.object(cq_bot_image.subprocess, "Popen", side_effect=FileNotFoundError("engine")), \
                self.assertRaises(FileNotFoundError):
            cq_bot_image._save(self.engine, tag, out)
        self.assert_kept(out)
        started, popen = [], subprocess.Popen

        def track(*a, **kw):
            started.append(popen(*a, **kw))
            return started[-1]
        with unittest.mock.patch.object(cq_bot_image.subprocess, "Popen", track), \
                unittest.mock.patch.object(cq_bot_image.shutil, "copyfileobj",
                                           side_effect=OSError(errno.ENOSPC, "No space left on device")), \
                self.assertRaises(OSError):
            cq_bot_image._save(self.engine, tag, out)
        self.assert_kept(out)
        self.assertEqual(len(started), 1)
        self.assertIsNotNone(started[0].returncode)

    def main(self, *argv, engines=("podman",), root=None):
        """cq_bot_image.main on the test repo, with only `engines` (the fake), git and python3 on PATH."""
        bin_dir = os.path.join(self.tmp, "bin")
        os.makedirs(bin_dir, exist_ok=True)
        for name in os.listdir(bin_dir):
            os.remove(os.path.join(bin_dir, name))
        for tool, path in (("git", shutil.which("git")), ("python3", sys.executable)):
            os.symlink(path, os.path.join(bin_dir, tool))
        for name in engines:
            os.symlink(self.engine, os.path.join(bin_dir, name))
        out, err = io.StringIO(), io.StringIO()
        with unittest.mock.patch.object(cq_bot_image, "ROOT", root or self.repo), \
                unittest.mock.patch.dict(os.environ, {"PATH": bin_dir}), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = cq_bot_image.main(list(argv))
        return rc, out.getvalue(), err.getvalue()

    def test_main_saves_with_the_named_engine(self):
        out = os.path.join(self.tmp, "bot.tar.gz")
        rc, stdout, err = self.main("--engine", "podman", "--save", out)
        tag = "videocall-bot:cq-" + self.commit[:12]
        self.assertEqual((rc, stdout, err), (0, f"{tag} saved to {out}\n", ""))
        with gzip.open(out, "rt", encoding="utf-8") as fh:
            self.assertEqual(fh.read(), "archive of " + tag)

    def test_main_exits_3_on_a_missing_engine_or_a_failed_git(self):
        rc, stdout, err = self.main("--engine", "docker")
        self.assertEqual((rc, stdout), (3, ""))
        self.assertRegex(err, r"^cq-bot-image: .*docker")
        os.remove(os.path.join(self.repo, ".git", "HEAD"))
        rc, stdout, err = self.main("--engine", "podman")
        self.assertEqual((rc, stdout), (3, ""))
        self.assertRegex(err, r"^cq-bot-image: Command .*git.* returned non-zero exit status")

    def test_main_refuses_a_save_path_inside_the_checkout(self):
        for path in (os.path.join(self.repo, "bot.tar.gz"), os.path.join(self.repo, "sub", "..", "bot.tar.gz")):
            rc, stdout, err = self.main("--save", path)
            self.assertEqual((rc, stdout), (3, ""))
            self.assertIn("is inside the checkout, so it would dirty the tree", err)
        self.assertEqual(self.calls(), [])
        self.assertEqual(git(self.repo, "status", "--porcelain"), "")

    def test_main_resolves_symlinks_on_both_sides_of_the_inside_check(self):
        link = os.path.join(self.tmp, "link-to-repo")
        os.symlink(self.repo, link)
        inside_via_link, inside = os.path.join(link, "bot.tar.gz"), os.path.join(self.repo, "bot.tar.gz")
        for root, path in ((self.repo, inside_via_link), (link, inside)):
            with self.subTest(root=os.path.basename(root)):
                rc, stdout, err = self.main("--save", path, root=root)
                self.assertEqual((rc, stdout), (3, ""))
                self.assertIn("is inside the checkout, so it would dirty the tree", err)
        self.assertEqual(self.calls(), [])
        self.assertEqual(git(self.repo, "status", "--porcelain"), "")

    def test_main_refuses_with_exit_3(self):
        def refuse(root, engine, save):
            raise cq_bot_image.Refused("the tree is dirty")
        err = io.StringIO()
        with unittest.mock.patch.object(cq_bot_image, "build", refuse), contextlib.redirect_stderr(err):
            self.assertEqual(cq_bot_image.main([]), cq_bot_image.EXIT_REFUSED)
        self.assertEqual(err.getvalue(), "cq-bot-image: the tree is dirty\n")


if __name__ == "__main__":
    unittest.main()
