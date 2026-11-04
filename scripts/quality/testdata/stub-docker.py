#!/usr/bin/env python3
"""A stand-in `docker` and `git` for test_cq_local.py: records every argv and answers from the world in $STUB_WORLD."""

import json
import os
import sys
import time

WORLD, LOG = os.environ["STUB_WORLD"], os.environ["STUB_LOG"]
PINNED = ("VIDEOCALL_RELEASE_BUILD", "DIOXUS_SERVE_MODE", "WT_DEV_CERT_HASH_INJECT")


def load():
    with open(WORLD, encoding="utf-8") as fh:
        return json.load(fh)


def save(w):
    with open(WORLD, "w", encoding="utf-8") as fh:
        json.dump(w, fh)


def opt(args, flag):
    return [args[i + 1] for i, a in enumerate(args[:-1]) if a == flag]


def state(c):
    return {"Status": c["status"], "Running": c["status"] == "running", "ExitCode": c.get("exit_code", 0),
            "StartedAt": "2026-10-07T09:00:00.5Z", "FinishedAt": c.get("finished", "0001-01-01T00:00:00Z"),
            "OOMKilled": False, "Error": c.get("error", "")}


def git(w, args):
    if "status" in args:
        print(w.get("git_status", ""))
    elif "rev-parse" in args:
        print(w.get("head", "f" * 40))
    return 0


def main(argv):
    w = load()
    prog = os.path.basename(sys.argv[0])
    with open(LOG, "a", encoding="utf-8") as fh:
        fh.write(json.dumps({"prog": prog, "argv": argv, "own_session": os.getsid(0) == os.getpid(),
                             "env_token": "BOT_CTL_TOKEN" in os.environ, "env_jwt": "JWT_SECRET" in os.environ,
                             "env": {k: os.environ.get(k) for k in PINNED}}) + "\n")
    if prog == "git":
        return git(w, argv)
    cmd, args = argv[0], argv[1:]
    if w.get("sleep", {}).get(cmd):
        time.sleep(w["sleep"][cmd])
    if w.get("fail", {}).get(cmd):
        sys.stderr.write(w["fail"][cmd] + "\n")
        return 1
    containers = w.setdefault("containers", {})
    if cmd == "version":
        print("27.0.0")
    elif cmd == "info":
        print(w.get("docker_os", "Docker Desktop"))
    elif cmd == "image":
        if args[:1] == ["ls"]:
            print("\n".join(w.get("bot_tags", [])))
            return 0
        if args[:1] == ["prune"]:
            return 0
        if "--format" in args and "revision" in args[args.index("--format") + 1]:
            if args[-1] not in w.get("bot_labels", {}):
                return 1
            print(w["bot_labels"][args[-1]])
        elif "--format" in args:
            print("sha256:" + "ab" * 32)
        return 0 if w.get("images", True) else 1
    elif cmd == "ps":
        filters = opt(args, "--filter")
        fmt = (opt(args, "--format") or [""])[0]
        if any(f.startswith("label=cq.run_id=") for f in filters):
            rid = next(f for f in filters if f.startswith("label=cq.run_id=")).split("=", 2)[2]
            print("\n".join(n for n, c in containers.items() if c["labels"].get("cq.run_id") == rid))
        elif "label=cq.run_id" in filters:
            print("\n".join(w.get("stale", []) + (w.get("stale_exited", []) if "-a" in args else [])))
        elif "Ports" in fmt:
            print("\n".join(r for r in w.get("ports_lines", []) if "-a" in args or "\trunning\t" in r))
        elif "working_dir" in fmt:
            print("\n".join(w.get("working_dirs", [])))
        else:
            print("\n".join(w.get("project_ids", []) + (w.get("stopped_ids", []) if "-a" in args else [])))
    elif cmd == "run":
        if "--rm" in args:
            if "resource-verdict" in args:
                host = next(v.split(":")[0] for v in opt(args, "-v") if v.endswith(":/host"))
                if "verdict" in w:
                    with open(os.path.join(host, "resource-verdict.json"), "w", encoding="utf-8") as fh:
                        json.dump(dict(w["verdict"], window={"from": float(opt(args, "--from")[0]),
                                                             "to": float(opt(args, "--to")[0])}), fh)
                return w.get("verdict_rc", 0)
            print(json.dumps(w.get("qdisc", [])))
            return 0
        name = opt(args, "--name")[0]
        labels = dict(lab.split("=", 1) for lab in opt(args, "--label"))
        containers[name] = {"status": "running", "labels": labels, "argv": argv}
        save(w)
        print("c0ffee" + name)
    elif cmd == "inspect":
        if "--format" in args and "cq.proc_id" in args[args.index("--format") + 1]:
            for n in args[args.index("--format") + 2:]:
                print(f"/{n}\t{containers[n]['labels'].get('cq.proc_id', '')}\tsha256:{n}")
            return 0
        if "--format" in args and "Config.Env" in args[args.index("--format") + 1]:
            for svc, env in w.get("stack_env", {}).items():
                print(f"{svc}\t{json.dumps(env)}")
            return 0
        if "--format" in args and "Config.Labels}}" in args[args.index("--format") + 1]:
            for line in w.get("stack_state", []):
                print(line)
            return 0
        names = [n.lstrip("/") for n in args[args.index("--format") + 2:]]
        missing = False
        for n in names:
            if n in containers:
                print(json.dumps("/" + n) + "\t" + json.dumps(state(containers[n])))
            else:
                missing = True
        if missing:
            sys.stderr.write("Error: No such object\n")
            return 1
    elif cmd in ("stop", "kill"):
        names = args[2:] if cmd == "stop" else args[2:]
        for n in names:
            if n in containers and containers[n]["status"] == "running" and n not in w.get("stuck", []):
                containers[n].update(status="exited", finished=w.get("finished_at", {}).get(n, "2026-10-07T09:30:00.25Z"),
                                     exit_code=0 if cmd == "stop" else 143)
        save(w)
    elif cmd == "exec":
        path = args[-2]
        reply = w.get("ctl", {}).get(path, {"status": 404, "body": "{}"})
        print(json.dumps(reply))
    elif cmd == "logs":
        print(w.get("log_text", "joined token=abc.def-123 and Bearer s3cr3t"))
    elif cmd == "rm":
        for n in args[1:]:
            containers.pop(n, None)
        save(w)
    elif cmd == "rmi":
        return 0
    elif cmd == "compose":
        if "ps" in args:
            print("websocket-api\tvideocall-scale-websocket-api-1\trunning\tUp 5 minutes")
        elif "logs" in args:
            print("relay up token=zzz")
    elif cmd == "build":
        return w.get("build_rc", 0)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
