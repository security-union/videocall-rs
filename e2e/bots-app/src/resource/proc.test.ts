import { describe, expect, it } from "vitest";

import {
  cpuBusyJiffies,
  cpuPercentBetween,
  cpuTotalJiffies,
  netRateBetween,
  parseCpuJiffies,
  parseRawCsv,
  procCpuPercentBetween,
  type CpuJiffies,
} from "./proc";

const PREV: CpuJiffies = {
  user: 100,
  nice: 0,
  system: 50,
  idle: 800,
  iowait: 50,
  irq: 0,
  softirq: 0,
  steal: 0,
};
const CUR: CpuJiffies = {
  user: 150,
  nice: 0,
  system: 100,
  idle: 850,
  iowait: 50,
  irq: 0,
  softirq: 10,
  steal: 40,
};

describe("parseCpuJiffies", () => {
  it("reads the 8-field jiffies tail", () => {
    expect(parseCpuJiffies(["150", "0", "100", "850", "50", "0", "10", "40"])).toEqual(CUR);
  });

  it("defaults a missing steal field (older kernels) to 0", () => {
    const j = parseCpuJiffies(["150", "0", "100", "850", "50", "0", "10"]);
    expect(j.steal).toBe(0);
  });

  it("degrades a non-numeric field to 0 rather than NaN", () => {
    expect(parseCpuJiffies(["x", "0", "0", "0", "0", "0", "0", "0"]).user).toBe(0);
  });
});

describe("cpu jiffies helpers", () => {
  it("total sums every field", () => {
    expect(cpuTotalJiffies(PREV)).toBe(1000);
    expect(cpuTotalJiffies(CUR)).toBe(1200);
  });
  it("busy excludes idle + iowait", () => {
    expect(cpuBusyJiffies(PREV)).toBe(150);
    expect(cpuBusyJiffies(CUR)).toBe(300);
  });
});

describe("cpuPercentBetween", () => {
  it("computes busy% and steal% from the delta window", () => {
    // totalDelta = 200, busyDelta = 150 → 75%; stealDelta = 40 → 20%.
    expect(cpuPercentBetween(PREV, CUR)).toEqual({ busyPct: 75, stealPct: 20 });
  });

  it("returns 0/0 on a counter reset (negative total delta)", () => {
    expect(cpuPercentBetween(CUR, PREV)).toEqual({ busyPct: 0, stealPct: 0 });
  });

  it("returns 0/0 when the two snapshots are identical (no window)", () => {
    expect(cpuPercentBetween(CUR, CUR)).toEqual({ busyPct: 0, stealPct: 0 });
  });

  it("clamps a >100% arithmetic artifact to 100", () => {
    const prev: CpuJiffies = { ...PREV, idle: 0, iowait: 0 };
    const cur: CpuJiffies = { ...prev, user: prev.user + 100, idle: 0 };
    expect(cpuPercentBetween(prev, cur).busyPct).toBeLessThanOrEqual(100);
  });
});

describe("netRateBetween", () => {
  it("computes bytes/sec over the interval", () => {
    expect(netRateBetween({ rx: 1000, tx: 500 }, { rx: 6000, tx: 2500 }, 5)).toEqual({
      rxBytesPerSec: 1000,
      txBytesPerSec: 400,
    });
  });
  it("returns 0 on a counter reset", () => {
    expect(netRateBetween({ rx: 6000, tx: 0 }, { rx: 1000, tx: 0 }, 5).rxBytesPerSec).toBe(0);
  });
  it("returns 0 for a non-positive interval", () => {
    expect(netRateBetween({ rx: 0, tx: 0 }, { rx: 100, tx: 100 }, 0)).toEqual({
      rxBytesPerSec: 0,
      txBytesPerSec: 0,
    });
  });
});

describe("procCpuPercentBetween", () => {
  it("converts jiffies to a per-core-relative percent (can exceed 100)", () => {
    // deltaJiffies = 1000, clkTck 100 → 10 cpu-seconds over 5s wall = 200%.
    expect(procCpuPercentBetween(100, 1100, 5, 100)).toBe(200);
  });
  it("returns 0 when the process used no CPU in the window", () => {
    expect(procCpuPercentBetween(500, 500, 5, 100)).toBe(0);
  });
  it("returns 0 for a non-positive interval or clkTck", () => {
    expect(procCpuPercentBetween(0, 1000, 0, 100)).toBe(0);
    expect(procCpuPercentBetween(0, 1000, 5, 0)).toBe(0);
  });
});

describe("parseRawCsv", () => {
  const csv = [
    "meta,1,my-run,box-7,100,16,0,1,5",
    "load,1000,1.5,1.2,1.0",
    "cpu,1000,150,0,100,850,50,0,10,40",
    "core,1000,0,150,0,100,850,50,0,10,40",
    "mem,1000,16000000,8000000,2000000,2000000",
    "net,1000,1000,500",
    "proc,1000,4242,chrome,100,20,512000",
    "proccount,1000,1",
    "cpu,1005,300,0,200,1650,100,0,20,80",
    "proc,1005,4242,chrome,600,120,540000",
    "proccount,1005,1",
    "",
  ].join("\n");

  it("captures the meta row (clkTck, sysstat flags)", () => {
    const parsed = parseRawCsv(csv);
    expect(parsed.meta).not.toBeNull();
    expect(parsed.meta?.clkTck).toBe(100);
    expect(parsed.meta?.ncpu).toBe(16);
    expect(parsed.meta?.haveMpstat).toBe(false);
    expect(parsed.meta?.havePidstat).toBe(true);
    expect(parsed.meta?.intervalSec).toBe(5);
  });

  it("groups rows into one sample per epoch, in order", () => {
    const parsed = parseRawCsv(csv);
    expect(parsed.samples.map((s) => s.epoch)).toEqual([1000, 1005]);
    const first = parsed.samples[0];
    expect(first.load).toEqual({ l1: 1.5, l5: 1.2, l15: 1.0 });
    expect(first.cpu).toEqual(CUR);
    expect(first.cores.get(0)).toEqual(CUR);
    expect(first.mem).toEqual({
      totalKb: 16000000,
      availKb: 8000000,
      swapTotalKb: 2000000,
      swapFreeKb: 2000000,
    });
    expect(first.net).toEqual({ rx: 1000, tx: 500 });
    expect(first.procs).toEqual([{ pid: 4242, comm: "chrome", jiffies: 120, rssKb: 512000 }]);
    expect(first.procCount).toBe(1);
  });

  it("flags an unsupported (non-Linux) capture", () => {
    const parsed = parseRawCsv("meta,1,r,mac,100,8,0,0,5\nunsupported,1000,no /proc/stat\n");
    expect(parsed.unsupported).toBe(true);
    expect(parsed.samples).toHaveLength(0);
  });

  it("ignores unknown row types and blank lines (forward compatible)", () => {
    const parsed = parseRawCsv("meta,1,r,h,100,4,1,1,5\nwatch-exit,1000,gone\n\nfuture,1000,x\n");
    expect(parsed.samples).toHaveLength(0);
    expect(parsed.unsupported).toBe(false);
  });
});
