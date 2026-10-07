#!/usr/bin/env python3
# Scale-ports overlay and the VIDEOCALL_RELEASE_BUILD switch (#2914).
from __future__ import annotations

import ast
import json
import re
import os
import shutil
import signal
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
E2E = "docker/docker-compose.e2e.yaml"
MONITORING = "docker/docker-compose.monitoring.yaml"
SCALE = "docker/docker-compose.scale-ports.yaml"
DEV = "docker/docker-compose.yaml"
START_DIOXUS = REPO / "docker" / "start-dioxus.sh"
E2E_BACKEND = REPO / "docker" / "e2e-backend.sh"
TRUNK_TOML = REPO / "dioxus-ui" / "Trunk.toml"

SCALE_PORTS = {
    "dioxus-ui": {("3011", 3001, "tcp")},
    "meeting-api": {("8181", 8081, "tcp")},
    "websocket-api": {("8180", 8080, "tcp")},
    "webtransport-api": {("4434", 4433, "udp"), ("5331", 5321, "tcp")},
    "prometheus": {("9190", 9090, "tcp")},
    "grafana": {("3100", 3000, "tcp")},
}
BASE_PORTS = {
    "dioxus-ui": {("3001", 3001, "tcp")},
    "meeting-api": {("8081", 8081, "tcp")},
    "websocket-api": {("8080", 8080, "tcp")},
    "webtransport-api": {("4433", 4433, "udp"), ("5321", 5321, "tcp")},
    "prometheus": {("9090", 9090, "tcp")},
    "grafana": {("3000", 3000, "tcp")},
}
# docker/docker-compose.yaml leaves these to host processes (identity service, dx serve).
DEV_HOST_PROCESS_PORTS = {("8080", "tcp"), ("8081", "tcp")}
SCALE_UI = "http://localhost:3011"
PASSED_ENV = ("CARGO_INCREMENTAL", "E2E_CARGO_RELEASE", "VIDEOCALL_RELEASE_BUILD")
RELEASE_TARGET_SERVICES = ("webtransport-api", "metrics-api", "server-stats-api")
STALE_ORIGINS = ("localhost:3001", "127.0.0.1:4433", "localhost:4433", "localhost:8080", "localhost:8081")
STRIPPED_ENV = (
    "VIDEOCALL_RELEASE_BUILD",
    "E2E_CARGO_RELEASE",
    "DIOXUS_SERVE_MODE",
    "WT_DEV_CERT_HASH_INJECT",
    "PROMETHEUS_HOST_PORT",
    "GRAFANA_HOST_PORT",
    "COMPOSE_FILE",
    "COMPOSE_PROFILES",
    "COMPOSE_PROJECT_NAME",
    "COMPOSE_ENV_FILES",
)


def _compose_available() -> bool:
    try:
        return subprocess.run(["docker", "compose", "version"], capture_output=True).returncode == 0
    except FileNotFoundError:
        return False


def compose_config(*files: str, **env: str) -> dict:
    run_env = {k: v for k, v in os.environ.items() if k not in STRIPPED_ENV}
    run_env.update(env)
    cmd = ["docker", "compose", "-p", "videocall-e2e", "--env-file", os.devnull]
    for f in files:
        cmd += ["-f", f]
    cmd += ["--profile", "monitoring", "config", "--format", "json"]
    out = subprocess.run(cmd, cwd=REPO, env=run_env, capture_output=True, text=True)
    if out.returncode != 0:
        raise AssertionError(f"{' '.join(cmd)} failed:\n{out.stderr}")
    return json.loads(out.stdout)["services"]


def published(service: dict) -> set[tuple[str, int, str]]:
    return {(p.get("published"), p["target"], p.get("protocol", "tcp")) for p in service.get("ports", [])}


@unittest.skipUnless(_compose_available() or os.environ.get("CI"), "needs docker compose; never skipped under CI")
class ComposeConfigTest(unittest.TestCase):
    def test_overlay_publishes_the_alternate_ports(self):
        services = compose_config(E2E, MONITORING, SCALE)
        for name, ports in SCALE_PORTS.items():
            self.assertEqual(published(services[name]), ports, name)

    def test_overlay_ports_do_not_collide_with_the_dev_stack(self):
        scale = compose_config(E2E, MONITORING, SCALE)
        dev = compose_config(DEV)
        held = {(p, proto) for s in dev.values() for p, _, proto in published(s) if p} | DEV_HOST_PROCESS_PORTS
        for name, service in scale.items():
            clash = {(p, proto) for p, _, proto in published(service)} & held
            self.assertFalse(clash, f"{name} publishes dev-stack ports {clash}")

    def test_overlay_moves_every_origin_env_to_the_new_ports(self):
        services = compose_config(E2E, MONITORING, SCALE)
        for name, service in services.items():
            for key, value in (service.get("environment") or {}).items():
                for stale in STALE_ORIGINS:
                    self.assertNotIn(stale, value or "", f"{name} {key}")
        self.assertEqual(services["dioxus-ui"]["environment"]["UI_URL"], SCALE_UI)
        ui = services["dioxus-ui"]["environment"]
        self.assertEqual(ui["WEBTRANSPORT_HOST"], "https://127.0.0.1:4434")
        self.assertEqual(ui["ACTIX_UI_BACKEND_URL"], "ws://localhost:8180")
        self.assertEqual(ui["API_BASE_URL"], "http://localhost:8181")
        self.assertEqual(ui["LOGIN_URL"], "http://localhost:8181/login")
        self.assertEqual(services["websocket-api"]["environment"]["UI_ENDPOINT"], SCALE_UI)
        meeting = services["meeting-api"]["environment"]
        for key in ("CORS_ALLOWED_ORIGIN", "AFTER_LOGIN_URL", "ALLOWED_REDIRECT_URLS"):
            self.assertEqual(meeting[key], SCALE_UI, key)

    def test_overlay_monitoring_ports_follow_the_host_port_env(self):
        services = compose_config(E2E, MONITORING, SCALE, PROMETHEUS_HOST_PORT="9290", GRAFANA_HOST_PORT="3200")
        self.assertEqual(published(services["prometheus"]), {("9290", 9090, "tcp")})
        self.assertEqual(published(services["grafana"]), {("3200", 3000, "tcp")})

    def test_overlay_injects_the_wt_cert_hash(self):
        services = compose_config(E2E, MONITORING, SCALE)
        self.assertEqual(services["dioxus-ui"]["environment"]["WT_DEV_CERT_HASH_INJECT"], "true")

    def test_release_flag_reaches_the_ui_and_relay(self):
        for files in ((E2E, MONITORING), (E2E, MONITORING, SCALE)):
            services = compose_config(*files, VIDEOCALL_RELEASE_BUILD="1")
            self.assertEqual(services["dioxus-ui"]["environment"]["VIDEOCALL_RELEASE_BUILD"], "1", files)
            self.assertEqual(services["websocket-api"]["environment"]["E2E_CARGO_RELEASE"], "1", files)
            self.assertEqual(services["webtransport-api"]["environment"]["E2E_CARGO_RELEASE"], "1", files)
            for name in RELEASE_TARGET_SERVICES:
                self.assertEqual(services[name]["environment"]["VIDEOCALL_RELEASE_BUILD"], "1", name)

    def test_default_and_zero_keep_the_debug_dev_stack(self):
        for env in ({}, {"VIDEOCALL_RELEASE_BUILD": "0"}):
            services = compose_config(E2E, MONITORING, **env)
            ui = services["dioxus-ui"]["environment"]
            self.assertEqual(ui["VIDEOCALL_RELEASE_BUILD"], "0", env)
            self.assertEqual(ui["DIOXUS_SERVE_MODE"], "dev", env)
            self.assertEqual(ui["WT_DEV_CERT_HASH_INJECT"], "false", env)
            self.assertEqual(ui["UI_URL"], "http://localhost:3001", env)
            self.assertEqual(ui["WEBTRANSPORT_HOST"], "https://127.0.0.1:4433", env)
            self.assertEqual(services["websocket-api"]["environment"]["E2E_CARGO_RELEASE"], "0", env)
            self.assertEqual(services["webtransport-api"]["environment"]["E2E_CARGO_RELEASE"], "1", env)
            for name, ports in BASE_PORTS.items():
                self.assertEqual(published(services[name]), ports, name)


def trunk_config_local_hook() -> str:
    hooks = [h for h in TRUNK_TOML.read_text().split("[[hooks]]")[1:] if "config.local.js" in h]
    if len(hooks) != 1:
        raise AssertionError(f"expected one config.local.js hook in {TRUNK_TOML}, found {len(hooks)}")
    hook = hooks[0]
    if not re.search(r'^stage = "post_build"$', hook, re.M) or not re.search(r'^command = "sh"$', hook, re.M):
        raise AssertionError(f"config.local.js hook in {TRUNK_TOML} is no longer a post_build sh hook")
    args = ast.literal_eval(re.search(r"^command_arguments = (\[.*?\])$", hook, re.M | re.S).group(1))
    if len(args) != 2 or args[0] != "-c":
        raise AssertionError(f"unexpected command_arguments {args!r}")
    return args[1]


RECORDER = "\n".join(
    (
        "#!/bin/sh",
        'echo "$(basename "$0") $*" >> "$STUB_LOG"',
        'echo "$(basename "$0") CARGO_INCREMENTAL=${CARGO_INCREMENTAL-unset}" >> "$STUB_LOG.env"',
        'if [ "$(basename "$0")" = "trunk" ] && [ -n "${STUB_TRUNK_FAIL:-}" ]; then exit 3; fi',
        'prev=""; for a in "$@"; do',
        '  if [ "$(basename "$0")" = trunk ] && [ "$prev" = --dist ]; then',
        '    rm -rf "$a" && mkdir -p "$a"',
        '    (cd "$START_DIOXUS_APP_ROOT/dioxus-ui" && TRUNK_STAGING_DIR="$a" sh "$STUB_TRUNK_HOOK")',
        "  fi",
        '  prev="$a"',
        "done",
        "exit 0",
        "",
    )
)


class StubbedScriptTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="scale-overlay-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for tool in self.tools:
            stub = self.bin / tool
            stub.write_text(RECORDER)
            stub.chmod(0o755)
        self.log = self.root / "calls.log"
        self.hook = self.root / "trunk-post-build-hook.sh"
        self.hook.write_text(trunk_config_local_hook())

    def run_script(self, command: list[str], **env: str) -> tuple[int, list[str]]:
        run_env = {k: v for k, v in os.environ.items() if k not in STRIPPED_ENV}
        run_env.update(
            PATH=f"{self.bin}:{os.environ['PATH']}",
            STUB_LOG=str(self.log),
            STUB_TRUNK_HOOK=str(self.hook),
            CARGO_INCREMENTAL="1",
        )
        run_env.update(env)
        stderr_path = self.root / "stderr.log"
        with stderr_path.open("w") as stderr:
            proc = subprocess.Popen(
                command,
                cwd=self.root,
                env=run_env,
                stdout=subprocess.DEVNULL,
                stderr=stderr,
                start_new_session=True,
            )
            try:
                code = proc.wait(timeout=30)
            finally:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        self.stderr = stderr_path.read_text()
        calls = self.log.read_text().splitlines() if self.log.exists() else []
        env_log = Path(f"{self.log}.env")
        self.incremental = sorted(set(env_log.read_text().splitlines())) if env_log.exists() else []
        env_log.unlink(missing_ok=True)
        return code, calls


class StartDioxusTest(StubbedScriptTest):
    tools = ("trunk", "tailwindcss", "miniserve")

    def start(self, **env: str) -> tuple[int, list[str]]:
        return self.run_script(
            ["sh", str(START_DIOXUS)],
            START_DIOXUS_APP_ROOT=str(self.root),
            CARGO_TARGET_DIR=str(self.root / "target"),
            **env,
        )

    @property
    def release_dist(self) -> str:
        return f"{self.root}/target/videocall-release/dist"

    def trunk_calls(self, calls: list[str]) -> list[str]:
        return [c for c in calls if c.startswith("trunk ")]

    def test_release_builds_with_release_and_serves_static_even_in_dev_mode(self):
        code, calls = self.start(VIDEOCALL_RELEASE_BUILD="1", DIOXUS_SERVE_MODE="dev")
        self.assertEqual(code, 0)
        self.assertEqual(self.trunk_calls(calls), [f"trunk build --dist {self.release_dist} --release"])
        self.assertIn("trunk CARGO_INCREMENTAL=0", self.incremental)
        self.assertNotIn("trunk CARGO_INCREMENTAL=1", self.incremental)
        served = [c for c in calls if c.startswith("miniserve ")]
        self.assertEqual(len(served), 1)
        self.assertTrue(served[0].endswith(f" {self.release_dist}"), served[0])

    def test_release_leaves_the_shared_checkout_alone(self):
        shared = self.root / "dioxus-ui" / "scripts" / "config.local.js"
        shared.parent.mkdir(parents=True)
        shared.write_text("written by the dev stack\n")
        code, _ = self.start(VIDEOCALL_RELEASE_BUILD="1", WEBTRANSPORT_HOST="https://127.0.0.1:4434")
        self.assertEqual(code, 0)
        self.assertEqual(shared.read_text(), "written by the dev stack\n")
        self.assertFalse((self.root / "dioxus-ui" / "dist").exists())
        private = (Path(self.release_dist) / "config.local.js").read_text()
        self.assertIn("https://127.0.0.1:4434", private)
        self.assertNotIn("written by the dev stack", private)

    def test_unset_and_zero_keep_the_debug_commands(self):
        for env in ({}, {"VIDEOCALL_RELEASE_BUILD": "0"}):
            self.log.unlink(missing_ok=True)
            code, calls = self.start(DIOXUS_SERVE_MODE="dev", **env)
            self.assertEqual(code, 0, env)
            self.assertEqual(self.trunk_calls(calls), ["trunk serve --address 0.0.0.0 --port 3001 --poll"], env)
            self.log.unlink(missing_ok=True)
            code, calls = self.start(DIOXUS_SERVE_MODE="static", **env)
            self.assertEqual(code, 0, env)
            self.assertEqual(self.trunk_calls(calls), ["trunk build"], env)
            self.assertIn("trunk CARGO_INCREMENTAL=1", self.incremental, env)
            self.assertTrue((self.root / "dioxus-ui" / "dist" / "config.local.js").exists(), env)

    def test_unknown_value_fails_before_building(self):
        for value in ("true", "yes", "2"):
            self.log.unlink(missing_ok=True)
            code, calls = self.start(VIDEOCALL_RELEASE_BUILD=value)
            self.assertEqual(code, 64, value)
            self.assertEqual(calls, [], value)

    def test_failed_release_build_is_not_served(self):
        code, calls = self.start(VIDEOCALL_RELEASE_BUILD="1", STUB_TRUNK_FAIL="1")
        self.assertNotEqual(code, 0)
        self.assertEqual(self.trunk_calls(calls), [f"trunk build --dist {self.release_dist} --release"])
        self.assertFalse([c for c in calls if c.startswith("miniserve ")])


class E2eBackendReleaseTest(StubbedScriptTest):
    tools = ("cargo",)

    def build_run(self, **env: str) -> tuple[int, list[str]]:
        return self.run_script(
            ["bash", str(E2E_BACKEND), "build-run", "fake-bin"], E2E_STAMP_DIR=str(self.root / "stamps"), **env
        )

    def test_one_builds_and_runs_release(self):
        code, calls = self.build_run(E2E_CARGO_RELEASE="1")
        self.assertEqual(code, 0)
        self.assertEqual(calls, ["cargo build -r --bin fake-bin", "cargo run -r --bin fake-bin"])
        self.assertEqual(self.incremental, ["cargo CARGO_INCREMENTAL=1"])

    def test_release_flag_turns_incremental_off(self):
        code, calls = self.build_run(E2E_CARGO_RELEASE="1", VIDEOCALL_RELEASE_BUILD="1")
        self.assertEqual(code, 0)
        self.assertEqual(calls, ["cargo build -r --bin fake-bin", "cargo run -r --bin fake-bin"])
        self.assertEqual(self.incremental, ["cargo CARGO_INCREMENTAL=0"])

    def test_unset_and_zero_build_debug(self):
        for env in ({}, {"E2E_CARGO_RELEASE": "0"}):
            self.log.unlink(missing_ok=True)
            code, calls = self.build_run(**env)
            self.assertEqual(code, 0, env)
            self.assertEqual(calls, ["cargo build --bin fake-bin", "cargo run --bin fake-bin"], env)
            self.assertEqual(self.incremental, ["cargo CARGO_INCREMENTAL=1"], env)

    def test_unknown_value_fails_before_cargo(self):
        for env in ({"E2E_CARGO_RELEASE": "true"}, {"VIDEOCALL_RELEASE_BUILD": "yes"}):
            code, calls = self.build_run(**env)
            self.assertEqual(code, 64, env)
            self.assertEqual(calls, [], env)


@unittest.skipUnless(_compose_available() or os.environ.get("CI"), "needs docker compose; never skipped under CI")
class ReleaseTargetIncrementalTest(StubbedScriptTest):
    tools = ("cargo",)

    def run_service(self, service: dict) -> tuple[int, list[str]]:
        self.log.unlink(missing_ok=True)
        env = {k: v for k, v in service["environment"].items() if k in PASSED_ENV}
        command = service["command"]
        if "/app/docker/e2e-backend.sh" in command[-1]:
            argv = ["bash", str(E2E_BACKEND), "build-run", "fake-bin"]
        else:
            argv = ["bash", "-c", command[-1].replace("$$", "$")]
        return self.run_script(argv, E2E_STAMP_DIR=str(self.root / "stamps"), **env)

    def incremental_seen(self, service: dict) -> list[str]:
        self.run_service(service)
        return self.incremental

    def test_default_keeps_incremental_on(self):
        services = compose_config(E2E, MONITORING)
        for name in (*RELEASE_TARGET_SERVICES, "websocket-api"):
            self.assertEqual(self.incremental_seen(services[name]), ["cargo CARGO_INCREMENTAL=1"], name)

    def test_release_flag_turns_incremental_off_for_the_shared_target(self):
        services = compose_config(E2E, MONITORING, VIDEOCALL_RELEASE_BUILD="1")
        for name in (*RELEASE_TARGET_SERVICES, "websocket-api"):
            self.assertEqual(self.incremental_seen(services[name]), ["cargo CARGO_INCREMENTAL=0"], name)

    def test_unknown_flag_value_fails_before_cargo(self):
        services = compose_config(E2E, MONITORING, VIDEOCALL_RELEASE_BUILD="true")
        for name in (*RELEASE_TARGET_SERVICES, "websocket-api"):
            code, calls = self.run_service(services[name])
            self.assertEqual(code, 64, name)
            self.assertEqual(calls, [], name)
            self.assertIn("is not 0 or 1", self.stderr, name)


if __name__ == "__main__":
    unittest.main(verbosity=2)
