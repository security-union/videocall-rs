#!/usr/bin/env python3
"""Print a shared target's signing key, for `JWT_SECRET="$(...)"`, after the runner's own kubeconfig checks (#2914)."""

import argparse
import base64
import http.client
import json
import os
import resource
import shutil
import signal
import socket
import ssl
import stat
import sys
import tempfile
import threading
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import cq_local  # noqa: E402
import cq_remote  # noqa: E402
from cq_runner import AdapterError  # noqa: E402

EXIT_REFUSED = 3
FETCH_DEADLINE_S = 20.0
MAX_BODY = 1 << 20
PR_GET_DUMPABLE = 3
KEY_PARENT = cq_remote.SECRET_PARENT
STOP_SIGNALS = (signal.SIGTERM, signal.SIGHUP, signal.SIGINT)


class Stopped(BaseException):
    def __init__(self, signum, folder=None):
        super().__init__(signum)
        self.signum, self.folder = signum, folder


def harden():
    """No core file, and no reading this process's memory as the same user. False when the platform refuses."""
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    try:
        import ctypes
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.prctl(cq_remote.PR_SET_DUMPABLE, 0, 0, 0, 0) == 0 and libc.prctl(PR_GET_DUMPABLE, 0, 0, 0, 0) == 0:
            return True
    except (OSError, AttributeError):
        pass
    return False


def _key_parent():
    """The owner-only tmpfs folder the kubeconfig copy and client key go in, as the run's bot secrets do."""
    path = KEY_PARENT.format(uid=os.getuid())
    try:
        st = os.lstat(path)
    except OSError:
        st = None
    if st is None or not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o077 \
            or cq_remote.fs_type(path) != "tmpfs":
        raise AdapterError(f"{path} is not an owner-only tmpfs folder of this user, so the kubeconfig copy could land "
                           "on a disk-backed filesystem; read the key on the load host (section 3.3)")
    return path


def _stop_on_signals(held):
    """Turns the first SIGTERM, SIGHUP or SIGINT into Stopped naming held["folder"], removed on the way out."""
    stopping = []

    def stop(signum, _frame):
        if not stopping:
            stopping.append(signum)
            raise Stopped(signum, held.get("folder"))
    return {s: signal.signal(s, stop) for s in STOP_SIGNALS}


def _start_without_stop_signals(thread):
    """Starts `thread` with STOP_SIGNALS blocked, so they only reach the main thread, whose mask then holds."""
    mask = signal.pthread_sigmask(signal.SIG_BLOCK, STOP_SIGNALS)
    try:
        thread.start()
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, mask)


def _snapshot(path, from_env, tmp):
    """The kubeconfig read once, through a descriptor, into a 0600 file in `tmp`."""
    full = os.path.expanduser(path)
    if not from_env and os.path.realpath(full) == os.path.realpath(os.path.expanduser(cq_remote.DEFAULT_KUBECONFIG)):
        raise AdapterError(f"the kubeconfig is kubectl's default {cq_remote.DEFAULT_KUBECONFIG}; keep the target's in "
                           f"its own file, or name it in {cq_remote.KUBECONFIG_ENV}")
    try:
        fd = os.open(full, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    except OSError:
        raise AdapterError("no kubeconfig at the path in CQ_KUBECONFIG or the profile, or it is a symlink") from None
    st = os.fstat(fd)
    if not stat.S_ISREG(st.st_mode) or st.st_mode & 0o077:
        os.close(fd)
        raise AdapterError("the kubeconfig is not a regular file" if not stat.S_ISREG(st.st_mode) else
                           "the kubeconfig is readable by group or others; chmod 600 it")
    with os.fdopen(fd, "rb") as fh:
        data = fh.read()
    copy = os.path.join(tmp, "kubeconfig")
    with os.fdopen(os.open(copy, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as out:
        out.write(data)
    return copy


def read_key(target, secret, key, environ=None, profile_path=None):
    env = os.environ if environ is None else environ
    if not harden():
        raise AdapterError("this process could not be made undumpable, so another process of this user could read "
                           "the key from its memory")
    profile = cq_remote.load_profile(target, profile_path)
    from_env = bool(env.get(cq_remote.KUBECONFIG_ENV))
    where = f"{secret}/{key} in namespace {profile['namespace']}"
    parent = _key_parent()
    mask = signal.pthread_sigmask(signal.SIG_BLOCK, STOP_SIGNALS)
    held = {}
    old, tmp = _stop_on_signals(held), None
    try:
        tmp = held["folder"] = tempfile.mkdtemp(prefix="cq-key-", dir=parent)
        signal.pthread_sigmask(signal.SIG_SETMASK, mask)
        copy = _snapshot(env.get(cq_remote.KUBECONFIG_ENV) or profile["kubeconfig"], from_env, tmp)
        ctx = cq_remote.Remote({"target": target}, tmp, cq_local.Settings(), stack_mode="reuse", profile=profile,
                               kubeconfig=copy, kubeconfig_from_env=True)
        problems = ctx.kubeconfig_problems()
        if problems:
            raise AdapterError("; ".join(problems))
        encoded = _get_secret(ctx, copy, profile, secret, key, where, tmp)
    finally:
        signal.pthread_sigmask(signal.SIG_BLOCK, STOP_SIGNALS)
        if tmp is not None:
            shutil.rmtree(tmp, ignore_errors=True)
        for sig, handler in old.items():
            signal.signal(sig, handler)
        signal.pthread_sigmask(signal.SIG_SETMASK, mask)
        if tmp is not None and os.path.lexists(tmp):
            raise AdapterError(f"{tmp} could not be removed; remove it by hand")
    try:
        value = base64.b64decode(encoded, validate=True).decode("utf-8")
    except (ValueError, TypeError):
        raise AdapterError(f"{where} is not base64 UTF-8 text") from None
    if any(ord(c) < 0x20 or ord(c) == 0x7F for c in value):
        raise AdapterError(f"{where} contains a control character (NUL, CR, LF or another), which a shell would "
                           "change or drop")
    if len(value) < cq_remote.MIN_SECRET_LEN:
        raise AdapterError(f"{where} is empty or shorter than {cq_remote.MIN_SECRET_LEN} characters")
    if value == profile["dev_secret"]:
        raise AdapterError(f"{where} is the profile's dev_secret, not the deployment's key")
    return value


def _get_secret(ctx, copy, profile, secret, key, where, tmp):
    """The Secret's base64 value, read over HTTPS in this hardened process, so no child process ever holds it."""
    p = ctx.sh(["kubectl", "config", "view", "--raw", "--minify", "--flatten", "-o", "json", "--kubeconfig", copy,
                "--context", ctx.kube_context], cq_remote.KUBECTL_S, check=False, env=ctx.env(KUBERC="off"))
    if p.returncode != 0:
        raise AdapterError(f"kubectl config view --raw exited {p.returncode}")
    try:
        doc = json.loads(p.stdout)
        cluster, user = doc["clusters"][0]["cluster"], doc["users"][0]["user"]
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        if sys.version_info >= (3, 13):
            tls.verify_flags |= ssl.VERIFY_X509_PARTIAL_CHAIN | ssl.VERIFY_X509_STRICT
        tls.load_verify_locations(cadata=base64.b64decode(cluster["certificate-authority-data"]).decode())
    except Exception:  # noqa: BLE001 - nothing from the raw view may reach the output
        raise AdapterError("kubectl config view --raw gave no usable cluster, user or certificate authority") from None
    if cluster.get("server") != profile["kube_server"]:
        raise AdapterError("the kubeconfig's server is not the profile's kube_server")
    headers = {"Accept": "application/json"}
    token = user.get("token")
    if token:
        if not isinstance(token, str) or any(ord(c) < 0x21 or ord(c) == 0x7F for c in token):
            raise AdapterError("the kubeconfig's token has a control character or space")
        headers["Authorization"] = f"Bearer {token}"
    else:
        pair = []
        for name, field in (("client.crt", "client-certificate-data"), ("client.key", "client-key-data")):
            pair.append(os.path.join(tmp, name))
            with os.fdopen(os.open(pair[-1], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as out:
                out.write(base64.b64decode(user.get(field) or ""))
        try:
            tls.load_cert_chain(*pair)
        except Exception:  # noqa: BLE001
            raise AdapterError("the kubeconfig's client certificate or key could not be loaded") from None
    path = (f"/api/v1/namespaces/{urllib.parse.quote(profile['namespace'], safe='')}"
            f"/secrets/{urllib.parse.quote(secret, safe='')}")
    try:
        status, body = _https_get(profile["kube_server"], path, headers, tls)
    except AdapterError:
        raise
    except Exception:  # noqa: BLE001 - an exception's text may carry the response, so it is never shown
        raise AdapterError(f"reading {where} failed") from None
    if status != 200:
        raise AdapterError(f"reading {where} answered HTTP status {status}")
    try:
        return json.loads(body)["data"][key]
    except Exception:  # noqa: BLE001
        raise AdapterError(f"{where} is missing from the Secret") from None


def _https_get(server, path, headers, tls):
    """(status, body) of one GET to exactly `server`: no redirects followed, no proxy, at most MAX_BODY bytes, and
    everything from the name lookup to the last byte inside FETCH_DEADLINE_S."""
    url = urllib.parse.urlsplit(server)
    host, port = url.hostname, url.port or 443
    end = time.monotonic() + FETCH_DEADLINE_S
    sockets, expired = [], threading.Event()

    def left():
        rest = end - time.monotonic()
        if expired.is_set() or rest <= 0:
            raise TimeoutError("the deadline passed")
        return rest

    def cut():
        expired.set()
        for sock in list(sockets):
            try:
                socket.socket.shutdown(sock, socket.SHUT_RDWR)
            except OSError:
                pass
    watchdog = threading.Timer(FETCH_DEADLINE_S, cut)
    conn = http.client.HTTPSConnection(host, port, context=tls)
    try:
        _start_without_stop_signals(watchdog)
        conn.sock = _connect(host, port, tls, left, sockets)
        conn.sock.settimeout(left())
        conn.request("GET", path, headers=headers)
        resp = conn.getresponse()
        if resp.status != 200:
            return resp.status, b""
        body = bytearray()
        while True:
            chunk = resp.read1(64 * 1024)
            if expired.is_set() or not chunk and resp.length:
                raise TimeoutError("the read was cut off")
            if not chunk:
                return 200, bytes(body)
            body += chunk
            if len(body) > MAX_BODY:
                raise AdapterError(f"the Secret's response is larger than {MAX_BODY} bytes")
    finally:
        watchdog.cancel()
        conn.close()
        for sock in sockets:
            sock.close()


def _connect(host, port, tls, left, sockets):
    """A TLS socket to host:port, with the name lookup, the TCP connect and the handshake each bounded by left()."""
    found = []

    def lookup():
        try:
            found.append(socket.getaddrinfo(host, port, type=socket.SOCK_STREAM))
        except Exception as exc:  # noqa: BLE001 - raised below, in the caller's thread
            found.append(exc)
    resolver = threading.Thread(target=lookup, daemon=True)
    _start_without_stop_signals(resolver)
    resolver.join(left())
    if not found:
        raise TimeoutError("the name lookup did not finish")
    if isinstance(found[0], Exception):
        raise found[0]
    for family, kind, proto, _, addr in found[0]:
        raw = socket.socket(family, kind, proto)
        sockets.append(raw)
        raw.settimeout(left())
        try:
            raw.connect(addr)
        except OSError:
            continue
        sock = tls.wrap_socket(raw, server_hostname=host, do_handshake_on_connect=False)
        sockets.append(sock)
        sock.settimeout(left())
        sock.do_handshake()
        return sock
    raise ConnectionError("no address of the API server answered")


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("target")
    p.add_argument("--secret", required=True, help="the Secret's name")
    p.add_argument("--key", required=True, help="the key in that Secret")
    args = p.parse_args(argv)
    if sys.stdout.isatty():
        print("cq-target-key: stdout is a terminal; read the key into a variable: JWT_SECRET=\"$(...)\"",
              file=sys.stderr)
        return EXIT_REFUSED
    try:
        value = read_key(args.target, args.secret, args.key)
    except AdapterError as exc:
        print(f"cq-target-key: {exc}", file=sys.stderr)
        return EXIT_REFUSED
    except Stopped as exc:
        left = exc.folder is not None and os.path.lexists(exc.folder)
        print(f"cq-target-key: stopped by {signal.Signals(exc.signum).name}; "
              + (f"{exc.folder} is left; remove it by hand" if left else "no key folder is left"), file=sys.stderr)
        return 128 + exc.signum
    except KeyboardInterrupt:
        print("cq-target-key: stopped by SIGINT", file=sys.stderr)
        return 128 + signal.SIGINT
    except Exception as exc:  # noqa: BLE001 - its text could carry a credential, so only its type is shown
        print(f"cq-target-key: internal error ({type(exc).__name__})", file=sys.stderr)
        return EXIT_REFUSED
    sys.stdout.write(value)
    return 0


if __name__ == "__main__":
    sys.exit(main())
