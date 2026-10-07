#!/usr/bin/env python3
"""Serves the UI image's nginx.conf from the UI image's nginx base and checks cache headers (#2936).

Needs docker. NGINX_CONF overrides the config under test.
"""
from __future__ import annotations

import http.client
import os
import re
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
NGINX_CONF = Path(os.environ.get("NGINX_CONF", ROOT / "nginx.conf")).resolve()
IMAGE = re.findall(r"^FROM\s+(\S+)", (ROOT / "Dockerfile.dioxus").read_text(), re.M)[-1]
CSP = "default-src 'self'"
IMMUTABLE = "public, max-age=31536000, immutable"

HASHED = [
    "/videocall-ui-e4d33ff03b411b60.js",
    "/videocall-ui-e4d33ff03b411b60_bg.wasm",
    "/tailwind-f395f3b8e8c05fa.css",
    "/wt_session_worker_loader-3ec17c702cb596f2.js",
    "/wt_session_worker-494b593f61580576.js",
    "/wt_session_worker_bg-c10b07eff2b31d87.wasm",
]
NO_STORE = ["/index.html", "/", "/meeting/abc"]
UNHASHED = [
    "/config.js",
    "/console-log-collector.js",
    "/recording.js",
    "/encoderWorker.min.js",
    "/static/style.css",
    "/snippets/videocall-ui-c6958005d5fa01bc/inline0.js",
]


def write(root: Path, rel: str, body: str) -> None:
    path = root / rel.lstrip("/")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body)


class NginxCacheHeadersTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if shutil.which("docker") is None:
            raise RuntimeError("docker is required for this test")
        cls.tmp = Path(tempfile.mkdtemp(dir=os.environ.get("RUNNER_TEMP")))
        html, csp = cls.tmp / "html", cls.tmp / "csp"
        cls.addClassCleanup(shutil.rmtree, cls.tmp, True)
        for rel in HASHED + UNHASHED + ["/index.html"]:
            write(html, rel, f"body of {rel}\n")
        write(csp, "csp.conf", f'add_header Content-Security-Policy "{CSP}" always;\n')
        for dirpath, _, files in os.walk(cls.tmp):
            os.chmod(dirpath, 0o755)
            for name in files:
                os.chmod(os.path.join(dirpath, name), 0o644)
        mounts = ["-v", f"{NGINX_CONF}:/etc/nginx/nginx.conf:ro",
                  "-v", f"{csp}:/etc/nginx/csp:ro",
                  "-v", f"{html}:/usr/share/nginx/html:ro"]
        check = subprocess.run(["docker", "run", "--rm", *mounts, IMAGE, "nginx", "-t"],
                               capture_output=True, text=True)
        if check.returncode != 0:
            raise RuntimeError(f"nginx -t failed:\n{check.stdout}{check.stderr}")
        cls.container = subprocess.run(
            ["docker", "run", "-d", "--rm", "-p", "127.0.0.1::80", *mounts,
             IMAGE, "timeout", "300", "nginx", "-g", "daemon off;"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        cls.addClassCleanup(subprocess.run, ["docker", "rm", "-f", cls.container], capture_output=True)
        port = subprocess.run(
            ["docker", "port", cls.container, "80/tcp"],
            check=True, capture_output=True, text=True,
        ).stdout.splitlines()[0].rsplit(":", 1)[1]
        cls.port = int(port)
        deadline = time.monotonic() + 30
        while True:
            try:
                cls.get("/index.html")
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.2)

    @classmethod
    def get(cls, path, headers=None):
        conn = http.client.HTTPConnection("127.0.0.1", cls.port, timeout=5)
        try:
            conn.request("GET", path, headers=headers or {})
            resp = conn.getresponse()
            resp.read()
            return resp.status, resp.headers
        finally:
            conn.close()

    def assert_csp(self, path, headers):
        self.assertEqual(headers.get("Content-Security-Policy"), CSP, path)

    def test_hashed_assets_are_immutable(self):
        for path in HASHED:
            with self.subTest(path=path):
                status, headers = self.get(path)
                self.assertEqual(status, 200)
                self.assertEqual(headers.get_all("Cache-Control"), [IMMUTABLE])
                self.assertIsNone(headers.get("Pragma"))
                self.assert_csp(path, headers)
                etag = headers.get("ETag")
                self.assertIsNotNone(etag)
                status, headers = self.get(path, {"Range": "bytes=999999-"})
                self.assertEqual(status, 200)
                self.assertEqual(headers.get_all("Cache-Control"), [IMMUTABLE])
                status, headers = self.get(path, {"If-None-Match": etag})
                self.assertEqual(status, 304)
                self.assertEqual(headers.get_all("Cache-Control"), [IMMUTABLE])

    def test_index_html_is_not_stored(self):
        for path in NO_STORE:
            with self.subTest(path=path):
                status, headers = self.get(path)
                self.assertEqual(status, 200)
                self.assertEqual(headers.get_all("Cache-Control"), ["no-store"])
                self.assert_csp(path, headers)

    def test_unhashed_assets_revalidate(self):
        for path in UNHASHED:
            with self.subTest(path=path):
                status, headers = self.get(path)
                self.assertEqual(status, 200)
                self.assertEqual(headers.get_all("Cache-Control"), ["no-cache"])
                self.assert_csp(path, headers)
                etag = headers.get("ETag")
                self.assertIsNotNone(etag)
                status, headers = self.get(path, {"If-None-Match": etag})
                self.assertEqual(status, 304)
                self.assert_csp(path, headers)

    def test_missing_hashed_asset_is_an_uncached_404(self):
        path = "/videocall-ui-0123456789abcdef.js"
        status, headers = self.get(path)
        self.assertEqual(status, 404)
        self.assertEqual(headers.get_all("Cache-Control"), ["no-cache"])
        self.assert_csp(path, headers)


if __name__ == "__main__":
    unittest.main()
