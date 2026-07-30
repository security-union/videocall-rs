/**
 * Derived-CSV formatting for the resource capture (issue 2032).
 *
 * The raw sampler CSV carries cumulative /proc counters; at run end the TS layer
 * derives per-second values and writes THIS human-/Prometheus-friendly CSV next
 * to it. Epoch seconds is the first column so a row overlays directly onto the
 * meeting timeline and any external Prometheus scrape. Per-core % and the
 * per-process breakdown are packed into single `;`-separated cells so the row
 * width is stable regardless of core count or how many Chrome processes were
 * matched at that tick.
 */

import type { DerivedProc, DerivedSample } from "./derive";

/** Column order for the derived CSV. Keep in sync with {@link formatDerivedRow}. */
export const DERIVED_CSV_COLUMNS = [
  "epoch",
  "dt_sec",
  "cpu_busy_pct",
  "cpu_steal_pct",
  "per_core_pct",
  "load1",
  "load5",
  "load15",
  "mem_total_kb",
  "mem_used_kb",
  "mem_avail_kb",
  "swap_used_kb",
  "nic_rx_bytes_per_sec",
  "nic_tx_bytes_per_sec",
  "proc_count",
  "procs",
] as const;

export const DERIVED_CSV_HEADER = DERIVED_CSV_COLUMNS.join(",");

/** Round to one decimal place; keep integers integer-looking (`3` not `3.0`). */
export function round1(v: number): number {
  return Math.round(v * 10) / 10;
}

/**
 * Render one process breakdown as `comm:pid:cpuPct:rssKb`, joined by `;`. The
 * comm has already had commas stripped by the sampler; we additionally strip
 * `;` and `:` so the packed cell can never grow extra fields from a hostile
 * process name.
 */
export function formatProcsCell(procs: readonly DerivedProc[]): string {
  return procs
    .map((p) => `${sanitizeComm(p.comm)}:${p.pid}:${round1(p.cpuPct)}:${p.rssKb}`)
    .join(";");
}

function sanitizeComm(comm: string): string {
  return comm.replace(/[;:,]/g, "_");
}

/** Format one derived sample as a single CSV row (no trailing newline). */
export function formatDerivedRow(s: DerivedSample): string {
  return [
    s.epoch,
    round1(s.dtSec),
    round1(s.cpuBusyPct),
    round1(s.cpuStealPct),
    s.perCorePct.map(round1).join(";"),
    round1(s.load1),
    round1(s.load5),
    round1(s.load15),
    s.memTotalKb,
    s.memUsedKb,
    s.memAvailKb,
    s.swapUsedKb,
    Math.round(s.nicRxBytesPerSec),
    Math.round(s.nicTxBytesPerSec),
    s.procCount ?? "",
    formatProcsCell(s.procs),
  ].join(",");
}

/** Header + one row per derived sample, newline-joined, with a trailing newline. */
export function formatDerivedCsv(samples: readonly DerivedSample[]): string {
  return [DERIVED_CSV_HEADER, ...samples.map(formatDerivedRow)].join("\n") + "\n";
}
