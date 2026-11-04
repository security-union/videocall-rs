import { writeFileSync } from "node:fs";
import { join } from "node:path";

import { afterAll, describe, expect, it } from "vitest";

import type { DeliveryReport, StretchResult } from "./analyze";
import { runReport, runReportWithPrometheus, type ReportOptions } from "./command";
import { ROTATION_PLAN } from "../speech-plan";
import {
  cleanTempDirs,
  ONSET_MS,
  sessionId,
  type SynthOptions,
  T0,
  T1,
  tempDir,
  writeSynthDir,
} from "./fixtures/synthetic";
import { type BotLog, ReportInputError } from "./parse";
import {
  fetchPromSamples,
  pairKey,
  PROM_AUDIO,
  PROM_VIDEO,
  promFlagged,
  type PromInput,
  type PromSamples,
  readableSpans,
  type Sample,
  scorePrometheus,
} from "./prometheus";

afterAll(cleanTempDirs);

const id = (bot: number): string => `7${bot}`;
const bot = (ordinal: number, over: Partial<BotLog> = {}): BotLog => ({
  ordinal,
  stem: `b${ordinal}`,
  sessionIds: [id(ordinal)],
  firstMs: 0,
  lastMs: 1e9,
  onEdges: new Map(),
  peerSpeakingLines: 1,
  mic: [{ ms: 0, on: true }],
  camera: [{ ms: 0, on: true }],
  ...over,
});
const stretch = (startMs: number, endMs: number, heard = true): StretchResult => ({
  speaker: 0,
  startMs,
  endMs,
  leadMs: 0,
  status: "scored",
  receivers: [1, 2].map((receiver) => ({
    receiver,
    onsetMs: heard ? 200 : null,
    offsetMs: null,
    flag: heard ? null : "MISSED",
  })),
  medianOnsetMs: null,
  micOnMs: 0,
  latePublisher: false,
});
const every = (stepMs: number, value: number, from = 0, to = 300_000): Sample[] =>
  Array.from({ length: Math.floor((to - from) / stepMs) + 1 }, (_, i) => [
    from + i * stepMs,
    value,
  ]);
/** Publisher bot-0 to receivers 1 and 2, the same samples for each pair unless overridden. */
function samples(
  audio: Sample[],
  video: Sample[],
  over: { audio?: Record<number, Sample[]>; video?: Record<number, Sample[]> } = {},
): PromInput {
  const map = (base: Sample[], o: Record<number, Sample[]> = {}): Map<string, Sample[]> =>
    new Map([1, 2].map((r) => [pairKey(id(r), id(0)), o[r] ?? base]));
  return { url: "u", samples: { audio: map(audio, over.audio), video: map(video, over.video) } };
}
const verdicts = (input: PromInput, s: StretchResult, publisher = bot(0)) => {
  const r = scorePrometheus([s], [publisher, bot(1), bot(2)], input);
  return { r, pairs: r.stretches[0]?.pairs.map((p) => [p.audio, p.video]) };
};

describe("readableSpans", () => {
  it("opens a span only after the state has been on for the lag, and restarts the lag after an off", () => {
    const states = [
      { ms: 0, on: true },
      { ms: 50_000, on: false },
      { ms: 70_000, on: true },
    ];
    expect(readableSpans(states, 0, 200_000, 10_000)).toEqual([
      [10_000, 50_000],
      [80_000, 200_000],
    ]);
  });
});

describe("scorePrometheus", () => {
  it("reads audio and video as seen when the readable samples show delivery", () => {
    const { r, pairs } = verdicts(
      samples(every(15_000, 50), every(15_000, 15)),
      stretch(100_000, 160_000),
    );
    expect(pairs).toEqual([
      ["seen", "seen"],
      ["seen", "seen"],
    ]);
    expect([r.flags, r.noData, r.sampleSpacingMs]).toEqual([0, 0, 15_000]);
    expect(r.judged).toEqual({ audio: 2, audioPairs: 2, video: 2, videoPairs: 2 });
    expect(r.receivers.find((x) => x.receiver === 1)?.audio).toEqual({ read: 4, on: 4 });
  });

  it("reads an absent video series as camera off, not a miss, while the publisher's camera is off", () => {
    const off = bot(0, {
      camera: [
        { ms: 0, on: true },
        { ms: 90_000, on: false },
      ],
    });
    const { r, pairs } = verdicts(
      samples(every(15_000, 50), [], { video: { 2: every(15_000, 15) } }),
      stretch(100_000, 160_000),
      off,
    );
    expect(pairs?.map(([, v]) => v)).toEqual(["camera-off", "camera-off"]);
    expect(r.flags).toBe(0);
    expect(r.judged).toEqual({ audio: 2, audioPairs: 2, video: 0, videoPairs: 0 });
  });

  it("flags PROM_VIDEO_MISSED when the camera is on and the receiver reports audio but no decoded video", () => {
    const { r, pairs } = verdicts(
      samples(every(15_000, 50), every(15_000, 15), { video: { 1: every(15_000, 15, 0, 60_000) } }),
      stretch(100_000, 160_000),
    );
    expect(pairs).toEqual([
      ["seen", "missed"],
      ["seen", "seen"],
    ]);
    expect(r.flags).toBe(1);
  });

  it("does not judge video on samples taken within the edge lag of the camera coming on", () => {
    const late = bot(0, {
      camera: [
        { ms: 0, on: false },
        { ms: 130_000, on: true },
      ],
    });
    const { r, pairs } = verdicts(samples(every(15_000, 50), []), stretch(100_000, 160_000), late);
    expect(pairs?.map(([, v]) => v)).toEqual(["too-short", "too-short"]);
    expect(r.flags).toBe(0);
  });

  it("does not judge audio on samples taken within the edge lag of the mic coming on", () => {
    const late = bot(0, {
      mic: [
        { ms: 0, on: false },
        { ms: 130_000, on: true },
      ],
    });
    const { r, pairs } = verdicts(
      samples(every(15_000, 0), every(15_000, 15)),
      stretch(100_000, 160_000),
      late,
    );
    expect(pairs?.map(([a]) => a)).toEqual(["too-short", "too-short"]);
    expect(r.flags).toBe(0);
  });

  it("calls an audio miss on two readable samples reading off, but not on one", () => {
    const two = verdicts(samples(every(15_000, 0), every(15_000, 15)), stretch(100_000, 160_000));
    expect(two.pairs?.map(([a]) => a)).toEqual(["missed", "missed"]);
    const one = verdicts(samples(every(30_000, 0), every(30_000, 15)), stretch(100_000, 131_000));
    expect(one.pairs?.map(([a]) => a)).toEqual(["too-short", "too-short"]);
    expect([two.r.flags, one.r.flags]).toEqual([2, 0]);
    expect([two.r.judged.audio, one.r.judged.audio]).toEqual([2, 0]);
  });

  it("never judges a stretch under 30 s", () => {
    const { r, pairs } = verdicts(samples(every(5_000, 0), []), stretch(100_000, 125_000));
    expect(pairs).toEqual([
      ["too-short", "too-short"],
      ["too-short", "too-short"],
    ]);
    expect(r.flags).toBe(0);
  });

  it("reports a pair with no audio sample near the stretch as no data, never seen or missed", () => {
    const { r, pairs } = verdicts(
      samples(every(15_000, 50), every(15_000, 15), { audio: { 1: [] } }),
      stretch(100_000, 160_000),
    );
    expect(pairs?.[0]).toEqual(["no-data", "no-data"]);
    expect([r.flags, r.noData]).toEqual([0, 2]);
  });

  it("reads video as no data, not a miss, when Prometheus has no decoded-video series at all", () => {
    const input = samples(every(15_000, 50), []);
    input.samples?.video.clear();
    const { r, pairs } = verdicts(input, stretch(100_000, 160_000));
    expect(pairs?.map(([, v]) => v)).toEqual(["no-data", "no-data"]);
    expect([r.flags, r.noData]).toEqual([0, 2]);
  });

  it("counts an audio answer that contradicts the log as a disagreement", () => {
    const heardMissed = verdicts(
      samples(every(15_000, 0), every(15_000, 15)),
      stretch(100_000, 160_000, true),
    );
    const missedSeen = verdicts(
      samples(every(15_000, 50), every(15_000, 15)),
      stretch(100_000, 160_000, false),
    );
    expect([heardMissed.r.disagreements, missedSeen.r.disagreements]).toEqual([2, 2]);
  });

  it("does not fail --fail-on-flag on one kind judged and the other not", () => {
    const { r } = verdicts(
      samples(every(15_000, 50), every(15_000, 15)),
      stretch(100_000, 160_000),
    );
    const judged = (audio: number, video: number) => ({
      ...r,
      judged: { ...r.judged, audio, video },
    });
    expect(promFlagged(judged(0, 1))).toBe(false);
    expect(promFlagged(judged(1, 0))).toBe(false);
  });

  it("carries a fetch error through with no stretches and no flags", () => {
    const r = scorePrometheus([stretch(100_000, 160_000)], [bot(0), bot(1), bot(2)], {
      url: "u",
      error: "boom",
    });
    expect([r.error, r.stretches.length, r.flags]).toEqual(["boom", 0, 0]);
  });
});

describe("scorePrometheus boundaries", () => {
  const audioOf = (input: PromInput, s: StretchResult, publisher = bot(0)) =>
    verdicts(input, s, publisher).pairs?.map(([a]) => a);
  const video = every(15_000, 15);

  it("judges a stretch of exactly 30 s and not one of 29.999 s", () => {
    const input = samples(every(15_000, 50), video);
    expect(verdicts(input, stretch(100_000, 130_000)).pairs).toEqual([
      ["seen", "seen"],
      ["seen", "seen"],
    ]);
    expect(audioOf(input, stretch(100_000, 129_999))).toEqual(["too-short", "too-short"]);
  });

  it("calls a miss on exactly 2 readable samples", () => {
    expect(audioOf(samples(every(30_000, 0), video), stretch(100_000, 160_000))).toEqual([
      "missed",
      "missed",
    ]);
  });

  it("reads a sample from exactly 36 s after the mic came on, and not 1 ms before", () => {
    const late = bot(0, {
      mic: [
        { ms: 0, on: false },
        { ms: 100_000, on: true },
      ],
    });
    const at = (t: number) => audioOf(samples([[t, 50]], video), stretch(100_000, 160_000), late);
    expect(at(136_000)).toEqual(["seen", "seen"]);
    expect(at(135_999)).toEqual(["too-short", "too-short"]);
  });

  it("reads 3 audio packets per second as delivery and 2 as not", () => {
    const at = (v: number) => audioOf(samples(every(15_000, v), video), stretch(100_000, 160_000));
    expect(at(2)).toEqual(["missed", "missed"]);
    expect(at(3)).toEqual(["seen", "seen"]);
  });

  it("reads a decoded-video sample of 0 as not decoded", () => {
    const { pairs } = verdicts(
      samples(every(15_000, 50), every(15_000, 0)),
      stretch(100_000, 160_000),
    );
    expect(pairs?.map(([, v]) => v)).toEqual(["missed", "missed"]);
  });

  it("reads one sample showing delivery among several off as seen", () => {
    const mixed: Sample[] = [
      [105_000, 0],
      [120_000, 50],
      [135_000, 0],
      [150_000, 0],
    ];
    expect(audioOf(samples(mixed, video), stretch(100_000, 160_000))).toEqual(["seen", "seen"]);
  });

  it("reads a lone sample within 36 s of the stretch as too short and one beyond as no data", () => {
    const at = (t: number) => audioOf(samples([[t, 0]], video), stretch(100_000, 160_000));
    expect(at(64_000)).toEqual(["too-short", "too-short"]);
    expect(at(196_000)).toEqual(["too-short", "too-short"]);
    expect(at(63_999)).toEqual(["no-data", "no-data"]);
    expect(at(196_001)).toEqual(["no-data", "no-data"]);
  });

  it("matches a decoded-video sample within 1 s of an audio sample, and not 1.001 s", () => {
    const at = (t: number) =>
      verdicts(samples(every(30_000, 50), [[t, 15]]), stretch(100_000, 160_000)).pairs?.map(
        ([, v]) => v,
      );
    expect(at(121_000)).toEqual(["seen", "seen"]);
    expect(at(119_000)).toEqual(["seen", "seen"]);
    expect(at(121_001)).toEqual(["missed", "missed"]);
  });

  it("reads camera off when the camera turns off exactly at the stretch start or on exactly at its end", () => {
    for (const camera of [
      [
        { ms: 0, on: true },
        { ms: 100_000, on: false },
      ],
      [
        { ms: 0, on: false },
        { ms: 160_000, on: true },
      ],
    ]) {
      const { pairs } = verdicts(
        samples(every(15_000, 50), every(15_000, 15)),
        stretch(100_000, 160_000),
        bot(0, { camera }),
      );
      expect(pairs?.map(([, v]) => v)).toEqual(["camera-off", "camera-off"]);
    }
  });

  it("does not read a sample taken exactly at the stretch end", () => {
    const end: Sample[] = [
      [130_000, 0],
      [160_000, 50],
    ];
    expect(audioOf(samples(end, video), stretch(100_000, 160_000))).toEqual([
      "too-short",
      "too-short",
    ]);
  });
});

const FLEET = [0, 1, 2, 3, 4, 5, 6, 7];
/** Every pair of the 8-bot synthetic run, 15 s apart, delivering, minus `drop`. */
function synthSamples(drop: { audio?: string[]; video?: string[] } = {}): PromSamples {
  const all = (value: number, without: string[] = []): Map<string, Sample[]> =>
    new Map(
      FLEET.flatMap((r) =>
        FLEET.filter((p) => p !== r).map(
          (p) => [pairKey(sessionId(r), sessionId(p)), every(15_000, value, T0, T1)] as const,
        ),
      ).filter(([k]) => !without.includes(k)),
    );
  return { audio: all(50, drop.audio), video: all(15, drop.video) };
}
/** Every bot's camera on from T0; the synthetic run's first `Host render:` line has it off. */
const cameraOn = Object.fromEntries(
  FLEET.map((b) => [b, [[T0 + 3, true]] as Array<[number, boolean]>]),
);

function report(
  prom: PromInput | undefined,
  extra: Partial<ReportOptions> = {},
  synth: SynthOptions = { mic: cameraOn },
): { code: number; r: DeliveryReport; text: string } {
  const logs = writeSynthDir(synth);
  let json = "";
  let text = "";
  const base = { logs, fleetSize: String(FLEET.length), lateMs: "1000" };
  const code = runReport({ ...base, json: true, ...extra }, (t) => (json = t), prom);
  runReport({ ...base, ...extra, json: false }, (t) => (text = t), prom);
  return { code, r: JSON.parse(json) as DeliveryReport, text };
}

describe("report with Prometheus", () => {
  it("judges stretches of 30 s and over and passes --fail-on-flag when every pair delivered", () => {
    const { code, r, text } = report({ url: "u", samples: synthSamples() }, { failOnFlag: true });
    const judged = r.prometheus?.stretches.filter((s) =>
      s.pairs.every((p) => p.audio === "seen" && p.video === "seen"),
    );
    expect(judged?.length).toBeGreaterThan(0);
    expect([r.prometheus?.flags, r.prometheus?.noData, code]).toEqual([0, 0, 0]);
    expect(text).not.toMatch(/video delivery is not checked/);
    expect(text).toMatch(/Prometheus u \(delivered at all\)/);
  });

  it("adds a Prometheus miss to the report's flags, so --fail-on-flag exits 1", () => {
    const prom = {
      url: "u",
      samples: synthSamples({ video: [pairKey(sessionId(2), sessionId(0))] }),
    };
    const { code, r, text } = report(prom, { failOnFlag: true });
    expect(r.prometheus?.flags).toBeGreaterThan(0);
    expect(r.flags).toBe(r.prometheus?.flags);
    expect(code).toBe(1);
    expect(text).toMatch(/PROM_VIDEO_MISSED bot-2/);
    expect(report(prom).code).toBe(0);
  });

  it("exits 1 under --fail-on-flag when a pair has no Prometheus data", () => {
    const prom = {
      url: "u",
      samples: synthSamples({ audio: [pairKey(sessionId(1), sessionId(0))] }),
    };
    const { code, r } = report(prom, { failOnFlag: true });
    expect([r.prometheus?.flags, code]).toEqual([0, 1]);
    expect(r.prometheus?.noData).toBeGreaterThan(0);
  });

  it("fails --fail-on-flag when no pair was judged, though nothing is flagged", () => {
    const records = tempDir("records-");
    for (const name of ["a.json", "b.json"]) {
      writeFileSync(join(records, name), JSON.stringify({ bot_id: name, speech: ROTATION_PLAN }));
    }
    const prom = { url: "u", samples: synthSamples() };
    for (const list of [...prom.samples.audio.values(), ...prom.samples.video.values()]) {
      for (const sample of list) sample[1] = 0;
    }
    const synth = { mic: cameraOn, plan: ROTATION_PLAN };
    const { code, r, text } = report(prom, { records, failOnFlag: true }, synth);
    const verdicts = r.prometheus?.stretches.flatMap((s) =>
      s.pairs.flatMap((p) => [p.audio, p.video]),
    );
    expect(new Set(verdicts)).toEqual(new Set(["too-short"]));
    expect([r.flags, r.prometheus?.noData, code]).toEqual([0, 0, 1]);
    expect(text).toMatch(/audio judged on 0 of [1-9]\d* pairs, video on 0 of [1-9]\d* pairs/);
    expect(report(prom, { records }, synth).code).toBe(0);
  });

  it("exits 1 under --fail-on-flag on a single no-data pair", () => {
    const synth = { mic: { ...cameraOn, 0: [] } };
    const base = report({ url: "u", samples: synthSamples() }, {}, synth).r.prometheus;
    const s = base?.stretches.find((x) => x.speaker === 0 && x.pairs[0].audio === "seen");
    if (!s) throw new Error("fixture has no judged bot-0 stretch");
    const prom = { url: "u", samples: synthSamples() };
    const key = pairKey(sessionId(1), sessionId(0));
    prom.samples.audio.set(
      key,
      (prom.samples.audio.get(key) ?? []).filter(
        ([t]) => t < s.startMs - 36_000 || t > s.endMs + 36_000,
      ),
    );
    const { code, r } = report(prom, { failOnFlag: true }, synth);
    expect([r.flags, r.prometheus?.noData, code]).toEqual([0, 1, 1]);
  });

  it("leaves a camera-off publisher's pairs out of the video denominator on short stretches too", () => {
    const synth = { mic: { ...cameraOn, 0: [] } };
    const { r, text } = report({ url: "u", samples: synthSamples() }, {}, synth);
    const stretches = r.prometheus?.stretches ?? [];
    expect(stretches.some((s) => s.speaker === 0 && s.endMs - s.startMs < 30_000)).toBe(true);
    const cameraOnPairs = stretches
      .filter((s) => s.speaker !== 0)
      .reduce((n, s) => n + s.pairs.length, 0);
    expect(text).toMatch(new RegExp(`video on \\d+ of ${cameraOnPairs} pairs with the camera on`));
  });

  it("does not count a no-data pair as judged", () => {
    const prom = { url: "u", samples: synthSamples() };
    for (const key of prom.samples.audio.keys()) prom.samples.audio.set(key, []);
    const { r, text } = report(prom);
    expect(r.prometheus?.noData).toBeGreaterThan(0);
    expect(text).toMatch(/audio judged on 0 of [1-9]\d* pairs, video on 0 of [1-9]\d* pairs/);
  });

  it("prints a log and Prometheus audio disagreement in each direction", () => {
    const synth: SynthOptions = {
      mic: cameraOn,
      edgeAt: (receiver, s) => (receiver === 1 && s.speaker === 0 ? null : s.startMs + ONSET_MS),
    };
    const prom = { url: "u", samples: synthSamples() };
    for (const sample of prom.samples.audio.get(pairKey(sessionId(2), sessionId(0))) ?? []) {
      sample[1] = 0;
    }
    const { text } = report(prom, {}, synth);
    expect(text).toContain("PROM_AUDIO_SEEN bot-1 (log: MISSED)");
    expect(text).toContain("PROM_AUDIO_MISSED bot-2 (log: heard)");
  });

  it("prints a column of only too-short or only camera-off pairs as one word", () => {
    const { text } = report(
      { url: "u", samples: synthSamples() },
      {},
      { mic: { ...cameraOn, 0: [] } },
    );
    expect(text).toMatch(/s bot-\d+ {2}audio too short {2}video too short$/m);
    expect(text).toMatch(/s bot-0 {2}audio \d+\/\d+ {2}video camera off$/m);
  });

  it("leaves the report without a prometheus key when no Prometheus input is given", () => {
    const { r, text } = report(undefined);
    expect("prometheus" in r).toBe(false);
    expect(text).toMatch(/video delivery is not checked/);
  });
});

type Fetch = typeof fetch;
const matrix = (result: unknown[]): Response =>
  new Response(JSON.stringify({ status: "success", data: { resultType: "matrix", result } }));
const opts = (prometheus: string): ReportOptions & { prometheus: string } => ({
  logs: writeSynthDir({ mic: cameraOn }),
  fleetSize: String(FLEET.length),
  lateMs: "1000",
  prometheus,
});

describe("runReportWithPrometheus", () => {
  it.each([
    ["a network error", (() => Promise.reject(new TypeError("fetch failed"))) as Fetch],
    ["an HTTP error", (async () => new Response("{}", { status: 503 })) as Fetch],
    ["no series for the run", (async () => matrix([])) as Fetch],
  ])("prints no Prometheus data and exits 2 on %s, never a pass", async (_, fetchImpl) => {
    let text = "";
    await expect(
      runReportWithPrometheus(opts("http://p:9090"), (t) => (text = t), fetchImpl),
    ).rejects.toThrow(/no Prometheus data/);
    expect(text).toMatch(/Prometheus http:\/\/p:9090\/: no data/);
  });

  it("reports an unreachable Prometheus as no data through the real fetch", async () => {
    const err = await runReportWithPrometheus(opts("http://127.0.0.1:1"), () => {}).catch(
      (e: unknown) => e,
    );
    expect(err).toBeInstanceOf(ReportInputError);
    expect(String(err)).toMatch(/no Prometheus data/);
  });

  it.each([
    ["http://u@p:9090", /credentials/],
    ["http://:pw@p:9090", /credentials/],
    ["ftp://p", /must be http\(s\)/],
    ["file:///x", /must be http\(s\)/],
    ["http://h/prom?x=1", /query or fragment/],
    ["http://h/prom?", /query or fragment/],
    ["http://h/prom#x", /query or fragment/],
  ])("refuses %s before any request", async (url, message) => {
    let called = false;
    const fetchImpl = (async () => ((called = true), matrix([]))) as Fetch;
    const err = await runReportWithPrometheus(opts(url), () => {}, fetchImpl).catch(
      (e: unknown) => e,
    );
    expect(err).toBeInstanceOf(ReportInputError);
    expect(String(err)).toMatch(message);
    expect(called).toBe(false);
  });

  it.each([
    [
      "a vector result",
      {
        status: "success",
        data: {
          resultType: "vector",
          result: [{ metric: { session_id: "71", to_peer: "70" }, value: [150, "50"] }],
        },
      },
      /HTTP 200 success$/,
    ],
    [
      "an error status",
      {
        status: "error",
        error: "boom",
        data: {
          resultType: "matrix",
          result: [{ metric: { session_id: "71", to_peer: "70" }, values: [[150, "50"]] }],
        },
      },
      /HTTP 200 boom$/,
    ],
  ])("names the metric on a 200 response with %s", async (_, body, message) => {
    const fetchImpl = (async () => new Response(JSON.stringify(body))) as Fetch;
    const err = await fetchPromSamples(
      new URL("http://p"),
      [bot(0), bot(1)],
      100_000,
      200_000,
      fetchImpl,
    ).catch((e: unknown) => e);
    expect(String(err)).toMatch(new RegExp(`(${PROM_AUDIO}|${PROM_VIDEO}): `));
    expect(String(err)).toMatch(message);
  });

  it.each([
    [
      "Node's string-length error",
      Object.assign(new Error("Cannot create a string longer than 0x1fffffe8 characters"), {
        code: "ERR_STRING_TOO_LONG",
      }),
      "Cannot create a string longer than 0x1fffffe8 characters",
    ],
    ["a RangeError", new RangeError("Invalid string length"), "Invalid string length"],
  ])(
    "names the metric and says to narrow the window on %s reading the body",
    async (_, error, msg) => {
      const fetchImpl = (async () => ({
        ok: true,
        status: 200,
        json: () => Promise.reject(error),
      })) as unknown as Fetch;
      const err = await fetchPromSamples(
        new URL("http://p"),
        [bot(0), bot(1)],
        100_000,
        200_000,
        fetchImpl,
      ).catch((e: unknown) => e);
      expect(String(err)).toContain(msg);
      expect(String(err)).toMatch(
        new RegExp(`(${PROM_AUDIO}|${PROM_VIDEO}): .*; narrow --from/--to$`),
      );
    },
  );

  it("names the metric but gives no narrowing hint when the body is not a size failure", async () => {
    const fetchImpl = (async () => ({
      ok: true,
      status: 200,
      json: () => Promise.reject(new SyntaxError("Unexpected token '<'")),
    })) as unknown as Fetch;
    const err = await fetchPromSamples(
      new URL("http://p"),
      [bot(0), bot(1)],
      100_000,
      200_000,
      fetchImpl,
    ).catch((e: unknown) => e);
    expect(String(err)).toMatch(
      new RegExp(`(${PROM_AUDIO}|${PROM_VIDEO}): .*Unexpected token '<'`),
    );
    expect(String(err)).not.toMatch(/narrow/);
  });

  it("reports the HTTP status of a failed request whose body is not JSON", async () => {
    const fetchImpl = (async () => new Response("bad gateway", { status: 502 })) as Fetch;
    await expect(
      fetchPromSamples(new URL("http://p"), [bot(0), bot(1)], 100_000, 200_000, fetchImpl),
    ).rejects.toThrow(/: HTTP 502$/);
  });

  it("queries from 36 s before the window start to 36 s after its end", async () => {
    const bodies: URLSearchParams[] = [];
    const fetchImpl = (async (_: URL, init: RequestInit) => {
      bodies.push(new URLSearchParams(String(init.body)));
      return matrix([{ metric: { session_id: id(1), to_peer: id(0) }, values: [[150, "50"]] }]);
    }) as unknown as Fetch;
    await fetchPromSamples(new URL("http://p"), [bot(0), bot(1)], 100_000, 200_000, fetchImpl);
    expect(bodies.map((b) => b.get("time"))).toEqual(["236", "236"]);
    for (const b of bodies) expect(b.get("query")).toMatch(/\[172s\]$/);
  });

  it("fetches the samples after the window end that separate too short from no data", async () => {
    const data = new Map<string, Sample[]>([
      [pairKey(id(1), id(0)), [[170_000, 0]]],
      [pairKey(id(2), id(0)), [[120_000, 50]]],
    ]);
    const fetchImpl = (async (_: URL, init: RequestInit) => {
      const body = new URLSearchParams(String(init.body));
      const at = Number(body.get("time")) * 1000;
      const span = Number(/\[(\d+)s\]$/.exec(body.get("query") ?? "")?.[1]) * 1000;
      const result = [...data].map(([key, list]) => {
        const [receiver, publisher] = key.split(" ");
        const values = list
          .filter(([t]) => t > at - span && t <= at)
          .map(([t, v]) => [t / 1000, String(v)]);
        return { metric: { session_id: receiver, to_peer: publisher }, values };
      });
      return matrix(result);
    }) as unknown as Fetch;
    const bots = [bot(0), bot(1), bot(2)];
    const fetched = await fetchPromSamples(new URL("http://p"), bots, 100_000, 163_000, fetchImpl);
    const r = scorePrometheus([stretch(100_000, 160_000)], bots, { url: "u", samples: fetched });
    expect(r.stretches[0].pairs.map((p) => p.audio)).toEqual(["too-short", "seen"]);
  });

  it("ignores a series whose receiver or publisher is outside the run", async () => {
    const bots = [bot(0), bot(1)];
    const values = [[150, "50"]];
    const outside = [
      { metric: { session_id: id(1), to_peer: "99" }, values },
      { metric: { session_id: "99", to_peer: id(0) }, values },
    ];
    const only = (async () => matrix(outside)) as unknown as Fetch;
    await expect(
      fetchPromSamples(new URL("http://p"), bots, 100_000, 200_000, only),
    ).rejects.toThrow(/no videocall_neteq_packets_per_sec series/);
    const fetchImpl = (async () =>
      matrix([
        { metric: { session_id: id(1), to_peer: id(0) }, values },
        ...outside,
      ])) as unknown as Fetch;
    const { audio } = await fetchPromSamples(
      new URL("http://p"),
      bots,
      100_000,
      200_000,
      fetchImpl,
    );
    expect([...audio.keys()]).toEqual([pairKey(id(1), id(0))]);
  });

  it("POSTs a raw range query filtered to the run's session ids and keys receivers by session_id, never from_peer", async () => {
    const bodies: string[] = [];
    const values = every(15_000, 50, T0, T1).map(([t, v]) => [t / 1000, String(v)]);
    const fetchImpl = (async (url: URL, init: RequestInit) => {
      expect([String(url), init.method]).toEqual(["http://p:9090/prom/api/v1/query", "POST"]);
      bodies.push(String(init.body));
      return matrix([
        { metric: { session_id: sessionId(1), from_peer: "x@y", to_peer: sessionId(0) }, values },
        { metric: { from_peer: "alice@example.com", to_peer: sessionId(0) }, values },
        { metric: { from_peer: sessionId(2), to_peer: sessionId(0) }, values },
        { metric: { session_id: "1", to_peer: sessionId(0) }, values },
      ]);
    }) as unknown as Fetch;
    let json = "";
    await runReportWithPrometheus(
      { ...opts("http://p:9090/prom"), json: true },
      (t) => (json = t),
      fetchImpl,
    );
    const query = bodies.map((b) => new URLSearchParams(b).get("query") ?? "");
    expect(query.map((q) => q.split("{")[0]).sort()).toEqual([PROM_AUDIO, PROM_VIDEO].sort());
    expect(query[0]).toContain(`to_peer=~"${FLEET.map(sessionId).join("|")}"`);
    const r = JSON.parse(json) as DeliveryReport;
    const bot0 = r.prometheus?.stretches.find(
      (s) => s.speaker === 0 && s.pairs.some((p) => p.audio === "seen"),
    );
    expect(bot0?.pairs.filter((p) => p.audio === "seen").map((p) => p.receiver)).toEqual([1]);
  });

  it("reads video as no data and exits 0 without --fail-on-flag when Prometheus has no video series", async () => {
    const values = every(15_000, 50, T0, T1).map(([t, v]) => [t / 1000, String(v)]);
    const audio = FLEET.flatMap((r) =>
      FLEET.filter((p) => p !== r).map((p) => ({
        metric: { session_id: sessionId(r), to_peer: sessionId(p) },
        values,
      })),
    );
    const fetchImpl = (async (_: URL, init: RequestInit) =>
      matrix(String(init.body).includes(PROM_AUDIO) ? audio : [])) as unknown as Fetch;
    let json = "";
    const run = (failOnFlag: boolean) =>
      runReportWithPrometheus(
        { ...opts("http://p:9090"), json: true, failOnFlag },
        (t) => (json = t),
        fetchImpl,
      );
    expect(await run(false)).toBe(0);
    const r = JSON.parse(json) as DeliveryReport;
    const judged = r.prometheus?.stretches.filter((s) => s.pairs.every((p) => p.audio === "seen"));
    expect(judged?.length).toBeGreaterThan(0);
    expect(new Set(judged?.flatMap((s) => s.pairs.map((p) => p.video)))).toEqual(
      new Set(["no-data"]),
    );
    expect(await run(true)).toBe(1);
  });
});
