import { describe, expect, it } from "vitest";

import type { FpsStats } from "./fps";
import {
  formatResourceReport,
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
    verdict: { starved: false, reasons: [], cpuStarved: false, fpsStarved: false },
    fpsByBot: new Map<string, FpsStats>(),
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
    const text = formatResourceReport(base({ supported: false }));
    expect(text).toContain(RESOURCE_OK_BANNER);
    expect(text).toContain("unsupported");
  });
});
