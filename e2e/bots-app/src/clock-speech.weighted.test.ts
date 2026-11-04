import { describe, expect, it } from "vitest";

import { loadClockSpeech, type TurnPlan } from "./clock-init";
import { SPEECH_PLAN_DEFAULT, type WeightedSpeechPlan } from "./speech-plan";

const plan = (over: Partial<WeightedSpeechPlan> = {}): WeightedSpeechPlan => ({
  ...SPEECH_PLAN_DEFAULT,
  ...over,
});
const schedule = (p: WeightedSpeechPlan, fleetSize: number) =>
  loadClockSpeech(p).createSchedule(p, fleetSize);
const blockAtUtc = (h: number, m: number): number => Date.UTC(2026, 9, 8, h, m, 0) / 10_000;
const DAY = blockAtUtc(0, 0);
const MAX_LEAD_S = loadClockSpeech(null).MAX_LEAD_MS / 1_000;

function walk(blockAt: (block: number) => TurnPlan, from: number, blocks: number) {
  const counts = new Map<number, number>();
  let handoffs = 0;
  let overlaps = 0;
  const badLeads: { block: number; lead: number }[] = [];
  for (let k = from; k < from + blocks; k += 1) {
    const { speaker, lead } = blockAt(k);
    counts.set(speaker, (counts.get(speaker) ?? 0) + 1);
    const handoff = k > from && blockAt(k - 1).speaker !== speaker;
    if (handoff) handoffs += 1;
    if (handoff && lead > 0) overlaps += 1;
    if (lead < 0 || lead > MAX_LEAD_S || (!handoff && k > from && lead !== 0))
      badLeads.push({ block: k, lead });
  }
  return { counts, handoffs, overlaps, badLeads };
}

describe("weighted speech: talkativeness weights (issue 3090)", () => {
  const sorted = (w: readonly number[]) => [...w].sort((a, b) => b - a);

  it("gives the dominant talkers their shares and splits the rest evenly", () => {
    const w = sorted(schedule(plan(), 8).weights);
    [0.35, 0.2, 0.075, 0.075, 0.075, 0.075, 0.075, 0.075].forEach((x, i) =>
      expect(w[i]).toBeCloseTo(x, 9),
    );
  });

  it("renormalises a room too small for the configured shares, keeping dominant talkers on top", () => {
    const shares = (n: number) => sorted(schedule(plan(), n).weights);
    [7 / 15, 4 / 15, 4 / 15].forEach((x, i) => expect(shares(3)[i]).toBeCloseTo(x, 9));
    [7 / 11, 4 / 11].forEach((x, i) => expect(shares(2)[i]).toBeCloseTo(x, 9));
    expect(shares(1)).toEqual([1]);
  });

  it("gives silent observers weight 0 and they never take the floor", () => {
    const sch = schedule(plan({ observers: 2 }), 8);
    const w = sorted(sch.weights);
    [0.35, 0.2, 0.1125, 0.1125, 0.1125, 0.1125, 0, 0].forEach((x, i) =>
      expect(w[i]).toBeCloseTo(x, 9),
    );
    const silent = sch.weights.flatMap((x, i) => (x === 0 ? [i] : []));
    const { counts } = walk(sch.blockAt, DAY, 8_640);
    expect(silent.map((i) => counts.get(i) ?? 0)).toEqual([0, 0]);
  });
});

describe("weighted speech: 30-minute acceptance over seeds (issue 3090)", () => {
  const windows = [blockAtUtc(14, 0), blockAtUtc(14, 45), blockAtUtc(3, 15)];
  const seeds = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

  it("keeps each dominant talker within 5 points of its weight, and the others combined", () => {
    const misses: string[] = [];
    for (const seed of seeds) {
      const sch = schedule(plan({ seed }), 8);
      const dominant = [0.35, 0.2].map((x) => sch.weights.findIndex((w) => Math.abs(w - x) < 1e-9));
      for (const from of windows) {
        const { counts } = walk(sch.blockAt, from, 180);
        const share = (i: number) => (counts.get(i) ?? 0) / 180;
        const others = 1 - share(dominant[0]) - share(dominant[1]);
        const got = [share(dominant[0]), share(dominant[1]), others];
        [0.35, 0.2, 0.45].forEach((want, i) => {
          if (Math.abs(got[i] - want) > 0.05)
            misses.push(`seed ${seed} @${from}: ${got[i]} vs ${want}`);
        });
      }
    }
    expect(misses).toEqual([]);
  });

  it("overlaps the configured fraction of handoffs, none starting more than MAX_LEAD_MS early", () => {
    const misses: string[] = [];
    for (const seed of seeds) {
      const sch = schedule(plan({ seed }), 8);
      for (const from of windows) {
        const { handoffs, overlaps, badLeads } = walk(sch.blockAt, from, 180);
        expect(badLeads).toEqual([]);
        if (Math.abs(overlaps / handoffs - 0.3) > 0.1) misses.push(`seed ${seed} @${from}`);
      }
    }
    expect(misses).toEqual([]);
  });

  it("draws each lead as MAX_LEAD_MS * u^2: quartiles 0.25 / 1 / 2.25 s, mean 1.33 s, max 4 s", () => {
    expect(MAX_LEAD_S).toBe(4);
    const leads: number[] = [];
    for (const seed of seeds) {
      const { blockAt } = schedule(plan({ seed }), 8);
      for (let k = DAY; k < DAY + 8_640; k += 1)
        if (blockAt(k).lead > 0) leads.push(blockAt(k).lead);
    }
    leads.sort((a, b) => a - b);
    const quantile = (q: number) => leads[Math.floor(q * leads.length)];
    expect(leads.length).toBeGreaterThan(5_000);
    const mean = leads.reduce((a, b) => a + b, 0) / leads.length;
    const got = [quantile(0.25), quantile(0.5), quantile(0.75), mean];
    [0.25, 1, 2.25, 4 / 3].forEach((want, i) => expect(Math.abs(got[i] - want)).toBeLessThan(0.1));
    expect(leads[leads.length - 1]).toBeGreaterThan(3.9);
    expect(leads[leads.length - 1]).toBeLessThanOrEqual(MAX_LEAD_S);
  });

  it("keeps lead > 0: per hour, within 1 of overlap * handoffs", () => {
    const misses: string[] = [];
    let hours = 0;
    for (const fleetSize of [3, 4, 8]) {
      for (const seed of [1, 2, 3, 4, 5]) {
        const { blockAt } = schedule(plan({ seed }), fleetSize);
        for (let anchor = DAY; anchor < DAY + 8_640; anchor += 360) {
          let handoffs = 0;
          let overlaps = 0;
          for (let k = anchor; k < anchor + 360; k += 1) {
            if (blockAt(k).speaker === blockAt(k - 1).speaker) continue;
            handoffs += 1;
            if (blockAt(k).lead > 0) overlaps += 1;
          }
          hours += 1;
          if (Math.abs(overlaps - 0.3 * handoffs) > 1)
            misses.push(`n=${fleetSize} seed ${seed} @${anchor}: ${overlaps}/${handoffs}`);
        }
      }
    }
    expect(hours).toBe(360);
    expect(misses).toEqual([]);
  });

  it("never overlaps when the overlap fraction is 0", () => {
    const { overlaps, handoffs } = walk(schedule(plan({ overlap: 0 }), 8).blockAt, DAY, 8_640);
    expect(handoffs).toBeGreaterThan(1_000);
    expect(overlaps).toBe(0);
  });
});

describe("weighted speech: keeping the floor (issue 3090)", () => {
  const handoffsPerDay = (keep: number) =>
    walk(schedule(plan({ keep }), 8).blockAt, DAY, 8_640).handoffs;

  it("hands off at nearly every block when keep is 0", () => {
    expect(handoffsPerDay(0)).toBeGreaterThan(8_640 * 0.95);
  });

  it("holds the floor longer as keep rises", () => {
    const [half, default75, ninety] = [0.5, 0.75, 0.9].map(handoffsPerDay);
    expect(default75).toBeLessThan(half * 0.8);
    expect(ninety).toBeLessThan(default75 * 0.85);
  });
});

describe("weighted speech: replay from the UTC hour (issue 3090)", () => {
  const seven = plan({ seed: 7 });
  const speakers = (from: number, n: number, blockAt = schedule(seven, 4).blockAt) =>
    Array.from({ length: n }, (_, i) => blockAt(from + i).speaker).join("");

  it("pins one seed's schedule at the hour top, across the half hour and into the next hour", () => {
    const hour = blockAtUtc(14, 0);
    expect(speakers(hour, 12)).toBe("103332221111");
    expect(speakers(hour + 174, 12)).toBe("222331111110");
    expect(speakers(hour + 354, 12)).toBe("011111300122");
  });

  it("answers any block the same whichever blocks were asked first", () => {
    const from = blockAtUtc(14, 50);
    const reversed = schedule(seven, 4);
    const backwards = Array.from({ length: 120 }, (_, i) => reversed.blockAt(from + 119 - i));
    const fresh = schedule(seven, 4);
    expect(backwards.reverse()).toEqual(
      Array.from({ length: 120 }, (_, i) => fresh.blockAt(from + i)),
    );
  });

  it("leads only on a real handoff across the hour boundary", () => {
    let continued = 0;
    let handedOff = 0;
    for (const seed of [1, 2, 3, 4, 5, 6, 7, 8]) {
      const sch = schedule(plan({ seed }), 3);
      for (let h = 0; h < 24; h += 1) {
        const top = blockAtUtc(h, 0);
        const before = sch.blockAt(top - 1).speaker;
        const { speaker, lead } = sch.blockAt(top);
        if (speaker === before) {
          continued += 1;
          expect(lead).toBe(0);
        } else if (lead > 0) handedOff += 1;
      }
    }
    expect(continued).toBeGreaterThan(10);
    expect(handedOff).toBeGreaterThan(10);
  });

  it("moves with the seed", () => {
    const hour = blockAtUtc(14, 0);
    expect(speakers(hour, 60, schedule(plan({ seed: 8 }), 4).blockAt)).not.toBe(speakers(hour, 60));
  });
});

describe("weighted speech: backchannels (issue 3090)", () => {
  it("never gives a backchannel to the bot about to open the next block early", () => {
    const p = plan();
    const speech = loadClockSpeech(p);
    const sch = speech.createSchedule(p, 3);
    let early = 0;
    const clashes: number[] = [];
    for (let slot = DAY * 2.5; slot < (DAY + 8_640) * 2.5; slot += 1) {
      const b = speech.backchannelForSlot(slot, 3);
      if (b === null) continue;
      const next = sch.blockAt(Math.floor(b.start / 10) + 1);
      if (next.lead > 0) early += 1;
      if (next.lead > 0 && b.listener === next.speaker) clashes.push(slot);
    }
    expect(early).toBeGreaterThan(100);
    expect(clashes).toEqual([]);
  });

  it("drops a 2-bot room's backchannel when the only listener opens the next block early", () => {
    const p = plan();
    const speech = loadClockSpeech(p);
    const sch = speech.createSchedule(p, 2);
    const kept: { listener: number; nextLead: number }[] = [];
    for (let slot = DAY * 2.5; slot < (DAY + 8_640) * 2.5; slot += 1) {
      const b = speech.backchannelForSlot(slot, 2);
      if (b !== null) {
        kept.push({
          listener: b.listener,
          nextLead: sch.blockAt(Math.floor(b.start / 10) + 1).lead,
        });
      }
    }
    expect(kept.length).toBeGreaterThan(1_000);
    expect(kept.filter((b) => b.nextLead > 0 || ![0, 1].includes(b.listener))).toEqual([]);
  });
});
