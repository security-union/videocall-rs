#!/usr/bin/env python3
# scripts/ci/trim-cargo-targets.sh must never sweep a profile whose lock files it could not open.
from __future__ import annotations

import os
import re
import shutil
import signal
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "scripts" / "ci" / "trim-cargo-targets.sh"
_TEXT = SCRIPT.read_text()
_MAIN = list(re.finditer(r'^for dir in "\$@"; do$', _TEXT, re.M))
assert len(_MAIN) == 1, "expected one top-level main loop in trim-cargo-targets.sh"
FUNCTIONS = _TEXT[: _MAIN[0].start()]

FAKE_SWEEP = '#!/bin/sh\ntouch "$SWEEP_MARKER"\n'


class TrimCargoTargetsLockTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="trim-cargo-targets-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.sweep = self.root / "cargo-sweep"
        self.sweep.write_text(FAKE_SWEEP)
        self.sweep.chmod(0o755)
        self.marker = self.root / "swept"
        self.target = self.root / "target"
        self.profile = self.target / "debug"
        (self.profile / ".fingerprint").mkdir(parents=True)

    def _sweep_dir(self) -> subprocess.CompletedProcess[str]:
        program = FUNCTIONS + f'\nsweep_dir "{self.target}" 7 "{self.root}" 0\nexit "$FAILED"\n'
        env = dict(
            os.environ,
            CARGO_SWEEP=str(self.sweep),
            SWEEP_MARKER=str(self.marker),
            LOCK_WAIT_SECS="0",
        )
        return subprocess.run(
            ["bash", "-c", program, "trim-cargo-targets.sh", str(self.target)],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )

    def _assert_unopenable_lock_fails_without_sweeping(self, lock: str) -> None:
        (self.profile / lock).mkdir()
        r = self._sweep_dir()
        out = r.stdout + r.stderr
        self.assertFalse(self.marker.exists(), f"swept without holding {lock}:\n{out}")
        self.assertNotEqual(r.returncode, 0, out)
        self.assertIn("::error::", out)

    def test_first_lock_unopenable(self) -> None:
        self._assert_unopenable_lock_fails_without_sweeping(".cargo-build-lock")

    def test_second_lock_unopenable(self) -> None:
        self._assert_unopenable_lock_fails_without_sweeping(".cargo-lock")

    def test_busy_lock_skips_with_warning(self) -> None:
        lock = self.profile / ".cargo-lock"
        ready = self.root / "held"
        holder = subprocess.Popen(
            ["flock", str(lock), "sh", "-c", f'touch "{ready}"; exec sleep 60'],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self.addCleanup(holder.wait)
        self.addCleanup(os.killpg, holder.pid, signal.SIGKILL)
        for _ in range(200):
            if ready.exists():
                break
            subprocess.run(["sleep", "0.05"])
        self.assertTrue(ready.exists(), "lock holder never started")
        r = self._sweep_dir()
        out = r.stdout + r.stderr
        self.assertFalse(self.marker.exists(), out)
        self.assertEqual(r.returncode, 0, out)
        self.assertIn("::warning::", out)
        self.assertNotIn("::error::", out)

    def test_free_locks_sweep(self) -> None:
        r = self._sweep_dir()
        out = r.stdout + r.stderr
        self.assertEqual(r.returncode, 0, out)
        self.assertTrue(self.marker.exists(), out)


if __name__ == "__main__":
    unittest.main()
