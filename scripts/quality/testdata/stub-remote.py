#!/usr/bin/env python3
"""A stand-in podman, kubectl, curl, git and timedatectl for test_cq_remote.py: records every call, answers from
the world in $STUB_WORLD."""

import base64
import json
import os
import sys
import time

WORLD, LOG = os.environ["STUB_WORLD"], os.environ["STUB_LOG"]


def load():
    with open(WORLD, encoding="utf-8") as fh:
        return json.load(fh)


def save(w):
    with open(WORLD, "w", encoding="utf-8") as fh:
        json.dump(w, fh)


def opt(args, flag):
    return [args[i + 1] for i, a in enumerate(args[:-1]) if a == flag]


def state(c):
    return {"Status": c["status"], "Running": c["status"] == "running", "Paused": False, "OOMKilled": False,
            "Dead": False, "Pid": 0, "ExitCode": c.get("exit_code", 0), "Error": "",
            "StartedAt": "2026-10-14T15:30:00.5+07:00", "FinishedAt": c.get("finished", "0001-01-01T00:00:00Z")}


def curl(w, args):
    url = next(a for a in reversed(args) if a.startswith("http"))
    params = dict(p.split("=", 1) for p in opt(args, "--data-urlencode"))
    query = params.get("query", "")
    for needle, seconds in w.get("curl_sleep", {}).items():
        if needle in url + query:
            time.sleep(seconds)
    if w.get("curl_fail") and any(n in url + query for n in w["curl_fail"]):
        sys.stderr.write("curl: (28) Operation timed out\n")
        return 28
    if "/api/v1/targets" in url:
        status, body = 200, json.dumps({"status": "success", "data": {"activeTargets": w.get("targets", [])}})
    elif "/api/v1/query" in url and query == "time()":
        status, body = 200, json.dumps({"status": "success", "data": {"resultType": "scalar",
                                                                     "result": [w.get("prom_time"), "x"]}})
    elif "/api/v1/query" in url:
        hit = next((r for n, r in w.get("prom", []) if n in query), None)
        if isinstance(hit, dict):
            hit = hit["seq"].pop(0) if len(hit["seq"]) > 1 else hit["seq"][0]
            save(w)
        if isinstance(hit, str):
            status, body = 200, hit
        else:
            status, body = 200, json.dumps({"status": "success", "data": {"resultType": "vector", "result": [
                {"metric": dict(m), "value": [0, v]} for m, v in (hit or [])]}})
    else:
        status, body = w.get("http", {}).get(url, [200, "ok"])
    sys.stdout.write(body + "\n" + str(status))
    return 0


def kubectl(w, args):
    fail = w.get("kubectl_fail", {}).get(args[0])
    if fail:
        sys.stdout.write(fail.get("stdout", ""))
        sys.stderr.write(fail.get("stderr", ""))
        return fail["rc"]
    if args[:3] == ["config", "view", "--raw"]:
        print(json.dumps(w["kubeconfig_raw"]))
    elif args[:2] == ["config", "view"]:
        print(json.dumps(w["kubeconfig_view"]))
    elif args[:2] == ["get", "pods"]:
        print(json.dumps({"items": w.get("pods", [])}))
    else:
        return 1
    return 0


def podman(w, argv):
    cmd, args = argv[0], argv[1:]
    containers = w.setdefault("containers", {})
    if cmd == "version":
        print("5.4.0")
    elif cmd == "image":
        if args[:1] == ["inspect"] and "--format" in args and "revision" in args[args.index("--format") + 1]:
            print(w.get("bot_labels", ""))
            return 0 if w.get("bot_labels") else 1
        if args[:1] == ["inspect"] and "--format" in args:
            print("ab" * 32)
        return 0 if w.get("images", True) else 1
    elif cmd == "ps":
        if "json" in args:
            print(json.dumps(w.get("stale", [])))
        else:
            rid = next(f for f in opt(args, "--filter") if f.startswith("label=cq.run_id=")).split("=", 2)[2]
            print("\n".join(n for n, c in containers.items() if c["labels"].get("cq.run_id") == rid))
    elif cmd == "run":
        if "--rm" in args and "bot --help 2>&1" in args:
            print(w.get("bot_help",
                        "  --video-mode <ekg|replay>\n  --diagnostics <on|off>\n  --deadline <epoch-seconds>"))
            return 0
        if "--rm" in args:
            if "resource-verdict" in args:
                host = next(v.split(":")[0] for v in opt(args, "-v") if ":/host" in v)
                with open(os.path.join(host, "resource-verdict.json"), "w", encoding="utf-8") as fh:
                    json.dump({"ok": True, "window": {"from": float(opt(args, "--from")[0]),
                                                      "to": float(opt(args, "--to")[0])}}, fh)
                return 0
            print(json.dumps(w.get("qdisc", [])))
            return 0
        name = opt(args, "--name")[0]
        containers[name] = {"status": "running", "labels": dict(x.split("=", 1) for x in opt(args, "--label"))}
        save(w)
        print("c0ffee")
    elif cmd == "inspect":
        if w.get("inspect_fail"):
            sys.stderr.write("Error: cannot connect to Podman\n")
            return 125
        if "cq.proc_id" in args[args.index("--format") + 1]:
            for n in args[args.index("--format") + 2:]:
                print(f"{n}\t{containers[n]['labels'].get('cq.proc_id', '')}\tab12")
            return 0
        missing = False
        for n in args[args.index("--format") + 2:]:
            if n in containers:
                print(json.dumps(n) + "\t" + json.dumps(state(containers[n])))
            else:
                missing = True
        if missing:
            sys.stderr.write(f"Error: no such container {n}\n")
            return 125
    elif cmd in ("stop", "kill"):
        for n in [a for a in args if a.startswith("cq-")]:
            if n in containers and not (cmd == "stop" and n in w.get("stuck", [])) \
                    and n not in w.get("unkillable", []):
                containers[n].update(status="exited", finished=w.get("finished", "2026-10-14T15:40:00.25+07:00"),
                                     exit_code=0 if cmd == "stop" else 143)
        save(w)
        if cmd == "kill" and set(args) & set(w.get("unkillable", [])):
            sys.stderr.write("Error: cannot kill container\n")
            return 125
    elif cmd == "exec":
        if "sh" in args:
            print("\n".join(w.get("env_hits", []) + (["done"] if w.get("env_done", True) else [])))
        else:
            print(json.dumps(w.get("ctl", {}).get(args[-2], {"status": 404, "body": "{}"})))
    elif cmd == "logs":
        seg = [base64.urlsafe_b64encode(x).rstrip(b"=").decode() for x in (b'{"alg":"HS256"}', b'{"sub":"bot"}',
                                                                          b"signature-bytes")]
        print(w.get("log_text", "joined ?" + "token=" + ".".join(seg)))
    elif cmd == "rm":
        for n in args[1:]:
            containers.pop(n, None)
        save(w)
    return 0


def main(argv):
    w = load()
    prog = os.path.basename(sys.argv[0])
    with open(LOG, "a", encoding="utf-8") as fh:
        fh.write(json.dumps({"prog": prog, "argv": argv, "own_session": os.getsid(0) == os.getpid(),
                             "env_jwt": "JWT_SECRET" in os.environ, "env_kubeconfig": "KUBECONFIG" in os.environ,
                             "kuberc": os.environ.get("KUBERC"), "env_token": "BOT_CTL_TOKEN" in os.environ,
                             "env_proxy": sorted(k for k in os.environ if k.upper().endswith("_PROXY")),
                             "env_values": [k for k, v in os.environ.items() if w.get("secret") and w["secret"] in v],
                             "kubeconfig_text": next((open(argv[i + 1], encoding="utf-8").read()
                                                      for i, a in enumerate(argv[:-1]) if a == "--kubeconfig"
                                                      and os.path.isfile(argv[i + 1])), None)})
                 + "\n")
    if prog == "git":
        print(w.get("git_status", "") if "status" in argv else "f" * 40)
        return 0
    if prog == "timedatectl":
        print("yes")
        return 0
    if prog == "loginctl":
        print(w.get("linger", "yes"))
        return 0
    return {"curl": curl, "kubectl": kubectl, "podman": podman}[prog](w, argv)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
