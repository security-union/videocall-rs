import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

import { afterEach, describe, expect, it, vi } from "vitest";

const CLOCK_SOURCE = readFileSync(
  fileURLToPath(new URL("./clock-source.js", import.meta.url)),
  "utf8",
);

type TrackKind = "audio" | "video";

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
    return { width: 1280, height: 720 };
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
    width: 0,
    height: 0,
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
    redraw: () => redraw(),
    setParticipant: (participant: string) => {
      sandbox.__CLOCK_PARTICIPANT = participant;
    },
  };
}

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
    const { drawingContext, redraw, setParticipant } = installClockSource(() => true);
    drawingContext.fillText.mockClear();

    setParticipant("Late Participant");
    redraw();

    expect(drawingContext.fillText).toHaveBeenCalledWith("Late Participant", 640, 585, 1_160);
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
