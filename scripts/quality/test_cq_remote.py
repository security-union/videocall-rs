#!/usr/bin/env python3
"""Tests for the remote adapters (#2914 PR-8b), against a stub podman, kubectl, curl and git on PATH."""

import base64
import calendar
import contextlib
import gzip
import io
import json
import math
import os
import re
import select
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import unittest.mock
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cq_collect  # noqa: E402
import cq_local  # noqa: E402
import cq_remote  # noqa: E402
import cq_runner  # noqa: E402
import cq_scenario  # noqa: E402
import cq_target_key  # noqa: E402
import scenario_run  # noqa: E402
import test_cq_shared_target as tst  # noqa: E402
import test_scenario_run as tsr  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(HERE))
STUB = os.path.join(HERE, "testdata", "stub-remote.py")
S0 = os.path.join(HERE, "scenarios", "phase0", "s0.yaml")
COMMITTED = os.path.join(cq_remote.PROFILE_DIR, "hcl-daily.json")
DEPLOY_FILES = (".github/workflows/daily-deploy-hcl.yaml", "helm/util/hcl-daily/prometheus-ingress/values.yaml")
DAILY_HOSTS = sorted({urllib.parse.urlsplit(v).hostname for k, v in cq_remote.load_profile("hcl-daily").items()
                      if k.endswith(("_url", "_server"))}) if os.path.exists(COMMITTED) else []
RUN_ID = "p0s0-20261014t0830-0123abc"
DRAIN_BLIND = ("the drain could not be confirmed for 90 s: no Prometheus answer, or a relay target down "
               "or missing")
ROOM = f"scale-{RUN_ID}"
SECRET = 'd4ily?ke>/key+with=odd é "chars"'
T0830 = calendar.timegm((2026, 10, 14, 8, 30, 0, 0, 0, 0))
PROFILE = {"schema": "cq-target/v1", "target": "hcl-daily", "ui_url": "https://ui.test.invalid",
           "api_url": "https://api.test.invalid", "ws_url": "wss://relay.test.invalid",
           "prom_url": "https://prom.test.invalid", "cookie_name": "test-session",
           "kube_server": "https://192.0.2.10:6443", "kube_context": None, "kubeconfig": "~/kc-unused",
           "namespace": "vc",
           "pods": {"relay_ws": "ws-relay-.*", "relay_wt": "wt-relay-.*", "ui": "ui-.*", "nats": "nats-[0-9]+",
                    "metrics_api": "metrics-.*"},
           "dev_secret": "dev-jwt-secret-change-me",
           "probe_cores": {"camera_off": 3.0, "camera_off_mic_on": 3.2, "camera_on": None}}
WINDOW = ["06:00", "10:00"]
POD_NAMES = {"relay_ws": "ws-relay-7f9c", "relay_wt": "wt-relay-2b1a", "ui": "ui-5d4e", "nats": "nats-0",
             "metrics_api": "metrics-6c3b"}
HEALTHY = [[{"cq": "relay_up_min"}, "1"], [{"cq": "relay_up_count"}, "2"], [{"cq": "slow_consumers"}, "4"]]
MUTATING = (["apply", "-f", "x.yaml"], ["delete", "pod", "x"], ["create", "ns", "x"], ["edit", "deploy", "x"],
            ["patch", "deploy", "x"], ["replace", "-f", "x"], ["scale", "deploy", "x"], ["rollout", "restart", "x"],
            ["exec", "x", "--", "sh"], ["port-forward", "x", "8222"], ["cp", "x:/a", "b"], ["attach", "x"],
            ["run", "x", "--image", "y"], ["label", "pod", "x", "a=b"], ["annotate", "pod", "x", "a=b"],
            ["set", "image", "deploy/x", "c=y"], ["drain", "n"], ["cordon", "n"], ["taint", "n", "k=v:NoSchedule"],
            ["debug", "x"], ["expose", "deploy", "x"], ["proxy"], ["config", "set-context", "x"],
            ["config", "use-context", "x"], ["config", "set", "a", "b"], ["config", "delete-context", "x"],
            ["auth", "reconcile", "-f", "x"], ["certificate", "approve", "x"], ["-n", "kube-system", "delete", "x"],
            ["config", "view", "--raw"], ["get", "secret", "--raw", "/api"], ["get", "--raw=/api/v1/secrets"],
            ["config", "view", "--raw=true"], ["config", "view", "--flatten=true"], ["get", "secrets", "-o", "yaml"],
            ["get", "pods", "-o", "json", "--user=exec-user"], ["get", "pods", "-o", "json", "--context=other"],
            ["get", "pods", "-o", "json", "--as", "x"], ["get", "pods"], ["config", "view"],
            ["config", "view", "--minify", "-o", "json", "--raw"], ["kubectl-get", "pods", "-o", "json"])


def pod(kind, started="2026-10-14T07:00:00Z", ready=True, restarts=0):
    return {"metadata": {"name": POD_NAMES[kind], "uid": "u-1"},
            "spec": {"nodeName": "worker-3.internal"},
            "status": {"phase": "Running", "hostIP": "198.51.100.7", "podIP": "198.51.100.9", "startTime": started,
                       "containerStatuses": [{"name": kind, "ready": ready, "restartCount": restarts,
                                              "image": f"registry.internal.example:5000/team/{kind}:2026-10-14",
                                              "imageID": f"registry.internal.example:5000/team/{kind}@sha256:"
                                                         + "c" * 64,
                                              "state": {"running": {"startedAt": started}}}]}}


def target(kind, health="up", interval="15s"):
    return {"labels": {"namespace": "vc", "pod": POD_NAMES[kind], "job": "kubernetes-pods",
                       "instance": "198.51.100.9:9090"}, "health": health, "scrapeInterval": interval}


def fake_jwt(sub):
    def seg(raw):
        return base64.urlsafe_b64encode(raw).rstrip(b"=")

    def compact(obj):
        return json.dumps(obj, separators=(",", ":")).encode()
    return b".".join((seg(compact({"alg": "HS256"})), seg(compact({"sub": sub})), seg(b"signature-bytes-12")))


def kubeconfig_view(context="daily", **cluster):
    return {"current-context": context, "clusters": [{"cluster": dict(
        {"server": PROFILE["kube_server"], "certificate-authority-data": "DATA+OMITTED"}, **cluster)}],
        "users": [{"user": {"client-certificate-data": "DATA+OMITTED", "client-key-data": "DATA+OMITTED"}}]}


def world(**over):
    w = {"secret": SECRET, "prom_time": T0830 + 0.2, "qdisc": tst_qdisc(),
         "targets": [target(k) for k in cq_remote.SCRAPED_KINDS],
         "pods": [pod(k) for k in cq_remote.POD_KINDS],
         "kubeconfig_view": kubeconfig_view(),
         "prom": [["label_replace(", HEALTHY], ["[1h]", []], ["[7d]", [[{}, "5"]]],
                  ["videocall_peer_can_listen", [[{}, "3"]]]],
         "bot_labels": "f" * 40 + "\tclean",
         "ctl": {"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}}}
    w.update(over)
    return w


def tst_qdisc():
    return [{"dev": "lo", "kind": "noqueue", "parent": 0xFFFFFFFF}, {"dev": "eth0", "kind": "mq", "parent": 0xFFFFFFFF}]


def two_probes():
    """S0 with two probes, as it was before it went to one, and a join window that fits the owner wait."""
    with open(S0, encoding="utf-8") as fh:
        text = fh.read()
    for old, new in (("count: 1, media: {camera: off", "count: 2, media: {camera: off"),
                     ("join_window: 6m", "join_window: 780s")):
        assert text.count(old) == 1, old
        text = text.replace(old, new)
    return text


def n_probes(probes, window, stagger=4):
    """S0 with `probes` camera-off probes, a join window of `window` seconds and a join stagger of `stagger`."""
    with open(S0, encoding="utf-8") as fh:
        text = fh.read().replace("count: 1, media: {camera: off", f"count: {probes}, media: {{camera: off")
    text = text.replace("join_stagger: 4s", f"join_stagger: {stagger}s")
    return plan_of(text=text.replace("join_window: 6m", f"join_window: {window}s"))


def mic_on_pair():
    """Two probes, both camera off: probe-00 muted and probe-01 talking."""
    text = two_probes().replace("count: 2, media: {camera: off", "count: 1, media: {camera: off")
    role = "  - {role: probes, fleet: browser,"
    talker = ("  - {role: talking-probes, fleet: browser, count: 1, media: {camera: off, mic: continuous}, "
              "transport: websocket, decode_budget: client}\n")
    assert text.count(role) == 1
    return text.replace("  - {role: talkers, fleet: rust", talker + "  - {role: talkers, fleet: rust")


def plan_of(path=S0, run_id=RUN_ID, text=None):
    if text is None:
        with open(path, encoding="utf-8") as fh:
            text = fh.read()
    return cq_scenario.compile_scenario(cq_scenario.parse_yaml(text), run_id=run_id, scenario_file="s0.yaml",
                                        scenario_sha256="0" * 64)


class FakeTime:
    def __init__(self, wall=T0830):
        self.wall, self.mono, self.slept = wall, 5000.0, []

    def now(self):
        return self.wall

    def monotonic(self):
        return self.mono

    def sleep(self, seconds):
        if seconds < 0:
            raise ValueError("sleep length must be non-negative")
        self.slept.append(seconds)
        self.advance(seconds)

    def advance(self, seconds):
        self.wall += seconds
        self.mono += seconds


class FakeCpu:
    """/proc/stat of an 8-CPU host whose busy share is `pct` from the last read on."""

    def __init__(self, clock, pct=20.0):
        self.clock, self.pct, self.total, self.idle, self.at = clock, pct, 0.0, 0.0, None

    def __call__(self):
        if self.at is not None:
            ticks = (self.clock.mono - self.at) * 800
            self.total += ticks
            self.idle += ticks * (1 - self.pct / 100)
        self.at = self.clock.mono
        return int(self.total), int(self.idle)


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="cq-remote-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        bin_dir = os.path.join(self.tmp, "bin")
        os.makedirs(bin_dir)
        for prog in ("podman", "kubectl", "curl", "git", "timedatectl", "loginctl"):
            os.symlink(STUB, os.path.join(bin_dir, prog))
        self.world_path, self.log_path = os.path.join(self.tmp, "world.json"), os.path.join(self.tmp, "log.jsonl")
        self.set_world(world())
        self.kubeconfig = os.path.join(self.tmp, "kube", "config")
        os.makedirs(os.path.dirname(self.kubeconfig))
        with open(self.kubeconfig, "w", encoding="utf-8") as fh:
            fh.write("apiVersion: v1\n")
        os.chmod(self.kubeconfig, 0o600)
        env = {"PATH": bin_dir + os.pathsep + os.environ["PATH"], "STUB_WORLD": self.world_path,
               "STUB_LOG": self.log_path, "KUBECONFIG": "/elsewhere/config", "JWT_SECRET": SECRET,
               "CQ_TARGET_CONFIRM": "hcl-daily", "CQ_KUBECONFIG": self.kubeconfig, "TMUX": "/tmp/tmux-test,1,0"}
        patcher = unittest.mock.patch.dict(os.environ, env)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.profile_path = os.path.join(self.tmp, "profile.json")
        with open(self.profile_path, "w", encoding="utf-8") as fh:
            json.dump(PROFILE, fh)
        self.root = self.checkout()
        self.run_dir = os.path.join(self.tmp, "runs", RUN_ID)
        os.makedirs(self.run_dir)
        self.plan = plan_of(text=two_probes())
        self.make()

    def make(self, plan=None):
        os.environ[cq_runner.SECRET_ENV] = SECRET
        self.stack, self.fleet = cq_remote.factory(plan or self.plan, self.run_dir, "reuse",
                                                   profile_path=self.profile_path)
        c = self.ctx = self.stack.ctx
        self.clock = FakeTime()
        self.cpu = FakeCpu(self.clock)
        c.root, c.now, c.mono, c.sleep, c.cpu = self.root, self.clock.now, self.clock.monotonic, self.clock.sleep, \
            self.cpu
        c.ncpu = lambda: 8
        c.fs, c.port_open = (lambda path: "tmpfs"), (lambda port: False)
        c.secret_parent = os.path.join(self.tmp, "run-user")
        os.makedirs(c.secret_parent, exist_ok=True)
        c.load = cq_remote.LoadLine(self.cpu)
        self.fleet.sampler = unittest.mock.Mock(poll=lambda: 0)

    def checkout(self):
        root = os.path.join(self.tmp, "checkout")
        for rel in ("Dockerfile.bot", cq_local.SAMPLER, os.path.join("e2e", cq_local.TSX),
                    os.path.join("e2e", cq_local.CLI), cq_remote.PRELOAD, cq_remote.BROWSER_ENV, cq_local.TALKER_WAV):
            os.makedirs(os.path.dirname(os.path.join(root, rel)), exist_ok=True)
            src = os.path.join(ROOT, rel)
            if rel in (cq_remote.BROWSER_ENV, cq_local.TALKER_WAV, cq_remote.PRELOAD):
                shutil.copyfile(src, os.path.join(root, rel))
            else:
                open(os.path.join(root, rel), "w", encoding="utf-8").close()
        return root

    def set_world(self, w):
        with open(self.world_path, "w", encoding="utf-8") as fh:
            json.dump(w, fh)

    def get_world(self):
        with open(self.world_path, encoding="utf-8") as fh:
            return json.load(fh)

    def update(self, **kw):
        w = self.get_world()
        w.update(kw)
        self.set_world(w)

    def calls(self, prog=None):
        if not os.path.exists(self.log_path):
            return []
        with open(self.log_path, encoding="utf-8") as fh:
            rows = [json.loads(x) for x in fh]
        return [r for r in rows if prog is None or r["prog"] == prog]

    def podman_runs(self):
        return [r["argv"] for r in self.calls("podman") if r["argv"][:2] == ["run", "-d"]]

    def proc(self, pid):
        return next(p for p in self.plan["processes"] if p["proc_id"] == pid)

    def launch_all(self):
        self.fleet.clock_facts()
        self.assertEqual(self.stack.preflight(self.plan), [])
        for p in self.plan["processes"]:
            self.fleet.launch(p)

    def write_joins(self, marker=True):
        for p in self.plan["processes"]:
            if p["fleet"] == "rust":
                path = os.path.join(self.run_dir, "rust", f"participants-{p['proc_id']}.json")
                with open(path, "w", encoding="utf-8") as fh:
                    json.dump({"media_started_at": T0830 + 20, "participants": [
                        {"user_id": u, "join_ts": T0830 + 10} for u in p["participants"]]}, fh)
            else:
                out = os.path.join(self.run_dir, "probes", str(p["probe_index"]))
                os.makedirs(os.path.join(out, "participants"), exist_ok=True)
                with open(os.path.join(out, "participants", "bot.json"), "w", encoding="utf-8") as fh:
                    json.dump({"participant": {"user_id": p["participants"][0], "join_ts": T0830 + 5}}, fh)
                if marker:
                    open(os.path.join(out, cq_remote.KEY_READ), "w", encoding="utf-8").close()


class PreflightOnly(Base):

    def preflight_only(self, text):
        scenario = os.path.join(self.tmp, "s.yaml")
        with open(scenario, "w", encoding="utf-8") as fh:
            fh.write(text)
        out_dir = os.path.join(self.tmp, "runs-out")

        def factory(plan, run_dir, mode):
            self.plan, self.run_dir = plan, run_dir
            self.make(plan)
            return self.stack, self.fleet
        err, out = io.StringIO(), io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(out):
            rc = scenario_run.main(["--scenario", scenario, "--target", "hcl-daily", "--stack", "reuse", "--out-dir",
                                    out_dir, "--preflight-only"], adapters={"hcl-daily": factory},
                                   tree=tsr.fakes.FakeTree(tsr.fakes.FakeClock(), self.root, commit="f" * 40),
                                   clock=tsr.fakes.FakeClock(wall=T0830))
        self.assertFalse(os.path.exists(out_dir))
        self.assertFalse(os.path.exists(self.run_dir))
        return rc, out.getvalue(), err.getvalue()

    def test_preflight_only_makes_the_remote_checks_and_starts_no_bot_or_probe(self):
        self.assertEqual(self.calls(), [])
        rc, out, err = self.preflight_only(two_probes())
        self.assertEqual(rc, 0, err)
        self.assertIn("preflight passed; nothing was launched", out)
        podman = [r["argv"] for r in self.calls("podman")]
        self.assertIn(["image", "inspect", cq_local.PW_IMAGE], podman)
        runs = [a for a in podman if a[0] == "run"]
        self.assertEqual([a[a.index("--entrypoint") + 1:a.index("--entrypoint") + 3] for a in runs],
                         [["sh", "videocall-bot:cq-ffffffffffff"], ["python3", cq_local.PW_IMAGE]])
        self.assertTrue(all("--rm" in a and "-d" not in a for a in runs), runs)
        self.assertFalse([a for a in runs if {"--config", "--meeting-url", cq_local.CLI} & set(a)])
        self.assertFalse([a for a in podman if a[0] in ("start", "create", "exec", "kill", "stop", "rm")])
        self.assertFalse([r for r in self.calls("kubectl") if not {"get", "config"} & set(r["argv"])])
        self.assertEqual(os.listdir(self.ctx.secret_parent), [])

    def test_preflight_only_lists_the_remote_problems_and_exits_3(self):
        self.update(bot_labels="0" * 40 + "\tclean")
        rc, out, err = self.preflight_only(two_probes().replace("join_window: 780s", "join_window: 625s"))
        self.assertEqual((rc, out), (3, ""))
        self.assertIn("\n  - no bot image videocall-bot:cq-ffffffffffff built from this checkout", err)
        self.assertIn("\n  - the join's worst case takes 746.0 s, more than the join window plus 120 s: raise "
                      "steps[0].join_window to at least 626 s (Rust bots launched and joined, media started 90 s;", err)
        self.assertFalse([r for r in self.calls("podman") if r["argv"][:2] == ["run", "-d"]])


def _openssl(*args):
    subprocess.run(["openssl", *args], check=True, capture_output=True)


def _make_pki(d):
    """A CA, a server certificate for 127.0.0.1, a client certificate, and a second CA, in folder `d`."""
    for ca in ("ca", "other-ca"):
        _openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", f"/CN={ca}",
                 "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign",
                 "-keyout", os.path.join(d, f"{ca}.key"), "-out", os.path.join(d, f"{ca}.crt"))
    for name, ext in (("server", "subjectAltName=IP:127.0.0.1"), ("client", "extendedKeyUsage=clientAuth")):
        with open(os.path.join(d, f"{name}.ext"), "w", encoding="utf-8") as fh:
            fh.write(f"{ext}\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n")
        _openssl("req", "-newkey", "rsa:2048", "-nodes", "-subj", f"/CN={name}", "-keyout",
                 os.path.join(d, f"{name}.key"), "-out", os.path.join(d, f"{name}.csr"))
        _openssl("x509", "-req", "-days", "2", "-in", os.path.join(d, f"{name}.csr"), "-CA", os.path.join(d, "ca.crt"),
                 "-CAkey", os.path.join(d, "ca.key"), "-CAcreateserial", "-extfile", os.path.join(d, f"{name}.ext"),
                 "-out", os.path.join(d, f"{name}.crt"))


HARDEN = cq_target_key.harden


@unittest.skipUnless(shutil.which("openssl"), "needs openssl to make a test certificate authority")
class TargetKey(Base):
    """cq_target_key.py against stub kubectl views and a real local HTTPS API server."""

    @classmethod
    def setUpClass(cls):
        import http.server
        import ssl
        import threading
        cls.pki = tempfile.mkdtemp(prefix="cq-key-pki-")
        _make_pki(cls.pki)

        def b64(v):
            return base64.b64encode(v.encode()).decode()
        cls.secrets = {"jwt-secret": b64(SECRET), "short": b64("abc"), "dev": b64(PROFILE["dev_secret"]), "blank": "",
                       "nul": b64("key-with\x00nul"), "lf": b64("key-ends-in-lf\n"), "cr": b64("key\rwith-cr"),
                       "del": b64("key-with\x7fdel"), "non-ascii": "a2V5LXZhbHVl\u00e9"}
        cls.seen, cls.redirected = [], []

        class Sink(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                cls.redirected.append((self.path, self.headers.get("Authorization")))
                body = json.dumps({"data": {"secret": cls.secrets["jwt-secret"]}}).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a):
                pass

        class Api(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                name = self.path.rsplit("/", 1)[-1]
                if name in cls.scripts:
                    return cls.scripts[name](self)
                peer = self.connection.getpeercert()
                cls.seen.append((self.path, self.headers.get("Authorization"),
                                 dict(x[0] for x in peer["subject"]).get("commonName") if peer else None))
                name = self.path.rsplit("/", 1)[-1]
                authed = self.headers.get("Authorization") == "Bearer api-token" or peer
                if not authed or not self.path.startswith("/api/v1/namespaces/vc/secrets/") or name not in cls.secrets:
                    self.send_response(404 if authed else 401)
                    self.end_headers()
                    self.wfile.write(b"the body names " + SECRET.encode())
                    return
                body = json.dumps({"data": {"secret": cls.secrets[name]}}).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a):
                pass
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(os.path.join(cls.pki, "server.crt"), os.path.join(cls.pki, "server.key"))
        tls.load_verify_locations(os.path.join(cls.pki, "ca.crt"))
        tls.verify_mode = ssl.CERT_OPTIONAL
        cls.servers = []
        for handler, wrap in ((Api, True), (Sink, True), (Sink, False)):
            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
            server.socket = tls.wrap_socket(server.socket, server_side=True) if wrap else server.socket
            server.handle_error = lambda *a: None
            threading.Thread(target=server.serve_forever, daemon=True).start()
            cls.servers.append(server)
        cls.server = cls.servers[0]
        cls.url = f"https://127.0.0.1:{cls.server.server_address[1]}"
        sink_https, sink_http = (f"{scheme}://127.0.0.1:{s.server_address[1]}"
                                 for scheme, s in (("https", cls.servers[1]), ("http", cls.servers[2])))

        def raw(data, pause=0.0):
            def send(h):
                for piece in data:
                    h.wfile.write(piece)
                    h.wfile.flush()
                    time.sleep(pause)
            return send

        def redirect(to):
            def send(h):
                h.send_response(302)
                h.send_header("Location", to + "/api/v1/namespaces/vc/secrets/jwt-secret")
                h.end_headers()
            return send
        cls.scripts = {
            "redirect-https": redirect(sink_https), "redirect-http": redirect(sink_http),
            "bad-status": raw([b"HTTP/1.1 2O0 CANARY-KEY\r\n\r\n"]),
            "bad-header": raw([b"HTTP/1.1 200 OK\r\nX: " + b"CANARY-KEY" * 8000 + b"\r\n\r\n"]),
            "bad-chunk": raw([b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nZZ CANARY-KEY\r\n"]),
            "huge": raw([b"HTTP/1.1 200 OK\r\nContent-Length: 3000000\r\n\r\n"] + [b"CANARY-KEY" * 100] * 3000),
            "trickle": raw([b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n"] + [b"C"] * 100, pause=0.1),
            "trickle-head": raw([b"HTTP/1.1 200 OK\r\n"] + [b"X"] * 200, pause=0.05),
        }

    @classmethod
    def tearDownClass(cls):
        for server in cls.servers:
            server.shutdown()
            server.server_close()
        shutil.rmtree(cls.pki, True)

    def pem(self, name):
        with open(os.path.join(self.pki, name), "rb") as fh:
            return base64.b64encode(fh.read()).decode()

    def setUp(self):
        super().setUp()
        self.seen.clear()
        self.redirected.clear()
        self.raw(user={"token": "api-token"})
        self.update(kubeconfig_view=kubeconfig_view(server=self.url))
        self.hardened = []
        self.parent = os.path.join(self.tmp, "run-user-key")
        os.makedirs(self.parent)
        os.chmod(self.parent, 0o700)
        for patcher in (unittest.mock.patch.object(cq_target_key, "harden",
                                                   lambda: self.hardened.append(len(self.seen)) or True),
                        unittest.mock.patch.object(cq_target_key, "KEY_PARENT", self.parent),
                        unittest.mock.patch.object(cq_remote, "fs_type", lambda path: "tmpfs")):
            patcher.start()
            self.addCleanup(patcher.stop)

    def raw(self, user, ca="ca.crt"):
        self.update(kubeconfig_raw={"clusters": [{"cluster": {"server": self.url,
                                                              "certificate-authority-data": self.pem(ca)}}],
                                    "users": [{"user": user}]})

    def key(self, secret="jwt-secret", **profile):
        path = os.path.join(self.tmp, "key-profile.json")
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(dict(PROFILE, kube_server=self.url, **profile), fh)
        return cq_target_key.read_key("hcl-daily", secret, "secret", profile_path=path)

    def test_the_key_is_read_over_verified_tls_in_this_process_with_the_checked_context(self):
        self.assertEqual(self.key(), SECRET)
        self.assertEqual(self.hardened, [0])
        self.assertEqual(self.seen, [("/api/v1/namespaces/vc/secrets/jwt-secret", "Bearer api-token", None)])
        views = [r["argv"] for r in self.calls("kubectl")]
        self.assertEqual([a[:3] for a in views], [["config", "view", "--minify"], ["config", "view", "--raw"]])
        self.assertEqual(views[1][views[1].index("--context") + 1], "daily")
        copy = views[1][views[1].index("--kubeconfig") + 1]
        self.assertNotEqual(copy, self.kubeconfig)
        self.assertEqual(os.path.dirname(os.path.dirname(copy)), self.parent)
        self.assertEqual(os.listdir(self.parent), [])

    def test_a_client_certificate_authenticates_and_proxies_are_ignored(self):
        self.raw(user={"client-certificate-data": self.pem("client.crt"), "client-key-data": self.pem("client.key")})
        proxy = "http://127.0.0.1:9"
        with unittest.mock.patch.dict(os.environ, {"HTTPS_PROXY": proxy, "https_proxy": proxy}):
            self.assertEqual(self.key(), SECRET)
        self.assertEqual(self.seen, [("/api/v1/namespaces/vc/secrets/jwt-secret", None, "client")])

    def test_a_server_certificate_from_another_ca_is_refused(self):
        self.raw(user={"token": "api-token"}, ca="other-ca.crt")
        with self.assertRaisesRegex(cq_runner.AdapterError, "reading jwt-secret/secret in namespace vc failed$"):
            self.key()
        self.assertEqual(self.seen, [])

    def test_the_kubeconfig_is_read_once_so_a_swap_after_the_checks_is_not_used(self):
        with open(self.kubeconfig, encoding="utf-8") as fh:
            checked = fh.read()
        check = cq_remote.Remote.kubeconfig_problems

        def check_then_swap(ctx):
            problems = check(ctx)
            os.remove(self.kubeconfig)
            with open(self.kubeconfig, "w", encoding="utf-8") as fh:
                fh.write("users: [{user: {exec: {command: /bin/evil}}}]\n")
            return problems
        with unittest.mock.patch.object(cq_remote.Remote, "kubeconfig_problems", check_then_swap):
            self.assertEqual(self.key(), SECRET)
        self.assertEqual([r["kubeconfig_text"] for r in self.calls("kubectl")], [checked, checked])

    def test_a_symlinked_kubeconfig_is_refused_before_any_kubectl_call(self):
        link = os.path.join(self.tmp, "kube", "link")
        os.symlink(self.kubeconfig, link)
        os.environ[cq_remote.KUBECONFIG_ENV] = link
        with self.assertRaisesRegex(cq_runner.AdapterError, "or it is a symlink"):
            self.key()
        self.assertEqual(self.calls("kubectl"), [])

    def test_failures_report_the_step_and_code_and_never_kubectl_or_server_output(self):
        canary = "CANARY-TOKEN-ONE"
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.key("nope")
        self.assertEqual(str(cm.exception), "reading nope/secret in namespace vc answered HTTP status 404")
        self.update(kubectl_fail={"config": {"rc": 1, "stdout": canary, "stderr": f"duplicate name: {canary}"}})
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.key()
        self.assertEqual(str(cm.exception),
                         "cannot read the kubeconfig's current context: kubectl config view exited 1")
        self.update(kubectl_fail={})
        raw = cq_local.Local.sh

        def raw_view_fails(ctx, argv, timeout, check=True, env=None):
            if "--raw" in argv:
                return subprocess.CompletedProcess(argv, 2, canary, canary)
            return raw(ctx, argv, timeout, check, env)
        with unittest.mock.patch.object(cq_local.Local, "sh", raw_view_fails), \
                self.assertRaises(cq_runner.AdapterError) as cm:
            self.key()
        self.assertEqual(str(cm.exception), "kubectl config view --raw exited 2")

    def test_a_redirect_is_a_failure_and_the_credential_never_reaches_its_target(self):
        for user in ({"token": "api-token"},
                     {"client-certificate-data": self.pem("client.crt"), "client-key-data": self.pem("client.key")}):
            self.raw(user=user)
            for secret in ("redirect-https", "redirect-http"):
                with self.subTest(secret=secret, user=sorted(user)), \
                        self.assertRaisesRegex(cq_runner.AdapterError, "answered HTTP status 302$"):
                    self.key(secret)
        self.assertEqual(self.redirected, [])

    def test_a_malformed_huge_or_slow_response_fails_without_showing_any_of_it(self):
        for secret, why in (("bad-status", "failed$"), ("bad-header", "failed$"), ("bad-chunk", "failed$"),
                            ("huge", "larger than 1048576 bytes$"), ("trickle", "failed$"),
                            ("trickle-head", "failed$")):
            with self.subTest(secret=secret), unittest.mock.patch.object(cq_target_key, "FETCH_DEADLINE_S", 2.0):
                start = time.monotonic()
                with self.assertRaisesRegex(cq_runner.AdapterError, why) as cm:
                    self.key(secret)
                self.assertNotIn("CANARY", str(cm.exception))
                self.assertTrue(cm.exception.__suppress_context__ or cm.exception.__context__ is None)
                self.assertLess(time.monotonic() - start, 6)

    def test_no_request_goes_out_after_a_slow_name_lookup_or_handshake(self):
        relay = socket.create_server(("127.0.0.1", 0))
        self.addCleanup(relay.close)
        api = self.server.server_address

        def pump(src, dst, piece, pause):
            try:
                while True:
                    data = src.recv(piece)
                    if not data:
                        break
                    dst.sendall(data)
                    time.sleep(pause)
            except OSError:
                pass
            for s in (src, dst):
                s.close()

        def serve():
            while True:
                try:
                    client, _ = relay.accept()
                except OSError:
                    return
                upstream = socket.create_connection(api)
                threading.Thread(target=pump, args=(client, upstream, 65536, 0), daemon=True).start()
                threading.Thread(target=pump, args=(upstream, client, 32, 0.04), daemon=True).start()
        threading.Thread(target=serve, daemon=True).start()
        full = socket.create_server(("127.0.0.1", 0), backlog=0)
        backlog = [full] + [socket.socket() for _ in range(3)]
        for filler in backlog[1:]:
            filler.setblocking(False)
            filler.connect_ex(full.getsockname())
        for sock in backlog:
            self.addCleanup(sock.close)
        real_lookup = socket.getaddrinfo

        def slow_lookup(*a, **kw):
            time.sleep(2.5)
            return real_lookup(*a, **kw)
        real_connect = cq_target_key._connect

        def late_connect(*a):
            sock = real_connect(*a)
            time.sleep(0.4)
            return sock
        idle = (cq_target_key.threading, "Timer", unittest.mock.MagicMock())
        for how, patches, within in (
                ("lookup", [(cq_target_key.socket, "getaddrinfo", slow_lookup)], 1.5),
                ("lookup without the watchdog", [idle, (cq_target_key.socket, "getaddrinfo", slow_lookup)], 1.5),
                ("slow connect", [], 1.5), ("slow connect without the watchdog", [idle], 1.5),
                ("slow handshake", [], 1.5), ("slow handshake without the watchdog", [idle], 1.5),
                ("connected at the deadline, without the watchdog",
                 [idle, (cq_target_key, "_connect", late_connect)], 5)):
            with self.subTest(how=how), unittest.mock.patch.object(cq_target_key, "FETCH_DEADLINE_S", 0.3), \
                    contextlib.ExitStack() as stack:
                if not how.startswith("lookup"):
                    port = {"slow connect": full, "slow handshake": relay}.get(how.split(" without")[0])
                    self.url = f"https://127.0.0.1:{port.getsockname()[1]}" if port else type(self).url
                    if port is full and patches:
                        threading.Timer(3, lambda: [sock.close() for sock in backlog]).start()
                    self.raw(user={"token": "api-token"})
                    self.update(kubeconfig_view=kubeconfig_view(server=self.url))
                for target, name, value in patches:
                    stack.enter_context(unittest.mock.patch.object(target, name, value))
                start = time.monotonic()
                with self.assertRaisesRegex(cq_runner.AdapterError, "failed$"):
                    self.key()
                self.assertLess(time.monotonic() - start, within)
                time.sleep(2.7)
                self.assertEqual(self.seen, [])

    def test_the_tls_context_verifies_as_the_stdlib_default_does(self):
        import ssl
        used = []
        real = cq_target_key._https_get
        with unittest.mock.patch.object(cq_target_key, "_https_get",
                                        lambda server, path, headers, tls: used.append(tls) or real(server, path,
                                                                                                    headers, tls)):
            self.assertEqual(self.key(), SECRET)
        with unittest.mock.patch.dict(os.environ):
            os.environ.pop("SSLKEYLOGFILE", None)
            default = ssl.create_default_context()
        for name in ("verify_flags", "options", "minimum_version", "maximum_version", "verify_mode", "check_hostname"):
            self.assertEqual(getattr(used[0], name), getattr(default, name), name)
        self.assertEqual(used[0].cert_store_stats()["x509_ca"], 1)

    def test_no_tls_secret_is_logged_while_sslkeylogfile_stays_set(self):
        log = os.path.join(self.tmp, "keys.log")

        class Kept(dict):
            def pop(self, key, *default):
                return self.get(key, *default)
        with unittest.mock.patch.object(os, "environ", Kept(os.environ, SSLKEYLOGFILE=log)):
            self.assertEqual(self.key(), SECRET)
        self.assertFalse(os.path.exists(log))

    def test_the_environment_is_unchanged_after_a_read_that_succeeds_or_fails(self):
        with unittest.mock.patch.dict(os.environ, {"SSLKEYLOGFILE": os.path.join(self.tmp, "keys.log")}):
            before = dict(os.environ)
            self.assertEqual(self.key(), SECRET)
            self.assertEqual(dict(os.environ), before)
            self.raw(user={"token": "api-token"}, ca="other-ca.crt")
            with self.assertRaises(cq_runner.AdapterError):
                self.key()
            self.assertEqual(dict(os.environ), before)

    def test_a_token_with_a_control_character_or_space_is_refused_before_any_request(self):
        for token in ("api-token\nCANARY-KEY", "api-token CANARY-KEY"):
            self.raw(user={"token": token})
            with self.subTest(token=token.split("-")[1][:5]), self.assertRaises(cq_runner.AdapterError) as cm:
                self.key()
            self.assertEqual(str(cm.exception), "the kubeconfig's token has a control character or space")
        self.assertEqual(self.seen, [])

    def test_main_shows_no_exception_text(self):
        def boom(*a):
            raise RuntimeError("CANARY-KEY")
        err = io.StringIO()
        with unittest.mock.patch.object(cq_target_key, "read_key", boom), contextlib.redirect_stderr(err), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(cq_target_key.main(["hcl-daily", "--secret", "s", "--key", "k"]), 3)
        self.assertEqual(err.getvalue(), "cq-target-key: internal error (RuntimeError)\n")

    def test_a_key_with_a_control_character_is_refused(self):
        for secret in ("nul", "lf", "cr", "del"):
            with self.subTest(secret=secret), self.assertRaisesRegex(cq_runner.AdapterError, "control character"):
                self.key(secret)

    def test_a_secret_value_that_is_not_ascii_base64_is_refused_cleanly(self):
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.key("non-ascii")
        self.assertEqual(str(cm.exception), "non-ascii/secret in namespace vc is not base64 UTF-8 text")

    def test_a_kubeconfig_open_to_others_not_a_file_or_kubectls_default_is_refused(self):
        os.chmod(self.kubeconfig, 0o644)
        with self.assertRaisesRegex(cq_runner.AdapterError, "^the kubeconfig is readable by group or others"):
            self.key()
        os.chmod(self.kubeconfig, 0o600)
        os.chmod(os.path.dirname(self.kubeconfig), 0o700)
        os.environ[cq_remote.KUBECONFIG_ENV] = os.path.dirname(self.kubeconfig)
        with self.assertRaisesRegex(cq_runner.AdapterError, "^the kubeconfig is not a regular file$"):
            self.key()
        del os.environ[cq_remote.KUBECONFIG_ENV]
        with self.assertRaisesRegex(cq_runner.AdapterError, "^the kubeconfig is kubectl's default ~/.kube/config"):
            self.key(kubeconfig="~/.kube/config")
        self.assertEqual(self.calls("kubectl"), [])

    def test_a_process_that_cannot_be_made_undumpable_reads_no_key(self):
        with unittest.mock.patch.object(cq_target_key, "harden", lambda: False), \
                self.assertRaisesRegex(cq_runner.AdapterError, "could not be made undumpable"):
            self.key()
        self.assertEqual((self.calls("kubectl"), self.seen), ([], []))

        class NoPrctl:
            def prctl(self, *a):
                return -1
        with unittest.mock.patch("ctypes.CDLL", lambda *a, **kw: NoPrctl()), \
                unittest.mock.patch.object(cq_target_key.resource, "setrlimit"):
            self.assertIs(HARDEN(), False)

    def test_the_key_folder_needs_an_owner_only_tmpfs_parent(self):
        for name, setup in (("group-readable", lambda: os.chmod(self.parent, 0o750)),
                            ("not tmpfs", lambda: setattr(cq_remote, "fs_type", lambda path: "ext4")),
                            ("missing", lambda: os.rmdir(self.parent))):
            with self.subTest(name), unittest.mock.patch.object(cq_remote, "fs_type", cq_remote.fs_type):
                setup()
                with self.assertRaisesRegex(cq_runner.AdapterError, "is not an owner-only tmpfs folder of this user"):
                    self.key()
                if os.path.isdir(self.parent):
                    os.chmod(self.parent, 0o700)
        self.assertEqual(self.calls("kubectl"), [])

    def test_a_key_folder_that_cannot_be_removed_is_named(self):
        with unittest.mock.patch.object(cq_target_key.shutil, "rmtree"), \
                self.assertRaisesRegex(cq_runner.AdapterError, "could not be removed; remove it by hand$"):
            self.key()
        self.assertEqual(len(os.listdir(self.parent)), 1)

    def test_a_signal_during_the_read_removes_the_key_folder(self):
        import signal
        profiles = os.path.join(self.tmp, "profiles")
        os.makedirs(profiles)
        with open(os.path.join(profiles, "hcl-daily.json"), "w", encoding="utf-8") as fh:
            json.dump(dict(PROFILE, kube_server=self.url), fh)
        code = ("import os, sys, time; sys.path.insert(0, sys.argv[1]); import cq_remote, cq_target_key as k; "
                "k.KEY_PARENT, cq_remote.PROFILE_DIR = sys.argv[2], sys.argv[3]; k.harden = lambda: True; "
                "cq_remote.fs_type = lambda path: 'tmpfs'; real = k._snapshot\n"
                "def held(path, from_env, tmp):\n"
                "    copy = real(path, from_env, tmp); open(os.path.join(tmp, 'client.key'), 'w').close()\n"
                "    print(tmp, flush=True); time.sleep(60)\n"
                "k._snapshot = held; sys.exit(k.main(['hcl-daily', '--secret', 's', '--key', 'k']))")
        for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
            with self.subTest(sig=sig.name):
                p = subprocess.Popen([sys.executable, "-c", code, HERE, self.parent, profiles], stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True)
                self.addCleanup(p.kill)
                self.assertTrue(select.select([p.stdout], [], [], 20)[0], "the helper never held the folder")
                folder = p.stdout.readline().strip()
                self.assertEqual(sorted(os.listdir(folder)), ["client.key", "kubeconfig"])
                p.send_signal(sig)
                _, err = p.communicate(timeout=20)
                self.assertEqual(p.returncode, 128 + sig)
                self.assertEqual(err, f"cq-target-key: stopped by {sig.name}; no key folder is left\n")
                self.assertEqual(os.listdir(self.parent), [])

    def test_a_signal_just_before_the_cleanup_mask_names_the_folder_it_leaves(self):
        profiles = os.path.join(self.tmp, "profiles")
        os.makedirs(profiles)
        with open(os.path.join(profiles, "hcl-daily.json"), "w", encoding="utf-8") as fh:
            json.dump(dict(PROFILE, kube_server=self.url), fh)
        code = ("import os, signal, sys; sys.path.insert(0, sys.argv[1]); import cq_remote, cq_target_key as k; "
                "k.KEY_PARENT, cq_remote.PROFILE_DIR = sys.argv[2], sys.argv[3]; k.harden = lambda: True; "
                "cq_remote.fs_type = lambda path: 'tmpfs'; armed = []\n"
                "real, mask = k._snapshot, signal.pthread_sigmask\n"
                "def held(path, from_env, tmp):\n"
                "    real(path, from_env, tmp); armed.append(1); raise k.AdapterError('refused')\n"
                "def late(how, sigs):\n"
                "    if armed and how == signal.SIG_BLOCK:\n"
                "        armed.clear(); os.kill(os.getpid(), signal.SIGTERM)\n"
                "    return mask(how, sigs)\n"
                "k._snapshot, signal.pthread_sigmask = held, late\n"
                "sys.exit(k.main(['hcl-daily', '--secret', 's', '--key', 'k']))")
        p = subprocess.run([sys.executable, "-c", code, HERE, self.parent, profiles], capture_output=True, text=True,
                           timeout=20)
        (folder,) = os.listdir(self.parent)
        folder = os.path.join(self.parent, folder)
        self.assertEqual((p.returncode, sorted(os.listdir(folder))), (143, ["kubeconfig"]))
        self.assertEqual(p.stderr, f"cq-target-key: stopped by SIGTERM; {folder} is left; remove it by hand\n")

    def test_a_sigint_during_the_cleanup_shows_no_traceback(self):
        code = ("import os, sys, time; sys.path.insert(0, sys.argv[1]); import cq_remote, cq_target_key as k; "
                "k.KEY_PARENT = sys.argv[2]; k.harden = lambda: True; cq_remote.fs_type = lambda path: 'tmpfs'; "
                "cq_remote.load_profile = lambda *a: {'namespace': 'vc', 'kubeconfig': 'x'}; real = k.shutil.rmtree\n"
                "def held(path, from_env, tmp):\n"
                "    raise RuntimeError('CANARY-CREDENTIAL')\n"
                "def slow(path, **kw):\n"
                "    print('cleaning', flush=True); time.sleep(1); real(path, **kw)\n"
                "k._snapshot, k.shutil.rmtree = held, slow\n"
                "sys.exit(k.main(['hcl-daily', '--secret', 's', '--key', 'k']))")
        p = subprocess.Popen([sys.executable, "-c", code, HERE, self.parent], stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True)
        self.addCleanup(p.kill)
        self.assertTrue(select.select([p.stdout], [], [], 20)[0], "the helper never reached its cleanup")
        self.assertEqual(p.stdout.readline(), "cleaning\n")
        p.send_signal(cq_target_key.signal.SIGINT)
        _, err = p.communicate(timeout=20)
        self.assertEqual((p.returncode, err), (130, "cq-target-key: stopped by SIGINT\n"))
        self.assertEqual(os.listdir(self.parent), [])

    def test_the_read_threads_leave_the_stop_signals_to_the_main_thread(self):
        import signal
        masks, timers, lookup = {}, [], socket.getaddrinfo

        def masked_lookup(*a, **kw):
            masks["resolver"] = signal.pthread_sigmask(signal.SIG_BLOCK, [])
            return lookup(*a, **kw)

        class Watchdog(threading.Timer):
            def run(self):
                masks["watchdog"] = signal.pthread_sigmask(signal.SIG_BLOCK, [])
                super().run()
        with unittest.mock.patch.object(cq_target_key.socket, "getaddrinfo", masked_lookup), \
                unittest.mock.patch.object(cq_target_key.threading, "Timer",
                                           lambda *a: timers.append(Watchdog(*a)) or timers[-1]):
            self.assertEqual(self.key(), SECRET)
        for t in timers:
            t.join(5)
        self.assertEqual({k: set(cq_target_key.STOP_SIGNALS) <= v for k, v in masks.items()},
                         {"resolver": True, "watchdog": True})
        self.assertFalse(set(cq_target_key.STOP_SIGNALS) & signal.pthread_sigmask(signal.SIG_BLOCK, []))

    def test_harden_drops_core_files_and_dumpability(self):
        code = ("import ctypes, resource, sys; sys.path.insert(0, sys.argv[1]); import cq_target_key as k; "
                "ok = k.harden(); libc = ctypes.CDLL(None); "
                "print(ok, libc.prctl(k.PR_GET_DUMPABLE, 0, 0, 0, 0), resource.getrlimit(resource.RLIMIT_CORE))")
        out = subprocess.run([sys.executable, "-c", code, HERE], capture_output=True, text=True, check=True).stdout
        self.assertEqual(out.strip(), "True 0 (0, 0)")

    def test_a_kubeconfig_on_another_context_or_server_is_refused_before_the_secret_is_read(self):
        with self.assertRaisesRegex(cq_runner.AdapterError, "no current context, or not the profile's"):
            self.key(kube_context="safe")
        self.update(kubeconfig_view=kubeconfig_view(server="https://198.51.100.9:6443"))
        with self.assertRaisesRegex(cq_runner.AdapterError, "not the profile's API server"):
            self.key()
        self.update(kubeconfig_view=kubeconfig_view(server=self.url))
        w = self.get_world()
        w["kubeconfig_raw"]["clusters"][0]["cluster"]["server"] = "https://198.51.100.9:6443"
        self.set_world(w)
        with self.assertRaisesRegex(cq_runner.AdapterError, "server is not the profile's kube_server"):
            self.key()
        self.assertEqual(self.seen, [])

    def test_a_missing_short_blank_or_dev_default_key_is_refused(self):
        for secret, why in (("nope", "HTTP status 404"), ("short", "shorter than"), ("blank", "empty"),
                            ("dev", "the profile's dev_secret")):
            with self.subTest(secret=secret), self.assertRaisesRegex(cq_runner.AdapterError, why):
                self.key(secret)

    def test_main_prints_only_the_key_or_exits_3(self):
        profiles = os.path.join(self.tmp, "profiles")
        os.makedirs(profiles)
        with open(os.path.join(profiles, "hcl-daily.json"), "w", encoding="utf-8") as fh:
            json.dump(dict(PROFILE, kube_server=self.url), fh)
        for secret, rc, want in (("jwt-secret", 0, SECRET), ("nope", 3, "")):
            out, err = io.StringIO(), io.StringIO()
            with unittest.mock.patch.object(cq_remote, "PROFILE_DIR", profiles), contextlib.redirect_stdout(out), \
                    contextlib.redirect_stderr(err):
                self.assertEqual(cq_target_key.main(["hcl-daily", "--secret", secret, "--key", "secret"]), rc)
            self.assertEqual(out.getvalue(), want)
            self.assertEqual(bool(err.getvalue()), rc != 0)
            self.assertNotIn(SECRET, err.getvalue())

    def test_main_refuses_a_terminal_and_reads_no_key(self):
        class Tty(io.StringIO):
            def isatty(self):
                return True
        profiles = os.path.join(self.tmp, "profiles")
        os.makedirs(profiles)
        with open(os.path.join(profiles, "hcl-daily.json"), "w", encoding="utf-8") as fh:
            json.dump(dict(PROFILE, kube_server=self.url), fh)
        out, err = Tty(), io.StringIO()
        with unittest.mock.patch.object(cq_remote, "PROFILE_DIR", profiles), contextlib.redirect_stdout(out), \
                contextlib.redirect_stderr(err):
            self.assertEqual(cq_target_key.main(["hcl-daily", "--secret", "jwt-secret", "--key", "secret"]), 3)
        self.assertEqual(out.getvalue(), "")
        self.assertIn("stdout is a terminal", err.getvalue())
        self.assertEqual((self.calls("kubectl"), self.seen), ([], []))

    def test_a_fifo_kubeconfig_is_refused_without_waiting_for_a_writer(self):
        class Waited(Exception):
            pass

        def waited(*_):
            raise Waited("opening the kubeconfig waited for a writer")
        fifo = os.path.join(self.tmp, "kube", "fifo")
        os.mkfifo(fifo, 0o600)
        os.environ[cq_remote.KUBECONFIG_ENV] = fifo
        old = signal.signal(signal.SIGALRM, waited)
        signal.setitimer(signal.ITIMER_REAL, 5)
        try:
            with self.assertRaisesRegex(cq_runner.AdapterError, "^the kubeconfig is not a regular file$"):
                self.key()
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, old)
        self.assertEqual((self.calls("kubectl"), os.listdir(self.parent)), ([], []))


@unittest.skipUnless(shutil.which("kubectl"), "needs a real kubectl")
class RealKubectlErrors(unittest.TestCase):

    def test_kubectls_duplicate_user_diagnostic_never_reaches_the_output(self):
        tmp = tempfile.mkdtemp(prefix="cq-key-dup-")
        self.addCleanup(shutil.rmtree, tmp, True)
        ca = base64.b64encode(b"not-a-real-ca").decode()
        kubeconfig = os.path.join(tmp, "kubeconfig")
        cluster = {"server": PROFILE["kube_server"], "certificate-authority-data": ca}
        doc = {"apiVersion": "v1", "kind": "Config", "current-context": "x",
               "clusters": [{"name": "c", "cluster": cluster}],
               "contexts": [{"name": "x", "context": {"cluster": "c", "user": "u"}}],
               "users": [{"name": "u", "user": {"token": "CANARY-TOKEN-ONE"}},
                         {"name": "u", "user": {"token": "CANARY-TOKEN-TWO"}}]}
        with open(kubeconfig, "w", encoding="utf-8") as fh:
            json.dump(doc, fh)
        os.chmod(kubeconfig, 0o600)
        probe = subprocess.run(["kubectl", "config", "view", "--minify", "-o", "json", "--kubeconfig", kubeconfig],
                               capture_output=True, text=True)
        self.assertIn("CANARY-TOKEN-ONE", probe.stderr)
        profiles = os.path.join(tmp, "profiles")
        os.makedirs(profiles)
        with open(os.path.join(profiles, "hcl-daily.json"), "w", encoding="utf-8") as fh:
            json.dump(PROFILE, fh)
        parent = os.path.join(tmp, "run-user")
        os.mkdir(parent, 0o700)
        out, err = io.StringIO(), io.StringIO()
        with unittest.mock.patch.dict(os.environ, {cq_remote.KUBECONFIG_ENV: kubeconfig}), \
                unittest.mock.patch.object(cq_remote, "PROFILE_DIR", profiles), \
                unittest.mock.patch.object(cq_target_key, "KEY_PARENT", parent), \
                unittest.mock.patch.object(cq_remote, "fs_type", lambda path: "tmpfs"), \
                unittest.mock.patch.object(cq_target_key, "harden"), contextlib.redirect_stdout(out), \
                contextlib.redirect_stderr(err):
            self.assertEqual(cq_target_key.main(["hcl-daily", "--secret", "jwt-secret", "--key", "secret"]), 3)
        self.assertEqual(out.getvalue(), "")
        self.assertNotIn("CANARY", err.getvalue())
        self.assertIn("kubectl config view exited 1", err.getvalue())


class ProfileTargets(unittest.TestCase):

    def test_profile_targets_are_the_profile_files_named_like_targets(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        for name in ("lab-a.json", "lab-b.json", "local.json", "ci.json", "Bad_Name.json", "notes.txt", "lab-c\n.json",
                     "local\n.json"):
            open(os.path.join(tmp, name), "w", encoding="utf-8").close()
        self.assertEqual(cq_scenario.profile_targets(tmp), ["lab-a", "lab-b"])
        self.assertEqual(cq_scenario.profile_targets(os.path.join(tmp, "missing")), [])

    def test_a_profile_name_with_a_leading_dash_is_not_a_target(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        for name in ("-lab.json", "--target.json", "lab.json"):
            open(os.path.join(tmp, name), "w", encoding="utf-8").close()
        self.assertEqual(cq_scenario.profile_targets(tmp), ["lab"])

    def test_a_profile_file_alone_adds_a_shared_target_guarded_by_its_own_values(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        repo = os.path.join(tmp, "repo")
        quality = os.path.join(repo, "scripts", "quality")
        shutil.copytree(HERE, quality, ignore=shutil.ignore_patterns("__pycache__"))
        with open(os.path.join(quality, "targets", "lab-a.json"), "w", encoding="utf-8") as fh:
            json.dump(dict(PROFILE, target="lab-a", dev_secret="lab-a-dev-secret"), fh)
        with open(S0, encoding="utf-8") as fh:
            text = fh.read().replace("target: hcl-daily", "target: lab-a")
        with open(os.path.join(quality, "scenarios", "lab-a.yaml"), "w", encoding="utf-8") as fh:
            fh.write(text)
        for argv in (["init", "-q"], ["add", "-A"], ["-c", "user.name=t", "-c", "user.email=t@example.invalid",
                                                     "commit", "-q", "-m", "x"]):
            subprocess.run(["git", "-C", repo, *argv], check=True, capture_output=True)

        def run(*args, **env):
            return subprocess.run([sys.executable, os.path.join(quality, "scenario_run.py"), "--scenario",
                                   os.path.join(quality, "scenarios", "lab-a.yaml"), *args], capture_output=True,
                                  text=True, env=dict({k: v for k, v in os.environ.items() if not k.startswith("CQ_")
                                                       and k != "JWT_SECRET"}, **env), timeout=60)
        plan = json.loads(run("--compile-only").stdout)
        self.assertEqual((plan["target"], plan["environment"], plan["scoring"]["mode"]), ("lab-a", "lab-a", "report"))
        self.assertIn("pass --target lab-a and --stack reuse", run("--target", "lab-a").stderr)
        self.assertIn("set CQ_TARGET_CONFIRM=lab-a", run("--target", "lab-a", "--stack", "reuse", "--out-dir",
                                                         os.path.join(tmp, "runs-1"), JWT_SECRET=SECRET).stderr)
        p = run("--target", "lab-a", "--stack", "reuse", "--out-dir", os.path.join(tmp, "runs-2"),
                JWT_SECRET="lab-a-dev-secret", CQ_TARGET_CONFIRM="lab-a")
        self.assertEqual(p.returncode, 3)
        self.assertIn("JWT_SECRET is the e2e dev default", p.stderr)


class Profile(Base):

    @unittest.skipUnless(os.path.exists(COMMITTED), "the HCL target profile is stripped from the public tree")
    def test_the_committed_probe_costs_cover_s0s_measured_probe_and_admit_a_second_on_s0s_host(self):
        cores = cq_remote.load_profile("hcl-daily")["probe_cores"]
        self.assertGreaterEqual(cores["camera_off"], 0.93)
        self.assertGreater(cores["camera_off_mic_on"], cores["camera_off"])
        with open(self.profile_path, "w", encoding="utf-8") as fh:
            json.dump(dict(PROFILE, probe_cores=cores), fh)
        self.plan = plan_of(text=mic_on_pair())
        self.make()
        self.fleet.clock_facts()
        self.fleet.launch(self.proc("probe-00"))
        self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}})
        self.assertTrue(self.fleet._in_meeting("probe-00"))
        self.cpu.pct = 2.96 / 8 * 100
        self.cpu()
        self.assertEqual(self.fleet.admit(self.proc("probe-01")), (
            True, "host 2.96 + camera-off-mic-on probe 1.20 cores <= 5.30 (the 70% line less 0.3 overhead)"))

    @unittest.skipUnless(os.path.exists(COMMITTED), "the HCL target profile is stripped from the public tree")
    def test_the_committed_profile_matches_dailys_deploy_and_names_no_window(self):
        p = cq_remote.load_profile("hcl-daily")
        deploy = ""
        for rel in DEPLOY_FILES:
            with open(os.path.join(ROOT, rel), encoding="utf-8") as fh:
                deploy += fh.read()
        for key in ("ui_url", "api_url", "ws_url", "prom_url"):
            self.assertIn(f"host: {urllib.parse.urlsplit(p[key]).hostname}", deploy.replace("host=", "host: "), key)
        self.assertIn(f'grep -qxF "{p["kube_server"]}"', deploy)
        self.assertIn(f"NAMESPACE: {p['namespace']}", deploy)
        self.assertIn(f"runtimeConfig.wsUrl={p['ws_url']}", deploy)
        self.assertIn(f'--set "env[13].value={p["cookie_name"]}"', deploy)
        self.assertEqual(p["kubeconfig"], "~/.kube/hcl-daily.yaml")
        self.assertNotIn("window_utc", p)
        self.assertEqual(p["probe_cores"], {"camera_off": 1.0, "camera_off_mic_on": 1.2, "camera_on": None})
        self.assertEqual(plan_of()["target"], "hcl-daily")

    def test_a_video_mode_other_than_ekg_or_replay_is_refused(self):
        with open(S0, encoding="utf-8") as fh:
            text = fh.read()
        for mode, ok in (("replay", True), ("ekg", True), ("costume", False)):
            with self.subTest(mode=mode):
                doc = cq_scenario.parse_yaml(text.replace("video_mode: replay", f"video_mode: {mode}"))
                if ok:
                    self.assertEqual(cq_scenario.compile_scenario(doc, run_id=RUN_ID, scenario_file="s0.yaml",
                                                                  scenario_sha256="0" * 64)["video_mode"], mode)
                else:
                    with self.assertRaises(cq_scenario.ScenarioError) as cm:
                        cq_scenario.compile_scenario(doc, run_id=RUN_ID, scenario_file="s0.yaml",
                                                     scenario_sha256="0" * 64)
                    self.assertIn("run.video_mode: must be one of ekg, replay", "\n".join(cm.exception.errors))

    def test_a_malformed_profile_is_refused(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        bad = {"ui_url": "http://ui.test.invalid", "ws_url": "ws://relay.test.invalid",
               "pods": dict(PROFILE["pods"], ui='ui"}) or vector(1'), "window_utc": ["10:00", "06:00"],
               "namespace": 'vc",x="y', "target": "ci"}
        full = PROFILE["probe_cores"]
        cores = [{"camera_off": 3.0}, dict(full, camera_off=0), dict(full, camera_off=-1.0), dict(full, camera_off="3"),
                 dict(full, camera_on=float("nan")), dict(full, x=1), None]
        for key, value in [*bad.items(), ("extra", 1), *(("probe_cores", c) for c in cores)]:
            with self.subTest(key=key):
                path = os.path.join(tmp, f"{key}.json")
                with open(path, "w", encoding="utf-8") as fh:
                    json.dump(dict(PROFILE, **{key: value}), fh)
                with self.assertRaises(cq_runner.AdapterError):
                    cq_remote.load_profile("hcl-daily", path)


class Factory(unittest.TestCase):

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp)
        self.profile = os.path.join(self.tmp, "profile.json")
        with open(self.profile, "w", encoding="utf-8") as fh:
            json.dump(PROFILE, fh)

    def env(self, **kw):
        return dict({"JWT_SECRET": SECRET, "CQ_TARGET_CONFIRM": "hcl-daily"}, **kw)

    def factory(self, plan, mode, env):
        return cq_remote.factory(plan, self.tmp, mode, environ=env, profile_path=self.profile)

    def test_the_key_leaves_the_environment_even_when_the_run_is_refused(self):
        plan, tmp = plan_of(), tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        cases = [(self.env(CQ_TARGET_CONFIRM=""), "reuse", "set CQ_TARGET_CONFIRM=hcl-daily"),
                 (self.env(JWT_SECRET="dev-jwt-secret-change-me"), "reuse", "the e2e dev default"),
                 (self.env(JWT_SECRET="short"), "reuse", "shorter than 8"),
                 ({"CQ_TARGET_CONFIRM": "hcl-daily"}, "reuse", "JWT_SECRET is not set"),
                 (self.env(), "up", "pass --stack reuse")]
        for env, mode, needle in cases:
            with self.subTest(needle=needle):
                with self.assertRaises(cq_runner.AdapterError) as cm:
                    self.factory(plan, mode, env)
                self.assertIn(needle, str(cm.exception))
                self.assertNotIn("JWT_SECRET", env)
        env = self.env()
        stack, fleet = self.factory(plan, "reuse", env)
        self.assertEqual((type(stack), type(fleet), stack.ctx.docker, stack.ctx.jwt_secret, "JWT_SECRET" in env),
                         (cq_remote.RemoteStack, cq_remote.RemoteFleet, "podman", SECRET, False))

    def test_a_meeting_id_that_could_break_out_of_a_promql_string_is_refused(self):
        plan, tmp = plan_of(), tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        plan["meeting_id"] = 'scale-x",room!="'
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.factory(plan, "reuse", self.env())
        self.assertIn("not a scale room name", str(cm.exception))

    def test_the_runner_becomes_undumpable(self):
        code = ("import ctypes, sys; sys.path.insert(0, sys.argv[1]); import cq_remote; cq_remote.undumpable(); "
                "print(ctypes.CDLL(None).prctl(3, 0, 0, 0, 0))")
        out = subprocess.run(["timeout", "--signal=KILL", "30", sys.executable, "-c", code, HERE],
                             capture_output=True, text=True, check=True).stdout.strip()
        self.assertEqual(out, "0")

    def test_a_runner_that_cannot_become_undumpable_refuses_the_run(self):
        class Libc:
            def __init__(self, get):
                self.get = get

            def prctl(self, option, *args):
                return self.get if option == cq_remote.PR_GET_DUMPABLE else 0
        for libc in (Libc(1), Libc(-1), None):
            with self.subTest(libc=libc and libc.get):
                cdll = unittest.mock.Mock(side_effect=OSError("no libc")) if libc is None else \
                    unittest.mock.Mock(return_value=libc)
                with unittest.mock.patch("ctypes.CDLL", cdll), self.assertRaisesRegex(
                        cq_runner.AdapterError, "could not make itself undumpable"):
                    cq_remote.undumpable()
        with unittest.mock.patch("ctypes.CDLL", unittest.mock.Mock(return_value=Libc(0))):
            cq_remote.undumpable()

    def test_host_readers_fail_as_adapter_errors_or_unknown(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        for text in ("", "cpu  1 2 x 4\n", "intr 5\n"):
            with open(os.path.join(tmp, "stat"), "w", encoding="utf-8") as fh:
                fh.write(text)
            with self.subTest(text=text), self.assertRaises(cq_runner.AdapterError):
                cq_remote.read_cpu(os.path.join(tmp, "stat"))
        with self.assertRaises(cq_runner.AdapterError):
            cq_remote.read_cpu(os.path.join(tmp, "missing"))
        self.assertEqual(len(cq_remote.read_cpu()), 2)
        self.assertEqual(cq_remote.fs_type("/proc/self"), "proc")

    def test_hcl_daily_runs_through_the_remote_adapter_and_its_guard(self):
        self.assertIs(scenario_run.ADAPTERS["hcl-daily"], cq_remote.factory)
        self.assertFalse(hasattr(scenario_run, "MISSING_ADAPTERS"))
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        err = io.StringIO()
        with unittest.mock.patch.dict(os.environ, {"JWT_SECRET": SECRET}), contextlib.redirect_stderr(err):
            os.environ.pop("CQ_TARGET_CONFIRM", None)
            rc = scenario_run.main(["--scenario", S0, "--target", "hcl-daily", "--stack", "reuse", "--out-dir", tmp],
                                   tree=tsr.fakes.FakeTree(tsr.fakes.FakeClock(), ROOT), clock=tsr.fakes.FakeClock())
            self.assertNotIn("JWT_SECRET", os.environ)
        self.assertEqual(rc, 3)
        self.assertIn("set CQ_TARGET_CONFIRM=hcl-daily", err.getvalue())


class Secrets(Base):

    def test_no_child_process_inherits_the_key(self):
        self.assertNotIn("JWT_SECRET", os.environ)
        self.launch_all()
        self.write_joins()
        self.fleet.joined()
        for p in self.plan["processes"]:
            self.fleet.stop(p)
        self.fleet.collect(self.run_dir)
        rows = self.calls()
        self.assertEqual({r["prog"] for r in rows}, {"podman", "kubectl", "curl", "git", "timedatectl", "loginctl"})
        self.assertEqual([r for r in rows if r["env_jwt"] or r["env_values"] or not r["own_session"]], [])
        self.assertEqual({(r["env_kubeconfig"], r["kuberc"]) for r in self.calls("kubectl")}, {(False, "off")})

    def test_the_runners_own_git_and_tool_children_get_no_key(self):
        with unittest.mock.patch.dict(os.environ, {"JWT_SECRET": SECRET}):
            cq_runner.GitTree(self.root).commit()
            cq_runner.SubprocessTools().run(["git", "status"])
            self.assertEqual(os.environ["JWT_SECRET"], SECRET)
        self.assertEqual([(r["argv"][-1], r["env_jwt"], r["env_values"]) for r in self.calls("git")],
                         [("HEAD", False, []), ("status", False, [])])

    def test_child_env_drops_the_key_and_kubeconfig_even_if_set_again(self):
        with unittest.mock.patch.dict(os.environ, {"JWT_SECRET": SECRET, "KUBECONFIG": "/x"}):
            env, runner_env = self.ctx.env(BOT_CTL_TOKEN="t"), cq_runner.child_env()
        self.assertEqual(({"JWT_SECRET", "KUBECONFIG"} & set(env), env["BOT_CTL_TOKEN"]), (set(), "t"))
        self.assertEqual((runner_env.get("JWT_SECRET"), runner_env.get("KUBECONFIG")), (None, "/x"))

    def test_the_load_host_sampler_records_only_bot_and_runtime_processes(self):
        started = []
        with unittest.mock.patch.object(cq_local.subprocess, "Popen", lambda argv, **kw: started.append(argv) or
                                        unittest.mock.Mock(poll=lambda: None)), \
                unittest.mock.patch.object(cq_local, "sampler_writers", lambda csv: 1):
            self.fleet._start_sampler()
        grep = started[0][started[0].index("--proc-grep") + 1]
        for comm, kept in (("bot", True), ("chrome", True), ("node", True), ("conmon", True), ("pasta.avx2", True),
                           ("falcon-sensor", False), ("k3s-server", False), ("sshd", False), ("containerd", False)):
            self.assertEqual(bool(re.search(grep, comm)), kept, comm)

    def test_the_key_is_never_on_argv_and_reaches_bots_only_as_tmpfs_files(self):
        self.launch_all()
        argvs = [r["argv"] for r in self.calls()]
        self.assertFalse([a for a in argvs if any(SECRET in x or "JWT_SECRET" in x for x in a)])
        for pid in ("rust-pub", "probe-00"):
            d = self.ctx.secret_dir(pid)
            self.assertEqual(stat.S_IMODE(os.stat(d).st_mode), 0o700)
            for name in os.listdir(d):
                self.assertEqual(stat.S_IMODE(os.stat(os.path.join(d, name)).st_mode), 0o600)
        with open(os.path.join(self.ctx.secret_dir("rust-pub"), "rust-pub.yaml"), encoding="utf-8") as fh:
            self.assertEqual(json.load(fh)["jwt_secret"], SECRET)
        with open(os.path.join(self.run_dir, "in", "rust-pub.yaml"), encoding="utf-8") as fh:
            self.assertNotIn("jwt_secret", json.load(fh))
        rust = next(a for a in self.podman_runs() if "--config" in a)
        self.assertIn(f"{self.ctx.secret_dir('rust-pub')}:/cfg:ro,z", rust)
        self.assertEqual(rust[rust.index("--config") + 1], "/cfg/rust-pub.yaml")

    def test_secret_files_go_once_each_bot_joined_and_all_go_at_collect(self):
        self.launch_all()
        self.write_joins()
        self.fleet.joined()
        self.assertFalse(os.path.exists(self.ctx.secret_dir("rust-pub")))
        self.assertFalse(os.path.exists(self.ctx.secret_dir("probe-00")))
        self.assertTrue(os.path.isdir(self.ctx.secret_dir()))
        self.fleet.collect(self.run_dir)
        self.assertFalse(os.path.exists(self.ctx.secret_dir()))

    def test_a_probe_holding_the_key_in_its_environment_or_without_the_preload_ends_the_run(self):
        self.launch_all()
        self.write_joins(marker=False)
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.fleet.joined()
        self.assertIn("never confirmed it read the key", str(cm.exception))
        self.update(env_hits=["/proc/41", "/proc/77"])
        self.write_joins()
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.fleet.joined()
        self.assertIn("2 process(es) in the probe hold JWT_SECRET", str(cm.exception))
        self.update(env_hits=[], env_done=False)
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.fleet.joined()
        self.assertIn("could not read the probe's process environments", str(cm.exception))

    def test_the_secret_parent_must_be_tmpfs_and_hold_no_earlier_run(self):
        self.ctx.fs = lambda path: "xfs"
        self.assertIn("is not a tmpfs", "; ".join(self.stack.preflight(self.plan)))
        self.ctx.fs = lambda path: "tmpfs"
        os.makedirs(os.path.join(self.ctx.secret_parent, "cq-p0s0-older"))
        self.assertIn("1 tmpfs secret folder(s) of an earlier run remain", "; ".join(self.stack.preflight(self.plan)))
        self.assertEqual(self.calls("curl"), [])

    def test_a_checkout_whose_bots_app_passes_the_key_to_chromium_is_refused(self):
        with open(os.path.join(self.root, cq_remote.BROWSER_ENV), "w", encoding="utf-8") as fh:
            fh.write("const BROWSER_ENV_STRIP_RE = /^(BOT_(EMAIL|PASSWORD|CTL_TOKEN)(_\\d+)?)$/;\n")
        self.assertIn("does not strip JWT_SECRET", "; ".join(self.stack.preflight(self.plan)))

    def test_the_real_bots_app_strips_the_key(self):
        with open(os.path.join(ROOT, cq_remote.BROWSER_ENV), encoding="utf-8") as fh:
            self.assertRegex(fh.read(), cq_remote.STRIP_RE)


class Argv(Base):

    def test_every_url_comes_from_the_profile(self):
        self.launch_all()
        probe = next(a for a in self.podman_runs() if "--meeting-url" in a)
        self.assertEqual(probe[probe.index("--meeting-url") + 1], f"https://ui.test.invalid/meeting/{ROOM}")
        with open(os.path.join(self.run_dir, "in", "rust-pub.yaml"), encoding="utf-8") as fh:
            self.assertEqual(json.load(fh)["ws_url"], "wss://relay.test.invalid")
        urls = {next(a for a in reversed(r["argv"]) if a.startswith("http")) for r in self.calls("curl")}
        self.assertTrue(urls)
        self.assertTrue(all(u.startswith(("https://prom.test.invalid/", "https://ui.test.invalid/",
                                          "https://api.test.invalid/")) for u in urls), urls)
        gets = [r["argv"] for r in self.calls("kubectl") if r["argv"][0] == "get"]
        self.assertTrue(gets)
        self.assertEqual({a[a.index("--server") + 1] for a in gets}, {"https://192.0.2.10:6443"})
        blob = json.dumps(self.calls())
        for root, _, files in os.walk(self.run_dir):
            for name in files:
                with open(os.path.join(root, name), "rb") as fh:
                    blob += fh.read().decode("utf-8", "replace")
        self.assertTrue(DAILY_HOSTS or not os.path.exists(COMMITTED))
        self.assertFalse([h for h in DAILY_HOSTS if h in blob])

    def test_rust_bots_get_the_host_network_and_a_loopback_metrics_port_only(self):
        self.launch_all()
        rust = [a for a in self.podman_runs() if "--config" in a]
        self.assertEqual(len(rust), 2)
        for k, argv in enumerate(rust):
            self.assertEqual(argv[argv.index("--network") + 1], "host")
            self.assertIn("RUST_LOG=info,bot::video_producer::replay=debug", argv)
            self.assertEqual(argv[argv.index("--diagnostics") + 1], "off")
            self.assertEqual(argv[argv.index("--metrics-port") + 1], str(19100 + k))
            self.assertEqual(argv[argv.index("--metrics-bind") + 1], "127.0.0.1")
            for flag, value in (("--userns", "keep-id"), ("--log-driver", "k8s-file"), ("--pull", "never"),
                                ("--ulimit", "core=0")):
                self.assertEqual(argv[argv.index(flag) + 1], value)
            mounts = [argv[i + 1] for i, a in enumerate(argv) if a == "-v"]
            self.assertTrue(mounts and all(v.endswith((":z", ",z")) for v in mounts), mounts)

    def test_probes_get_no_host_network_no_host_ipc_and_log_in_over_websocket(self):
        self.launch_all()
        probes = [a for a in self.podman_runs() if "--meeting-url" in a]
        self.assertEqual(len(probes), 2)
        for argv in probes:
            self.assertNotIn("--network", argv)
            self.assertNotIn("--ipc", argv)
            self.assertEqual(argv[argv.index("--shm-size") + 1], "1g")
            self.assertEqual(argv[argv.index("--transport") + 1], "ws")
            self.assertEqual(argv[argv.index("--auth") + 1], "jwt")
            self.assertEqual(argv[argv.index("--sso-state-file") + 1], "")
            self.assertEqual(argv[argv.index("--entrypoint") + 1], "node")
            self.assertIn("/cq/preload.mjs", argv[argv.index("--import", argv.index(cq_local.PW_IMAGE)) + 3])
            self.assertIn("COOKIE_NAME=test-session", argv)
            self.assertEqual(argv[argv.index("--log-driver") + 1], "k8s-file")

    def test_podman_only_stops_and_removes_this_runs_containers(self):
        self.launch_all()
        for p in self.plan["processes"]:
            self.fleet.stop(p)
        self.fleet.collect(self.run_dir)
        verbs = {r["argv"][0] for r in self.calls("podman")}
        self.assertLessEqual(verbs, {"version", "image", "ps", "run", "inspect", "stop", "exec", "logs", "rm"})
        for r in self.calls("podman"):
            if r["argv"][0] in ("stop", "kill", "rm"):
                names = [a for a in r["argv"][1:] if not a.startswith("-") and not a.isdigit()]
                self.assertTrue(names and all(n.startswith(f"cq-{RUN_ID}-") for n in names), r["argv"])

    def test_poll_is_one_batched_inspect_bounded_well_under_the_runner_timeout(self):
        self.launch_all()
        seen = []
        real = cq_remote.Remote.sh

        def spy(ctx, argv, timeout, **kw):
            seen.append((argv[1], timeout, len(argv)))
            return real(ctx, argv, timeout, **kw)
        with unittest.mock.patch.object(cq_remote.Remote, "sh", spy):
            self.assertEqual(self.fleet.poll(), [])
        self.assertEqual(seen, [("inspect", cq_remote.FLEET_INSPECT_S, 4 + len(self.plan["processes"]))])
        self.assertLess(cq_remote.FLEET_INSPECT_S, cq_local.DOCKER_S)


class Kubectl(Base):

    def test_every_mutating_verb_is_refused_before_a_process_starts(self):
        for args in MUTATING:
            with self.subTest(args=args):
                with self.assertRaises(cq_runner.AdapterError):
                    self.ctx.kubectl(*args)
        self.assertEqual(self.calls("kubectl"), [])
        self.ctx.kube_context = "daily"
        self.ctx.kubectl("get", "pods", "-o", "json")
        self.ctx.kubectl("config", "view", "--minify", "-o", "json")
        get, view = [r["argv"] for r in self.calls("kubectl")]
        self.assertEqual((get[get.index("--context") + 1], view[view.index("--context") + 1]), ("daily", "daily"))
        self.assertEqual((get[:2], view[:2]), (["get", "pods"], ["config", "view"]))
        for a in (get, view):
            self.assertEqual(a[a.index("--kubeconfig") + 1], self.kubeconfig)
            self.assertIn("--request-timeout=10s", a)
        self.assertEqual((get[get.index("-n") + 1], get[get.index("--server") + 1]), ("vc", PROFILE["kube_server"]))
        self.assertFalse({"--server", "-n"} & set(view))

    def test_state_is_one_bounded_kubectl_read(self):
        seen = []
        real = cq_remote.Remote.sh

        def spy(ctx, argv, timeout, **kw):
            seen.append((argv[:3], timeout))
            return real(ctx, argv, timeout, **kw)
        with unittest.mock.patch.object(cq_remote.Remote, "sh", spy):
            self.stack.state()
        self.assertEqual(seen, [(["kubectl", "get", "pods"], cq_remote.KUBECTL_S)])
        self.assertLess(cq_remote.KUBECTL_S, cq_runner.TOOL_TIMEOUT_S)

    def test_the_kubeconfig_is_only_read(self):
        before = (os.stat(self.kubeconfig).st_mtime_ns, sorted(os.listdir(os.path.dirname(self.kubeconfig))))
        self.launch_all()
        self.stack.state()
        self.assertEqual((os.stat(self.kubeconfig).st_mtime_ns, sorted(os.listdir(os.path.dirname(self.kubeconfig)))),
                         before)
        self.assertFalse([r for r in self.calls() if r["prog"] != "kubectl" and
                          any(self.kubeconfig in a for a in r["argv"])])

    def test_a_kubeconfig_that_is_shared_elsewhere_or_runs_code_is_refused(self):
        exec_user = dict(kubeconfig_view(), users=[{"user": {"exec": {"command": "x"}}}])
        cases = [(lambda: os.chmod(self.kubeconfig, 0o644), "readable by group or others"),
                 (lambda: self.update(kubeconfig_view=kubeconfig_view(server="https://127.0.0.1:6443")),
                  "not the profile's API server"),
                 (lambda: self.update(kubeconfig_view=exec_user), "uses exec"),
                 (lambda: self.update(kubeconfig_view=dict(kubeconfig_view(), users=[{"user": {"auth-provider": {}}}])),
                  "uses auth-provider"),
                 (lambda: self.update(kubeconfig_view=dict(kubeconfig_view(), users=[{"user": {"tokenFile": "/t"}}])),
                  "names its user tokenFile as a file; embed it with kubectl config view --raw --flatten"),
                 (lambda: self.update(kubeconfig_view=kubeconfig_view(**{"certificate-authority": "/etc/ca.crt"})),
                  "names its cluster certificate-authority as a file"),
                 (lambda: self.update(kubeconfig_view=dict(kubeconfig_view(), users=[{"user": {
                     "client-certificate": "/c.crt", "client-key-data": "DATA+OMITTED"}}])),
                  "names its user client-certificate as a file"),
                 (lambda: self.update(kubeconfig_view=dict(kubeconfig_view(), users=[{"user": {
                     "client-certificate-data": "DATA+OMITTED", "client-key": "/c.key"}}])),
                  "names its user client-key as a file"),
                 (lambda: self.update(kubeconfig_view=dict(kubeconfig_view(), users=[{"user": {
                     "client-certificate-data": "DATA+OMITTED"}}])), "has no embedded credential"),
                 (lambda: self.update(kubeconfig_view=dict(kubeconfig_view(), users=[{"user": {}}])),
                  "has no embedded credential"),
                 (lambda: self.update(kubeconfig_view=kubeconfig_view(**{"proxy-url": "http://p:3128"})),
                  "uses proxy-url"),
                 (lambda: self.update(kubeconfig_view=kubeconfig_view(**{"insecure-skip-tls-verify": True})),
                  "uses insecure-skip-tls-verify"),
                 (lambda: self.update(kubeconfig_view=kubeconfig_view(**{"certificate-authority-data": ""})),
                  "has no certificate authority embedded"),
                 (lambda: self.update(kubeconfig_view=kubeconfig_view(context="")), "has no current context"),
                 (lambda: self.ctx.profile.update(kube_context="daily-admin"), "not the profile's")]
        for setup, needle in cases:
            with self.subTest(needle=needle):
                os.chmod(self.kubeconfig, 0o600)
                self.set_world(world())
                self.ctx.profile["kube_context"] = None
                setup()
                self.assertIn(needle, "; ".join(self.ctx.kubeconfig_problems()))
        for user in ({"token": "REDACTED"},
                     {"client-certificate-data": "DATA+OMITTED", "client-key-data": "DATA+OMITTED"}):
            with self.subTest(user=user):
                self.ctx.profile["kube_context"] = None
                self.set_world(world(kubeconfig_view=dict(kubeconfig_view(), users=[{"user": user}])))
                self.assertEqual(self.ctx.kubeconfig_problems(), [])
                self.assertEqual(self.ctx.kube_context, "daily")
        default = os.path.expanduser(cq_remote.DEFAULT_KUBECONFIG)
        self.ctx.kubeconfig, self.ctx.kubeconfig_from_env = default, False
        self.assertIn("kubectl's default ~/.kube/config", self.ctx.kubeconfig_problems()[0])
        self.ctx.kubeconfig_from_env = True
        self.assertNotIn("kubectl's default", "; ".join(self.ctx.kubeconfig_problems()))
        self.ctx.kubeconfig = self.kubeconfig
        link = self.kubeconfig + ".link"
        os.symlink(self.kubeconfig, link)
        self.ctx.kubeconfig = link
        self.assertEqual(self.ctx.kubeconfig_problems(), ["the kubeconfig is not a regular file"])

    def test_state_names_pods_but_no_node_address_or_registry_and_sees_a_restart(self):
        st = self.stack.state()
        self.assertEqual(sorted(st), sorted(POD_NAMES.values()))
        self.assertNotRegex(json.dumps(st), r"worker-3|198\.51\.100|registry\.internal")
        self.assertEqual(st["ws-relay-7f9c"]["image"], ["relay_ws:2026-10-14@sha256:" + "c" * 64])
        self.update(pods=[pod(k, restarts=1 if k == "relay_ws" else 0) for k in cq_remote.POD_KINDS])
        self.assertNotEqual(self.stack.state(), st)
        images = self.stack.images()
        self.assertEqual(sorted(images), sorted(cq_collect.IMAGE_KEYS))
        self.assertNotRegex(json.dumps(images), r"registry\.internal")
        self.assertEqual(images["relay_wt"], "relay_wt:2026-10-14@sha256:" + "c" * 64)


class WindowGuard(Base):
    """A profile may name a UTC window; hcl-daily's does not, but the mechanism stays."""

    def setUp(self):
        super().setUp()
        with open(self.profile_path, "w", encoding="utf-8") as fh:
            json.dump(dict(PROFILE, window_utc=WINDOW), fh)
        self.make()

    def at(self, hhmm, extra=0):
        self.clock.wall = T0830 - 8.5 * 3600 + cq_remote._day_s(hhmm) + extra

    def test_the_window_refuses_outside_its_hours_any_day(self):
        for hhmm, ok in (("05:59", False), ("06:00", True), ("08:00", True), ("10:00", False), ("12:00", False),
                         ("04:31", False), ("23:00", False)):
            with self.subTest(hhmm=hhmm):
                self.at(hhmm)
                problems = self.stack.window.problems(self.clock.now(), cq_remote.planned_s(self.plan))
                self.assertEqual(problems == [], ok, problems)
        self.at("05:59")
        self.assertIn("outside the approved window 06:00Z-10:00Z", "; ".join(self.stack.preflight(self.plan)))
        self.assertEqual(self.calls("curl"), [])

    def test_a_run_that_cannot_finish_with_teardown_before_the_window_end_is_refused(self):
        step = self.plan["steps"][0]
        need = cq_remote.planned_s(self.plan) + self.stack.window.teardown + cq_remote.WATCH_GAP_S
        self.assertGreaterEqual(cq_remote.planned_s(self.plan), step["join_window_s"] + cq_runner.JOIN_BUDGET_EXTRA_S
                                + cq_runner.MEDIA_SETTLE_S + step["hold_s"] + cq_runner.TEARDOWN_DELAY_S)
        end = cq_remote._day_s("10:00")
        self.at("00:00", end - need)
        self.assertEqual(self.stack.window.problems(self.clock.now(), cq_remote.planned_s(self.plan)), [])
        self.at("00:00", end - need + 1)
        self.assertIn("would end after 10:00Z",
                      self.stack.window.problems(self.clock.now(), cq_remote.planned_s(self.plan))[0])

    def test_the_watch_stops_with_the_teardown_budget_left_before_the_window_end(self):
        self.launch_all()
        stop = cq_remote._day_s("10:00") - self.stack.window.teardown - cq_remote.WATCH_GAP_S
        self.at("00:00", stop - 1)
        self.assertEqual(self.stack.watch(), [])
        self.at("00:00", stop)
        self.assertIn("the approved window closes at 10:00Z", "; ".join(self.stack.watch()))
        self.assertGreaterEqual(cq_remote.WATCH_GAP_S, cq_runner.WATCH_STARVE_S + cq_runner.WATCH_BUDGET_S)

    def test_a_teardown_whose_every_call_takes_its_full_timeout_fits_the_teardown_budget(self):
        self.launch_all()
        clock = self.clock

        class Clock:
            def now(self):
                return cq_runner.Stamp(clock.wall, clock.mono)

            def sleep(self, seconds):
                clock.sleep(seconds)
        runner = cq_runner.Runner(self.plan, b"", self.run_dir, stack=self.stack, fleet=self.fleet,
                                  tree=tsr.fakes.FakeTree(tsr.fakes.FakeClock(), self.root), clock=Clock(),
                                  stack_mode="reuse")
        runner.log = cq_runner.EventLog(os.path.join(self.run_dir, "events.jsonl"), runner._now)
        runner.s.stack_ready = {"pod": {}}
        runner.s.launched = {p["proc_id"]: dict(p, launched_at=clock.wall) for p in self.plan["processes"]}

        def hang_sh(ctx, argv, timeout, check=True, env=None):
            clock.advance(timeout)
            raise cq_runner.AdapterError(f"{argv[0]} timed out after {timeout} s")

        def hang_curl(ctx, url, params=None, max_time=cq_remote.HTTP_S):
            clock.advance(max_time + cq_remote.WATCH_KILL_S)
            return None, b""
        start = clock.mono
        with unittest.mock.patch.object(cq_remote.Remote, "sh", hang_sh), \
                unittest.mock.patch.object(cq_remote.Remote, "curl", hang_curl):
            runner._teardown()
        took, budget = clock.mono - start, self.stack.window.teardown
        self.assertEqual(budget, cq_remote.teardown_s(self.plan))
        self.assertLessEqual(took, budget)
        self.assertLessEqual(budget - took, cq_remote.TEARDOWN_SLACK_S + 1e-6)
        self.assertLessEqual(cq_remote.TEARDOWN_SLACK_S, 15)
        self.assertEqual(len(runner.s.teardown_problems), 1 + len(self.plan["processes"]))
        self.assertEqual(runner.s.drain, [DRAIN_BLIND])

    def test_up_checks_the_window_again_before_the_join(self):
        self.launch_all()
        self.at("09:50")
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.stack.up(self.plan, "reuse")
        self.assertIn("before the join: the run needs", str(cm.exception))

    def test_bots_end_by_the_window_close_even_if_the_runner_dies(self):
        need = int(cq_remote.run_need_s(self.plan))
        for hhmm, extra, want in (("09:55", 0, 300), ("09:59", 0, 60), ("08:30", 0, need)):
            self.at(hhmm, extra)
            self.fleet.run_end = None
            self.assertEqual(self.fleet._ttl_s(), want, hhmm)
            self.assertEqual(self.fleet._deadline(), int(self.clock.now()) + want, hhmm)
        self.at("09:59", 1)
        self.fleet.run_end = None
        with self.assertRaisesRegex(cq_runner.AdapterError, "only 59 s are left before the run's deadline"):
            self.fleet._ttl_s()


class Admit(Base):

    def probe(self, camera):
        text = two_probes().replace("media: {camera: off, mic: muted}", f"media: {{camera: {camera}, mic: muted}}")
        self.plan = plan_of(text=text)
        self.make()
        return self.proc("probe-01")

    def test_a_probe_is_admitted_only_if_host_now_plus_its_cost_fits_the_line_less_overhead(self):
        proc = self.proc("probe-01")
        for ncpu, pct, ok, detail in (
                (8, 25.0, True, "host 2.00 + camera-off probe 3.00 cores <= 5.30"),
                (8, 30.0, False, "host 2.40 + camera-off probe 3.00 cores > 5.30"),
                (4, 25.0, False, "host 1.00 + camera-off probe 3.00 cores > 2.50")):
            with self.subTest(ncpu=ncpu, pct=pct):
                self.ctx.ncpu, self.cpu.pct = (lambda n=ncpu: n), pct
                self.cpu()
                self.clock.slept.clear()
                tail = "" if ok else ", sample 2"
                self.assertEqual(self.fleet.admit(proc), (ok, detail + f" (the 70% line less 0.3 overhead{tail})"))
                self.assertEqual(sum(self.clock.slept), cq_remote.ADMIT_SAMPLE_S * (1 if ok else 2))

    def test_a_probe_exactly_on_the_line_less_overhead_is_admitted(self):
        reads = iter([(0, 0), (1000, 630)])
        self.ctx.cpu, self.ctx.ncpu = (lambda: next(reads)), (lambda: 10)
        self.assertEqual(self.fleet.admit(self.proc("probe-00")),
                         (True, "host 3.70 + camera-off probe 3.00 cores <= 6.70 (the 70% line less 0.3 overhead)"))

    def test_the_owner_is_admitted_on_a_second_sample(self):
        sleep = self.ctx.sleep

        def the_spike_passes_after_one_sample(seconds):
            if sum(self.clock.slept) >= cq_remote.ADMIT_SAMPLE_S:
                self.cpu.pct = 25.0
            sleep(seconds)
        self.ctx.sleep, self.cpu.pct = the_spike_passes_after_one_sample, 30.0
        self.cpu()
        self.clock.slept.clear()
        self.assertEqual(self.fleet.admit(self.proc("probe-00")), (
            True, "host 2.00 + camera-off probe 3.00 cores <= 5.30 (the 70% line less 0.3 overhead, sample 2)"))
        self.assertEqual(sum(self.clock.slept), cq_remote.ADMIT_SAMPLE_S * 2)

    def later_probe(self, pct_after_join, before=lambda: None):
        """probe-01's admission with the owner in the meeting from now, the host's busy share by seconds since."""
        self.fleet.clock_facts()
        self.assertEqual(self.stack.preflight(self.plan), [])
        before()
        self.fleet.launch(self.proc("probe-00"))
        self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}})
        self.assertTrue(self.fleet._in_meeting("probe-00"))
        joined, reads, cpu = self.clock.mono, [], self.cpu

        def read():
            self.cpu.pct = pct_after_join(self.clock.mono - joined)
            reads.append(self.clock.mono - joined)
            return cpu()
        self.ctx.cpu, self.ctx.load = read, cq_remote.LoadLine(read)
        return self.fleet.admit(self.proc("probe-01")), reads, self.clock.mono - joined

    def test_a_later_probe_is_sampled_after_the_previous_probes_join_transient(self):
        (ok, detail), _, _ = self.later_probe(lambda s: 50.0 if s <= cq_remote.ADMIT_SETTLE_S else 25.0)
        self.assertEqual((ok, detail), (True, "host 2.00 + camera-off probe 3.00 cores <= 5.30 (the 70% line less "
                                              "0.3 overhead)"))

    def test_a_later_probe_admitted_long_after_the_previous_probe_joined_samples_at_once(self):
        self.fleet.clock_facts()
        self.assertEqual(self.stack.preflight(self.plan), [])
        self.fleet.launch(self.proc("probe-00"))
        self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}})
        self.assertTrue(self.fleet._in_meeting("probe-00"))
        self.clock.advance(cq_remote.ADMIT_SETTLE_S + 30)
        start = self.clock.mono
        self.assertTrue(self.fleet.admit(self.proc("probe-01"))[0])
        self.assertLess(self.clock.mono - start, cq_remote.ADMIT_SETTLE_S)

    def test_a_40_s_admission_watches_the_stack_every_15_s_across_its_samples(self):
        watched, real, admit, start = [], self.stack.watch, self.fleet.admit, []
        self.stack.watch = lambda: watched.append(self.clock.mono) or real()
        self.fleet.admit = lambda proc: start.append(self.clock.mono) or admit(proc)
        (ok, _), _, took = self.later_probe(lambda s: 30.0)
        start = start[0]
        self.assertFalse(ok)
        self.assertGreaterEqual(took, cq_remote.ADMIT_SETTLE_CAP_S)
        self.assertEqual([round(t - start) for t in watched], [15, 30])

    def hot_sample(self, sample, cool):
        start = cq_remote.ADMIT_SETTLE_S + (sample - 1) * cq_remote.ADMIT_SAMPLE_S
        with self.assertRaisesRegex(cq_runner.AdapterError, "85% hard stop"):
            self.later_probe(lambda s: 90.0 if start < s <= start + cq_remote.ADMIT_SAMPLE_S else cool)

    def test_a_hot_admission_sample_hits_the_hard_stop_instead_of_admitting(self):
        self.hot_sample(1, 20.0)

    def test_a_hot_last_admission_sample_hits_the_hard_stop_before_the_refusal(self):
        self.hot_sample(cq_remote.LATER_SAMPLES, 30.0)

    def test_an_admission_after_the_owners_watches_at_once_when_15_s_have_passed(self):
        watched, real, admit, start = [], self.stack.watch, self.fleet.admit, []

        def owner_admitted_then_launched():
            self.assertTrue(admit(self.proc("probe-00"))[0])
            self.clock.advance(cq_runner.WATCH_INTERVAL_S)
            self.stack.watch = lambda: watched.append(self.clock.mono) or real()
            self.fleet.admit = lambda proc: start.append(self.clock.mono) or admit(proc)
        self.later_probe(lambda s: 30.0, owner_admitted_then_launched)
        self.assertEqual([round(t - start[0]) for t in watched], [0, 15, 30])

    def test_a_third_probe_waits_out_the_second_probes_join_not_the_owners(self):
        self.plan = n_probes(3, 951)
        self.make()
        self.fleet.clock_facts()
        self.assertEqual(self.stack.preflight(self.plan), [])
        self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}})
        for pid in ("probe-00", "probe-01"):
            self.clock.advance(60)
            self.fleet.launch(self.proc(pid))
            self.assertTrue(self.fleet._in_meeting(pid))
        joined, cpu = self.clock.mono, self.cpu

        def read():
            self.cpu.pct = 50.0 if self.clock.mono - joined <= cq_remote.ADMIT_SETTLE_S else 25.0
            return cpu()
        self.ctx.cpu, self.ctx.load = read, cq_remote.LoadLine(read)
        self.assertEqual(self.fleet.admit(self.proc("probe-02")), (
            True, "host 2.00 + camera-off probe 3.00 cores <= 5.30 (the 70% line less 0.3 overhead)"))

    def test_a_join_stagger_of_at_most_max_stagger_s_is_accepted(self):
        for stagger, refused in ((175, False), (176, True), (cq_remote.MAX_STAGGER_S, False)):
            with self.subTest(stagger=stagger):
                plan = n_probes(2, 626, int(stagger))
                plan["processes"][0]["join_stagger_s"] = stagger
                self.assertEqual(plan["processes"][0]["proc_id"], "probe-00")
                self.make(plan)
                problems = "; ".join(self.stack.preflight(plan))
                self.assertEqual("probe-00: join_stagger" in problems, refused, problems)

    def test_a_later_probe_is_resampled_until_the_cap_then_refused(self):
        (ok, detail), _, took = self.later_probe(lambda s: 30.0)
        self.assertEqual((ok, detail), (False, "host 2.40 + camera-off probe 3.00 cores > 5.30 (the 70% line less "
                                               "0.3 overhead, sample 4)"))
        self.assertLessEqual(took, cq_remote.ADMIT_SETTLE_CAP_S)

    def test_a_later_probe_is_sampled_after_the_owner_wait_not_before_it(self):
        self.fleet.clock_facts()
        self.assertEqual(self.stack.preflight(self.plan), [])
        self.fleet.launch(self.proc("probe-00"))
        self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 0})}})
        self.cpu.pct, origin, sleep = 10.0, self.clock.mono, self.ctx.sleep

        def owner_joins_while_the_host_gets_busy(seconds):
            sleep(seconds)
            if self.clock.mono - origin >= 20:
                self.cpu.pct = 40.0
            if self.clock.mono - origin >= 40:
                self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}})
        self.ctx.sleep = owner_joins_while_the_host_gets_busy
        self.cpu()
        self.assertEqual(self.fleet.admit(self.proc("probe-01")),
                         (False, "host 3.20 + camera-off probe 3.00 cores > 5.30 (the 70% line less 0.3 overhead, "
                                 "sample 4)"))

    def test_an_impossible_cpu_reading_or_cpu_count_refuses(self):
        proc = self.proc("probe-00")
        for name, reads, ncpu in (("idle beyond total", [(1000, 0), (1800, 1600)], 8),
                                  ("no cpu count", None, None)):
            with self.subTest(name):
                seq = iter(reads or [(0, 0), (800, 400)])
                self.ctx.cpu = lambda s=seq: next(s)
                self.ctx.ncpu = lambda n=ncpu: n
                self.assertEqual(self.fleet.admit(proc), (False, "the load host's CPU could not be read"))

    def test_a_camera_on_probe_needs_a_cost_from_the_profile(self):
        proc = self.probe("on")
        self.clock.slept.clear()
        self.assertEqual(self.fleet.admit(proc), (False, "no camera-on probe cost in the target profile"))
        self.assertEqual(self.clock.slept, [])
        self.assertIn("probe-00, probe-01: no CPU cost for this kind of probe in the target profile's probe_cores",
                      "; ".join(self.stack.preflight(self.plan)))
        with open(self.profile_path, "w", encoding="utf-8") as fh:
            json.dump(dict(PROFILE, probe_cores=dict(PROFILE["probe_cores"], camera_on=5.0)), fh)
        self.make()
        self.cpu.pct = 0.0
        self.cpu()
        self.assertEqual(self.fleet.admit(proc)[0], True)
        self.assertEqual(self.stack.preflight(self.plan), [])

    def test_s0s_probe_skips_paint_turns_diag_packets_off_and_keeps_the_client_decode_budget(self):
        self.plan = plan_of()
        self.make()
        self.launch_all()
        argv = next(a for a in self.podman_runs() if "--meeting-url" in a)
        for flag, value in (("--skip-canvas-paint", "true"), ("--diag-packets", "off"), ("--decode-budget", "client"),
                            ("--speaking-glow", "off")):
            self.assertEqual(argv[argv.index(flag) + 1], value, flag)
        self.assertEqual(self.plan["fidelity_waivers"], [cq_scenario.SKIP_PAINT_WAIVER,
                                                         cq_scenario.SPEAKING_GLOW_WAIVER,
                                                         cq_scenario.PROBE_DIAG_PACKETS_OFF_WAIVER])
        self.assertEqual(self.plan["bot_diagnostics"], "off")
        self.assertIn("+ camera-off probe 3.00 cores", self.fleet.admit(self.proc("probe-00"))[1])

    def test_a_mic_on_probe_is_priced_and_launched_as_one(self):
        self.plan = plan_of(text=mic_on_pair())
        self.make()
        muted, talking = self.proc("probe-00"), self.proc("probe-01")
        self.assertEqual((self.ctx.probe_kind(muted), self.ctx.probe_kind(talking)),
                         ("camera_off", "camera_off_mic_on"))
        self.launch_all()
        runs = {a[a.index("--participant") + 1]: a for a in self.podman_runs() if "--meeting-url" in a}
        talker = next(a for p, a in runs.items() if "talking-probes" in p)
        self.assertNotIn("--join-mic-muted", talker)
        self.assertIn("--join-camera-off", talker)
        self.assertEqual(talker[talker.index("--video-mode") + 1], "clock")
        self.assertIn("--join-mic-muted", next(a for p, a in runs.items() if "talking-probes" not in p))
        self.cpu.pct = 27.0
        self.cpu()
        self.assertEqual(self.fleet.admit(talking), (False, "host 2.16 + camera-off-mic-on probe 3.20 cores > 5.30 "
                                                           "(the 70% line less 0.3 overhead, sample 4)"))
        self.assertTrue(self.fleet.admit(muted)[0])

    def test_a_mic_on_probe_without_a_cost_is_refused(self):
        with open(self.profile_path, "w", encoding="utf-8") as fh:
            json.dump(dict(PROFILE, probe_cores=dict(PROFILE["probe_cores"], camera_off_mic_on=None)), fh)
        self.plan = plan_of(text=mic_on_pair())
        self.make()
        self.clock.slept.clear()
        self.assertEqual(self.fleet.admit(self.proc("probe-01")),
                         (False, "no camera-off-mic-on probe cost in the target profile"))
        self.assertEqual(self.clock.slept, [])
        self.assertIn("probe-01: no CPU cost for this kind of probe", "; ".join(self.stack.preflight(self.plan)))

    def test_s0_runs_one_camera_off_probe_and_passes_preflight(self):
        plan = plan_of()
        self.assertEqual([(p["proc_id"], self.ctx.probe_kind(p)) for p in plan["processes"] if p["fleet"] == "browser"],
                         [("probe-00", "camera_off")])
        self.plan = plan
        self.make()
        self.assertEqual(self.stack.preflight(plan), [])


def bots_app_join_s():
    """The bots-app join helper's longest valid run, from its own constants: every attempt's timeout and click."""
    with open(os.path.join(ROOT, "e2e", "bots-app", "src", "meeting-join.ts"), encoding="utf-8") as fh:
        text = fh.read()
    const = {k: int(re.search(rf"const {k} = ([\d_]+);", text).group(1).replace("_", ""))
             for k in ("JOIN_ATTEMPTS", "JOIN_ATTEMPT_TIMEOUT_MS", "JOIN_ACTION_TIMEOUT_MS")}
    return const["JOIN_ATTEMPTS"] * (const["JOIN_ATTEMPT_TIMEOUT_MS"] + const["JOIN_ACTION_TIMEOUT_MS"]) / 1000


def scrape_s():
    """hcl-daily's Prometheus scrape interval, from its deployment values."""
    with open(os.path.join(ROOT, "helm", "global", "hcl-daily-deployment", "prometheus", "values.yaml"),
              encoding="utf-8") as fh:
        return int(re.search(r"scrape_interval: (\d+)s", fh.read()).group(1))


class JoinDeadlines(Base):
    """The real _join() against the real adapters' timeouts: every podman, healthz and Prometheus call takes the
    timeout the adapter gives it, Prometheus sees a probe one scrape after it joins, and every admission passes on
    its last sample. `rust_launch_s` and `in_meeting` set when the Rust bots launch and each probe is in the
    meeting (from its readiness deadline)."""

    def join(self, plan, rust_launch_s, in_meeting):
        self.plan, self.run_dir = plan, tempfile.mkdtemp(dir=self.tmp)
        self.set_world(world())
        self.make()
        self.ctx.secret_parent = tempfile.mkdtemp(dir=self.tmp)
        self.fleet.clock_facts()
        problems = self.stack.preflight(plan)
        if problems:
            return problems, None, None
        clock, ctx, fleet, real_sh = self.clock, self.ctx, self.fleet, cq_local.Local.sh
        procs = {ctx.container(p["proc_id"]): p for p in plan["processes"]}
        in_at, written, admitting = {}, set(), {}
        self.launch_end, self.admit_at = {}, {}
        start = clock.mono
        advance = clock.advance
        rust_ready = {}

        def elapse(seconds):
            advance(seconds)
            if "rust" not in written and clock.mono >= rust_ready.get("at", math.inf):
                written.add("rust")
                for p in (p for p in plan["processes"] if p["fleet"] == "rust"):
                    with open(os.path.join(self.run_dir, "rust", f"participants-{p['proc_id']}.json"), "w",
                              encoding="utf-8") as fh:
                        json.dump({"media_started_at": clock.wall, "participants": [
                            {"user_id": u, "join_ts": clock.wall} for u in p["participants"]]}, fh)
            for pid, at in in_at.items():
                if pid not in written and clock.mono >= at:
                    written.add(pid)
                    proc = next(p for p in plan["processes"] if p["proc_id"] == pid)
                    out = os.path.join(self.run_dir, "probes", str(proc["probe_index"]))
                    os.makedirs(os.path.join(out, "participants"), exist_ok=True)
                    with open(os.path.join(out, "participants", "bot.json"), "w", encoding="utf-8") as fh:
                        json.dump({"participant": {"user_id": proc["participants"][0],
                                                   "join_ts": clock.wall - (clock.mono - at)}}, fh)
                    open(os.path.join(out, cq_remote.KEY_READ), "w", encoding="utf-8").close()
        clock.advance = elapse

        def sh(c, argv, timeout, check=True, env=None):
            cmd, rust = argv[1:3], "--network" in argv and "--shm-size" not in argv
            if cmd == ["exec", argv[2]] and "node" in argv:
                pid = procs[argv[2]]["proc_id"]
                elapse(timeout)
                body = json.dumps({"inMeeting": int(clock.mono >= in_at.get(pid, math.inf))})
                return subprocess.CompletedProcess(argv, 0, json.dumps({"status": 200, "body": body}), "")
            if cmd == ["run", "-d"] and rust:
                elapse(rust_launch_s(timeout))
            elif argv[1] in ("exec", "inspect") or cmd == ["run", "-d"]:
                elapse(timeout)
            out = real_sh(c, argv, timeout, check, env)
            if cmd == ["run", "-d"] and rust and len(fleet.launched) == len([
                    p for p in plan["processes"] if p["fleet"] == "rust"]):
                rust_ready["at"] = start + cq_runner.RUST_READY_S - (
                    cq_runner.JOIN_POLL_S + cq_remote.FLEET_INSPECT_S + cq_remote.WATCH_S)
            if cmd == ["run", "-d"] and not rust:
                pid = procs[argv[argv.index("--name") + 1]]["proc_id"]
                self.launch_end[pid] = clock.mono
                in_at[pid] = in_meeting(pid, fleet.ready_by[pid] - cq_remote.PROBE_READY_S, fleet.ready_by[pid])
            return out

        def prom_vector(query, max_time=cq_remote.HTTP_S, timeout=None):
            elapse(max_time)
            uid = re.search(r'from_peer="([^"]+)"', query).group(1)
            pid = next(p["proc_id"] for p in plan["processes"] if uid in p["participants"])
            return {"{}": 1.0} if clock.mono >= in_at.get(pid, math.inf) + scrape_s() else {}
        real_admit, real_wait = fleet.admit, fleet._wait_watched

        def admit(proc):
            self.admit_at[proc["proc_id"]] = clock.mono
            browsers = [p["proc_id"] for p in plan["processes"] if p["fleet"] == "browser"]
            admitting.update(n=0, last=cq_remote.OWNER_SAMPLES if proc["proc_id"] == browsers[0]
                             else cq_remote.LATER_SAMPLES)
            return real_admit(proc)

        def wait_watched(until, what, done=lambda: False):
            if what == "the admission sample":
                admitting["n"] += 1
                self.cpu.pct = 25.0 if admitting["n"] == admitting["last"] else 50.0
            out = real_wait(until, what, done)
            self.cpu.pct = 25.0
            return out
        fleet.admit, fleet._wait_watched, ctx.prom_vector = admit, wait_watched, prom_vector
        self.watched = []
        self.stack._watch_round = lambda: self.watched.append(clock.mono) or elapse(cq_remote.WATCH_S) or []
        self.cpu.pct = 25.0

        class Clock:
            def now(self):
                return cq_runner.Stamp(clock.wall, clock.mono)

            def sleep(self, seconds):
                clock.sleep(seconds)
        runner = cq_runner.Runner(plan, b"", self.run_dir, stack=self.stack, fleet=fleet,
                                  tree=tsr.fakes.FakeTree(tsr.fakes.FakeClock(), self.root), clock=Clock(),
                                  stack_mode="reuse")
        runner.log = cq_runner.EventLog(os.path.join(self.run_dir, "events.jsonl"), runner._now)
        self.addCleanup(runner.log.close)
        os.makedirs(os.path.join(self.run_dir, "rust"), exist_ok=True)
        try:
            with unittest.mock.patch.object(cq_remote.Remote, "sh", sh):
                runner._join()
        except (cq_runner.Abort, cq_runner.AdapterError) as exc:
            return [], clock.mono - start, str(exc)
        self.assertEqual(set(runner.s.joined), {p["user_id"] for p in plan["participants"]})
        return [], clock.mono - start, None

    def test_each_participant_is_in_by_its_deadline_or_the_run_stops_there_and_says_who(self):
        problems, took, why = self.join(n_probes(1, 360, 900), lambda timeout: timeout / 2,
                                        lambda pid, launched, ready_by: ready_by - 1)
        self.assertEqual((problems, why), ([], None))
        self.assertLessEqual(took, 360 + cq_runner.JOIN_BUDGET_EXTRA_S)
        problems, _, _ = self.join(n_probes(1, 120), lambda timeout: 0, lambda pid, launched, ready_by: ready_by)
        self.assertIn(f"raise steps[0].join_window to at least {cq_remote.join_floor_s(plan_of())} s",
                      "; ".join(problems))
        _, took, why = self.join(plan_of(), lambda timeout: timeout, lambda pid, launched, ready_by: ready_by - 1)
        self.assertRegex(why, r"^Rust media did not start within 90 s; not joined: .*-talkers-00")
        self.assertLessEqual(took, cq_runner.RUST_READY_S)
        last_s = cq_local.DOCKER_S + bots_app_join_s()
        for probes in (1, 2, 3):
            floor = cq_remote.join_floor_s(n_probes(probes, 120))
            budget = floor + cq_runner.JOIN_BUDGET_EXTRA_S
            last = f"probe-{probes - 1:02d}"
            with self.subTest(probes=probes):
                problems, _, _ = self.join(n_probes(probes, floor - 1), lambda timeout: 0, None)
                self.assertIn(f"at least {floor} s", "; ".join(problems))
                for name, in_meeting in (
                        ("each probe in the meeting 1 s before its deadline", lambda pid, at, by: by - 1),
                        ("each probe joining as slowly as bots-app's join helper can",
                         lambda pid, at, by: at + last_s)):
                    _, took, why = self.join(n_probes(probes, floor), lambda timeout: timeout / 2, in_meeting)
                    self.assertIsNone(why, name)
                    self.assertLessEqual(took, budget, name)
                for late in (1, 30):
                    _, took, why = self.join(n_probes(probes, floor), lambda timeout: timeout / 2,
                                             lambda pid, at, by: by + late if pid == last else by - 1)
                    self.assertEqual(why, f"{last}: not in the meeting by its readiness deadline, "
                                          f"{cq_remote.PROBE_READY_S} s from its launch", late)
                    self.assertLessEqual(took, budget)
                if probes > 1:
                    stagger = int(cq_remote.MAX_STAGGER_S)
                    for joins in ("1 s after its launch", "1 s before its deadline"):
                        _, took, why = self.join(n_probes(probes, floor, stagger), lambda timeout: timeout / 2,
                                                 lambda pid, at, by: by - 1 if "deadline" in joins
                                                 else at + cq_local.DOCKER_S + 1)
                        self.assertIsNone(why, joins)
                        self.assertLessEqual(took, budget, joins)
                        for k in range(1, probes):
                            prev, pid = f"probe-{k - 1:02d}", f"probe-{k:02d}"
                            self.assertGreaterEqual(self.admit_at[pid], self.launch_end[prev] + stagger, joins)
                            self.assertLessEqual(self.admit_at[pid], self.fleet.ready_by[prev], joins)
                    problems, _, _ = self.join(n_probes(probes, floor, 900), lambda timeout: 0, None)
                    self.assertIn("probe-00: join_stagger 900 s is longer than the 175.2 s its readiness deadline "
                                  "leaves after its launch", problems)
                    self.assertNotIn(f"{last}: join_stagger", "; ".join(problems))
        self.assertLessEqual(last_s, cq_remote.PROBE_READY_S)


    def test_no_two_watches_of_a_two_probe_join_are_closer_than_the_watch_interval(self):
        _, _, why = self.join(plan_of(text=two_probes()), lambda timeout: timeout / 2,
                              lambda pid, launched, ready_by: ready_by - 1)
        self.assertIsNone(why)
        gaps = [b - a for a, b in zip(self.watched, self.watched[1:])]
        self.assertGreaterEqual(min(gaps), cq_runner.WATCH_INTERVAL_S, gaps)


class NoWindow(Base):

    def test_a_profile_without_a_window_passes_preflight_at_any_hour(self):
        for hour in (7.5, 10.5, 12, 23, 27):
            with self.subTest(hour=hour):
                self.clock.wall = T0830 - 8.5 * 3600 + hour * 3600 + 1234
                self.assertIsNone(self.stack.window)
                self.assertEqual(self.stack.preflight(self.plan), [])
                self.stack.up(self.plan, "reuse")
                self.assertEqual(self.stack.watch(), [])

    def test_without_a_window_every_lifetime_ends_at_one_run_deadline(self):
        self.fleet.clock_facts()
        self.assertEqual(self.stack.preflight(self.plan), [])
        need = int(cq_remote.run_need_s(self.plan))
        self.assertAlmostEqual(cq_remote.run_need_s(self.plan) - cq_remote.planned_s(self.plan)
                               - cq_remote.teardown_s(self.plan), cq_remote.WATCH_GAP_S)
        self.assertLess(need, cq_local.LocalFleet._ttl_s(self.fleet))
        rust = [p for p in self.plan["processes"] if p["fleet"] == "rust"]
        t0 = self.clock.now()
        self.fleet.launch(rust[0])
        self.clock.advance(30)
        self.fleet.launch(rust[1])
        self.clock.advance(40)
        self.fleet.launch(self.proc("probe-00"))
        runs = self.podman_runs()
        self.assertEqual([a[a.index("--duration") + 1] for a in runs if "--config" in a],
                         [f"{need}s", f"{need - 30}s"])
        probe = next(a for a in runs if "--meeting-url" in a)
        self.assertEqual(probe[probe.index("--ttl") + 1], f"{need - 70}s")
        self.assertEqual({a[a.index("--deadline") + 1] for a in runs}, {str(int(t0 + cq_remote.run_need_s(self.plan)))})
        for pid, left in ((rust[0]["proc_id"], need), (rust[1]["proc_id"], need - 30)):
            with open(os.path.join(self.run_dir, "in", f"{pid}.yaml"), encoding="utf-8") as fh:
                self.assertEqual(json.load(fh)["token_ttl_secs"], left, pid)
        self.clock.advance(23 * 3600)
        with self.assertRaisesRegex(cq_runner.AdapterError, "only 0 s are left before the run's deadline"):
            self.fleet._ttl_s()

    def test_a_launch_with_too_little_of_the_run_left_is_refused_not_stretched(self):
        self.fleet.clock_facts()
        self.assertEqual(self.stack.preflight(self.plan), [])
        rust = [p for p in self.plan["processes"] if p["fleet"] == "rust"]
        self.fleet.launch(rust[0])
        self.clock.advance(cq_remote.run_need_s(self.plan) - 10)
        before = len(self.podman_runs())
        with self.assertRaisesRegex(cq_runner.AdapterError, "only 10 s are left before the run's deadline"):
            self.fleet.launch(rust[1])
        self.assertEqual(len(self.podman_runs()), before)


class Meeting(Base):

    def answer(self, *extra, base=HEALTHY):
        w = self.get_world()
        w["prom"][0] = ["label_replace(", list(base) + [list(e) for e in extra]]
        self.set_world(w)

    def test_a_real_meeting_count_of_1_refuses_and_names_no_room(self):
        for extra, needle in (([{"cq": "real_sessions"}, "1"], "1 sessions outside scale rooms"),
                              ([{"cq": "real_meetings"}, "1"], "1 meetings with client packets"),
                              ([{"cq": "other_runs"}, "2"], "another scale run is live (2 rooms)")):
            with self.subTest(extra=extra):
                self.answer(extra)
                problems = "; ".join(self.stack.preflight(self.plan))
                self.assertIn(needle, problems)
        self.answer([{"cq": "real_sessions"}, "0"])
        self.assertEqual(self.stack.preflight(self.plan), [])

    def test_a_real_meeting_starting_mid_run_aborts(self):
        self.launch_all()
        self.assertEqual(self.stack.watch(), [])
        self.answer([{"cq": "real_sessions"}, "3"])
        self.clock.advance(15)
        self.assertIn("a real meeting is live (3 sessions", "; ".join(self.stack.watch()))

    def test_a_used_meeting_id_or_a_blind_session_family_refuses(self):
        w = self.get_world()
        w["prom"][1] = ["[1h]", [[{}, "1"]]]
        self.set_world(w)
        self.assertIn("already used in the last hour", "; ".join(self.stack.preflight(self.plan)))
        w["prom"][1], w["prom"][2] = ["[1h]", []], ["[7d]", []]
        self.set_world(w)
        self.assertIn("would be blind", "; ".join(self.stack.preflight(self.plan)))

    def test_a_watcher_without_data_or_a_relay_down_refuses(self):
        for base, needle in ((HEALTHY[2:], "no relay up series"), (HEALTHY[:2], "no relay slow-consumer series"),
                             ([[{"cq": "relay_up_min"}, "0"]] + HEALTHY[1:], "a relay target is down")):
            with self.subTest(needle=needle):
                self.answer(base=base)
                self.assertIn(needle, "; ".join(self.stack.preflight(self.plan)))

    def test_readiness_needs_the_ui_api_scrape_targets_and_settled_pods(self):
        fresh = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(T0830 - 300))
        cases = [({"http": {"https://ui.test.invalid/": [302, ""]}}, "UI answered 302"),
                 ({"targets": [target("relay_ws", interval="30s"), target("relay_wt"), target("metrics_api")]},
                  "relay_ws scrape target is not scraped every 15s"),
                 ({"targets": [target("relay_ws", health="down"), target("relay_wt"), target("metrics_api")]},
                  "relay_ws scrape target is not up"),
                 ({"targets": [target("relay_ws"), target("relay_wt")]}, "no metrics_api scrape target"),
                 ({"pods": [pod(k, ready=k != "nats") for k in cq_remote.POD_KINDS]}, "a nats pod is not ready"),
                 ({"pods": [pod(k, started=fresh if k == "relay_ws" else "2026-10-14T07:00:00Z")
                            for k in cq_remote.POD_KINDS]}, "a relay_ws pod started less than 10 min ago"),
                 ({"pods": [pod(k) for k in cq_remote.POD_KINDS if k != "ui"]}, "no ui pod")]
        for over, needle in cases:
            with self.subTest(needle=needle):
                self.set_world(world(**over))
                self.assertIn(needle, "; ".join(self.stack.preflight(self.plan)))

    def test_a_busy_metrics_port_refuses_the_run(self):
        self.ctx.port_open = lambda port: port == 19101
        self.assertIn("Rust metrics ports in use on 127.0.0.1: 19101", "; ".join(self.stack.preflight(self.plan)))

    def test_a_remote_probe_must_name_its_transport(self):
        with open(S0, encoding="utf-8") as fh:
            text = fh.read().replace("transport: websocket, ", "")
        plan = cq_scenario.compile_scenario(cq_scenario.parse_yaml(text), run_id=RUN_ID, scenario_file="s0.yaml",
                                            scenario_sha256="0" * 64)
        self.assertIn("probe-00: transport 'auto'; a remote probe needs websocket or webtransport",
                      "; ".join(self.stack.preflight(plan)))

    def test_a_busy_load_host_refuses_the_run(self):
        self.cpu.pct = 71.0
        self.assertIn("load host CPU is 71% before any bot", "; ".join(self.stack.preflight(self.plan)))

    def test_the_clock_must_be_within_1_s_of_prometheus(self):
        self.update(prom_time=T0830 + 1.2)
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.fleet.clock_facts()
        self.assertIn("1200.0 ms off Prometheus", str(cm.exception))
        self.update(prom_time=T0830 - 0.4)
        self.assertEqual(self.fleet.clock_facts(), {"sync": "ntp", "per_node": [{"node": "load-a",
                                                                                  "skew_ms": -400.0}]})
        self.update(prom_time=None)
        with self.assertRaises(cq_runner.AdapterError):
            self.fleet.clock_facts()


class Watch(Base):

    def setUp(self):
        super().setUp()
        self.launch_all()
        self.mark = len(self.calls("curl"))

    def answer(self, result):
        w = self.get_world()
        w["prom"][0] = ["label_replace(", result]
        self.set_world(w)

    def test_one_small_instant_query_per_round_with_a_1_s_server_timeout(self):
        self.assertEqual(self.stack.watch(), [])
        calls = self.calls("curl")[self.mark:]
        self.assertEqual(len(calls), 1)
        argv = calls[0]["argv"]
        self.assertEqual(argv[-1], "https://prom.test.invalid/api/v1/query")
        self.assertEqual(argv[argv.index("--max-time") + 1], "1.5")
        self.assertEqual(argv[argv.index("--proto") + 1], "=https")
        params = dict(p.split("=", 1) for p in [argv[i + 1] for i, a in enumerate(argv) if a == "--data-urlencode"])
        self.assertEqual(params["timeout"], "1s")
        self.assertNotRegex(params["query"], r"\[|rate\(|increase\(|:")
        self.assertIn('namespace="vc"', params["query"])
        relays = 'namespace="vc",pod=~"ws-relay-.*|wt-relay-.*"'
        for part in (f"min(up{{{relays}}})", f"count(up{{{relays}}})", f'relay_nats_events_total{{{relays},'):
            self.assertIn(part, params["query"])

    def test_the_throttle_selector_names_the_relay_and_nats_pods_only(self):
        sel = self.stack.throttle_pod_selector()
        self.assertEqual(sel, 'namespace="vc",pod=~"ws-relay-.*|wt-relay-.*|nats-[0-9]+"')
        pods = re.fullmatch(r'namespace="vc",pod=~"(.*)"', sel).group(1)
        named = [p for p in (*POD_NAMES.values(), "grafana-7fbdf4cc6b-68qkj", "postgres-postgresql-0")
                 if re.fullmatch(pods, p)]
        self.assertEqual(named, [POD_NAMES[k] for k in ("relay_ws", "relay_wt", "nats")])

    def test_a_hung_prometheus_costs_a_round_at_most_its_deadline_and_is_absorbed(self):
        self.update(curl_sleep={"label_replace(": 30})
        t = time.monotonic()
        self.assertEqual(self.stack.watch(), [])
        took = time.monotonic() - t
        self.assertLess(took, cq_runner.WATCH_BUDGET_S + 0.3)
        self.assertGreaterEqual(took, cq_remote.WATCH_DEADLINE_S)

    def test_isolated_failures_are_absorbed_until_30_s_blind(self):
        self.update(curl_fail=["label_replace("])
        self.clock.advance(15)
        self.assertEqual(self.stack.watch(), [])
        self.clock.advance(14.5)
        self.assertEqual(self.stack.watch(), [])
        self.clock.advance(0.5)
        self.assertIn("no Prometheus answer for 30 s", "; ".join(self.stack.watch()))
        self.update(curl_fail=[])
        self.assertEqual(self.stack.watch(), [])
        self.update(curl_fail=["label_replace("])
        self.clock.advance(15)
        self.assertEqual(self.stack.watch(), [])

    def test_malformed_answers_are_failures_and_only_misconfiguration_raises(self):
        vector = {"status": "success", "data": {"resultType": "vector", "result": []}}
        bad = [{"metric": {"cq": "relay_up_min"}, "value": [0, "x"]}, {"metric": ["cq"], "value": [0, "1"]},
               {"metric": {"cq": "relay_up_min"}}]
        for body in ["not json", json.dumps({"status": "error"})] + [
                json.dumps(dict(vector, data={"resultType": "vector", "result": [b]})) for b in bad]:
            with self.subTest(body=body):
                self.answer(body)
                self.assertIsNone(self.stack._values(1.5))
                self.assertEqual(self.stack.watch(), [])
        with unittest.mock.patch.dict(os.environ, {"PATH": "/nonexistent"}):
            with self.assertRaises(cq_runner.AdapterError) as cm:
                self.stack.watch()
        self.assertIn("curl could not start", str(cm.exception))

    def test_a_relay_read_down_past_one_scrape_or_a_lost_relay_aborts(self):
        down = [[{"cq": "relay_up_min"}, "0"]] + HEALTHY[1:]
        for gaps in ((6.8,), (15.0, 14.0), (cq_remote.RELAY_DOWN_S - 0.1,)):
            with self.subTest(gaps=gaps):
                self.answer(down)
                self.assertEqual(self.stack.watch(), [])
                for gap in gaps:
                    self.clock.advance(gap)
                    self.assertEqual(self.stack.watch(), [])
                self.answer(HEALTHY)
                self.assertEqual(self.stack.watch(), [])
                self.clock.advance(1)
        self.answer(down)
        self.assertEqual(self.stack.watch(), [])
        self.clock.advance(cq_remote.RELAY_DOWN_S)
        self.assertIn(f"a relay target has read down for {cq_remote.RELAY_DOWN_S:.0f} s, past one scrape",
                      "; ".join(self.stack.watch()))
        self.answer(HEALTHY)
        self.assertEqual(self.stack.watch(), [])
        self.answer([HEALTHY[0], [{"cq": "relay_up_count"}, "1"], HEALTHY[2]])
        self.assertIn("1 relay targets, 2 before the run", "; ".join(self.stack.watch()))

    def test_a_rise_in_relay_slow_consumers_aborts_and_a_counter_reset_rebases(self):
        self.answer(HEALTHY[:2] + [[{"cq": "slow_consumers"}, "0"]])
        self.assertEqual(self.stack.watch(), [])
        self.answer(HEALTHY[:2] + [[{"cq": "slow_consumers"}, "1"]])
        self.assertIn("relay NATS slow consumers rose by 1", "; ".join(self.stack.watch()))
        self.answer(HEALTHY[:2] + [[{"cq": "slow_consumers"}, "6"]])
        self.assertIn("rose by 6", "; ".join(self.stack.watch()))

    def test_a_failed_relay_scrape_that_drops_its_slow_consumer_series_neither_rebases_nor_aborts(self):
        self.answer([[{"cq": "relay_up_min"}, "0"], HEALTHY[1], [{"cq": "slow_consumers"}, "1"]])
        self.assertEqual(self.stack.watch(), [])
        self.answer(HEALTHY)
        self.assertEqual(self.stack.watch(), [])

    def test_a_room_that_never_shows_in_prometheus_is_a_blind_watch(self):
        limit = self.plan["steps"][0]["join_window_s"] + cq_runner.JOIN_BUDGET_EXTRA_S + cq_remote.BLIND_S
        self.assertEqual(self.stack.watch(), [])
        self.clock.advance(limit)
        self.assertEqual(self.stack.watch(), [])
        self.clock.advance(1)
        self.assertIn("never showed this run's 12 sessions in its room (at most 0)", "; ".join(self.stack.watch()))
        self.answer(HEALTHY + [[{"cq": "own_sessions"}, "12"]])
        self.assertEqual(self.stack.watch(), [])

    def test_the_load_host_above_70_percent_for_60_s_aborts(self):
        self.cpu.pct = 75.0
        self.stack.watch()
        for _ in range(3):
            self.clock.advance(15)
            self.assertEqual(self.stack.watch(), [])
        self.clock.advance(15)
        self.assertIn("load host CPU 75% over the last 60 s, above the 70% line", "; ".join(self.stack.watch()))

    def test_the_70_percent_line_is_a_mean_so_a_dip_does_not_reset_it(self):
        self.stack.watch()
        out = []
        for k in range(8):
            self.cpu.pct = (66.0, 84.0)[k % 2]
            self.clock.advance(15)
            out += self.stack.watch()
        self.assertTrue(any("above the 70% line" in r for r in out), out)
        self.assertFalse(any("hard stop" in r for r in out), out)
        self.cpu.pct = 65.0
        for _ in range(5):
            self.clock.advance(15)
            out = self.stack.watch()
        self.assertEqual(out, [])

    def test_one_busy_interval_inside_a_quiet_minute_is_not_the_70_percent_line(self):
        self.stack.watch()
        out = []
        for pct in (60.0, 60.0, 60.0, 80.0):
            self.cpu.pct = pct
            self.clock.advance(15)
            out += self.stack.watch()
        self.assertEqual(out, [])

    def test_the_load_host_above_85_percent_is_a_hard_stop_at_once(self):
        self.stack.watch()
        self.cpu.pct = 86.0
        self.clock.advance(15)
        self.assertIn("load host CPU 86% over the last 15 s, above the 85% hard stop", "; ".join(self.stack.watch()))



class Drain(Base):

    def drain(self, seq):
        w = self.get_world()
        w["prom"][0] = ["label_replace(", {"seq": seq}]
        self.set_world(w)

    def own(self, n, base=HEALTHY):
        return list(base) + ([[{"cq": "own_sessions"}, str(n)]] if n is not None else [])

    def test_the_room_drains_when_its_sessions_reach_0_or_vanish_with_every_relay_up(self):
        for seq in ([self.own(4), self.own(4), self.own(None)], [self.own(2), self.own(0)]):
            with self.subTest(seq=seq):
                self.drain(seq)
                self.clock.slept.clear()
                self.assertEqual(self.stack.drained(), [])
                self.assertEqual(len(self.clock.slept), len(seq) - 1)

    def test_a_room_still_busy_after_the_drain_wait_or_a_blind_drain_check_is_a_problem(self):
        self.drain([self.own(3)])
        start = self.clock.mono
        self.assertEqual(self.stack.drained(), ["the room still had 3 sessions 90 s after the stops"])
        self.assertLessEqual(self.clock.mono - start, cq_remote.DRAIN_S + cq_remote.DRAIN_POLL_S)
        self.update(curl_fail=["label_replace("])
        self.assertEqual(self.stack.drained(), [DRAIN_BLIND])

    def test_an_absent_room_never_seen_with_a_session_is_not_drained(self):
        self.drain([self.own(None)])
        self.assertEqual(self.stack.drained(), ["the room was never seen with a session, so its absence for 90 s does "
                                                "not show it drained"])
        self.stack.own_max = 1
        self.assertEqual(self.stack.drained(), [])

    def test_a_room_seen_during_the_run_drains_when_its_series_is_gone(self):
        self.drain([self.own(3)])
        self.assertEqual(self.stack.watch(), [])
        self.drain([self.own(None)])
        self.clock.slept.clear()
        self.assertEqual(self.stack.drained(), [])
        self.assertEqual(self.clock.slept, [])

    def test_a_relay_lost_after_a_busy_reading_is_reported_as_unconfirmed(self):
        self.stack.relay_count = 2
        self.drain([self.own(2), self.own(None, [[{"cq": "relay_up_min"}, "1"], [{"cq": "relay_up_count"}, "1"]])])
        self.assertEqual(self.stack.drained(), [DRAIN_BLIND])

    def test_an_absent_room_with_a_relay_down_or_missing_is_not_drained(self):
        self.stack.relay_count, self.stack.own_max = 2, 3
        for base in ([], [[{"cq": "relay_up_min"}, "0"], [{"cq": "relay_up_count"}, "2"]],
                     [[{"cq": "relay_up_min"}, "1"], [{"cq": "relay_up_count"}, "1"]]):
            with self.subTest(base=base):
                self.drain([self.own(None, base)])
                self.assertEqual(self.stack.drained(), [DRAIN_BLIND])


class PodmanCompat(Base):

    def test_an_offset_finished_at_parses(self):
        z = cq_local.docker_time("2026-10-14T08:40:00.25Z")
        self.assertEqual(cq_local.docker_time("2026-10-14T15:40:00.25+07:00"), z)
        self.assertEqual(cq_local.docker_time("2026-10-14T05:10:00.25-03:30"), z)
        self.assertEqual(cq_local.docker_time("2026-10-14T15:40:00.123456789+07:00"), z - 0.25 + 0.123456789)
        self.assertIsNone(cq_local.docker_time("0001-01-01T00:00:00Z"))
        self.assertIsNone(cq_local.docker_time("2026-10-14T15:40:00+0700"))

    def test_podman_state_keys_give_each_exit_its_own_time_and_code(self):
        self.launch_all()
        for p in self.plan["processes"]:
            ex = self.fleet.stop(p)
            self.assertEqual((ex.proc_id, ex.exited_at, ex.exit_code),
                             (p["proc_id"], cq_local.docker_time("2026-10-14T08:40:00.25Z"), 0))

    def test_podmans_lowercase_no_such_container_is_absent_not_an_error(self):
        self.assertEqual(self.ctx.inspect(["cq-gone"]), {})
        self.launch_all()
        self.update(containers={})
        self.assertEqual([(e.proc_id, e.exited_at) for e in self.fleet.poll()],
                         [(p["proc_id"], None) for p in self.plan["processes"]])

    def test_the_owner_wait_outlasts_the_bots_app_join_budget(self):
        with open(os.path.join(ROOT, "e2e", "bots-app", "src", "meeting-join.ts"), encoding="utf-8") as fh:
            text = fh.read()
        attempts = int(re.search(r"const JOIN_ATTEMPTS = (\d+);", text).group(1))
        each_ms = int(re.search(r"const JOIN_ATTEMPT_TIMEOUT_MS = ([\d_]+);", text).group(1).replace("_", ""))
        self.assertGreater(cq_local.OWNER_WAIT_S, attempts * each_ms / 1000 + 30)


class Leaks(unittest.TestCase):

    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir, True)
        os.makedirs(os.path.join(self.dir, "logs"))
        self.put("run.json", b"{}")

    def put(self, rel, data):
        path = os.path.join(self.dir, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as fh:
            fh.write(data)
        return path

    def leaks(self, data, name="logs/x.log", tokens=()):
        path = self.put(name, data)
        try:
            return "; ".join(cq_remote.leak_scan(self.dir, SECRET, tokens))
        finally:
            os.remove(path)

    def test_the_key_in_any_encoded_form_is_a_leak(self):
        raw = SECRET.encode()
        forms = {"raw": raw, "hex": raw.hex().encode(), "HEX": raw.hex().upper().encode(), "b64": base64.b64encode(raw),
                 "b64url": base64.urlsafe_b64encode(raw),
                 "json-utf8": json.dumps(SECRET, ensure_ascii=False)[1:-1].encode(),
                 "b64-nopad": base64.b64encode(raw).rstrip(b"="),
                 "b64url-nopad": base64.urlsafe_b64encode(raw).rstrip(b"="),
                 "json": json.dumps(SECRET).encode()[1:-1], "url": urllib.parse.quote(SECRET, safe="").encode(),
                 "url-lower": re.sub(r"%[0-9A-F]{2}", lambda m: m.group(0).lower(),
                                     urllib.parse.quote(SECRET, safe="")).encode(),
                 "url-plus": urllib.parse.quote_plus(SECRET).encode(), "utf-16le": SECRET.encode("utf-16-le")}
        for prefix in (b"", b"X=", b"Y", b"JWT_SECRET="):
            for enc in (base64.b64encode, base64.urlsafe_b64encode):
                forms[f"{enc.__name__}({prefix!r}+key)"] = enc(prefix + raw)
                forms[f"{enc.__name__}({prefix!r}+key+tail)"] = enc(prefix + raw + b"\xff\xff")
        for name, data in forms.items():
            with self.subTest(form=name):
                self.assertIn("logs/x.log.gz carry the signing key",
                              self.leaks(gzip.compress(b"prefix " + data + b" suffix"), "logs/x.log.gz"))
                self.assertIn("logs/x.log carry the signing key", self.leaks(gzip.compress(data)))
        self.assertEqual(cq_remote.leak_scan(self.dir, SECRET), [])

    def test_a_jwt_carrying_the_key_at_any_alignment_is_a_leak_not_redacted(self):
        head = base64.urlsafe_b64encode(b'{"alg":"HS256"}').rstrip(b"=")
        for pad in ("k", "kk", "kkk"):
            with self.subTest(pad=pad):
                body = base64.urlsafe_b64encode(json.dumps({pad: SECRET}).encode()).rstrip(b"=")
                token = head + b"." + body + b".c2lnbmF0dXJlLWJ5dGVz"
                path = self.put("probes/0/console.txt", b"GET /lobby?token=" + token)
                self.assertIn("probes/0/console.txt carry the signing key",
                              "; ".join(cq_remote.leak_scan(self.dir, SECRET)))
                with open(path, "rb") as fh:
                    self.assertIn(token, fh.read())
                os.remove(path)

    def test_room_token_jwts_are_redacted_in_place_not_called_a_leak(self):
        jwt = fake_jwt("bot-1")
        self.put("logs/probe-00.log.gz", gzip.compress(b"GET wss://x/lobby?token=" + jwt + b" 101"))
        self.put("probes/0/console.txt", b'{"cookie": "' + jwt + b'"}')
        events = self.put("events.jsonl", b'{"note": "' + jwt + b'"}\n')
        before = os.stat(events).st_ino
        with open(events, "a", encoding="utf-8") as held:
            self.assertEqual(cq_remote.leak_scan(self.dir, SECRET), [])
            held.write('{"type": "done"}\n')
        with gzip.open(os.path.join(self.dir, "logs", "probe-00.log.gz")) as fh:
            self.assertEqual(fh.read(), b"GET wss://x/lobby?token=<jwt> 101")
        with open(events, "rb") as fh:
            self.assertEqual((os.stat(events).st_ino, fh.read()), (before, b'{"note": "<jwt>"}\n{"type": "done"}\n'))
        with open(os.path.join(self.dir, cq_remote.LEAK_SUMMARY), encoding="utf-8") as fh:
            self.assertEqual(json.load(fh)["jwt_redacted"],
                             ["events.jsonl", "logs/probe-00.log.gz", "probes/0/console.txt"])

    def test_a_planted_link_never_makes_the_scan_write_outside_the_run_folder(self):
        outside = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, outside)
        victim = os.path.join(outside, "authorized_keys")
        with open(victim, "wb") as fh:
            fh.write(b"ssh-ed25519 AAAA khai\n")
        jwt = fake_jwt("bot-1")
        self.put("probes/0/evil.log", b"owned " + jwt)
        for name in ("evil.log.cq-tmp", "evil.log.tmp", "link.log"):
            os.symlink(victim, os.path.join(self.dir, "probes", "0", name))
        os.symlink(outside, os.path.join(self.dir, "probes", "0", "dirlink"))
        problems = "; ".join(cq_remote.leak_scan(self.dir, SECRET))
        with open(victim, "rb") as fh:
            self.assertEqual(fh.read(), b"ssh-ed25519 AAAA khai\n")
        self.assertEqual(sorted(os.listdir(outside)), ["authorized_keys"])
        self.assertIn("not single-link regular files", problems)
        for name in ("evil.log.cq-tmp", "link.log", "dirlink"):
            self.assertIn(f"probes/0/{name}", problems)
        with open(os.path.join(self.dir, "probes", "0", "evil.log"), "rb") as fh:
            self.assertEqual(fh.read(), b"owned <jwt>")

    def test_the_scan_fails_closed(self):
        cases = [(lambda: self.put("logs/bad.log", cq_remote.GZIP_MAGIC + b"not gzip"),
                  "logs/bad.log could not be read"),
                 (lambda: os.symlink("/etc/hostname", os.path.join(self.dir, "link")), "link are not single-link"),
                 (lambda: os.mkfifo(os.path.join(self.dir, "fifo")), "fifo are not single-link"),
                 (lambda: os.link(self.put("a.json", b"{}"), os.path.join(self.dir, "b.json")),
                  "a.json, b.json are not"),
                 (lambda: self.put("tok.txt", b"ctl abcdef0123456789abcdef0123456789"),
                  "tok.txt carry a control token")]
        if os.geteuid() != 0:
            cases.append((lambda: os.chmod(self.put("locked.json", b"{}"), 0), "locked.json could not be read"))
        for setup, needle in cases:
            with self.subTest(needle=needle):
                setup()
                self.assertIn(needle, "; ".join(cq_remote.leak_scan(self.dir, SECRET,
                                                                    ["abcdef0123456789abcdef0123456789"])))
                for name in ("logs/bad.log", "link", "fifo", "a.json", "b.json", "tok.txt", "locked.json"):
                    with contextlib.suppress(FileNotFoundError):
                        os.remove(os.path.join(self.dir, name))
        empty = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, empty)
        self.assertEqual(cq_remote.leak_scan(empty, SECRET), ["the run folder has no files to scan"])
        self.assertEqual(cq_remote.leak_scan(self.dir, None), ["no signing key to scan for"])


class RunnerLeakGate(unittest.TestCase):

    def test_a_planted_secret_in_the_run_folder_ends_the_run_error_leak(self):
        for plant in (SECRET.encode(), base64.b64encode(SECRET.encode()), json.dumps(SECRET).encode()):
            with self.subTest(plant=plant):
                h = tst.harness(self)
                h.fleet.leaks = lambda run_dir: cq_remote.leak_scan(run_dir, SECRET)
                collect = h.fleet.collect

                def planted(run_dir, collect=collect, plant=plant):
                    collect(run_dir)
                    with open(os.path.join(run_dir, "logs", "probe-00.log"), "wb") as fh:
                        fh.write(plant)
                h.fleet.collect = planted
                run = h.run()
                self.assertEqual((run["verdict"], run["exit_code"]), ("ERROR", 3))
                self.assertTrue(run["error"].startswith("LEAK: logs/probe-00.log carry the signing key"), run["error"])
                with open(os.path.join(h.dir, "run.json"), encoding="utf-8") as fh:
                    self.assertEqual(json.load(fh)["verdict"], "ERROR")
                self.assertEqual(h.lines("done")[0]["verdict"], "ERROR")

    def test_a_clean_folder_keeps_the_verdict_and_writes_the_scan_summary(self):
        h = tst.harness(self)
        h.fleet.leaks = lambda run_dir: cq_remote.leak_scan(run_dir, SECRET)
        run = h.run()
        self.assertEqual((run["verdict"], run["error"]), ("INVALID", None))
        with open(os.path.join(h.dir, cq_remote.LEAK_SUMMARY), encoding="utf-8") as fh:
            self.assertGreater(json.load(fh)["files"], 5)

    def test_a_failing_or_malformed_leak_check_is_an_error(self):
        for leaks, needle in ((cq_runner.AdapterError("disk gone"), "the leak check failed (disk gone)"),
                              (None, "fleet leaks() must return a list of non-blank strings"),
                              ([""], "fleet leaks() must return")):
            with self.subTest(leaks=leaks):
                h = tst.harness(self, fleet={"leaks": [leaks]})
                run = h.run()
                self.assertEqual(run["verdict"], "ERROR")
                self.assertIn(needle, run["error"])

    def test_a_leak_found_only_in_the_record_withholds_it(self):
        h = tst.harness(self, fleet={"leaks": [[], ["run.json carry the signing key"]]})
        run = h.run()
        self.assertEqual((run["verdict"], run["exit_code"], run["gates"]), ("ERROR", 3, []))
        self.assertEqual(run["error"], "LEAK: run.json carry the signing key; the record was withheld")
        with open(os.path.join(h.dir, "run.json"), encoding="utf-8") as fh:
            self.assertEqual(json.load(fh), run)
        self.assertEqual(h.lines("done")[-1]["verdict"], "ERROR")

    def test_a_shared_target_needs_a_fleet_with_a_leak_check(self):
        h = tst.harness(self, fleet={"leaks": None}, wrap=tst.Recording)
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("its fleet adapter has no leaks()", run["error"])
        self.assertEqual((h.runner.stack.used, h.runner.fleet.used), ([], []))

    def test_the_leftover_secret_folder_is_a_leak(self):
        with tempfile.TemporaryDirectory() as tmp:
            fleet = unittest.mock.Mock(spec=cq_remote.RemoteFleet)
            fleet.ctx = unittest.mock.Mock(jwt_secret=SECRET, tokens={}, secret_dir=lambda pid=None: tmp)
            with open(os.path.join(tmp, "a.json"), "w", encoding="utf-8") as fh:
                fh.write("{}")
            self.assertIn("the run's tmpfs secret folder still exists",
                          cq_remote.RemoteFleet.leaks(fleet, tmp))


class FleetBounds(Base):

    def test_poll_absorbs_an_isolated_inspect_failure_and_raises_after_30_s(self):
        self.launch_all()
        self.update(inspect_fail=True)
        self.assertEqual(self.fleet.poll(), [])
        self.clock.advance(cq_remote.BLIND_S - 1)
        self.assertEqual(self.fleet.poll(), [])
        self.clock.advance(1)
        with self.assertRaises(cq_runner.AdapterError):
            self.fleet.poll()
        self.update(inspect_fail=False)
        self.assertEqual(self.fleet.poll(), [])

    def test_a_container_that_ignores_stop_is_killed_before_the_drain(self):
        self.launch_all()
        stuck = f"cq-{RUN_ID}-rust-pub"
        self.update(stuck=[stuck])
        for p in self.plan["processes"]:
            self.assertIsNotNone(self.fleet.stop(p).exit_code)
        kills = [r["argv"] for r in self.calls("podman") if r["argv"][0] == "kill"]
        self.assertEqual(kills, [["kill", "--signal", "KILL", stuck]])
        inspects = [r["argv"] for r in self.calls("podman") if r["argv"][0] == "inspect"]
        self.assertEqual([len(a) - 3 for a in inspects[-3:]], [2, 2, 1])

    def test_join_checks_are_sticky_and_spaced(self):
        self.launch_all()
        self.write_joins()
        self.update(prom=[["videocall_peer_can_listen", []]] + world()["prom"])
        def execs():
            return len([r for r in self.calls("podman") if r["argv"][0] == "exec" and "node" in r["argv"]])

        def asks():
            return [r["argv"] for r in self.calls("curl") if any("peer_can_listen" in a for a in r["argv"])]
        before = execs()
        for _ in range(4):
            self.fleet.joined()
            self.clock.advance(1)
        self.assertEqual(execs() - before, 1)
        self.assertEqual(len(asks()), 2)
        argv = asks()[0]
        self.assertEqual(argv[argv.index("--max-time") + 1], str(cq_remote.JOIN_PROM_S))
        self.assertIn('namespace="vc"', " ".join(argv))
        self.clock.advance(cq_remote.JOIN_PROM_EVERY_S)
        self.fleet.joined()
        self.assertEqual(len(asks()), 4)
        self.assertEqual(self.fleet.CTL_S, cq_remote.CTL_S)

    def test_the_owner_wait_ends_at_the_owners_readiness_deadline_and_says_so(self):
        self.fleet.clock_facts()
        self.assertEqual(self.stack.preflight(self.plan), [])
        self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 0})}})
        launched = self.clock.mono
        self.fleet.launch(self.proc("probe-00"))
        argv = self.podman_runs()[-1]
        deadline = self.fleet.ready_by["probe-00"]
        self.assertEqual(deadline, launched + cq_remote.PROBE_READY_S)
        self.assertEqual(argv[argv.index("--join-deadline") + 1],
                         str(int(self.clock.now() + cq_remote.PROBE_READY_S)))
        self.clock.advance(150)
        with self.assertRaisesRegex(cq_runner.AdapterError, "probe-00 owns the meeting and was not in it by its "
                                                            "readiness deadline, 240 s from its launch"):
            self.fleet.launch(self.proc("probe-01"))
        self.assertGreaterEqual(self.clock.mono, deadline)
        self.assertLessEqual(self.clock.mono, deadline + cq_local.OWNER_POLL_S)

    def test_a_launch_gets_only_what_is_left_of_its_readiness_deadline(self):
        self.fleet.clock_facts()
        rust = [p for p in self.plan["processes"] if p["fleet"] == "rust"]
        timeouts = []
        real = cq_local.Local.sh

        def sh(c, argv, timeout, check=True, env=None):
            if argv[1:3] == ["run", "-d"]:
                timeouts.append(timeout)
            return real(c, argv, timeout, check, env)
        with unittest.mock.patch.object(cq_remote.Remote, "sh", sh):
            self.fleet.launch(rust[0])
            self.clock.advance(cq_runner.RUST_READY_S - 10)
            self.fleet.launch(rust[1])
            self.clock.advance(10)
            with self.assertRaisesRegex(cq_runner.AdapterError,
                                        f"^{rust[0]['proc_id']}: its readiness deadline passed before its launch$"):
                self.fleet._launch_s(rust[0]["proc_id"])
        self.assertEqual(timeouts, [cq_local.DOCKER_S, 10])

    def test_a_probe_is_in_the_meeting_only_if_its_own_record_has_it_joined_by_its_deadline(self):
        self.fleet.clock_facts()
        self.fleet.launch(self.proc("probe-00"))
        self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}})
        record = os.path.join(self.run_dir, "probes", "0", "participants", "bot.json")
        os.makedirs(os.path.dirname(record))
        uid = self.proc("probe-00")["participants"][0]
        self.assertEqual(self.fleet.join_by["probe-00"], int(self.clock.now() + cq_remote.PROBE_READY_S))
        self.clock.advance(cq_remote.PROBE_READY_S + 1)
        for join_ts, seen in ((self.fleet.join_by["probe-00"] + 1, False), (self.fleet.join_by["probe-00"], True)):
            with open(record, "w", encoding="utf-8") as fh:
                json.dump({"participant": {"user_id": uid, "join_ts": join_ts}}, fh)
            self.assertEqual(self.fleet._in_meeting("probe-00"), seen, join_ts)

    def test_a_probe_healthz_does_not_show_in_after_its_deadline_stops_the_run_whatever_its_record_says(self):
        self.fleet.clock_facts()
        self.fleet.launch(self.proc("probe-00"))
        self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 0})}})
        record = os.path.join(self.run_dir, "probes", "0", "participants", "bot.json")
        os.makedirs(os.path.dirname(record))
        with open(record, "w", encoding="utf-8") as fh:
            json.dump({"participant": {"user_id": self.proc("probe-00")["participants"][0],
                                       "join_ts": self.fleet.join_by["probe-00"] - 10}}, fh)
        self.clock.advance(cq_remote.PROBE_READY_S + 1)
        with self.assertRaisesRegex(cq_runner.AdapterError, "^probe-00: not in the meeting by its readiness "
                                                            "deadline, 240 s from its launch$"):
            self.fleet.joined()

    def test_a_watch_that_ends_past_the_owners_deadline_still_accepts_an_owner_in_by_then(self):
        self.fleet.clock_facts()
        self.fleet.launch(self.proc("probe-00"))
        deadline = self.fleet.ready_by["probe-00"]
        record = os.path.join(self.run_dir, "probes", "0", "participants", "bot.json")
        os.makedirs(os.path.dirname(record))
        with open(record, "w", encoding="utf-8") as fh:
            json.dump({"participant": {"user_id": self.proc("probe-00")["participants"][0],
                                       "join_ts": self.fleet.join_by["probe-00"] - 1}}, fh)
        self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 0})}})

        def watch():
            self.clock.advance(cq_remote.WATCH_S)
            self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}})
            return []
        self.stack.watch = watch
        self.clock.advance(deadline - cq_runner.WATCH_INTERVAL_S - 0.5 - self.clock.mono)
        self.fleet._await_in_meeting("probe-00", "probe-01")
        self.assertGreater(self.clock.mono, deadline)
        self.assertIn("probe-00", self.fleet.in_meeting)

    def test_a_healthz_call_gets_only_what_is_left_of_the_join_budget(self):
        self.fleet.clock_facts()
        self.fleet.launch(self.proc("probe-00"))
        timeouts = []
        real = cq_local.Local.sh

        def sh(c, argv, timeout, check=True, env=None):
            timeouts.append(timeout)
            if timeout < cq_remote.CTL_S:
                raise cq_runner.AdapterError("podman exec timed out")
            return real(c, argv, timeout, check, env)
        with unittest.mock.patch.object(cq_remote.Remote, "sh", sh):
            self.clock.advance(self.fleet._budget_left() - 2)
            self.assertFalse(self.fleet._in_meeting("probe-00"))
            self.clock.advance(2)
            self.assertFalse(self.fleet._in_meeting("probe-00"))
        self.assertEqual(timeouts, [2])

    def test_the_env_check_gets_only_what_is_left_of_the_join_budget(self):
        self.launch_all()
        self.write_joins()
        budget = self.plan["steps"][0]["join_window_s"] + cq_runner.JOIN_BUDGET_EXTRA_S
        timeouts = []
        real = cq_local.Local.sh

        def sh(c, argv, timeout, check=True, env=None):
            if argv[1] == "exec" and "sh" in argv:
                timeouts.append(timeout)
            return real(c, argv, timeout, check, env)
        with unittest.mock.patch.object(cq_remote.Remote, "sh", sh):
            self.clock.advance(budget - 10 - (self.clock.mono - self.fleet.join_mono))
            self.fleet._env_check("probe-00")
            self.clock.advance(10)
            with self.assertRaisesRegex(cq_runner.AdapterError,
                                        "^probe-01: the join budget ran out before its environment check$"):
                self.fleet._env_check("probe-01")
        self.assertEqual(timeouts, [10])

    def test_the_owner_wait_is_watched_and_ends_inside_the_join_budget(self):
        self.fleet.clock_facts()
        self.assertEqual(self.stack.preflight(self.plan), [])
        self.update(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 0})}})
        self.fleet.launch(self.proc("probe-00"))
        start = self.clock.mono
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.fleet.launch(self.proc("probe-01"))
        step = self.plan["steps"][0]
        waited = self.clock.mono - start
        self.assertLess(waited, step["join_window_s"] + cq_runner.JOIN_BUDGET_EXTRA_S)
        self.assertIn("by its readiness deadline", str(cm.exception))
        watched = [r for r in self.calls("curl") if any(a.startswith("query=label_replace(") for a in r["argv"])]
        self.assertGreaterEqual(len(watched), int(waited // cq_runner.WATCH_INTERVAL_S))
        w = self.get_world()
        w["prom"][0] = ["label_replace(", HEALTHY + [[{"cq": "real_sessions"}, "2"]]]
        self.set_world(w)
        self.fleet.launched.remove("probe-01")
        self.fleet.ready_by["probe-00"] = self.clock.mono + cq_remote.PROBE_READY_S
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.fleet.launch(self.proc("probe-01"))
        self.assertIn("stack watch during the owner wait: a real meeting is live", str(cm.exception))

    def test_the_e2e_tree_is_mounted_read_only_into_every_container(self):
        self.launch_all()
        self.fleet.generator_verdicts(T0830, T0830 + 180)
        mounts = [a[i + 1] for a in (r["argv"] for r in self.calls("podman") if r["argv"][0] == "run")
                  for i, x in enumerate(a) if x == "-v" and ":/app/e2e" in a[i + 1]]
        self.assertEqual(len(mounts), 3)
        self.assertTrue(all(m.endswith(":/app/e2e:ro,z") for m in mounts), mounts)

    def test_a_probes_key_file_goes_as_soon_as_its_preload_read_it(self):
        self.launch_all()
        self.assertTrue(os.path.isdir(self.ctx.secret_dir("probe-00")))
        open(os.path.join(self.run_dir, "probes", "0", cq_remote.KEY_READ), "w", encoding="utf-8").close()
        self.clock.advance(cq_remote.LOAD_EVERY_S)
        self.fleet.poll()
        self.assertFalse(os.path.exists(self.ctx.secret_dir("probe-00")))
        self.assertTrue(os.path.isdir(self.ctx.secret_dir("probe-01")))


class LoadCadence(Base):

    def test_the_load_line_samples_on_its_own_and_a_hot_sample_stops_the_run_at_once(self):
        self.launch_all()
        for _ in range(12):
            self.clock.advance(cq_remote.LOAD_EVERY_S)
            self.fleet.poll()
        self.cpu.pct = 90.0
        self.clock.advance(cq_remote.LOAD_EVERY_S)
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.fleet.poll()
        self.assertIn("load host CPU 90% over the last 5 s, above the 85% hard stop", str(cm.exception))

    def test_the_load_line_reads_proc_stat_at_most_every_5_s(self):
        reads = []
        line = cq_remote.LoadLine(lambda: reads.append(self.clock.mono) or self.cpu())
        for _ in range(12):
            line.sample(self.clock.mono)
            self.clock.advance(1)
        self.assertEqual([round(r - reads[0]) for r in reads], [0, 5, 10])

    def test_the_70_percent_line_is_exact(self):
        for pct, aborts in ((71.0, True), (69.0, False)):
            with self.subTest(pct=pct):
                self.ctx.load = cq_remote.LoadLine(self.cpu)
                self.cpu.pct = pct
                out = []
                for _ in range(14):
                    out += self.ctx.load.check(self.clock.mono)
                    self.clock.advance(cq_remote.LOAD_EVERY_S)
                self.assertEqual(any("above the 70% line" in r for r in out), aborts, out)


class Images(Base):

    def test_the_remote_stack_never_builds_and_refuses_a_missing_or_foreign_bot_image(self):
        for labels in ("", "e" * 40 + "\tclean", "f" * 40 + "\tdirty"):
            with self.subTest(labels=labels):
                self.update(bot_labels=labels)
                self.assertIn("no bot image videocall-bot:cq-ffffffffffff built from this checkout",
                              "; ".join(self.stack.preflight(self.plan)))
        self.update(bot_labels="f" * 40 + "\tclean")
        self.assertEqual(self.stack.preflight(self.plan), [])
        self.stack.up(self.plan, "reuse")
        self.assertFalse([r for r in self.calls("podman") if r["argv"][0] == "build"])

    def test_the_bot_image_needs_the_deadline_option(self):
        for help_text, ok in (("  --video-mode <ekg|replay>\n  --diagnostics <on|off>", False),
                              ("  --video-mode <ekg|replay>\n  --diagnostics <on|off>\n  --deadline-ms <n>", False),
                              ("  --video-mode <ekg|replay>\n  --diagnostics <on|off>\n  --deadline <s>", True)):
            self.update(bot_help=help_text)
            problems = "; ".join(self.stack.preflight(self.plan))
            self.assertEqual("has no --deadline option" not in problems, ok, help_text)

    def test_either_diagnostics_value_needs_a_bot_image_with_the_switch(self):
        self.assertEqual(self.plan["bot_diagnostics"], "off")
        for help_text in ("  --video-mode <ekg|replay>\n  --deadline <s>",
                          "  --video-mode <ekg|replay>\n  --diagnostics-interval-ms <ms>\n  --deadline <s>"):
            self.update(bot_help=help_text)
            self.assertIn("has no --diagnostics switch", "; ".join(self.stack.preflight(self.plan)), help_text)
        self.update(bot_help="  --video-mode <ekg|replay>\n  --deadline <s>")
        for plan in (dict(self.plan, video_mode="ekg"), dict(self.plan, video_mode="ekg", bot_diagnostics="on")):
            self.make(plan)
            self.assertIn(f"run.bot_diagnostics is {plan['bot_diagnostics']}", "; ".join(self.stack.preflight(plan)))
        self.update(bot_help="  --video-mode <ekg|replay>\n  --diagnostics <on|off>\n  --deadline <s>")
        self.assertEqual(self.stack.preflight(plan), [])

    def test_replay_needs_a_bot_image_with_the_replay_source(self):
        self.assertEqual(self.plan["video_mode"], "replay")
        self.update(bot_help="  --video-mode <ekg|costume>")
        self.assertIn("has no replay video source", "; ".join(self.stack.preflight(self.plan)))
        self.update(bot_help="  --video-mode <ekg|costume|replay>\n  --diagnostics <on|off>\n  --deadline <s>")
        self.assertEqual(self.stack.preflight(self.plan), [])
        self.update(prom_time=self.clock.wall)
        self.launch_all()
        with open(os.path.join(self.run_dir, "in", "rust-pub.yaml"), encoding="utf-8") as fh:
            self.assertEqual(json.load(fh)["video_mode"], "replay")


class Session(Base):

    def test_linger_must_be_on(self):
        self.update(linger="no")
        self.assertIn("linger is off for this user", "; ".join(self.stack.preflight(self.plan)))

    def test_a_runner_outside_tmux_is_warned(self):
        err = io.StringIO()
        with unittest.mock.patch.dict(os.environ, {"TMUX": "", "STY": ""}), contextlib.redirect_stderr(err), \
                unittest.mock.patch.object(cq_remote.signal, "getsignal", lambda sig: cq_remote.signal.SIG_DFL):
            self.stack.preflight(self.plan)
        self.assertIn("warning: the runner is not under tmux, screen or nohup", err.getvalue())
        err = io.StringIO()
        with unittest.mock.patch.dict(os.environ, {"TMUX": "/tmp/tmux-1/default,1,0"}), contextlib.redirect_stderr(err):
            self.stack.preflight(self.plan)
        self.assertNotIn("warning:", err.getvalue())

    def test_proxies_reach_no_child_and_curl_ignores_curlrc(self):
        with unittest.mock.patch.dict(os.environ, {"HTTPS_PROXY": "http://p:3128", "https_proxy": "http://p:3128",
                                                   "NO_PROXY": "x", "all_proxy": "socks5://p"}):
            self.launch_all()
        self.assertEqual([r for r in self.calls() if r["env_proxy"]], [])
        self.assertTrue(all(r["argv"][0] == "-q" for r in self.calls("curl")))


class Probes(Base):

    def test_the_env_scan_script_finds_a_process_holding_the_key(self):
        child = subprocess.Popen(["sleep", "30"], env=dict(os.environ, JWT_SECRET="x"))
        self.addCleanup(child.kill)
        out = subprocess.run(["timeout", "--signal=KILL", "20", "sh", "-c", cq_remote.ENV_CHECK_SH],
                             capture_output=True, text=True).stdout.split()
        self.assertIn(f"/proc/{child.pid}", out)
        self.assertNotIn(f"/proc/{os.getpid()}", out)
        self.assertEqual(out[-1], "done")

    def test_a_container_printing_the_key_is_a_leak_although_its_log_is_redacted(self):
        self.launch_all()
        self.update(log_text=f"bot config jwt_secret={SECRET}")
        for p in self.plan["processes"]:
            self.fleet.stop(p)
        self.fleet.collect(self.run_dir)
        with gzip.open(os.path.join(self.run_dir, "logs", "rust-pub.log.gz"), "rt", encoding="utf-8") as fh:
            self.assertNotIn(SECRET, fh.read())
        self.assertIn("the signing key was in container or tool output", "; ".join(self.fleet.leaks(self.run_dir)))
        self.ctx.key_scrubs = 0
        self.ctx.scrub("token abcdef")
        self.assertEqual(self.ctx.key_scrubs, 0)

    def test_the_drain_query_is_bounded_at_5_s_and_one_session_is_not_drained(self):
        seen = []
        real = cq_remote.Remote.curl

        def spy(ctx, url, params=None, max_time=cq_remote.HTTP_S):
            seen.append(max_time)
            return real(ctx, url, params, max_time)
        w = self.get_world()
        w["prom"][0] = ["label_replace(", HEALTHY + [[{"cq": "own_sessions"}, "1"]]]
        self.set_world(w)
        with unittest.mock.patch.object(cq_remote.Remote, "curl", spy):
            self.assertEqual(self.stack.drained(), ["the room still had 1 sessions 90 s after the stops"])
        self.assertEqual(set(seen), {cq_remote.DRAIN_QUERY_S})

    def test_only_every_planned_session_in_this_runs_room_ends_the_blind_canary(self):
        n = len(self.plan["participants"])
        for seen, blind in ((1, True), (n - 1, True), (n, False)):
            with self.subTest(seen=seen):
                self.stack.own_max, self.stack.first_watch = 0, None
                w = self.get_world()
                w["prom"][0] = ["label_replace(", HEALTHY + [[{"cq": "own_sessions"}, str(seen)]]]
                self.set_world(w)
                self.stack.watch()
                self.clock.advance(self.plan["steps"][0]["join_window_s"] + cq_runner.JOIN_BUDGET_EXTRA_S + 60)
                self.assertEqual(bool(self.stack.watch()), blind)


class RoundOne(Base):

    def test_a_container_that_survives_stop_and_kill_is_an_error_not_a_clean_exit(self):
        self.launch_all()
        stuck = f"cq-{RUN_ID}-rust-pub"
        self.update(stuck=[stuck], unkillable=[stuck])
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.fleet.stop(self.proc("rust-pub"))
        self.assertIn("rust-pub is still running after podman stop and kill", str(cm.exception))

    def test_a_container_gone_at_stop_or_poll_has_no_exit_code(self):
        self.launch_all()
        self.update(containers={})
        self.assertEqual([(e.exited_at, e.exit_code) for e in self.fleet.poll()], [(None, None)] * 4)
        self.fleet.reported.clear()
        self.assertEqual((self.fleet.stop(self.proc("rust-pub")).exit_code), None)

    def test_an_unanswered_uniqueness_query_refuses_even_if_the_family_query_answers(self):
        self.update(curl_fail=["[1h]"])
        self.assertIn("did not answer the meeting-uniqueness", "; ".join(self.stack.preflight(self.plan)))

    def test_up_waits_for_readiness_and_gives_up_after_120_s(self):
        self.launch_all()
        answers = [["UI answered 503"], []]
        self.stack.ready_problems = lambda pods=True: answers.pop(0)
        self.clock.slept.clear()
        self.stack.up(self.plan, "reuse")
        self.assertEqual(self.clock.slept.count(10.0), 1)
        self.stack.ready_problems = lambda pods=True: ["UI answered 503"]
        start = self.clock.mono
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.stack.up(self.plan, "reuse")
        self.assertIn("stack not ready after 120 s: UI answered 503", str(cm.exception))
        self.assertGreaterEqual(self.clock.mono - start, cq_local.READY_REUSE_S)

    def test_a_scrape_target_in_another_namespace_does_not_count(self):
        other = [dict(t, labels=dict(t["labels"], namespace="other")) for t in world()["targets"]]
        self.update(targets=other)
        problems = "; ".join(self.stack.preflight(self.plan))
        for kind in cq_remote.SCRAPED_KINDS:
            self.assertIn(f"no {kind} scrape target", problems)

    def test_a_poll_success_ends_the_blind_spell(self):
        self.launch_all()
        self.update(inspect_fail=True)
        self.assertEqual(self.fleet.poll(), [])
        self.clock.advance(20)
        self.update(inspect_fail=False)
        self.assertEqual(self.fleet.poll(), [])
        self.update(inspect_fail=True)
        self.clock.advance(20)
        self.assertEqual(self.fleet.poll(), [])

    def test_the_scorer_and_other_runner_children_get_no_proxy(self):
        names = {"HTTPS_PROXY": "a", "all_proxy": "b", "Socks_Proxy": "c", "PROXY_HOST": "d"}
        code = "import json, os; print(json.dumps(sorted(k for k in os.environ if k.upper().endswith('_PROXY'))))"
        with unittest.mock.patch.dict(os.environ, names):
            rc, out, _ = cq_runner.SubprocessTools().run([sys.executable, "-c", code])
            self.assertEqual(cq_runner.child_env().get("PROXY_HOST"), "d")
        self.assertEqual((rc, json.loads(out)), (0, []))

    def test_a_secret_folder_that_cannot_be_removed_is_an_error(self):
        self.ctx.write_secret("rust-pub", "x.yaml", "k")
        with unittest.mock.patch.object(cq_remote.shutil, "rmtree", lambda *a, **k: None):
            with self.assertRaises(cq_runner.AdapterError) as cm:
                self.ctx.drop_secret("rust-pub")
        self.assertIn("could not be removed", str(cm.exception))

    def test_every_proxy_variable_is_dropped_from_child_environments(self):
        names = {"HTTP_PROXY": "a", "socks_proxy": "b", "GRPC_PROXY": "c", "no_proxy": "d", "PROXY_HOST": "e"}
        with unittest.mock.patch.dict(os.environ, names):
            env = self.ctx.env()
        self.assertEqual(sorted(k for k in env if k in names), ["PROXY_HOST"])

    def test_a_shared_target_refuses_allow_dirty_before_any_stack_or_fleet_call(self):
        h = tst.harness(self, wrap=tst.Recording)
        h.runner.allow_dirty = True
        run = h.run()
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("is shared: it does not take --allow-dirty", run["error"])
        self.assertEqual(h.runner.stack.used, [])


class PackedFiles(unittest.TestCase):

    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir, True)
        with open(os.path.join(self.dir, "run.json"), "w", encoding="utf-8") as fh:
            fh.write("{}")

    def test_compressed_or_archived_files_beyond_one_gzip_layer_are_unchecked(self):
        import bz2
        import lzma
        import zipfile
        import zlib as zl
        payload = b"trace " + SECRET.encode() * 40
        zipped = io.BytesIO()
        with zipfile.ZipFile(zipped, "w", zipfile.ZIP_DEFLATED) as zf:
            zf.writestr("trace.json", payload)
        for name, data in (("double.gz", gzip.compress(gzip.compress(payload))), ("z.bin", zl.compress(payload)),
                           ("b.bz2", bz2.compress(payload)), ("trace.zip", zipped.getvalue()),
                           ("x.xz", lzma.compress(payload)), ("inner.log.gz", gzip.compress(zl.compress(payload)))):
            with self.subTest(name=name):
                path = os.path.join(self.dir, name)
                with open(path, "wb") as fh:
                    fh.write(data)
                self.assertIn(f"{name} are compressed or archived beyond one gzip layer",
                              "; ".join(cq_remote.leak_scan(self.dir, SECRET)))
                os.remove(path)
        self.assertEqual(cq_remote.leak_scan(self.dir, SECRET), [])

    def test_every_packed_header_is_unchecked_and_a_plain_key_in_a_packed_file_is_still_a_leak(self):
        headers = {"zstd": b"\x28\xb5\x2f\xfd", "7z": b"7z\xbc\xaf\x27\x1c", "rar": b"Rar!\x1a\x07\x00",
                   "lz4": b"\x04\x22\x4d\x18", "bz2": b"BZh91AY", "zip-empty": b"PK\x05\x06", "xz": b"\xfd7zXZ\x00"}
        for name, head in headers.items():
            with self.subTest(name=name):
                path = os.path.join(self.dir, name)
                with open(path, "wb") as fh:
                    fh.write(head + b"\x00\x01opaque")
                self.assertIn(f"{name} are compressed or archived", "; ".join(cq_remote.leak_scan(self.dir, SECRET)))
                with open(path, "wb") as fh:
                    fh.write(head + b"stored " + SECRET.encode())
                self.assertIn(f"{name} carry the signing key", "; ".join(cq_remote.leak_scan(self.dir, SECRET)))
                os.remove(path)

    def test_utf_16_be_is_a_leak(self):
        with open(os.path.join(self.dir, "w.txt"), "wb") as fh:
            fh.write(b"\x00x" + SECRET.encode("utf-16-be"))
        self.assertIn("w.txt carry the signing key", "; ".join(cq_remote.leak_scan(self.dir, SECRET)))


class RoundOneTony(Base):

    def test_the_real_leak_check_reports_a_planted_key_and_a_control_token(self):
        self.launch_all()
        self.fleet.collect(self.run_dir)
        with open(os.path.join(self.run_dir, "probes", "0", "key.txt"), "w", encoding="utf-8") as fh:
            fh.write(f"cfg {SECRET}")
        with open(os.path.join(self.run_dir, "probes", "1", "tok.txt"), "w", encoding="utf-8") as fh:
            fh.write(f"ctl {self.ctx.tokens['probe-01']}")
        problems = "; ".join(self.fleet.leaks(self.run_dir))
        self.assertIn("probes/0/key.txt carry the signing key", problems)
        self.assertIn("probes/1/tok.txt carry a control token", problems)

    def test_up_refuses_a_real_meeting_that_starts_during_the_readiness_wait(self):
        self.launch_all()
        answers = [["UI answered 503"], []]

        def ready(pods=True):
            w = self.get_world()
            w["prom"][0] = ["label_replace(", HEALTHY + [[{"cq": "real_sessions"}, "1"]]]
            self.set_world(w)
            return answers.pop(0)
        self.stack.ready_problems = ready
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.stack.up(self.plan, "reuse")
        self.assertIn("before the join: a real meeting is live (1 sessions", str(cm.exception))

    def test_the_probe_key_folder_is_mounted_read_only(self):
        self.launch_all()
        for k, pid in enumerate(("probe-00", "probe-01")):
            argv = [a for a in self.podman_runs() if "--meeting-url" in a][k]
            self.assertIn(f"{self.ctx.secret_dir(pid)}:/cfg:ro,z", argv)

    def test_the_factory_makes_the_runner_undumpable(self):
        with unittest.mock.patch.object(cq_remote, "undumpable") as hardened, \
                unittest.mock.patch.dict(os.environ, {"JWT_SECRET": SECRET}):
            self.make()
        hardened.assert_called_once_with()

    def test_session_gauges_count_only_positive_series(self):
        self.launch_all()
        self.stack.watch()
        query = dict(p.split("=", 1) for p in [a for r in self.calls("curl") for a in r["argv"]
                                               if a.startswith("query=label_replace(")][-1:])["query"]
        self.assertIn('sum(relay_active_sessions_per_room{namespace="vc",room!~"scale-.*"} > 0)', query)
        self.assertIn(f'sum(relay_active_sessions_per_room{{namespace="vc",room="{ROOM}"}} > 0)', query)
        w = self.get_world()
        w["prom"][0] = ["label_replace(", HEALTHY + [[{"cq": "own_sessions"}, "-1"]]]
        self.set_world(w)
        self.clock.slept.clear()
        self.assertEqual(self.stack.drained(), [])
        self.assertEqual(self.clock.slept, [])

    def test_the_join_model_its_floor_and_the_refusal_that_lists_it(self):
        for probes, seconds, floor in ((1, [90, 13.6, 240, 100.6], 325),
                                       (2, [90, 13.6, 240, 12.8, 49.0, 240, 100.6], 626),
                                       (3, [90, 13.6, 240, 12.8, 49.0, 240, 12.8, 49.0, 240, 123.6], 951)):
            with self.subTest(probes=probes):
                self.make(n_probes(probes, floor))
                rows = cq_remote.join_timeline(n_probes(probes, floor))
                self.assertEqual([s for _, _, s in rows], seconds)
                self.assertEqual(cq_remote.join_floor_s(n_probes(probes, floor)), floor)
                self.assertNotIn("join_window", "; ".join(self.stack.preflight(n_probes(probes, floor))))
                problems = "; ".join(self.stack.preflight(n_probes(probes, floor - 1)))
                self.assertIn(f"raise steps[0].join_window to at least {floor} s", problems)
                for _, what, s in rows:
                    self.assertIn(f"{what} {s:g} s", problems)


if __name__ == "__main__":
    unittest.main()
