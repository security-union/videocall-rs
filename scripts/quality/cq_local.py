"""Local adapters for the scenario runner (#2914 PR-5): the compose stack, Rust bot and bots-app probe containers."""

import calendar
import datetime
import gzip
import json
import os
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import wave
from dataclasses import dataclass, field

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from cq_runner import AdapterError, EventOutcome, Exit, Joined  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(HERE))
PW_IMAGE = "mcr.microsoft.com/playwright:v1.58.2-noble"
BOT_REPO = "videocall-bot"
BOT_IMAGE = BOT_REPO + ":cq-{}"
TREE_LABEL = "cq.tree"
REVISION_LABEL = "org.opencontainers.image.revision"
BOT_LABELS_FORMAT = f'{{{{index .Config.Labels "{REVISION_LABEL}"}}}}\t{{{{index .Config.Labels "{TREE_LABEL}"}}}}'
LABEL = "cq.run_id"
PROC_LABEL = "cq.proc_id"
COMPOSE_E2E = "docker/docker-compose.e2e.yaml"
COMPOSE_MONITORING = "docker/docker-compose.monitoring.yaml"
COMPOSE_ALT = "docker/docker-compose.scale-ports.yaml"
TSX = "node_modules/.bin/tsx"
CLI = "bots-app/src/cli.ts"
SAMPLER = "e2e/bots-app/scripts/resource-sampler.sh"
PROBE_NODE_CMDLINE = r"^(/[^ ]*/)?node( --(import|require)[= ][^ ]+)* bots-app/src/cli\.ts run( |$)"
PROBE_NODE_COMM = "bots-app-node"
TALKER_WAV = "bot/BundyBests2.wav"
PORTS = {
    "default": {"project": "videocall-e2e", "ui": 3001, "api": 8081, "ws": 8080, "wt": 4433, "wt_metrics": 5321,
                "prom": 9090, "grafana": 3000},
    "alt": {"project": "videocall-scale", "ui": 3011, "api": 8181, "ws": 8180, "wt": 4434, "wt_metrics": 5331,
            "prom": 9190, "grafana": 3100},
}
UDP_PORTS = ("wt",)
E2E_PROJECTS = {m["project"] for m in PORTS.values()}
EXPECTED_JOBS = ("prometheus", "videocall-client-metrics", "videocall-server-stats", "relay-ws", "relay-wt")
SCRAPE_INTERVAL = "15s"
SCRAPE_UP_SELECTOR = 'up{job=~"relay-ws|relay-wt|videocall-client-metrics|videocall-server-stats"}'
IMAGE_SERVICES = {"ui": "dioxus-ui", "relay_ws": "websocket-api", "relay_wt": "webtransport-api"}
CTL_PORT_BASE = 7800
MAX_PROBES = 99
RUST_STOP_S = 45
PROBE_STOP_S = 30
DOCKER_S = 60
LOGS_S = 120
CERT_S = 120
UP_S = 1800
BUILD_S = 1800
READY_S = 1800
READY_REUSE_S = 120
DOWN_S = 300
HTTP_S = 10
SAMPLER_STOP_S = 15
SAMPLER_INTERVAL_S = 5
GATE_MAX_S = 600
GATE_POLL_S = 30
GATE_LOAD1 = 4.0
GATE_HOT_PCT = 20.0
GATE_BUSY = ("cargo", "rustc")
POLL_TRIES = 3
OWNER_WAIT_S = 3 * 45 + 60
OWNER_POLL_S = 1.0
TTL_MARGIN_S = 1200
PRUNE_AGE_S = 86400
DECODE_OFF_VALUE = "all"
RUST_VIDEO_MODE = "ekg"
RUST_LOG = "info,bot::video_producer::replay=debug"
PINNED_ENV = {"DIOXUS_SERVE_MODE": "dev", "WT_DEV_CERT_HASH_INJECT": "true"}
STACK_ENV = ("VIDEOCALL_RELEASE_BUILD", *PINNED_ENV)
CTL_JS = ("const [port, method, path, body] = process.argv.slice(1);"
          "fetch(`http://127.0.0.1:${port}${path}`, {method, body: body || undefined, signal: AbortSignal.timeout(15000),"
          " headers: {authorization: `Bearer ${process.env.BOT_CTL_TOKEN}`, 'content-type': 'application/json'}})"
          ".then(async (r) => console.log(JSON.stringify({status: r.status, body: await r.text()})))"
          ".catch((e) => console.log(JSON.stringify({status: 0, body: String(e)})));")
QDISC_PY = r'''
import json, os, socket, struct
s = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 0)
s.bind((0, 0))
tcm = struct.pack("BxxxiIII", 0, 0, 0, 0, 0)
s.send(struct.pack("=IHHII", 16 + len(tcm), 38, 0x301, 1, 0) + tcm)
out, done = [], False
while not done:
    data = s.recv(65536)
    off = 0
    while off + 16 <= len(data):
        ln, typ = struct.unpack_from("=IH", data, off)
        if typ in (2, 3):
            done = True
            if typ == 2:
                raise SystemExit("netlink error")
            break
        _, ifi, handle, parent, _ = struct.unpack_from("BxxxiIII", data, off + 16)
        a, kind = off + 36, None
        while a + 4 <= off + ln:
            alen, atype = struct.unpack_from("=HH", data, a)
            if alen < 4:
                break
            if atype == 1:
                kind = data[a + 4:a + alen].rstrip(b"\0").decode()
            a += (alen + 3) & ~3
        out.append({"ifindex": ifi, "kind": kind, "parent": parent})
        off += (ln + 3) & ~3
names = {int(open(f"/sys/class/net/{n}/ifindex").read()): n for n in os.listdir("/sys/class/net")}
print(json.dumps([dict(q, dev=names.get(q["ifindex"])) for q in out]))
'''
DOCKER_TIME_RE = re.compile(r"^(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)(?:\.(\d{1,9}))?(?:Z|([+-])(\d\d):(\d\d))$")
TOKEN_RE = re.compile(r"((?:token|access_token|jwt)(?:=|%3D|\"\s*:\s*\"))[A-Za-z0-9._%~+/-]+", re.IGNORECASE)
BEARER_RE = re.compile(r"(Bearer\s+)[A-Za-z0-9._~+/=-]+", re.IGNORECASE)
JWT_RE = re.compile(r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}")
PORT_RE = re.compile(r":(\d+)->\d+/(tcp|udp)")


def docker_time(text):
    """Epoch seconds of a Docker or Podman RFC 3339 timestamp, or None for the zero time or anything unparseable."""
    m = DOCKER_TIME_RE.match(text or "")
    if m is None or m.group(1) == "0001":
        return None
    y, mo, d, h, mi, s = (int(g) for g in m.groups()[:6])
    frac = m.group(7) or "0"
    sign = {None: 0, "+": 1, "-": -1}[m.group(8)]
    offset = sign * (int(m.group(9) or 0) * 3600 + int(m.group(10) or 0) * 60)
    return calendar.timegm((y, mo, d, h, mi, s, 0, 0, 0)) + int(frac) / 10 ** len(frac) - offset


def redact(text, values=()):
    for v in sorted((v for v in values if v), key=len, reverse=True):
        text = text.replace(v, "<redacted>")
    text = TOKEN_RE.sub(r"\1<redacted>", text)
    text = BEARER_RE.sub(r"\1<redacted>", text)
    return JWT_RE.sub("<jwt>", text)


def serve_mode(ui_env):
    """What docker/start-dioxus.sh serves: a release build is always static."""
    if ui_env is None:
        return None
    return "static" if ui_env.get("VIDEOCALL_RELEASE_BUILD") == "1" else ui_env.get("DIOXUS_SERVE_MODE", "dev")


def image_created(text):
    """Epoch seconds of a `docker image ls` CreatedAt ("2026-10-07 11:35:24 +0700 +07"), or now if unparseable."""
    try:
        return datetime.datetime.strptime(" ".join(text.split()[:3]), "%Y-%m-%d %H:%M:%S %z").timestamp()
    except ValueError:
        return time.time()


def describe(exc):
    return str(exc) if isinstance(exc, AdapterError) else f"{type(exc).__name__}: {exc}"


def is_camera(m, cameras, muted):
    """The Rust bot's camera spread over non-broadcaster positions (bot/src/config.rs build_roster)."""
    return cameras >= muted or (m + 1) * cameras // muted > m * cameras // muted


def probe_flags(proc):
    return (["--skip-canvas-paint", "true"] if proc.get("skip_canvas_paint") else []) + (
        ["--diag-packets", "off"] if proc.get("diag_packets") == "off" else []) + (
        ["--speaking-glow", "off"] if proc.get("speaking_glow") == "off" else [])


def rust_flags(plan):
    return ["--diagnostics", plan["bot_diagnostics"]]


def rust_layout(plan):
    """Shared Rust roster in position order, and per process its bot flags; every role maps exactly onto the plan."""
    parts = {p["user_id"]: p for p in plan["participants"]}
    roster, flags, problems = [], {}, []
    for proc in sorted((p for p in plan["processes"] if p["fleet"] == "rust"), key=lambda p: p["roster_offset"]):
        members = [parts[u] for u in proc["participants"]]
        talkers = [p for p in members if p["talker"]]
        rest = [p for p in members if not p["talker"]]
        cams = [p for p in rest if p["publishes"]["camera"]]
        plain = [p for p in rest if not p["publishes"]["camera"]]
        offset, n = proc["roster_offset"], len(members)
        if offset != len(roster):
            problems.append(f"{proc['proc_id']}: roster offset {offset} is not contiguous")
        if any(not p["publishes"]["camera"] for p in talkers):
            problems.append(f"{proc['proc_id']}: a Rust talker always publishes its camera; a camera-off talker "
                            "cannot be honoured")
        broadcasters = offset + len(talkers)
        if broadcasters == 0:
            problems.append(f"{proc['proc_id']}: the Rust bot reads --broadcasters 0 as every participant, so a "
                            "process at roster offset 0 needs a talker")
        order, cam_it, plain_it = list(talkers), iter(cams), iter(plain)
        order += [next(cam_it) if is_camera(m, len(cams), len(rest)) else next(plain_it) for m in range(len(rest))]
        roster += order
        flags[proc["proc_id"]] = ["--roster-offset", str(offset), "--users", str(n), "--run-size", str(offset + n),
                                  "--broadcasters", str(broadcasters), "--talkers", str(broadcasters),
                                  "--cameras", str(len(cams))]
    return roster, flags, problems


def targets_problems(doc, jobs=EXPECTED_JOBS, interval=SCRAPE_INTERVAL):
    """Problems with a Prometheus /api/v1/targets answer; empty: every expected job is up, scraped every 15 s."""
    if not isinstance(doc, dict) or doc.get("status") != "success" or not isinstance(doc.get("data"), dict):
        return ["Prometheus /api/v1/targets did not answer success"]
    active = doc["data"].get("activeTargets")
    if not isinstance(active, list):
        return ["Prometheus /api/v1/targets has no activeTargets list"]
    problems = []
    for job in jobs:
        ts = [t for t in active if isinstance(t, dict) and (t.get("labels") or {}).get("job") == job]
        if not ts:
            problems.append(f"target {job}: not configured")
        for t in ts:
            if t.get("health") != "up":
                problems.append(f"target {job}: health {t.get('health')!r}")
            if t.get("scrapeInterval") != interval:
                problems.append(f"target {job}: scrape interval {t.get('scrapeInterval')!r}, the scorer needs "
                                f"{interval}")
    return problems


def slim_targets(doc):
    active = ((doc or {}).get("data") or {}).get("activeTargets") or []
    keep = ("health", "lastScrape", "lastScrapeDuration", "scrapeInterval", "scrapeTimeout")
    return [dict({k: t.get(k) for k in keep}, job=(t.get("labels") or {}).get("job"),
                 instance=(t.get("labels") or {}).get("instance")) for t in active if isinstance(t, dict)]


def qdisc_problems(qdiscs):
    """The probes' link counts as unshaped only with no netem anywhere and a noqueue root on lo."""
    if not isinstance(qdiscs, list) or not qdiscs:
        return ["no qdisc read from the probes' network namespace"]
    problems = [f"netem qdisc on {q.get('dev')}" for q in qdiscs if q.get("kind") == "netem"]
    lo = [q.get("kind") for q in qdiscs if q.get("dev") == "lo" and q.get("parent") == 0xFFFFFFFF]
    if lo != ["noqueue"]:
        problems.append(f"lo root qdisc is {lo}, not noqueue")
    return problems


def gate_reading(procs_before, procs_after, dt, load1, own):
    """(ok, detail) for the quiet-host gate: no cargo/rustc/vitest, nothing above 20 % of a core, load1 below 4."""
    tick = os.sysconf("SC_CLK_TCK")
    busy = sorted({c for pid, (c, _, _) in procs_after.items() if pid not in own
                   and (c in GATE_BUSY or "vitest" in procs_after[pid][2])})
    hot = sorted(f"{c}:{100 * (j - procs_before[pid][1]) / tick / dt:.0f}%"
                 for pid, (c, j, _) in procs_after.items() if pid in procs_before and pid not in own
                 and 100 * (j - procs_before[pid][1]) / tick / dt > GATE_HOT_PCT)
    ok = not busy and not hot and load1 < GATE_LOAD1
    return ok, f"load1={load1:.2f} busy={busy} hot={hot}"


def read_procs():
    out = {}
    for pid in (p for p in os.listdir("/proc") if p.isdigit()):
        try:
            with open(f"/proc/{pid}/stat", encoding="utf-8", errors="replace") as fh:
                stat = fh.read()
            with open(f"/proc/{pid}/cmdline", "rb") as fh:
                cmd = fh.read().replace(b"\0", b" ").decode(errors="replace")
        except OSError:
            continue
        comm, rest = stat[stat.index("(") + 1:stat.rindex(")")], stat[stat.rindex(")") + 2:].split()
        out[int(pid)] = (comm, int(rest[11]) + int(rest[12]), cmd)
    return out


def own_tree():
    parents = {}
    for pid in (p for p in os.listdir("/proc") if p.isdigit()):
        try:
            with open(f"/proc/{pid}/stat", encoding="utf-8", errors="replace") as fh:
                parents[int(pid)] = int(fh.read().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            continue
    tree, grew = {os.getpid()}, True
    while grew:
        new = {pid for pid, ppid in parents.items() if ppid in tree} - tree
        tree |= new
        grew = bool(new)
    return tree


def sampler_writers(csv_path):
    """Sampler processes writing this CSV, not counting the subshells a sampler forks."""
    want, found = os.path.realpath(csv_path), {}
    for pid in (p for p in os.listdir("/proc") if p.isdigit()):
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as fh:
                argv = [a.decode(errors="replace") for a in fh.read().split(b"\0")]
            with open(f"/proc/{pid}/stat", encoding="utf-8", errors="replace") as fh:
                ppid = int(fh.read().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            continue
        out = argv[argv.index("--out") + 1:argv.index("--out") + 2] if "--out" in argv else []
        if any(a.endswith("resource-sampler.sh") for a in argv) and out and os.path.realpath(out[0]) == want:
            found[int(pid)] = ppid
    return sum(1 for ppid in found.values() if ppid not in found)


def wav_ms(path):
    with wave.open(path, "rb") as w:
        return int(1000 * w.getnframes() / w.getframerate())


@dataclass
class Settings:
    ports: str = "auto"
    release: bool = False

    @classmethod
    def from_env(cls, env):
        ports = env.get("CQ_PORTS") or "auto"
        if ports not in ("auto", "default", "alt"):
            raise AdapterError(f"CQ_PORTS must be auto, default or alt, not {ports!r}")
        release = env.get("CQ_RELEASE_BUILD") or "0"
        if release not in ("0", "1"):
            raise AdapterError(f"CQ_RELEASE_BUILD must be 0 or 1, not {release!r}")
        return cls(ports, release == "1")


@dataclass
class Local:
    """What the stack and fleet adapters share for one run."""
    plan: dict
    run_dir: str
    settings: Settings
    root: str = ROOT
    docker: str = "docker"
    uid: int = field(default_factory=os.getuid)
    gid: int = field(default_factory=os.getgid)
    mode: str = None
    stack_mode: str = "up"
    jwt_secret: str = None
    commit: str = None
    dirty: bool = None
    netem_applied: str = None
    tokens: dict = field(default_factory=dict)

    @property
    def run_id(self):
        return self.plan["run_id"]

    @property
    def node(self):
        return "ci" if self.plan["target"] == "ci" else "local"

    @property
    def ports(self):
        return PORTS[self.mode or "default"]

    @property
    def project(self):
        return self.ports["project"]

    @property
    def ui_url(self):
        return f"http://localhost:{self.ports['ui']}"

    @property
    def ws_url(self):
        return f"ws://localhost:{self.ports['ws']}"

    @property
    def bot_image(self):
        return bot_image_tag(self.commit, self.dirty)

    def container(self, proc_id):
        return f"cq-{self.run_id}-{proc_id}"

    def path(self, *parts):
        return os.path.join(self.run_dir, *parts)

    def scrub(self, text):
        text = redact(text, self.secrets())
        subs = [(self.run_dir, "$RUN"), (self.root, "$CHECKOUT"), (os.path.expanduser("~"), "~"),
                (socket.gethostname(), "<host>")]
        for old, new in sorted((s for s in subs if s[0] and len(s[0]) > 1), key=lambda s: -len(s[0])):
            text = text.replace(old, new)
        return text

    def scrub_obj(self, obj):
        if isinstance(obj, str):
            return self.scrub(obj)
        if isinstance(obj, dict):
            return {k: self.scrub_obj(v) for k, v in obj.items()}
        if isinstance(obj, list):
            return [self.scrub_obj(v) for v in obj]
        return obj

    def secrets(self):
        return [self.jwt_secret, *self.tokens.values()]

    def env(self, **extra):
        env = dict(os.environ, **{k: str(v) for k, v in extra.items()})
        if self.jwt_secret:
            env["JWT_SECRET"] = self.jwt_secret
        return env

    def sh(self, argv, timeout, check=True, env=None):
        words = argv[:3] if argv[1:2] == ["compose"] else argv[:2]
        what = " ".join([os.path.basename(words[0])] + words[1:])
        if words[1:2] == ["compose"]:
            what = "docker compose " + next((a for a in argv if a in ("up", "down", "logs", "ps", "rm")), "")
        try:
            p = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, check=False,
                               start_new_session=True, env=env or self.env(), cwd=self.root)
        except subprocess.TimeoutExpired:
            raise AdapterError(f"{what} timed out after {timeout} s") from None
        except OSError as exc:
            raise AdapterError(f"{what} could not start: {exc.strerror}") from None
        if check and p.returncode != 0:
            raise AdapterError(f"{what} exited {p.returncode}: {self.scrub(p.stderr.strip())[-400:]}")
        return p

    def compose(self, *args):
        files = [COMPOSE_E2E, COMPOSE_MONITORING] + ([COMPOSE_ALT] if self.mode == "alt" else [])
        return [self.docker, "compose", "-p", self.project, *[a for f in files for a in ("-f", f)],
                "--profile", "monitoring", *args]

    def compose_env(self):
        return self.env(PROMETHEUS_HOST_PORT=self.ports["prom"], GRAFANA_HOST_PORT=self.ports["grafana"],
                        VIDEOCALL_RELEASE_BUILD="1" if self.settings.release else "0", **PINNED_ENV)

    def run_flags(self):
        return []

    def read_qdiscs(self):
        p = self.sh([self.docker, "run", "--rm", *self.run_flags(), "--network", "host", "--label",
                     f"{LABEL}={self.run_id}", "--entrypoint", "python3", PW_IMAGE, "-c", QDISC_PY], DOCKER_S,
                    check=False)
        try:
            return json.loads(p.stdout)
        except ValueError:
            return None

    def mount(self, src, dst, *opts):
        return ":".join([src, dst] + ([",".join(opts)] if opts else []))

    def inspect(self, names, timeout=DOCKER_S):
        """{name: State} for the containers that exist; a missing one is simply absent."""
        if not names:
            return {}
        p = self.sh([self.docker, "inspect", "--format", "{{json .Name}}\t{{json .State}}", *names], timeout,
                    check=False)
        out = {}
        for line in p.stdout.splitlines():
            name, _, state = line.partition("\t")
            try:
                out[json.loads(name).lstrip("/")] = json.loads(state)
            except ValueError:
                continue
        if p.returncode != 0 and not out and "no such" not in p.stderr.lower():
            raise AdapterError(f"{os.path.basename(self.docker)} inspect exited {p.returncode}: "
                               f"{self.scrub(p.stderr.strip())[-300:]}")
        return out

    def http(self, url, timeout=HTTP_S):
        try:
            with urllib.request.urlopen(url, timeout=timeout) as r:  # noqa: S310 - localhost only
                return r.status, r.read()
        except urllib.error.HTTPError as exc:
            return exc.code, b""
        except (urllib.error.URLError, OSError, ValueError):
            return None, b""

    def prom_json(self, path, **params):
        url = f"http://localhost:{self.ports['prom']}{path}"
        if params:
            url += "?" + urllib.parse.urlencode(params)
        status, body = self.http(url)
        if status != 200:
            return None
        try:
            return json.loads(body)
        except ValueError:
            return None

    def dump(self, rel, obj):
        path = self.path(rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(self.scrub_obj(obj), fh, indent=1)
            fh.write("\n")

    def scrub_file(self, rel):
        path = self.path(rel)
        if not os.path.exists(path):
            return
        with open(path, encoding="utf-8", errors="replace") as fh:
            text = fh.read()
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(self.scrub(text))

    def write_log(self, rel, text):
        path = self.path(rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with gzip.open(path, "wt", encoding="utf-8") as fh:
            fh.write(self.scrub(text))


def bot_image_tag(commit, dirty):
    return BOT_IMAGE.format((commit or "unknown")[:12]) + ("-dirty" if dirty else "")


def bot_labels(commit, dirty):
    """What BOT_LABELS_FORMAT reads from an image built by bot_build_argv from this tree."""
    return f"{commit}\t{'dirty' if dirty else 'clean'}"


def bot_build_argv(docker, commit, tree, tag):
    return [docker, "build", "-f", "Dockerfile.bot", "--build-arg", f"GIT_SHA={commit}", "--label", f"{TREE_LABEL}={tree}",
            "-t", tag, "."]


class BotImage:
    """The Rust bot image of this checkout, built by the container runtime when missing."""

    def _bot_image(self):
        """A clean tree reuses its commit's clean image; a dirty tree always builds, under a -dirty tag."""
        c = self.ctx
        c.dirty = c.sh(["git", "-C", c.root, "status", "--porcelain=v1", "--untracked-files=all"],
                       DOCKER_S).stdout.strip() != ""
        tree = "dirty" if c.dirty else "clean"
        p = c.sh([c.docker, "image", "inspect", "--format", BOT_LABELS_FORMAT, c.bot_image], DOCKER_S, check=False)
        if p.returncode != 0 or p.stdout.strip() != f"{c.commit}\tclean":
            b = c.sh(bot_build_argv(c.docker, c.commit, tree, c.bot_image), BUILD_S, check=False)
            c.write_log(os.path.join("stack", "bot-build.log.gz"), b.stdout + b.stderr)
            if b.returncode != 0:
                raise AdapterError(f"docker build of the Rust bot exited {b.returncode}; see stack/bot-build.log.gz")
        self._prune_bot_images()

    def _prune_bot_images(self):
        c = self.ctx
        p = c.sh([c.docker, "image", "ls", "--format", "{{.Repository}}:{{.Tag}}\t{{.CreatedAt}}", BOT_REPO], DOCKER_S,
                 check=False)
        old = sorted({t for t, _, created in (line.partition("\t") for line in p.stdout.splitlines())
                      if t.startswith(BOT_IMAGE.format("")) and t != c.bot_image
                      and time.time() - image_created(created) > PRUNE_AGE_S})
        if old:
            c.sh([c.docker, "rmi", *old], DOCKER_S, check=False)
        c.sh([c.docker, "image", "prune", "-f", "--filter", f"label={TREE_LABEL}"], DOCKER_S, check=False)


class LocalStack(BotImage):
    """The e2e + monitoring compose stack, on the default ports or the scale-ports overlay."""

    def __init__(self, ctx):
        self.ctx = ctx
        self.owned, self.since = False, None

    def preflight(self, plan):
        c, problems = self.ctx, []
        try:
            c.sh([c.docker, "version", "--format", "{{.Server.Version}}"], DOCKER_S)
        except AdapterError as exc:
            return [f"docker is not reachable: {exc}"]
        need = [COMPOSE_E2E, COMPOSE_MONITORING, COMPOSE_ALT, "Dockerfile.bot", SAMPLER, os.path.join("e2e", TSX)]
        if any(p["talker"] and p["fleet"] == "rust" for p in plan["participants"]):
            need.append(TALKER_WAV)
        missing = [f for f in need if not os.path.exists(os.path.join(c.root, f))]
        if missing:
            problems.append(f"missing in the checkout: {', '.join(missing)} (e2e/node_modules needs npm ci in the "
                            "Playwright image)")
        if c.sh([c.docker, "image", "inspect", PW_IMAGE], DOCKER_S, check=False).returncode != 0:
            problems.append(f"no image {PW_IMAGE}: docker pull it")
        problems += self._stale()
        problems += self._choose_ports()
        problems += self._foreign_project()
        problems += self._fleet_preflight(plan)
        try:
            with open(os.path.join(c.root, COMPOSE_E2E), encoding="utf-8") as fh:
                m = re.search(r"JWT_SECRET=([^\s\"']+)", fh.read())
            c.jwt_secret = m.group(1) if m else None
        except OSError:
            c.jwt_secret = None
        if not c.jwt_secret:
            problems.append(f"no dev JWT_SECRET in {COMPOSE_E2E}")
        c.commit = c.sh(["git", "-C", c.root, "rev-parse", "HEAD"], DOCKER_S).stdout.strip()
        return problems

    def _stale(self):
        c = self.ctx
        p = c.sh([c.docker, "ps", "-a", "--filter", f"label={LABEL}", "--format", f'{{{{.Label "{LABEL}"}}}}'],
                 DOCKER_S)
        runs = sorted({r for r in p.stdout.split() if r})
        if not runs:
            return []
        return [f"containers labelled {LABEL}={r} exist; that run may still be going. Check with docker ps --filter "
                f"label={LABEL}={r}, and once it has ended remove them" for r in runs]

    def _here(self, working_dir):
        return bool(working_dir) and os.path.realpath(working_dir) == os.path.realpath(
            os.path.join(self.ctx.root, "docker"))

    def _containers(self):
        """(compose project, working dir, state, published ports) of every container, running or not."""
        c = self.ctx
        p = c.sh([c.docker, "ps", "-a", "--format", '{{.Label "com.docker.compose.project"}}\t'
                  '{{.Label "com.docker.compose.project.working_dir"}}\t{{.State}}\t{{.Ports}}'], DOCKER_S)
        return [tuple((line.split("\t") + ["", "", ""])[:4]) for line in p.stdout.splitlines() if line.strip()]

    def _shared_here(self, rows):
        """Problems from stacks of this checkout that share its bind-mounted state with a new stack."""
        c, out = self.ctx, []
        for project in sorted({r[0] for r in rows if r[2] not in ("exited", "dead") and self._here(r[1])}):
            if project in E2E_PROJECTS:
                out.append(f"compose project {project} runs from this checkout and shares e2e/.stack-stamps and "
                           "the UI's config with a new stack; pass --stack reuse or run from a separate worktree")
            elif not c.settings.release:
                out.append(f"compose project {project} runs from this checkout and shares dioxus-ui's dist with a "
                           "new stack; pass RELEASE=1 or run from a separate worktree")
        return out

    def _tcp_open(self, port):
        with socket.socket() as s:
            s.settimeout(1)
            return s.connect_ex(("127.0.0.1", port)) == 0

    def _choose_ports(self):
        c = self.ctx
        modes = [c.settings.ports] if c.settings.ports != "auto" else ["default", "alt"]
        rows, why = self._containers(), []
        shared = self._shared_here(rows) if c.stack_mode == "up" else []
        for mode in modes:
            ports, project = PORTS[mode], PORTS[mode]["project"]
            ours = [r for r in rows if r[0] == project]
            mine = any(r[2] == "running" for r in ours) and all(self._here(r[1]) for r in ours)
            busy = {(int(n), proto) for r in rows if not (mine and r[0] == project)
                    for n, proto in PORT_RE.findall(r[3])}
            taken = sorted(f"{n}/{'udp' if k in UDP_PORTS else 'tcp'}" for k, n in ports.items() if k != "project"
                           and ((n, "udp" if k in UDP_PORTS else "tcp") in busy
                                or (k not in UDP_PORTS and not mine and self._tcp_open(n))))
            problems = [f"{mode} ports busy: {', '.join(taken)}"] if taken else []
            if c.stack_mode == "up" and ours:
                problems.append(f"compose project {project} already has containers; --stack up only takes down a "
                                "stack it started, so " + ("pass --stack reuse or remove it" if mine else "remove it"))
            elif c.stack_mode == "reuse" and not mine:
                problems.append(f"--stack reuse: compose project {project} is not running from this checkout")
            if not problems and not shared:
                c.mode = mode
                return []
            why += problems
        c.mode = modes[-1]
        return ["; ".join(shared + why)]

    def _project_running(self, project, stopped=False):
        c = self.ctx
        p = c.sh([c.docker, "ps", *(["-a"] if stopped else []), "-q", "--filter",
                  f"label=com.docker.compose.project={project}"], DOCKER_S)
        return bool(p.stdout.strip())

    def _foreign_project(self):
        c = self.ctx
        p = c.sh([c.docker, "ps", "-a", "--filter", f"label=com.docker.compose.project={c.project}", "--format",
                  '{{.Label "com.docker.compose.project.working_dir"}}'], DOCKER_S)
        dirs = {d for d in p.stdout.split() if d}
        want = os.path.realpath(os.path.join(c.root, "docker"))
        if dirs and {os.path.realpath(d) for d in dirs} != {want}:
            return [f"compose project {c.project} belongs to another checkout; stop it or use the other ports"]
        return []

    def _fleet_preflight(self, plan):
        c, problems = self.ctx, []
        probes = [p for p in plan["processes"] if p["fleet"] == "browser"]
        if len(probes) > MAX_PROBES:
            problems.append(f"{len(probes)} probes; at most {MAX_PROBES} control ports")
        problems += [f"{p['proc_id']}: transport {p['transport']!r}; bots-app probes run auto only"
                     for p in probes if p["transport"] != "auto"]
        problems += rust_layout(plan)[2]
        if probes:
            qdiscs = c.read_qdiscs()
            c.dump(os.path.join("probes", "netns-qdisc.json"), qdiscs)
            link = qdisc_problems(qdiscs)
            c.netem_applied = None if link else "none"
            problems += [f"probe network: {x}" for x in link]
        return problems

    def up(self, plan, mode):
        c = self.ctx
        self.since = time.time()
        if mode == "up":
            self._refuse_new_stacks()
            c.sh(["bash", "scripts/regen-dev-cert.sh"], CERT_S)
            self._refuse_new_stacks()
            self._clear_stamps()
            self.owned = True
            c.sh(c.compose("up", "-d", "--build"), UP_S, env=c.compose_env())
        elif not self._project_running(c.project):
            raise AdapterError(f"--stack reuse: compose project {c.project} is not running")
        self._bot_image()
        self._wait_ready(READY_S if mode == "up" else READY_REUSE_S)
        self._quiet_gate()

    def _refuse_new_stacks(self):
        c = self.ctx
        if self._project_running(c.project, stopped=True):
            raise AdapterError(f"compose project {c.project} has containers that appeared after preflight; "
                               "--stack up does not adopt them")
        shared = self._shared_here(self._containers())
        if shared:
            raise AdapterError("a stack appeared after preflight: " + "; ".join(shared))

    def _clear_stamps(self):
        stamps = os.path.join(self.ctx.root, "e2e", ".stack-stamps")
        try:
            shutil.rmtree(stamps)
        except FileNotFoundError:
            return
        except OSError:
            c = self.ctx
            c.sh([c.docker, "run", "--rm", "--label", f"{LABEL}={c.run_id}", "--user", "0", "-v",
                  f"{os.path.join(c.root, 'e2e')}:/e2e", "--entrypoint", "rm", PW_IMAGE, "-rf", "/e2e/.stack-stamps"],
                 DOCKER_S)

    def ready_problems(self):
        c, ports, problems = self.ctx, self.ctx.ports, []
        for name, url in (("UI", f"http://localhost:{ports['ui']}/"),
                          ("meeting-api", f"http://localhost:{ports['api']}/version")):
            status, _ = c.http(url)
            if status != 200:
                problems.append(f"{name} answered {status}")
        doc = c.prom_json("/api/v1/targets")
        problems += targets_problems(doc)
        return problems, doc

    def _wait_ready(self, budget=READY_S, step=10.0):
        deadline = time.monotonic() + budget
        while True:
            problems, doc = self.ready_problems()
            if not problems:
                self.ctx.dump(os.path.join("stack", "targets-ready.json"), slim_targets(doc))
                return
            if time.monotonic() >= deadline:
                raise AdapterError(f"stack not ready after {budget} s: {'; '.join(problems)}")
            time.sleep(step)

    def _quiet_gate(self, max_s=GATE_MAX_S, poll_s=GATE_POLL_S, sample_s=3.0):
        start, log = time.monotonic(), []
        while True:
            before = read_procs()
            time.sleep(sample_s)
            after = read_procs()
            with open("/proc/loadavg", encoding="utf-8") as fh:
                load1 = float(fh.read().split()[0])
            ok, detail = gate_reading(before, after, sample_s, load1, own_tree())
            log.append({"wall": time.time(), "ok": ok, "detail": detail})
            if ok or time.monotonic() - start >= max_s:
                self.ctx.dump(os.path.join("stack", "quiet-gate.json"),
                              {"passed": ok, "readings": log,
                               "note": None if ok else f"host still busy after {max_s} s; the run continued"})
                return
            time.sleep(poll_s)

    def state(self):
        c = self.ctx
        p = c.sh([c.docker, "ps", "-aq", "--filter", f"label=com.docker.compose.project={c.project}"], DOCKER_S)
        ids = p.stdout.split()
        if not ids:
            return {}
        q = c.sh([c.docker, "inspect", "--format",
                  '{{json .Config.Labels}}\t{{.Id}}\t{{.Image}}\t{{.State.StartedAt}}\t{{.RestartCount}}', *ids],
                 DOCKER_S)
        out = {}
        for line in q.stdout.splitlines():
            labels, cid, image, started, restarts = line.split("\t")
            svc = json.loads(labels).get("com.docker.compose.service", cid[:12])
            out[svc] = {"id": cid[:12], "image": image, "started_at": started, "restart_count": int(restarts)}
        return out

    def images(self):
        c = self.ctx
        st = self.state()
        out = {k: (st.get(svc) or {}).get("image") for k, svc in IMAGE_SERVICES.items()}
        missing = [k for k, v in out.items() if not v]
        if missing:
            raise AdapterError(f"no container for {', '.join(missing)}")
        pw = c.sh([c.docker, "image", "inspect", "--format", "{{.Id}}", PW_IMAGE], DOCKER_S).stdout.strip()
        bot = c.sh([c.docker, "image", "inspect", "--format", "{{.Id}}", c.bot_image], DOCKER_S).stdout.strip()
        out.update(bots_app=f"{pw} + e2e@{c.commit}", rust_bot=f"{bot} ({c.bot_image})")
        return out

    def prom_url(self):
        return f"http://localhost:{self.ctx.ports['prom']}"

    def scrape_up_selector(self):
        return SCRAPE_UP_SELECTOR

    def throttle_pod_selector(self):
        return None

    def service_env(self):
        """{service: {name: value}} of STACK_ENV as the stack's containers carry it."""
        c = self.ctx
        ids = c.sh([c.docker, "ps", "-aq", "--filter", f"label=com.docker.compose.project={c.project}"],
                   DOCKER_S).stdout.split()
        if not ids:
            return {}
        q = c.sh([c.docker, "inspect", "--format",
                  '{{index .Config.Labels "com.docker.compose.service"}}\t{{json .Config.Env}}', *ids], DOCKER_S)
        out = {}
        for line in q.stdout.splitlines():
            svc, _, env = line.partition("\t")
            pairs = dict(e.split("=", 1) for e in json.loads(env or "[]") if "=" in e)
            out[svc] = {k: pairs[k] for k in STACK_ENV if k in pairs}
        return out

    def save(self):
        """stack/ps, targets.json, compose.json and every service log since up(), for the run folder."""
        c = self.ctx
        p = c.sh(c.compose("ps", "-a", "--format", "{{.Service}}\t{{.Name}}\t{{.State}}\t{{.Status}}"), DOCKER_S,
                 env=c.compose_env())
        with open(c.path("stack", "ps"), "w", encoding="utf-8") as fh:
            fh.write(c.scrub(p.stdout))
        c.dump(os.path.join("stack", "targets.json"), slim_targets(c.prom_json("/api/v1/targets")))
        env = self.service_env()
        release = {e["VIDEOCALL_RELEASE_BUILD"] for e in env.values() if "VIDEOCALL_RELEASE_BUILD" in e}
        info = c.sh([c.docker, "info", "--format", "{{.OperatingSystem}}"], DOCKER_S, check=False)
        c.dump(os.path.join("stack", "compose.json"), {
            "ports": c.mode, "project": c.project, "stack": c.stack_mode,
            "release_build": {frozenset({"1"}): True, frozenset({"0"}): False}.get(frozenset(release)),
            "ui_env": env.get("dioxus-ui"), "ui_serve_mode": serve_mode(env.get("dioxus-ui")),
            "docker_os": info.stdout.strip() if info.returncode == 0 else None})
        since = [] if self.since is None else ["--since", time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(self.since))]
        for svc in sorted(self.state()):
            q = c.sh(c.compose("logs", "--no-color", "--timestamps", *since, svc), LOGS_S, check=False,
                     env=c.compose_env())
            c.write_log(os.path.join("stack", "logs", f"{svc}.log.gz"), q.stdout + q.stderr)

    def down(self):
        """Removes only a stack this run started, and its anonymous volumes; named volumes stay."""
        c = self.ctx
        if not self.owned:
            return
        try:
            c.sh(c.compose("rm", "-s", "-f", "-v"), DOWN_S, env=c.compose_env())
        finally:
            c.sh(c.compose("down"), DOWN_S, env=c.compose_env())


class LocalFleet:
    """Rust bot containers and bots-app probe containers, one per planned process, all labelled with the run id."""
    PROC_GREP = "."
    CTL_S = DOCKER_S

    def __init__(self, ctx, stack=None):
        self.ctx, self.stack = ctx, stack
        self.procs = {p["proc_id"]: p for p in ctx.plan["processes"]}
        self.parts = {p["user_id"]: p for p in ctx.plan["participants"]}
        self.launched, self.reported, self.stopping, self.joins, self.bot_ids = [], set(), set(), {}, {}
        self.sampler = None
        self.run_end = None

    def clock_facts(self):
        p = self.ctx.sh(["timedatectl", "show", "-p", "NTPSynchronized", "--value"], DOCKER_S, check=False) \
            if shutil.which("timedatectl") else None
        sync = "ntp" if p is not None and p.returncode == 0 and p.stdout.strip() == "yes" else "unknown"
        return {"sync": sync, "per_node": [{"node": self.ctx.node, "skew_ms": 0}]}

    def launch(self, proc):
        if self.sampler is None:
            self._start_sampler()
        self.launched.append(proc["proc_id"])
        if proc["fleet"] == "browser":
            self._launch_probe(proc)
        else:
            self._launch_rust(proc)

    def _ttl_s(self):
        step = self.ctx.plan["steps"][0]
        return int(step["join_window_s"] + step["hold_s"] + TTL_MARGIN_S)

    def _deadline(self):
        """The run's one deadline in whole epoch seconds: one TTL from the first launch."""
        if self.run_end is None:
            self.run_end = int(time.time()) + self._ttl_s()
        return self.run_end

    def _start_sampler(self):
        c = self.ctx
        csv = c.path("host", "host-raw.csv")
        os.makedirs(os.path.dirname(csv), exist_ok=True)
        with open(c.path("host", "sampler.log"), "w", encoding="utf-8") as log:
            self.sampler = subprocess.Popen(  # noqa: SIM115 - stopped in collect()
                ["bash", SAMPLER, "--out", csv, "--interval", str(SAMPLER_INTERVAL_S),
                 "--proc-grep", self.PROC_GREP, "--cmdline-grep", PROBE_NODE_CMDLINE, "--cmdline-name", PROBE_NODE_COMM,
                 "--label", "host", "--max-seconds", str(self._ttl_s())],
                stdout=log, stderr=subprocess.STDOUT, start_new_session=True, cwd=c.root)
        deadline = time.monotonic() + 5
        while sampler_writers(csv) != 1 and time.monotonic() < deadline and self.sampler.poll() is None:
            time.sleep(0.2)
        n = sampler_writers(csv)
        if n != 1:
            raise AdapterError(f"expected exactly one host resource sampler writing host/host-raw.csv, found {n}")

    def _base_run(self, proc, stop_s):
        c = self.ctx
        return [c.docker, "run", "-d", "--init", *c.run_flags(), "--stop-timeout", str(stop_s),
                "--name", c.container(proc["proc_id"]),
                "--label", f"{LABEL}={c.run_id}", "--label", f"{PROC_LABEL}={proc['proc_id']}", "--network", "host",
                "--user", f"{c.uid}:{c.gid}", "-e", "JWT_SECRET"]

    def _in_meeting(self, pid, timeout=None):
        status, body = self._ctl(pid, "GET", "/healthz", timeout=timeout)
        try:
            return status == 200 and json.loads(body).get("inMeeting", 0) >= 1
        except (ValueError, AttributeError):
            return False

    def _await_owner(self, pid):
        """The first probe owns the meeting and turns its waiting room off; a probe that joins earlier is parked."""
        owner = next(p for p in self.launched if self.procs[p]["fleet"] == "browser")
        if owner == pid:
            return
        deadline = time.monotonic() + OWNER_WAIT_S
        while not self._in_meeting(owner):
            if time.monotonic() >= deadline:
                raise AdapterError(f"{pid}: {owner} owns the meeting and was not in it after {OWNER_WAIT_S} s, so "
                                   "the waiting room may still be on")
            time.sleep(OWNER_POLL_S)

    def _launch_probe(self, proc):
        c = self.ctx
        self._await_owner(proc["proc_id"])
        part = self.parts[proc["participants"][0]]
        out = c.path("probes", str(proc["probe_index"]))
        os.makedirs(out, exist_ok=True)
        token = secrets.token_hex(16)
        c.tokens[proc["proc_id"]] = token
        port = CTL_PORT_BASE + proc["probe_index"]
        argv = self._base_run(proc, PROBE_STOP_S) + [
            "--ipc", "host", "-e", "BOT_CTL_TOKEN", "-e", "BOT_CTL_STATE_DIR=/tmp/ctl", "-e", "HOME=/tmp/h",
            "-e", "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1", "-e", f"BOT_NETEM_APPLIED={c.netem_applied or 'unknown'}",
            "-v", f"{os.path.join(c.root, 'e2e')}:/app/e2e", "-v", f"{out}:/out", "-w", "/app/e2e", PW_IMAGE,
            TSX, CLI, "run", "--meeting-url", f"{c.ui_url}/meeting/{c.plan['meeting_id']}",
            "--participant", part["user_id"].split("@")[0], "--display-name", part["name"], "--video-mode", "clock",
            "--headless", "--ttl", f"{-(-self._ttl_s() // 60)}m", "--deadline", str(self._deadline()), "--manifest", "",
            "--assets-dir", "/out",
            "--ctl-port", str(port), "--role", part["role"], "--decode-budget", proc.get("decode_budget", "off")]
        argv += probe_flags(proc)
        argv += [] if part["publishes"]["camera"] else ["--join-camera-off"]
        argv += [] if part["publishes"]["mic"] else ["--join-mic-muted"]
        c.sh(argv, DOCKER_S, env=c.env(BOT_CTL_TOKEN=token))

    def _rust_inputs(self):
        c = self.ctx
        conv = c.path("in", "conversation")
        if os.path.exists(os.path.join(conv, "manifest.yaml")):
            return
        roster, _, _ = rust_layout(c.plan)
        talkers = [p["name"] for p in roster if p["talker"]]
        os.makedirs(os.path.join(conv, "lines"), exist_ok=True)
        lines = []
        for name in talkers:
            dst = os.path.join(conv, "lines", f"{name}.wav")
            shutil.copyfile(os.path.join(c.root, TALKER_WAV), dst)
            lines.append({"speaker": name, "audio_file": f"lines/{name}.wav", "duration_ms": wav_ms(dst)})
        manifest = {"participants": [{"name": p["name"], "voice": "local"} for p in roster], "pause_ms": 500,
                    "lines": lines}
        with open(os.path.join(conv, "manifest.yaml"), "w", encoding="utf-8") as fh:
            json.dump(manifest, fh, indent=1)

    def _rust_cfg(self, proc):
        c = self.ctx
        cfg = {"ws_url": c.ws_url, "meeting_id": c.plan["meeting_id"],
               "conversation_dir": "/in/conversation", "video_mode": c.plan.get("video_mode", RUST_VIDEO_MODE), "warmup_secs": 15,
               "ramp_up_delay_ms": 500}
        if proc["video_layers"] is not None:
            cfg["simulcast_layers"] = proc["video_layers"]
        rec = proc.get("receive") or {}
        if rec.get("pin_video_layer") is not None:
            cfg["pin_layer"] = rec["pin_video_layer"]
        if rec.get("viewport_visible_count") is not None:
            cfg["viewport_visible_count"] = rec["viewport_visible_count"]
        return cfg

    def _launch_rust(self, proc):
        c = self.ctx
        self._rust_inputs()
        os.makedirs(c.path("rust"), exist_ok=True)
        cfg = self._rust_cfg(proc)
        with open(c.path("in", f"{proc['proc_id']}.yaml"), "w", encoding="utf-8") as fh:
            json.dump(cfg, fh, indent=1)
        argv = self._base_run(proc, RUST_STOP_S) + [
            "-e", f"RUST_LOG={RUST_LOG}", "-w", "/rust", "-v", f"{c.path('in')}:/in:ro", "-v", f"{c.path('rust')}:/rust",
            c.bot_image, "--config", f"/in/{proc['proc_id']}.yaml", "--id-prefix", c.run_id,
            "--participants-out", f"/rust/participants-{proc['proc_id']}.json", "--no-impair",
            "--duration", f"{self._ttl_s()}s", "--deadline", str(self._deadline()), *rust_flags(c.plan),
            *rust_layout(c.plan)[1][proc["proc_id"]]]
        c.sh(argv, DOCKER_S)

    def _read(self, path):
        try:
            with open(path, encoding="utf-8") as fh:
                return json.load(fh)
        except (OSError, ValueError):
            return None

    def _ctl(self, proc, method, path, body=None, timeout=None):
        c = self.ctx
        port = CTL_PORT_BASE + self.procs[proc]["probe_index"]
        p = c.sh([c.docker, "exec", c.container(proc), "node", "-e", CTL_JS, str(port), method, path,
                  json.dumps(body) if body is not None else ""], timeout or self.CTL_S, check=False)
        try:
            out = json.loads(p.stdout.strip().splitlines()[-1])
            return out["status"], out["body"]
        except (ValueError, IndexError, KeyError, TypeError):
            return None, ""

    def _prom_reports(self, uid):
        q = f'count(videocall_peer_can_listen{{meeting_id="{self.ctx.plan["meeting_id"]}",from_peer="{uid}"}})'
        doc = self.ctx.prom_json("/api/v1/query", query=q)
        res = ((doc or {}).get("data") or {}).get("result") or []
        return bool(res) and float(res[0]["value"][1]) > 0

    def joined(self):
        c = self.ctx
        for pid in self.launched:
            proc = self.procs[pid]
            if proc["fleet"] == "rust":
                f = self._read(c.path("rust", f"participants-{pid}.json")) or {}
                for p in f.get("participants") or []:
                    if isinstance(p, dict) and p.get("join_ts") is not None:
                        self.joins[p.get("user_id")] = Joined(p["join_ts"], f.get("media_started_at"))
                continue
            uid = proc["participants"][0]
            if uid in self.joins:
                continue
            folder = c.path("probes", str(proc["probe_index"]), "participants")
            recs = [self._read(os.path.join(folder, n)) for n in sorted(os.listdir(folder))
                    if n.endswith(".json")] if os.path.isdir(folder) else []
            rec = next((r for r in recs if isinstance(r, dict)
                        and (r.get("participant") or {}).get("user_id") == uid), None)
            if rec is None or rec["participant"].get("join_ts") is None or rec.get("outcome") is not None:
                continue
            if self._in_meeting(pid) and self._prom_reports(uid):
                self.joins[uid] = Joined(rec["participant"]["join_ts"])
        return {u: j for u, j in self.joins.items() if u in self.parts}

    def _bot_id(self, pid):
        if pid not in self.bot_ids:
            status, body = self._ctl(pid, "GET", "/bots")
            try:
                bots = json.loads(body)["bots"] if status == 200 else []
            except (ValueError, KeyError, TypeError):
                bots = []
            if len(bots) != 1 or not isinstance(bots[0], dict) or not bots[0].get("botId"):
                raise AdapterError(f"{pid}: the control API lists {len(bots)} bots, expected 1")
            self.bot_ids[pid] = bots[0]["botId"]
        return self.bot_ids[pid]

    def apply(self, event):
        c, action = self.ctx, event["action"]
        for uid in event["participants"]:
            pid = self.parts[uid]["process"]
            if self.parts[uid]["fleet"] == "rust":
                if action != "leave" or set(self.procs[pid]["participants"]) - set(event["participants"]):
                    raise AdapterError(f"{action} of {uid}: only a leave of a whole Rust process can be applied")
                c.sh([c.docker, "kill", "--signal", "SIGTERM", c.container(pid)], DOCKER_S)
                continue
            bot = urllib.parse.quote(self._bot_id(pid), safe="")
            path, body, want = {"leave": (f"/bots/{bot}/leave", None, (202,)),
                                "mute": (f"/bots/{bot}/mute", {"mic": True}, (200,)),
                                "unmute": (f"/bots/{bot}/mute", {"mic": False}, (200,))}[action]
            status, _ = self._ctl(pid, "POST", path, body)
            if status not in want:
                return EventOutcome(None, f"{action} of {uid}: the control API answered {status}")
        return EventOutcome(time.time(), "ok")

    def _exit(self, pid, state):
        return Exit(pid, docker_time(state.get("FinishedAt")), state.get("ExitCode"))

    def poll(self):
        c, alive = self.ctx, [p for p in self.launched if p not in self.reported]
        for attempt in range(POLL_TRIES):
            try:
                states = c.inspect([c.container(p) for p in alive])
                break
            except AdapterError:
                if attempt + 1 == POLL_TRIES:
                    raise
                time.sleep(1)
        out = []
        for pid in alive:
            st = states.get(c.container(pid))
            if st is None or not st.get("Running", True):
                self.reported.add(pid)
                out.append(Exit(pid, None, None) if st is None else self._exit(pid, st))
        return out

    def stop(self, proc):
        c, pid = self.ctx, proc["proc_id"]
        if pid not in self.stopping:
            group = [p for p in self.launched if self.procs[p]["fleet"] == proc["fleet"]
                     and p not in self.reported and p not in self.stopping]
            self.stopping |= set(group)
            stop_s = RUST_STOP_S if proc["fleet"] == "rust" else PROBE_STOP_S
            c.sh([c.docker, "stop", "-t", str(stop_s), *[c.container(p) for p in group]], stop_s + DOCKER_S,
                 check=False)
        st = c.inspect([c.container(pid)]).get(c.container(pid))
        if st is None:
            self.reported.add(pid)
            return Exit(pid, None, None)
        if st.get("Running"):
            raise AdapterError(f"{pid} is still running after docker stop")
        self.reported.add(pid)
        return self._exit(pid, st)

    def _stop_sampler(self):
        if self.sampler is None:
            return
        if self.sampler.poll() is None:
            os.killpg(self.sampler.pid, signal.SIGTERM)
            try:
                self.sampler.wait(SAMPLER_STOP_S)
            except subprocess.TimeoutExpired:
                os.killpg(self.sampler.pid, signal.SIGKILL)
                self.sampler.wait(5)
        for rel in (os.path.join("host", "host-raw.csv"), os.path.join("host", "sampler.log")):
            self.ctx.scrub_file(rel)

    def collect(self, run_dir):
        c, problems = self.ctx, []
        steps = [("sampler", self._stop_sampler), ("containers", self._save_containers)]
        if self.stack is not None:
            steps.append(("stack", self.stack.save))
        if any(p["fleet"] == "browser" for p in self.procs.values()):
            steps.append(("probe network", self._recheck_link))
        for what, fn in steps:
            try:
                fn()
            except Exception as exc:  # noqa: BLE001 - every step runs; an interrupt still escapes
                problems.append(f"{what}: {describe(exc)}")
        if problems:
            raise AdapterError(c.scrub("collect: " + "; ".join(problems)))

    def _save_containers(self):
        c = self.ctx
        p = c.sh([c.docker, "ps", "-aq", "--filter", f"label={LABEL}={c.run_id}"], DOCKER_S)
        ids = p.stdout.split()
        try:
            self._save_container_files(ids)
        finally:
            if ids:
                c.sh([c.docker, "rm", "-f", *ids], DOCKER_S)

    def _save_container_files(self, ids):
        c, names, problems = self.ctx, {}, []
        if ids:
            q = c.sh([c.docker, "inspect", "--format", '{{.Name}}\t{{index .Config.Labels "cq.proc_id"}}\t{{.Image}}',
                      *ids], DOCKER_S)
            names = {n.lstrip("/"): (proc, image) for n, proc, image in
                     (line.split("\t") for line in q.stdout.splitlines() if line.count("\t") == 2)}
        states = c.inspect(list(names))
        c.dump(os.path.join("fleet", "containers.json"),
               {proc: dict({k: (states.get(n) or {}).get(k) for k in
                            ("Status", "ExitCode", "OOMKilled", "StartedAt", "FinishedAt", "Error")}, Image=image)
                for n, (proc, image) in sorted(names.items(), key=lambda x: x[1])})
        for n, (proc, _) in names.items():
            try:
                q = c.sh([c.docker, "logs", "--timestamps", n], LOGS_S, check=False)
                c.write_log(os.path.join("logs", f"{proc}.log.gz"), q.stdout + q.stderr)
            except Exception as exc:  # noqa: BLE001 - the other logs are still saved
                problems.append(f"{proc} log: {describe(exc)}")
        if problems:
            raise AdapterError("; ".join(problems))

    def _recheck_link(self):
        c = self.ctx
        qdiscs = c.read_qdiscs()
        c.dump(os.path.join("probes", "netns-qdisc-end.json"), qdiscs)
        link = qdisc_problems(qdiscs)
        if link:
            raise AdapterError(f"the probes' link changed during the run: {'; '.join(link)}")

    def generator_hosts(self):
        return [self.ctx.node]

    def generator_verdicts(self, hold_start, hold_end):
        c = self.ctx
        out = os.path.join("host", "resource-verdict.json")
        os.makedirs(c.path("host"), exist_ok=True)
        p = c.sh([c.docker, "run", "--rm", "--init", *c.run_flags(), "--label", f"{LABEL}={c.run_id}", "--user",
                  f"{c.uid}:{c.gid}", "-e", "HOME=/tmp/h", "-v", c.mount(os.path.join(c.root, "e2e"), "/app/e2e"),
                  "-v", c.mount(c.path("host"), "/host"), "-w", "/app/e2e", PW_IMAGE, TSX, CLI, "resource-verdict",
                  "--csv", "/host/host-raw.csv", "--from", repr(float(hold_start)), "--to", repr(float(hold_end)),
                  "--out", "/host/resource-verdict.json"], DOCKER_S * 2, check=False)
        if p.returncode not in (0, 1):
            raise AdapterError(f"bots-app resource-verdict exited {p.returncode}: {c.scrub(p.stderr.strip())[-300:]}")
        verdict = self._read(c.path(out))
        if not isinstance(verdict, dict):
            raise AdapterError("bots-app resource-verdict wrote no verdict")
        return [dict(verdict, host=c.node)]


def factory(plan, run_dir, stack_mode="up", environ=None):
    ctx = Local(plan, run_dir, Settings.from_env(os.environ if environ is None else environ), stack_mode=stack_mode)
    stack = LocalStack(ctx)
    return stack, LocalFleet(ctx, stack)
