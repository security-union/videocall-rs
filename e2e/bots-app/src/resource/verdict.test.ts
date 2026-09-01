import { describe, expect, it } from "vitest";

import type { DerivedSample } from "./derive";
import type { FpsStats } from "./fps";
import {
  evaluateVerdict,
  hasSustainedCpuSaturation,
  RESOURCE_CPU_STARVED_PCT,
  RESOURCE_CPU_SUSTAIN_SAMPLES,
  RESOURCE_FPS_BASE_RUNG,
  RESOURCE_FPS_SUSTAIN_MS,
  summarize,
} from "./verdict";

/** Minimal derived sample with a given overall CPU%, defaults elsewhere. */
function mk(cpuBusyPct: number, o: Partial<DerivedSample> = {}): DerivedSample {
  return {
    epoch: o.epoch ?? 1000,
    dtSec: o.dtSec ?? 5,
    cpuBusyPct,
    cpuStealPct: o.cpuStealPct ?? 0,
    perCorePct: o.perCorePct ?? [cpuBusyPct],
    load1: o.load1 ?? 0,
    load5: o.load5 ?? 0,
    load15: o.load15 ?? 0,
    memTotalKb: o.memTotalKb ?? 16000,
    memUsedKb: o.memUsedKb ?? 8000,
    memAvailKb: o.memAvailKb ?? 8000,
    swapUsedKb: o.swapUsedKb ?? 0,
    nicRxBytesPerSec: o.nicRxBytesPerSec ?? 0,
    nicTxBytesPerSec: o.nicTxBytesPerSec ?? 0,
    procCount: o.procCount ?? null,
    procs: o.procs ?? [],
  };
}

const fps = (over: Partial<FpsStats> = {}): FpsStats => ({
  latest: 8,
  min: 8,
  mean: 8,
  count: 10,
  maxSustainedBelowRungMs: 0,
  ...over,
});

describe("hasSustainedCpuSaturation (boundary)", () => {
  it("fires on exactly `window` consecutive samples above threshold", () => {
    const s = [mk(90), mk(90), mk(90)];
    expect(hasSustainedCpuSaturation(s, 85, 3)).toBe(true);
  });

  it("does NOT fire on window-1 consecutive samples (off-by-one guard)", () => {
    const s = [mk(90), mk(90), mk(10), mk(90)];
    expect(hasSustainedCpuSaturation(s, 85, 3)).toBe(false);
  });

  it("uses strict > so a sample exactly at the threshold breaks the run", () => {
    const s = [mk(90), mk(85), mk(90), mk(90)];
    expect(hasSustainedCpuSaturation(s, 85, 3)).toBe(false);
  });

  it("resets the run on a dip and requires a fresh consecutive window", () => {
    const s = [mk(90), mk(90), mk(10), mk(90), mk(90), mk(90)];
    expect(hasSustainedCpuSaturation(s, 85, 3)).toBe(true);
  });

  it("never fires for a non-positive window", () => {
    expect(hasSustainedCpuSaturation([mk(99), mk(99)], 85, 0)).toBe(false);
  });
});

describe("evaluateVerdict — CPU rule", () => {
  it("marks starved on 3 sustained samples over 85% (defaults)", () => {
    const v = evaluateVerdict([mk(86), mk(90), mk(99)], new Map());
    expect(v.starved).toBe(true);
    expect(v.cpuStarved).toBe(true);
    expect(v.fpsStarved).toBe(false);
    expect(v.reasons[0]).toContain("CPU saturated");
  });

  it("does not mark starved on only 2 sustained samples", () => {
    const v = evaluateVerdict([mk(99), mk(99), mk(50), mk(99)], new Map());
    expect(v.starved).toBe(false);
    expect(v.reasons).toHaveLength(0);
  });

  it("honors a custom threshold + window", () => {
    const v = evaluateVerdict([mk(60), mk(60)], new Map(), null, {
      cpuThresholdPct: 50,
      cpuSustainSamples: 2,
    });
    expect(v.cpuStarved).toBe(true);
  });
});

describe("evaluateVerdict — FPS rule", () => {
  it("fps rule fires at exactly the sustain duration, not just below it", () => {
    const at = new Map([["a", fps({ min: 2, maxSustainedBelowRungMs: RESOURCE_FPS_SUSTAIN_MS })]]);
    const below = new Map([
      ["b", fps({ min: 2, maxSustainedBelowRungMs: RESOURCE_FPS_SUSTAIN_MS - 1 })],
    ]);
    const verdict = evaluateVerdict([], at);
    expect(verdict.fpsStarved).toBe(true);
    expect(verdict.reasons[0]).toContain(
      "encoder FPS sustained below base rung 5 for 10.0s (min 2.0)",
    );
    expect(evaluateVerdict([], below).fpsStarved).toBe(false);
  });

  it("does NOT fire for a brief sub-rung blip", () => {
    const map = new Map([["a", fps({ min: 2, maxSustainedBelowRungMs: 2000 })]]);
    expect(evaluateVerdict([], map).fpsStarved).toBe(false);
  });

  it("ignores bots with no readings", () => {
    expect(evaluateVerdict([], new Map([["a", fps({ count: 0 })]])).fpsStarved).toBe(false);
  });

  it("fpsSustainMs override changes the threshold", () => {
    const map = new Map([["a", fps({ min: 2, maxSustainedBelowRungMs: 3000 })]]);
    expect(evaluateVerdict([], map, null, { fpsSustainMs: 3000 }).fpsStarved).toBe(true);
    expect(evaluateVerdict([], map).fpsStarved).toBe(false);
  });

  it("degrades to a CPU-only verdict when no fps was captured", () => {
    const v = evaluateVerdict([mk(99), mk(99), mk(99)], new Map());
    expect(v.starved).toBe(true);
    expect(v.fpsStarved).toBe(false);
  });

  it("is independent — fps starvation trips the verdict even with a healthy CPU", () => {
    const v = evaluateVerdict(
      [mk(10), mk(12)],
      new Map([["bot-a", fps({ min: 1, maxSustainedBelowRungMs: RESOURCE_FPS_SUSTAIN_MS })]]),
    );
    expect(v.starved).toBe(true);
    expect(v.cpuStarved).toBe(false);
    expect(v.fpsStarved).toBe(true);
  });
});

describe("evaluateVerdict — no-evidence rule (#2358)", () => {
  const healthy = [mk(10), mk(12), mk(9)];

  it("refuses a clean verdict when no bot joined", () => {
    const v = evaluateVerdict(healthy, new Map(), 0);
    expect(v.noEvidence).toBe(true);
    expect(v.starved).toBe(false);
    expect(v.reasons.join(" ")).toContain("no bot was observed to join");
  });

  it("refuses a clean verdict when nothing was sampled", () => {
    const v = evaluateVerdict([], new Map(), 2);
    expect(v.noEvidence).toBe(true);
    expect(v.reasons.join(" ")).toContain("no resource samples were derived");
  });

  it("stays clean for a sampled run with joins", () => {
    const v = evaluateVerdict(healthy, new Map(), 1);
    expect(v.noEvidence).toBe(false);
    expect(v.starved).toBe(false);
    expect(v.reasons).toHaveLength(0);
  });

  it("claims nothing when joins are not tracked", () => {
    expect(evaluateVerdict(healthy, new Map(), null).noEvidence).toBe(false);
  });

  it("yields to a fired rule — starvation is evidence", () => {
    const cpu = evaluateVerdict([mk(99), mk(99), mk(99)], new Map(), 0);
    expect(cpu.starved).toBe(true);
    expect(cpu.noEvidence).toBe(false);
    const fpsStarving = new Map([
      ["a", fps({ min: 1, maxSustainedBelowRungMs: RESOURCE_FPS_SUSTAIN_MS })],
    ]);
    const byFps = evaluateVerdict([], fpsStarving, 0);
    expect(byFps.starved).toBe(true);
    expect(byFps.noEvidence).toBe(false);
  });
});

describe("summarize", () => {
  it("returns an all-zero summary for no samples", () => {
    const s = summarize([]);
    expect(s.sampleCount).toBe(0);
    expect(s.cpuPeakPct).toBe(0);
    expect(s.procPeaks).toHaveLength(0);
  });

  it("folds peaks + means across samples", () => {
    const samples = [
      mk(40, {
        cpuStealPct: 1,
        perCorePct: [40, 20],
        load1: 2,
        memUsedKb: 8000,
        memAvailKb: 8000,
        swapUsedKb: 100,
        nicRxBytesPerSec: 1000,
        nicTxBytesPerSec: 500,
        procCount: 4,
        procs: [{ pid: 1, comm: "chrome", cpuPct: 50, rssKb: 300000 }],
      }),
      mk(80, {
        cpuStealPct: 5,
        perCorePct: [95, 60],
        load1: 6,
        memUsedKb: 12000,
        memAvailKb: 4000,
        swapUsedKb: 250,
        nicRxBytesPerSec: 3000,
        nicTxBytesPerSec: 2000,
        procCount: 2,
        procs: [{ pid: 1, comm: "chrome", cpuPct: 180, rssKb: 540000 }],
      }),
    ];
    const s = summarize(samples);
    expect(s.sampleCount).toBe(2);
    expect(s.durationSec).toBe(10);
    expect(s.cpuPeakPct).toBe(80);
    expect(s.cpuMeanPct).toBe(60);
    expect(s.cpuStealPeakPct).toBe(5);
    expect(s.perCorePeakPct).toBe(95);
    expect(s.load1Peak).toBe(6);
    expect(s.memUsedPeakKb).toBe(12000);
    expect(s.memAvailMinKb).toBe(4000);
    expect(s.swapUsedPeakKb).toBe(250);
    expect(s.nicRxPeakBytesPerSec).toBe(3000);
    expect(s.nicTxPeakBytesPerSec).toBe(2000);
    expect(s.procCountPeak).toBe(4);
    expect(s.procCountMin).toBe(2);
    // per-pid peak folds the two ticks of pid 1.
    expect(s.procPeaks).toEqual([{ pid: 1, comm: "chrome", cpuPctPeak: 180, rssKbPeak: 540000 }]);
  });

  it("sorts proc peaks by peak CPU descending", () => {
    const s = summarize([
      mk(10, {
        procs: [
          { pid: 1, comm: "a", cpuPct: 10, rssKb: 1 },
          { pid: 2, comm: "b", cpuPct: 90, rssKb: 1 },
        ],
      }),
    ]);
    expect(s.procPeaks.map((p) => p.pid)).toEqual([2, 1]);
  });
});

describe("exported thresholds", () => {
  it("match the documented defaults", () => {
    expect(RESOURCE_CPU_STARVED_PCT).toBe(85);
    expect(RESOURCE_CPU_SUSTAIN_SAMPLES).toBe(3);
    expect(RESOURCE_FPS_BASE_RUNG).toBe(5);
    expect(RESOURCE_FPS_SUSTAIN_MS).toBe(10000);
  });
});
