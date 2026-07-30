/**
 * Render the per-run resource summary + verdict as a prominent text block
 * (issue 2032). Kept pure so the exact wording is unit-tested and stable — the
 * RESOURCE_STARVED banner in particular must be greppable in run output so a
 * confounded run is flagged and not meeting-analyzed as a product freeze.
 */

import type { FpsStats } from "./fps";
import type { ResourceSummary, ResourceVerdict } from "./verdict";

/** Greppable banner line emitted when the verdict is starved. */
export const RESOURCE_STARVED_BANNER = "RESOURCE_STARVED";
/** Counterpart banner for a clean run. */
export const RESOURCE_OK_BANNER = "RESOURCE_OK";

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
  /** `false` when the sampler ran on a box without /proc (non-Linux). */
  supported: boolean;
  /** `true` when the sysstat tools were absent (a capability note, not a failure). */
  sysstatMissing: boolean;
  rawCsvPath: string;
  derivedCsvPath: string;
}

/**
 * Build the multi-line report. A dedicated banner line — `RESOURCE_STARVED` or
 * `RESOURCE_OK` — is emitted near the top (right after the opening divider) on
 * its own line, so a one-line grep classifies the run.
 */
export function formatResourceReport(input: ReportInput): string {
  const { summary: s, verdict, fpsByBot } = input;
  const lines: string[] = [];

  lines.push("──────────────────────────────────────────────────────────────");
  if (!input.supported) {
    lines.push(`${RESOURCE_OK_BANNER} (resource capture unsupported on this box — no /proc)`);
    lines.push("──────────────────────────────────────────────────────────────");
    return lines.join("\n");
  }
  if (s.sampleCount === 0) {
    lines.push(`${RESOURCE_OK_BANNER} (no resource samples captured)`);
    lines.push("──────────────────────────────────────────────────────────────");
    return lines.join("\n");
  }

  lines.push(verdict.starved ? RESOURCE_STARVED_BANNER : RESOURCE_OK_BANNER);
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
  } else {
    lines.push(`[resource] VERDICT: ${RESOURCE_OK_BANNER} — box had headroom for this run.`);
  }
  lines.push(`[resource] raw counters: ${input.rawCsvPath}`);
  lines.push(`[resource] derived csv:  ${input.derivedCsvPath}`);
  lines.push("──────────────────────────────────────────────────────────────");
  return lines.join("\n");
}
