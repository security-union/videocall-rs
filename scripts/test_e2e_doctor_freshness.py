#!/usr/bin/env python3
# Regression tests for the backend-freshness section of e2e-doctor.sh.
#
# `stat -f %m` reads %m as a FILE on GNU and prints a filesystem report, which
# reaching $(( )) aborts the whole script under `set -u` — taking the collected
# summary with it and skipping the staleness case entirely.
from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("e2e-doctor.sh")

COMPOSE = "\n".join(
    [
        "services:",
        "  websocket-api:",
        "    command: >",
        "      nix develop /app#backend-dev --command bash -c",
        '      "/app/docker/e2e-backend.sh supervise websocket_server"',
        "",
    ]
)

# Mimics GNU `stat -f %m <file>`: multi-line output whose first word is a bare
# identifier, which turns $(( now - $x )) into an "unbound variable". Exits 0,
# so the `||` branch never runs and both orderings see the same garbage.
STAT_STUB = "\n".join(
    [
        "#!/usr/bin/env bash",
        "echo '  File: \"stub\"'",
        "echo '    ID: 0        Namelen: 255'",
        "exit 0",
        "",
    ]
)

NOOP_STUB = "#!/usr/bin/env bash\nexit 1\n"


class DoctorFreshnessTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="e2e-doctor-test-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        (self.root / "scripts").mkdir()
        (self.root / "docker").mkdir()
        (self.root / "e2e" / ".stack-stamps").mkdir(parents=True)
        shutil.copy2(SCRIPT, self.root / "scripts" / SCRIPT.name)
        (self.root / "docker" / "docker-compose.e2e.yaml").write_text(COMPOSE)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for tool in ("docker", "curl", "openssl"):
            self._stub(tool, NOOP_STUB)

    def _stub(self, name: str, body: str) -> None:
        path = self.bin / name
        path.write_text(body)
        path.chmod(0o755)

    def _write_stamp(self, build: str = "ok", age_secs: int = 0) -> None:
        stamp = self.root / "e2e" / ".stack-stamps" / "websocket_server.json"
        stamp.write_text('{"service":"websocket_server","build":"%s","at":"x"}\n' % build)
        if age_secs:
            when = time.time() - age_secs
            os.utime(stamp, (when, when))

    def _run(self) -> str:
        env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}")
        proc = subprocess.run(
            ["bash", str(self.root / "scripts" / SCRIPT.name)],
            capture_output=True,
            text=True,
            env=env,
            timeout=120,
        )
        return proc.stdout + proc.stderr

    def test_stale_stamp_is_reported_as_unsupervised(self) -> None:
        self._write_stamp(build="ok", age_secs=3600)
        self.assertIn("no watcher is supervising it", self._run())

    def test_non_numeric_mtime_does_not_abort_the_script(self) -> None:
        self._write_stamp(build="ok", age_secs=3600)
        self._stub("stat", STAT_STUB)
        out = self._run()
        self.assertNotIn("unbound variable", out)
        self.assertIn("no watcher is supervising it", out)

    def test_report_reaches_its_summary(self) -> None:
        self._write_stamp(build="ok", age_secs=3600)
        self.assertIn("== Summary ==", self._run())

    def test_fresh_ok_stamp_is_not_called_stale(self) -> None:
        self._write_stamp(build="ok", age_secs=0)
        out = self._run()
        self.assertIn("supervised, last build ok", out)
        self.assertNotIn("no watcher is supervising it", out)

    def test_failed_build_is_reported(self) -> None:
        self._write_stamp(build="failed", age_secs=0)
        self.assertIn("last build FAILED", self._run())

    def test_unreadable_stamp_is_not_reported_as_a_good_build(self) -> None:
        stamp = self.root / "e2e" / ".stack-stamps" / "websocket_server.json"
        stamp.write_text("{ truncated")
        out = self._run()
        self.assertIn("unreadable", out)
        self.assertNotIn("supervised, last build ok", out)

    def test_missing_stamp_warns_rather_than_failing(self) -> None:
        self.assertIn("no build stamp", self._run())

    def test_compose_without_a_supervisor_is_reported(self) -> None:
        (self.root / "docker" / "docker-compose.e2e.yaml").write_text(
            '  websocket-api:\n    command: "cargo run --bin websocket_server"\n'
        )
        self.assertIn("no supervised backends found", self._run())


if __name__ == "__main__":
    unittest.main()
