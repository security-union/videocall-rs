#!/usr/bin/env python3
"""Regression tests for check-protos-regen.sh using a synthetic git repo.

The real guard intentionally builds Docker and regenerates the full proto set.
These tests stub only Docker so the failure semantics stay cheap to validate:
build/run failures fail closed, empty regen output is not accepted, tracked
drift is reported, and brand-new generated files are not missed as untracked
git output.
"""
from __future__ import annotations

import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("check-protos-regen.sh")


FAKE_DOCKER = """#!/usr/bin/env bash
set -euo pipefail

if [[ "${PROTO_TEST_DOCKER_FAIL:-}" == "1" ]]; then
  echo "simulated docker failure" >&2
  exit 42
fi

case "${1:-}" in
  build)
    iidfile=""
    while [[ "$#" -gt 0 ]]; do
      case "$1" in
        --iidfile)
          iidfile="$2"
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    if [[ -z "$iidfile" ]]; then
      echo "fake docker: missing --iidfile" >&2
      exit 99
    fi
    if [[ "${PROTO_TEST_EMPTY_IID:-}" != "1" ]]; then
      printf '%s\\n' "fake-image-id" > "$iidfile"
    fi
    exit 0
    ;;
  run)
    workdir=""
    while [[ "$#" -gt 0 ]]; do
      case "$1" in
        -w)
          workdir="$2"
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    if [[ -z "$workdir" ]]; then
      echo "fake docker: missing -w" >&2
      exit 99
    fi
    if [[ "${PROTO_TEST_EMPTY_OUTPUT:-}" == "1" ]]; then
      exit 0
    fi
    mkdir -p "$workdir/build/rust"
    printf '%s\\n' "${PROTO_TEST_GENERATED_BODY:-generated}" > "$workdir/build/rust/foo.rs"
    if [[ -n "${PROTO_TEST_EXTRA_FILE:-}" ]]; then
      printf '%s\\n' "new generated file" > "$workdir/build/rust/$PROTO_TEST_EXTRA_FILE"
    fi
    exit 0
    ;;
esac

echo "fake docker: unexpected invocation: $*" >&2
exit 99
"""


def write(path: Path, body: str, executable: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    if executable:
        path.chmod(path.stat().st_mode | stat.S_IXUSR)


class ProtoRegenGuardTest(unittest.TestCase):
    def setUp(self) -> None:
        self.roots: list[Path] = []

    def tearDown(self) -> None:
        for root in self.roots:
            shutil.rmtree(root, ignore_errors=True)

    def workspace(
        self,
        committed_body: str = "generated\n",
        extra_tracked_generated: tuple[str, str] | None = None,
    ) -> tuple[Path, Path]:
        root = Path(tempfile.mkdtemp(prefix="proto-regen-"))
        self.roots.append(root)
        write(root / "protobuf/build-env-rust.Dockerfile", "FROM scratch\n")
        write(root / "protobuf/types/foo.proto", 'syntax = "proto3";\n')
        write(root / "videocall-types/src/protos/foo.rs", committed_body)
        write(root / "videocall-types/src/protos/mod.rs", "pub mod foo;\n")
        if extra_tracked_generated is not None:
            name, body = extra_tracked_generated
            write(root / f"videocall-types/src/protos/{name}", body)
        fake_bin = root / "fake-bin"
        write(fake_bin / "docker", FAKE_DOCKER, executable=True)
        subprocess.run(["git", "-C", root, "init", "-q"], check=True)
        subprocess.run(["git", "-C", root, "add", "-A"], check=True)
        return root, fake_bin

    def guard(self, **env: str) -> subprocess.CompletedProcess[str]:
        root, fake_bin = self.workspace(
            env.pop("committed_body", "generated\n"),
            env.pop("extra_tracked_generated", None),
        )
        run_env = os.environ.copy()
        run_env.update(env)
        run_env["PATH"] = f"{fake_bin}:{run_env['PATH']}"
        return subprocess.run(
            ["bash", str(SCRIPT)],
            cwd=root,
            env=run_env,
            capture_output=True,
            text=True,
            timeout=30,
        )

    def assert_failed(self, out: subprocess.CompletedProcess[str], why: str) -> None:
        self.assertNotEqual(
            out.returncode,
            0,
            f"{why}\nstdout:\n{out.stdout}\nstderr:\n{out.stderr}",
        )

    def test_matching_generated_output_is_green(self) -> None:
        out = self.guard()
        self.assertEqual(out.returncode, 0, f"stdout:\n{out.stdout}\nstderr:\n{out.stderr}")
        self.assertIn("up to date", out.stdout)

    def test_tracked_drift_fails_and_names_file(self) -> None:
        out = self.guard(committed_body="stale\n", PROTO_TEST_GENERATED_BODY="fresh")
        self.assert_failed(out, "tracked generated drift must fail")
        self.assertIn("videocall-types/src/protos/foo.rs", out.stderr)
        self.assertIn("Regenerate them", out.stderr)

    def test_new_generated_file_is_not_missed_as_untracked(self) -> None:
        out = self.guard(PROTO_TEST_EXTRA_FILE="new_packet.rs")
        self.assert_failed(out, "untracked generated output must fail")
        self.assertIn("videocall-types/src/protos/new_packet.rs", out.stderr)
        self.assertIn("Untracked generated files", out.stderr)

    def test_stale_tracked_generated_file_is_deleted_and_reported(self) -> None:
        out = self.guard(extra_tracked_generated=("old_packet.rs", "stale\n"))
        self.assert_failed(out, "tracked output for a removed proto must fail")
        self.assertIn("videocall-types/src/protos/old_packet.rs", out.stderr)

    def test_mod_rs_is_preserved_as_handwritten_module_list(self) -> None:
        out = self.guard()
        self.assertEqual(out.returncode, 0, f"stdout:\n{out.stdout}\nstderr:\n{out.stderr}")

    def test_empty_regen_output_fails_closed(self) -> None:
        out = self.guard(PROTO_TEST_EMPTY_OUTPUT="1")
        self.assert_failed(out, "empty generator output must not pass")
        self.assertIn("produced no Rust files", out.stderr)

    def test_missing_build_image_id_fails_closed(self) -> None:
        out = self.guard(PROTO_TEST_EMPTY_IID="1")
        self.assert_failed(out, "missing docker build image ID must fail closed")
        self.assertIn("did not write an image ID", out.stderr)
        self.assertNotIn("up to date", out.stdout)

    def test_docker_error_fails_closed_before_diff_success(self) -> None:
        out = self.guard(PROTO_TEST_DOCKER_FAIL="1")
        self.assert_failed(out, "docker failure must fail closed")
        self.assertIn("simulated docker failure", out.stderr)
        self.assertNotIn("up to date", out.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
