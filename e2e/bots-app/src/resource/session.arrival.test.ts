import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import type { ArrivalSpread } from "./arrival";
import type { FpsStats } from "./fps";
import { deriveReport } from "./session";

const META = "meta,1,run,box,100,4,1,1,5";

function rawCsv(epochs: number[]): string {
  const lines = [META];
  for (const [i, epoch] of epochs.entries()) {
    lines.push(`cpu,${epoch},${100 + i * 50},0,0,${900 - i * 50},0,0,0,0`);
    lines.push(`proccount,${epoch},0`);
  }
  return lines.join("\n") + "\n";
}

async function report(
  epochs: number[],
  arrival: ArrivalSpread | null,
  joinedBots: number | null = arrival?.count ?? null,
): Promise<string> {
  const dir = mkdtempSync(join(tmpdir(), "bots-arrival-"));
  const result = await deriveReport({
    rawCsvText: rawCsv(epochs),
    rawCsvPath: join(dir, "raw.csv"),
    derivedCsvPath: join(dir, "derived.csv"),
    reportPath: join(dir, "report.txt"),
    fpsByBot: new Map<string, FpsStats>(),
    arrival,
    joinedBots,
  });
  return result.reportText;
}

describe("deriveReport's arrival spread (#2294)", () => {
  const spread: ArrivalSpread = {
    count: 4,
    firstJoinMs: 1_002_000,
    lastJoinMs: 1_010_000,
    spreadMs: 8_000,
  };

  it("carries the spread it was handed onto the receipt it writes", async () => {
    const text = await report([1000, 1005, 1010, 1015], spread);
    expect(text).toContain("arrival spread: 8.0s across 4 local joins");
    expect(text).toContain("verdict lines below cover that ramp");
  });

  it("reports it untracked when the run handed it none", async () => {
    expect(await report([1000, 1005, 1010, 1015], null)).toContain(
      "arrival spread: not tracked for this report",
    );
  });

  it("banners no-evidence for a sampled run whose bots never joined (#2358)", async () => {
    const text = await report([1000, 1005, 1010, 1015], null, 0);
    expect(text).toContain("RESOURCE_NO_EVIDENCE");
    expect(text).not.toContain("RESOURCE_OK");
    expect(text).toContain("no bot was observed to join");
  });
});
