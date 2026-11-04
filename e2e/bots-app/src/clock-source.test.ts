import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { CLOCK_SPEECH_SCRIPT, loadClockSpeech, parseTalkingClockLine } from "./clock-init";
import { SD_SOURCE } from "./posture";
import { SD_VIDEO_NOISE } from "./video-noise";
import { SPEECH_PLAN_DEFAULT, type SpeechPlan } from "./speech-plan";

const CLOCK_SOURCE = readFileSync(
  fileURLToPath(new URL("./clock-source.js", import.meta.url)),
  "utf8",
);

interface FakeNode {
  connect: ReturnType<typeof vi.fn>;
}
interface FakeGain extends FakeNode {
  gain: { value: number };
}
interface FakeOscillator extends FakeNode {
  start: ReturnType<typeof vi.fn>;
}
interface FakeBufferSource extends FakeNode {
  buffer: { duration: number; clip?: string } | null;
  start: ReturnType<typeof vi.fn>;
}

interface TalkingClockSetup {
  state?: "running" | "suspended";
  currentTime?: number;
  fleetIndex?: number;
  fleetSize?: number;
  clips?: Record<string, number>;
  plan?: SpeechPlan;
  /** Replaces the page's `__CLOCK_SPEECH`, given the one clock-speech.js built. */
  speech?: (real: Record<string, unknown>) => Record<string, unknown>;
}

type TrackKind = "audio" | "video";

interface FillCall {
  style: string;
  x: number;
  y: number;
  w: number;
  h: number;
}

// Set by the fake canvas below when the module configures it, so the track's
// reported dimensions follow the source instead of a hardcoded pair.
let capturedCanvasWidth = 0;
let capturedCanvasHeight = 0;

class FakeTrack {
  readyState: "live" | "ended" = "live";
  enabled = true;
  readonly stop = vi.fn(() => {
    this.readyState = "ended";
  });
  readonly requestFrame = vi.fn();

  constructor(
    readonly kind: TrackKind,
    private readonly dimensionsReady: () => boolean,
    private readonly clones?: FakeTrack[],
  ) {}

  clone(): FakeTrack {
    const clone = new FakeTrack(this.kind, this.dimensionsReady);
    this.clones?.push(clone);
    return clone;
  }

  getSettings(): { width?: number; height?: number } {
    if (this.kind !== "video" || !this.dimensionsReady()) return {};
    return { width: capturedCanvasWidth, height: capturedCanvasHeight };
  }
}

class FakeMediaStream {
  constructor(private readonly tracks: FakeTrack[] = []) {}

  getTracks(): FakeTrack[] {
    return this.tracks;
  }

  getVideoTracks(): FakeTrack[] {
    return this.tracks.filter((track) => track.kind === "video");
  }

  getAudioTracks(): FakeTrack[] {
    return this.tracks.filter((track) => track.kind === "audio");
  }
}

function installClockSource(
  dimensionsReady: () => boolean,
  injected?: {
    width?: unknown;
    height?: unknown;
    noise?: unknown;
    random?: () => number;
    failTiles?: boolean;
    /** false = no video track requested, as for a camera-off bot; default requests one. */
    camera?: boolean;
  },
  talking?: TalkingClockSetup,
): {
  getUserMedia: (constraints: { video?: boolean; audio?: boolean }) => Promise<FakeMediaStream>;
  audioContextCount: () => number;
  clonedTracks: FakeTrack[];
  baseVideoTrack: FakeTrack;
  captureStream: ReturnType<typeof vi.fn>;
  drawingContext: {
    fillText: ReturnType<typeof vi.fn>;
    fillRect: ReturnType<typeof vi.fn>;
  };
  ops: { op: string; alpha: number; args: unknown[] }[];
  noiseTiles: { width: number; height: number; puts: number }[];
  errorLines: string[];
  fills: FillCall[];
  canvas: { width: number; height: number };
  redraw: () => void;
  setParticipant: (participant: string) => void;
  intlCalls: { locale: unknown; options: Record<string, unknown> }[];
  gains: FakeGain[];
  sources: FakeBufferSource[];
  oscillators: FakeOscillator[];
  destination: object;
  intervals: (() => void)[];
  statusLines: string[];
  audio: { state: "running" | "suspended"; currentTime: number };
} {
  let audioContexts = 0;
  const audio = { state: talking?.state ?? "suspended", currentTime: talking?.currentTime ?? 0 };
  const gains: FakeGain[] = [];
  const sources: FakeBufferSource[] = [];
  const oscillators: FakeOscillator[] = [];
  const destination = { stream: new FakeMediaStream([]) };
  const intervals: (() => void)[] = [];
  const statusLines: string[] = [];
  const errorLines: string[] = [];
  let redraw = (): void => {
    throw new Error("clock source did not install its draw interval");
  };
  const clonedTracks: FakeTrack[] = [];
  const baseVideoTrack = new FakeTrack("video", dimensionsReady, clonedTracks);
  const baseAudioTrack = new FakeTrack("audio", () => false, clonedTracks);

  class FakeAudioContext {
    get state() {
      return audio.state;
    }
    get currentTime() {
      return audio.currentTime;
    }

    constructor() {
      audioContexts += 1;
    }

    createMediaStreamDestination() {
      destination.stream = new FakeMediaStream([baseAudioTrack]);
      return destination;
    }

    createGain(): FakeGain {
      const gain = { gain: { value: 1 }, connect: vi.fn() };
      gains.push(gain);
      return gain;
    }

    createBuffer(_channels: number, length: number) {
      const samples = new Float32Array(length);
      return { duration: length / 48_000, getChannelData: () => samples };
    }

    createOscillator(): FakeOscillator {
      const oscillator = { connect: vi.fn(), start: vi.fn() };
      oscillators.push(oscillator);
      return oscillator;
    }

    createBufferSource(): FakeBufferSource {
      const source = { buffer: null, connect: vi.fn(), start: vi.fn() };
      sources.push(source);
      return source;
    }

    async decodeAudioData(bytes: ArrayBuffer) {
      const clip = new TextDecoder().decode(bytes);
      return { clip, duration: talking?.clips?.[clip] ?? 0 };
    }

    async resume(): Promise<void> {}
  }

  const ops: { op: string; alpha: number; args: unknown[] }[] = [];
  const recorded = (op: string) =>
    vi.fn((...args: unknown[]) => ops.push({ op, alpha: drawingContext.globalAlpha, args }));
  const fills: FillCall[] = [];
  const drawingContext = {
    fillStyle: "",
    font: "",
    textAlign: "",
    textBaseline: "",
    globalAlpha: 1,
    fillRect: vi.fn((x: number, y: number, w: number, h: number) => {
      fills.push({ style: drawingContext.fillStyle, x, y, w, h });
      ops.push({ op: "fillRect", alpha: drawingContext.globalAlpha, args: [x, y, w, h] });
    }),
    fillText: recorded("fillText"),
    drawImage: recorded("drawImage"),
  };
  const noiseTiles: { width: number; height: number; puts: number }[] = [];
  let canvasesCreated = 0;
  class FakeTileCanvas {
    puts = 0;
    width = 0;
    height = 0;
    getContext() {
      if (injected?.failTiles) throw new Error("tile context unavailable");
      return {
        createImageData: (w: number, h: number) => ({ data: new Uint8ClampedArray(w * h * 4) }),
        putImageData: () => {
          this.puts += 1;
        },
      };
    }
  }
  const captureStream = vi.fn(() => new FakeMediaStream([baseVideoTrack]));
  const canvas = {
    set width(v: number) {
      capturedCanvasWidth = v;
    },
    get width(): number {
      return capturedCanvasWidth;
    },
    set height(v: number) {
      capturedCanvasHeight = v;
    },
    get height(): number {
      return capturedCanvasHeight;
    },
    getContext: vi.fn(() => drawingContext),
    captureStream,
  };
  const navigator = {
    mediaDevices: {
      getUserMedia: vi.fn(),
    },
  };
  const intlCalls: { locale: unknown; options: Record<string, unknown> }[] = [];
  const RecordingDateTimeFormat = function (
    locale: unknown,
    options: Record<string, unknown>,
  ): Intl.DateTimeFormat {
    intlCalls.push({ locale, options });
    return new Intl.DateTimeFormat(locale as string, options);
  } as unknown as typeof Intl.DateTimeFormat;
  const sandbox = {
    __CLOCK_PARTICIPANT: "",
    __CLOCK_WIDTH: injected?.width,
    __CLOCK_HEIGHT: injected?.height,
    __CLOCK_VIDEO_NOISE: injected !== undefined && "noise" in injected ? injected.noise : 0,
    __CLOCK_FLEET_INDEX: talking?.fleetIndex,
    __CLOCK_FLEET_SIZE: talking?.fleetSize,
    __CLOCK_SPEECH_PLAN: talking?.plan,
    __CLOCK_VOICE_CLIPS:
      talking?.clips === undefined
        ? undefined
        : Object.fromEntries(
            Object.keys(talking.clips).map((clip) => [clip, Buffer.from(clip).toString("base64")]),
          ),
    atob,
    AudioContext: FakeAudioContext,
    Date,
    Intl: { DateTimeFormat: RecordingDateTimeFormat },
    MediaStream: FakeMediaStream,
    Promise,
    console: {
      ...console,
      info: (line: string) => statusLines.push(line),
      error: (line: string) => errorLines.push(line),
    },
    ...(injected?.random
      ? { Math: Object.assign(Object.create(Math), { random: injected.random }) }
      : {}),
    document: {
      // The first canvas is the captured one; every later one is a noise tile.
      createElement: vi.fn(() => {
        if (canvasesCreated++ === 0) return canvas;
        const tile = new FakeTileCanvas();
        noiseTiles.push(tile);
        return tile;
      }),
    },
    navigator,
    setInterval: vi.fn((callback: () => void) => {
      if (intervals.length === 0) redraw = callback;
      intervals.push(callback);
    }),
    setTimeout,
  };

  if (talking?.speech !== undefined) {
    const page = vm.createContext(sandbox) as Record<string, unknown>;
    vm.runInContext(CLOCK_SPEECH_SCRIPT, page);
    page.__CLOCK_SPEECH = talking.speech(page.__CLOCK_SPEECH as Record<string, unknown>);
    vm.runInContext(CLOCK_SOURCE, page, { filename: "clock-source.js" });
  } else {
    const script = talking === undefined ? CLOCK_SOURCE : `${CLOCK_SPEECH_SCRIPT}\n${CLOCK_SOURCE}`;
    vm.runInNewContext(script, sandbox, { filename: "clock-source.js" });
  }
  if (injected?.camera !== false) {
    void navigator.mediaDevices.getUserMedia({ video: true }).catch(() => undefined);
  }
  return {
    getUserMedia: navigator.mediaDevices.getUserMedia,
    audioContextCount: () => audioContexts,
    clonedTracks,
    baseVideoTrack,
    captureStream,
    drawingContext,
    ops,
    noiseTiles,
    errorLines,
    fills,
    canvas,
    redraw: () => redraw(),
    setParticipant: (participant: string) => {
      sandbox.__CLOCK_PARTICIPANT = participant;
    },
    intlCalls,
    gains,
    sources,
    oscillators,
    destination,
    intervals,
    statusLines,
    audio,
  };
}

beforeEach(() => {
  // Module-level and shared: each installClockSource re-runs the module and
  // reassigns these, but reset so one test cannot observe another's canvas.
  capturedCanvasWidth = 0;
  capturedCanvasHeight = 0;
});

afterEach(() => {
  vi.useRealTimers();
});

describe("clock-source getUserMedia width gate", () => {
  it("auto-samples the canvas at 30 fps without manual requestFrame calls", () => {
    const { baseVideoTrack, captureStream } = installClockSource(() => true);

    expect(captureStream).toHaveBeenCalledWith(30);
    expect(baseVideoTrack.requestFrame).not.toHaveBeenCalled();
  });

  it("reads the participant label on each frame instead of snapshotting init order", () => {
    const { drawingContext, redraw, setParticipant, canvas } = installClockSource(() => true);
    drawingContext.fillText.mockClear();

    setParticipant("Late Participant");
    redraw();

    const call = drawingContext.fillText.mock.calls.find((c) => c[0] === "Late Participant");
    expect(call).toBeDefined();
    const [, x] = call as [string, number, number, number];
    expect(x).toBe(canvas.width / 2);
  });

  it.each<[string, { width: number; height: number } | undefined]>([
    ["the 640x480 default", undefined],
    ["an injected 1280x720 source", { width: 1280, height: 720 }],
  ])(
    "keeps EVERY drawn element inside the canvas at %s, none left absolute",
    (_label, injected) => {
      // The invariant is proportionality: each element must sit at the same FRACTION
      // of frame height it occupied in the 1280x720 reference layout.
      const { drawingContext, redraw, setParticipant, canvas } = installClockSource(
        () => true,
        injected,
      );
      setParticipant("Someone");
      drawingContext.fillText.mockClear();
      redraw();

      const calls = drawingContext.fillText.mock.calls as [string, number, number, number?][];
      expect(calls.length).toBe(3);

      // Reference fractions from the authored 1280x720 layout: time 330/720,
      // date 465/720, name 585/720.
      const expectedFractions = [330 / 720, 465 / 720, 585 / 720];
      calls.forEach(([text, x, y], i) => {
        expect(x).toBe(canvas.width / 2);
        expect(y).toBeGreaterThan(0);
        expect(y).toBeLessThan(canvas.height);
        expect(y / canvas.height).toBeCloseTo(expectedFractions[i], 5);
        void text;
      });

      // Ordering must stay top-to-bottom with no overlap collapse.
      const ys = calls.map(([, , y]) => y);
      expect(ys[0]).toBeLessThan(ys[1]);
      expect(ys[1]).toBeLessThan(ys[2]);

      calls.forEach(([, , , maxWidth]) => {
        expect(maxWidth).toBeLessThanOrEqual(canvas.width);
      });
    },
  );

  it("falls back to SD_SOURCE, in lockstep with posture.ts", () => {
    const { canvas } = installClockSource(() => true);
    expect([canvas.width, canvas.height]).toEqual([SD_SOURCE.width, SD_SOURCE.height]);
    expect([canvas.width, canvas.height]).toEqual([640, 480]);
  });

  it.each<[{ width?: unknown; height?: unknown } | undefined, number, number]>([
    [undefined, 640, 480],
    [{}, 640, 480],
    [{ width: "1280", height: "720" }, 640, 480],
    [{ width: 0, height: -1 }, 640, 480],
    [{ width: 1280.5, height: 720.5 }, 640, 480],
    [{ width: 1280, height: 720 }, 1280, 720],
    [{ width: 1280 }, 1280, 480],
    [{ height: 720 }, 640, 720],
  ])("sizes the canvas from the injected geometry per axis: %j (#2236)", (injected, w, h) => {
    const { canvas } = installClockSource(() => true, injected);
    expect([canvas.width, canvas.height]).toEqual([w, h]);
  });

  describe("rendered clock is pinned, not container-resolved (#2294)", () => {
    const FIXED = "2026-08-18T23:45:07.123Z";
    const ORIGINAL_TZ = process.env.TZ;

    beforeEach(() => {
      process.env.TZ = "America/New_York";
    });

    afterEach(() => {
      if (ORIGINAL_TZ === undefined) delete process.env.TZ;
      else process.env.TZ = ORIGINAL_TZ;
    });

    function drawnTexts(): string[] {
      vi.useFakeTimers();
      vi.setSystemTime(new Date(FIXED));
      const { drawingContext, redraw } = installClockSource(() => true);
      drawingContext.fillText.mockClear();
      redraw();
      return drawingContext.fillText.mock.calls.map((c) => c[0] as string);
    }

    it("renders the UTC wall clock, not the container's local time", () => {
      expect(drawnTexts()[0]).toBe(new Date(FIXED).toISOString().slice(11, 23));
    });

    it("names the zone in the frame so an offset cannot be misread as a freeze", () => {
      expect(drawnTexts()[1]).toMatch(/ UTC$/);
    });

    it("renders the date in the pinned locale's order", () => {
      expect(drawnTexts()[1]).toMatch(/^18 Aug 2026\b/);
    });

    it("pins locale, zone and hour cycle on BOTH formatters", () => {
      const { intlCalls } = installClockSource(() => true);

      expect(intlCalls).toHaveLength(2);
      for (const { locale, options } of intlCalls) {
        expect(locale).toBe("en-GB");
        expect(options.timeZone).toBe("UTC");
      }
      expect(intlCalls[0].options.hourCycle).toBe("h23");
      expect(intlCalls[0].options).not.toHaveProperty("hour12");
    });
  });

  it("rejects and stops tracks when video dimensions never appear", async () => {
    vi.useFakeTimers();
    const { getUserMedia, clonedTracks } = installClockSource(() => false, { camera: false });
    const request = getUserMedia({ video: true, audio: true });
    const rejection = expect(request).rejects.toThrow(
      "clock getUserMedia: track never reported dimensions",
    );

    await vi.advanceTimersByTimeAsync(5_100);

    await rejection;
    expect(clonedTracks).toHaveLength(2);
    expect(clonedTracks.every((track) => track.stop.mock.calls.length === 1)).toBe(true);
  });

  it("resolves with fresh video and audio tracks once dimensions are present", async () => {
    vi.useFakeTimers();
    const { getUserMedia, audioContextCount } = installClockSource(() => true);

    const first = await getUserMedia({ video: true, audio: true });
    const second = await getUserMedia({ video: true, audio: true });

    expect(first.getVideoTracks()).toHaveLength(1);
    expect(first.getAudioTracks()).toHaveLength(1);
    expect(second.getVideoTracks()).toHaveLength(1);
    expect(second.getAudioTracks()).toHaveLength(1);
    expect(second.getVideoTracks()[0]).not.toBe(first.getVideoTracks()[0]);
    expect(second.getAudioTracks()[0]).not.toBe(first.getAudioTracks()[0]);
    expect(audioContextCount()).toBe(1);
  });
});

describe("clock-source draws only while a video track is live", () => {
  const drawsOver = (clock: ReturnType<typeof installClockSource>, frames: number): number => {
    clock.drawingContext.fillRect.mockClear();
    for (let frame = 0; frame < frames; frame++) clock.redraw();
    return clock.drawingContext.fillRect.mock.calls.filter((c) => c[3] === clock.canvas.height)
      .length;
  };

  it("makes no recurring draws for a camera-off bot, mic on or not", async () => {
    const clock = installClockSource(() => true, { camera: false });
    expect(drawsOver(clock, 3)).toBe(0);
    await clock.getUserMedia({ audio: true });
    expect(drawsOver(clock, 3)).toBe(0);
  });

  it("draws every frame while a requested video track is live", async () => {
    const clock = installClockSource(() => true, { camera: false });
    await clock.getUserMedia({ video: true, audio: true });
    expect(drawsOver(clock, 30)).toBe(30);
  });

  it("stops when the camera is turned off and restarts when it is turned back on", async () => {
    const clock = installClockSource(() => true, { camera: false });
    const first = await clock.getUserMedia({ video: true });
    first.getVideoTracks()[0].stop();
    expect(drawsOver(clock, 3)).toBe(0);
    await clock.getUserMedia({ video: true });
    expect(drawsOver(clock, 3)).toBe(3);
  });

  it("stops when the browser ends the track", async () => {
    const clock = installClockSource(() => true, { camera: false });
    const stream = await clock.getUserMedia({ video: true });
    stream.getVideoTracks()[0].readyState = "ended";
    expect(drawsOver(clock, 3)).toBe(0);
  });

  it("keeps drawing until the last of several clones ends", async () => {
    const clock = installClockSource(() => true, { camera: false });
    const a = await clock.getUserMedia({ video: true });
    const b = await clock.getUserMedia({ video: true });
    a.getVideoTracks()[0].stop();
    expect(drawsOver(clock, 3)).toBe(3);
    b.getVideoTracks()[0].stop();
    expect(drawsOver(clock, 3)).toBe(0);
  });

  it("keeps drawing while a clone of a stopped track is live", async () => {
    const clock = installClockSource(() => true, { camera: false });
    const stream = await clock.getUserMedia({ video: true });
    const parent = stream.getVideoTracks()[0];
    const child = parent.clone();
    const grandchild = child.clone();
    parent.stop();
    child.stop();
    expect(drawsOver(clock, 3)).toBe(3);
    grandchild.stop();
    expect(drawsOver(clock, 3)).toBe(0);
  });

  it("clones the track it is called on, so a borrowed clone() stays audio", async () => {
    const clock = installClockSource(() => true, { camera: false });
    const video = (await clock.getUserMedia({ video: true })).getVideoTracks()[0];
    const audio = (await clock.getUserMedia({ audio: true })).getAudioTracks()[0];
    const copy = video.clone.call(audio);
    expect(copy.kind).toBe("audio");
    video.stop();
    expect(drawsOver(clock, 3)).toBe(0);
  });

  it("rejects an unbound clone() like the native method", async () => {
    const clock = installClockSource(() => true, { camera: false });
    const { clone } = (await clock.getUserMedia({ video: true })).getVideoTracks()[0];
    expect(() => clone()).toThrow(TypeError);
  });

  it("drops an ended track on the next tick even behind a live one", async () => {
    const clock = installClockSource(() => true, { camera: false });
    await clock.getUserMedia({ video: true });
    const ended = (await clock.getUserMedia({ video: true })).getVideoTracks()[0];
    ended.stop();
    let reads = 0;
    Object.defineProperty(ended, "readyState", {
      get: () => {
        reads += 1;
        return "ended";
      },
    });
    expect(drawsOver(clock, 3)).toBe(3);
    expect(reads).toBe(1);
  });

  it("keeps drawing for a disabled but live track", async () => {
    const clock = installClockSource(() => true, { camera: false });
    const stream = await clock.getUserMedia({ video: true });
    stream.getVideoTracks()[0].enabled = false;
    expect(drawsOver(clock, 3)).toBe(3);
  });
});

describe("clock-source video noise (issue 3089)", () => {
  const SMALL = { width: 64, height: 48 };

  it("draws one tile per frame at the injected alpha, right after the background fill", () => {
    const { ops, noiseTiles, redraw } = installClockSource(() => true, { ...SMALL, noise: 0.2 });
    ops.length = 0;
    redraw();

    expect(ops[0]).toMatchObject({ op: "fillRect", alpha: 1, args: [0, 0, 64, 48] });
    expect(ops[1]).toMatchObject({ op: "drawImage", alpha: 0.2 });
    expect(ops.filter((o) => o.op === "drawImage")).toHaveLength(1);
    expect(ops.slice(2).every((o) => o.alpha === 1)).toBe(true);
    expect(ops.some((o) => o.op === "fillText")).toBe(true);

    const [tile, x, y] = ops[1].args as [{ width: number; height: number }, number, number];
    expect(noiseTiles).toContain(tile);
    expect(x).toBeLessThanOrEqual(0);
    expect(y).toBeLessThanOrEqual(0);
    expect(tile.width + x).toBeGreaterThanOrEqual(SMALL.width);
    expect(tile.height + y).toBeGreaterThanOrEqual(SMALL.height);
  });

  it("builds eight tiles once at startup and none per frame", () => {
    const { noiseTiles, redraw } = installClockSource(() => true, { ...SMALL, noise: 0.1 });
    expect(noiseTiles).toHaveLength(8);
    for (let frame = 0; frame < 5; frame++) redraw();
    expect(noiseTiles).toHaveLength(8);
    expect(noiseTiles.map((t) => t.puts)).toEqual(Array(8).fill(1));
  });

  it("draws no noise and builds no tiles at alpha 0", () => {
    const { ops, noiseTiles, redraw } = installClockSource(() => true, { ...SMALL, noise: 0 });
    redraw();
    expect(noiseTiles).toHaveLength(0);
    expect(ops.some((o) => o.op === "drawImage")).toBe(false);
  });

  it("varies the tile and offset frame to frame, never uncovering an edge", () => {
    const sequence = [0.99, 0, 0.5, 0.25, 0.75, 0.6, 0.1];
    let call = 0;
    const random = (): number => sequence[call++ % sequence.length];
    const { ops, redraw } = installClockSource(() => true, { ...SMALL, noise: 0.1, random });
    ops.length = 0;
    for (let frame = 0; frame < 12; frame++) redraw();
    const draws = ops
      .filter((o) => o.op === "drawImage")
      .map((o) => o.args as [{ width: number; height: number }, number, number]);
    expect(draws).toHaveLength(12);
    expect(new Set(draws.map(([tile]) => tile)).size).toBeGreaterThan(1);
    expect(new Set(draws.map(([, x, y]) => `${x},${y}`)).size).toBeGreaterThan(1);
    for (const [tile, x, y] of draws) {
      expect(x).toBeLessThanOrEqual(0);
      expect(y).toBeLessThanOrEqual(0);
      expect(tile.width + x).toBeGreaterThanOrEqual(SMALL.width);
      expect(tile.height + y).toBeGreaterThanOrEqual(SMALL.height);
    }
  });

  it("accepts alpha exactly 1", () => {
    const { ops, redraw } = installClockSource(() => true, { ...SMALL, noise: 1 });
    ops.length = 0;
    redraw();
    expect(ops.find((o) => o.op === "drawImage")?.alpha).toBe(1);
  });

  it("keeps publishing the clock without noise when the tile build throws", async () => {
    const { ops, redraw, errorLines, getUserMedia } = installClockSource(() => true, {
      ...SMALL,
      noise: 0.2,
      failTiles: true,
    });
    ops.length = 0;
    redraw();
    expect(ops.some((o) => o.op === "drawImage")).toBe(false);
    expect(ops.some((o) => o.op === "fillText")).toBe(true);
    expect(errorLines).toEqual([
      "[clock-source] noise background OFF, tile build failed: Error: tile context unavailable",
    ]);
    const stream = await getUserMedia({ video: true });
    expect(stream.getVideoTracks()).toHaveLength(1);
  });

  it.each([undefined, 1.5, -0.1, "0.3", Number.NaN])(
    "falls back to SD_VIDEO_NOISE for %s, in lockstep with video-noise.ts",
    (noise) => {
      const { ops, redraw } = installClockSource(() => true, { ...SMALL, noise });
      ops.length = 0;
      redraw();
      expect(ops.find((o) => o.op === "drawImage")?.alpha).toBe(SD_VIDEO_NOISE);
    },
  );
});

describe("clock-source per-second colour marker survives a cover-fit tile crop (issue 3088)", () => {
  const HD = { width: 1280, height: 720 };
  const SD = { width: 640, height: 480 };

  /** The bright per-second marker, told apart from the 18%-lightness background by its lightness. */
  const isMarker = (fill: FillCall): boolean => {
    const lightness = /^hsl\(\d+ \d+% (\d+)%\)$/.exec(fill.style)?.[1];
    return lightness !== undefined && Number(lightness) >= 40;
  };

  function drawFrame(injected: { width: number; height: number }) {
    const clock = installClockSource(() => true, injected);
    clock.setParticipant("Someone");
    clock.fills.length = 0;
    clock.drawingContext.fillRect.mockClear();
    clock.drawingContext.fillText.mockClear();
    clock.redraw();
    return clock;
  }

  /** Source region left visible when the frame is scaled to cover a centred tile. */
  function coverCrop(width: number, height: number, tileAspect: number) {
    return width / height > tileAspect
      ? { x: (width - height * tileAspect) / 2, y: 0, w: height * tileAspect, h: height }
      : { x: 0, y: (height - width / tileAspect) / 2, w: width, h: width / tileAspect };
  }

  it.each([
    ["1280x720", "3:2", HD, 3 / 2],
    ["640x480", "3:2", SD, 3 / 2],
    ["1280x720", "1:1", HD, 1],
    ["640x480", "1:1", SD, 1],
    ["1280x720", "9:16", HD, 9 / 16],
    ["640x480", "9:16", SD, 9 / 16],
    ["1280x720", "2:1", HD, 2],
    ["640x480", "2:1", SD, 2],
  ] as const)("%s source in a %s tile keeps the marker visible", (_s, _t, injected, aspect) => {
    const { fills } = drawFrame(injected);
    const marker = fills.filter(isMarker);
    expect(marker.length).toBeGreaterThan(0);

    const crop = coverCrop(injected.width, injected.height, aspect);
    const visibleArea = marker.reduce((sum, f) => {
      const w = Math.min(f.x + f.w, crop.x + crop.w) - Math.max(f.x, crop.x);
      const h = Math.min(f.y + f.h, crop.y + crop.h) - Math.max(f.y, crop.y);
      return sum + Math.max(0, w) * Math.max(0, h);
    }, 0);
    expect(visibleArea / (crop.w * crop.h)).toBeGreaterThan(0.01);
  });

  it.each([HD, SD])("paints the marker after every text row at %j", (injected) => {
    const { drawingContext, fills } = drawFrame(injected);
    const rectOrder = drawingContext.fillRect.mock.invocationCallOrder;
    const markerOrder = fills.flatMap((f, i) => (isMarker(f) ? [rectOrder[i]] : []));
    const textOrder = drawingContext.fillText.mock.invocationCallOrder;
    expect(textOrder).toHaveLength(3);
    expect(markerOrder.length).toBeGreaterThan(0);
    expect(Math.min(...markerOrder)).toBeGreaterThan(Math.max(...textOrder));
    expect(fills.slice(fills.findIndex(isMarker)).every(isMarker)).toBe(true);
  });
});

describe("clock-source talking clock audio (issue 3072)", () => {
  const TURN = Date.UTC(2026, 9, 7, 18, 41, 30);
  const CLIPS = { n18: 0.6, n41: 0.7, n30: 0.5, n32: 0.6, n39: 0.6 };

  async function installAt(nowMs: number, setup: TalkingClockSetup) {
    vi.useFakeTimers();
    vi.setSystemTime(nowMs);
    const installed = installClockSource(() => true, undefined, setup);
    await vi.waitFor(() => expect(installed.intervals).toHaveLength(2));
    vi.setSystemTime(nowMs);
    return installed;
  }

  it("publishes digital silence between clips: one oscillator through gain 0, nothing else", () => {
    const { sources, gains, oscillators, destination } = installClockSource(() => true);
    expect(sources).toEqual([]);
    expect(oscillators).toHaveLength(1);
    const feeds = gains.filter((g) => g.connect.mock.calls.some(([to]) => to === destination));
    expect(feeds).toHaveLength(1);
    expect(feeds[0].gain.value).toBe(0);
    expect(oscillators[0].connect.mock.calls).toEqual([[feeds[0]]]);
  });

  it("schedules this bot's turn on the wall-clock second boundaries", async () => {
    const { intervals, sources, destination, statusLines } = await installAt(TURN - 500, {
      state: "running",
      currentTime: 100,
      fleetIndex: Math.floor(TURN / 10_000) % 4,
      fleetSize: 4,
      clips: CLIPS,
    });
    expect(statusLines.map(parseTalkingClockLine)).toEqual([
      "decoding",
      "ready: 5 clips decoded, 0 failed, speech model rotation",
    ]);
    intervals[1]();
    const spoken = sources;
    expect(spoken.map((s) => s.buffer?.clip)).toEqual(["n18", "n41", "n30", "n32", "n39"]);
    const starts = spoken.map((s) => s.start.mock.calls[0][0] as number);
    [100.5, 101.18, 101.96, 102.5, 109.5].forEach((t, i) => expect(starts[i]).toBeCloseTo(t, 9));
    for (const s of spoken) expect(s.connect).toHaveBeenCalledWith(destination);

    intervals[1]();
    expect(sources).toHaveLength(5);
  });

  it("stays silent during another bot's turn", async () => {
    const { intervals, sources } = await installAt(TURN - 500, {
      state: "running",
      fleetIndex: (Math.floor(TURN / 10_000) + 1) % 4,
      fleetSize: 4,
      clips: CLIPS,
    });
    intervals[1]();
    expect(sources).toEqual([]);
  });

  it("schedules nothing while the AudioContext is suspended", async () => {
    const { intervals, sources } = await installAt(TURN - 500, {
      state: "suspended",
      fleetIndex: Math.floor(TURN / 10_000) % 4,
      fleetSize: 4,
      clips: CLIPS,
    });
    intervals[1]();
    expect(sources).toEqual([]);
  });

  it("speaks a turn that began while suspended once the AudioContext resumes", async () => {
    const { intervals, sources, audio } = await installAt(TURN - 500, {
      state: "suspended",
      currentTime: 100,
      fleetIndex: Math.floor(TURN / 10_000) % 4,
      fleetSize: 4,
      clips: CLIPS,
    });
    intervals[1]();
    audio.state = "running";
    intervals[1]();
    expect(sources.map((s) => s.buffer?.clip)).toEqual(["n18", "n41", "n30", "n32", "n39"]);
  });

  it("drops the clips of a turn whose start times have already passed", async () => {
    const { intervals, sources } = await installAt(TURN + 2_500, {
      state: "running",
      currentTime: 100,
      fleetIndex: Math.floor(TURN / 10_000) % 4,
      fleetSize: 4,
      clips: CLIPS,
    });
    intervals[1]();
    const spoken = sources;
    expect(spoken.map((s) => s.buffer?.clip)).toEqual(["n39"]);
    expect(spoken[0].start.mock.calls[0][0]).toBeCloseTo(106.5, 9);
  });

  it("plays the room's planned backchannel only on the listener it names", async () => {
    const plans: {
      __CLOCK_SPEECH?: {
        backchannelForSlot(
          slot: number,
          n: number,
        ): { start: number; listener: number; clip: string } | null;
      };
    } = {};
    vm.runInNewContext(CLOCK_SPEECH_SCRIPT, plans);
    let slot = Math.floor(TURN / 4_000);
    while (plans.__CLOCK_SPEECH!.backchannelForSlot(slot, 4) === null) slot += 1;
    const planned = plans.__CLOCK_SPEECH!.backchannelForSlot(slot, 4)!;
    const nowMs = slot * 4_000 - loadClockSpeech(null).MAX_LEAD_MS - 500;
    const played = async (fleetIndex: number) => {
      const { intervals, sources } = await installAt(nowMs, {
        state: "running",
        currentTime: 50,
        fleetIndex,
        fleetSize: 4,
        clips: { [planned.clip]: 0.4 },
      });
      intervals[1]();
      intervals[1]();
      return sources.filter((s) => s.buffer?.clip === planned.clip);
    };

    const listened = await played(planned.listener);
    expect(listened).toHaveLength(1);
    expect(listened[0].start.mock.calls[0][0]).toBeCloseTo(50 + planned.start - nowMs / 1_000, 9);
    expect(await played((planned.listener + 1) % 4)).toEqual([]);
  });

  describe("weighted speech model (issue 3090)", () => {
    const plan: SpeechPlan = { ...SPEECH_PLAN_DEFAULT, seed: 3 };
    const pad = (n: number) => String(n).padStart(2, "0");
    const SAID = Object.fromEntries(
      [
        ...Array.from({ length: 60 }, (_, n) => `n${pad(n)}`),
        ...Array.from({ length: 10 }, (_, n) => `m${pad(n)}`),
        "f-um",
        "f-uh",
      ].map((clip) => [clip, 0.5]),
    );

    /** Ticks a page from `fromMs` to `toMs` like `setInterval` would; returns each clip's epoch start (s). */
    async function speak(
      fleetIndex: number,
      fromMs: number,
      toMs: number,
      over: Partial<TalkingClockSetup> = {},
    ) {
      const page = await installAt(fromMs, {
        state: "running",
        currentTime: 100,
        fleetIndex,
        fleetSize: 4,
        clips: SAID,
        plan,
        ...over,
      });
      const said: string[] = [];
      for (let t = fromMs; t <= toMs; t += 250) {
        vi.setSystemTime(t);
        page.audio.currentTime = 100 + (t - fromMs) / 1_000;
        const before = page.sources.length;
        page.intervals[1]();
        for (const src of page.sources.slice(before)) {
          const when = src.start.mock.calls[0][0] as number;
          said.push(
            `${src.buffer?.clip}@${(when - page.audio.currentTime + t / 1_000).toFixed(3)}`,
          );
        }
      }
      return said;
    }

    it("opens a MAX_LEAD_MS handoff at its lead whatever the tick phase", async () => {
      const turnMs = Date.UTC(2026, 9, 8, 14, 20, 0);
      const maxLead = loadClockSpeech(plan).MAX_LEAD_MS / 1_000;
      const speech = (real: Record<string, unknown>) => ({
        ...real,
        turnPlanFor: (epochS: number) =>
          epochS * 1_000 === turnMs ? { speaker: 0, lead: maxLead } : { speaker: 1, lead: 0 },
        backchannelForSlot: () => null,
      });
      const opened: Record<number, string | undefined> = {};
      for (const phase of [0, 1, 124, 249]) {
        const said = await speak(0, turnMs - 9_750 + phase, turnMs, { speech });
        opened[phase] = said[0]?.split("@")[1];
      }
      const want = (turnMs / 1_000 - maxLead).toFixed(3);
      expect(opened).toEqual({ 0: want, 1: want, 124: want, 249: want });
    });

    it("plays the same schedule on pages started minutes apart", async () => {
      const windowStart = Date.UTC(2026, 9, 8, 14, 20, 0);
      const windowEnd = windowStart + 120_000;
      const inWindow = (said: string[]) =>
        said.filter((s) => Number(s.split("@")[1]) * 1_000 >= windowStart + 2_000);
      const early: string[][] = [];
      const late: string[][] = [];
      for (const index of [0, 1, 2, 3]) {
        early.push(inWindow(await speak(index, windowStart - 8 * 60_000 + 500, windowEnd)));
        late.push(inWindow(await speak(index, windowStart - 30_000 + 500, windowEnd)));
      }
      expect(late).toEqual(early);
      expect(early.filter((said) => said.length > 0).length).toBeGreaterThan(1);
    });
  });

  it("reports no voice and keeps video working when no clips were injected", async () => {
    const { statusLines, getUserMedia, intervals } = installClockSource(() => true);
    expect(statusLines.map(parseTalkingClockLine)).toEqual(["no voice"]);
    expect(intervals).toHaveLength(1);
    const stream = await getUserMedia({ video: true, audio: true });
    expect(stream.getVideoTracks()).toHaveLength(1);
    expect(stream.getAudioTracks()).toHaveLength(1);
  });
});
