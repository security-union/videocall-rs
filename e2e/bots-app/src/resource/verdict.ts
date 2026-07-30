/**
 * Per-run summary aggregation + the RESOURCE_STARVED verdict (issue 2032).
 *
 * A self-starved bot run (the box the bots run on saturated its own CPU) looks
 * exactly like a product regression from the client signals alone — collapsed
 * encoder FPS, ballooning RTT, simulcast sheds. The field incident on
 * 2026-07-28 was INFERRED from those client signals because the box was not in
 * Prometheus. This verdict measures the box directly and flags a confounded run
 * so it is not mis-analyzed as a product freeze.
 *
 * Pure: `summarize` folds derived samples into peaks/means; `evaluateVerdict`
 * applies two independent rules. Both are unit-tested at their boundaries so a
 * threshold typo or an off-by-one in the sustained-window scan goes red.
 */

import type { DerivedSample } from "./derive";
import type { FpsStats } from "./fps";

/**
 * Peak overall-CPU threshold (%). At or below this the box had headroom; a
 * SUSTAINED excursion above it is treated as saturation. 85% leaves margin for
 * the normal encode/transport spikes a healthy run produces.
 */
export const RESOURCE_CPU_STARVED_PCT = 85;

/**
 * Number of CONSECUTIVE derived samples that must all exceed
 * {@link RESOURCE_CPU_STARVED_PCT} to count as sustained saturation. At the
 * default ~5s cadence, 3 samples ≈ 15s — long enough that a single GC pause or
 * encode burst does not trip the verdict, short enough to catch a real freeze.
 */
export const RESOURCE_CPU_SUSTAIN_SAMPLES = 3;

/**
 * Minimum SUSTAINED sub-base-rung duration (ms) that marks a bot starved
 * (#2064). The client republishes the fps global on its ~5s health tick, so
 * ~10s spans about two publications — a genuinely sustained low, not a single
 * transient publication (which the 2s poll would oversample). Time-based, not a
 * read count, so it is independent of the poll rate. Compare to the CPU rule's
 * ~15s (3 samples x ~5s); fps is shorter because a sustained sub-rung encoder is
 * a more direct starvation signal.
 */
export const RESOURCE_FPS_SUSTAIN_MS = 10000;

/**
 * Default base-rung FPS floor. A bot whose reported encoder FPS never reaches
 * this is treated as starved regardless of CPU (the encoder could not even hold
 * the lowest simulcast rung). It SHOULD be kept aligned with the
 * videocall-client's lowest-rung target FPS. The default of 5 cleanly separates
 * the field incident's collapsed bots (1-4 fps) from the humans that held 8 fps.
 */
export const RESOURCE_FPS_BASE_RUNG = 5;

/** One process's peak footprint over the run, keyed by pid. */
export interface ProcPeak {
  pid: number;
  comm: string;
  cpuPctPeak: number;
  rssKbPeak: number;
}

export interface ResourceSummary {
  sampleCount: number;
  durationSec: number;
  cpuPeakPct: number;
  cpuMeanPct: number;
  cpuStealPeakPct: number;
  perCorePeakPct: number;
  load1Peak: number;
  memTotalKb: number;
  memUsedPeakKb: number;
  memAvailMinKb: number;
  swapUsedPeakKb: number;
  nicRxPeakBytesPerSec: number;
  nicTxPeakBytesPerSec: number;
  procCountPeak: number | null;
  procCountMin: number | null;
  /** Per-pid peaks, sorted by peak CPU% descending. */
  procPeaks: ProcPeak[];
}

/** Fold derived samples into per-run peaks/means. */
export function summarize(samples: readonly DerivedSample[]): ResourceSummary {
  const empty: ResourceSummary = {
    sampleCount: 0,
    durationSec: 0,
    cpuPeakPct: 0,
    cpuMeanPct: 0,
    cpuStealPeakPct: 0,
    perCorePeakPct: 0,
    load1Peak: 0,
    memTotalKb: 0,
    memUsedPeakKb: 0,
    memAvailMinKb: 0,
    swapUsedPeakKb: 0,
    nicRxPeakBytesPerSec: 0,
    nicTxPeakBytesPerSec: 0,
    procCountPeak: null,
    procCountMin: null,
    procPeaks: [],
  };
  if (samples.length === 0) return empty;

  let cpuSum = 0;
  let cpuPeak = 0;
  let stealPeak = 0;
  let corePeak = 0;
  let load1Peak = 0;
  let memTotal = 0;
  let memUsedPeak = 0;
  let memAvailMin = Number.POSITIVE_INFINITY;
  let swapUsedPeak = 0;
  let rxPeak = 0;
  let txPeak = 0;
  let procCountPeak: number | null = null;
  let procCountMin: number | null = null;
  let durationSec = 0;
  const procByPid = new Map<number, ProcPeak>();

  for (const s of samples) {
    cpuSum += s.cpuBusyPct;
    cpuPeak = Math.max(cpuPeak, s.cpuBusyPct);
    stealPeak = Math.max(stealPeak, s.cpuStealPct);
    for (const c of s.perCorePct) corePeak = Math.max(corePeak, c);
    load1Peak = Math.max(load1Peak, s.load1);
    if (s.memTotalKb > 0) memTotal = s.memTotalKb;
    memUsedPeak = Math.max(memUsedPeak, s.memUsedKb);
    if (s.memAvailKb > 0) memAvailMin = Math.min(memAvailMin, s.memAvailKb);
    swapUsedPeak = Math.max(swapUsedPeak, s.swapUsedKb);
    rxPeak = Math.max(rxPeak, s.nicRxBytesPerSec);
    txPeak = Math.max(txPeak, s.nicTxBytesPerSec);
    if (s.procCount !== null) {
      procCountPeak = procCountPeak === null ? s.procCount : Math.max(procCountPeak, s.procCount);
      procCountMin = procCountMin === null ? s.procCount : Math.min(procCountMin, s.procCount);
    }
    durationSec += s.dtSec;
    for (const p of s.procs) {
      const cur = procByPid.get(p.pid);
      if (cur === undefined) {
        procByPid.set(p.pid, {
          pid: p.pid,
          comm: p.comm,
          cpuPctPeak: p.cpuPct,
          rssKbPeak: p.rssKb,
        });
      } else {
        cur.cpuPctPeak = Math.max(cur.cpuPctPeak, p.cpuPct);
        cur.rssKbPeak = Math.max(cur.rssKbPeak, p.rssKb);
      }
    }
  }

  const procPeaks = Array.from(procByPid.values()).sort((a, b) => b.cpuPctPeak - a.cpuPctPeak);

  return {
    sampleCount: samples.length,
    durationSec,
    cpuPeakPct: cpuPeak,
    cpuMeanPct: cpuSum / samples.length,
    cpuStealPeakPct: stealPeak,
    perCorePeakPct: corePeak,
    load1Peak,
    memTotalKb: memTotal,
    memUsedPeakKb: memUsedPeak,
    memAvailMinKb: Number.isFinite(memAvailMin) ? memAvailMin : 0,
    swapUsedPeakKb: swapUsedPeak,
    nicRxPeakBytesPerSec: rxPeak,
    nicTxPeakBytesPerSec: txPeak,
    procCountPeak,
    procCountMin,
    procPeaks,
  };
}

export interface VerdictOptions {
  cpuThresholdPct?: number;
  cpuSustainSamples?: number;
  fpsSustainMs?: number;
}

export interface ResourceVerdict {
  starved: boolean;
  /** Human-readable reason lines (one per rule that fired). Empty when healthy. */
  reasons: string[];
  /** Whether the CPU sustained-saturation rule fired. */
  cpuStarved: boolean;
  /** Whether the FPS-below-base-rung rule fired. */
  fpsStarved: boolean;
}

/**
 * Apply the two RESOURCE_STARVED rules:
 *
 *  1. CPU: any run of `cpuSustainSamples` CONSECUTIVE derived samples whose
 *     overall busy% all exceed `cpuThresholdPct`. Strictly-greater at the
 *     boundary — a sample sitting exactly at the threshold does not count, so
 *     the healthy-headroom line is inclusive.
 *  2. FPS: any bot whose worst reading is sustained below the base rung for
 *     `fpsSustainMs`. Only bots that actually reported a reading are considered;
 *     a run with no FPS data cannot trip this rule (it degrades to a CPU-only
 *     verdict).
 *
 * The two rules are independent — either alone marks the run RESOURCE_STARVED.
 */
export function evaluateVerdict(
  samples: readonly DerivedSample[],
  fpsByBot: ReadonlyMap<string, FpsStats>,
  opts: VerdictOptions = {},
): ResourceVerdict {
  const cpuThreshold = opts.cpuThresholdPct ?? RESOURCE_CPU_STARVED_PCT;
  const sustain = opts.cpuSustainSamples ?? RESOURCE_CPU_SUSTAIN_SAMPLES;
  const reasons: string[] = [];

  const cpuStarved = hasSustainedCpuSaturation(samples, cpuThreshold, sustain);
  if (cpuStarved) {
    const peak = samples.reduce((m, s) => Math.max(m, s.cpuBusyPct), 0);
    reasons.push(
      `CPU saturated: ${sustain}+ consecutive samples above ${cpuThreshold}% overall (peak ${peak.toFixed(1)}%)`,
    );
  }

  const fpsSustainMs = opts.fpsSustainMs ?? RESOURCE_FPS_SUSTAIN_MS;
  let fpsStarved = false;
  for (const [botId, stats] of fpsByBot) {
    if (stats.count > 0 && stats.maxSustainedBelowRungMs >= fpsSustainMs) {
      fpsStarved = true;
      reasons.push(
        `bot ${botId} encoder FPS sustained below base rung ${RESOURCE_FPS_BASE_RUNG} ` +
          `for ${(stats.maxSustainedBelowRungMs / 1000).toFixed(1)}s (min ${stats.min.toFixed(1)})`,
      );
    }
  }

  return { starved: cpuStarved || fpsStarved, reasons, cpuStarved, fpsStarved };
}

/**
 * Scan for a window of `window` consecutive samples all strictly above
 * `threshold`. A running counter (not an index-math slice) so an off-by-one is
 * caught by the boundary tests: exactly `window-1` sustained samples must NOT
 * fire, exactly `window` must.
 */
export function hasSustainedCpuSaturation(
  samples: readonly DerivedSample[],
  threshold: number,
  window: number,
): boolean {
  if (window <= 0) return false;
  let run = 0;
  for (const s of samples) {
    if (s.cpuBusyPct > threshold) {
      run += 1;
      if (run >= window) return true;
    } else {
      run = 0;
    }
  }
  return false;
}
