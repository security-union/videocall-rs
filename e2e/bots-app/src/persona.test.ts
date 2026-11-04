import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

import { buildClockInitScript, CLOCK_SPEECH_SCRIPT, loadClockSpeech } from "./clock-init";
import {
  assignPersonas,
  CAMERA_OFFSET,
  CAMERA_SHARE_SLACK,
  CAMERA_ON_DEFAULT,
  createPersonaPlanner,
  DELAY_P90_RATIO,
  MIC_ON_DEFAULT,
  MIN_TOGGLE_GAP_MS,
  MUTE,
  UNMUTE,
  type Interval,
  mutedListeners,
  personaFor,
  personaShortfalls,
  type PersonaSettings,
  personaTargets,
  resolvePersonaSettings,
  type RoomPersonas,
  sizeRow,
  stateAt,
} from "./persona";
import { SD_SOURCE } from "./posture";
import { SPEECH_PLAN_DEFAULT, type SpeechPlan } from "./speech-plan";

const settings = (env: Record<string, string> = {}): PersonaSettings => {
  const r = resolvePersonaSettings(true, env);
  if (r.kind !== "ok" || r.value === null) throw new Error(JSON.stringify(r));
  return r.value;
};
const room = (n: number, plan: SpeechPlan = SPEECH_PLAN_DEFAULT, s = settings()): RoomPersonas =>
  assignPersonas(createPersonaPlanner(plan, n), n, s);
const HOURS4 = 1_440;
const median = (xs: number[]): number => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];

describe("resolvePersonaSettings", () => {
  it("is off unless the flag or BOT_PERSONAS=on", () => {
    expect(resolvePersonaSettings(undefined, {})).toEqual({ kind: "ok", value: null });
    expect(resolvePersonaSettings(undefined, { BOT_PERSONAS: "off" })).toEqual({
      kind: "ok",
      value: null,
    });
    expect(resolvePersonaSettings(undefined, { BOT_PERSONAS: " ON " })).toMatchObject({
      kind: "ok",
      value: { cameraAlwaysOn: false },
    });
    expect(resolvePersonaSettings(true, {})).toMatchObject({
      kind: "ok",
      value: { micOn: [0.8, 0.65, 0.37, 0.25, 0.11] },
    });
  });

  it("refuses a malformed value and a persona knob without personas", () => {
    for (const env of [
      { BOT_PERSONAS: "yes" },
      { BOT_PERSONAS: "on", BOT_CAMERA: "on" },
      { BOT_PERSONAS: "on", BOT_PERSONA_MIC_ON: "0.8,0.6" },
      { BOT_PERSONAS: "on", BOT_PERSONA_CAMERA_ON: "0.5,0.5,0.5,0.5,1.5" },
      { BOT_CAMERA: "always-on" },
      { BOT_PERSONA_MIC_ON: "1,1,1,1,1" },
    ]) {
      expect(resolvePersonaSettings(undefined, env).kind, JSON.stringify(env)).toBe("invalid");
    }
    expect(settings({ BOT_CAMERA: "always-on", BOT_PERSONA_MIC_ON: "1,.5,0,0.25,1.0" })).toEqual({
      cameraAlwaysOn: true,
      micOn: [1, 0.5, 0, 0.25, 1],
      cameraOn: [0.53, 0.48, 0.14, 0.01, 0.08],
    });
  });

  it("maps room sizes onto the five rows", () => {
    expect([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 40].map(sizeRow)).toEqual([
      0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4,
    ]);
  });
});

describe("assignPersonas", () => {
  it("is the same room from two independent loads of the schedule", () => {
    for (const n of [3, 8]) {
      for (const seed of [0, 7]) {
        const plan = { ...SPEECH_PLAN_DEFAULT, seed, observers: 1 };
        const a = room(n, plan);
        const b = assignPersonas(createPersonaPlanner(plan, n), n, settings());
        expect(b).toEqual(a);
      }
    }
  });

  it("assigns every counted persona, at most one open mic, and none above 6 bots", () => {
    for (let n = 1; n <= 12; n += 1) {
      const r = room(n, { ...SPEECH_PLAN_DEFAULT, observers: n > 3 ? 1 : 0 });
      for (const [persona, count] of Object.entries(r.counts)) {
        expect(
          [...r.mic, ...r.camera].filter((p) => p === persona).length,
          `${persona} at ${n}`,
        ).toBe(count);
      }
      expect(r.counts.open, `open mics at ${n}`).toBeLessThanOrEqual(n <= 6 ? 1 : 0);
      expect(r.mic.length).toBe(n);
    }
    expect(room(6).counts.open).toBe(1);
    expect(room(7).counts.open).toBe(0);
  });

  it("makes the weight-0 bots, and only them, silent observers", () => {
    const plan = { ...SPEECH_PLAN_DEFAULT, observers: 2 };
    const planner = createPersonaPlanner(plan, 8);
    const r = assignPersonas(planner, 8, settings());
    expect(r.mic.map((m) => m === "observer")).toEqual(planner.weights.map((w) => w === 0));
    expect(r.counts.observer).toBe(2);
  });

  it("forces every camera on under BOT_CAMERA=always-on and keeps the mic personas", () => {
    const forced = room(8, SPEECH_PLAN_DEFAULT, settings({ BOT_CAMERA: "always-on" }));
    expect(forced.camera.every((c) => c === "always")).toBe(true);
    expect(forced.mic).toEqual(room(8).mic);
  });

  it("matches the README derivation table", () => {
    const readme = readFileSync(resolve(import.meta.dirname, "..", "README.md"), "utf8");
    for (const n of [3, 8]) {
      const row = new RegExp(`^\\|\\s*${n}\\s*\\|(.+)\\|\\s*$`, "m").exec(readme);
      expect(row, `README has no persona row for ${n} bots`).not.toBeNull();
      const r = room(n);
      const f = (x: number): string => x.toFixed(2);
      const c = r.counts;
      expect(row![1].split("|").map((cell) => cell.trim())).toEqual([
        `${c.open} / ${c["push-to-talk"]} / ${c.observer}`,
        `${c.always} / ${c["with-voice"]} / ${c.never}`,
        `${f(r.expected.micOn)} (${f(r.target.micOn)})`,
        `${f(r.expected.cameraOn)} (${f(r.target.cameraOn)})`,
        `${f(r.expected.micTogglesPerMin)} / ${f(r.expected.cameraTogglesPerMin)}`,
      ]);
    }
  });

  it("reports a share more than 0.05 below its target, and none at 0.045", () => {
    const at = (gap: number): string[] =>
      personaShortfalls({
        ...room(8),
        target: { micOn: 0.3 + gap, cameraOn: 0 },
        expected: { ...room(8).expected, micOn: 0.3 },
      });
    expect(at(0.055)).toEqual([expect.stringContaining("under-represents audio")]);
    expect(at(0.045)).toEqual([]);
  });

  it("states every timing default in the README parameter table", () => {
    const readme = readFileSync(resolve(import.meta.dirname, "..", "README.md"), "utf8");
    const rows = new Map(
      [...readme.matchAll(/^\|\s*([^|]+?)\s*\|\s*([-\d. /]+?)\s*\|[^|]+\|\s*$/gm)].map((m) => [
        m[1],
        m[2].split(" / ").map(Number),
      ]),
    );
    const expected: Record<string, readonly number[]> = {
      "Mic-on share by room size (`BOT_PERSONA_MIC_ON`)": MIC_ON_DEFAULT,
      "Camera-on share by room size (`BOT_PERSONA_CAMERA_ON`)": CAMERA_ON_DEFAULT,
      "Unmute before the stretch, median (s)": [UNMUTE.medianS],
      "Unmute before the stretch, maximum (s)": [UNMUTE.maxS],
      "Unmute after the stretch starts instead, probability": [UNMUTE.pLate],
      "Mute after the stretch, median (s)": [MUTE.medianS],
      "Mute after the stretch, maximum (s)": [MUTE.maxS],
      "Mute before the stretch ends instead, probability": [MUTE.pCut],
      "Unmute and mute delays, p90 / median (log-normal)": [DELAY_P90_RATIO],
      "Camera offset from the mic toggle, centre (s)": [CAMERA_OFFSET.meanS],
      "Camera offset, IQR (s)": [CAMERA_OFFSET.iqrS],
      "Camera offset, maximum either way (s)": [CAMERA_OFFSET.maxAbsS],
      "Minimum time between toggles of one control (s)": [MIN_TOGGLE_GAP_MS / 1_000],
      "Camera share slack for fewer toggles": [CAMERA_SHARE_SLACK],
    };
    for (const [label, values] of Object.entries(expected)) {
      expect(rows.get(label), label).toEqual([...values]);
    }
  });

  it("takes a zero-toggle camera mix exactly at the slack, and not just outside it", () => {
    const at = (target: string): RoomPersonas =>
      room(
        3,
        SPEECH_PLAN_DEFAULT,
        settings({ BOT_PERSONA_CAMERA_ON: `${target},0.48,0.14,0.01,0.08` }),
      );
    expect(at("0.15").counts).toMatchObject({ always: 0, "with-voice": 0 });
    expect(at("0.1501").counts["with-voice"]).toBeGreaterThan(0);
  });

  it("keeps no open mic unless one is more than 0.005 closer to the mic-on target", () => {
    const micOn = (target: string): RoomPersonas =>
      room(
        3,
        SPEECH_PLAN_DEFAULT,
        settings({ BOT_PERSONA_MIC_ON: `${target},0.65,0.37,0.25,0.11` }),
      );
    const shut = micOn("0").expected.micOn;
    const open = micOn("1").expected.micOn;
    const mid = (shut + open) / 2;
    expect(micOn((mid + 0.0022).toFixed(4)).counts.open).toBe(0);
    expect(micOn((mid + 0.0028).toFixed(4)).counts.open).toBe(1);
  });

  it("breaks a toggle tie by the closest camera share", () => {
    const r = room(
      8,
      SPEECH_PLAN_DEFAULT,
      settings({ BOT_PERSONA_CAMERA_ON: "0.53,0.48,0.14,0.12,0.08" }),
    );
    expect(r.counts).toMatchObject({ always: 1, "with-voice": 0, never: 7 });
  });

  it("reports a share below target as an under-representation", () => {
    expect(personaShortfalls(room(3))).toEqual([expect.stringContaining("under-represents audio")]);
    expect(
      personaShortfalls(
        room(3, SPEECH_PLAN_DEFAULT, settings({ BOT_PERSONA_MIC_ON: "0.5,0.65,0.37,0.25,0.11" })),
      ),
    ).toEqual([]);
  });
});

describe("planned toggles", () => {
  const plan = SPEECH_PLAN_DEFAULT;
  const n = 3;
  const speech = loadClockSpeech(plan);
  const schedule = speech.createSchedule(plan, n);
  const planner = createPersonaPlanner(plan, n);
  const SPAN = 4_320;

  /** Stretches of `index` with no own stretch within 40 s either side, so no edge merged. */
  function isolated(index: number): Array<{ s: number; e: number; lead: number }> {
    const out: Array<{ s: number; e: number; lead: number }> = [];
    for (let b = 1; b < SPAN - 1; b += 1) {
      if (schedule.blockAt(b).speaker !== index || schedule.blockAt(b - 1).speaker === index)
        continue;
      let last = b;
      while (schedule.blockAt(last + 1).speaker === index) last += 1;
      const quiet = (from: number, to: number): boolean => {
        for (let k = from; k <= to; k += 1) if (schedule.blockAt(k).speaker === index) return false;
        return true;
      };
      if (quiet(b - 5, b - 1) && quiet(last + 1, last + 5)) {
        const lead = schedule.blockAt(b).lead * 1_000;
        out.push({ s: b * 10_000 - lead, e: (last + 1) * 10_000, lead });
      }
      b = last;
    }
    return out;
  }
  const containing = (list: Interval[], s: number, e: number): Interval => {
    const iv = list.find((x) => x.off > s - 20_000 && x.on < e + 20_000);
    if (iv === undefined) throw new Error(`no interval near ${s}`);
    return iv;
  };
  const stretches = [0, 1, 2].flatMap((i) => isolated(i).map((st) => ({ ...st, i })));
  const voice = [0, 1, 2].map((i) => planner.voiceIntervals(i, -1, SPAN));

  it("has enough isolated stretches to measure", () => {
    expect(stretches.length).toBeGreaterThan(500);
  });

  it("unmutes before the stretch, lead included, except about 15% late", () => {
    const late = stretches.filter((st) => containing(voice[st.i].mic, st.s, st.e).on > st.s);
    expect(late.length / stretches.length).toBeGreaterThan(0.11);
    expect(late.length / stretches.length).toBeLessThan(0.19);
    const led = stretches.filter((st) => st.lead > 0);
    expect(led.length).toBeGreaterThan(20);
    const ledLate = led.filter((st) => containing(voice[st.i].mic, st.s, st.e).on > st.s);
    expect(ledLate.length / led.length).toBeLessThan(0.25);
  });

  it("mutes seconds after the stretch, rarely before it ends", () => {
    const delays = stretches.map((st) => containing(voice[st.i].mic, st.s, st.e).off - st.e);
    expect(median(delays) / 1_000).toBeGreaterThan(2.4);
    expect(median(delays) / 1_000).toBeLessThan(4.0);
    expect(Math.max(...delays)).toBeLessThanOrEqual(10_000);
    expect(delays.filter((d) => d < 0).length / delays.length).toBeLessThan(0.08);
    expect(
      delays.filter((d) => d < 0).length,
      "no stretch took the early-mute branch",
    ).toBeGreaterThan(5);
  });

  it("moves the with-voice camera around the mic toggle, earlier more often", () => {
    const offsets = stretches.map(
      (st) =>
        containing(voice[st.i].camera, st.s, st.e).on - containing(voice[st.i].mic, st.s, st.e).on,
    );
    expect(median(offsets) / 1_000).toBeGreaterThan(-1.0);
    expect(median(offsets) / 1_000).toBeLessThan(-0.2);
    const sorted = [...offsets].sort((a, b) => a - b);
    const iqr =
      (sorted[Math.floor(sorted.length * 0.75)] - sorted[Math.floor(sorted.length * 0.25)]) / 1_000;
    expect(iqr).toBeGreaterThan(4.0);
    expect(iqr).toBeLessThan(5.6);
  });

  it("caps the unmute lead at 3 s, the mute delay at 10 s and the camera offset at 10 s", () => {
    const iv = (kind: "mic" | "camera", st: (typeof stretches)[number]): Interval =>
      containing(voice[st.i][kind], st.s, st.e);
    const leads = stretches.map((st) => st.s - iv("mic", st).on);
    const delays = stretches.map((st) => iv("mic", st).off - st.e);
    const offsets = stretches.flatMap((st) => [
      iv("camera", st).on - iv("mic", st).on,
      iv("camera", st).off - iv("mic", st).off,
    ]);
    expect(Math.max(...leads)).toBeCloseTo(3_000, 3);
    expect(Math.max(...delays)).toBeCloseTo(10_000, 3);
    expect(Math.min(...offsets)).toBeCloseTo(-10_000, 3);
  });

  it("starts a lone bot's endless stretch 360 blocks back", () => {
    const lone = createPersonaPlanner({ model: "rotation" }, 1);
    const b = 100_000;
    const on = lone.voiceIntervals(0, b, b).mic[0].on;
    expect(on).toBeGreaterThanOrEqual((b - 360) * 10_000 - 3_000);
    expect(on).toBeLessThanOrEqual((b - 360) * 10_000 + 3_000);
  });

  it("never plans two edges of one control within 1 s", () => {
    // Two bots in rotation: each 10 s stretch ends about 10 s before the next, so mute and unmute crowd.
    const rotation = createPersonaPlanner({ model: "rotation" }, 2);
    const plans = [...voice, ...[0, 1].map((i) => rotation.voiceIntervals(i, -1, HOURS4))];
    let merged = 0;
    for (const v of plans) {
      for (const list of [v.mic, v.camera]) {
        const edges = list.flatMap((iv) => [iv.on, iv.off]);
        for (let k = 1; k < edges.length; k += 1) {
          expect(edges[k] - edges[k - 1]).toBeGreaterThanOrEqual(1_000);
        }
        merged += list.filter((iv) => iv.off - iv.on > 25_000).length;
      }
    }
    expect(merged, "no stretch merged with the next, so the gap rule never ran").toBeGreaterThan(
      50,
    );
  });

  it("gives the same state from the windowed lookup as from the whole plan", () => {
    const r = room(n);
    for (let i = 0; i < n; i += 1) {
      const p = {
        ...personaFor(r, { index: i, fleetSize: n }),
        mic: "push-to-talk" as const,
        camera: "with-voice" as const,
      };
      for (let t = 600_000; t < (HOURS4 - 60) * 10_000; t += 3_700) {
        const got = personaTargets(planner, p, t);
        for (const kind of ["mic", "camera"] as const) {
          const whole = stateAt(voice[i][kind], t);
          expect(got[kind].on, `${kind} ${i} at ${t}`).toBe(whole.on);
          if (t - whole.since <= 20_000) expect(got[kind].since).toBe(whole.since);
        }
      }
    }
  });

  it("holds the fixed personas", () => {
    const base = personaFor(room(n), { index: 0, fleetSize: n });
    for (const t of [0, 123_456_789, 1_760_000_000_000]) {
      expect(personaTargets(planner, { ...base, mic: "open", camera: "always" }, t)).toEqual({
        mic: { on: true, since: -Infinity },
        camera: { on: true, since: -Infinity },
      });
      expect(personaTargets(planner, { ...base, mic: "observer", camera: "never" }, t)).toEqual({
        mic: { on: false, since: -Infinity },
        camera: { on: false, since: -Infinity },
      });
    }
  });
});

describe("backchannels of muted listeners (issue 3091)", () => {
  const plan = SPEECH_PLAN_DEFAULT;
  const slots = Array.from({ length: 5_000 }, (_, k) => 440_000_000 + k);

  it("drops, and never redraws, a slot whose listener is muted", () => {
    const open = loadClockSpeech(plan);
    const muted = loadClockSpeech(plan, [0, 1]);
    let dropped = 0;
    for (const slot of slots) {
      const all = open.backchannelForSlot(slot, 3);
      const kept = muted.backchannelForSlot(slot, 3);
      if (all !== null && all.listener !== 2) {
        expect(kept).toBeNull();
        dropped += 1;
      } else {
        expect(kept).toEqual(all);
      }
    }
    expect(dropped).toBeGreaterThan(100);
  });

  it("drops nothing when no listener is muted", () => {
    const a = loadClockSpeech(plan);
    const b = loadClockSpeech(plan, []);
    for (const slot of slots.slice(0, 500))
      expect(b.backchannelForSlot(slot, 8)).toEqual(a.backchannelForSlot(slot, 8));
  });

  it("mutes every bot that is not an open mic, and the page receives that set before the schedule loads", () => {
    const r = room(3);
    expect(r.mic).toEqual(["push-to-talk", "push-to-talk", "open"]);
    expect(mutedListeners(r)).toEqual([0, 1]);
    const script = buildClockInitScript({
      displayName: "x",
      sourceGeometry: SD_SOURCE,
      videoNoise: 0,
      slot: { index: 0, fleetSize: 3 },
      voice: null,
      speechPlan: plan,
      mutedListeners: [0, 1],
    });
    const at = script.indexOf("globalThis.__CLOCK_MUTED_LISTENERS = [0,1];");
    expect(at).toBeGreaterThan(-1);
    expect(at).toBeLessThan(script.indexOf(CLOCK_SPEECH_SCRIPT));
  });
});
