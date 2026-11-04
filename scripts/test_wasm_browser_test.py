import importlib.machinery
import importlib.util
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

WRAPPER = pathlib.Path(__file__).resolve().parent / "wasm-browser-test"
_spec = importlib.util.spec_from_loader(
    "wasm_browser_test", importlib.machinery.SourceFileLoader("wasm_browser_test", str(WRAPPER)))
wbt = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(wbt)

MAC = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
PROFILE = "/var/folders/79/x/T/org.chromium.Chromium.scoped_dir.AbC123"
LINUX_PROFILE = "/tmp/.org.chromium.Chromium.Xy12Zq"

ORPHAN_BLOCKING_THE_RUNNER = "; ".join([
    "import os, subprocess, sys",
    "subprocess.Popen(['bash', '-c', 'exec -a \"$0\" bash -c \"$1\" inner \"$2\"', 'chromedriver --port=0', "
    "'(exec -a \"$1\" sleep 60) & wait', os.environ['DRIVEN']], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)",
    "subprocess.Popen(['bash', '-c', 'trap \"\" TERM; exec -a \"$0\" sleep 60', os.environ['ORPHAN']]).wait()",
    "sys.exit(1)",
])
ORPHAN_LEFT_AT_EXIT = "; ".join([
    "import os, subprocess",
    "subprocess.Popen(['bash', '-c', 'exec -a \"$0\" bash -c \"$GRACEFUL\"', os.environ['ORPHAN']])",
])
GRACEFUL = (r'trap "sleep 1; mkdir -p \"$P\"; touch \"$P/Last Session\" \"$P.closed\"; exit 0" TERM; '
            r'while :; do sleep 0.2; done')


def automation(profile):
    return f"--allow-pre-commit-input --enable-automation --headless --remote-debugging-port=0 --user-data-dir={profile} data:,"


def fake_chrome(profile):
    return f"chrome --user-data-dir={profile} --enable-automation"


def live(profile):
    return [pid for pid, (_, cmd) in wbt.processes().items() if profile in cmd]


def kill_all(profile):
    for pid in live(profile):
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


class OrphanSelectionTest(unittest.TestCase):
    PROCS = {
        10: (1, f"{MAC} {automation(PROFILE)}"),
        20: (1, "/opt/cft/chromedriver-mac-arm64/chromedriver --port=51828"),
        21: (20, f"{MAC} {automation(PROFILE + 'Live')}"),
        22: (1, f"/opt/google/chrome/chrome --type=renderer --user-data-dir={LINUX_PROFILE} --enable-automation"),
        30: (1, f"{MAC} --flag-switches-begin --flag-switches-end"),
        31: (1, f"{MAC} --enable-automation --user-data-dir=/var/folders/T/playwright_chromiumdev_profile-q1"),
        32: (1, f"/x/chrome_crashpad_handler --user-data-dir={PROFILE} --enable-automation"),
        40: (1, f"/opt/google/chrome/chrome --headless --user-data-dir={LINUX_PROFILE} about:blank"),
        41: (1, f"/opt/google/chrome/chrome --user-data-dir={LINUX_PROFILE}2 about:blank"),
        50: (1, f"/usr/bin/grep -- --user-data-dir={PROFILE} --headless"),
    }

    def test_selects_only_driverless_automation_browser_roots(self):
        self.assertEqual(wbt.orphaned_browsers(self.PROCS), {10: PROFILE, 40: LINUX_PROFILE})


class WrapperTest(unittest.TestCase):
    def run_wrapper(self, *argv, env=None, timeout=30):
        return subprocess.run([sys.executable, str(WRAPPER), *argv], env=env,
                              capture_output=True, text=True, timeout=timeout)

    def profiles(self, tmp, *names):
        paths = [os.path.join(tmp, f"org.chromium.Chromium.scoped_dir.{n}") for n in names]
        for p in paths:
            os.mkdir(p)
        return paths

    def test_passes_through_the_exit_status(self):
        self.assertEqual(self.run_wrapper("true").returncode, 0)
        self.assertEqual(self.run_wrapper("sh", "-c", "exit 3").returncode, 3)
        self.assertEqual(self.run_wrapper("sh", "-c", "kill -TERM $$").returncode, 143)
        self.assertEqual(self.run_wrapper("/nonexistent/command").returncode, 127)
        self.assertEqual(self.run_wrapper().returncode, 2)

    def test_a_signal_to_the_wrapper_stops_the_whole_command(self):
        for sig, rc in ((signal.SIGINT, 130), (signal.SIGTERM, 143), (signal.SIGHUP, 129)):
            with self.subTest(sig=sig), tempfile.TemporaryDirectory() as tmp:
                pidfile = os.path.join(tmp, "grandchild")
                code = ("import subprocess, sys; p = subprocess.Popen(['sleep', '60']); "
                        "open(sys.argv[1], 'w').write(str(p.pid)); p.wait()")
                w = subprocess.Popen([sys.executable, str(WRAPPER), sys.executable, "-c", code, pidfile],
                                     start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                grandchild = None
                try:
                    deadline = time.monotonic() + 20
                    while grandchild is None and time.monotonic() < deadline:
                        time.sleep(0.1)
                        try:
                            with open(pidfile) as f:
                                grandchild = int(f.read())
                        except (OSError, ValueError):
                            pass
                    self.assertIsNotNone(grandchild, "command never started")
                    os.killpg(w.pid, sig)
                    self.assertEqual(w.wait(timeout=10), rc)
                    deadline = time.monotonic() + 5
                    while grandchild in wbt.processes() and time.monotonic() < deadline:
                        time.sleep(0.1)
                    self.assertNotIn(grandchild, wbt.processes())
                finally:
                    if w.poll() is None:
                        w.kill()
                        w.wait()
                    if grandchild:
                        try:
                            os.kill(grandchild, signal.SIGKILL)
                        except ProcessLookupError:
                            pass

    def test_signals_a_new_orphan_that_reuses_a_killed_pid(self):
        sent, killed = [], {}
        with mock.patch.object(wbt, "in_session", return_value=True), \
                mock.patch.object(wbt.os, "kill", side_effect=lambda pid, sig: sent.append((pid, sig))):
            for profile in (PROFILE, PROFILE + "Next"):
                with mock.patch.object(wbt, "processes", return_value={7: (1, f"{MAC} {automation(profile)}")}):
                    wbt.reap(killed, 0)
        self.assertEqual(sent, [(7, signal.SIGTERM), (7, signal.SIGTERM)])
        self.assertEqual(set(killed), {PROFILE, PROFILE + "Next"})

    def test_kills_only_its_own_runs_orphan_while_the_runner_blocks_on_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            orphan, driven, bystander = self.profiles(tmp, "Orphan", "Driven", "Bystander")
            other = subprocess.Popen(["bash", "-c", 'exec -a "$0" sleep 60', fake_chrome(bystander)],
                                     start_new_session=True)
            try:
                r = self.run_wrapper(sys.executable, "-c", ORPHAN_BLOCKING_THE_RUNNER, env={
                    **os.environ, "ORPHAN": fake_chrome(orphan), "DRIVEN": fake_chrome(driven)})
                self.assertEqual(r.returncode, 1, r.stderr)
                self.assertIn("killed 1 orphaned Chrome", r.stderr)
                self.assertFalse(os.path.exists(orphan))
                self.assertTrue(live(driven))
                self.assertTrue(os.path.exists(driven))
                self.assertIsNone(other.poll())
                self.assertTrue(os.path.exists(bystander))
            finally:
                other.kill()
                other.wait()
                kill_all(driven)

    def test_reaps_an_orphan_left_at_exit_after_it_releases_its_profile(self):
        with tempfile.TemporaryDirectory() as tmp:
            orphan, = self.profiles(tmp, "LeftAtExit")
            try:
                r = self.run_wrapper(sys.executable, "-c", ORPHAN_LEFT_AT_EXIT,
                                     env={**os.environ, "ORPHAN": fake_chrome(orphan), "P": orphan,
                                          "GRACEFUL": GRACEFUL})
                self.assertEqual(r.returncode, 0, r.stderr)
                self.assertIn("killed 1 orphaned Chrome", r.stderr)
                self.assertEqual(live(orphan), [])
                self.assertTrue(os.path.exists(orphan + ".closed"))
                self.assertFalse(os.path.exists(orphan))
            finally:
                kill_all(orphan)

    def test_keeps_a_profile_a_live_process_still_names(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile, = self.profiles(tmp, "StillNamed")
            helper = subprocess.Popen(["bash", "-c", 'exec -a "$0" sleep 30',
                                       f"chrome --type=renderer --user-data-dir={profile}"])
            try:
                time.sleep(0.5)
                with mock.patch.object(wbt, "GRACE_SECS", 0.2), mock.patch.object(wbt, "POLL_SECS", 0.2):
                    wbt.finish({profile: 0.0}, -1)
                self.assertTrue(os.path.exists(profile))
            finally:
                helper.kill()
                helper.wait()

    def test_lists_a_process_whose_command_line_is_not_utf8(self):
        p = subprocess.Popen([b"bash", b"-c", b'exec -a "$0" sleep 30', b"bad\xff"])
        try:
            time.sleep(0.5)
            self.assertIn(p.pid, wbt.processes())
        finally:
            p.kill()
            p.wait()

    @unittest.skipIf(os.getuid() == 0, "pid 1 belongs to the caller")
    def test_lists_only_the_callers_processes(self):
        self.assertNotIn(1, wbt.processes())
        self.assertIn(os.getpid(), wbt.processes())


if __name__ == "__main__":
    unittest.main(verbosity=2)
