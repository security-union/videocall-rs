/**
 * Turn the sampler's raw-counter ticks into per-tick DERIVED records (issue
 * 2032). Each derived sample needs the tick BEFORE it to diff cumulative
 * counters, so N raw ticks yield N-1 derived samples (the first tick is the
 * baseline). All arithmetic is delegated to the unit-tested primitives in
 * `proc.ts`; this module only walks the pairs and matches per-process PIDs.
 */

import {
  cpuPercentBetween,
  DEFAULT_CLK_TCK,
  netRateBetween,
  type ParsedRawCsv,
  procCpuPercentBetween,
  type RawSample,
} from "./proc";

/** One matched process's derived reading at a tick. */
export interface DerivedProc {
  pid: number;
  comm: string;
  cpuPct: number;
  rssKb: number;
}

/** All derived values for one tick (the second half of a raw-sample pair). */
export interface DerivedSample {
  epoch: number;
  /** Wall seconds since the previous tick (the delta window). */
  dtSec: number;
  cpuBusyPct: number;
  cpuStealPct: number;
  perCorePct: number[];
  load1: number;
  load5: number;
  load15: number;
  memTotalKb: number;
  memUsedKb: number;
  memAvailKb: number;
  swapUsedKb: number;
  nicRxBytesPerSec: number;
  nicTxBytesPerSec: number;
  procs: DerivedProc[];
  procCount: number | null;
}

/**
 * Derive per-tick records from a parsed raw CSV. Returns an empty array for an
 * unsupported (non-Linux) capture or when fewer than two ticks were recorded —
 * both are legitimate "nothing measurable" states, not errors.
 */
export function deriveSamples(parsed: ParsedRawCsv): DerivedSample[] {
  if (parsed.unsupported) return [];
  const clkTck =
    parsed.meta?.clkTck && parsed.meta.clkTck > 0 ? parsed.meta.clkTck : DEFAULT_CLK_TCK;
  const fallbackDt =
    parsed.meta?.intervalSec && parsed.meta.intervalSec > 0 ? parsed.meta.intervalSec : 5;
  const out: DerivedSample[] = [];
  for (let i = 1; i < parsed.samples.length; i++) {
    const prev = parsed.samples[i - 1];
    const cur = parsed.samples[i];
    // A non-positive epoch delta (clock stepped back, duplicate second) falls
    // back to the configured interval so per-second rates stay finite.
    const rawDt = cur.epoch - prev.epoch;
    const dtSec = rawDt > 0 ? rawDt : fallbackDt;
    out.push(deriveOne(prev, cur, dtSec, clkTck));
  }
  return out;
}

function deriveOne(prev: RawSample, cur: RawSample, dtSec: number, clkTck: number): DerivedSample {
  const cpu =
    prev.cpu && cur.cpu ? cpuPercentBetween(prev.cpu, cur.cpu) : { busyPct: 0, stealPct: 0 };

  const perCorePct: number[] = [];
  for (const [idx, curCore] of cur.cores) {
    const prevCore = prev.cores.get(idx);
    if (prevCore) perCorePct[idx] = cpuPercentBetween(prevCore, curCore).busyPct;
  }

  const net =
    prev.net && cur.net
      ? netRateBetween(prev.net, cur.net, dtSec)
      : { rxBytesPerSec: 0, txBytesPerSec: 0 };

  const prevByPid = new Map(prev.procs.map((p) => [p.pid, p]));
  const procs: DerivedProc[] = cur.procs.map((p) => {
    const prior = prevByPid.get(p.pid);
    const cpuPct = prior ? procCpuPercentBetween(prior.jiffies, p.jiffies, dtSec, clkTck) : 0;
    return { pid: p.pid, comm: p.comm, cpuPct, rssKb: p.rssKb };
  });

  const mem = cur.mem;
  return {
    epoch: cur.epoch,
    dtSec,
    cpuBusyPct: cpu.busyPct,
    cpuStealPct: cpu.stealPct,
    perCorePct: Array.from(perCorePct, (v) => v ?? 0),
    load1: cur.load?.l1 ?? 0,
    load5: cur.load?.l5 ?? 0,
    load15: cur.load?.l15 ?? 0,
    memTotalKb: mem?.totalKb ?? 0,
    memUsedKb: mem ? Math.max(0, mem.totalKb - mem.availKb) : 0,
    memAvailKb: mem?.availKb ?? 0,
    swapUsedKb: mem ? Math.max(0, mem.swapTotalKb - mem.swapFreeKb) : 0,
    nicRxBytesPerSec: net.rxBytesPerSec,
    nicTxBytesPerSec: net.txBytesPerSec,
    procCount: cur.procCount,
    procs,
  };
}
