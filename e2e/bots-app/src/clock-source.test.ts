import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

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

function installClockSource(dimensionsReady: () => boolean): {
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
  const sandbox = {
    __CLOCK_PARTICIPANT: "",
    AudioContext: FakeAudioContext,
    Date,
    Intl,
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

  it("keeps EVERY drawn element inside the canvas, none left absolute", () => {
    // Parameterised over all three `fillText` calls, not just the label. Reverting
    // any ONE y-coordinate to its absolute 720p value must fail here: at 480 high,
    // `330` and `465` are both still < 480, so a per-element assertion passes while
    // the text is jammed together near the bottom — the silent failure #2171 names.
    //
    // The invariant is proportionality: each element must sit at the same FRACTION
    // of frame height it occupied in the 1280x720 reference layout.
    const { drawingContext, redraw, setParticipant, canvas } = installClockSource(() => true);
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

    // maxWidth, where present, must stay inside the frame.
    calls.forEach(([, , , maxWidth]) => {
      if (maxWidth !== undefined) expect(maxWidth).toBeLessThanOrEqual(canvas.width);
    });
  });

  it("captures at the geometry real publishers actually send", () => {
    const { canvas } = installClockSource(() => true);
    // 21 of 25 observed human publishers emit exactly 640x480; a 1280x720 clock
    // made every bot ~3x a real user's pixel load (#2171). Pinned because the
    // relational assertions above are all satisfied at 720p too, so without this
    // the resolution change has no regression test at all.
    expect([canvas.width, canvas.height]).toEqual([640, 480]);
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
