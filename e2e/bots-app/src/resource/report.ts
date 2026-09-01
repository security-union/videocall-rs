/**
 * Render the per-run resource summary + verdict as a prominent text block
 * (issue 2032). Kept pure so the exact wording is unit-tested and stable — the
 * RESOURCE_STARVED banner in particular must be greppable in run output so a
 * confounded run is flagged and not meeting-analyzed as a product freeze.
 */

import type { ArrivalSpread } from "./arrival";
import type { FpsStats } from "./fps";
import type { ResourceSummary, ResourceVerdict } from "./verdict";

/** Greppable banner line emitted when the verdict is starved. */
export const RESOURCE_STARVED_BANNER = "RESOURCE_STARVED";
/** Counterpart banner for a clean run. */
export const RESOURCE_OK_BANNER = "RESOURCE_OK";
/** Banner for a run that produced no evidence either way (issue #2358). */
export const RESOURCE_NO_EVIDENCE_BANNER = "RESOURCE_NO_EVIDENCE";

/** Starved wins over no-evidence: a fired rule IS evidence. */
export function bannerFor(verdict: ResourceVerdict): string {
  if (verdict.starved) return RESOURCE_STARVED_BANNER;
  if (verdict.noEvidence) return RESOURCE_NO_EVIDENCE_BANNER;
  return RESOURCE_OK_BANNER;
}

function mib(kb: number): string {
  return `${Math.round(kb / 1024)} MiB`;
}

function mbps(bytesPerSec: number): string {
  return `${(bytesPerSec / 125_000).toFixed(2)} Mbps`;
}

export interface ReportInput {
  summary: ResourceSummary;
  verdict: ResourceVerdict;
  fpsByBot: ReadonlyMap<string, FpsStats>;
  arrival: ArrivalSpread | null;
  /** `false` when the sampler ran on a box without /proc (non-Linux). */
  supported: boolean;
  /** `true` when the sysstat tools were absent (a capability note, not a failure). */
  sysstatMissing: boolean;
  rawCsvPath: string;
  derivedCsvPath: string;
}

function arrivalLines(spread: ArrivalSpread | null, withAggregates: boolean): string[] {
  if (spread === null) {
    return ["[resource] arrival spread: not tracked for this report"];
  }
  const iso = (ms: number): string => new Date(ms).toISOString();
  const secs = (ms: number): string => (ms / 1000).toFixed(1);
  if (spread.count < 2) {
    const lines = [
      `[resource] arrival spread: n/a — only ${spread.count} local join observed,` +
        ` at ${iso(spread.firstJoinMs)} (a fleet's ramp spans processes: issue #2337)`,
    ];
    if (withAggregates) {
      lines.push(
        "[resource] the capture starts before the launch, so the CPU, RAM, NIC, process and" +
          " verdict lines below include the pre-join stretch",
      );
    }
    return lines;
  }
  const lines = [
    `[resource] arrival spread: ${secs(spread.spreadMs)}s across ${spread.count} local joins` +
      ` (first ${iso(spread.firstJoinMs)} → last ${iso(spread.lastJoinMs)});` +
      " SSH-launched bots join in their own process and are not counted",
  ];
  if (withAggregates) {
    lines.push(
      "[resource] the capture starts before the first launch, so the CPU, RAM, NIC, process and" +
        " verdict lines below cover that ramp. This receipt does not state how long the room held" +
        " every bot",
    );
  }
  return lines;
}

/**
 * Build the multi-line report. A dedicated banner line — `RESOURCE_STARVED`,
 * `RESOURCE_NO_EVIDENCE` or `RESOURCE_OK` — is emitted near the top (right
 * after the opening divider) on its own line, so a one-line grep classifies the
 * run. `RESOURCE_OK` is reserved for a run that produced figures to judge.
 */
export function formatResourceReport(input: ReportInput): string {
  const { summary: s, verdict, fpsByBot } = input;
  const lines: string[] = [];

  lines.push("──────────────────────────────────────────────────────────────");
  const withoutHostCapture = (note: string): string => {
    lines.push(`${bannerFor(verdict)} (${note})`);
    if (verdict.starved) for (const r of verdict.reasons) lines.push(`[resource]   - ${r}`);
    lines.push(...arrivalLines(input.arrival, false));
    lines.push("──────────────────────────────────────────────────────────────");
    return lines.join("\n");
  };
  if (!input.supported) {
    return withoutHostCapture("resource capture unsupported on this box — no /proc");
  }
  if (s.sampleCount === 0) return withoutHostCapture("no resource samples captured");

  lines.push(bannerFor(verdict));
  lines.push(...arrivalLines(input.arrival, true));
  lines.push(
    `[resource] run resource capture — ${s.sampleCount} samples over ${Math.round(s.durationSec)}s`,
  );
  lines.push(
    `[resource] CPU: peak ${s.cpuPeakPct.toFixed(1)}% / mean ${s.cpuMeanPct.toFixed(1)}% overall` +
      ` · per-core peak ${s.perCorePeakPct.toFixed(1)}% · steal peak ${s.cpuStealPeakPct.toFixed(1)}%` +
      ` · load1 peak ${s.load1Peak.toFixed(2)}`,
  );
  lines.push(
    `[resource] RAM: used peak ${mib(s.memUsedPeakKb)} / ${mib(s.memTotalKb)} total` +
      ` · avail min ${mib(s.memAvailMinKb)} · swap used peak ${mib(s.swapUsedPeakKb)}`,
  );
  lines.push(
    `[resource] NIC: rx peak ${mbps(s.nicRxPeakBytesPerSec)} · tx peak ${mbps(s.nicTxPeakBytesPerSec)}`,
  );
  if (s.procCountPeak !== null) {
    const crash =
      s.procCountMin !== null && s.procCountMin < s.procCountPeak
        ? ` (dropped to ${s.procCountMin} — possible renderer crash)`
        : "";
    lines.push(`[resource] matched processes: peak ${s.procCountPeak}${crash}`);
  }
  for (const p of s.procPeaks.slice(0, 5)) {
    lines.push(
      `[resource]   ${p.comm} (pid ${p.pid}): CPU peak ${p.cpuPctPeak.toFixed(1)}% · RSS peak ${mib(p.rssKbPeak)}`,
    );
  }

  if (fpsByBot.size > 0) {
    for (const [botId, f] of fpsByBot) {
      lines.push(
        `[resource] bot ${botId} encoder fps: min ${f.min.toFixed(1)} / mean ${f.mean.toFixed(1)} / latest ${f.latest.toFixed(1)} (${f.count} readings)`,
      );
    }
  } else {
    lines.push(
      "[resource] bot encoder fps: not reported " +
        "(no window.__videocall_encoder_fps readings — client build without " +
        "#2057, or camera off / encoder never warmed the whole run)",
    );
  }

  if (input.sysstatMissing) {
    lines.push(
      "[resource] note: sysstat (mpstat/pidstat) absent on the box — sampled from /proc directly",
    );
  }

  if (verdict.starved) {
    lines.push(
      `[resource] VERDICT: ${RESOURCE_STARVED_BANNER} — this run was resource-constrained:`,
    );
    for (const r of verdict.reasons) lines.push(`[resource]   - ${r}`);
    lines.push(
      "[resource] Treat client-signal regressions (encoder fps, RTT, sheds) from THIS run as",
    );
    lines.push("[resource] confounded by box saturation, not a product regression.");
  } else if (verdict.noEvidence) {
    lines.push(
      `[resource] VERDICT: ${RESOURCE_NO_EVIDENCE_BANNER} — this run produced no evidence to judge:`,
    );
    for (const r of verdict.reasons) lines.push(`[resource]   - ${r}`);
    lines.push("[resource] No figure from THIS run is representative of a loaded fleet.");
  } else {
    lines.push(`[resource] VERDICT: ${RESOURCE_OK_BANNER} — box had headroom for this run.`);
  }
  lines.push(`[resource] raw counters: ${input.rawCsvPath}`);
  lines.push(`[resource] derived csv:  ${input.derivedCsvPath}`);
  lines.push("──────────────────────────────────────────────────────────────");
  return lines.join("\n");
}
