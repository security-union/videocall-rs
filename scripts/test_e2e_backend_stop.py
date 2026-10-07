#!/usr/bin/env python3
# docker/e2e-backend.sh build-run must stop its `cargo run` child on SIGTERM, escalating
# to SIGKILL after STOP_TIMEOUT_SECS, or cargo-watch cannot restart a backend that ignores SIGTERM.
from __future__ import annotations

import os
import re
import shutil
import signal
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "docker" / "e2e-backend.sh"
STOP_TIMEOUT_SECS = int(re.search(r"^STOP_TIMEOUT_SECS=(\d+)$", SCRIPT.read_text(), re.M).group(1))
STOP_CHILD = re.search(r"^stop_child\(\) \{$.*?^\}$", SCRIPT.read_text(), re.M | re.S).group(0)
FORK_HOLD_SECS = 0.5
FORK_HOLD_STRACE = (
    "strace", "-o", os.devnull, "-e", "trace=clone,clone3,fork,vfork",
    "-e", f"inject=clone,clone3,fork,vfork:delay_exit={int(FORK_HOLD_SECS * 1_000_000)}",
)
STRACE_USABLE = (
    shutil.which("strace") is not None
    and subprocess.run([*FORK_HOLD_STRACE, "true"], stderr=subprocess.DEVNULL).returncode == 0
)

FAKE_CARGO = """#!/usr/bin/env python3
import os, signal, sys, time
if sys.argv[1] != "run":
    sys.exit(0)
mode = os.environ["FAKE_CARGO_MODE"]
if mode == "exit":
    sys.exit(7)
if mode == "ignore":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
else:
    signal.signal(signal.SIGTERM, lambda *_: os._exit(42))
ready = os.environ["FAKE_CARGO_READY"]
with open(ready + ".tmp", "w") as f:
    f.write(str(os.getpid()))
os.replace(ready + ".tmp", ready)
time.sleep(120)
"""


class E2eBackendStopTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="e2e-backend-stop-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        cargo = self.root / "cargo"
        cargo.write_text(FAKE_CARGO)
        cargo.chmod(0o755)
        self.ready = self.root / "ready"

    def _start(
        self,
        mode: str,
        wrapper: tuple[str, ...] = (),
        command: tuple[str, ...] = ("bash", str(SCRIPT), "build-run", "fake-bin"),
    ) -> subprocess.Popen:
        env = dict(os.environ)
        env["PATH"] = f"{self.root}:{env['PATH']}"
        env["FAKE_CARGO_MODE"] = mode
        env["FAKE_CARGO_READY"] = str(self.ready)
        env.pop("E2E_CARGO_RELEASE", None)
        env["E2E_STAMP_DIR"] = str(self.root / "stamps")
        proc = subprocess.Popen(
            [*wrapper, *command],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self.addCleanup(self._kill_group, proc)
        return proc

    @staticmethod
    def _kill_group(proc: subprocess.Popen) -> None:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait()

    def _child_pid(self) -> int:
        deadline = time.monotonic() + 10
        while not self.ready.exists():
            self.assertLess(time.monotonic(), deadline, "fake cargo run never started")
            time.sleep(0.05)
        return int(self.ready.read_text())

    def _terminate(
        self, proc: subprocess.Popen, sig: signal.Signals = signal.SIGTERM
    ) -> tuple[int, float]:
        start = time.monotonic()
        proc.send_signal(sig)
        rc = proc.wait(timeout=STOP_TIMEOUT_SECS + 5)
        return rc, time.monotonic() - start

    def test_child_exiting_on_sigterm_propagates_its_status_promptly(self) -> None:
        proc = self._start("term")
        self._child_pid()
        rc, elapsed = self._terminate(proc)
        self.assertEqual(rc, 42)
        self.assertLess(elapsed, STOP_TIMEOUT_SECS / 2)

    def test_sigint_stops_the_child_like_sigterm(self) -> None:
        proc = self._start("term")
        self._child_pid()
        rc, elapsed = self._terminate(proc, signal.SIGINT)
        self.assertEqual(rc, 42)
        self.assertLess(elapsed, STOP_TIMEOUT_SECS / 2)

    def test_child_ignoring_sigterm_is_sigkilled_after_the_timeout(self) -> None:
        proc = self._start("ignore")
        child = self._child_pid()
        rc, elapsed = self._terminate(proc)
        self.assertEqual(rc, 128 + signal.SIGKILL)
        self.assertGreaterEqual(elapsed, STOP_TIMEOUT_SECS - 1)
        with self.assertRaises(ProcessLookupError):
            os.kill(child, 0)

    def test_repeated_signal_does_not_restart_the_timeout(self) -> None:
        proc = self._start("ignore")
        self._child_pid()
        start = time.monotonic()
        proc.send_signal(signal.SIGTERM)
        time.sleep(STOP_TIMEOUT_SECS / 2)
        proc.send_signal(signal.SIGTERM)
        rc = proc.wait(timeout=STOP_TIMEOUT_SECS * 2)
        self.assertEqual(rc, 128 + signal.SIGKILL)
        self.assertLess(time.monotonic() - start, STOP_TIMEOUT_SECS + 2)

    def test_child_exiting_on_its_own_propagates_its_status(self) -> None:
        proc = self._start("exit")
        self.assertEqual(proc.wait(timeout=10), 7)

    def _run_stop_child(self, setup: str) -> int:
        body = (
            f"set -uo pipefail\nBIN=fake-bin\nSTOP_TIMEOUT_SECS={STOP_TIMEOUT_SECS}\n{STOP_CHILD}\n"
            f'CHILD=""\n{setup}stop_child\n'
        )
        return self._start("term", command=("bash", "-c", body)).wait(timeout=STOP_TIMEOUT_SECS + 5)

    def test_stop_child_before_child_assignment_stops_the_last_background_job(self) -> None:
        setup = 'cargo run &\nuntil [[ -e "${FAKE_CARGO_READY}" ]]; do sleep 0.05; done\n'
        self.assertEqual(self._run_stop_child(setup), 42)

    def test_stop_child_before_any_background_job_exits_143(self) -> None:
        self.assertEqual(self._run_stop_child(""), 143)

    @unittest.skipUnless(STRACE_USABLE, "needs strace with ptrace permitted")
    def test_signal_between_fork_and_child_assignment_still_stops_the_child(self) -> None:
        proc = self._start("term", FORK_HOLD_STRACE)
        child = self._child_pid()
        bash = int(re.search(r"^PPid:\s+(\d+)$", Path(f"/proc/{child}/status").read_text(), re.M).group(1))
        state = re.search(r"^State:\s+(\S)", Path(f"/proc/{bash}/status").read_text(), re.M).group(1)
        self.assertEqual(state, "t", "bash is not held inside the fork, so the signal would not precede CHILD=$!")
        os.kill(bash, signal.SIGTERM)
        self.assertEqual(proc.wait(timeout=FORK_HOLD_SECS + STOP_TIMEOUT_SECS + 5), 42)


if __name__ == "__main__":
    unittest.main()
