#!/usr/bin/env python3
# `make e2e-up` clears e2e/.stack-stamps/, which the container created as root,
# so on Linux the clear can fail and must not abort the recipe (#2513). The primary
# cases stub `rm` rather than using file modes: root ignores modes, so a
# mode-based test would pass under a root CI runner either way.
from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SENTINEL = "COMPOSE_STUB_RAN"
FAILING_RM = "#!/usr/bin/env bash\necho 'rm: cannot remove: Permission denied' >&2\nexit 1\n"


class E2eUpStampClearTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="e2e-up-test-"))
        self.addCleanup(self._cleanup)
        shutil.copy2(REPO / "Makefile", self.root / "Makefile")
        (self.root / "scripts").mkdir()
        self._script("scripts/regen-dev-cert.sh", "#!/usr/bin/env bash\nexit 0\n")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.stamps = self.root / "e2e" / ".stack-stamps"
        self.stamps.mkdir(parents=True)
        (self.stamps / "websocket_server.json").write_text("{}")

    def _cleanup(self) -> None:
        if self.stamps.exists():
            self.stamps.chmod(0o755)
        shutil.rmtree(self.root, ignore_errors=True)

    def _script(self, rel: str, body: str) -> None:
        path = self.root / rel
        path.write_text(body)
        path.chmod(0o755)

    def _make(self, target: str = "e2e-up", stub_rm: bool = False):
        env = dict(os.environ)
        if stub_rm:
            self._script("bin/rm", FAILING_RM)
            env["PATH"] = f"{self.bin}:{env['PATH']}"
        return subprocess.run(
            ["make", target, f"COMPOSE_E2E=echo {SENTINEL}"],
            cwd=self.root,
            capture_output=True,
            text=True,
            env=env,
            timeout=120,
        )

    def _assert_brought_the_stack_up(self, proc) -> None:
        detail = proc.stdout + proc.stderr
        self.assertEqual(proc.returncode, 0, detail)
        # Exit 0 alone would also pass a recipe that silently skipped compose.
        self.assertIn(SENTINEL, proc.stdout, detail)

    def test_a_failing_clear_does_not_abort_the_recipe(self) -> None:
        self._assert_brought_the_stack_up(self._make(stub_rm=True))

    def test_a_failing_clear_does_not_abort_the_impair_recipe(self) -> None:
        self._assert_brought_the_stack_up(self._make("e2e-up-impair", stub_rm=True))

    @unittest.skipIf(os.geteuid() == 0, "root ignores the permission bits this relies on")
    def test_an_unwritable_stamp_dir_does_not_abort_the_recipe(self) -> None:
        self.stamps.chmod(0o555)
        self._assert_brought_the_stack_up(self._make())

    def test_a_clearable_stamp_dir_is_still_actually_cleared(self) -> None:
        self._assert_brought_the_stack_up(self._make())
        self.assertFalse(self.stamps.exists())


if __name__ == "__main__":
    unittest.main()
