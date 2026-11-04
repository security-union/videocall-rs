"""Local mode of the fidelity harness: one mixed room on a running e2e stack, captured at the relay."""

import calendar
import gzip
import hashlib
import json
import os
import re
import secrets
import shutil
import subprocess
import threading
import time

import cq_local
import fidelity_measures
from fidelity_signals import STOP
from cq_local import AdapterError

HERE = os.path.dirname(os.path.abspath(__file__))
CAPTURE_DOCKERFILE = os.path.join(HERE, "Dockerfile.capture")
CAPTURE_REPO = "videocall-fidelity-capture"
RELAY_SERVICE = "websocket-api"
RELAY_PORT = 8080
PRODUCT_PATHS = ("actix-api", "bot", "videocall-client", "videocall-codecs", "videocall-types", "videocall-aq",
                 "videocall-transport", "videocall-netsim", "videocall-diagnostics", "videocall-meeting-types",
                 "videocall-meeting-client", "neteq", "protobuf", "meeting-api", "dioxus-ui", "e2e/bots-app/src",
                 "vendor/quinn", "vendor/quinn-proto", "vendor/web-transport-quinn",
                 "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "Dockerfile.bot",
                 ":(exclude)dioxus-ui/static/tailwind.css")
STAMPS = os.path.join("e2e", ".stack-stamps")
RELAY_STAMP = "websocket_server.json"
CAPTURE_READY_S = 30
CAPTURE_STOP_S = 30
STAMP_SETTLE_S = 60
BUILD_AFTER_MOVE_S = 2
STAMP_BUILD_S = 900
MIN_FREE_GB = 10
RUST_JOIN_S = 300
BROWSER_JOIN_S = 300
SAMPLE_S = 5
HOLD_POLL_S = 10
LEAVE_GRACE_S = 10
DROPPED_RE = re.compile(r"(\d+) packets? dropped by (kernel|interface)")
CAPTURED_RE = re.compile(r"(\d+) packets? captured")


def bot_indices(browser_procs, parts, sources):
    """bots-app --bot-index per browser process: every 6th index from 1 captures 1280x720, the rest 640x480."""
    nxt = {"640x480": 2, "1280x720": 1}
    out = {}
    for proc in browser_procs:
        geometry = sources.get(parts[proc["participants"][0]]["role"], "640x480")
        out[proc["proc_id"]] = nxt[geometry]
        step = 6 if geometry == "1280x720" else 1
        nxt[geometry] += step
        while geometry == "640x480" and nxt[geometry] % 6 == 1:
            nxt[geometry] += 1
    return out


def existing_parent(path):
    path = os.path.abspath(path)
    while not os.path.exists(path) and os.path.dirname(path) != path:
        path = os.path.dirname(path)
    return path


def iso_epoch(text):
    try:
        return calendar.timegm(time.strptime(text, "%Y-%m-%dT%H:%M:%SZ"))
    except (TypeError, ValueError):
        return None


def capture_stats(log):
    """(captured, dropped by kernel or interface) from tcpdump's exit summary; None when it printed no summary."""
    drops, c = DROPPED_RE.findall(log or ""), CAPTURED_RE.search(log or "")
    kernel = any(where == "kernel" for _, where in drops)
    return (int(c.group(1)) if c else None), (sum(int(n) for n, _ in drops) if kernel else None)


class Fleet(cq_local.LocalFleet):
    """cq_local's fleet with bots-app browsers on WS, a fixed capture geometry and a simulcast ceiling."""

    def __init__(self, ctx, layers, cores, indices):
        super().__init__(ctx)
        self.layers, self.cores, self.indices = layers, cores, indices

    def start(self, proc):
        self.launched.append(proc["proc_id"])
        (self._launch_probe if proc["fleet"] == "browser" else self._launch_rust)(proc)

    def _launch_probe(self, proc):
        c = self.ctx
        self._await_owner(proc["proc_id"])
        part = self.parts[proc["participants"][0]]
        out = c.path("probes", str(proc["probe_index"]))
        os.makedirs(out, exist_ok=True)
        token = secrets.token_hex(16)
        c.tokens[proc["proc_id"]] = token
        argv = self._base_run(proc, cq_local.PROBE_STOP_S) + [
            "--ipc", "host", "-e", "BOT_CTL_TOKEN", "-e", "BOT_CTL_STATE_DIR=/tmp/ctl", "-e", "HOME=/tmp/h",
            "-e", "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1", "-e", f"BOT_SIMULCAST_MAX_LAYERS={self.layers}",
            "-v", f"{os.path.join(c.root, 'e2e')}:/app/e2e", "-v", f"{out}:/out", "-w", "/app/e2e", cq_local.PW_IMAGE,
            cq_local.TSX, cq_local.CLI, "run", "--meeting-url", f"{c.ui_url}/meeting/{c.plan['meeting_id']}",
            "--participant", part["user_id"].split("@")[0], "--display-name", part["name"], "--video-mode", "clock",
            "--headless", "--ttl", f"{-(-self._ttl_s() // 60)}m", "--deadline", str(self._deadline()),
            "--manifest", "", "--assets-dir", "/out", "--ctl-port", str(cq_local.CTL_PORT_BASE + proc["probe_index"]),
            "--role", part["role"], "--decode-budget", proc.get("decode_budget", "off"), "--transport", "ws",
            "--bot-index", str(self.indices[proc["proc_id"]]), "--hardware-concurrency", str(self.cores)]
        argv += cq_local.probe_flags(proc)
        argv += [] if part["publishes"]["camera"] else ["--join-camera-off"]
        argv += [] if part["publishes"]["mic"] else ["--join-mic-muted"]
        c.sh(argv, cq_local.DOCKER_S, env=c.env(BOT_CTL_TOKEN=token))

    def media_problems(self, browsers):
        """Browsers whose bots-app record shows a camera or mic other than the plan's, read after they joined."""
        out = []
        for proc in browsers:
            uid = proc["participants"][0]
            folder = self.ctx.path("probes", str(proc["probe_index"]), "participants")
            recs = [self._read(os.path.join(folder, n)) for n in sorted(os.listdir(folder))
                    if n.endswith(".json")] if os.path.isdir(folder) else []
            rec = next((r for r in recs if isinstance(r, dict)
                        and (r.get("participant") or {}).get("user_id") == uid), None)
            got = ((rec or {}).get("participant") or {}).get("publishes")
            want = self.parts[uid]["publishes"]
            if not isinstance(got, dict):
                out.append(f"{proc['proc_id']}: no bots-app participant record with its media state")
                continue
            for media in ("camera", "mic"):
                if bool(got.get(media)) != want[media]:
                    out.append(f"{proc['proc_id']}: {media} is {'on' if got.get(media) else 'off'} after joining, "
                               f"the scenario has it {'on' if want[media] else 'off'}")
        return out

    def rust_ready(self, proc):
        f = self._read(self.ctx.path("rust", f"participants-{proc['proc_id']}.json")) or {}
        ps = f.get("participants") or []
        return f.get("media_started_at") is not None and len(ps) == len(proc["participants"]) and all(
            isinstance(p, dict) and p.get("join_ts") is not None for p in ps)


class LocalRun:
    def __init__(self, plan, fid, run_dir, log, relay_container=None, iface="eth0", min_free_gb=MIN_FREE_GB):
        self.plan, self.fid, self.run_dir, self.log = plan, fid, run_dir, log
        self.ctx = cq_local.Local(plan, run_dir, cq_local.Settings("default", False), stack_mode="reuse")
        self.ctx.mode = "default"
        self.relay_container, self.iface, self.min_free_gb = relay_container, iface, min_free_gb
        self.preconditions, self.facts = [], {}
        self.stack_root, self.window, self.teardown_error = None, None, None
        self.capture_proc, self.capture_log = None, None
        self.stop = threading.Event()
        parts = {p["user_id"]: p for p in plan["participants"]}
        browsers = [p for p in plan["processes"] if p["fleet"] == "browser"]
        self.fleet = Fleet(self.ctx, fid["browser_simulcast_max_layers"], fid["browser_hardware_concurrency"],
                           bot_indices(browsers, parts, fid["browser_source"]))

    def event(self, name, **kw):
        rec = {"t": round(time.time(), 3), "event": name, **kw}
        with open(os.path.join(self.run_dir, "events.jsonl"), "a", encoding="utf-8") as fh:
            fh.write(json.dumps(self.ctx.scrub_obj(rec)) + "\n")
        self.log(f"{time.strftime('%H:%M:%S')} {name} {json.dumps(kw) if kw else ''}".rstrip())
        return rec["t"]

    def git(self, root, *args, check=False):
        return self.ctx.sh(["git", "-C", root, *args], cq_local.DOCKER_S, check=check)

    def preflight(self):
        """Problems that refuse the run before anything starts."""
        c, problems = self.ctx, []
        try:
            c.sh([c.docker, "version", "--format", "{{.Server.Version}}"], cq_local.DOCKER_S)
        except AdapterError as exc:
            return [f"docker is not reachable: {exc}"]
        need = ["Dockerfile.bot", cq_local.TALKER_WAV, os.path.join("e2e", cq_local.TSX), cq_local.COMPOSE_E2E]
        missing = [f for f in need if not os.path.exists(os.path.join(c.root, f))]
        if missing:
            problems.append(f"missing in the checkout: {', '.join(missing)} (e2e/node_modules needs npm ci)")
        if c.sh([c.docker, "image", "inspect", cq_local.PW_IMAGE], cq_local.DOCKER_S, check=False).returncode:
            problems.append(f"no image {cq_local.PW_IMAGE}: docker pull it")
        stale = self.run_containers()
        for run_id in sorted(set(stale)):
            problems.append(f"{stale.count(run_id)} container(s) of run {run_id} exist; another run may be going. "
                            f"If it was killed: docker rm -f $(docker ps -aq --filter label={cq_local.LABEL}={run_id})")
        try:
            with open(os.path.join(c.root, cq_local.COMPOSE_E2E), encoding="utf-8") as fh:
                m = re.search(r"JWT_SECRET=([^\s\"']+)", fh.read())
            c.jwt_secret = m.group(1) if m else None
        except OSError:
            c.jwt_secret = None
        if not c.jwt_secret:
            problems.append(f"no dev JWT_SECRET in {cq_local.COMPOSE_E2E}")
        if self.relay_container is None:
            ids = c.sh([c.docker, "ps", "-q", "--filter", f"label=com.docker.compose.project={c.project}", "--filter",
                        f"label=com.docker.compose.service={RELAY_SERVICE}"], cq_local.DOCKER_S).stdout.split()
            if len(ids) != 1:
                problems.append(f"expected one running {RELAY_SERVICE} container of compose project {c.project}, "
                                f"found {len(ids)}; bring the e2e stack up or pass --relay-container")
            else:
                self.relay_container = ids[0]
        free = shutil.disk_usage(existing_parent(self.run_dir)).free / 2**30
        if free < self.min_free_gb:
            problems.append(f"{free:.1f} GiB free where the run folder goes; the capture needs {self.min_free_gb:g} "
                            "GiB (--min-free-gb)")
        if not os.path.exists(os.path.join(c.root, cq_local.SAMPLER)):
            problems.append(f"missing in the checkout: {cq_local.SAMPLER}")
        if self.relay_container == "":
            problems.append("--relay-container is empty: name the e2e stack's websocket-api container or leave it out")
        elif self.relay_container:
            fmt = "\t".join('{{index .Config.Labels "com.docker.compose.%s"}}' % k
                            for k in ("project", "service", "project.working_dir"))
            q = c.sh([c.docker, "inspect", "--format", fmt, self.relay_container], cq_local.DOCKER_S, check=False)
            project, service, wd = (q.stdout.rstrip("\n").split("\t") + ["", ""])[:3]
            if (project, service) != (c.project, RELAY_SERVICE):
                problems.append(f"{self.relay_container} is service {service or '-'} of compose project "
                                f"{project or '-'}, not {RELAY_SERVICE} of {c.project}")
            else:
                self.stack_root = os.path.dirname(wd) if wd and os.path.isdir(wd) else None
                problems += self.stamp_problems()
                problems += self.provenance_problems()
        for name, url in (("UI", f"{c.ui_url}/"), ("relay", f"http://localhost:{c.ports['ws']}/metrics")):
            status, _ = c.http(url)
            if status != 200:
                problems.append(f"{name} at {url} answered {status}")
        c.commit = self.git(c.root, "rev-parse", "HEAD", check=True).stdout.strip()
        return problems

    def run_containers(self):
        """The run id of every container labelled cq.run_id, once per container."""
        c = self.ctx
        return c.sh([c.docker, "ps", "-a", "--filter", f"label={cq_local.LABEL}", "--format",
                     f'{{{{.Label "{cq_local.LABEL}"}}}}'], cq_local.DOCKER_S).stdout.split()

    def stamps(self):
        folder = os.path.join(self.stack_root, STAMPS)
        out = {}
        for name in sorted(os.listdir(folder)) if os.path.isdir(folder) else []:
            try:
                with open(os.path.join(folder, name), encoding="utf-8") as fh:
                    out[name] = json.load(fh).get("build")
            except (OSError, ValueError):
                out[name] = None
        return out

    def stamp_problems(self, settle_s=STAMP_SETTLE_S, build_s=STAMP_BUILD_S, poll_s=2.0):
        """Wait while a backend build is running; any stamp other than ok refuses. With no relay stamp after
        settle_s the relay has not rebuilt since the stamps were cleared, which is accepted and recorded."""
        if self.stack_root is None:
            return ["the relay's checkout could not be found, so its build state is unknown"]
        start = time.monotonic()
        while True:
            st, waited = self.stamps(), time.monotonic() - start
            building = [n for n, b in st.items() if b == "building"]
            if (building and waited < build_s) or (RELAY_STAMP not in st and waited < settle_s):
                STOP.pause(poll_s, "the wait for the stack's build")
                continue
            self.facts["build_stamps"] = st
            return [f"{STAMPS}/{n}: build {b!r}; the stack did not finish building" for n, b in st.items()
                    if b != "ok"]

    def provenance_problems(self):
        """The running relay must be shown to be built from the stack checkout: its git_sha matches, or it was
        built after the checkout last moved. Anything else could be a stale binary, so the run is refused."""
        if self.stack_root is None:
            return []
        c = self.ctx
        status, body = c.http(f"http://localhost:{c.ports['ws']}/version")
        try:
            info = json.loads(body) if status == 200 else None
        except ValueError:
            info = None
        head = self.git(self.stack_root, "rev-parse", "HEAD")
        moved = self.git(self.stack_root, "reflog", "show", "-1", "--date=unix", "--format=%gd", "HEAD")
        m = re.search(r"\{(\d+)\}", moved.stdout or "")
        moved_at = int(m.group(1)) if m and not moved.returncode else None
        self.facts["relay_build"] = {"version": info, "checkout_moved_at": moved_at}
        if not isinstance(info, dict):
            return [f"the relay's /version answered {status}, so its build cannot be traced to the checkout"]
        sha = str(info.get("git_sha") or "unknown")
        commit = head.stdout.strip() if not head.returncode else ""
        if sha != "unknown":
            if commit and commit.startswith(sha):
                return []
            return [f"the relay reports git_sha {sha}, the stack checkout is at {commit[:12] or 'unknown'}. Restart "
                    "the websocket-api container so it rebuilds; if it still reports the old commit, its build "
                    "script did not re-run (actix-api/build.rs re-runs only when .git/HEAD, or GIT_SHA, GIT_BRANCH or "
                    "BUILD_TIMESTAMP in its environment, changes), so touch "
                    "actix-api/build.rs in the stack checkout and restart again"]
        built = iso_epoch(info.get("build_timestamp"))
        if built is None or moved_at is None:
            return ["the relay's build time or the checkout's last move is unknown, so a stale relay cannot be ruled "
                    "out"]
        if built < moved_at + BUILD_AFTER_MOVE_S:
            moved_iso = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(moved_at))
            return [f"the relay was built at {info.get('build_timestamp')}, not at least {BUILD_AFTER_MOVE_S} s after "
                    f"the stack checkout moved at {moved_iso}. Restart "
                    "the websocket-api container so it rebuilds; if it still reports the old build time, its build "
                    "script did not re-run (actix-api/build.rs re-runs only when .git/HEAD, or GIT_SHA, GIT_BRANCH or "
                    "BUILD_TIMESTAMP in its environment, changes), so touch "
                    "actix-api/build.rs in the stack checkout and restart again"]
        return []

    def note_dirty(self, root, which):
        p = self.git(root, "status", "--porcelain", "--untracked-files=all", "--", *PRODUCT_PATHS)
        if p.returncode:
            self.preconditions.append(f"git status of the {which} checkout exited {p.returncode}, so uncommitted "
                                      "product changes cannot be ruled out")
        elif p.stdout.strip():
            self.preconditions.append(f"the {which} checkout has uncommitted product changes")

    def commit_facts(self):
        """The harness and stack checkouts must carry the same product code; a difference makes the run INVALID."""
        c = self.ctx
        self.facts["harness_commit"] = c.commit
        self.note_dirty(c.root, "harness")
        stack_root = self.stack_root
        head = self.git(stack_root, "rev-parse", "HEAD") if stack_root else None
        if head is None or head.returncode:
            self.facts["stack_commit"] = None
            self.preconditions.append("the relay's checkout could not be read, so one commit cannot be verified")
            return
        stack_commit = head.stdout.strip()
        self.facts["stack_commit"] = stack_commit
        self.note_dirty(stack_root, "stack")
        if stack_commit != c.commit:
            diff = self.git(c.root, "diff", "--quiet", stack_commit, c.commit, "--", *PRODUCT_PATHS)
            if diff.returncode == 1:
                self.preconditions.append(f"the stack runs {stack_commit[:12]} and the harness {c.commit[:12]}; "
                                          "their product code differs")
            elif diff.returncode:
                self.preconditions.append(f"git diff of {stack_commit[:12]} and {c.commit[:12]} exited "
                                          f"{diff.returncode}, so their product code cannot be compared")

    def capture_image(self):
        with open(CAPTURE_DOCKERFILE, "rb") as fh:
            tag = f"{CAPTURE_REPO}:{hashlib.sha256(fh.read()).hexdigest()[:12]}"
        c = self.ctx
        if c.sh([c.docker, "image", "inspect", tag], cq_local.DOCKER_S, check=False).returncode:
            c.sh([c.docker, "build", "-q", "-t", tag, "-f", CAPTURE_DOCKERFILE, HERE], cq_local.BUILD_S)
        return tag

    def start_capture(self, image):
        """tcpdump in the relay's network namespace with only NET_RAW, writing the pcap to its stdout; the file is
        created here, mode 0600 in a 0700 folder, and dockerd keeps no copy. busybox timeout ends a sidecar whose
        harness was killed."""
        c = self.ctx
        cap = c.path("capture")
        os.makedirs(cap, mode=0o700, exist_ok=True)
        os.chmod(cap, 0o700)
        self.capture_log = os.path.join(cap, "tcpdump.log")
        argv = [c.docker, "run", "--rm", "--name", c.container("capture"), "--label", f"{cq_local.LABEL}={c.run_id}",
                "--label", f"{cq_local.PROC_LABEL}=capture", "--network", f"container:{self.relay_container}",
                "--cap-drop", "ALL", "--cap-add", "NET_RAW", "--security-opt", "no-new-privileges", "--read-only",
                "--ulimit", "core=0", "--log-driver", "none", "--entrypoint", "timeout", image,
                str(self.fleet._ttl_s()), "tcpdump", "-i", self.iface, "-p", "-s", "0", "-B", "262144", "-w", "-",
                "tcp", "port", str(RELAY_PORT)]
        fd = os.open(os.path.join(cap, "relay.pcap"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "wb") as out, open(self.capture_log, "wb") as err:
            self.capture_proc = subprocess.Popen(argv, stdout=out, stderr=err, stdin=subprocess.DEVNULL,
                                                 start_new_session=True)
        deadline = time.monotonic() + CAPTURE_READY_S
        while time.monotonic() < deadline:
            log = self.read_capture_log()
            if "listening on" in log:
                return
            if self.capture_proc.poll() is not None:
                raise AdapterError(f"the capture sidecar exited: {c.scrub(log.strip())[-300:]}")
            STOP.pause(1, "the capture start")
        raise AdapterError(f"the capture sidecar was not listening after {CAPTURE_READY_S} s")

    def read_capture_log(self):
        try:
            with open(self.capture_log, encoding="utf-8", errors="replace") as fh:
                return fh.read()
        except (OSError, TypeError):
            return ""

    def stop_capture(self):
        c, proc = self.ctx, self.capture_proc
        try:
            c.sh([c.docker, "stop", "-t", "10", c.container("capture")], cq_local.DOCKER_S)
            if proc is not None:
                proc.wait(CAPTURE_STOP_S)
        except (AdapterError, subprocess.TimeoutExpired) as exc:
            if proc is not None and proc.poll() is None:
                proc.kill()
            self.facts["capture"] = {"packets_captured": None, "packets_dropped_by_kernel": None}
            self.preconditions.append(f"stopping the capture failed, so the pcap may be incomplete: {exc}")
            return
        captured, dropped = capture_stats(self.read_capture_log())
        self.facts["capture"] = {"packets_captured": captured, "packets_dropped_by_kernel": dropped}
        if dropped is None or captured is None:
            self.preconditions.append("tcpdump printed no capture summary, so kernel drops are unknown")
        elif dropped:
            self.preconditions.append(f"the capture dropped {dropped} packets in the kernel")

    def save_capture_log(self):
        """Runs after _save_containers, whose docker logs of a capture left running has no copy to read."""
        if self.capture_log is not None:
            self.ctx.write_log(os.path.join("logs", "capture.log.gz"), self.read_capture_log())

    def remove_containers(self):
        """Force-remove every container labelled with this run; returns what could not be done."""
        c, failures = self.ctx, []
        if self.capture_proc is not None and self.capture_proc.poll() is None:
            self.capture_proc.kill()
        try:
            p = c.sh([c.docker, "ps", "-aq", "--filter", f"label={cq_local.LABEL}={c.run_id}"], cq_local.DOCKER_S,
                     check=False)
            if p.returncode:
                return [f"docker ps exited {p.returncode}: {c.scrub(p.stderr.strip())[-200:]}"]
            ids = p.stdout.split()
            if ids:
                q = c.sh([c.docker, "rm", "-f", *ids], cq_local.DOCKER_S, check=False)
                if q.returncode:
                    failures.append(f"docker rm -f exited {q.returncode}: {c.scrub(q.stderr.strip())[-200:]}")
        except AdapterError as exc:
            failures.append(str(exc))
        return failures

    def hold(self, hold_s):
        end = time.monotonic() + hold_s
        while time.monotonic() < end:
            STOP.pause(min(HOLD_POLL_S, max(0.0, end - time.monotonic())), "the hold")
            for ex in self.fleet.poll():
                self.preconditions.append(f"{ex.proc_id} exited during the hold (code {ex.exit_code})")

    def sampler(self):
        c = self.ctx
        path = c.path("samples", "relay_metrics.jsonl.gz")
        os.makedirs(os.path.dirname(path), exist_ok=True)

        def loop():
            with gzip.open(path, "at", encoding="utf-8") as fh:
                while not self.stop.is_set():
                    t = time.time()
                    status, body = c.http(f"http://localhost:{c.ports['ws']}/metrics")
                    lines = fidelity_measures.room_lines(body.decode(errors="replace").splitlines(),
                                                         self.plan["meeting_id"]) if status == 200 else []
                    fh.write(json.dumps({"t": round(t, 3), "v": lines}) + "\n")
                    self.stop.wait(max(0.1, SAMPLE_S - (time.time() - t)))

        def stats_loop():
            names = {c.container("capture"): "tcpdump", self.relay_container: "relay"}
            with gzip.open(c.path("samples", "docker_stats.jsonl.gz"), "at", encoding="utf-8") as fh:
                while not self.stop.is_set():
                    t = time.time()
                    try:
                        p = c.sh([c.docker, "stats", "--no-stream", "--format", "{{.Name}}\t{{.ID}}\t{{.CPUPerc}}",
                                  *names], cq_local.DOCKER_S, check=False)
                    except AdapterError:
                        p = None
                    cpu = {}
                    for line in (p.stdout.splitlines() if p is not None else []):
                        name, cid, pct = (line.split("\t") + ["", ""])[:3]
                        role = names.get(name) or next((r for n, r in names.items() if cid and n.startswith(cid)),
                                                       None)
                        try:
                            cpu[role or name] = float(pct.rstrip("%"))
                        except ValueError:
                            continue
                    fh.write(json.dumps({"t": round(t, 3), "cpu_pct": cpu}) + "\n")
                    self.stop.wait(max(0.1, SAMPLE_S - (time.time() - t)))

        threads = [threading.Thread(target=f, daemon=True) for f in (loop, stats_loop)]
        for th in threads:
            th.start()
        return threads

    def host_verdict(self, t0, t1):
        """The scale runner's host resource verdict over the hold; anything but ok makes the run INVALID."""
        try:
            verdict = self.fleet.generator_verdicts(t0, t1)[0]
        except (AdapterError, IndexError) as exc:
            verdict = {"ok": False, "reasons": [f"no host resource verdict: {exc}"]}
        self.facts["host_resource_verdict"] = verdict
        if not verdict.get("ok"):
            self.preconditions.append("host resources over the hold: " + "; ".join(
                verdict.get("reasons") or [str(verdict.get("detail"))]))

    def wait(self, what, ok, budget):
        deadline = time.monotonic() + budget
        while not ok():
            if time.monotonic() >= deadline:
                raise AdapterError(f"{what} not done after {budget} s")
            STOP.pause(2, what)

    def run(self):
        """Launch, hold, leave; returns the hold window. A recorded signal stops it with Interrupted at the next
        check. The finally block stops the capture and collects and removes the run's containers; cmd_run
        force-removes any left after that."""
        c, procs = self.ctx, self.plan["processes"]
        STOP.check("the start")
        rust = [p for p in procs if p["fleet"] == "rust"]
        browsers = [p for p in procs if p["fleet"] == "browser"]
        self.log("building the Rust bot image and the capture image (a first build takes minutes)")
        image_builder = cq_local.BotImage()
        image_builder.ctx = c
        image_builder._bot_image()
        self.log(f"Rust bot image {c.bot_image}")
        image = self.capture_image()
        STOP.check("the image builds")
        samplers = []
        try:
            self.fleet._start_sampler()
            self.start_capture(image)
            self.event("capture_started")
            samplers = self.sampler()
            for p in rust:
                self.fleet.start(p)
                STOP.check("the Rust bot launch")
            self.wait("Rust bots joining with media", lambda: all(self.fleet.rust_ready(p) for p in rust),
                      RUST_JOIN_S)
            self.event("rust_joined")
            for i, p in enumerate(browsers):
                if i:
                    STOP.pause(p["join_stagger_s"], "the browser launch")
                self.fleet.start(p)
                STOP.check("the browser launch")
            self.wait("browsers joining", lambda: all(self.fleet._in_meeting(p["proc_id"]) for p in browsers),
                      BROWSER_JOIN_S)
            self.event("browsers_joined")
            wrong = self.fleet.media_problems(browsers)
            if wrong:
                raise AdapterError("browser media differs from the scenario, so the hold would be INVALID: "
                                   + "; ".join(wrong))
            STOP.pause(self.fid["settle_s"], "the settle before the hold")
            t0 = self.event("hold_start")
            self.hold(self.plan["steps"][0]["hold_s"])
            self.window = (t0, self.event("hold_end"))
            self.host_verdict(*self.window)
            try:
                for p in browsers:
                    if p["proc_id"] not in self.fleet.reported:
                        self.fleet.apply({"action": "leave", "participants": p["participants"]})
                STOP.pause(LEAVE_GRACE_S, "leaving")
                for p in rust + browsers:
                    self.fleet.stop(p)
                self.event("left")
            except AdapterError as exc:
                self.teardown_error = str(exc)
            return self.window
        finally:
            self.stop.set()
            for th in samplers:
                th.join(SAMPLE_S * 3)
            try:
                self.stop_capture()
            finally:
                try:
                    self.fleet._stop_sampler()
                finally:
                    try:
                        self.fleet._save_containers()
                    finally:
                        self.save_capture_log()
