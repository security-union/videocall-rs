import { readFileSync, writeFileSync } from "node:fs";

import type { Command } from "commander";

import { taggedLine } from "../log-line";
import { deriveSamples } from "./derive";
import { type CpuJiffies, cpuTotalJiffies, parseRawCsv, type RawSample } from "./proc";
import { RESOURCE_NO_EVIDENCE_BANNER, RESOURCE_OK_BANNER, RESOURCE_STARVED_BANNER } from "./report";
import {
  evaluateVerdict,
  hasSustainedCpuSaturation,
  RESOURCE_CPU_STARVED_PCT,
  RESOURCE_CPU_SUSTAIN_SAMPLES,
  summarize,
} from "./verdict";

/** The `meta,<version>,…` row layout this reader understands. */
export const RAW_CSV_SCHEMA_VERSION = 1;

/** The sampling cadence the CPU sustain rule is calibrated for. */
export const RESOURCE_SAMPLE_INTERVAL_SEC = 5;

/** No in-window tick, nor the gap from the last tick to `to`, may exceed this; a 25 s burst then spans 3 full ticks. */
export const RESOURCE_MAX_TICK_SEC = 6.25;

export interface GeneratorVerdict {
  ok: boolean;
  detail: string;
  starved: boolean;
  noEvidence: boolean;
  reasons: string[];
  window: { from: number; to: number; unit: "epoch_seconds"; samples: number };
}

function unjudged(from: number, to: number, reasons: string[]): GeneratorVerdict {
  return {
    ok: false,
    detail: `${RESOURCE_NO_EVIDENCE_BANNER}: ${reasons.join("; ")}`,
    starved: false,
    noEvidence: true,
    reasons,
    window: {
      from: Number.isFinite(from) ? from : 0,
      to: Number.isFinite(to) ? to : 0,
      unit: "epoch_seconds",
      samples: 0,
    },
  };
}

/** Epochs of `cpu` rows with a missing or non-finite field (which `parseCpuJiffies` reads as 0) or an all-zero total. */
function garbledCpuEpochs(csvText: string): Set<number> {
  const out = new Set<number>();
  for (const line of csvText.split(/\r?\n/)) {
    const f = line.trim().split(",");
    if (f[0] !== "cpu") continue;
    const jiffies = f.slice(2, 10);
    const bad = jiffies.some((v) => v.trim() === "" || !Number.isFinite(Number(v)));
    if (jiffies.length < 8 || bad || jiffies.every((v) => Number(v) === 0)) {
      out.add(Number.parseInt(f[1], 10));
    }
  }
  return out;
}

function intact(row: RawSample, garbled: Set<number>): row is RawSample & { cpu: CpuJiffies } {
  return row.cpu !== null && !garbled.has(row.epoch);
}

/**
 * A pair is usable when both rows are intact, the total rises, and the baseline row rose from
 * the last intact row before it.
 */
function cpuUsable(raw: readonly RawSample[], i: number, garbled: Set<number>): boolean {
  const prev = raw[i];
  const cur = raw[i + 1];
  if (!intact(prev, garbled) || !intact(cur, garbled)) return false;
  if (cpuTotalJiffies(cur.cpu) <= cpuTotalJiffies(prev.cpu)) return false;
  for (let j = i - 1; j >= 0; j--) {
    const before = raw[j];
    if (intact(before, garbled)) return cpuTotalJiffies(prev.cpu) > cpuTotalJiffies(before.cpu);
  }
  return true;
}

/** Judge the derived samples stamped inside `[from, to]` (closed) of a raw sampler CSV. */
export function windowVerdict(csvText: string, from: number, to: number): GeneratorVerdict {
  if (!Number.isFinite(from) || !Number.isFinite(to) || from > to) {
    return unjudged(from, to, [`invalid window [${from}, ${to}]`]);
  }
  const parsed = parseRawCsv(csvText);
  if (parsed.meta === null) {
    return unjudged(from, to, ["malformed CSV: no meta row, so not a resource-sampler raw CSV"]);
  }
  if (parsed.meta.schemaVersion !== RAW_CSV_SCHEMA_VERSION) {
    return unjudged(from, to, [
      `malformed CSV: unknown schema version ${parsed.meta.schemaVersion}`,
    ]);
  }
  if (parsed.unsupported) {
    return unjudged(from, to, ["resource capture unsupported on the sampled box (no /proc)"]);
  }

  const interval = parsed.meta.intervalSec;
  if (interval !== RESOURCE_SAMPLE_INTERVAL_SEC) {
    return unjudged(from, to, [
      `sampler interval ${interval}s: the CPU rule is calibrated on ${RESOURCE_SAMPLE_INTERVAL_SEC}s samples`,
    ]);
  }

  const raw = parsed.samples;
  const derived = deriveSamples(parsed);
  const garbled = garbledCpuEpochs(csvText);
  const idx = derived.flatMap((s, i) => (s.epoch >= from && s.epoch <= to ? [i] : []));
  const inWindow = idx.map((i) => derived[i]);
  const usable = idx.map((i) => cpuUsable(raw, i, garbled));
  const lackingCpu = usable.filter((u) => !u).length;

  const verdict = evaluateVerdict(inWindow, new Map(), null);
  const validRuns: (typeof inWindow)[] = [[]];
  inWindow.forEach((s, k) =>
    usable[k] ? validRuns[validRuns.length - 1].push(s) : validRuns.push([]),
  );
  const starved =
    verdict.starved &&
    validRuns.some((run) =>
      hasSustainedCpuSaturation(run, RESOURCE_CPU_STARVED_PCT, RESOURCE_CPU_SUSTAIN_SAMPLES),
    );
  const reasons = starved || !verdict.starved ? [...verdict.reasons] : [];
  const span =
    raw.length > 0
      ? `the CSV covers [${raw[0].epoch}, ${raw[raw.length - 1].epoch}]`
      : "the CSV has no ticks";
  const gaps: string[] = [];
  if (inWindow.length < RESOURCE_CPU_SUSTAIN_SAMPLES) {
    gaps.push(
      `${inWindow.length} derived samples in window [${from}, ${to}] epoch seconds, fewer than the ${RESOURCE_CPU_SUSTAIN_SAMPLES} the CPU rule needs; ${span}`,
    );
  } else {
    if (raw[idx[0]].epoch > from + interval) {
      gaps.push(`window start ${from} is not covered: ${span}`);
    }
    if (inWindow[inWindow.length - 1].epoch < to - RESOURCE_MAX_TICK_SEC) {
      gaps.push(`window end ${to} is not covered: ${span}`);
    }
    const holes = idx.filter((i) => {
      const dt = raw[i + 1].epoch - raw[i].epoch;
      return dt < interval || dt > RESOURCE_MAX_TICK_SEC;
    });
    if (holes.length > 0) {
      const at = holes.map((i) => `${raw[i].epoch}->${raw[i + 1].epoch}`).join(", ");
      gaps.push(
        `tick outside [${interval}, ${RESOURCE_MAX_TICK_SEC}]s inside the window (sampler gap or clock step): ${at}`,
      );
    }
  }
  reasons.push(...gaps);
  if (lackingCpu > 0) {
    reasons.push(
      `malformed CSV: ${lackingCpu} of ${inWindow.length} samples in the window lack valid CPU counters`,
    );
  }

  const s = summarize(inWindow);
  const ok = !verdict.starved && !verdict.noEvidence && gaps.length === 0 && lackingCpu === 0;
  const figures =
    `${inWindow.length} samples in [${from}, ${to}]; CPU peak ${s.cpuPeakPct.toFixed(1)}%` +
    ` / mean ${s.cpuMeanPct.toFixed(1)}%; steal peak ${s.cpuStealPeakPct.toFixed(1)}%;` +
    " CPU rule only, the encoder-fps rule is not evaluated from the CSV";
  const banner = starved
    ? RESOURCE_STARVED_BANNER
    : ok
      ? RESOURCE_OK_BANNER
      : RESOURCE_NO_EVIDENCE_BANNER;
  return {
    ok,
    detail: [`${banner}: ${figures}`, ...reasons].join("; "),
    starved,
    noEvidence: !ok && !starved,
    reasons,
    window: { from, to, unit: "epoch_seconds", samples: inWindow.length },
  };
}

function parseEpoch(v: string): number {
  return /^\d+(\.\d+)?$/.test(v.trim()) ? Number(v) : Number.NaN;
}

interface ResourceVerdictOptions {
  csv: string;
  from: string;
  to: string;
  out: string;
}

export function registerResourceVerdictCommand(program: Command): void {
  program
    .command("resource-verdict")
    .description(
      'Judge one window of a resource-sampler raw CSV and write {"ok","detail",…} for the scorer\'s ' +
        "--generator-verdict (G-V5). CPU rule only: the CSV carries no encoder fps. Steal has read 0 on " +
        "WSL2, so a low steal does not show the Windows host had headroom. The samples must cover the " +
        `window: within one interval of the start and ${RESOURCE_MAX_TICK_SEC}s of the end, every tick in [${RESOURCE_SAMPLE_INTERVAL_SEC}, ` +
        `${RESOURCE_MAX_TICK_SEC}]s, and at least ` +
        `${RESOURCE_CPU_SUSTAIN_SAMPLES} samples, from a sampler set to every ${RESOURCE_SAMPLE_INTERVAL_SEC}s. ` +
        `STARVED needs ${RESOURCE_CPU_SUSTAIN_SAMPLES} consecutive hot samples. ` +
        "Saturation sustained for >= 25s anywhere inside [from, to] is always reported when every tick, and " +
        `the gap from the last tick to --to, is <= ${RESOURCE_MAX_TICK_SEC}s; otherwise the window is not judged. ` +
        "Exits 0 when ok, 1 when not ok, 2 on a usage error or when the JSON cannot be written.",
    )
    .exitOverride((err) => process.exit(err.exitCode === 0 ? 0 : 2))
    .requiredOption("--csv <path>", "the sampler's <label>-raw.csv")
    .requiredOption(
      "--from <epochSec>",
      "window start, inclusive: Unix epoch SECONDS on the sampled box's wall clock (the CSV's own `date +%s`)",
    )
    .requiredOption("--to <epochSec>", "window end, inclusive, in the same unit")
    .requiredOption("--out <path>", "where to write the verdict JSON")
    .action((opts: ResourceVerdictOptions) => {
      const from = parseEpoch(opts.from);
      const to = parseEpoch(opts.to);
      let text: string | null;
      try {
        text = readFileSync(opts.csv, "utf8");
      } catch {
        text = null;
      }
      const verdict =
        text === null
          ? unjudged(from, to, [`malformed CSV: cannot read ${opts.csv}`])
          : windowVerdict(text, from, to);
      try {
        writeFileSync(opts.out, JSON.stringify(verdict, null, 2) + "\n", "utf8");
      } catch (err) {
        console.error(taggedLine("resource-verdict", `cannot write ${opts.out}: ${String(err)}`));
        process.exitCode = 2;
        return;
      }
      console.log(taggedLine("resource-verdict", `${verdict.detail} -> ${opts.out}`));
      process.exitCode = verdict.ok ? 0 : 1;
    });
}
