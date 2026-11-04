#!/usr/bin/env python3
"""Measure the probes' CPU cost on the load host from a run folder, for a target profile's probe_cores (#2914).

  cq_probe_cost.py RUN_DIR

Reads host/host-raw.csv and the hold from events.jsonl. Exit 0 with the result, 3 when the data is missing.
"""

import argparse
import collections
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import cq_collect  # noqa: E402
import cq_local  # noqa: E402

EXIT_REFUSED = 3
PROBE_COMM = re.compile(rf"chrom.*|node|tsx|{cq_local.PROBE_NODE_COMM}")


class Refused(Exception):
    pass


def hold_window(run_dir):
    try:
        events = cq_collect.read_events(os.path.join(run_dir, cq_collect.EVENTS_FILE))
    except cq_collect.CollectError as exc:
        raise Refused(f"cannot read {cq_collect.EVENTS_FILE}: {exc}") from None
    marks = {e["type"]: e["wall"] for e in events if e["type"] in ("hold_start", "hold_end")}
    if set(marks) != {"hold_start", "hold_end"}:
        raise Refused(f"{cq_collect.EVENTS_FILE} has no complete hold")
    return marks["hold_start"], marks["hold_end"]


def probe_count(run_dir):
    try:
        with open(os.path.join(run_dir, cq_collect.PLAN_FILE), encoding="utf-8") as fh:
            return sum(p["fleet"] == "browser" for p in json.load(fh)["processes"])
    except (OSError, ValueError, KeyError, TypeError):
        return None


def read_samples(path):
    """(CLK_TCK, ncpu, {tick epoch s: {pid: utime + stime}}) for the probe's processes."""
    clk = ncpu = None
    ticks = collections.defaultdict(dict)
    try:
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                f = line.rstrip("\n").split(",")
                if f[0] == "meta" and len(f) >= 6:
                    clk, ncpu = int(f[4]), int(f[5])
                elif f[0] == "proc" and len(f) >= 7 and PROBE_COMM.fullmatch(f[3]):
                    ticks[int(f[1])][f[2]] = int(f[4]) + int(f[5])
    except (OSError, ValueError) as exc:
        raise Refused(f"cannot read {path}: {exc}") from None
    if clk is None or clk <= 0:
        raise Refused(f"{path} has no meta row with a positive CLK_TCK")
    return clk, ncpu, ticks


def cores_by_interval(clk, ticks, start, end):
    """Cores the probe processes used over each pair of consecutive samples inside [start, end]."""
    times = sorted(t for t in ticks if start <= t <= end)
    out = []
    for t1, t2 in zip(times, times[1:]):
        both = ticks[t2].keys() & ticks[t1].keys()
        if not both:
            continue
        used = sum(max(0, ticks[t2][pid] - ticks[t1][pid]) for pid in both)
        out.append(used / (clk * (t2 - t1)))
    return out


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("run_dir")
    args = p.parse_args(argv)
    try:
        start, end = hold_window(args.run_dir)
        clk, ncpu, ticks = read_samples(os.path.join(args.run_dir, "host", "host-raw.csv"))
        cores = cores_by_interval(clk, ticks, start, end)
        if not cores:
            raise Refused("no probe process was in two consecutive samples inside the hold")
    except Refused as exc:
        print(f"cq-probe-cost: {exc}", file=sys.stderr)
        return EXIT_REFUSED
    n = probe_count(args.run_dir)
    per = f"; the peak over {n} probes, {max(cores) / n:.2f} cores each" if n and n > 1 else ""
    print(f"probes' CPU over the hold ({len(cores)} intervals, CLK_TCK {clk}, {ncpu} CPUs): peak {max(cores):.2f} "
          f"cores, mean {sum(cores) / len(cores):.2f} cores; {n if n is not None else 'unknown'} probe(s){per}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
