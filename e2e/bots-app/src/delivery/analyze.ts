import { type BotLog, ReportInputError } from "./parse";
import type { PrometheusReport } from "./prometheus";
import { HEARD_LEAD_SLACK_MS, HEARD_TAIL_MS, type Stretch } from "./schedule";

export const JOIN_SETTLE_MS = 60_000;
export const END_MARGIN_MS = 5_000;
export const MIN_RECEIVERS_FOR_LATE = 5;

export type StretchStatus = "scored" | "publisher-muted" | "publisher-no-log";

export interface ReceiverOnset {
  receiver: number;
  /** First `on` edge minus the stretch start; null when MISSED. */
  onsetMs: number | null;
  /** `onsetMs` minus the stretch's median onset; null below MIN_RECEIVERS_FOR_LATE onsets. */
  offsetMs: number | null;
  flag: "MISSED" | "LATE_RECEIVER" | null;
}

export interface StretchResult extends Stretch {
  status: StretchStatus;
  receivers: ReceiverOnset[];
  medianOnsetMs: number | null;
  /** Publisher mic-on edge minus the stretch start when the mic came on inside it, else 0; LATE_PUBLISHER reads onsets from there. */
  micOnMs: number;
  latePublisher: boolean;
}

export interface ReceiverSummary {
  receiver: number;
  scored: number;
  heard: number;
  medianOffsetMs: number | null;
  p90OffsetMs: number | null;
  lateReceiver: number;
}

export interface PublisherSummary {
  publisher: number;
  scored: number;
  muted: number;
  medianOnsetMs: number | null;
  latePublisher: number;
}

export interface DeliveryReport extends AnalysisWindow {
  lateMs: number;
  fleetSize: number;
  baselineOnsetMs: number | null;
  unjudged: number;
  stretches: StretchResult[];
  receivers: ReceiverSummary[];
  publishers: PublisherSummary[];
  flags: number;
  /** Publishers with stretches in the window, every one of them muted. */
  mutedPublishers: number[];
  /** Present only with `--prometheus`. */
  prometheus?: PrometheusReport;
}

/** Linear-interpolated quantile; null for an empty list. */
export function quantile(values: readonly number[], q: number): number | null {
  if (values.length === 0) return null;
  const v = [...values].sort((a, b) => a - b);
  const pos = (v.length - 1) * q;
  const lo = Math.floor(pos);
  return v[lo] + (v[Math.ceil(pos)] - v[lo]) * (pos - lo);
}

export interface AnalysisWindow {
  fromMs: number;
  toMs: number;
  /** What set each end: a bot's log, or the flag. */
  fromBy: string;
  toBy: string;
}

export function analysisWindow(
  bots: readonly BotLog[],
  from?: number,
  to?: number,
): AnalysisWindow {
  const joined = bots.reduce((a, b) => (b.firstMs > a.firstMs ? b : a));
  const ended = bots.reduce((a, b) => (b.lastMs < a.lastMs ? b : a));
  const w: AnalysisWindow = {
    fromMs: joined.firstMs + JOIN_SETTLE_MS,
    toMs: ended.lastMs - END_MARGIN_MS,
    fromBy: `bot-${joined.ordinal} first line + ${JOIN_SETTLE_MS / 1000} s`,
    toBy: `bot-${ended.ordinal} last line - ${END_MARGIN_MS / 1000} s`,
  };
  if (from !== undefined && from > w.fromMs) Object.assign(w, { fromMs: from, fromBy: "--from" });
  if (to !== undefined && to < w.toMs) Object.assign(w, { toMs: to, toBy: "--to" });
  if (!(w.fromMs < w.toMs)) throw new ReportInputError("the analysis window is empty");
  return w;
}

/** `size`, once every logged ordinal sits below it. */
export function resolveFleetSize(bots: readonly BotLog[], size: number): number {
  const outside = bots.map((b) => b.ordinal).filter((o) => o >= size);
  if (outside.length > 0) {
    throw new ReportInputError(
      `bot-${outside.join(", bot-")} outside a fleet of ${size}; pass the replica count as --fleet-size`,
    );
  }
  return size;
}

/** A stretch is not scored when its publisher's mic is off for longer than this within it. */
export const MUTED_OFF_MS = 3_000;

function micOffMs(bot: BotLog, s: Stretch): number {
  let on = false;
  let at = s.startMs;
  let off = 0;
  for (const m of bot.mic) {
    if (m.ms >= s.endMs) break;
    if (m.ms > s.startMs) {
      if (!on) off += m.ms - at;
      at = m.ms;
    }
    on = m.on;
  }
  return on ? off : off + s.endMs - at;
}

/** The last mic line at or before the stretch start if it shows the mic on, else the first mic-on line inside the stretch; null if neither. */
function micOnEdge(bot: BotLog, s: Stretch): number | null {
  let edge: number | null = null;
  for (const m of bot.mic) {
    if (m.ms >= s.endMs || (m.ms > s.startMs && edge !== null)) break;
    edge = m.on ? m.ms : null;
  }
  return edge;
}

function firstOn(receiver: BotLog, publisher: BotLog, s: Stretch): number | null {
  let first: number | null = null;
  for (const id of publisher.sessionIds) {
    for (const t of receiver.onEdges.get(id) ?? []) {
      if (
        t >= s.startMs - HEARD_LEAD_SLACK_MS &&
        t <= s.endMs + HEARD_TAIL_MS &&
        (first === null || t < first)
      ) {
        first = t;
      }
    }
  }
  return first === null ? null : first - s.startMs;
}

export function analyze(
  bots: readonly BotLog[],
  stretches: readonly Stretch[],
  window: AnalysisWindow,
  fleetSize: number,
  lateMs: number,
): DeliveryReport {
  const byOrdinal = new Map(bots.map((b) => [b.ordinal, b]));
  const results: StretchResult[] = stretches.map((s) => {
    const publisher = byOrdinal.get(s.speaker);
    const base = { ...s, receivers: [], medianOnsetMs: null, micOnMs: 0, latePublisher: false };
    if (!publisher) return { ...base, status: "publisher-no-log" };
    if (micOffMs(publisher, s) > MUTED_OFF_MS) return { ...base, status: "publisher-muted" };
    const receivers: ReceiverOnset[] = bots
      .filter((b) => b.ordinal !== s.speaker)
      .map((r) => {
        const onsetMs = firstOn(r, publisher, s);
        return {
          receiver: r.ordinal,
          onsetMs,
          offsetMs: null,
          flag: onsetMs === null ? "MISSED" : null,
        };
      });
    const onsets = receivers.flatMap((r) => (r.onsetMs === null ? [] : [r.onsetMs]));
    const medianOnsetMs = quantile(onsets, 0.5);
    if (onsets.length >= MIN_RECEIVERS_FOR_LATE && medianOnsetMs !== null) {
      for (const r of receivers) {
        if (r.onsetMs === null) continue;
        r.offsetMs = r.onsetMs - medianOnsetMs;
        if (r.offsetMs > lateMs) r.flag = "LATE_RECEIVER";
      }
    }
    const edge = micOnEdge(publisher, s);
    const micOnMs = edge === null ? 0 : Math.max(s.startMs, edge) - s.startMs;
    return { ...base, status: "scored", receivers, medianOnsetMs, micOnMs };
  });
  const scored = results.filter((r) => r.status === "scored");
  const judged = scored.filter(
    (r) => r.receivers.filter((x) => x.onsetMs !== null).length >= MIN_RECEIVERS_FOR_LATE,
  );
  const fromMicOn = (r: StretchResult): number[] =>
    r.medianOnsetMs === null ? [] : [r.medianOnsetMs - r.micOnMs];
  const baselineOnsetMs = quantile(judged.flatMap(fromMicOn), 0.5);
  for (const r of judged) {
    const [onset] = fromMicOn(r);
    if (onset !== undefined && baselineOnsetMs !== null) {
      r.latePublisher = onset - baselineOnsetMs > lateMs;
    }
  }
  const receivers = bots.map((b): ReceiverSummary => {
    const rows = scored.flatMap((s) => s.receivers.filter((r) => r.receiver === b.ordinal));
    const offsets = rows.flatMap((r) => (r.offsetMs === null ? [] : [r.offsetMs]));
    return {
      receiver: b.ordinal,
      scored: rows.length,
      heard: rows.filter((r) => r.onsetMs !== null).length,
      medianOffsetMs: quantile(offsets, 0.5),
      p90OffsetMs: quantile(offsets, 0.9),
      lateReceiver: rows.filter((r) => r.flag === "LATE_RECEIVER").length,
    };
  });
  const publishers = Array.from({ length: fleetSize }, (_, p): PublisherSummary => {
    const own = results.filter((s) => s.speaker === p);
    const ownScored = own.filter((s) => s.status === "scored");
    return {
      publisher: p,
      scored: ownScored.length,
      muted: own.filter((s) => s.status === "publisher-muted").length,
      medianOnsetMs: quantile(
        ownScored.flatMap((s) => (s.medianOnsetMs === null ? [] : [s.medianOnsetMs])),
        0.5,
      ),
      latePublisher: ownScored.filter((s) => s.latePublisher).length,
    };
  });
  const flags =
    scored.filter((s) => s.latePublisher).length +
    scored.reduce((n, s) => n + s.receivers.filter((r) => r.flag !== null).length, 0);
  return {
    ...window,
    lateMs,
    fleetSize,
    baselineOnsetMs,
    unjudged: scored.length - judged.length,
    stretches: results,
    receivers,
    publishers,
    flags,
    mutedPublishers: publishers
      .filter((p) => p.muted > 0 && p.scored === 0)
      .map((p) => p.publisher),
  };
}
