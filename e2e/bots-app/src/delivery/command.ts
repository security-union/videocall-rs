import type { Command } from "commander";

import { taggedLine } from "../log-line";
import { SPEECH_PLAN_DEFAULT } from "../speech-plan";
import { analysisWindow, analyze, type DeliveryReport, resolveFleetSize } from "./analyze";
import { formatReport } from "./format";
import { type BotLog, botsByOrdinal, type EdgeRange, readLogDir, ReportInputError } from "./parse";
import {
  fetchPromSamples,
  promFlagged,
  type PromInput,
  prometheusUrl,
  scorePrometheus,
} from "./prometheus";
import { HEARD_LEAD_SLACK_MS, planFromRecords, stretchesBetween } from "./schedule";

export interface ReportOptions {
  logs: string;
  fleetSize?: string;
  records?: string;
  from?: string;
  to?: string;
  lateMs: string;
  failOnFlag?: boolean;
  json?: boolean;
  prometheus?: string;
}

const ISO_UTC = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d{1,3})?)?Z$/;

function isoMs(flag: string, raw: string | undefined): number | undefined {
  if (raw === undefined) return undefined;
  if (!ISO_UTC.test(raw)) throw new ReportInputError(`${flag} must be ISO 8601 UTC, got "${raw}"`);
  const ms = Date.parse(raw);
  if (Number.isNaN(ms)) throw new ReportInputError(`${flag} is not a valid date, got "${raw}"`);
  return ms;
}

function positiveInt(flag: string, raw: string): number {
  if (!/^[1-9]\d{0,5}$/.test(raw)) {
    throw new ReportInputError(`${flag} must be a positive integer, got "${raw}"`);
  }
  return Number(raw);
}

/** The on edges a window inside --from/--to can score: a stretch keeps `endMs + HEARD_TAIL_MS <= to`. */
export function edgeRange(from?: number, to?: number): EdgeRange {
  return { fromMs: (from ?? -Infinity) - HEARD_LEAD_SLACK_MS, toMs: to ?? Infinity };
}

interface Prepared {
  bots: BotLog[];
  report: DeliveryReport;
}

function prepare(opts: ReportOptions, speechScript?: string): Prepared {
  const from = isoMs("--from", opts.from);
  const to = isoMs("--to", opts.to);
  const lateMs = positiveInt("--late-ms", opts.lateMs);
  if (opts.fleetSize === undefined) {
    throw new ReportInputError("--fleet-size is required: pass the fleet's replica count");
  }
  const requested = positiveInt("--fleet-size", opts.fleetSize);
  const sessions = readLogDir(opts.logs, edgeRange(from, to)).filter(
    (s) => s.lastMs >= (from ?? -Infinity) && s.firstMs <= (to ?? Infinity),
  );
  const bots = botsByOrdinal(sessions);
  if (bots.length === 0) throw new ReportInputError("no session overlaps --from/--to");
  if (bots.every((b) => b.peerSpeakingLines === 0)) {
    throw new ReportInputError(
      "no [PEER_SPEAKING] lines: the client predates them or logs above info",
    );
  }
  const fleetSize = resolveFleetSize(bots, requested);
  const plan = opts.records ? planFromRecords(opts.records) : SPEECH_PLAN_DEFAULT;
  const window = analysisWindow(bots, from, to);
  const stretches = stretchesBetween(plan, fleetSize, window.fromMs, window.toMs, speechScript);
  return { bots, report: analyze(bots, stretches, window, fleetSize, lateMs) };
}

function finish(
  { bots, report }: Prepared,
  opts: ReportOptions,
  out: (line: string) => void,
  prom?: PromInput,
): number {
  if (prom) {
    report.prometheus = scorePrometheus(report.stretches, bots, prom);
    report.flags += report.prometheus.flags;
  }
  out(opts.json ? JSON.stringify(report, null, 2) : formatReport(report));
  if (report.prometheus?.error !== undefined) {
    throw new ReportInputError(`no Prometheus data: ${report.prometheus.error}`);
  }
  if (!report.stretches.some((s) => s.status === "scored")) {
    throw new ReportInputError("no stretch was scored, so this report shows nothing");
  }
  const flagged =
    report.flags > 0 ||
    report.mutedPublishers.length > 0 ||
    (report.prometheus !== undefined && promFlagged(report.prometheus));
  return flagged && opts.failOnFlag ? 1 : 0;
}

/** Runs the report; returns the exit code. `prom` is the Prometheus half, already fetched; `speechScript` replaces the checkout's clock-speech.js. */
export function runReport(
  opts: ReportOptions,
  out: (line: string) => void,
  prom?: PromInput,
  speechScript?: string,
): number {
  return finish(prepare(opts, speechScript), opts, out, prom);
}

/** `runReport` with the Prometheus half fetched from `opts.prometheus`; a failed fetch is reported as no data. */
export async function runReportWithPrometheus(
  opts: ReportOptions & { prometheus: string },
  out: (line: string) => void,
  fetchImpl: typeof fetch = fetch,
): Promise<number> {
  const url = prometheusUrl(opts.prometheus);
  const prepared = prepare(opts);
  const { fromMs, toMs } = prepared.report;
  const prom: PromInput = { url: url.origin + url.pathname };
  try {
    prom.samples = await fetchPromSamples(url, prepared.bots, fromMs, toMs, fetchImpl);
  } catch (err) {
    prom.error = err instanceof Error ? err.message : String(err);
  }
  return finish(prepared, opts, out, prom);
}

export function registerReportCommand(program: Command): void {
  program
    .command("report")
    .description(
      "Read a run's console-log chunks and print, per publisher talking stretch, which receivers " +
        "heard the speech ([PEER_SPEAKING] on edges) and who was late. Reads logs, and Prometheus only with --prometheus. " +
        "Sessions are matched to fleet ordinals by their 'bot-<N>' display name; --fleet-size sets the schedule. " +
        "Exits 0 (1 with --fail-on-flag when anything is flagged, a publisher is muted on all its stretches, " +
        "a Prometheus pair has no data or no Prometheus pair was judged), 2 on an input error, when no stretch was scored " +
        "or when the Prometheus request fails, cannot be read or returns no audio series.",
    )
    .exitOverride((err) => process.exit(err.exitCode === 0 ? 0 : 2))
    .requiredOption("--logs <dir>", "directory of <user>_<epochms>_<chunk>.log.gz chunks")
    .requiredOption(
      "--fleet-size <n>",
      "the run's replica count, which sets the talking-clock schedule; every logged bot-<N> must be below it",
    )
    .option(
      "--records <dir>",
      "read the speech plan from these participant records (default plan otherwise)",
    )
    .option("--from <iso>", "narrow the window start (ISO 8601 UTC)")
    .option("--to <iso>", "narrow the window end (ISO 8601 UTC)")
    .option("--late-ms <ms>", "late threshold", "1000")
    .option(
      "--fail-on-flag",
      "exit 1 when anything is flagged, a publisher is muted on all its stretches, a Prometheus pair has no data or no Prometheus pair was judged",
    )
    .option("--json", "print the report as JSON")
    .option(
      "--prometheus <url>",
      "also read per-pair audio and video delivery from this Prometheus (question 1); exits 2 when the request fails, cannot be read or returns no audio series",
    )
    .action(async (opts: ReportOptions) => {
      const print = (text: string): void => console.log(text);
      try {
        const { prometheus } = opts;
        process.exitCode =
          prometheus === undefined
            ? runReport(opts, print)
            : await runReportWithPrometheus({ ...opts, prometheus }, print);
      } catch (err) {
        console.error(taggedLine("report", err instanceof Error ? err.message : String(err)));
        process.exitCode = 2;
      }
    });
}
