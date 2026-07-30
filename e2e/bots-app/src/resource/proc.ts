/**
 * Pure parsers + delta math for the resource sampler's raw CSV (issue 2032).
 *
 * The shell sampler (`scripts/resource-sampler.sh`) writes RAW /proc counters —
 * cumulative CPU jiffies, cumulative NIC bytes, per-process cpu-jiffies — with
 * one block of typed rows per tick, all sharing an epoch-seconds timestamp.
 * This module turns two consecutive ticks into DERIVED, human-readable values:
 * CPU %, steal %, NIC bytes/s, per-process CPU %. Keeping every arithmetic step
 * here (not in the shell) means the delta math has exactly one home and is
 * unit-tested against known counter pairs in `proc.test.ts`.
 *
 * All functions are pure (string/number in, value out) — no I/O, no clock.
 */

/** CLK_TCK fallback when the meta row is missing/malformed. Linux default. */
export const DEFAULT_CLK_TCK = 100;

/** One machine's aggregate CPU jiffies, parsed from a `cpu` / `core` row. */
export interface CpuJiffies {
  user: number;
  nice: number;
  system: number;
  idle: number;
  iowait: number;
  irq: number;
  softirq: number;
  steal: number;
}

/** Sum of every jiffies field — the denominator for a busy-fraction delta. */
export function cpuTotalJiffies(j: CpuJiffies): number {
  return j.user + j.nice + j.system + j.idle + j.iowait + j.irq + j.softirq + j.steal;
}

/** Non-idle jiffies. `idle + iowait` is idle time; everything else is busy. */
export function cpuBusyJiffies(j: CpuJiffies): number {
  return cpuTotalJiffies(j) - j.idle - j.iowait;
}

/**
 * Parse the numeric tail of a `cpu` / `core` row into {@link CpuJiffies}. The
 * caller has already split the CSV line; `fields` is the 8-value jiffies tail
 * (user..steal). Missing trailing fields (older kernels omit steal) default 0.
 */
export function parseCpuJiffies(fields: readonly string[]): CpuJiffies {
  const n = (i: number): number => {
    const v = Number(fields[i]);
    return Number.isFinite(v) ? v : 0;
  };
  return {
    user: n(0),
    nice: n(1),
    system: n(2),
    idle: n(3),
    iowait: n(4),
    irq: n(5),
    softirq: n(6),
    steal: n(7),
  };
}

/**
 * Busy% and steal% between two cumulative CPU snapshots. The denominator is the
 * total-jiffies delta, so the result is independent of the wall-clock interval
 * and correct even when ticks are irregular. A zero (or negative, on counter
 * reset) total delta returns 0 for both — the safe reading for "no measurable
 * window" rather than a divide-by-zero spike.
 */
export function cpuPercentBetween(
  prev: CpuJiffies,
  cur: CpuJiffies,
): { busyPct: number; stealPct: number } {
  const totalDelta = cpuTotalJiffies(cur) - cpuTotalJiffies(prev);
  if (totalDelta <= 0) return { busyPct: 0, stealPct: 0 };
  const busyDelta = cpuBusyJiffies(cur) - cpuBusyJiffies(prev);
  const stealDelta = cur.steal - prev.steal;
  return {
    busyPct: clampPct((busyDelta / totalDelta) * 100),
    stealPct: clampPct((stealDelta / totalDelta) * 100),
  };
}

/**
 * NIC throughput (bytes/s) between two cumulative byte counters over `dtSec`
 * seconds. A non-positive interval or a counter reset (negative delta) yields
 * 0 — never a negative or infinite rate.
 */
export function netRateBetween(
  prev: { rx: number; tx: number },
  cur: { rx: number; tx: number },
  dtSec: number,
): { rxBytesPerSec: number; txBytesPerSec: number } {
  if (dtSec <= 0) return { rxBytesPerSec: 0, txBytesPerSec: 0 };
  const rxDelta = cur.rx - prev.rx;
  const txDelta = cur.tx - prev.tx;
  return {
    rxBytesPerSec: rxDelta > 0 ? rxDelta / dtSec : 0,
    txBytesPerSec: txDelta > 0 ? txDelta / dtSec : 0,
  };
}

/**
 * Per-process CPU% between two snapshots of a single PID's (utime+stime)
 * jiffies over `dtSec` seconds. Jiffies are converted to seconds via `clkTck`,
 * then divided by the wall interval: 100% == one core fully saturated by this
 * process (so a multi-threaded encoder can legitimately exceed 100%). A
 * non-positive interval or counter reset yields 0.
 */
export function procCpuPercentBetween(
  prevJiffies: number,
  curJiffies: number,
  dtSec: number,
  clkTck: number,
): number {
  if (dtSec <= 0 || clkTck <= 0) return 0;
  const deltaJiffies = curJiffies - prevJiffies;
  if (deltaJiffies <= 0) return 0;
  return (deltaJiffies / clkTck / dtSec) * 100;
}

function clampPct(v: number): number {
  if (!Number.isFinite(v)) return 0;
  if (v < 0) return 0;
  if (v > 100) return 100;
  return v;
}

// ── Raw-CSV row grouping ────────────────────────────────────────────────────

/** Per-process raw counters at one tick. */
export interface ProcSampleRaw {
  pid: number;
  comm: string;
  /** utime + stime jiffies (cumulative). */
  jiffies: number;
  rssKb: number;
}

/** One tick's worth of raw counters, grouped by shared epoch. */
export interface RawSample {
  epoch: number;
  load: { l1: number; l5: number; l15: number } | null;
  cpu: CpuJiffies | null;
  /** Per-core jiffies indexed by core number. */
  cores: Map<number, CpuJiffies>;
  mem: { totalKb: number; availKb: number; swapTotalKb: number; swapFreeKb: number } | null;
  net: { rx: number; tx: number } | null;
  procs: ProcSampleRaw[];
  /** Explicit matched-process count (renderer-crash signal). */
  procCount: number | null;
}

/** Environment captured in the sampler's `meta` row. */
export interface SamplerMeta {
  schemaVersion: number;
  label: string;
  hostname: string;
  clkTck: number;
  ncpu: number;
  haveMpstat: boolean;
  havePidstat: boolean;
  intervalSec: number;
}

export interface ParsedRawCsv {
  meta: SamplerMeta | null;
  /** `true` when the sampler wrote an `unsupported` row (non-Linux box). */
  unsupported: boolean;
  samples: RawSample[];
}

/**
 * Parse the sampler's raw CSV into a meta record plus one {@link RawSample} per
 * distinct epoch, in first-seen order. Unknown row types are ignored (forward
 * compatibility); malformed numeric fields degrade to 0 rather than throwing,
 * because a single corrupt tick must not sink the whole summary.
 */
export function parseRawCsv(text: string): ParsedRawCsv {
  let meta: SamplerMeta | null = null;
  let unsupported = false;
  const byEpoch = new Map<number, RawSample>();
  const order: number[] = [];

  const ensure = (epoch: number): RawSample => {
    let s = byEpoch.get(epoch);
    if (s === undefined) {
      s = {
        epoch,
        load: null,
        cpu: null,
        cores: new Map(),
        mem: null,
        net: null,
        procs: [],
        procCount: null,
      };
      byEpoch.set(epoch, s);
      order.push(epoch);
    }
    return s;
  };

  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (line === "") continue;
    const f = line.split(",");
    const kind = f[0];
    if (kind === "meta") {
      meta = {
        schemaVersion: intOr(f[1], 0),
        label: f[2] ?? "",
        hostname: f[3] ?? "",
        clkTck: intOr(f[4], DEFAULT_CLK_TCK),
        ncpu: intOr(f[5], 0),
        haveMpstat: f[6] === "1",
        havePidstat: f[7] === "1",
        intervalSec: numOr(f[8], 5),
      };
      continue;
    }
    if (kind === "unsupported") {
      unsupported = true;
      continue;
    }
    const epoch = intOr(f[1], NaN);
    if (!Number.isFinite(epoch)) continue;
    switch (kind) {
      case "load":
        ensure(epoch).load = { l1: numOr(f[2], 0), l5: numOr(f[3], 0), l15: numOr(f[4], 0) };
        break;
      case "cpu":
        ensure(epoch).cpu = parseCpuJiffies(f.slice(2));
        break;
      case "core": {
        const coreIdx = intOr(f[2], -1);
        if (coreIdx >= 0) ensure(epoch).cores.set(coreIdx, parseCpuJiffies(f.slice(3)));
        break;
      }
      case "mem":
        ensure(epoch).mem = {
          totalKb: intOr(f[2], 0),
          availKb: intOr(f[3], 0),
          swapTotalKb: intOr(f[4], 0),
          swapFreeKb: intOr(f[5], 0),
        };
        break;
      case "net":
        ensure(epoch).net = { rx: numOr(f[2], 0), tx: numOr(f[3], 0) };
        break;
      case "proc":
        ensure(epoch).procs.push({
          pid: intOr(f[2], 0),
          comm: f[3] ?? "?",
          jiffies: numOr(f[4], 0) + numOr(f[5], 0),
          rssKb: intOr(f[6], 0),
        });
        break;
      case "proccount":
        ensure(epoch).procCount = intOr(f[2], 0);
        break;
      default:
        // watch-exit and any future row types: ignored.
        break;
    }
  }

  return { meta, unsupported, samples: order.map((e) => byEpoch.get(e) as RawSample) };
}

function intOr(v: string | undefined, fallback: number): number {
  if (v === undefined) return fallback;
  const n = Number.parseInt(v, 10);
  return Number.isFinite(n) ? n : fallback;
}

function numOr(v: string | undefined, fallback: number): number {
  if (v === undefined) return fallback;
  const n = Number(v);
  return Number.isFinite(n) ? n : fallback;
}
