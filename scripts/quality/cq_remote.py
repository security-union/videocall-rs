"""Remote adapters for the scenario runner (#2914 PR-8b): a shared stack used read only, and Podman bots on vc2."""

import base64
import binascii
import errno
import glob
import gzip
import json
import math
import os
import re
import secrets
import shutil
import signal
import socket
import stat
import subprocess
import sys
import time
import urllib.parse
import zlib
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cq_local  # noqa: E402
import cq_runner  # noqa: E402
import cq_score  # noqa: E402
from cq_runner import AdapterError  # noqa: E402

PROFILE_DIR = os.path.join(HERE, "targets")
PROFILE_SCHEMA = "cq-target/v1"
PROFILE_KEYS = {"schema": str, "target": str, "ui_url": str, "api_url": str, "ws_url": str, "prom_url": str,
                "cookie_name": str, "kube_server": str, "kube_context": (str, type(None)), "kubeconfig": str,
                "namespace": str, "pods": dict, "dev_secret": str, "probe_cores": dict}
PROFILE_OPTIONAL = {"window_utc": list, "_comment": str}
PROBE_KINDS = ("camera_off", "camera_off_mic_on", "camera_on")
POD_KINDS = ("relay_ws", "relay_wt", "ui", "nats", "metrics_api")
SCRAPED_KINDS = ("relay_ws", "relay_wt", "metrics_api")
THROTTLE_KINDS = ("relay_ws", "relay_wt", "nats")
HTTPS_RE = re.compile(r"^https://[a-z0-9.-]+(?::\d{1,5})?$")
WSS_RE = re.compile(r"^wss://[a-z0-9.-]+(?::\d{1,5})?$")
POD_RE_RE = re.compile(r"^[A-Za-z0-9_.*+?|()\[\]^$-]+$")
HHMM_RE = re.compile(r"^([01]\d|2[0-3]):([0-5]\d)$")
ROOM_RE = re.compile(r"^scale-[a-z0-9-]+$")
CONFIRM_ENV = "CQ_TARGET_CONFIRM"
KUBECONFIG_ENV = "CQ_KUBECONFIG"
CHILD_ENV_DROP = (cq_runner.SECRET_ENV, "KUBECONFIG")
MIN_SECRET_LEN = 8
NODE = "load-a"
KUBECTL_CALLS = (("get", "pods", "-o", "json"), ("config", "view", "--minify", "-o", "json"))
KUBECTL_S = 20
KUBECTL_REQUEST = "10s"
DEFAULT_KUBECONFIG = "~/.kube/config"
HTTP_S = 10
PROM_HEAVY_TIMEOUT = "9s"
WATCH_DEADLINE_S = 1.5
WATCH_KILL_S = 0.3
PROM_TIMEOUT = "1s"
BLIND_S = 2 * cq_runner.WATCH_INTERVAL_S
DRAIN_S = 90
DRAIN_POLL_S = 5
DRAIN_QUERY_S = 5
STOP_EXTRA_S = 15
KILL_S = 10
TEARDOWN_SLACK_S = 10
WATCH_GAP_S = 60
LOAD_LINE_PCT = 70.0
LOAD_HARD_PCT = 85.0
LOAD_SUSTAIN_S = 60.0
LOAD_EVERY_S = 5.0
LOAD_SAMPLE_S = 5.0
ADMIT_SAMPLE_S = 5.0
ADMIT_SETTLE_S = 20.0
ADMIT_SETTLE_CAP_S = 40.0
ADMIT_OVERHEAD_CORES = 0.3
FRESH_POD_S = 600
MAX_SKEW_MS = 1000.0
METRICS_PORT_BASE = 19100
FLEET_INSPECT_S = 3
CTL_S = 5
JOIN_PROM_EVERY_S = 5.0
JOIN_PROM_S = 2
PROM_SCRAPE_S = 15
RELAY_DOWN_S = 2 * PROM_SCRAPE_S + WATCH_DEADLINE_S
WATCH_S = WATCH_DEADLINE_S + WATCH_KILL_S
OWNER_SAMPLES = 2
LATER_SAMPLES = int((ADMIT_SETTLE_CAP_S - ADMIT_SETTLE_S) // ADMIT_SAMPLE_S)
SAMPLE_S = ADMIT_SAMPLE_S + WATCH_S
PROBE_READY_S = 240
MAX_STAGGER_S = PROBE_READY_S - cq_local.DOCKER_S - FLEET_INSPECT_S - WATCH_S
SEEN_S = cq_local.OWNER_POLL_S + WATCH_S + 2 * CTL_S
JOIN_LOOP_S = cq_runner.JOIN_POLL_S + FLEET_INSPECT_S + WATCH_S + CTL_S + JOIN_PROM_S
CONFIRM_S = max(PROM_SCRAPE_S, JOIN_PROM_EVERY_S) + 2 * JOIN_LOOP_S + cq_local.DOCKER_S
MIN_LAUNCH_S = 60
SECRET_PARENT = "/run/user/{uid}"
SECRET_PREFIX = "cq-"
PRELOAD = os.path.join("scripts", "quality", "probe-preload.mjs")
KEY_READ = ".cq-key-read"
BROWSER_ENV = os.path.join("e2e", "bots-app", "src", "browser-env.ts")
STRIP_RE = re.compile(r"BROWSER_ENV_STRIP_RE\s*=\s*/.*[(|]JWT_SECRET[)|]")
PROBE_TRANSPORTS = {"websocket": "ws", "webtransport": "wt"}
PROC_GREP = "bot|node|tsx|chrom|headless|conmon|podman|pasta|slirp"
ENV_CHECK_SH = ('for e in /proc/[0-9]*/environ; do tr "\\0" "\\n" <"$e" 2>/dev/null | grep -q "^JWT_SECRET=" '
                '&& echo "${e%/environ}"; done; echo done')
JWT_BYTES_RE = re.compile(rb"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}")
JWT_MARK = b"<jwt>"
GZIP_MAGIC = b"\x1f\x8b"
PACKED_MAGIC = (b"BZh", b"PK\x03\x04", b"PK\x05\x06", b"\xfd7zXZ\x00", b"\x28\xb5\x2f\xfd", b"7z\xbc\xaf\x27\x1c",
                b"Rar!\x1a\x07", b"\x04\x22\x4d\x18")
PR_GET_DUMPABLE = 3
PR_SET_DUMPABLE = 4
LEAK_SUMMARY = "leakcheck.json"


def load_profile(target, path=None):
    """The committed target profile, the only source of the target's URLs."""
    path = path or os.path.join(PROFILE_DIR, f"{target}.json")
    try:
        with open(path, encoding="utf-8") as fh:
            doc = json.load(fh)
    except (OSError, ValueError) as exc:
        raise AdapterError(f"target profile {target}: {cq_runner._describe(exc)}") from None
    if not isinstance(doc, dict) or not set(PROFILE_KEYS) <= set(doc) <= set(PROFILE_KEYS) | set(PROFILE_OPTIONAL):
        raise AdapterError(f"target profile {target}: keys must be exactly {', '.join(sorted(PROFILE_KEYS))}, "
                           f"and optionally {', '.join(sorted(PROFILE_OPTIONAL))}")
    problems = [k for k, t in {**PROFILE_KEYS, **PROFILE_OPTIONAL}.items() if k in doc and not isinstance(doc[k], t)]
    problems += [k for k in ("ui_url", "api_url", "prom_url", "kube_server") if not HTTPS_RE.match(str(doc[k]))]
    problems += [] if WSS_RE.match(str(doc["ws_url"])) else ["ws_url"]
    problems += [] if doc["schema"] == PROFILE_SCHEMA and doc["target"] == target else ["schema/target"]
    problems += [] if re.match(r"^[a-z0-9-]+$", str(doc["namespace"])) else ["namespace"]
    problems += [] if re.match(r"^[A-Za-z0-9_-]+$", str(doc["cookie_name"])) else ["cookie_name"]
    problems += [] if doc["kube_context"] is None or re.match(r"^[A-Za-z0-9_.@:/-]+$", str(doc["kube_context"])) \
        else ["kube_context"]
    w = doc.get("window_utc")
    if "window_utc" in doc and not (isinstance(w, list) and len(w) == 2
                                    and all(isinstance(x, str) and HHMM_RE.match(x) for x in w)
                                    and _day_s(w[0]) < _day_s(w[1])):
        problems.append("window_utc")
    pods = doc["pods"] if isinstance(doc["pods"], dict) else {}
    if set(pods) != set(POD_KINDS) or not all(isinstance(v, str) and POD_RE_RE.match(v) for v in pods.values()):
        problems.append("pods")
    cores = doc["probe_cores"] if isinstance(doc["probe_cores"], dict) else {}
    if set(cores) != set(PROBE_KINDS) or not all(v is None or cq_runner._num(v) and v > 0 for v in cores.values()):
        problems.append("probe_cores")
    if problems:
        raise AdapterError(f"target profile {target}: invalid {', '.join(problems)}")
    return doc


def _day_s(hhmm):
    m = HHMM_RE.match(hhmm)
    return int(m.group(1)) * 3600 + int(m.group(2)) * 60


def _clock(day_s):
    return f"{int(day_s) // 3600 % 24:02d}:{int(day_s) // 60 % 60:02d}Z"


def planned_s(plan):
    """Upper bound of join_start to the end of the settle-out, on the runner's own budgets."""
    step = plan["steps"][0]
    deadline = cq_score.cv(cq_score.load_config(), "quality_gates", "join_deadline_s")
    settle = max(deadline + cq_runner.JOIN_SETTLE_S, cq_runner.MEDIA_SETTLE_S)
    return (step["join_window_s"] + cq_runner.JOIN_BUDGET_EXTRA_S + settle + step["hold_s"] + cq_runner.EVENT_SLACK_S
            + cq_runner.TEARDOWN_DELAY_S)


def run_need_s(plan):
    """Join start to the end of teardown, with the watch's gap: what a window must hold, and the bots' TTL cap."""
    return planned_s(plan) + teardown_s(plan) + WATCH_GAP_S


def teardown_s(plan):
    """Upper bound of the runner's teardown up to drained(), with every call at its own timeout."""
    groups = {}
    for p in plan["processes"]:
        groups.setdefault(p["fleet"], []).append(p)
    stops = sum((cq_local.RUST_STOP_S if fleet == "rust" else cq_local.PROBE_STOP_S) + STOP_EXTRA_S + FLEET_INSPECT_S
                + len(procs) * (KILL_S + FLEET_INSPECT_S) for fleet, procs in groups.items())
    return KUBECTL_S + stops + DRAIN_S + DRAIN_QUERY_S + WATCH_KILL_S + TEARDOWN_SLACK_S


class Window:
    """The approved UTC window, any day: a run must end, teardown included, before its end."""

    def __init__(self, start, end, teardown):
        self.start, self.end, self.teardown = _day_s(start), _day_s(end), teardown
        self.stop = self.end - teardown - WATCH_GAP_S

    def problems(self, now, run_s):
        day = now % 86400
        need = run_s + self.teardown + WATCH_GAP_S
        if not self.start <= day < self.end:
            return [f"now ({_clock(day)}) is outside the approved window {_clock(self.start)}-{_clock(self.end)}"]
        if day + need > self.end + 1e-6:
            return [f"the run needs {need:.0f} s with teardown, so it would end after {_clock(self.end)}"]
        return []

    def reasons(self, now):
        day = now % 86400
        if not self.start <= day < self.stop:
            return [f"the approved window closes at {_clock(self.end)} and teardown needs {self.teardown:.0f} s"]
        return []

    def end_epoch(self, now):
        return now - now % 86400 + self.end


def read_cpu(path="/proc/stat"):
    """(total, idle) jiffies of the host's first /proc/stat line; idle counts iowait."""
    try:
        with open(path, encoding="utf-8") as fh:
            vals = [int(x) for x in fh.readline().split()[1:9]]
    except (OSError, ValueError):
        vals = []
    if len(vals) < 5:
        raise AdapterError("cannot read the load host's CPU counters")
    return sum(vals), vals[3] + vals[4]


def busy_pct(a, b):
    dt = b[1] - a[1]
    return None if dt <= 0 else 100.0 * (1 - (b[2] - a[2]) / dt)


class LoadLine:
    """Load-host CPU sampled every LOAD_EVERY_S: a hard stop above 85% since the previous sample, and the 70% line on
    the mean over the last 60 s."""

    def __init__(self, read):
        self.read, self.samples = read, []

    def sample(self, mono):
        if self.samples and mono - self.samples[-1][0] < LOAD_EVERY_S:
            return []
        total, idle = self.read()
        now, out = (mono, total, idle), []
        if self.samples:
            b = busy_pct(self.samples[-1], now)
            if b is not None and b > LOAD_HARD_PCT:
                out.append(f"load host CPU {b:.0f}% over the last {mono - self.samples[-1][0]:.0f} s, above the "
                           f"{LOAD_HARD_PCT:.0f}% hard stop")
        self.samples.append(now)
        old = [x for x in self.samples if x[0] <= mono - LOAD_SUSTAIN_S]
        if old:
            self.samples = self.samples[self.samples.index(old[-1]):]
        return out

    def line(self):
        if not self.samples:
            return []
        last = self.samples[-1]
        old = [x for x in self.samples if x[0] <= last[0] - LOAD_SUSTAIN_S]
        b = busy_pct(old[-1], last) if old else None
        if b is not None and b > LOAD_LINE_PCT:
            return [f"load host CPU {b:.0f}% over the last {last[0] - old[-1][0]:.0f} s, above the "
                    f"{LOAD_LINE_PCT:.0f}% line: stop and add a second load host"]
        return []

    def check(self, mono):
        return self.sample(mono) + self.line()


def fs_type(path):
    path, best, kind = os.path.realpath(path), "", None
    try:
        with open("/proc/self/mounts", encoding="utf-8") as fh:
            rows = [line.split() for line in fh]
    except OSError:
        return None
    for parts in rows:
        mnt = parts[1].replace("\\040", " ")
        if (path == mnt or path.startswith(mnt.rstrip("/") + "/")) and len(mnt) >= len(best):
            best, kind = mnt, parts[2]
    return kind


def secret_forms(secret):
    raw = secret.encode()
    quoted = [urllib.parse.quote(secret, safe=""), urllib.parse.quote_plus(secret)]
    forms = {secret, raw.hex(), raw.hex().upper(), json.dumps(secret)[1:-1],
             json.dumps(secret, ensure_ascii=False)[1:-1], *quoted,
             *(re.sub(r"%[0-9A-F]{2}", lambda m: m.group(0).lower(), q) for q in quoted)}
    out = {f.encode() for f in forms if f} | {secret.encode("utf-16-le"), secret.encode("utf-16-be")}
    for k, lead in ((0, 0), (1, 2), (2, 3)):
        end = (k + len(raw)) * 8 // 6
        for enc in (base64.b64encode, base64.urlsafe_b64encode):
            out.add(enc(b"\0" * k + raw)[lead:end])
    return sorted(f for f in out if f)


def has_key(data, forms):
    if any(f in data for f in forms):
        return True
    for token in JWT_BYTES_RE.findall(data):
        for seg in token.split(b"."):
            try:
                decoded = base64.urlsafe_b64decode(seg + b"=" * (-len(seg) % 4))
            except (ValueError, binascii.Error):
                continue
            if any(f in decoded for f in forms):
                return True
    return False


def unscannable(data):
    zlib_head = len(data) > 1 and data[0] == 0x78 and data[1] in (0x01, 0x5E, 0x9C, 0xDA)
    return data.startswith(PACKED_MAGIC) or zlib_head


def _read_fd(fd):
    chunks = []
    while True:
        chunk = os.read(fd, 1 << 20)
        if not chunk:
            return b"".join(chunks)
        chunks.append(chunk)


def leak_scan(run_dir, secret, tokens=()):
    """Problems with the run folder, fail closed. The key in any form, in a file or inside a JWT, or a control token,
    is a leak; other JWT-shaped strings (room and session tokens) are redacted in place and checked again. Files
    are opened by descriptor without following links, and a rewrite stays in the file it read."""
    if not isinstance(secret, str) or len(secret) < MIN_SECRET_LEN:
        return ["no signing key to scan for"]
    key, toks = secret_forms(secret), [t.encode() for t in tokens if t]
    keyed, tokened, unreadable, links, redacted, errors, packed_files, n = [], [], [], [], [], [], [], 0

    def walk_error(exc):
        errors.append(cq_runner._describe(exc))
    for root, dirs, files, dirfd in os.fwalk(run_dir, onerror=walk_error):
        for name in [d for d in dirs if stat.S_ISLNK(os.lstat(d, dir_fd=dirfd).st_mode)]:
            links.append(os.path.normpath(os.path.join(os.path.relpath(root, run_dir), name)))
        for name in sorted(files):
            rel = os.path.normpath(os.path.join(os.path.relpath(root, run_dir), name))
            try:
                fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=dirfd)
            except OSError as exc:
                (links if exc.errno == errno.ELOOP else unreadable).append(rel)
                continue
            try:
                st = os.fstat(fd)
                if not stat.S_ISREG(st.st_mode) or st.st_nlink != 1:
                    links.append(rel)
                    continue
                n += 1
                data = _read_fd(fd)
                packed = data[:2] == GZIP_MAGIC
                plain = gzip.decompress(data) if packed else data
                if has_key(plain, key) or (packed and has_key(data, key)):
                    keyed.append(rel)
                elif unscannable(plain) or (packed and plain[:2] == GZIP_MAGIC):
                    packed_files.append(rel)
                elif any(t in plain for t in toks):
                    tokened.append(rel)
                elif JWT_BYTES_RE.search(plain):
                    _rewrite(name, dirfd, st, JWT_BYTES_RE.sub(JWT_MARK, plain), packed)
                    (unreadable if _jwt_left(name, dirfd, packed) else redacted).append(rel)
            except (OSError, EOFError, zlib.error, gzip.BadGzipFile):
                unreadable.append(rel)
            finally:
                os.close(fd)
    problems = [f"walking the run folder failed: {e}" for e in errors]
    problems += [f"{', '.join(keyed)} carry the signing key (raw, hex, UTF-16, base64 at any alignment, JSON- or "
                 "URL-encoded, or inside a JWT): do not copy the run folder; rotate the key and tell the target's owner"] \
        if keyed else []
    problems += [f"{', '.join(tokened)} carry a control token"] if tokened else []
    problems += [f"{', '.join(packed_files)} are compressed or archived beyond one gzip layer, so they are unchecked"] \
        if packed_files else []
    problems += [f"{', '.join(unreadable)} could not be read or redacted, so they are unchecked"] if unreadable else []
    problems += [f"{', '.join(links)} are not single-link regular files, and the scan does not follow them"] \
        if links else []
    problems += [] if n else ["the run folder has no files to scan"]
    if not problems:
        fd = os.open(os.path.join(run_dir, LEAK_SUMMARY), os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW,
                     0o644)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump({"files": n, "jwt_redacted": sorted(redacted)}, fh, indent=1)
            fh.write("\n")
    return problems


def _rewrite(name, dirfd, st, plain, packed):
    fd = os.open(name, os.O_WRONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=dirfd)
    try:
        now = os.fstat(fd)
        if (now.st_dev, now.st_ino) != (st.st_dev, st.st_ino) or not stat.S_ISREG(now.st_mode) or now.st_nlink != 1:
            raise OSError(errno.ESTALE, "the file changed during the scan")
        os.ftruncate(fd, 0)
        os.write(fd, gzip.compress(plain) if packed else plain)
    finally:
        os.close(fd)


def _jwt_left(name, dirfd, packed):
    fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=dirfd)
    try:
        data = _read_fd(fd)
    finally:
        os.close(fd)
    return bool(JWT_BYTES_RE.search(gzip.decompress(data) if packed else data))


@dataclass
class Remote(cq_local.Local):
    """What the remote stack and fleet share: the profile, the signing key (never in a child's environment) and
    clocks that tests replace."""
    docker: str = "podman"
    profile: dict = None
    kubeconfig: str = None
    kubeconfig_from_env: bool = False
    kube_context: str = None
    key_scrubs: int = 0
    secret_parent: str = None
    skew_ms: float = None
    now: object = time.time
    mono: object = time.monotonic
    sleep: object = time.sleep
    cpu: object = read_cpu
    ncpu: object = os.cpu_count
    fs: object = fs_type
    port_open: object = None

    def __post_init__(self):
        if self.secret_parent is None:
            self.secret_parent = SECRET_PARENT.format(uid=self.uid)
        if self.port_open is None:
            self.port_open = _tcp_open
        self.load = LoadLine(lambda: self.cpu())
        self._forms = secret_forms(self.jwt_secret) if self.jwt_secret else []

    @property
    def node(self):
        return NODE

    @property
    def ui_url(self):
        return self.profile["ui_url"]

    @property
    def ws_url(self):
        return self.profile["ws_url"]

    @property
    def room(self):
        return self.plan["meeting_id"]

    @property
    def ns(self):
        return self.profile["namespace"]

    def env(self, **extra):
        env = {k: v for k, v in os.environ.items() if k not in CHILD_ENV_DROP and not k.upper().endswith("_PROXY")}
        env.update({k: str(v) for k, v in extra.items()})
        return env

    def run_flags(self):
        return ["--pull", "never", "--userns", "keep-id", "--log-driver", "k8s-file", "--ulimit", "core=0"]

    def mount(self, src, dst, *opts):
        return super().mount(src, dst, *opts, *(("ro",) if dst == "/app/e2e" and "ro" not in opts else ()), "z")

    def scrub(self, text):
        if self._forms and has_key(text.encode("utf-8", "surrogateescape"), self._forms):
            self.key_scrubs += 1
        return super().scrub(text)

    def curl(self, url, params=None, max_time=HTTP_S):
        """(status, body) of one HTTPS GET, or (None, b"") when it fails or overruns max_time; never raises for the
        network, only for a curl that cannot start."""
        argv = ["curl", "-q", "-sS", "--proto", "=https", "--max-time", str(max_time), "--get", "-o", "-", "-w",
                "\n%{http_code}", url]
        for k, v in (params or {}).items():
            argv[-1:-1] = ["--data-urlencode", f"{k}={v}"]
        try:
            p = subprocess.run(argv, capture_output=True, timeout=max_time + WATCH_KILL_S, check=False,
                               start_new_session=True, env=self.env())
        except subprocess.TimeoutExpired:
            return None, b""
        except OSError as exc:
            raise AdapterError(f"curl could not start: {exc.strerror}") from None
        body, _, code = p.stdout.rpartition(b"\n")
        if p.returncode != 0 or not code.strip().isdigit():
            return None, b""
        return int(code), body

    def http(self, url, timeout=HTTP_S):
        return self.curl(url, max_time=timeout)

    def prom_json(self, path, max_time=HTTP_S, **params):
        status, body = self.curl(self.profile["prom_url"] + path, params, max_time)
        if status != 200:
            return None
        try:
            doc = json.loads(body)
        except ValueError:
            return None
        return doc if isinstance(doc, dict) and doc.get("status") == "success" else None

    def prom_vector(self, query, max_time=HTTP_S, timeout=None):
        """{labels-as-json: value} of an instant vector query, or None for any failure or malformed answer."""
        doc = self.prom_json("/api/v1/query", max_time, query=query, **({"timeout": timeout} if timeout else {}))
        data = (doc or {}).get("data")
        if not isinstance(data, dict) or data.get("resultType") != "vector" or not isinstance(data.get("result"), list):
            return None
        out = {}
        for s in data["result"]:
            try:
                if not isinstance(s["metric"], dict):
                    return None
                out[json.dumps(s["metric"], sort_keys=True)] = float(s["value"][1])
            except (KeyError, IndexError, TypeError, ValueError):
                return None
        return out

    def prom_time(self):
        doc = self.prom_json("/api/v1/query", query="time()")
        data = (doc or {}).get("data")
        try:
            return float(data["result"][0]) if data["resultType"] == "scalar" else None
        except (KeyError, IndexError, TypeError, ValueError):
            return None

    def kubectl(self, *args):
        """stdout of one of the two kubectl reads this adapter makes; any other argv is refused before a process
        starts."""
        if tuple(args) not in KUBECTL_CALLS:
            raise AdapterError(f"kubectl {' '.join(args[:2])}: only {'; '.join(' '.join(c) for c in KUBECTL_CALLS)} "
                               "are allowed")
        argv = ["kubectl", *args, "--kubeconfig", os.path.expanduser(self.kubeconfig),
                f"--request-timeout={KUBECTL_REQUEST}"]
        argv += ["--context", self.kube_context] if self.kube_context else []
        argv += ["--server", self.profile["kube_server"], "-n", self.ns] if args[0] == "get" else []
        p = self.sh(argv, KUBECTL_S, check=False, env=self.env(KUBERC="off"))
        if p.returncode != 0:
            raise AdapterError(f"kubectl {' '.join(args[:2])} exited {p.returncode}")
        return p.stdout

    def kubeconfig_problems(self):
        path = os.path.expanduser(self.kubeconfig or "")
        if not self.kubeconfig_from_env and os.path.realpath(path) == os.path.realpath(
                os.path.expanduser(DEFAULT_KUBECONFIG)):
            return [f"the kubeconfig is kubectl's default {DEFAULT_KUBECONFIG}; keep the target's in its own file, or "
                    f"name it in {KUBECONFIG_ENV}"]
        try:
            st = os.lstat(path)
        except OSError:
            return [f"no kubeconfig at the path in {KUBECONFIG_ENV} or the profile"]
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISREG(st.st_mode):
            return ["the kubeconfig is not a regular file"]
        if st.st_mode & 0o077:
            return ["the kubeconfig is readable by group or others; chmod 600 it"]
        self.kube_context = self.profile["kube_context"]
        try:
            doc = json.loads(self.kubectl("config", "view", "--minify", "-o", "json"))
            cluster, user = doc["clusters"][0]["cluster"], doc["users"][0]["user"]
            current = doc.get("current-context")
        except (AdapterError, ValueError, KeyError, IndexError, TypeError) as exc:
            self.kube_context = None
            return [f"cannot read the kubeconfig's current context: {cq_runner._describe(exc)}"]
        if not isinstance(current, str) or not current or (self.kube_context and current != self.kube_context):
            self.kube_context = None
            return ["the kubeconfig has no current context, or not the profile's"]
        self.kube_context = current
        problems = [] if cluster.get("server") == self.profile["kube_server"] else [
            "the kubeconfig's current context is not the profile's API server"]
        user = user or {}
        bad = sorted(k for k in ("exec", "auth-provider") if k in user)
        bad += [k for k in ("proxy-url", "insecure-skip-tls-verify") if cluster.get(k)]
        files = [f"cluster {k}" for k in ("certificate-authority",) if cluster.get(k)]
        files += [f"user {k}" for k in ("client-certificate", "client-key", "tokenFile") if user.get(k)]
        problems += [f"the kubeconfig names its {f} as a file; embed it with kubectl config view --raw --flatten"
                     for f in files]
        problems += [] if cluster.get("certificate-authority-data") else [
            "the kubeconfig's cluster has no certificate authority embedded (certificate-authority-data)"]
        embedded = user.get("token") or user.get("client-certificate-data") and user.get("client-key-data")
        problems += [] if embedded else ["the kubeconfig's user has no embedded credential: client-certificate-data "
                                         "with client-key-data, or a token"]
        return problems + ([f"the kubeconfig uses {', '.join(bad)}; only an embedded certificate or token over "
                            "verified TLS is accepted"] if bad else [])

    def pods(self):
        """{pod: facts} for the profile's pods, with no node, address or registry host."""
        try:
            items = json.loads(self.kubectl("get", "pods", "-o", "json"))["items"]
        except (ValueError, KeyError, TypeError) as exc:
            raise AdapterError(f"kubectl get pods: unreadable answer ({type(exc).__name__})") from None
        out = {}
        for it in items if isinstance(items, list) else []:
            try:
                name, status = it["metadata"]["name"], it.get("status") or {}
                kind = next((k for k in POD_KINDS if re.fullmatch(self.profile["pods"][k], name)), None)
                if kind is None:
                    continue
                cs = status.get("containerStatuses") or []
                starts = sorted(t for t in (((c.get("state") or {}).get("running") or {}).get("startedAt")
                                            for c in cs) if t)
                out[name] = {"kind": kind, "phase": status.get("phase"),
                             "ready": bool(cs) and all(c.get("ready") is True for c in cs),
                             "restart_count": sum(int(c.get("restartCount", 0)) for c in cs),
                             "started_at": starts[-1] if starts else None,
                             "image": [_image(c.get("image"), c.get("imageID")) for c in cs]}
            except (KeyError, TypeError, ValueError) as exc:
                raise AdapterError(f"kubectl get pods: unreadable pod ({type(exc).__name__})") from None
        return out

    def secret_dir(self, pid=None):
        root = os.path.join(self.secret_parent, SECRET_PREFIX + self.run_id)
        return root if pid is None else os.path.join(root, pid)

    def write_secret(self, pid, name, text):
        d = self.secret_dir(pid)
        try:
            os.makedirs(self.secret_dir(), mode=0o700, exist_ok=True)
            os.mkdir(d, 0o700)
            fd = os.open(os.path.join(d, name), os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                fh.write(text)
        except OSError as exc:
            raise AdapterError(f"{pid}: cannot write its tmpfs secret file: {exc.strerror}") from None
        return d

    def probe_kind(self, proc):
        uid = proc["participants"][0]
        pub = next(p for p in self.plan["participants"] if p["user_id"] == uid)["publishes"]
        if pub["camera"]:
            return "camera_on"
        return "camera_off_mic_on" if pub["mic"] else "camera_off"

    def drop_secret(self, pid=None):
        path = self.secret_dir(pid)
        shutil.rmtree(path, ignore_errors=True)
        if os.path.lexists(path):
            raise AdapterError("a tmpfs secret folder could not be removed; remove it by hand")


def _image(image, image_id):
    name = str(image or "").rsplit("/", 1)[-1]
    digest = str(image_id or "").rpartition("@")[2]
    return f"{name}@{digest}" if digest.startswith("sha256:") else name


class RemoteStack:
    """A shared deployment, never brought up or down: refuses unless it is quiet, and watches it during the run."""

    def __init__(self, ctx):
        self.ctx = ctx
        p = ctx.profile
        self.window = Window(*p["window_utc"], teardown_s(ctx.plan)) if "window_utc" in p else None
        self.last_ok = self.first_watch = None
        self.relay_count = self.slow_base = None
        self.relay_down_since = self.watched_mono = None
        self.own_max = 0
        sel = f'namespace="{ctx.ns}"'
        relay = f'{sel},pod=~"{p["pods"]["relay_ws"]}|{p["pods"]["relay_wt"]}"'
        self.selector = 'up{' + sel + ',pod=~"' + "|".join(p["pods"][k] for k in SCRAPED_KINDS) + '"}'
        self.throttle = sel + ',pod=~"' + "|".join(p["pods"][k] for k in THROTTLE_KINDS) + '"'
        parts = {
            "real_sessions": f'sum(relay_active_sessions_per_room{{{sel},room!~"scale-.*"}} > 0)',
            "real_meetings": f'count(count by (meeting_id) (videocall_client_packets_sent_per_sec{{{sel},'
                             f'meeting_id!~"scale-.*"}} > 0))',
            "other_runs": f'count(relay_active_sessions_per_room{{{sel},room=~"scale-.*",room!="{ctx.room}"}} > 0)',
            "own_sessions": f'sum(relay_active_sessions_per_room{{{sel},room="{ctx.room}"}} > 0)',
            "relay_up_min": f"min(up{{{relay}}})",
            "relay_up_count": f"count(up{{{relay}}})",
            "slow_consumers": f'sum(relay_nats_events_total{{{relay},event="slow_consumer"}})',
        }
        self.watch_query = " or ".join(f'label_replace({q}, "cq", "{k}", "", "")' for k, q in parts.items())

    def scrape_up_selector(self):
        return self.selector

    def throttle_pod_selector(self):
        return self.throttle

    def prom_url(self):
        return self.ctx.profile["prom_url"]

    def down(self):
        return None

    def _values(self, max_time, timeout=None):
        vec = self.ctx.prom_vector(self.watch_query, max_time, timeout)
        if vec is None:
            return None
        out = {}
        for labels, v in vec.items():
            name = json.loads(labels).get("cq")
            if name is not None:
                out[name] = v
        return out

    def _rules(self, v):
        """Abort reasons from one answer of the watch query; counts only, never a room or meeting name."""
        out = []
        real, meetings = v.get("real_sessions", 0), v.get("real_meetings", 0)
        if real >= 1 or meetings >= 1:
            out.append(f"a real meeting is live ({real:.0f} sessions outside scale rooms, {meetings:.0f} meetings "
                       "with client packets)")
        if v.get("other_runs", 0) >= 1:
            out.append(f"another scale run is live ({v['other_runs']:.0f} rooms)")
        if "relay_up_count" not in v or "relay_up_min" not in v:
            out.append("Prometheus has no relay up series")
        else:
            if self.relay_count is not None and v["relay_up_count"] < self.relay_count:
                out.append(f"{v['relay_up_count']:.0f} relay targets, {self.relay_count:.0f} before the run")
            out += self._relay_down(v["relay_up_min"] < 1)
        if "slow_consumers" not in v:
            out.append("Prometheus has no relay slow-consumer series")
        elif self.slow_base is not None and v.get("relay_up_min", 0) >= 1:
            if v["slow_consumers"] > self.slow_base:
                out.append(f"relay NATS slow consumers rose by {v['slow_consumers'] - self.slow_base:.0f}")
            self.slow_base = min(self.slow_base, v["slow_consumers"])
        self.own_max = max(self.own_max, v.get("own_sessions", 0))
        return out

    def _relay_down(self, down):
        """Abort only on a relay read down RELAY_DOWN_S apart: one failed scrape reads down for at most an interval
        plus its timeout (at most the interval), and each reading is stamped up to WATCH_DEADLINE_S after its query."""
        mono = self.ctx.mono()
        if not down:
            self.relay_down_since = None
        elif self.relay_down_since is None:
            self.relay_down_since = mono
        elif mono - self.relay_down_since >= RELAY_DOWN_S:
            return [f"a relay target has read down for {mono - self.relay_down_since:.0f} s, past one scrape"]
        return []

    def _baseline(self, v):
        self.relay_count, self.slow_base = v.get("relay_up_count"), v.get("slow_consumers")
        self.last_ok = self.ctx.mono()

    def meeting_problems(self):
        """The live checks of the watch at a long timeout, plus a relay down now and the baselines."""
        v = self._values(HTTP_S)
        if v is None:
            return ["Prometheus did not answer the real-meeting and relay query"]
        self.relay_count = self.slow_base = None
        out = self._rules(v)
        if v.get("relay_up_min", 1) < 1:
            out.append("a relay target is down")
        self.relay_down_since = None
        if not out:
            self._baseline(v)
        return out

    def preflight(self, plan):
        c, problems = self.ctx, []
        problems += self.window.problems(c.now(), planned_s(plan)) if self.window else []
        problems += [f"{t} is not on PATH" for t in ("podman", "kubectl", "curl", "loginctl")
                     if shutil.which(t) is None]
        problems += self._secret_folder_problems()
        problems += self._checkout_problems(plan)
        if problems:
            return problems
        self._session_warnings()
        problems += self._load_problems()
        problems += self._linger_problems()
        kube = c.kubeconfig_problems()
        problems += kube
        c.dirty = c.sh(["git", "-C", c.root, "status", "--porcelain=v1", "--untracked-files=all"],
                       cq_local.DOCKER_S).stdout.strip() != ""
        c.commit = c.sh(["git", "-C", c.root, "rev-parse", "HEAD"], cq_local.DOCKER_S).stdout.strip()
        problems += self._podman_problems(plan)
        problems += self._fleet_problems(plan)
        problems += self.ready_problems(pods=not kube)
        problems += self.meeting_problems()
        problems += self._unique_problems()
        return problems

    def _session_warnings(self):
        held = os.environ.get("TMUX") or os.environ.get("STY") or signal.getsignal(signal.SIGHUP) == signal.SIG_IGN
        if not held:
            print("warning: the runner is not under tmux, screen or nohup; a dropped SSH session ends the run",
                  file=sys.stderr)

    def _linger_problems(self):
        c = self.ctx
        p = c.sh(["loginctl", "show-user", str(c.uid), "-p", "Linger", "--value"], cq_local.DOCKER_S, check=False)
        if p.returncode != 0 or p.stdout.strip() != "yes":
            return ["linger is off for this user, so logging out would remove its Podman runtime folder mid-run: "
                    "loginctl enable-linger"]
        return []

    def _secret_folder_problems(self):
        c = self.ctx
        if not os.path.isdir(c.secret_parent) or c.fs(c.secret_parent) != "tmpfs":
            return ["the secret parent folder is not a tmpfs, so a bot config with the key would touch disk"]
        left = sorted(glob.glob(os.path.join(c.secret_parent, SECRET_PREFIX + "*")))
        return [f"{len(left)} tmpfs secret folder(s) of an earlier run remain: remove them"] if left else []

    def _checkout_problems(self, plan):
        c = self.ctx
        need = ["Dockerfile.bot", cq_local.SAMPLER, os.path.join("e2e", cq_local.TSX),
                os.path.join("e2e", cq_local.CLI), PRELOAD, BROWSER_ENV]
        if any(p["talker"] and p["fleet"] == "rust" for p in plan["participants"]):
            need.append(cq_local.TALKER_WAV)
        missing = [f for f in need if not os.path.exists(os.path.join(c.root, f))]
        if missing:
            return [f"missing in the checkout: {', '.join(missing)}"]
        with open(os.path.join(c.root, BROWSER_ENV), encoding="utf-8") as fh:
            if not STRIP_RE.search(fh.read()):
                return [f"{BROWSER_ENV} does not strip JWT_SECRET from Chromium's environment (#3152); check out "
                        "PR-staging at or after it"]
        return []

    def _podman_problems(self, plan):
        c = self.ctx
        try:
            c.sh([c.docker, "version", "--format", "{{.Client.Version}}"], cq_local.DOCKER_S)
        except AdapterError as exc:
            return [f"podman is not usable: {exc}"]
        problems = []
        if c.sh([c.docker, "image", "inspect", cq_local.PW_IMAGE], cq_local.DOCKER_S, check=False).returncode != 0:
            problems.append(f"no image {cq_local.PW_IMAGE}: load it into Podman")
        problems += self._bot_image_problems(plan)
        p = c.sh([c.docker, "ps", "-a", "--filter", f"label={cq_local.LABEL}", "--format", "json"], cq_local.DOCKER_S)
        try:
            rows = json.loads(p.stdout or "[]")
            runs = sorted({(r.get("Labels") or {}).get(cq_local.LABEL) or "?" for r in rows})
        except (ValueError, AttributeError, TypeError):
            return problems + ["podman ps gave an unreadable answer"]
        problems += [f"containers labelled {cq_local.LABEL}={r} exist; that run may still be going" for r in runs]
        return problems

    def _bot_image_problems(self, plan):
        c = self.ctx
        p = c.sh([c.docker, "image", "inspect", "--format", cq_local.BOT_LABELS_FORMAT, c.bot_image], cq_local.DOCKER_S,
                 check=False)
        if p.returncode != 0 or p.stdout.strip() != cq_local.bot_labels(c.commit, c.dirty):
            return [f"no bot image {c.bot_image} built from this checkout: build it with make cq-bot-image, outside "
                    "the deploy window, and load it into Podman (the remote stack never builds)"]
        h = c.sh([c.docker, "run", "--rm", *c.run_flags(), "--network", "none", "--entrypoint", "sh", c.bot_image,
                  "-c", "bot --help 2>&1"], cq_local.DOCKER_S, check=False).stdout
        problems = [] if has_option(h, "--deadline") else [
            f"the bot image {c.bot_image} has no --deadline option, so its bots and tokens could outlive the run"]
        if plan.get("video_mode", cq_local.RUST_VIDEO_MODE) == "replay" and "replay" not in h:
            problems.append(f"run.video_mode is replay, but the bot image {c.bot_image} has no replay video source "
                            "(#3166)")
        if not has_option(h, "--diagnostics"):
            problems.append(f"run.bot_diagnostics is {plan['bot_diagnostics']}, but the bot image {c.bot_image} has no "
                            "--diagnostics switch (#2958)")
        return problems

    def _fleet_problems(self, plan):
        c, problems = self.ctx, []
        probes = [p for p in plan["processes"] if p["fleet"] == "browser"]
        if len(probes) > cq_local.MAX_PROBES:
            problems.append(f"{len(probes)} probes; at most {cq_local.MAX_PROBES} control ports")
        problems += [f"{p['proc_id']}: transport {p['transport']!r}; a remote probe needs websocket or webtransport"
                     for p in probes if p["transport"] not in PROBE_TRANSPORTS]
        problems += cq_local.rust_layout(plan)[2]
        problems += [f"{p['proc_id']}: join_stagger {p['join_stagger_s']:g} s is longer than the {MAX_STAGGER_S:g} s "
                     "its readiness deadline leaves after its launch" for p in probes[:-1]
                     if p["join_stagger_s"] > MAX_STAGGER_S]
        if probes and join_floor_s(plan) > plan["steps"][0]["join_window_s"]:
            rows = join_timeline(plan)
            problems.append(f"the join's worst case takes {sum(s for _, _, s in rows):.1f} s, more than the join "
                            f"window plus {cq_runner.JOIN_BUDGET_EXTRA_S} s: raise steps[0].join_window to at least "
                            f"{join_floor_s(plan)} s ({'; '.join(f'{what} {s:g} s' for _, what, s in rows)})")
        unpriced = [p["proc_id"] for p in probes if c.profile["probe_cores"][c.probe_kind(p)] is None]
        problems += [f"{', '.join(unpriced)}: no CPU cost for this kind of probe in the target profile's probe_cores, "
                     "so it cannot be admitted; measure it on the load host first"] if unpriced else []
        rust = [p for p in plan["processes"] if p["fleet"] == "rust"]
        busy = [str(metrics_port(plan, p)) for p in rust if c.port_open(metrics_port(plan, p))]
        problems += [f"Rust metrics ports in use on 127.0.0.1: {', '.join(busy)}"] if busy else []
        if probes:
            qdiscs = c.read_qdiscs()
            c.dump(os.path.join("probes", "netns-qdisc.json"), qdiscs)
            link = cq_local.qdisc_problems(qdiscs)
            c.netem_applied = None if link else "none"
            problems += [f"load host network (the probes' egress): {x}" for x in link]
        return problems

    def _load_problems(self):
        c = self.ctx
        a = (c.mono(), *c.cpu())
        c.sleep(LOAD_SAMPLE_S)
        b = busy_pct(a, (c.mono(), *c.cpu()))
        if b is None or b > LOAD_LINE_PCT:
            return [f"load host CPU is {'unknown' if b is None else f'{b:.0f}%'} before any bot, above the "
                    f"{LOAD_LINE_PCT:.0f}% line"]
        return []

    def ready_problems(self, pods=True):
        c, p, problems = self.ctx, self.ctx.profile, []
        for name, url in (("UI", p["ui_url"] + "/"), ("meeting-api", p["api_url"] + "/version")):
            status, _ = c.http(url)
            if status != 200:
                problems.append(f"{name} answered {status}")
        problems += self._targets_problems(c.prom_json("/api/v1/targets", state="active"))
        if pods:
            try:
                problems += self._pod_problems(c.pods())
            except AdapterError as exc:
                problems.append(str(exc))
        return problems

    def _targets_problems(self, doc):
        data = (doc or {}).get("data")
        active = data.get("activeTargets") if isinstance(data, dict) else None
        if not isinstance(active, list):
            return ["Prometheus /api/v1/targets did not answer"]
        problems, pods = [], self.ctx.profile["pods"]
        for kind in SCRAPED_KINDS:
            ts = [t for t in active if isinstance(t, dict) and (t.get("labels") or {}).get("namespace") == self.ctx.ns
                  and re.fullmatch(pods[kind], str((t.get("labels") or {}).get("pod", "")))]
            if not ts:
                problems.append(f"no {kind} scrape target")
            if any(t.get("health") != "up" for t in ts):
                problems.append(f"a {kind} scrape target is not up")
            if any(t.get("scrapeInterval") != cq_local.SCRAPE_INTERVAL for t in ts):
                problems.append(f"a {kind} scrape target is not scraped every {cq_local.SCRAPE_INTERVAL}")
        return problems

    def _pod_problems(self, pods):
        now, problems = self.ctx.now(), []
        for kind in POD_KINDS:
            mine = {n: f for n, f in pods.items() if f["kind"] == kind}
            if not mine:
                problems.append(f"no {kind} pod")
            elif not all(f["ready"] and f["phase"] == "Running" for f in mine.values()):
                problems.append(f"a {kind} pod is not ready")
            started = [cq_local.docker_time(f["started_at"]) for f in mine.values()]
            if any(t is None or now - t < FRESH_POD_S for t in started):
                problems.append(f"a {kind} pod started less than {FRESH_POD_S // 60} min ago, or its start is "
                                "unknown: daily may be mid-deploy")
        return problems

    def _unique_problems(self):
        c, room = self.ctx, self.ctx.room
        seen = c.prom_vector(f'count(last_over_time(relay_active_sessions_per_room{{room="{room}"}}[1h])) or '
                             f'count(last_over_time(videocall_client_packets_sent_per_sec{{meeting_id="{room}"}}[1h]))',
                             timeout=PROM_HEAVY_TIMEOUT)
        family = c.prom_vector(f'count(last_over_time(relay_active_sessions_per_room{{namespace="{c.ns}"}}[7d]))',
                               timeout=PROM_HEAVY_TIMEOUT)
        problems = []
        if seen is None or family is None:
            problems.append("Prometheus did not answer the meeting-uniqueness or session-family query")
        if seen:
            problems.append("this run's meeting id was already used in the last hour")
        if family is not None and not family:
            problems.append("relay_active_sessions_per_room has no series in 7 days, so the real-meeting watch "
                            "would be blind")
        return problems

    def up(self, plan, mode):
        c = self.ctx
        if mode != "reuse":
            raise AdapterError(f"{plan['target']} is shared: --stack reuse only")
        deadline = c.mono() + cq_local.READY_REUSE_S
        while True:
            problems = self.ready_problems()
            if not problems:
                break
            if c.mono() >= deadline:
                raise AdapterError(f"stack not ready after {cq_local.READY_REUSE_S} s: {'; '.join(problems)}")
            c.sleep(10.0)
        problems = (self.window.problems(c.now(), planned_s(plan)) if self.window else []) + self.meeting_problems()
        if problems:
            raise AdapterError("before the join: " + "; ".join(problems))
        c.dump(os.path.join("stack", "targets-ready.json"), self._slim_targets())

    def _slim_targets(self):
        doc = self.ctx.prom_json("/api/v1/targets", state="active")
        active = ((doc or {}).get("data") or {}).get("activeTargets") or []
        keep = ("health", "lastScrapeDuration", "scrapeInterval", "scrapeTimeout")
        return [dict({k: t.get(k) for k in keep}, job=t["labels"].get("job"), pod=t["labels"].get("pod"))
                for t in active if isinstance(t, dict) and isinstance(t.get("labels"), dict)
                and t["labels"].get("namespace") == self.ctx.ns]

    def state(self):
        return {n: {k: f[k] for k in ("kind", "restart_count", "started_at", "image")}
                for n, f in sorted(self.ctx.pods().items())}

    def images(self):
        c = self.ctx
        pods = c.pods()
        out = {}
        for key, kind in (("ui", "ui"), ("relay_ws", "relay_ws"), ("relay_wt", "relay_wt")):
            imgs = sorted({i for f in pods.values() if f["kind"] == kind for i in f["image"]})
            if not imgs:
                raise AdapterError(f"no {kind} pod for images.json")
            out[key] = " + ".join(imgs)
        pw = c.sh([c.docker, "image", "inspect", "--format", "{{.Id}}", cq_local.PW_IMAGE], cq_local.DOCKER_S)
        bot = c.sh([c.docker, "image", "inspect", "--format", "{{.Id}}", c.bot_image], cq_local.DOCKER_S)
        out.update(bots_app=f"{pw.stdout.strip()} + e2e@{c.commit}", rust_bot=f"{bot.stdout.strip()} ({c.bot_image})")
        return out

    def save(self):
        self.ctx.dump(os.path.join("stack", "targets.json"), self._slim_targets())

    def watch(self):
        """Abort reasons within WATCH_DEADLINE_S: the window, the load host, and one instant query; a failed query
        is absorbed until the watch has been blind for BLIND_S. Every caller's next watch is due from watched_mono."""
        try:
            return self._watch_round()
        finally:
            self.watched_mono = self.ctx.mono()

    def _watch_round(self):
        c = self.ctx
        mono = c.mono()
        self.first_watch = mono if self.first_watch is None else self.first_watch
        out = (self.window.reasons(c.now()) if self.window else []) + c.load.check(mono)
        v = self._values(WATCH_DEADLINE_S, PROM_TIMEOUT)
        mono = c.mono()
        if v is None:
            if self.last_ok is None or mono - self.last_ok >= BLIND_S:
                out.append(f"the watch has had no Prometheus answer for {BLIND_S:.0f} s or more")
            return out
        self.last_ok = mono
        out += self._rules(v)
        step = c.plan["steps"][0]
        n = len(c.plan["participants"])
        if self.own_max < n and mono - self.first_watch > step["join_window_s"] + cq_runner.JOIN_BUDGET_EXTRA_S + \
                BLIND_S:
            out.append(f"Prometheus never showed this run's {n} sessions in its room (at most {self.own_max:.0f}), "
                       "so the real-meeting watch may be blind")
        return out

    def drained(self):
        """The room is drained when its session series reads 0, or is gone after it was seen populated."""
        c = self.ctx
        deadline, last = c.mono() + DRAIN_S, None
        while True:
            v = self._values(DRAIN_QUERY_S)
            if v is not None:
                live = v.get("relay_up_count", 0) >= 1 and v.get("relay_up_min", 0) >= 1 and \
                    (self.relay_count is None or v["relay_up_count"] >= self.relay_count)
                n = v.get("own_sessions")
                self.own_max = max(self.own_max, n or 0)
                if live and (n <= 0 if n is not None else self.own_max > 0):
                    return []
                last = n if live else None
            if c.mono() >= deadline:
                if last is not None:
                    return [f"the room still had {last:.0f} sessions {DRAIN_S} s after the stops"]
                if v is not None and live and n is None:
                    return [f"the room was never seen with a session, so its absence for {DRAIN_S} s does not show "
                            "it drained"]
                return [f"the drain could not be confirmed for {DRAIN_S} s: no Prometheus answer, or a relay target "
                        "down or missing"]
            c.sleep(min(DRAIN_POLL_S, max(0.0, deadline - c.mono())))


def has_option(help_text, option):
    return re.search(rf"(?<![\w-]){re.escape(option)}(?![\w-])", help_text) is not None


def join_timeline(plan):
    """The preflight estimate of the join, from join start until every participant is confirmed joined:
    [(key, what, seconds)], each phase as long as the deadline, timeout or sample count that ends it."""
    probes = [p for p in plan["processes"] if p["fleet"] == "browser"]
    rows = [("rust", "Rust bots launched and joined, media started", cq_runner.RUST_READY_S)]
    for k, p in enumerate(probes):
        pid = p["proc_id"]
        if k == 0:
            rows.append((("admit", pid), f"{pid} admission ({OWNER_SAMPLES} samples)", OWNER_SAMPLES * SAMPLE_S))
        else:
            rows.append((("seen", pid), f"{probes[k - 1]['proc_id']} seen in the meeting", SEEN_S))
            rows.append((("admit", pid), f"{pid} admission (settle, {LATER_SAMPLES} samples)",
                         ADMIT_SETTLE_S + WATCH_S + LATER_SAMPLES * SAMPLE_S))
        rows.append((("ready", pid), f"{pid} launched and in the meeting", PROBE_READY_S))
    if probes:
        earlier = cq_local.DOCKER_S + WATCH_S + (len(probes) - 1) * CONFIRM_S - PROBE_READY_S
        rows.append(("confirm", f"the probes confirmed (Prometheus, env check) after {probes[-1]['proc_id']} is in",
                     CONFIRM_S + max(0, earlier)))
    return [(key, what, round(s, 1)) for key, what, s in rows]


def join_floor_s(plan):
    """The shortest join window the join's worst case fits."""
    return math.ceil(round(sum(s for _, _, s in join_timeline(plan)) - cq_runner.JOIN_BUDGET_EXTRA_S, 1))


def metrics_port(plan, proc):
    rust = [p["proc_id"] for p in plan["processes"] if p["fleet"] == "rust"]
    return METRICS_PORT_BASE + rust.index(proc["proc_id"])


def _tcp_open(port):
    with socket.socket() as s:
        s.settimeout(1)
        return s.connect_ex(("127.0.0.1", port)) == 0


class RemoteFleet(cq_local.LocalFleet):
    """Rust bot and bots-app probe containers under rootless Podman; the key reaches them only as tmpfs files."""
    PROC_GREP = PROC_GREP
    CTL_S = CTL_S

    def __init__(self, ctx, stack=None):
        super().__init__(ctx, stack)
        self.dropped, self.in_meeting, self.prom_asked, self.stopped = set(), set(), {}, {}
        self.poll_ok = self.run_end = None
        self.in_meeting_at, self.ready_by, self.join_by, self.join_mono = {}, {}, {}, None
        self.watch_at = None

    def launch(self, proc):
        c = self.ctx
        if self.join_mono is None:
            self.join_mono = c.mono()
        if proc["fleet"] == "rust":
            self.ready_by[proc["proc_id"]] = self.join_mono + cq_runner.RUST_READY_S
        super().launch(proc)

    def _launch_s(self, pid):
        """The launch's timeout: DOCKER_S, cut to what is left of its readiness deadline."""
        left = self.ready_by[pid] - self.ctx.mono()
        if left <= 0:
            raise AdapterError(f"{pid}: its readiness deadline passed before its launch")
        return min(cq_local.DOCKER_S, left)

    def _tick(self):
        hard = self.ctx.load.sample(self.ctx.mono())
        if hard:
            raise AdapterError("; ".join(hard))
        for pid in [p for p in self.launched if p not in self.dropped and self.procs[p]["fleet"] == "browser"]:
            if os.path.exists(self.ctx.path("probes", str(self.procs[pid]["probe_index"]), KEY_READ)):
                self.ctx.drop_secret(pid)

    def _deadline(self):
        """The run's one deadline in whole epoch seconds: run_need_s from the first call (the first launch, at
        join start), and never later than the window's end."""
        c = self.ctx
        now = c.now()
        if self.run_end is None:
            self.run_end = now + run_need_s(c.plan)
        window = self.stack.window if self.stack is not None else None
        return int(min(self.run_end, window.end_epoch(now) if window is not None else math.inf))

    def _ttl_s(self):
        left = self._deadline() - self.ctx.now()
        if left < MIN_LAUNCH_S:
            raise AdapterError(f"only {max(0, left):.0f} s are left before the run's deadline, less than the "
                               f"{MIN_LAUNCH_S} s a launch needs")
        return int(left)

    def poll(self):
        self._tick()
        c = self.ctx
        alive = [p for p in self.launched if p not in self.reported]
        try:
            states = c.inspect([c.container(p) for p in alive], FLEET_INSPECT_S)
        except AdapterError:
            self.poll_ok = c.mono() if self.poll_ok is None else self.poll_ok
            if c.mono() - self.poll_ok >= BLIND_S:
                raise
            return []
        self.poll_ok = c.mono()
        out = []
        for pid in alive:
            st = states.get(c.container(pid))
            if st is None or not st.get("Running", True):
                self.reported.add(pid)
                out.append(cq_runner.Exit(pid, None, None) if st is None else self._exit(pid, st))
        return out

    def stop(self, proc):
        """Stops the process's whole fleet group once, inspects the group in one call, and kills any container
        still running, so no bot outlives teardown's budget."""
        c, pid = self.ctx, proc["proc_id"]
        if pid not in self.stopping:
            group = [p for p in self.launched if self.procs[p]["fleet"] == proc["fleet"]
                     and p not in self.reported and p not in self.stopping]
            self.stopping |= set(group)
            stop_s = cq_local.RUST_STOP_S if proc["fleet"] == "rust" else cq_local.PROBE_STOP_S
            for step in (lambda: c.sh([c.docker, "stop", "-t", str(stop_s), *[c.container(p) for p in group]],
                                      stop_s + STOP_EXTRA_S, check=False),
                         lambda: self.stopped.update(c.inspect([c.container(p) for p in group], FLEET_INSPECT_S))):
                try:
                    step()
                except AdapterError:
                    pass
        st = self.stopped.get(c.container(pid))
        if st is None or st.get("Running"):
            try:
                c.sh([c.docker, "kill", "--signal", "KILL", c.container(pid)], KILL_S, check=False)
            except AdapterError:
                pass
            st = c.inspect([c.container(pid)], FLEET_INSPECT_S).get(c.container(pid))
        if st is None:
            self.reported.add(pid)
            return cq_runner.Exit(pid, None, None)
        if st.get("Running"):
            raise AdapterError(f"{pid} is still running after podman stop and kill")
        self.reported.add(pid)
        return self._exit(pid, st)

    def _in_meeting(self, pid):
        """In the meeting by healthz, and by its own record in time for its join deadline; then for good."""
        if pid not in self.in_meeting:
            left = self._budget_left()
            if left <= 0:
                return False
            try:
                seen = super()._in_meeting(pid, min(CTL_S, left))
            except AdapterError:
                if left >= CTL_S:
                    raise
                return False
            if seen and self._joined_in_time(pid):
                self.in_meeting.add(pid)
                self.in_meeting_at[pid] = self.ctx.mono()
        return pid in self.in_meeting

    def _joined_in_time(self, pid):
        """Before its readiness deadline, any probe in the meeting is; after it, only one whose own record has it
        joining by its join deadline."""
        if self.ctx.mono() <= self.ready_by[pid]:
            return True
        uid = self.procs[pid]["participants"][0]
        folder = self.ctx.path("probes", str(self.procs[pid]["probe_index"]), "participants")
        recs = [self._read(os.path.join(folder, n)) for n in sorted(os.listdir(folder))
                if n.endswith(".json")] if os.path.isdir(folder) else []
        ts = next(((r.get("participant") or {}).get("join_ts") for r in recs if isinstance(r, dict)
                   and (r.get("participant") or {}).get("user_id") == uid), None)
        return isinstance(ts, (int, float)) and ts <= self.join_by[pid]

    def _budget_left(self):
        c = self.ctx
        return self.join_mono + c.plan["steps"][0]["join_window_s"] + cq_runner.JOIN_BUDGET_EXTRA_S - c.mono()

    def _prom_reports(self, uid):
        mono = self.ctx.mono()
        if mono - self.prom_asked.get(uid, -math.inf) < JOIN_PROM_EVERY_S:
            return False
        self.prom_asked[uid] = mono
        q = (f'count(videocall_peer_can_listen{{namespace="{self.ctx.ns}",meeting_id="{self.ctx.room}",'
             f'from_peer="{uid}"}})')
        vec = self.ctx.prom_vector(q, JOIN_PROM_S, PROM_TIMEOUT)
        return bool(vec) and sum(vec.values()) > 0

    def admit(self, proc):
        """Admitted if the load host's busy cores over ADMIT_SAMPLE_S, plus the probe's cost from the target
        profile, fit under the 70% line less ADMIT_OVERHEAD_CORES. The owner, which the run cannot lose, gets a second
        sample if the first refuses it. A later probe waits for the previous probe to be in the meeting, then samples
        from ADMIT_SETTLE_S after that, past its join transient, until a sample fits or ADMIT_SETTLE_CAP_S is used."""
        c = self.ctx
        kind = c.probe_kind(proc)
        cost = c.profile["probe_cores"][kind]
        if cost is None:
            return False, f"no {kind.replace('_', '-')} probe cost in the target profile"
        prev = next((p for p in reversed(self.launched) if self.procs[p]["fleet"] == "browser"), None)
        samples = OWNER_SAMPLES
        if prev is not None:
            self._await_in_meeting(prev, proc["proc_id"])
            samples = LATER_SAMPLES
            self._wait_watched(self.in_meeting_at[prev] + ADMIT_SETTLE_S, "the admission wait")
        for sample in range(1, samples + 1):
            a = (c.mono(), *c.cpu())
            self._wait_watched(c.mono() + ADMIT_SAMPLE_S, "the admission sample")
            self._tick()
            pct, ncpu = busy_pct(a, (c.mono(), *c.cpu())), c.ncpu()
            if pct is None or not 0 <= pct <= 100 or not ncpu:
                return False, "the load host's CPU could not be read"
            now, limit = pct / 100 * ncpu, LOAD_LINE_PCT / 100 * ncpu - ADMIT_OVERHEAD_CORES
            ok = now + cost <= limit
            if ok:
                break
        return ok, (f"host {now:.2f} + {kind.replace('_', '-')} probe {cost:.2f} cores {'<=' if ok else '>'} "
                    f"{limit:.2f} (the {LOAD_LINE_PCT:.0f}% line less {ADMIT_OVERHEAD_CORES} overhead"
                    f"{f', sample {sample}' if sample > 1 else ''})")

    def _wait_watched(self, until, what, done=lambda: False):
        """Waits for `done()` or `until` on the monotonic clock, with the load line and the stack watch running."""
        c = self.ctx
        if self.watch_at is None:
            self.watch_at = c.mono() + cq_runner.WATCH_INTERVAL_S
        while not done() and c.mono() < until:
            self._tick()
            last = getattr(self.stack, "watched_mono", None)
            due = self.watch_at if last is None else last + cq_runner.WATCH_INTERVAL_S
            if self.stack is not None and c.mono() >= due:
                reasons = self.stack.watch()
                self.watch_at = c.mono() + cq_runner.WATCH_INTERVAL_S
                if reasons:
                    raise AdapterError(f"stack watch during {what}: " + "; ".join(reasons))
            c.sleep(max(0.0, min(cq_local.OWNER_POLL_S, until - c.mono())))
        return done()

    def _await_in_meeting(self, prev, pid):
        """A later probe waits for the previous probe to be in the meeting, until that probe's readiness deadline."""
        owner = next(p for p in self.launched if self.procs[p]["fleet"] == "browser")
        if not self._wait_watched(self.ready_by[prev], "the owner wait", lambda: self._in_meeting(prev)):
            waited = f"by its readiness deadline, {PROBE_READY_S} s from its launch"
            raise AdapterError(f"{pid}: {owner} owns the meeting and was not in it {waited}, so the waiting room may "
                               "still be on" if prev == owner else f"{pid}: the previous probe {prev} was not in the "
                               f"meeting {waited}")

    def _await_owner(self, pid):
        owner = next(p for p in self.launched if self.procs[p]["fleet"] == "browser")
        if owner != pid:
            self._await_in_meeting(owner, pid)

    def clock_facts(self):
        c = self.ctx
        p = c.sh(["timedatectl", "show", "-p", "NTPSynchronized", "--value"], cq_local.DOCKER_S, check=False) \
            if shutil.which("timedatectl") else None
        sync = "ntp" if p is not None and p.returncode == 0 and p.stdout.strip() == "yes" else "unknown"
        before = c.now()
        at = c.prom_time()
        after = c.now()
        if at is None:
            raise AdapterError("Prometheus gave no time(), so the clock skew to it is unknown")
        c.skew_ms = round((at - (before + after) / 2) * 1000, 1)
        if abs(c.skew_ms) > MAX_SKEW_MS:
            raise AdapterError(f"the clock is {c.skew_ms} ms off Prometheus; the limit is {MAX_SKEW_MS:.0f} ms")
        return {"sync": sync, "per_node": [{"node": NODE, "skew_ms": c.skew_ms}]}

    def _base_run(self, proc, stop_s):
        c = self.ctx
        return [c.docker, "run", "-d", "--init", *c.run_flags(), "--stop-timeout", str(stop_s),
                "--name", c.container(proc["proc_id"]), "--label", f"{cq_local.LABEL}={c.run_id}",
                "--label", f"{cq_local.PROC_LABEL}={proc['proc_id']}", "--user", f"{c.uid}:{c.gid}"]

    def _rust_cfg(self, proc):
        return dict(super()._rust_cfg(proc), token_ttl_secs=self._ttl_s())

    def _launch_rust(self, proc):
        c, pid = self.ctx, proc["proc_id"]
        self._rust_inputs()
        os.makedirs(c.path("rust"), exist_ok=True)
        cfg = self._rust_cfg(proc)
        with open(c.path("in", f"{pid}.yaml"), "w", encoding="utf-8") as fh:
            json.dump(cfg, fh, indent=1)
        cfg_dir = c.write_secret(pid, f"{pid}.yaml", json.dumps(dict(cfg, jwt_secret=c.jwt_secret)))
        argv = self._base_run(proc, cq_local.RUST_STOP_S) + [
            "--network", "host", "-e", f"RUST_LOG={cq_local.RUST_LOG}", "-w", "/rust", "-v", c.mount(c.path("in"), "/in", "ro"),
            "-v", c.mount(cfg_dir, "/cfg", "ro"), "-v", c.mount(c.path("rust"), "/rust"), c.bot_image,
            "--config", f"/cfg/{pid}.yaml", "--id-prefix", c.run_id, "--participants-out",
            f"/rust/participants-{pid}.json", "--no-impair", "--duration", f"{self._ttl_s()}s",
            "--deadline", str(self._deadline()),
            "--metrics-port", str(metrics_port(c.plan, proc)), "--metrics-bind", "127.0.0.1",
            *cq_local.rust_flags(c.plan), *cq_local.rust_layout(c.plan)[1][pid]]
        c.sh(argv, self._launch_s(pid))

    def _launch_probe(self, proc):
        c, pid = self.ctx, proc["proc_id"]
        self._await_owner(pid)
        self.ready_by[pid] = c.mono() + PROBE_READY_S
        join_by = self.join_by[pid] = int(c.now() + PROBE_READY_S)
        part = self.parts[proc["participants"][0]]
        out = c.path("probes", str(proc["probe_index"]))
        os.makedirs(out, exist_ok=True)
        token = secrets.token_hex(16)
        c.tokens[pid] = token
        cfg_dir = c.write_secret(pid, "probe-secret", c.jwt_secret)
        argv = self._base_run(proc, cq_local.PROBE_STOP_S) + [
            "--shm-size", "1g", "-e", "BOT_CTL_TOKEN", "-e", "BOT_CTL_STATE_DIR=/tmp/ctl", "-e", "HOME=/tmp/h",
            "-e", "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1", "-e", f"BOT_NETEM_APPLIED={c.netem_applied or 'unknown'}",
            "-e", f"COOKIE_NAME={c.profile['cookie_name']}", "-v", c.mount(os.path.join(c.root, "e2e"), "/app/e2e"),
            "-v", c.mount(out, "/out"), "-v", c.mount(cfg_dir, "/cfg", "ro"),
            "-v", c.mount(os.path.join(c.root, PRELOAD), "/cq/preload.mjs", "ro"), "-w", "/app/e2e",
            "--entrypoint", "node", cq_local.PW_IMAGE, "--import", "tsx", "--import", "/cq/preload.mjs",
            cq_local.CLI, "run", "--meeting-url", f"{c.ui_url}/meeting/{c.plan['meeting_id']}",
            "--participant", part["user_id"].split("@")[0], "--display-name", part["name"], "--video-mode", "clock",
            "--headless", "--ttl", f"{self._ttl_s()}s", "--deadline", str(self._deadline()),
            "--join-deadline", str(join_by), "--manifest", "",
            "--assets-dir", "/out",
            "--ctl-port", str(cq_local.CTL_PORT_BASE + proc["probe_index"]), "--role", part["role"],
            "--decode-budget", proc.get("decode_budget", "off"), "--auth", "jwt", "--sso-state-file", "",
            "--transport", PROBE_TRANSPORTS[proc["transport"]]]
        argv += cq_local.probe_flags(proc)
        argv += [] if part["publishes"]["camera"] else ["--join-camera-off"]
        argv += [] if part["publishes"]["mic"] else ["--join-mic-muted"]
        c.sh(argv, self._launch_s(pid), env=c.env(BOT_CTL_TOKEN=token))

    def joined(self):
        self._tick()
        out = super().joined()
        late = [p for p in self.launched if self.procs[p]["fleet"] == "browser" and p not in self.in_meeting
                and self.ctx.mono() > self.ready_by[p]]
        if late:
            raise AdapterError(f"{late[0]}: not in the meeting by its readiness deadline, {PROBE_READY_S} s from its "
                               "launch")
        for pid in [p for p in self.launched if p not in self.dropped]:
            proc = self.procs[pid]
            if all(u in out for u in proc["participants"]):
                if proc["fleet"] == "browser":
                    self._env_check(pid)
                self.ctx.drop_secret(pid)
                self.dropped.add(pid)
        return out

    def _env_check(self, pid):
        c = self.ctx
        left = self._budget_left()
        if left <= 0:
            raise AdapterError(f"{pid}: the join budget ran out before its environment check")
        p = c.sh([c.docker, "exec", c.container(pid), "sh", "-c", ENV_CHECK_SH], min(cq_local.DOCKER_S, left),
                 check=False)
        lines = p.stdout.split()
        if "done" not in lines:
            raise AdapterError(f"{pid}: could not read the probe's process environments")
        hits = [x for x in lines if x.startswith("/proc/")]
        if hits:
            raise AdapterError(f"{pid}: {len(hits)} process(es) in the probe hold JWT_SECRET in their environment")
        if not os.path.exists(c.path("probes", str(self.procs[pid]["probe_index"]), KEY_READ)):
            raise AdapterError(f"{pid}: the probe's preload never confirmed it read the key")

    def collect(self, run_dir):
        try:
            super().collect(run_dir)
        finally:
            shutil.rmtree(self.ctx.secret_dir(), ignore_errors=True)

    def leaks(self, run_dir):
        c = self.ctx
        left = ["the run's tmpfs secret folder still exists"] if os.path.lexists(c.secret_dir()) else []
        left += [f"the signing key was in container or tool output {c.key_scrubs} time(s) and was redacted before it "
                 "was saved: rotate the key and tell the target's owner"] if c.key_scrubs else []
        return left + leak_scan(run_dir, c.jwt_secret, list(c.tokens.values()))


def undumpable():
    """No core dump, and the same uid can no longer read this process's environ or memory; raises if that fails."""
    try:
        import ctypes
        libc = ctypes.CDLL(None, use_errno=True)
        libc.prctl(PR_SET_DUMPABLE, 0, 0, 0, 0)
        ok = libc.prctl(PR_GET_DUMPABLE, 0, 0, 0, 0) == 0
    except (OSError, AttributeError):
        ok = False
    if not ok:
        raise AdapterError("the runner could not make itself undumpable, so its environment, which held the key, "
                           "stays readable to this user's processes")


def factory(plan, run_dir, stack_mode="up", environ=None, profile_path=None):
    """The remote stack and fleet. JWT_SECRET leaves the environment here, before anything else, so no child of this
    process inherits it."""
    env = os.environ if environ is None else environ
    secret = env.pop(cq_runner.SECRET_ENV, None)
    target = plan["target"]
    if env.get(CONFIRM_ENV) != target:
        raise AdapterError(f"set {CONFIRM_ENV}={target} to confirm the run against the shared target")
    profile = load_profile(target, profile_path)
    if not secret or len(secret) < MIN_SECRET_LEN:
        raise AdapterError(f"{cq_runner.SECRET_ENV} is not set, or is shorter than {MIN_SECRET_LEN} characters")
    if secret == profile["dev_secret"]:
        raise AdapterError(f"{cq_runner.SECRET_ENV} is the e2e dev default, not the target's")
    if stack_mode != "reuse":
        raise AdapterError(f"{target} is shared: pass --stack reuse")
    if not ROOM_RE.match(plan["meeting_id"]):
        raise AdapterError("the plan's meeting id is not a scale room name")
    ctx = Remote(plan, run_dir, cq_local.Settings(), stack_mode=stack_mode, jwt_secret=secret, profile=profile,
                 kubeconfig=env.get(KUBECONFIG_ENV) or profile["kubeconfig"],
                 kubeconfig_from_env=bool(env.get(KUBECONFIG_ENV)))
    undumpable()
    stack = RemoteStack(ctx)
    return stack, RemoteFleet(ctx, stack)
