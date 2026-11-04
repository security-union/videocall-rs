#!/usr/bin/env python3
"""Tests for the local scenario-runner adapters (#2914 PR-5), against a stub docker on PATH and a fake Prometheus."""

import fcntl
import gzip
import http.server
import json
import os
import pty
import resource
import shutil
import socket
import struct
import termios
import re
import subprocess
import sys
import tempfile
import threading
import time
import tty
import unittest
import unittest.mock
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cq_local  # noqa: E402
import cq_runner  # noqa: E402
import cq_scenario  # noqa: E402

RUN_ID = "cqtest-20261007t1200-0123abc"
STUB = os.path.join(HERE, "testdata", "stub-docker.py")
SCENARIO = """\
schema: scale-scenario/v1
run: {id_prefix: cqtest, seed: 1, target: local, fidelity_waivers: []}
population:
  - {role: probe, fleet: browser, count: 2, media: {camera: off, mic: muted}, join_stagger: 1s}
  - {role: talkers, fleet: rust, count: 2, media: {camera: on, mic: continuous, video_layers: 3}}
  - {role: cameras, fleet: rust, count: 1, media: {camera: on, mic: muted, video_layers: 3}}
  - {role: viewers, fleet: rust, count: 3, media: {camera: off, mic: off},
     receive: {pin_video_layer: 0, viewport_visible_count: 3}}
steps: [{id: smoke, hold: 3m, headline: true}]
events: [{at: hold+90s, select: {role: viewers, count: 1}, action: leave},
         {at: hold+30s, select: {role: probe, count: 1}, action: unmute},
         {at: hold+60s, select: {role: probe, count: 1}, action: mute}]
"""
TARGETS_UP = {"status": "success", "data": {"activeTargets": [
    {"labels": {"job": j, "instance": f"{j}:1"}, "health": "up", "scrapeInterval": "15s",
     "globalUrl": "http://f00dcafe:9090/metrics",
     "lastError": 'Get "http://websocket-api:8080/metrics": dial tcp 172.23.0.5:8080: connect: connection refused'}
    for j in cq_local.EXPECTED_JOBS]}}
QDISC_OK = [{"dev": "lo", "kind": "noqueue", "parent": 0xFFFFFFFF}, {"dev": "eth0", "kind": "mq", "parent": 0xFFFFFFFF}]


def plan_of(text=SCENARIO):
    return cq_scenario.compile_scenario(cq_scenario.parse_yaml(text), run_id=RUN_ID, scenario_file="x.yaml",
                                        scenario_sha256="0" * 64)


class Prom:
    """A fake Prometheus and UI: answers /api/v1/targets, /api/v1/query, / and /version, and logs every path."""

    def __init__(self):
        self.targets, self.reporting, self.paths, self.status = TARGETS_UP, set(), [], {}
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):  # noqa: N802
                outer.paths.append(self.path)
                if self.path.startswith("/api/v1/targets"):
                    body = outer.targets
                elif self.path.startswith("/api/v1/query"):
                    hit = any(u in self.path for u in outer.reporting)
                    body = {"status": "success", "data": {"result": [{"value": [0, "3"]}] if hit else []}}
                else:
                    body = {}
                data = json.dumps(body).encode()
                self.send_response(outer.status.get(self.path, 200))
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *a):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="cq-local-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        bin_dir = os.path.join(self.tmp, "bin")
        os.makedirs(bin_dir)
        for prog in ("docker", "git"):
            os.symlink(STUB, os.path.join(bin_dir, prog))
        self.world_path, self.log_path = os.path.join(self.tmp, "world.json"), os.path.join(self.tmp, "log.jsonl")
        self.world({"qdisc": QDISC_OK, "ctl": {"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}}})
        env = {"PATH": bin_dir + os.pathsep + os.environ["PATH"], "STUB_WORLD": self.world_path,
               "STUB_LOG": self.log_path}
        patcher = unittest.mock.patch.dict(os.environ, env)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.prom = Prom()
        self.addCleanup(self.prom.close)
        ports = dict(cq_local.PORTS["alt"], ui=self.prom.port, api=self.prom.port, prom=self.prom.port)
        patcher = unittest.mock.patch.dict(cq_local.PORTS, {"test": ports})
        patcher.start()
        self.addCleanup(patcher.stop)
        self.run_dir = os.path.join(self.tmp, "run")
        os.makedirs(self.run_dir)
        self.plan = plan_of()
        self.stack, self.fleet = cq_local.factory(self.plan, self.run_dir, environ={})
        self.ctx = self.stack.ctx
        self.ctx.mode, self.ctx.jwt_secret, self.ctx.commit, self.ctx.netem_applied = "test", "jwt-s3cret", "f" * 40, \
            "none"
        self.fleet.sampler = unittest.mock.Mock(poll=lambda: 0)

    def world(self, w=None, **update):
        if w is None:
            with open(self.world_path, encoding="utf-8") as fh:
                w = json.load(fh)
        w.update(update)
        with open(self.world_path, "w", encoding="utf-8") as fh:
            json.dump(w, fh)
        return w

    def calls(self):
        if not os.path.exists(self.log_path):
            return []
        with open(self.log_path, encoding="utf-8") as fh:
            return [json.loads(line) for line in fh]

    def argvs(self, cmd=None):
        return [c["argv"] for c in self.calls() if cmd is None or c["argv"][0] == cmd]

    def launch_all(self):
        for p in self.plan["processes"]:
            self.fleet.launch(p)

    def proc(self, pid):
        return next(p for p in self.plan["processes"] if p["proc_id"] == pid)


class LaunchTest(Base):
    def test_rust_bots_are_told_the_plans_diagnostics_value_either_way(self):
        self.plan["bot_diagnostics"] = "on"
        self.launch_all()
        rust = [c["argv"] for c in self.calls() if c["argv"][:2] == ["run", "-d"] and "--config" in c["argv"]]
        self.assertTrue(rust)
        self.assertTrue(all(a[a.index("--diagnostics") + 1] == "on" for a in rust), rust)
        self.assertEqual(cq_local.rust_flags(dict(self.plan, bot_diagnostics="off")), ["--diagnostics", "off"])

    def test_every_bot_and_probe_gets_one_deadline_a_ttl_after_the_first_launch(self):
        real, calls = time.time, []

        def a_minute_per_call():
            calls.append(None)
            return real() + 60 * len(calls)
        before = int(real()) + 60
        with unittest.mock.patch.object(cq_local.time, "time", a_minute_per_call):
            self.launch_all()
        after = int(real()) + 60
        runs = [c["argv"] for c in self.calls() if c["argv"][:2] == ["run", "-d"]]
        rust = [a for a in runs if "--config" in a]
        probes = [a for a in runs if "--ipc" in a]
        self.assertTrue(rust and probes)
        deadlines = {a[a.index("--deadline") + 1] for a in rust + probes}
        self.assertEqual(len(deadlines), 1, deadlines)
        ttl = self.plan["steps"][0]["join_window_s"] + self.plan["steps"][0]["hold_s"] + cq_local.TTL_MARGIN_S
        self.assertTrue(before + ttl <= int(deadlines.pop()) <= after + ttl)

    def test_rust_bots_log_like_the_remote_runner(self):
        self.launch_all()
        rust = [c["argv"] for c in self.calls() if c["argv"][:2] == ["run", "-d"] and "--config" in c["argv"]]
        self.assertTrue(rust)
        self.assertTrue(all("RUST_LOG=info,bot::video_producer::replay=debug" in a for a in rust), rust)

    def test_a_probe_skips_paint_and_diag_packets_only_when_its_plan_says_so(self):
        probes = [p for p in self.plan["processes"] if p["fleet"] == "browser"]
        probes[0].update(skip_canvas_paint=True, diag_packets="off", speaking_glow="off")
        self.launch_all()
        runs = [c["argv"] for c in self.calls() if c["argv"][:2] == ["run", "-d"] and "--ipc" in c["argv"]]
        names = ("--skip-canvas-paint", "--diag-packets", "--speaking-glow")
        flags = [[a[i:i + 2] for i, x in enumerate(a) if x in names] for a in runs]
        self.assertEqual(flags, [[["--skip-canvas-paint", "true"], ["--diag-packets", "off"],
                                  ["--speaking-glow", "off"]], []])

    def test_probes_run_tsx_under_init_with_their_own_port_token_and_waiver(self):
        self.launch_all()
        runs = [c for c in self.calls() if c["argv"][:2] == ["run", "-d"] and "--ipc" in c["argv"]]
        self.assertEqual(len(runs), 2)
        ports = set()
        for c in runs:
            a = c["argv"]
            self.assertIn("--init", a)
            self.assertEqual(a[a.index("--stop-timeout") + 1], "30")
            self.assertIn(f"cq.run_id={RUN_ID}", a)
            self.assertEqual(a[a.index(cq_local.PW_IMAGE) + 1], "node_modules/.bin/tsx")
            self.assertEqual(a[a.index("--decode-budget") + 1], "off")
            self.assertIn("BOT_NETEM_APPLIED=none", a)
            self.assertIn("--join-camera-off", a)
            self.assertIn("--join-mic-muted", a)
            self.assertEqual(a[a.index("--participant") + 1] + "@bots-app.local",
                             next(p["user_id"] for p in self.plan["participants"]
                                  if p["name"] == a[a.index("--display-name") + 1]))
            ports.add(a[a.index("--ctl-port") + 1])
            self.assertTrue(c["env_token"] and c["env_jwt"] and c["own_session"])
            self.assertFalse(any(t in " ".join(a) for t in self.ctx.tokens.values()), "token on the command line")
        self.assertEqual(len(ports), 2)
        self.assertEqual(len(set(self.ctx.tokens.values())), 2)
        self.assertFalse(any("npx" in " ".join(a) for a in self.argvs()))

    def test_a_later_probe_waits_until_the_owner_is_in_the_meeting(self):
        self.world(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 0})}})
        self.fleet.launch(self.proc("probe-00"))
        with unittest.mock.patch.object(cq_local, "OWNER_WAIT_S", 0.3), \
                unittest.mock.patch.object(cq_local, "OWNER_POLL_S", 0.05), \
                self.assertRaises(cq_runner.AdapterError) as cm:
            self.fleet.launch(self.proc("probe-01"))
        self.assertIn("waiting room", str(cm.exception))
        self.assertEqual(len(self.argvs("run")), 1)
        self.world(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}})
        self.fleet.launched.remove("probe-01")
        self.fleet.launch(self.proc("probe-01"))
        calls = [a for a in self.argvs() if a[0] in ("run", "exec")]
        self.assertEqual([a[0] for a in calls[-2:]], ["exec", "run"])
        self.assertEqual(calls[-2][1], self.ctx.container("probe-00"))
        self.assertEqual(calls[-1][calls[-1].index("--name") + 1], self.ctx.container("probe-01"))

    def test_an_unverified_link_is_never_stamped_unshaped(self):
        self.ctx.netem_applied = None
        self.fleet.launch(self.proc("probe-00"))
        self.assertIn("BOT_NETEM_APPLIED=unknown", self.argvs("run")[0])

    def test_rust_processes_run_the_bot_image_with_records_ids_and_roster_flags(self):
        self.launch_all()
        runs = {a[a.index("--name") + 1]: a for a in self.argvs("run") if "--ipc" not in a}
        pub = runs[self.ctx.container("rust-pub")]
        self.assertIn("--init", pub)
        self.assertEqual(pub[pub.index("--stop-timeout") + 1], "45")
        self.assertEqual(pub[pub.index("--participants-out") + 1], "/rust/participants-rust-pub.json")
        self.assertEqual(pub[pub.index("--id-prefix") + 1], RUN_ID)
        self.assertEqual(pub[pub.index(self.ctx.bot_image) + 1:pub.index(self.ctx.bot_image) + 3],
                         ["--config", "/in/rust-pub.yaml"])
        flags = {k: pub[pub.index(k) + 1] for k in ("--users", "--broadcasters", "--talkers", "--cameras")}
        self.assertEqual(flags, {"--users": "3", "--broadcasters": "2", "--talkers": "2", "--cameras": "1"})
        leave = runs[self.ctx.container("rust-leave-00")]
        self.assertEqual([leave[leave.index(k) + 1] for k in ("--roster-offset", "--users", "--run-size")],
                         ["5", "1", "6"])
        with open(os.path.join(self.run_dir, "in", "conversation", "manifest.yaml"), encoding="utf-8") as fh:
            manifest = json.load(fh)
        uid = {p["name"]: p["user_id"] for p in self.plan["participants"]}
        names = [p["name"] for p in manifest["participants"]]
        rust_order = [u for p in self.plan["processes"] if p["fleet"] == "rust" for u in p["participants"]]
        self.assertEqual([f"{RUN_ID}-{n}" for n in names], [uid[n] for n in names])
        self.assertEqual(sorted(uid[n] for n in names), sorted(rust_order))
        self.assertEqual({ln["speaker"] for ln in manifest["lines"]}, {"talkers-00", "talkers-01"})
        with open(os.path.join(self.run_dir, "in", "rust-role-viewers.yaml"), encoding="utf-8") as fh:
            cfg = json.load(fh)
        self.assertEqual((cfg["pin_layer"], cfg["viewport_visible_count"]), (0, 3))
        self.assertTrue(cfg["ws_url"].startswith("ws://localhost:"))

    def test_every_container_carries_the_run_label(self):
        self.launch_all()
        self.stack._fleet_preflight(self.plan)
        self.world(verdict={"ok": True, "detail": "RESOURCE_OK"})
        self.fleet.generator_verdicts(1.5, 2.5)
        with unittest.mock.patch.object(cq_local.shutil, "rmtree", side_effect=PermissionError):
            self.stack._clear_stamps()
        runs = self.argvs("run")
        self.assertEqual(len(runs), len(self.plan["processes"]) + 3)
        self.assertTrue(all(f"cq.run_id={RUN_ID}" in a for a in runs))


class RosterTest(unittest.TestCase):
    def test_cameras_sit_where_the_rust_bot_spreads_them(self):
        text = SCENARIO.replace("{role: cameras, fleet: rust, count: 1, media: {camera: on, mic: muted, video_layers: 3}}",
                                "{role: cameras, fleet: rust, count: 2, media: {camera: on, mic: muted, video_layers: 3}}\n"
                                "  - {role: quiet, fleet: rust, count: 3, media: {camera: off, mic: muted}}")
        text = text.replace("count: 2, media: {camera: on, mic: continuous", "count: 1, media: {camera: on, mic: continuous")
        roster, flags, problems = cq_local.rust_layout(plan_of(text))
        self.assertEqual(problems, [])
        self.assertEqual(flags["rust-pub"][1::2][:6], ["0", "6", "6", "1", "1", "2"])
        kinds = ["P" if p["talker"] else "C" if p["publishes"]["camera"] else "V" for p in roster[:6]]
        self.assertEqual(kinds, ["P", "V", "V", "C", "V", "C"], "bot/src/config.rs: Presenter, Viewer, Viewer, "
                                                                 "Camera, Viewer, Camera for 1 broadcaster, 2 cameras")

    def test_a_leaving_talker_is_a_presenter_in_its_own_process(self):
        text = SCENARIO.replace("{at: hold+90s, select: {role: viewers, count: 1}, action: leave}",
                                "{at: hold+90s, select: {role: talkers, count: 1}, action: leave}")
        plan = plan_of(text)
        roster, flags, problems = cq_local.rust_layout(plan)
        self.assertEqual(problems, [])
        leave = dict(zip(flags["rust-leave-00"][::2], flags["rust-leave-00"][1::2]))
        self.assertEqual((leave["--roster-offset"], leave["--broadcasters"], leave["--talkers"]), ("5", "6", "6"))
        self.assertTrue(roster[5]["talker"])

    def test_rosters_the_bot_cannot_honour_are_refused(self):
        talker_off = SCENARIO.replace("media: {camera: on, mic: continuous, video_layers: 3}",
                                      "media: {camera: off, mic: continuous}")
        self.assertIn("camera-off talker", " ".join(cq_local.rust_layout(plan_of(talker_off))[2]))
        no_talker = SCENARIO.replace(
            "  - {role: talkers, fleet: rust, count: 2, media: {camera: on, mic: continuous, video_layers: 3}}\n"
            "  - {role: cameras, fleet: rust, count: 1, media: {camera: on, mic: muted, video_layers: 3}}\n", "")
        self.assertIn("--broadcasters 0", " ".join(cq_local.rust_layout(plan_of(no_talker))[2]))


class StopTest(Base):
    def test_rust_stops_first_in_one_parallel_stop_then_the_probes(self):
        self.launch_all()
        order = [p for p in self.plan["processes"] if p["fleet"] == "rust"] + \
                [p for p in self.plan["processes"] if p["fleet"] == "browser"]
        for p in order:
            ex = self.fleet.stop(p)
            self.assertEqual(ex.proc_id, p["proc_id"])
        stops = self.argvs("stop")
        self.assertEqual(len(stops), 2)
        self.assertEqual(stops[0][1:3], ["-t", "45"])
        self.assertEqual(sorted(stops[0][3:]), sorted(self.ctx.container(p) for p in
                                                      ("rust-pub", "rust-role-viewers", "rust-leave-00")))
        self.assertEqual(stops[1][1:3], ["-t", "30"])
        self.assertEqual(sorted(stops[1][3:]), sorted(self.ctx.container(p) for p in ("probe-00", "probe-01")))
        self.assertTrue(all(c["own_session"] for c in self.calls()))

    def test_a_rust_leave_is_a_sigterm_to_its_own_process_only(self):
        self.launch_all()
        ev = next(e for e in self.plan["events"] if e["action"] == "leave")
        before = time.time()
        out = self.fleet.apply(ev)
        self.assertEqual(out.result, "ok")
        self.assertGreaterEqual(out.t_confirmed, before)
        self.assertEqual(self.argvs("kill"), [["kill", "--signal", "SIGTERM", self.ctx.container("rust-leave-00")]])
        shared = dict(ev, participants=[self.proc("rust-role-viewers")["participants"][0]])
        with self.assertRaises(cq_runner.AdapterError):
            self.fleet.apply(shared)

    def test_exit_times_come_from_finished_at_not_the_reap(self):
        self.launch_all()
        w = self.world()
        w["containers"][self.ctx.container("probe-01")].update(status="exited", finished="2026-09-01T09:01:02Z",
                                                                exit_code=137)
        self.world(w)
        exits = self.fleet.poll()
        self.assertEqual([(e.proc_id, e.exited_at, e.exit_code) for e in exits],
                         [("probe-01", cq_local.docker_time("2026-09-01T09:01:02Z"), 137)])
        self.assertEqual(self.fleet.poll(), [])
        name = self.ctx.container("rust-role-viewers")
        self.world(finished_at={name: "2026-09-01T09:12:34.123456789Z"})
        ex = self.fleet.stop(self.proc("rust-role-viewers"))
        self.assertAlmostEqual(ex.exited_at, cq_local.docker_time("2026-09-01T09:12:34.123456789Z"), places=6)
        self.assertLess(ex.exited_at, time.time() - 3600)

    def test_a_container_still_running_after_docker_stop_is_an_error(self):
        self.launch_all()
        self.world(stuck=[self.ctx.container("probe-00")])
        with self.assertRaisesRegex(cq_runner.AdapterError, "probe-00 is still running after docker stop"):
            self.fleet.stop(self.proc("probe-00"))
        self.assertNotIn("probe-00", self.fleet.reported)

    def test_inspect_raises_on_a_daemon_error_but_not_on_a_missing_container(self):
        self.assertEqual(self.ctx.inspect(["cq-gone"]), {})
        self.world(fail={"inspect": "Cannot connect to the Docker daemon at unix:///var/run/docker.sock"})
        with self.assertRaisesRegex(cq_runner.AdapterError, "docker inspect exited 1: Cannot connect"):
            self.ctx.inspect(["cq-gone"])

    def test_a_container_that_vanished_has_no_exit_time(self):
        self.launch_all()
        w = self.world()
        del w["containers"][self.ctx.container("probe-00")]
        self.world(w)
        self.assertEqual([(e.proc_id, e.exited_at) for e in self.fleet.poll()], [("probe-00", None)])

    def test_docker_times(self):
        self.assertEqual(cq_local.docker_time("1970-01-01T00:00:01.5Z"), 1.5)
        self.assertIsNone(cq_local.docker_time("0001-01-01T00:00:00Z"))
        self.assertIsNone(cq_local.docker_time("yesterday"))
        self.assertIsNone(cq_local.docker_time(None))


class PreflightTest(Base):
    def preflight(self):
        self.ctx.mode = None
        return self.stack.preflight(self.plan)

    def test_containers_left_by_another_run_are_refused(self):
        for key in ("stale", "stale_exited"):
            self.world(**{key: ["cqold-20261001t0000-1111111"]})
            problems = self.preflight()
            hit = [p for p in problems if "cq.run_id=cqold-20261001t0000-1111111 exist" in p]
            self.assertTrue(hit, (key, problems))
            self.assertIn("may still be going", hit[0])
            self.assertNotIn("rm -f", hit[0])
            self.world(**{key: []})
            self.assertFalse(any("cq.run_id=cqold" in p for p in self.preflight()))

    def test_busy_default_ports_pick_the_alt_overlay_and_busy_alt_ports_refuse(self):
        with unittest.mock.patch.object(cq_local.LocalStack, "_tcp_open", lambda self, port: False):
            self.world(ports_lines=["docker\t/dev\trunning\t0.0.0.0:9090->9090/tcp, [::]:9090->9090/tcp"])
            self.assertFalse(any("ports busy" in p for p in self.preflight()))
            self.assertEqual((self.ctx.mode, self.ctx.project), ("alt", "videocall-scale"))
            self.assertIn("-f", self.ctx.compose("up"))
            self.assertIn(cq_local.COMPOSE_ALT, self.ctx.compose("up"))
            self.world(ports_lines=["docker\t/dev\trunning\t0.0.0.0:9090->9090/tcp",
                                    "other\t/o\trunning\t0.0.0.0:9190->9090/tcp"])
            self.assertTrue(any("alt ports busy: 9190/tcp" in p for p in self.preflight()))

    def here(self):
        return os.path.join(self.ctx.root, "docker")

    def stack_problems(self):
        return [p for p in self.preflight() if "compose project" in p or "ports busy" in p]

    def test_up_never_adopts_or_runs_beside_an_e2e_stack_of_this_checkout(self):
        line = f"videocall-e2e\t{self.here()}\trunning\t0.0.0.0:3001->3001/tcp, 0.0.0.0:9090->9090/tcp"
        self.world(ports_lines=[line])
        with unittest.mock.patch.object(cq_local.LocalStack, "_tcp_open", lambda self, port: port in (3001, 9090)):
            [problem] = self.stack_problems()
            self.assertIn("videocall-e2e runs from this checkout and shares e2e/.stack-stamps", problem)
            self.ctx.settings.ports = "default"
            [problem] = self.stack_problems()
            self.assertIn("compose project videocall-e2e already has containers; --stack up only takes down a stack it "
                          "started, so pass --stack reuse or remove it", problem)
            self.ctx.stack_mode = "reuse"
            self.assertEqual(self.stack_problems(), [])
            self.assertEqual(self.ctx.project, "videocall-e2e")

    def test_beside_a_dev_stack_of_this_checkout_up_needs_a_release_build(self):
        self.world(ports_lines=[f"docker\t{self.here()}\trunning\t0.0.0.0:9090->9090/tcp"])
        with unittest.mock.patch.object(cq_local.LocalStack, "_tcp_open", lambda self, port: port == 9090):
            [problem] = self.stack_problems()
            self.assertIn("compose project docker runs from this checkout and shares dioxus-ui's dist", problem)
            self.ctx.settings.release = True
            self.assertEqual(self.stack_problems(), [])
            self.assertEqual(self.ctx.project, "videocall-scale")

    def test_up_never_adopts_a_stopped_project(self):
        self.world(ports_lines=[f"videocall-e2e\t{self.here()}\texited\t"])
        with unittest.mock.patch.object(cq_local.LocalStack, "_tcp_open", lambda self, port: False):
            self.assertEqual(self.stack_problems(), [])
            self.assertEqual(self.ctx.project, "videocall-scale")
            self.ctx.settings.ports = "default"
            [problem] = self.stack_problems()
            self.assertIn("videocall-e2e already has containers; --stack up only takes down a stack it started, so "
                          "remove it", problem)
            self.ctx.stack_mode = "reuse"
            self.assertIn("not running from this checkout", " ".join(self.stack_problems()))

    def test_up_refuses_beside_a_paused_restarting_or_created_stack_of_this_checkout(self):
        with unittest.mock.patch.object(cq_local.LocalStack, "_tcp_open", lambda self, port: False):
            for state in ("paused", "restarting", "created", "exited", "dead"):
                self.world(ports_lines=[f"videocall-e2e\t{self.here()}\t{state}\t"])
                problems = self.stack_problems()
                if state in ("exited", "dead"):
                    self.assertEqual(problems, [], state)
                else:
                    self.assertTrue(any("videocall-e2e runs from this checkout and shares e2e/.stack-stamps" in p
                                        for p in problems), (state, problems))

    def test_a_stack_from_another_checkout_is_never_mine(self):
        line = "videocall-e2e\t/elsewhere/docker\trunning\t0.0.0.0:3001->3001/tcp"
        self.world(ports_lines=[line])
        with unittest.mock.patch.object(cq_local.LocalStack, "_tcp_open", lambda self, port: port == 3001):
            self.preflight()
            self.assertEqual(self.ctx.project, "videocall-scale")
            self.ctx.settings.ports, self.ctx.stack_mode = "default", "reuse"
            problems = self.preflight()
            self.assertTrue(any("compose project videocall-e2e is not running from this checkout" in p
                                for p in problems), problems)
            self.assertTrue(any("default ports busy: 3001/tcp" in p for p in problems), problems)

    def test_stopped_containers_of_another_checkout_are_refused(self):
        self.ctx.settings.ports = "default"
        with unittest.mock.patch.object(cq_local.LocalStack, "_tcp_open", lambda self, port: False):
            self.world(working_dirs=["/elsewhere/docker"])
            self.assertIn("compose project videocall-e2e belongs to another checkout; stop it or use the other ports",
                          self.preflight())
            self.world(working_dirs=[self.here()])
            self.assertFalse(any("another checkout" in p for p in self.preflight()))

    def test_a_shaped_probe_link_is_refused(self):
        self.world(qdisc=QDISC_OK + [{"dev": "eth0", "kind": "netem", "parent": 1}])
        self.assertIn("probe network: netem qdisc on eth0", self.preflight())
        self.assertIsNone(self.ctx.netem_applied)

    def test_qdisc_rules(self):
        self.assertEqual(cq_local.qdisc_problems(QDISC_OK), [])
        self.assertEqual(cq_local.qdisc_problems([{"dev": "lo", "kind": "pfifo_fast", "parent": 0xFFFFFFFF}]),
                         ["lo root qdisc is ['pfifo_fast'], not noqueue"])
        self.assertTrue(cq_local.qdisc_problems(None))


class ReadinessTest(Base):
    def test_ready_only_when_every_target_is_up_from_the_targets_api(self):
        self.assertEqual(self.stack.ready_problems()[0], [])
        self.assertFalse(any(p.startswith("/api/v1/query") for p in self.prom.paths), "an instant query")
        self.assertIn("/api/v1/targets", self.prom.paths)
        down = json.loads(json.dumps(TARGETS_UP))
        down["data"]["activeTargets"][3]["health"] = "down"
        self.prom.targets = down
        self.assertEqual(self.stack.ready_problems()[0], ["target relay-ws: health 'down'"])
        down["data"]["activeTargets"] = down["data"]["activeTargets"][:4]
        self.assertIn("target relay-wt: not configured", self.stack.ready_problems()[0])
        slow = json.loads(json.dumps(TARGETS_UP))
        slow["data"]["activeTargets"][0]["scrapeInterval"] = "30s"
        self.prom.targets = slow
        self.assertTrue(any("scrape interval '30s'" in p for p in self.stack.ready_problems()[0]))

    def test_the_ui_and_meeting_api_must_answer_200(self):
        self.prom.status = {"/": 503}
        self.assertEqual(self.stack.ready_problems()[0], ["UI answered 503"])
        self.prom.status = {"/version": 404}
        self.assertEqual(self.stack.ready_problems()[0], ["meeting-api answered 404"])

    def test_a_stack_that_never_gets_ready_times_out_naming_why(self):
        self.prom.targets = {"status": "error"}
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.stack._wait_ready(budget=0.2, step=0.05)
        self.assertIn("did not answer success", str(cm.exception))

    def test_targets_json_keeps_no_host_url(self):
        slim = cq_local.slim_targets(TARGETS_UP)
        self.assertEqual(len(slim), len(cq_local.EXPECTED_JOBS))
        for leak in ("f00dcafe", "172.23.0.5", "http://"):
            self.assertNotIn(leak, json.dumps(slim))


class StackTest(Base):
    def setUp(self):
        super().setUp()
        self.ctx.mode = "alt"
        self.budgets = []
        for name, fn in (("_bot_image", lambda s: None), ("_quiet_gate", lambda s: None),
                         ("_clear_stamps", lambda s: None), ("_wait_ready", lambda s, b: self.budgets.append(b))):
            patcher = unittest.mock.patch.object(cq_local.LocalStack, name, fn)
            patcher.start()
            self.addCleanup(patcher.stop)
        real = cq_local.Local.sh

        def sh(ctx, argv, *a, **k):
            return subprocess.CompletedProcess(argv, 0, "", "") if argv[0] == "bash" else real(ctx, argv, *a, **k)

        patcher = unittest.mock.patch.object(cq_local.Local, "sh", sh)
        patcher.start()
        self.addCleanup(patcher.stop)

    def composes(self):
        return [(c["argv"], c["env"]) for c in self.calls() if c["argv"][:1] == ["compose"]]

    def test_compose_up_and_down_argv_and_pinned_env(self):
        base = ["compose", "-p", "videocall-scale", "-f", cq_local.COMPOSE_E2E, "-f", cq_local.COMPOSE_MONITORING,
                "-f", cq_local.COMPOSE_ALT, "--profile", "monitoring"]
        with unittest.mock.patch.dict(os.environ, {"DIOXUS_SERVE_MODE": "static", "WT_DEV_CERT_HASH_INJECT": "false",
                                                   "VIDEOCALL_RELEASE_BUILD": "1"}):
            self.stack.up(self.plan, "up")
            self.stack.down()
        calls = self.composes()
        self.assertEqual([a for a, _ in calls], [base + ["up", "-d", "--build"], base + ["rm", "-s", "-f", "-v"],
                                                 base + ["down"]])
        for _, env in calls:
            self.assertEqual(env, {"VIDEOCALL_RELEASE_BUILD": "0", "DIOXUS_SERVE_MODE": "dev",
                                   "WT_DEV_CERT_HASH_INJECT": "true"})
        self.assertEqual(self.budgets, [cq_local.READY_S])

    def test_a_stack_that_appears_before_up_is_neither_adopted_nor_taken_down(self):
        for ids in ({"project_ids": ["abc123"]}, {"project_ids": [], "stopped_ids": ["abc123"]}):
            self.world(**ids)
            with self.assertRaisesRegex(cq_runner.AdapterError, "appeared after preflight"):
                self.stack.up(self.plan, "up")
            self.stack.down()
            self.assertEqual(self.composes(), [])

    def test_up_writes_nothing_shared_beside_a_stack_that_appeared_after_preflight(self):
        paused = f"videocall-e2e\t{os.path.join(self.ctx.root, 'docker')}\tpaused\t"
        real, steps, during = cq_local.Local.sh, [], {}

        def sh(ctx, argv, *a, **k):
            if argv[0] != "bash":
                return real(ctx, argv, *a, **k)
            steps.append("cert")
            self.world(**during)
            return subprocess.CompletedProcess(argv, 0, "", "")

        with unittest.mock.patch.object(cq_local.Local, "sh", sh), \
                unittest.mock.patch.object(cq_local.LocalStack, "_clear_stamps", lambda s: steps.append("stamps")):
            for appear in ({"project_ids": ["abc123"]}, {"stopped_ids": ["abc123"]}, {"ports_lines": [paused]}):
                for before, after in ((appear, {}), ({}, appear)):
                    self.world(**{"project_ids": [], "stopped_ids": [], "ports_lines": [], **before})
                    during.clear()
                    during.update(after)
                    steps.clear()
                    with self.assertRaisesRegex(cq_runner.AdapterError, "appeared after preflight"):
                        self.stack.up(self.plan, "up")
                    self.stack.down()
                    self.assertEqual(steps, ["cert"] if after else [], (appear, bool(after)))
                    self.assertEqual(self.composes(), [])

    def test_reuse_refuses_a_stopped_project(self):
        self.world(stopped_ids=["abc123"])
        with self.assertRaisesRegex(cq_runner.AdapterError, "--stack reuse: compose project videocall-scale is not "
                                                            "running"):
            self.stack.up(self.plan, "reuse")

    def test_reuse_starts_nothing_waits_briefly_and_never_takes_the_stack_down(self):
        self.world(project_ids=["abc123"])
        self.stack.up(self.plan, "reuse")
        self.stack.down()
        self.assertEqual(self.composes(), [])
        self.assertEqual(self.budgets, [cq_local.READY_REUSE_S])

    def test_compose_json_reads_the_stack_env_and_logs_start_at_up(self):
        self.ctx.stack_mode = "reuse"
        os.makedirs(os.path.join(self.run_dir, "stack"))
        ui = ["VIDEOCALL_RELEASE_BUILD=1", "DIOXUS_SERVE_MODE=dev", "WT_DEV_CERT_HASH_INJECT=true", "SECRET=x"]
        self.world(project_ids=["abc123"], stack_env={"dioxus-ui": ui, "websocket-api": ["VIDEOCALL_RELEASE_BUILD=1"]},
                   stack_state=['{"com.docker.compose.service": "websocket-api"}\tabc123\tsha256:1\t2026\t0'])
        with unittest.mock.patch.object(cq_local.time, "time", return_value=1759831000.9):
            self.stack.up(self.plan, "reuse")
        self.stack.save()
        with open(os.path.join(self.run_dir, "stack", "compose.json"), encoding="utf-8") as fh:
            doc = json.load(fh)
        self.assertEqual(doc, {"ports": "alt", "project": "videocall-scale", "stack": "reuse", "release_build": True,
                               "ui_env": {"VIDEOCALL_RELEASE_BUILD": "1", "DIOXUS_SERVE_MODE": "dev",
                                          "WT_DEV_CERT_HASH_INJECT": "true"}, "ui_serve_mode": "static",
                               "docker_os": "Docker Desktop"})
        logs = [a for a, _ in self.composes() if "logs" in a]
        self.assertEqual([a[a.index("logs"):] for a in logs],
                         [["logs", "--no-color", "--timestamps", "--since", "2025-10-07T09:56:40Z", "websocket-api"]])
        self.world(stack_env={"dioxus-ui": ["VIDEOCALL_RELEASE_BUILD=0"],
                              "websocket-api": ["VIDEOCALL_RELEASE_BUILD=1"]})
        self.stack.save()
        with open(os.path.join(self.run_dir, "stack", "compose.json"), encoding="utf-8") as fh:
            doc = json.load(fh)
        self.assertEqual((doc["release_build"], doc["ui_serve_mode"]), (None, "dev"))


class BotImageTest(Base):
    def tag(self, dirty=False):
        return f"videocall-bot:cq-{'f' * 12}" + ("-dirty" if dirty else "")

    def builds(self):
        return [a for a in self.argvs("build")]

    def test_a_clean_tree_reuses_only_a_clean_image_of_its_commit(self):
        self.world(bot_labels={self.tag(): f"{'f' * 40}\tclean"})
        self.stack._bot_image()
        self.assertEqual(self.builds(), [])
        self.assertEqual(self.ctx.bot_image, self.tag())
        for labels in (f"{'f' * 40}\tdirty", f"{'f' * 40}\t", f"{'e' * 40}\tclean"):
            self.world(bot_labels={self.tag(): labels})
            self.stack._bot_image()
        self.assertEqual(len(self.builds()), 3)
        self.assertTrue(all(b[b.index("-t") + 1] == self.tag() and "cq.tree=clean" in b for b in self.builds()))

    def test_a_dirty_tree_always_builds_under_a_dirty_tag(self):
        self.world(git_status=" M bot/src/main.rs", bot_labels={self.tag(): f"{'f' * 40}\tclean",
                                                                self.tag(True): f"{'f' * 40}\tdirty"})
        self.stack._bot_image()
        self.stack._bot_image()
        self.assertEqual(self.ctx.bot_image, self.tag(True))
        builds = self.builds()
        self.assertEqual(len(builds), 2)
        self.assertTrue(all(b[b.index("-t") + 1] == self.tag(True) and "cq.tree=dirty" in b for b in builds))

    def test_cq_tags_older_than_a_day_are_pruned_and_the_current_one_kept(self):
        old, new = "2026-10-01 11:35:24 +0700 +07", time.strftime("%Y-%m-%d %H:%M:%S +0000 UTC", time.gmtime())
        self.world(bot_labels={self.tag(): f"{'f' * 40}\tclean"},
                   bot_tags=[f"{self.tag()}\t{old}", f"videocall-bot:cq-0123456789ab\t{old}",
                             f"{self.tag(True)}\t{old}", f"videocall-bot:cq-ba9876543210\t{new}",
                             f"videocall-bot:local\t{old}", f"videocall-bot:cal-ac9a6a685\t{old}"])
        self.stack._bot_image()
        self.assertEqual(self.argvs("rmi"), [["rmi", "videocall-bot:cq-0123456789ab", self.tag(True)]])
        self.assertIn(["image", "prune", "-f", "--filter", "label=cq.tree"], self.argvs("image"))
        for created in ("2026-10-07 11:35:24 +0700 +07", "2026-10-06 23:35:24 -0500 EST"):
            self.assertEqual(cq_local.image_created(created), 1791347724.0)

    def test_images_name_the_bot_tag_so_a_dirty_build_shows(self):
        self.ctx.dirty = True
        self.world(stack_state=[json.dumps({"com.docker.compose.service": s}) + f"\t{s}0000000000\tsha256:{s}\t2026\t0"
                                for s in cq_local.IMAGE_SERVICES.values()], project_ids=["abc123"])
        images = self.stack.images()
        self.assertEqual(images["rust_bot"], f"sha256:{'ab' * 32} ({self.tag(True)})")


class JoinAndEventTest(Base):
    def record(self, k, join_ts, outcome=None):
        uid = self.proc(f"probe-{k:02d}")["participants"][0]
        folder = os.path.join(self.run_dir, "probes", str(k), "participants")
        os.makedirs(folder, exist_ok=True)
        with open(os.path.join(folder, "bot-x.json"), "w", encoding="utf-8") as fh:
            json.dump({"outcome": outcome, "participant": {"user_id": uid, "join_ts": join_ts}}, fh)
        return uid

    def test_a_probe_counts_as_joined_only_once_it_reports_to_prometheus(self):
        self.launch_all()
        uid = self.record(0, 100.0)
        self.world(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 1})}})
        self.assertNotIn(uid, self.fleet.joined())
        self.prom.reporting.add(urllib_quote(uid))
        self.assertEqual(self.fleet.joined()[uid].join_ts, 100.0)
        other = self.record(1, 101.0, outcome="browser-crash")
        self.prom.reporting.add(urllib_quote(other))
        self.assertNotIn(other, self.fleet.joined())

    def test_a_probe_out_of_the_meeting_is_not_joined(self):
        self.launch_all()
        uid = self.record(0, 100.0)
        self.prom.reporting.add(urllib_quote(uid))
        self.world(ctl={"/healthz": {"status": 200, "body": json.dumps({"inMeeting": 0})}})
        self.assertNotIn(uid, self.fleet.joined())

    def test_mute_and_unmute_go_through_the_probe_control_api(self):
        self.launch_all()
        self.world(ctl={"/bots": {"status": 200, "body": json.dumps({"bots": [{"botId": "b 1"}]})},
                        "/bots/b%201/mute": {"status": 200, "body": "{}"}})
        unmute = next(e for e in self.plan["events"] if e["action"] == "unmute")
        self.assertEqual(self.fleet.apply(unmute).result, "ok")
        execs = self.argvs("exec")
        self.assertEqual(execs[-1][-3:], ["POST", "/bots/b%201/mute", json.dumps({"mic": False})])
        self.assertEqual(execs[-1][1], self.ctx.container("probe-01"))
        self.world(ctl={"/bots": {"status": 200, "body": json.dumps({"bots": [{"botId": "b 1"}]})},
                        "/bots/b%201/mute": {"status": 500, "body": "{}"}})
        mute = next(e for e in self.plan["events"] if e["action"] == "mute")
        out = self.fleet.apply(mute)
        self.assertIsNone(out.t_confirmed)
        self.assertIn("answered 500", out.result)


def urllib_quote(uid):
    return urllib.parse.quote(uid, safe="")


class CollectTest(Base):
    def test_logs_are_saved_redacted_and_containers_removed(self):
        self.launch_all()
        self.ctx.tokens["probe-00"] = "c0ffee11"
        jwt = ".".join(["eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiJwcm9iZS0wMCJ9", "c2lnbmF0dXJlLWJ5dGVz"])
        self.world(log_text=f"lobby?token=abc.def-123 Bearer zz9 {self.ctx.jwt_secret} c0ffee11 {self.ctx.root} "
                            f"session {jwt}")
        self.fleet._save_containers()
        with gzip.open(os.path.join(self.run_dir, "logs", "probe-00.log.gz"), "rt", encoding="utf-8") as fh:
            text = fh.read()
        for secret in ("abc.def-123", "zz9", self.ctx.jwt_secret, "c0ffee11", self.ctx.root, jwt[:20]):
            self.assertNotIn(secret, text)
        self.assertIn("token=<redacted>", text)
        self.assertIn("session <jwt>", text)
        self.assertEqual(self.world()["containers"], {})
        with open(os.path.join(self.run_dir, "fleet", "containers.json"), encoding="utf-8") as fh:
            self.assertEqual(sorted(json.load(fh)), sorted(p["proc_id"] for p in self.plan["processes"]))

    def test_containers_json_records_state_and_image_and_names_no_host_path(self):
        self.launch_all()
        w = self.world()
        name = self.ctx.container("probe-00")
        w["containers"][name].update(status="created", error=f'error mounting "{self.ctx.root}/e2e" to rootfs at '
                                                              f'"/app/e2e" on {socket.gethostname()}')
        self.world(w)
        self.fleet._save_containers()
        with open(os.path.join(self.run_dir, "fleet", "containers.json"), encoding="utf-8") as fh:
            text = fh.read()
        doc = json.loads(text)
        self.assertEqual(doc["probe-00"]["Error"], 'error mounting "$CHECKOUT/e2e" to rootfs at "/app/e2e" on <host>')
        self.assertEqual((doc["probe-00"]["Status"], doc["rust-pub"]["Status"]), ("created", "running"))
        self.assertEqual(doc["rust-pub"]["Image"], "sha256:" + self.ctx.container("rust-pub"))
        self.assertNotIn(self.ctx.root, text)

    def test_a_step_that_fails_in_collect_never_skips_the_others(self):
        self.launch_all()
        self.fleet.sampler = unittest.mock.Mock(poll=lambda: None, pid=-1, wait=unittest.mock.Mock(
            side_effect=subprocess.TimeoutExpired(["bash", os.path.join(self.run_dir, "x")], 5)))
        with unittest.mock.patch.object(cq_local.os, "killpg"), \
                self.assertRaisesRegex(cq_runner.AdapterError, "collect: sampler: TimeoutExpired") as cm:
            self.fleet.collect(self.run_dir)
        self.assertNotIn(self.run_dir, str(cm.exception))
        self.assertEqual(self.world()["containers"], {})
        self.assertTrue(os.path.exists(os.path.join(self.run_dir, "logs", "probe-00.log.gz")))

    def test_a_failed_log_save_still_saves_the_others_and_removes_the_containers(self):
        self.launch_all()
        real = cq_local.Local.write_log

        def write_log(ctx, rel, text):
            if "probe-00" in rel:
                raise OSError(28, "No space left on device")
            return real(ctx, rel, text)

        with unittest.mock.patch.object(cq_local.Local, "write_log", write_log), \
                self.assertRaisesRegex(cq_runner.AdapterError, "probe-00 log: OSError"):
            self.fleet._save_containers()
        self.assertEqual(self.world()["containers"], {})
        self.assertTrue(os.path.exists(os.path.join(self.run_dir, "logs", "rust-pub.log.gz")))

    def test_the_home_folder_is_scrubbed_to_a_tilde(self):
        with unittest.mock.patch.dict(os.environ, {"HOME": "/home/cq-tester"}):
            self.assertEqual(self.ctx.scrub("wrote /home/cq-tester/videocall-scale-runs/x"),
                             "wrote ~/videocall-scale-runs/x")

    def test_redaction_runs_before_host_substitution_so_a_host_name_cannot_split_a_token(self):
        self.ctx.tokens["probe-00"] = "c0ffee" + "ab" * 13
        with unittest.mock.patch.object(cq_local.socket, "gethostname", return_value="abab"):
            self.assertEqual(self.ctx.scrub("ctl " + self.ctx.tokens["probe-00"] + " on abab"),
                             "ctl <redacted> on <host>")

    def test_token_patterns_cover_json_url_encoded_and_any_case_bearer(self):
        for text, want in (('{"token": "r00m-t0k.en", "x": 1}', '{"token": "<redacted>", "x": 1}'),
                           ("next=%2Flobby%3Ftoken%3Dabc.def&y=1", "next=%2Flobby%3Ftoken%3D<redacted>&y=1"),
                           ("url?token%3dabc", "url?token%3d<redacted>"),
                           ("authorization: bearer s3cr3t.v4lue", "authorization: bearer <redacted>"),
                           ("Authorization: BEARER s3cr3t", "Authorization: BEARER <redacted>")):
            with self.subTest(text=text):
                self.assertEqual(cq_local.redact(text), want)

    def test_adapter_errors_name_no_host_path(self):
        self.world(fail={"run": f"invalid mount {self.ctx.root}/e2e and {self.run_dir}/probes on {os.uname()[1]}"})
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.fleet.launch(self.proc("probe-00"))
        msg = str(cm.exception)
        self.assertTrue(msg.startswith("docker run exited 1"), msg)
        for host in (self.ctx.root, self.run_dir, os.path.expanduser("~")):
            self.assertNotIn(host, msg)

    def test_generator_verdict_is_judged_for_exactly_the_hold_and_names_its_host(self):
        self.world(verdict={"ok": True, "detail": "RESOURCE_OK: 36 samples"})
        hs, he = 1759830000.123456, 1759830180.654321
        [v] = self.fleet.generator_verdicts(hs, he)
        self.assertEqual((v["host"], v["ok"], v["window"]["from"], v["window"]["to"]), ("local", True, hs, he))
        run = next(a for a in self.argvs("run") if "resource-verdict" in a)
        self.assertEqual(run[run.index("--from") + 1], repr(hs))
        merged = cq_runner.compose_generator_verdict(["local"], [v], (hs, he), {"local", "ci", "local"})
        self.assertTrue(merged["ok"], merged)
        self.world(verdict_rc=2)
        with self.assertRaises(cq_runner.AdapterError):
            self.fleet.generator_verdicts(hs, he)


class SubprocessTest(Base):
    def test_a_hung_docker_is_time_bounded(self):
        self.world(sleep={"ps": 5})
        t = time.monotonic()
        with self.assertRaises(cq_runner.AdapterError) as cm:
            self.ctx.sh(["docker", "ps"], 0.5)
        self.assertLess(time.monotonic() - t, 4)
        self.assertEqual(str(cm.exception), "docker ps timed out after 0.5 s")

    def test_an_interrupt_is_never_swallowed(self):
        self.launch_all()
        with unittest.mock.patch.object(subprocess, "run", side_effect=KeyboardInterrupt), \
                self.assertRaises(KeyboardInterrupt):
            self.fleet.poll()
        with unittest.mock.patch.object(subprocess, "run", side_effect=KeyboardInterrupt), \
                self.assertRaises(KeyboardInterrupt):
            self.fleet.collect(self.run_dir)

    def test_poll_retries_a_transient_docker_error(self):
        self.launch_all()
        real, n = cq_local.Local.inspect, []

        def flaky(ctx, names, *rest):
            n.append(1)
            if len(n) == 1:
                raise cq_runner.AdapterError("docker inspect timed out after 60 s")
            return real(ctx, names, *rest)

        with unittest.mock.patch.object(cq_local.Local, "inspect", flaky), \
                unittest.mock.patch.object(cq_local.time, "sleep"):
            self.assertEqual(self.fleet.poll(), [])
        self.assertEqual(len(n), 2)


class SamplerTest(unittest.TestCase):
    def test_exactly_one_host_sampler_writes_the_csv(self):
        tmp = tempfile.mkdtemp(prefix="cq-sampler-")
        self.addCleanup(shutil.rmtree, tmp, True)
        plan = plan_of()
        _, fleet = cq_local.factory(plan, tmp, environ={})
        fleet._start_sampler()
        self.addCleanup(fleet._stop_sampler)
        csv = os.path.join(tmp, "host", "host-raw.csv")
        self.assertEqual(cq_local.sampler_writers(csv), 1)
        _, twin = cq_local.factory(plan, tmp, environ={})
        with self.assertRaises(cq_runner.AdapterError) as cm:
            twin._start_sampler()
        self.addCleanup(twin._stop_sampler)
        self.assertIn("exactly one host resource sampler", str(cm.exception))
        fleet._stop_sampler()
        self.assertIsNotNone(fleet.sampler.poll())
        with open(csv, encoding="utf-8") as fh:
            text = fh.read()
        self.assertIn(",host,<host>,", text)
        self.assertNotIn(socket.gethostname(), text)


    def test_the_samplers_cmdline_reader_ends_on_a_read_error_without_repeating_an_argument(self):
        with open(os.path.join(cq_local.ROOT, "e2e", "bots-app", "scripts", "resource-sampler.sh"), encoding="utf-8") as fh:
            func = re.search(r"^cmdline_args\(\) \{\n.*?^\}\n", fh.read(), re.M | re.S).group(0)
        master, slave = pty.openpty()
        tty.setraw(slave)
        os.write(master, b"node\x00bots-app/src/cli.ts\x00run\x00")
        proc = subprocess.Popen(["bash", "-c", func + 'cmdline_args\nprintf "%s|" "${args[@]}"'], stdin=slave,
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                preexec_fn=lambda: resource.setrlimit(resource.RLIMIT_AS, (512 << 20, 512 << 20)))
        deadline = time.monotonic() + 5
        while struct.unpack("i", fcntl.ioctl(slave, termios.TIOCINQ, b"\0" * 4))[0] and time.monotonic() < deadline:
            time.sleep(0.01)
        os.close(slave)
        os.close(master)
        try:
            out, _ = proc.communicate(timeout=5)
        finally:
            proc.kill()
        self.assertEqual(out.decode(), "node|bots-app/src/cli.ts|run|")

    def test_the_host_sampler_names_a_probes_node_process_by_its_command_line(self):
        tsc = "node node_modules/typescript/bin/tsc --noEmit bots-app/src/cli.ts"
        for line, hit in (("node --import tsx --import /cq/preload.mjs bots-app/src/cli.ts run --x", True),
                          ("/usr/local/bin/node --require /x/preflight.cjs --import=file:///x/loader.mjs "
                           "bots-app/src/cli.ts run", True),
                          ("podman run -d --name p img node --import tsx bots-app/src/cli.ts run", False),
                          (tsc, False), ("node node_modules/vitest/vitest.mjs run bots-app/src/cli.ts", False),
                          ("node node_modules/.bin/eslint bots-app/src/cli.ts run", False),
                          ("node --import tsx bots-app/src/cli.ts gen --users 5", False),
                          ("node --import tsx bots-app/src/cli.ts runner", False)):
            self.assertEqual(bool(re.search(cq_local.PROBE_NODE_CMDLINE, line)), hit, line)
        fake = tempfile.mkdtemp(prefix="cq-argv-")
        self.addCleanup(shutil.rmtree, fake, True)
        for script in ("bots-app/src/cli.ts", "node_modules/typescript/bin/tsc"):
            os.makedirs(os.path.join(fake, os.path.dirname(script)), exist_ok=True)
            with open(os.path.join(fake, script), "w", encoding="utf-8") as fh:
                fh.write("import time\ntime.sleep(30)\n")
        probe, other = (subprocess.Popen(["bash", "-c", 'exec -a node python3 "$@"', "-", *args], cwd=fake)
                        for args in (["bots-app/src/cli.ts", "run"], tsc.split()[1:]))
        for proc in (probe, other):
            self.addCleanup(proc.wait)
            self.addCleanup(proc.kill)
        time.sleep(0.3)
        tmp = tempfile.mkdtemp(prefix="cq-sampler-")
        self.addCleanup(shutil.rmtree, tmp, True)
        _, fleet = cq_local.factory(plan_of(), tmp, environ={})
        fleet._start_sampler()
        self.addCleanup(fleet._stop_sampler)
        csv = os.path.join(tmp, "host", "host-raw.csv")
        rows = []
        for _ in range(50):
            if os.path.exists(csv):
                with open(csv, encoding="utf-8") as fh:
                    rows = [r.split(",") for r in fh.read().splitlines() if r.startswith("proc,")]
            if any(r[2] == str(probe.pid) for r in rows):
                break
            time.sleep(0.1)
        self.assertEqual({r[3] for r in rows if r[2] == str(probe.pid)}, {cq_local.PROBE_NODE_COMM})
        self.assertNotIn(cq_local.PROBE_NODE_COMM, {r[3] for r in rows if r[2] == str(other.pid)})


class GateTest(unittest.TestCase):
    def test_quiet_host_gate(self):
        tick = os.sysconf("SC_CLK_TCK")
        before = {10: ("bash", 0, "bash"), 11: ("chrome", 0, "chrome"), 12: ("node", 0, "node x/vitest run")}
        quiet = {10: ("bash", 0, "bash"), 11: ("chrome", int(0.1 * tick), "chrome")}
        self.assertEqual(cq_local.gate_reading(before, quiet, 1.0, 1.0, set())[0], True)
        self.assertFalse(cq_local.gate_reading(before, quiet, 1.0, 4.5, set())[0])
        hot = {**quiet, 11: ("chrome", int(0.5 * tick), "chrome")}
        ok, detail = cq_local.gate_reading(before, hot, 1.0, 1.0, set())
        self.assertFalse(ok)
        self.assertIn("chrome:50%", detail)
        self.assertTrue(cq_local.gate_reading(before, hot, 1.0, 1.0, {11})[0], "the runner's own tree is exempt")
        self.assertFalse(cq_local.gate_reading(before, {**quiet, 13: ("cargo", 0, "cargo")}, 1, 1, set())[0])
        self.assertFalse(cq_local.gate_reading(before, {**quiet, 12: before[12]}, 1, 1, set())[0])


class FilesTest(unittest.TestCase):
    def test_the_alt_port_table_matches_the_compose_overlay(self):
        import yaml
        with open(os.path.join(cq_local.ROOT, cq_local.COMPOSE_ALT), encoding="utf-8") as fh:
            text = fh.read().replace("!override", "")
        svc = yaml.safe_load(text)["services"]
        host = {s: [p.rsplit(":", 1)[0] for p in v["ports"]] for s, v in svc.items()}
        alt = cq_local.PORTS["alt"]
        self.assertEqual(host["dioxus-ui"], [str(alt["ui"])])
        self.assertEqual(host["meeting-api"], [str(alt["api"])])
        self.assertEqual(host["websocket-api"], [str(alt["ws"])])
        self.assertEqual(host["webtransport-api"], [str(alt["wt"]), str(alt["wt_metrics"])])
        self.assertEqual(host["prometheus"], [f"${{PROMETHEUS_HOST_PORT:-{alt['prom']}}}"])
        self.assertEqual(host["grafana"], [f"${{GRAFANA_HOST_PORT:-{alt['grafana']}}}"])

    def test_the_scenarios_compile_to_their_documented_shapes(self):
        shapes = {}
        for name in ("local-smoke", "ci-smoke"):
            doc, raw = cq_scenario.load_scenario(os.path.join(HERE, "scenarios", f"{name}.yaml"))
            plan = cq_scenario.compile_scenario(doc, run_id=RUN_ID, scenario_file=name, scenario_sha256="0" * 64)
            self.assertEqual(cq_local.rust_layout(plan)[2], [])
            fleets = [p["fleet"] for p in plan["participants"]]
            rust = [p for p in plan["participants"] if p["fleet"] == "rust"]
            split = (sum(p["talker"] for p in rust), sum(p["publishes"]["camera"] and not p["talker"] for p in rust),
                     sum(not p["publishes"]["camera"] for p in rust))
            role = {p["user_id"]: p["role"] for p in plan["participants"]}
            shapes[name] = (plan["target"], fleets.count("browser"), fleets.count("rust"), split,
                            [s["hold_s"] for s in plan["steps"]], plan["scoring"]["expect"]["verdict"],
                            [(e["at_offset_s"], e["action"], [role[u] for u in e["participants"]])
                             for e in plan["events"]])
            self.assertIn(cq_scenario.DECODE_BUDGET_WAIVER, plan["fidelity_waivers"])
        self.assertEqual(shapes["local-smoke"], ("local", 1, 6, (2, 2, 2), [180], "INVALID",
                                                 [(90, "leave", ["viewers"])]))
        self.assertEqual(shapes["ci-smoke"], ("ci", 2, 5, (1, 3, 1), [300], "INVALID", []))

    def test_every_viewport_shows_a_publisher_and_hides_one(self):
        for name in ("local-smoke", "ci-smoke"):
            doc, _ = cq_scenario.load_scenario(os.path.join(HERE, "scenarios", f"{name}.yaml"))
            plan = cq_scenario.compile_scenario(doc, run_id=RUN_ID, scenario_file=name, scenario_sha256="0" * 64)
            self.assertTrue(any((p.get("receive") or {}).get("viewport_visible_count") for p in plan["processes"]))
            self.assertEqual(cq_scenario.viewport_warnings(plan), [], name)

    def test_a_viewport_that_cannot_exercise_both_filters_is_warned_about(self):
        with open(os.path.join(HERE, "scenarios", "local-smoke.yaml"), encoding="utf-8") as fh:
            smoke = fh.read()
        one_layer = smoke.replace("video_layers: 3", "video_layers: 1")
        warned = cq_scenario.viewport_warnings(plan_of(one_layer))
        self.assertEqual(len(warned), 2)
        self.assertTrue(all("relay_layer_filtered_total may not move" in w for w in warned), warned)
        wide = cq_scenario.viewport_warnings(plan_of(smoke.replace("viewport_visible_count: 3",
                                                                   "viewport_visible_count: 4")))
        self.assertEqual(len(wide), 2)
        self.assertTrue(all("shows all 4 camera publishers" in w for w in wide), wide)
        exempt = one_layer.replace("relay_path_exemptions: {}", "relay_path_exemptions: "
                                                                "{relay_layer_filtered_total: one layer}")
        self.assertEqual(cq_scenario.viewport_warnings(plan_of(exempt)), [])

    def test_the_cli_warns_at_compile_and_passes_the_stack_mode_to_the_adapters(self):
        import contextlib
        import io
        import scenario_run
        tmp = tempfile.mkdtemp(prefix="cq-cli-")
        self.addCleanup(shutil.rmtree, tmp, True)
        path = os.path.join(tmp, "s.yaml")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(SCENARIO.replace("video_layers: 3", "video_layers: 1"))
        seen = []

        def factory(plan, run_dir, stack_mode="up"):
            seen.append(stack_mode)
            raise cq_runner.AdapterError("stop here")

        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            rc = scenario_run.main(["--scenario", path, "--stack", "reuse", "--out-dir", os.path.join(tmp, "runs")],
                                   adapters={"local": factory})
        self.assertEqual((rc, seen), (3, ["reuse"]))
        self.assertIn("warning: ", err.getvalue())
        self.assertIn("relay_layer_filtered_total may not move", err.getvalue())

    def test_the_cli_registers_the_local_adapters_for_both_targets(self):
        import scenario_run
        self.assertIs(scenario_run.ADAPTERS["local"], cq_local.factory)
        self.assertIs(scenario_run.ADAPTERS["ci"], cq_local.factory)
        with self.assertRaises(cq_runner.AdapterError):
            cq_local.factory(plan_of(), "/nonexistent", environ={"CQ_PORTS": "both"})


if __name__ == "__main__":
    unittest.main()
