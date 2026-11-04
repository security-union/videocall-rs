import vm from "node:vm";

import { describe, expect, it } from "vitest";

import { CLOCK_SPEECH_SCRIPT } from "./clock-init";

interface Clip {
  clip: string;
  at: number;
}
interface Backchannel {
  start: number;
  listener: number;
  clip: string;
}
interface ClockSpeech {
  TURN_SECONDS: number;
  fillerForTurn(turnStart: number): string | null;
  backchannelSlotFor(epochSeconds: number): number;
  backchannelForSlot(slot: number, fleetSize: number): Backchannel | null;
  speakerForSecond(epochSeconds: number, fleetSize: number): number;
  turnStartFor(epochSeconds: number): number;
  turnClips(turnStart: number, durationOf: (clip: string) => number | undefined): Clip[];
}

function loadSpeech(): ClockSpeech {
  const sandbox: { __CLOCK_SPEECH?: ClockSpeech } = {};
  vm.runInNewContext(CLOCK_SPEECH_SCRIPT, sandbox);
  if (sandbox.__CLOCK_SPEECH === undefined) throw new Error("clock-speech.js exported nothing");
  return sandbox.__CLOCK_SPEECH;
}

const speech = loadSpeech();
const utc = (h: number, m: number, s: number): number => Date.UTC(2026, 9, 7, h, m, s) / 1_000;
const byClip = (durations: Record<string, number>) => (clip: string) => durations[clip];

describe("talking clock turn schedule (issue 3072)", () => {
  it("rotates 10 s turns through fleet indices from the epoch", () => {
    expect(speech.TURN_SECONDS).toBe(10);
    const at = (s: number): number => speech.speakerForSecond(s, 4);
    expect([0, 9, 10, 19, 20, 30, 39, 40, 49].map(at)).toEqual([0, 0, 1, 1, 2, 3, 3, 0, 0]);
  });

  it("takes the turn modulo the fleet size, not a neighbouring size", () => {
    const t = utc(18, 41, 40);
    expect(Math.floor(t / 10) % 4).not.toBe(Math.floor(t / 10) % 5);
    expect(speech.speakerForSecond(t, 4)).toBe(Math.floor(t / 10) % 4);
    expect(speech.speakerForSecond(t, 1)).toBe(0);
  });

  it("gives every 10 s window exactly one speaker, cycling through all of them", () => {
    const n = 7;
    const start = utc(23, 59, 0);
    const seen = new Set<number>();
    for (let s = start; s < start + 10 * n; s += 1) {
      const speaker = speech.speakerForSecond(s, n);
      expect(speaker).toBe(speech.speakerForSecond(speech.turnStartFor(s), n));
      seen.add(speaker);
    }
    expect([...seen].sort()).toEqual([0, 1, 2, 3, 4, 5, 6]);
  });

  it("floors any second to its turn's start", () => {
    expect(speech.turnStartFor(utc(18, 41, 37))).toBe(utc(18, 41, 30));
    expect(speech.turnStartFor(utc(18, 41, 30))).toBe(utc(18, 41, 30));
  });
});

describe("talking clock clip sequence (issue 3072)", () => {
  it("says the full UTC time, then counts each later second on its boundary", () => {
    expect(speech.fillerForTurn(utc(18, 41, 30))).toBeNull();
    const clips = speech.turnClips(utc(18, 41, 30), byClip({ n18: 0.6, n41: 0.7, n30: 0.5 }));
    expect(clips.map((c) => c.clip)).toEqual([
      "n18",
      "n41",
      "n30",
      ...["n32", "n33", "n34", "n35", "n36", "n37", "n38", "n39"],
    ]);
    expect(clips[0].at).toBe(0);
    expect(clips[1].at).toBeCloseTo(0.68, 9);
    expect(clips[2].at).toBeCloseTo(1.46, 9);
    expect(clips.slice(3).map((c) => c.at)).toEqual([2, 3, 4, 5, 6, 7, 8, 9]);
  });

  it("drops each count second the full-time phrase is still speaking over", () => {
    expect(speech.fillerForTurn(utc(9, 5, 0))).toBeNull();
    const clips = speech.turnClips(utc(9, 5, 0), () => 1.2);
    expect(clips.slice(0, 3).map((c) => c.clip)).toEqual(["n09", "m05", "n00"]);
    expect(clips[3]).toEqual({ clip: "n04", at: 4 });
  });

  it("reads minutes 00-09 as clock minutes and wraps the count at the day boundary", () => {
    expect([utc(0, 0, 50), utc(23, 59, 50)].map(speech.fillerForTurn)).toEqual([null, null]);
    expect(speech.turnClips(utc(0, 0, 50), () => 0.4)[1].clip).toBe("m00");
    const lastTurn = speech.turnClips(utc(23, 59, 50), () => 0.4);
    expect(lastTurn.slice(0, 3).map((c) => c.clip)).toEqual(["n23", "n59", "n50"]);
    expect(lastTurn.at(-1)).toEqual({ clip: "n59", at: 9 });
  });

  it("treats a clip with no known duration as zero length rather than throwing", () => {
    const clips = speech.turnClips(utc(18, 41, 30), () => undefined);
    expect(clips.map((c) => c.at)).toEqual([0, 0.08, 0.16, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
  });
});

/** Every planned backchannel from `from` for `seconds`, as one bot computes it. */
function plan(fleetSize: number, from: number, seconds: number, s: ClockSpeech = speech) {
  const out: Backchannel[] = [];
  const last = s.backchannelSlotFor(from + seconds);
  for (let slot = s.backchannelSlotFor(from); slot < last; slot += 1) {
    const backchannel = s.backchannelForSlot(slot, fleetSize);
    if (backchannel !== null) out.push(backchannel);
  }
  return out;
}

const DAY = utc(0, 0, 0);

describe("talking clock speaker fillers (issue 3072)", () => {
  const fillerTurn = (): number => {
    for (let t = utc(18, 0, 0); ; t += 10) if (speech.fillerForTurn(t) !== null) return t;
  };

  it("opens about one turn in three with um or uh", () => {
    let filled = 0;
    const seen = new Set<string>();
    for (let t = DAY; t < DAY + 86_400; t += 10) {
      const filler = speech.fillerForTurn(t);
      if (filler !== null) {
        filled += 1;
        seen.add(filler);
      }
    }
    expect(filled / 8_640).toBeGreaterThan(0.3);
    expect(filled / 8_640).toBeLessThan(0.37);
    expect([...seen].sort()).toEqual(["f-uh", "f-um"]);
  });

  it("shifts the full time after the filler and keeps the count on the wall-clock seconds", () => {
    const t = fillerTurn();
    const filler = speech.fillerForTurn(t)!;
    const clips = speech.turnClips(t, (clip) => (clip === filler ? 0.5 : 0.6));
    expect(clips[0]).toEqual({ clip: filler, at: 0 });
    const hour = String(new Date(t * 1_000).getUTCHours()).padStart(2, "0");
    expect(clips[1].clip).toBe(`n${hour}`);
    expect(clips.slice(2, 4).map((c) => c.clip)).toEqual(
      speech
        .turnClips(t, () => 0)
        .slice(2, 4)
        .map((c) => c.clip),
    );
    [0.58, 1.26, 1.94].forEach((at, i) => expect(clips[i + 1].at).toBeCloseTo(at, 9));
    expect(clips.slice(4).map((c) => c.at)).toEqual([3, 4, 5, 6, 7, 8, 9]);
  });
});

describe("talking clock listener backchannels (issue 3072)", () => {
  const day = (n: number): Backchannel[] => plan(n, DAY, 86_400);

  it("plans at most one backchannel in any 2 s window across the room", () => {
    const starts = day(4).map((b) => b.start);
    const tooClose = starts.filter((start, i) => i > 0 && start - starts[i - 1] < 2);
    expect(starts.length).toBeGreaterThan(5_000);
    expect(tooClose.slice(0, 3)).toEqual([]);
  });

  it("averages one backchannel every 6-10 s", () => {
    const meanInterval = 86_400 / day(4).length;
    expect(meanInterval).toBeGreaterThan(6);
    expect(meanInterval).toBeLessThan(10);
  });

  it.each([2, 4, 20])("never gives the backchannel to the speaker (N=%i)", (n) => {
    const all = day(n);
    const bad = all.filter(
      (b) =>
        b.listener === speech.speakerForSecond(Math.floor(b.start), n) ||
        b.listener < 0 ||
        b.listener >= n,
    );
    expect(all.length).toBeGreaterThan(5_000);
    expect(bad.slice(0, 3)).toEqual([]);
    expect(new Set(all.map((b) => b.listener)).size).toBe(n);
  });

  it("starts every backchannel 3-9 s into its turn", () => {
    const outside = day(4).filter((b) => {
      const intoTurn = b.start - speech.turnStartFor(Math.floor(b.start));
      return intoTurn < 3 || intoTurn >= 9;
    });
    expect(outside.slice(0, 3)).toEqual([]);
  });

  it("plans nothing for a bot alone in the rotation", () => {
    expect(day(1)).toEqual([]);
  });

  it("gives every bot the same plan for the same time", () => {
    const otherBot = loadSpeech();
    const from = utc(18, 41, 0);
    expect(JSON.stringify(plan(4, from, 3_600, otherBot))).toBe(
      JSON.stringify(plan(4, from, 3_600)),
    );
    expect(plan(4, from, 3_600).length).toBeGreaterThan(0);
  });
});
