import { describe, expect, it } from "vitest";

import { deriveSamples } from "./derive";
import { parseRawCsv } from "./proc";

/** Build a raw CSV from tick specs so the pair-walking is exercised end-to-end. */
function rawCsv(
  meta: string,
  ticks: Array<{
    epoch: number;
    cpu: number[];
    core0?: number[];
    net?: [number, number];
    mem?: [number, number, number, number];
    procs?: Array<[number, string, number, number, number]>;
  }>,
): string {
  const lines = [meta];
  for (const t of ticks) {
    lines.push(`cpu,${t.epoch},${t.cpu.join(",")}`);
    if (t.core0) lines.push(`core,${t.epoch},0,${t.core0.join(",")}`);
    if (t.mem) lines.push(`mem,${t.epoch},${t.mem.join(",")}`);
    if (t.net) lines.push(`net,${t.epoch},${t.net[0]},${t.net[1]}`);
    for (const p of t.procs ?? []) lines.push(`proc,${t.epoch},${p.join(",")}`);
    lines.push(`proccount,${t.epoch},${(t.procs ?? []).length}`);
  }
  return lines.join("\n") + "\n";
}

const META = "meta,1,run,box,100,4,1,1,5";

describe("deriveSamples", () => {
  it("yields N-1 derived samples (the first tick is the baseline)", () => {
    const csv = rawCsv(META, [
      { epoch: 1000, cpu: [100, 0, 50, 800, 50, 0, 0, 0] },
      { epoch: 1005, cpu: [150, 0, 100, 850, 50, 0, 10, 40] },
    ]);
    const derived = deriveSamples(parseRawCsv(csv));
    expect(derived).toHaveLength(1);
    expect(derived[0].epoch).toBe(1005);
    expect(derived[0].dtSec).toBe(5);
    expect(derived[0].cpuBusyPct).toBe(75);
    expect(derived[0].cpuStealPct).toBe(20);
  });

  it("returns an empty array for a single tick (no window to diff)", () => {
    const csv = rawCsv(META, [{ epoch: 1000, cpu: [1, 0, 1, 1, 0, 0, 0, 0] }]);
    expect(deriveSamples(parseRawCsv(csv))).toHaveLength(0);
  });

  it("returns an empty array for an unsupported capture", () => {
    const parsed = parseRawCsv("meta,1,r,mac,100,8,0,0,5\nunsupported,1,x\n");
    expect(deriveSamples(parsed)).toHaveLength(0);
  });

  it("matches per-process CPU by pid and treats a newly-spawned pid as 0%", () => {
    const csv = rawCsv(META, [
      {
        epoch: 1000,
        cpu: [100, 0, 0, 900, 0, 0, 0, 0],
        procs: [[4242, "chrome", 100, 0, 500000]],
      },
      {
        epoch: 1005,
        cpu: [200, 0, 0, 1800, 0, 0, 0, 0],
        // 4242 kept running (+1000 jiffies → 200%); 4243 is brand new.
        procs: [
          [4242, "chrome", 1100, 0, 520000],
          [4243, "chrome", 50, 0, 100000],
        ],
      },
    ]);
    const [d] = deriveSamples(parseRawCsv(csv));
    const byPid = new Map(d.procs.map((p) => [p.pid, p]));
    expect(byPid.get(4242)?.cpuPct).toBe(200);
    expect(byPid.get(4243)?.cpuPct).toBe(0);
  });

  it("derives NIC bytes/sec and memory used/avail", () => {
    const csv = rawCsv(META, [
      { epoch: 1000, cpu: [1, 0, 0, 9, 0, 0, 0, 0], net: [1000, 500], mem: [16000, 8000, 0, 0] },
      { epoch: 1005, cpu: [2, 0, 0, 18, 0, 0, 0, 0], net: [6000, 2500], mem: [16000, 4000, 0, 0] },
    ]);
    const [d] = deriveSamples(parseRawCsv(csv));
    expect(d.nicRxBytesPerSec).toBe(1000);
    expect(d.nicTxBytesPerSec).toBe(400);
    expect(d.memUsedKb).toBe(12000);
    expect(d.memAvailKb).toBe(4000);
  });

  it("falls back to the meta interval when the epoch delta is non-positive", () => {
    const csv = rawCsv(META, [
      { epoch: 1000, cpu: [0, 0, 0, 100, 0, 0, 0, 0], net: [0, 0] },
      { epoch: 1000, cpu: [0, 0, 0, 200, 0, 0, 0, 0], net: [500, 0] }, // same epoch
    ]);
    // Same epoch collapses to ONE sample in parseRawCsv, so craft distinct
    // epochs that step backwards instead to exercise the dt fallback.
    const back = rawCsv(META, [
      { epoch: 1005, cpu: [0, 0, 0, 100, 0, 0, 0, 0], net: [0, 0] },
      { epoch: 1000, cpu: [0, 0, 0, 200, 0, 0, 0, 0], net: [2500, 0] },
    ]);
    void csv;
    const [d] = deriveSamples(parseRawCsv(back));
    expect(d.dtSec).toBe(5); // meta interval fallback
    expect(d.nicRxBytesPerSec).toBe(500); // 2500 bytes / 5s
  });
});
