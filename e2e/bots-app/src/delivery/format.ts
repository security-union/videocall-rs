import { type DeliveryReport, MIN_RECEIVERS_FOR_LATE, type StretchResult } from "./analyze";
import {
  AUDIO_ON_PKTS,
  PROM_AUDIO,
  PROM_VIDEO,
  type PrometheusReport,
  type PromPair,
  type PromStretch,
  type PromVerdict,
} from "./prometheus";

const clock = (ms: number): string => new Date(ms).toISOString().slice(11, 23);
const secs = (ms: number | null, sign = true): string =>
  ms === null ? "-" : `${sign && ms >= 0 ? "+" : ""}${(ms / 1000).toFixed(2)}s`;

function stretchLine(s: StretchResult): string {
  const head = `${clock(s.startMs)} ${((s.endMs - s.startMs) / 1000).toFixed(1).padStart(5)}s bot-${s.speaker}`;
  if (s.status === "publisher-muted") return `${head}  publisher muted, not scored`;
  if (s.status === "publisher-no-log") return `${head}  publisher has no log, not scored`;
  const heard = s.receivers.filter((r) => r.onsetMs !== null).length;
  const flags = [
    ...(s.latePublisher ? [`LATE_PUBLISHER ${secs(s.medianOnsetMs)}`] : []),
    ...s.receivers.flatMap((r) =>
      r.flag === "MISSED"
        ? [`MISSED bot-${r.receiver}`]
        : r.flag === "LATE_RECEIVER"
          ? [`LATE_RECEIVER bot-${r.receiver} ${secs(r.offsetMs)}`]
          : [],
    ),
  ];
  return `${head}  heard ${heard}/${s.receivers.length} median ${secs(s.medianOnsetMs)}  ${flags.join("  ")}`.trimEnd();
}

function promColumn(pairs: PromPair[], kind: "audio" | "video"): string {
  const n = (v: PromVerdict): number => pairs.filter((p) => p[kind] === v).length;
  if (n("too-short") === pairs.length) return "too short";
  if (n("camera-off") === pairs.length) return "camera off";
  const judged = n("seen") + n("missed");
  const extra = [
    ...(n("no-data") > 0 ? [`no data ${n("no-data")}`] : []),
    ...(n("too-short") > 0 ? [`too short ${n("too-short")}`] : []),
  ];
  return [judged === 0 ? "-" : `${n("seen")}/${judged}`, ...extra].join(" ");
}

function promLine(s: PromStretch): string {
  const flags = s.pairs.flatMap((p) => [
    ...(p.audio === "missed"
      ? [`PROM_AUDIO_MISSED bot-${p.receiver}${p.logHeard ? " (log: heard)" : ""}`]
      : []),
    ...(p.audio === "seen" && !p.logHeard
      ? [`PROM_AUDIO_SEEN bot-${p.receiver} (log: MISSED)`]
      : []),
    ...(p.video === "missed" ? [`PROM_VIDEO_MISSED bot-${p.receiver}`] : []),
  ]);
  return `${clock(s.startMs)} ${((s.endMs - s.startMs) / 1000).toFixed(1).padStart(5)}s bot-${s.speaker}  audio ${promColumn(s.pairs, "audio")}  video ${promColumn(s.pairs, "video")}  ${flags.join("  ")}`.trimEnd();
}

function promSection(p: PrometheusReport): string[] {
  if (p.error !== undefined) {
    return [`Prometheus ${p.url}: no data (${p.error}); audio and video delivery not checked`];
  }
  const j = p.judged;
  return [
    `Prometheus ${p.url} (delivered at all): audio ${PROM_AUDIO} > ${AUDIO_ON_PKTS}, video ${PROM_VIDEO} > 0, ` +
      `a sample counts only from ${p.edgeLagMs / 1000} s after the publisher's mic or camera came on`,
    `audio judged on ${j.audio} of ${j.audioPairs} pairs, video on ${j.video} of ${j.videoPairs} pairs with the camera on; ` +
      `${p.flags} Prometheus flag(s) cover only those, so an unjudged pair is not a pass`,
    `${p.noData} pair(s) with no data, ${p.disagreements} audio disagreement(s) with the log`,
    "",
    "start (UTC)  length publisher  audio seen/judged  video seen/judged  flags",
    ...p.stretches.map(promLine),
    "",
    `receiver  audio  video  (readable samples showing delivery / readable samples, ${secs(p.sampleSpacingMs, false)} apart)`,
    ...p.receivers.map(
      (x) =>
        `bot-${x.receiver}`.padEnd(10) +
        `${x.audio.on}/${x.audio.read}`.padStart(5) +
        `${x.video.on}/${x.video.read}`.padStart(7),
    ),
  ];
}

export function formatReport(r: DeliveryReport): string {
  const scored = r.stretches.filter((s) => s.status === "scored").length;
  const lines = [
    `window ${new Date(r.fromMs).toISOString()} (${r.fromBy}) .. ${new Date(r.toMs).toISOString()} (${r.toBy}), fleet ${r.fleetSize}, ` +
      `${scored} of ${r.stretches.length} stretches scored, late threshold ${secs(r.lateMs, false)}, ` +
      `baseline onset ${secs(r.baselineOnsetMs)}`,
    r.prometheus === undefined
      ? "speech (audio) only: video delivery is not checked"
      : "speech (audio) from the logs; audio and video delivery from Prometheus below",
    ...(r.unjudged > 0
      ? [
          `${r.unjudged} of ${scored} stretches had fewer than ${MIN_RECEIVERS_FOR_LATE} receiver onsets: LATE_RECEIVER and LATE_PUBLISHER were not judged on them`,
        ]
      : []),
    ...(r.stretches.some((s) => s.status === "scored" && s.micOnMs > 0)
      ? [
          "LATE_PUBLISHER and the baseline measure a stretch whose publisher's mic came on after its start from that mic-on edge; the printed median does not",
        ]
      : []),
    ...(r.mutedPublishers.length > 0
      ? [`muted on all its stretches: bot-${r.mutedPublishers.join(", bot-")}`]
      : []),
    "",
    "start (UTC)  length publisher  heard  median onset vs schedule  flags",
    ...r.stretches.map(stretchLine),
    "",
    "receiver  scored  heard%  median/p90 vs median receiver  LATE_RECEIVER",
    ...r.receivers.map(
      (x) =>
        `bot-${x.receiver}`.padEnd(10) +
        String(x.scored).padStart(6) +
        (x.scored === 0 ? "-" : ((100 * x.heard) / x.scored).toFixed(0)).padStart(8) +
        `  ${secs(x.medianOffsetMs)} / ${secs(x.p90OffsetMs)}`.padEnd(32) +
        String(x.lateReceiver).padStart(4),
    ),
    "",
    "publisher  scored  muted  median onset vs schedule  LATE_PUBLISHER",
    ...r.publishers.map(
      (p) =>
        `bot-${p.publisher}`.padEnd(11) +
        String(p.scored).padStart(6) +
        String(p.muted).padStart(7) +
        `  ${secs(p.medianOnsetMs)}`.padEnd(26) +
        String(p.latePublisher).padStart(4),
    ),
    "",
    ...(r.prometheus === undefined ? [] : [...promSection(r.prometheus), ""]),
    `${r.flags} flag(s)`,
  ];
  return lines.join("\n");
}
