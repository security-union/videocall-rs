#!/usr/bin/env python3
"""Build videocall-bot:cq-<sha12> from a clean checkout, as the shared-target preflight checks it (#2914)."""

import argparse
import gzip
import os
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import cq_local  # noqa: E402

EXIT_REFUSED = 3
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


class Refused(Exception):
    pass


def _git(root, *args):
    return subprocess.run(["git", "-C", root, *args], capture_output=True, text=True, check=True).stdout


def build(root, engine="docker", save=None):
    if _git(root, "status", "--porcelain=v1", "--untracked-files=all").strip():
        raise Refused("the tree is dirty: commit or stash first; a shared target only takes a clean build")
    commit = _git(root, "rev-parse", "HEAD").strip()
    tag = cq_local.bot_image_tag(commit, dirty=False)
    rc = subprocess.run(cq_local.bot_build_argv(engine, commit, "clean", tag), cwd=root).returncode
    if rc != 0:
        raise Refused(f"{engine} build exited {rc}")
    p = subprocess.run([engine, "image", "inspect", "--format", cq_local.BOT_LABELS_FORMAT, tag],
                       capture_output=True, text=True)
    if p.returncode != 0 or p.stdout.strip() != cq_local.bot_labels(commit, dirty=False):
        raise Refused(f"{tag} does not carry the labels the preflight checks: {p.stdout.strip()!r}")
    if save:
        _save(engine, tag, save)
    return tag


def _save(engine, tag, path):
    tmp, p = path + ".part", None
    try:
        with open(tmp, "wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=0) as out:
            p = subprocess.Popen([engine, "save", tag], stdout=subprocess.PIPE)
            shutil.copyfileobj(p.stdout, out, 1 << 20)
        rc = p.wait()
        if rc != 0:
            raise Refused(f"{engine} save exited {rc}")
        os.replace(tmp, path)
    finally:
        if p is not None:
            p.stdout.close()
            if p.poll() is None:
                p.kill()
            p.wait()
        if os.path.exists(tmp):
            os.remove(tmp)


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--engine", default="docker", choices=("docker", "podman"))
    p.add_argument("--save", metavar="PATH.tar.gz")
    args = p.parse_args(argv)
    root = os.path.realpath(ROOT)
    if args.save and os.path.commonpath([os.path.realpath(args.save), root]) == root:
        print(f"cq-bot-image: --save {args.save} is inside the checkout, so it would dirty the tree; save it outside",
              file=sys.stderr)
        return EXIT_REFUSED
    try:
        tag = build(root, args.engine, args.save)
    except (Refused, subprocess.CalledProcessError, OSError) as exc:
        print(f"cq-bot-image: {exc}", file=sys.stderr)
        return EXIT_REFUSED
    print(tag + (f" saved to {args.save}" if args.save else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
