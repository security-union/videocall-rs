import { describe, expect, it } from "vitest";

import type { ArrivalSpread } from "./arrival";
import type { FpsStats } from "./fps";
import {
  bannerFor,
  formatResourceReport,
  RESOURCE_NO_EVIDENCE_BANNER,
  RESOURCE_OK_BANNER,
  RESOURCE_STARVED_BANNER,
  type ReportInput,
} from "./report";
import type { ResourceSummary, ResourceVerdict } from "./verdict";

const SUMMARY: ResourceSummary = {
  sampleCount: 12,
  durationSec: 60,
  cpuPeakPct: 96.4,
  cpuMeanPct: 88.1,
  cpuStealPeakPct: 3.2,
  perCorePeakPct: 99.9,
  load1Peak: 18.2,
  memTotalKb: 16000000,
  memUsedPeakKb: 12000000,
  memAvailMinKb: 4000000,
  swapUsedPeakKb: 250000,
  nicRxPeakBytesPerSec: 1250000,
  nicTxPeakBytesPerSec: 640000,
  procCountPeak: 4,
  procCountMin: 4,
  procPeaks: [{ pid: 4242, comm: "chrome", cpuPctPeak: 190.4, rssKbPeak: 540000 }],
};

function base(over: Partial<ReportInput> = {}): ReportInput {
  return {
    summary: SUMMARY,
    verdict: {
      starved: false,
      reasons: [],
      cpuStarved: false,
      fpsStarved: false,
      noEvidence: false,
    },
    fpsByBot: new Map<string, FpsStats>(),
    arrival: null,
    supported: true,
    sysstatMissing: false,
    rawCsvPath: "/run/resource/x-raw.csv",
    derivedCsvPath: "/run/resource/x-derived.csv",
    ...over,
  };
}

describe("formatResourceReport", () => {
  it("leads with RESOURCE_STARVED and lists each reason when starved", () => {
    const verdict: ResourceVerdict = {
      starved: true,
      cpuStarved: true,
      fpsStarved: true,
      noEvidence: false,
      reasons: [
        "CPU saturated: 3+ consecutive samples above 85% overall (peak 96.4%)",
        "bot x fell",
      ],
    };
    const text = formatResourceReport(base({ verdict }));
    const firstMeaningful = text.split("\n").find((l) => l.includes("RESOURCE_"));
    expect(firstMeaningful).toBe(RESOURCE_STARVED_BANNER);
    expect(text).toContain("CPU saturated");
    expect(text).toContain("confounded by box saturation");
  });

  it("leads with RESOURCE_OK for a healthy run", () => {
    const text = formatResourceReport(base());
    expect(text).toContain(RESOURCE_OK_BANNER);
    expect(text).not.toContain(RESOURCE_STARVED_BANNER);
  });

  it("notes the missing sysstat tools when absent", () => {
    expect(formatResourceReport(base({ sysstatMissing: true }))).toContain("sysstat");
  });

  it("states fps was not reported when no bot fps was captured", () => {
    const text = formatResourceReport(base());
    expect(text).toContain("bot encoder fps: not reported");
    // The remediation must point at the current mechanism (the window global),
    // not the retired console bridge (#2062).
    expect(text).toContain("window.__videocall_encoder_fps");
    expect(text).not.toContain("console bridge");
  });

  it("prints per-bot fps when captured", () => {
    const fpsByBot = new Map<string, FpsStats>([
      ["bot-a", { latest: 4, min: 2, mean: 3, count: 5, maxSustainedBelowRungMs: 3000 }],
    ]);
    const text = formatResourceReport(base({ fpsByBot }));
    expect(text).toContain("bot bot-a encoder fps: min 2.0");
  });

  it("flags a renderer crash when the matched-process count dropped", () => {
    const summary = { ...SUMMARY, procCountPeak: 4, procCountMin: 2 };
    expect(formatResourceReport(base({ summary }))).toContain("possible renderer crash");
  });

  it("reports gracefully on an unsupported (non-Linux) box", () => {
    const verdict: ResourceVerdict = {
      starved: false,
      cpuStarved: false,
      fpsStarved: false,
      noEvidence: true,
      reasons: ["no resource samples were derived, so the box was never measured"],
    };
    const text = formatResourceReport(base({ supported: false, verdict }));
    expect(text).toContain(RESOURCE_NO_EVIDENCE_BANNER);
    expect(text).not.toContain(RESOURCE_OK_BANNER);
    expect(text).toContain("unsupported");
  });

  describe("a run with nothing to judge (#2358)", () => {
    const emptyVerdict: ResourceVerdict = {
      starved: false,
      cpuStarved: false,
      fpsStarved: false,
      noEvidence: true,
      reasons: ["no bot was observed to join, so nothing loaded the box"],
    };

    it("banners no-evidence, never OK, and gives the reason", () => {
      const text = formatResourceReport(base({ verdict: emptyVerdict }));
      const firstMeaningful = text.split("\n").find((l) => l.includes("RESOURCE_"));
      expect(firstMeaningful).toBe(RESOURCE_NO_EVIDENCE_BANNER);
      expect(text).not.toContain(RESOURCE_OK_BANNER);
      expect(text).not.toContain(RESOURCE_STARVED_BANNER);
      expect(text).toContain("no bot was observed to join");
      expect(text).toContain("No figure from THIS run is representative");
    });

    it("banners zero captured samples as no-evidence, not OK", () => {
      const text = formatResourceReport(
        base({ summary: { ...SUMMARY, sampleCount: 0 }, verdict: emptyVerdict }),
      );
      expect(text).toContain(RESOURCE_NO_EVIDENCE_BANNER);
      expect(text).not.toContain(RESOURCE_OK_BANNER);
    });

    const fpsOnlyStarved: ResourceVerdict = {
      starved: true,
      cpuStarved: false,
      fpsStarved: true,
      noEvidence: false,
      reasons: ["bot bot-a encoder FPS sustained below base rung 5 for 12.0s (min 1.0)"],
    };
    const NO_HOST_CAPTURE: Array<[string, Partial<ReportInput>]> = [
      ["unsupported box", { supported: false }],
      ["zero samples", { summary: { ...SUMMARY, sampleCount: 0 } }],
    ];

    it.each(NO_HOST_CAPTURE)("still banners a fired rule on the %s path", (_l, over) => {
      const text = formatResourceReport(base({ verdict: fpsOnlyStarved, ...over }));
      const firstMeaningful = text.split("\n").find((l) => l.includes("RESOURCE_"));
      expect(firstMeaningful).toContain(RESOURCE_STARVED_BANNER);
      expect(text).not.toContain(RESOURCE_NO_EVIDENCE_BANNER);
      expect(text).toContain("encoder FPS sustained below base rung");
    });

    it.each(NO_HOST_CAPTURE)("keeps the host-capture note on the %s path", (_l, over) => {
      const text = formatResourceReport(base({ verdict: fpsOnlyStarved, ...over }));
      expect(text).toMatch(/RESOURCE_STARVED \((resource capture unsupported|no resource samples)/);
    });

    it("lets a fired rule outrank no-evidence", () => {
      expect(bannerFor({ ...emptyVerdict, starved: true, cpuStarved: true })).toBe(
        RESOURCE_STARVED_BANNER,
      );
    });
  });

  describe("arrival spread (#2294)", () => {
    const T0 = Date.UTC(2026, 7, 18, 22, 0, 0);
    const spread = (count: number, spreadMs: number): ArrivalSpread => ({
      count,
      firstJoinMs: T0,
      lastJoinMs: T0 + spreadMs,
      spreadMs,
    });
    const RAMP_NOTE =
      "the capture starts before the first launch, so the CPU, RAM, NIC, process and verdict" +
      " lines below cover that ramp";
    const NO_ROOM_WINDOW = "does not state how long the room held every bot";
    const PRE_JOIN_NOTE =
      "the capture starts before the launch, so the CPU, RAM, NIC, process and verdict" +
      " lines below include the pre-join stretch";
    const ramped = base({ arrival: spread(8, 61_000) });

    it("prints the first→last window, the local join count and the SSH exclusion", () => {
      const text = formatResourceReport(base({ arrival: spread(8, 61_500) }));
      expect(text).toContain(
        "arrival spread: 61.5s across 8 local joins" +
          " (first 2026-08-18T22:00:00.000Z → last 2026-08-18T22:01:01.500Z);" +
          " SSH-launched bots join in their own process and are not counted",
      );
    });

    it("scopes the ramp caveat to the box figures and the verdict, and banners nothing", () => {
      const lines = formatResourceReport(ramped).split("\n");
      expect(lines.filter((l) => l.includes(RAMP_NOTE))).toHaveLength(1);
      expect(lines.filter((l) => l.includes(RESOURCE_STARVED_BANNER))).toEqual([]);
    });

    it("puts the caveat above every figure it describes", () => {
      const lines = formatResourceReport(ramped).split("\n");
      const at = (needle: string): number => lines.findIndex((l) => l.includes(needle));
      expect(at(RAMP_NOTE)).toBeGreaterThanOrEqual(0);
      for (const below of ["run resource capture", "[resource] CPU:", "VERDICT:"]) {
        expect(at(below)).toBeGreaterThan(at(RAMP_NOTE));
      }
    });

    it("declines to state the full-room window, on the same line as the ramp caveat", () => {
      const lines = formatResourceReport(ramped).split("\n");
      const hits = lines.filter((l) => l.includes(NO_ROOM_WINDOW));
      expect(hits).toHaveLength(1);
      expect(hits[0], "the disclosure split off onto a line of its own").toContain(RAMP_NOTE);
    });

    it("treats the smallest real fleet ramp — two joins — as a spread, not n/a", () => {
      const text = formatResourceReport(base({ arrival: spread(2, 8_000) }));
      expect(text).toContain("arrival spread: 8.0s across 2 local joins");
      expect(text).toContain(RAMP_NOTE);
      expect(text).not.toContain("n/a");
    });

    it("refuses a single join as a fleet ramp, but still flags its lead-in", () => {
      const text = formatResourceReport(base({ arrival: spread(1, 0) }));
      expect(text).toContain("arrival spread: n/a — only 1 local join observed");
      expect(text).toContain("#2337");
      expect(text).not.toContain(RAMP_NOTE);
      // One bot per pod takes this branch, and prints aggregates below it.
      expect(text).toContain(PRE_JOIN_NOTE);
      // A pod cannot see the room at all, so the room-window disclosure would be noise here.
      expect(text).not.toContain(NO_ROOM_WINDOW);
    });

    it("says the spread was not tracked, naming no path, when no join was recorded", () => {
      const text = formatResourceReport(base());
      expect(text).toContain("arrival spread: not tracked for this report");
      expect(text).not.toContain(RAMP_NOTE);
    });

    const STUBS: Array<[string, Partial<ReportInput>]> = [
      ["unsupported box", { supported: false }],
      ["zero samples", { summary: { ...SUMMARY, sampleCount: 0 } }],
    ];

    it.each(STUBS)("claims nothing about aggregates on the %s stub", (_l, over) => {
      const text = formatResourceReport({ ...ramped, ...over });
      expect(text).toContain("arrival spread: 61.0s across 8 local joins");
      expect(text).not.toContain(RAMP_NOTE);
      expect(text).not.toContain(NO_ROOM_WINDOW);
    });

    it.each(STUBS)("omits the lead-in note for a single join on the %s stub", (_l, over) => {
      const text = formatResourceReport({ ...base({ arrival: spread(1, 0) }), ...over });
      expect(text).toContain("only 1 local join observed");
      expect(text).not.toContain(PRE_JOIN_NOTE);
    });
  });
});
