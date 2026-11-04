import { quantile, type StretchResult } from "./analyze";
import { type BotLog, ReportInputError } from "./parse";

export const PROM_AUDIO = "videocall_neteq_packets_per_sec";
export const PROM_VIDEO = "videocall_video_fps_decoder_output";
export const AUDIO_ON_PKTS = 2;
/** A sample is read only this long after the publisher's mic or camera came on: the edge reaches Prometheus late. */
export const PROM_EDGE_LAG_MS = 36_000;
/** Shorter stretches are never judged from Prometheus. */
export const PROM_MIN_STRETCH_MS = 30_000;
/** Audio and video samples from one scrape carry the same timestamp. */
const SAME_SCRAPE_MS = 1_000;
/** One reading shows delivery; a miss needs this many readable samples, none showing it. */
export const PROM_MISSED_MIN_SAMPLES = 2;
const FETCH_TIMEOUT_MS = 30_000;

export type Sample = [ms: number, value: number];
/** Samples keyed by `pairKey(receiver session id, publisher session id)`. */
export interface PromSamples {
  audio: Map<string, Sample[]>;
  video: Map<string, Sample[]>;
}
/** What the report command hands the scorer: samples, or why there are none. */
export interface PromInput {
  url: string;
  samples?: PromSamples;
  error?: string;
}

export type PromVerdict = "seen" | "missed" | "no-data" | "too-short" | "camera-off";

export interface SampleCount {
  read: number;
  on: number;
}

export interface PromPair {
  receiver: number;
  audio: PromVerdict;
  video: PromVerdict;
  /** Readable samples, and how many showed delivery. */
  audioSamples: SampleCount;
  videoSamples: SampleCount;
  logHeard: boolean;
}

export interface PromStretch {
  speaker: number;
  startMs: number;
  endMs: number;
  pairs: PromPair[];
}

export interface PrometheusReport {
  url: string;
  error?: string;
  edgeLagMs: number;
  /** Median gap between consecutive audio samples, pooled over all pairs; null with no gap. */
  sampleSpacingMs: number | null;
  stretches: PromStretch[];
  receivers: Array<{ receiver: number; audio: SampleCount; video: SampleCount }>;
  flags: number;
  noData: number;
  /** Pairs whose audio or video verdict is `seen` or `missed`; video pairs exclude `camera-off`. */
  judged: { audio: number; audioPairs: number; video: number; videoPairs: number };
  disagreements: number;
}

export const pairKey = (receiver: string, publisher: string): string => `${receiver} ${publisher}`;

/** Sub-spans of `[startMs, endMs)` where the state has been on for at least `lagMs`. */
export function readableSpans(
  states: ReadonlyArray<{ ms: number; on: boolean }>,
  startMs: number,
  endMs: number,
  lagMs: number,
): Array<[number, number]> {
  const spans: Array<[number, number]> = [];
  let onSince: number | null = null;
  const close = (at: number): void => {
    if (onSince === null) return;
    const a = Math.max(startMs, onSince + lagMs);
    const b = Math.min(endMs, at);
    if (a < b) spans.push([a, b]);
  };
  for (const s of states) {
    if (s.ms >= endMs) break;
    if (s.on && onSince === null) onSince = s.ms;
    else if (!s.on && onSince !== null) {
      close(s.ms);
      onSince = null;
    }
  }
  close(endMs);
  return spans;
}

function pairSamples(map: Map<string, Sample[]>, receiver: BotLog, publisher: BotLog): Sample[] {
  return receiver.sessionIds.flatMap((r) =>
    publisher.sessionIds.flatMap((p) => map.get(pairKey(r, p)) ?? []),
  );
}

const inSpans = (t: number, spans: Array<[number, number]>): boolean =>
  spans.some(([a, b]) => t >= a && t < b);

const NO_SAMPLES: SampleCount = { read: 0, on: 0 };

function verdict(
  heartbeat: Sample[],
  spans: Array<[number, number]>,
  s: StretchResult,
  seen: (t: number, v: number) => boolean,
): [PromVerdict, SampleCount] {
  const read = heartbeat.filter(([t]) => inSpans(t, spans));
  const count = { read: read.length, on: read.filter(([t, v]) => seen(t, v)).length };
  if (count.on > 0) return ["seen", count];
  if (count.read >= PROM_MISSED_MIN_SAMPLES) return ["missed", count];
  const near = heartbeat.some(
    ([t]) => t >= s.startMs - PROM_EDGE_LAG_MS && t <= s.endMs + PROM_EDGE_LAG_MS,
  );
  return [near ? "too-short" : "no-data", count];
}

function medianGapMs(series: Iterable<Sample[]>): number | null {
  const gaps: number[] = [];
  for (const list of series) {
    const t = list.map(([ms]) => ms).sort((a, b) => a - b);
    for (let i = 1; i < t.length; i += 1) gaps.push(t[i] - t[i - 1]);
  }
  return quantile(gaps, 0.5);
}

/** Question 1 per scored stretch and receiver: audio delivered, video decoded, judged only where a sample can show it. */
export function scorePrometheus(
  stretches: readonly StretchResult[],
  bots: readonly BotLog[],
  input: PromInput,
): PrometheusReport {
  const report: PrometheusReport = {
    url: input.url,
    edgeLagMs: PROM_EDGE_LAG_MS,
    sampleSpacingMs: null,
    stretches: [],
    receivers: [],
    flags: 0,
    noData: 0,
    judged: { audio: 0, audioPairs: 0, video: 0, videoPairs: 0 },
    disagreements: 0,
  };
  if (input.error !== undefined || input.samples === undefined) {
    return { ...report, error: input.error ?? "no samples" };
  }
  const { audio, video } = input.samples;
  report.sampleSpacingMs = medianGapMs(audio.values());
  const byOrdinal = new Map(bots.map((b) => [b.ordinal, b]));
  for (const s of stretches) {
    const publisher = byOrdinal.get(s.speaker);
    if (s.status !== "scored" || !publisher) continue;
    const short = s.endMs - s.startMs < PROM_MIN_STRETCH_MS;
    const micSpans = readableSpans(publisher.mic, s.startMs, s.endMs, PROM_EDGE_LAG_MS);
    const camera = publisher.camera ?? [];
    const camSpans = readableSpans(camera, s.startMs, s.endMs, PROM_EDGE_LAG_MS);
    const cameraOff = !camera.some((c, i, all) => {
      const until = all[i + 1]?.ms ?? Infinity;
      return c.on && c.ms < s.endMs && until > s.startMs;
    });
    const pairs = s.receivers.map((x): PromPair => {
      const receiver = byOrdinal.get(x.receiver);
      const heartbeat = receiver ? pairSamples(audio, receiver, publisher) : [];
      const frames = receiver ? pairSamples(video, receiver, publisher) : [];
      const decoded = (t: number): boolean =>
        frames.some(([ft, v]) => Math.abs(ft - t) <= SAME_SCRAPE_MS && v > 0);
      const [audioVerdict, audioSamples] = short
        ? (["too-short", NO_SAMPLES] as const)
        : verdict(heartbeat, micSpans, s, (_, v) => v > AUDIO_ON_PKTS);
      const [videoVerdict, videoSamples] = short
        ? (["too-short", NO_SAMPLES] as const)
        : cameraOff
          ? (["camera-off", NO_SAMPLES] as const)
          : video.size === 0
            ? (["no-data", NO_SAMPLES] as const)
            : verdict(heartbeat, camSpans, s, (t) => decoded(t));
      const pair: PromPair = {
        receiver: x.receiver,
        audio: audioVerdict,
        video: videoVerdict,
        audioSamples,
        videoSamples,
        logHeard: x.onsetMs !== null,
      };
      for (const v of [pair.audio, pair.video]) {
        if (v === "missed") report.flags += 1;
        if (v === "no-data") report.noData += 1;
      }
      const judged = (v: PromVerdict): number => (v === "seen" || v === "missed" ? 1 : 0);
      report.judged.audio += judged(pair.audio);
      report.judged.audioPairs += 1;
      report.judged.video += judged(pair.video);
      if (!cameraOff) report.judged.videoPairs += 1;
      if ((pair.audio === "missed" && pair.logHeard) || (pair.audio === "seen" && !pair.logHeard)) {
        report.disagreements += 1;
      }
      return pair;
    });
    report.stretches.push({ speaker: s.speaker, startMs: s.startMs, endMs: s.endMs, pairs });
  }
  const add = (a: SampleCount, b: SampleCount): SampleCount => ({
    read: a.read + b.read,
    on: a.on + b.on,
  });
  report.receivers = bots.map((b) => {
    const rows = report.stretches.flatMap((s) => s.pairs.filter((p) => p.receiver === b.ordinal));
    return {
      receiver: b.ordinal,
      audio: rows.map((p) => p.audioSamples).reduce(add, NO_SAMPLES),
      video: rows.map((p) => p.videoSamples).reduce(add, NO_SAMPLES),
    };
  });
  return report;
}

export const promFlagged = (p: PrometheusReport): boolean =>
  p.noData > 0 || p.judged.audio + p.judged.video === 0;

/** Throws on a URL that is not plain http(s) or that carries credentials. */
export function prometheusUrl(raw: string): URL {
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    throw new ReportInputError(`--prometheus is not a URL, got "${raw}"`);
  }
  if (!/^https?:$/.test(url.protocol)) throw new ReportInputError("--prometheus must be http(s)");
  if (url.username || url.password) {
    throw new ReportInputError("--prometheus must not carry credentials");
  }
  if (/[?#]/.test(url.href)) {
    throw new ReportInputError("--prometheus must not carry a query or fragment");
  }
  return url;
}

interface Matrix {
  status?: string;
  error?: string;
  data?: {
    resultType?: string;
    result?: Array<{ metric: Record<string, string>; values: Array<[number, string]> }>;
  };
}

async function rawSamples(
  url: URL,
  metric: string,
  ids: string[],
  fromMs: number,
  toMs: number,
  fetchImpl: typeof fetch,
): Promise<Map<string, Sample[]>> {
  const range = Math.ceil((toMs - fromMs) / 1000);
  const body = new URLSearchParams({
    query: `${metric}{to_peer=~"${ids.join("|")}"}[${range}s]`,
    time: String(toMs / 1000),
  });
  const res = await fetchImpl(new URL("api/v1/query", url.href.replace(/\/?$/, "/")), {
    method: "POST",
    body,
    signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
  });
  let json: Matrix;
  try {
    json = (await res.json()) as Matrix;
  } catch (err) {
    if (!res.ok) throw new Error(`${metric}: HTTP ${res.status}`, { cause: err });
    const msg = err instanceof Error ? err.message : String(err);
    const tooLong =
      err instanceof RangeError ||
      (err as { code?: unknown } | null)?.code === "ERR_STRING_TOO_LONG";
    const hint = tooLong ? "; narrow --from/--to" : "";
    throw new Error(`${metric}: could not read the response (${msg})${hint}`, { cause: err });
  }
  if (!res.ok || json.status !== "success" || json.data?.resultType !== "matrix") {
    throw new Error(`${metric}: HTTP ${res.status} ${json.error ?? json.status ?? ""}`.trim());
  }
  const known = new Set(ids);
  const out = new Map<string, Sample[]>();
  for (const r of json.data.result ?? []) {
    const receiver = r.metric.session_id;
    if (!known.has(receiver) || !known.has(r.metric.to_peer)) continue;
    const key = pairKey(receiver, r.metric.to_peer);
    const list = out.get(key) ?? [];
    for (const [t, v] of r.values) list.push([t * 1000, Number(v)]);
    out.set(key, list);
  }
  return out;
}

/** Raw samples of both series for the run's session ids; throws on a failed or unreadable request or no audio series. */
export async function fetchPromSamples(
  url: URL,
  bots: readonly BotLog[],
  fromMs: number,
  toMs: number,
  fetchImpl: typeof fetch = fetch,
): Promise<PromSamples> {
  const ids = bots.flatMap((b) => b.sessionIds);
  const lead = fromMs - PROM_EDGE_LAG_MS;
  const tail = toMs + PROM_EDGE_LAG_MS;
  const [audio, video] = await Promise.all(
    [PROM_AUDIO, PROM_VIDEO].map((m) => rawSamples(url, m, ids, lead, tail, fetchImpl)),
  );
  if (audio.size === 0) throw new Error(`no ${PROM_AUDIO} series for the run's session ids`);
  return { audio, video };
}
