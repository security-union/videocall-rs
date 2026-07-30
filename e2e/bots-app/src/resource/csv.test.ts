import { describe, expect, it } from "vitest";

import {
  DERIVED_CSV_HEADER,
  formatDerivedCsv,
  formatDerivedRow,
  formatProcsCell,
  round1,
} from "./csv";
import type { DerivedSample } from "./derive";

const SAMPLE: DerivedSample = {
  epoch: 1005,
  dtSec: 5,
  cpuBusyPct: 87.34,
  cpuStealPct: 12.5,
  perCorePct: [90.1, 84.9],
  load1: 3.25,
  load5: 2.1,
  load15: 1.05,
  memTotalKb: 16000000,
  memUsedKb: 12000000,
  memAvailKb: 4000000,
  swapUsedKb: 250000,
  nicRxBytesPerSec: 1250000.7,
  nicTxBytesPerSec: 640000.2,
  procCount: 2,
  procs: [
    { pid: 4242, comm: "chrome", cpuPct: 190.4, rssKb: 540000 },
    { pid: 4243, comm: "chrome", cpuPct: 5.02, rssKb: 100000 },
  ],
};

describe("round1", () => {
  it("rounds to one decimal and keeps integers integer-looking", () => {
    expect(round1(87.34)).toBe(87.3);
    expect(round1(5)).toBe(5);
    expect(round1(190.45)).toBe(190.5);
  });
});

describe("DERIVED_CSV_HEADER", () => {
  it("is a stable, comma-joined column list", () => {
    expect(DERIVED_CSV_HEADER).toBe(
      "epoch,dt_sec,cpu_busy_pct,cpu_steal_pct,per_core_pct,load1,load5,load15," +
        "mem_total_kb,mem_used_kb,mem_avail_kb,swap_used_kb," +
        "nic_rx_bytes_per_sec,nic_tx_bytes_per_sec,proc_count,procs",
    );
  });
});

describe("formatProcsCell", () => {
  it("packs comm:pid:cpu:rss joined by ;", () => {
    expect(formatProcsCell(SAMPLE.procs)).toBe("chrome:4242:190.4:540000;chrome:4243:5:100000");
  });

  it("sanitizes separators out of a hostile comm so it cannot inject fields", () => {
    expect(formatProcsCell([{ pid: 1, comm: "a;b:c,d", cpuPct: 1, rssKb: 2 }])).toBe(
      "a_b_c_d:1:1:2",
    );
  });
});

describe("formatDerivedRow", () => {
  it("emits epoch-first, one-decimal values, ints for bytes, and the packed procs cell", () => {
    expect(formatDerivedRow(SAMPLE)).toBe(
      "1005,5,87.3,12.5,90.1;84.9,3.3,2.1,1.1,16000000,12000000,4000000,250000," +
        "1250001,640000,2,chrome:4242:190.4:540000;chrome:4243:5:100000",
    );
  });

  it("renders a null proc_count as an empty cell", () => {
    const row = formatDerivedRow({ ...SAMPLE, procCount: null, procs: [] });
    // trailing proc_count + procs are both empty → row ends with two commas.
    expect(row.endsWith(",,")).toBe(true);
  });
});

describe("formatDerivedCsv", () => {
  it("prepends the header and terminates with a newline", () => {
    const out = formatDerivedCsv([SAMPLE]);
    const lines = out.split("\n");
    expect(lines[0]).toBe(DERIVED_CSV_HEADER);
    expect(lines[1]).toBe(formatDerivedRow(SAMPLE));
    expect(out.endsWith("\n")).toBe(true);
  });
});
