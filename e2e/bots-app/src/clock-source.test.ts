import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SD_SOURCE } from "./posture";

const CLOCK_SOURCE = readFileSync(
  fileURLToPath(new URL("./clock-source.js", import.meta.url)),
  "utf8",
);

type TrackKind = "audio" | "video";

// Set by the fake canvas below when the module configures it, so the track's
// reported dimensions follow the source instead of a hardcoded pair.
let capturedCanvasWidth = 0;
let capturedCanvasHeight = 0;

class FakeTrack {
  readonly stop = vi.fn();
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
  injected?: { width?: unknown; height?: unknown },
): {
  getUserMedia: (constraints: { video?: boolean; audio?: boolean }) => Promise<FakeMediaStream>;
  audioContextCount: () => number;
  clonedTracks: FakeTrack[];
  baseVideoTrack: FakeTrack;
  captureStream: ReturnType<typeof vi.fn>;
  drawingContext: {
    fillText: ReturnType<typeof vi.fn>;
  };
  canvas: { width: number; height: number };
  redraw: () => void;
  setParticipant: (participant: string) => void;
  intlCalls: { locale: unknown; options: Record<string, unknown> }[];
} {
  let audioContexts = 0;
  let redraw = (): void => {
    throw new Error("clock source did not install its draw interval");
  };
  const clonedTracks: FakeTrack[] = [];
  const baseVideoTrack = new FakeTrack("video", dimensionsReady, clonedTracks);
  const baseAudioTrack = new FakeTrack("audio", () => false, clonedTracks);

  class FakeAudioContext {
    readonly state = "suspended";

    constructor() {
      audioContexts += 1;
    }

    createMediaStreamDestination() {
      return { stream: new FakeMediaStream([baseAudioTrack]) };
    }

    createGain() {
      return {
        gain: { value: 1 },
        connect: vi.fn(),
      };
    }

    createOscillator() {
      return {
        connect: vi.fn(),
        start: vi.fn(),
      };
    }

    async resume(): Promise<void> {}
  }

  const drawingContext = {
    fillStyle: "",
    font: "",
    textAlign: "",
    textBaseline: "",
    fillRect: vi.fn(),
    fillText: vi.fn(),
  };
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
    AudioContext: FakeAudioContext,
    Date,
    Intl: { DateTimeFormat: RecordingDateTimeFormat },
    MediaStream: FakeMediaStream,
    Promise,
    console,
    document: {
      createElement: vi.fn(() => canvas),
    },
    navigator,
    setInterval: vi.fn((callback: () => void) => {
      redraw = callback;
    }),
    setTimeout,
  };

  vm.runInNewContext(CLOCK_SOURCE, sandbox, { filename: "clock-source.js" });
  return {
    getUserMedia: navigator.mediaDevices.getUserMedia,
    audioContextCount: () => audioContexts,
    clonedTracks,
    baseVideoTrack,
    captureStream,
    drawingContext,
    canvas,
    redraw: () => redraw(),
    setParticipant: (participant: string) => {
      sandbox.__CLOCK_PARTICIPANT = participant;
    },
    intlCalls,
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
    const { getUserMedia, clonedTracks } = installClockSource(() => false);
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
