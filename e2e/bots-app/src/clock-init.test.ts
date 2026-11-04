import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import vm from "node:vm";

import { afterEach, describe, expect, it } from "vitest";

import {
  buildClockInitScript,
  type ClockSpeech,
  CLOCK_SOURCE_SCRIPT,
  CLOCK_SPEECH_SCRIPT,
  CLOCK_VOICES_DIR,
  listClockVoices,
  loadClockSpeech,
  parseTalkingClockLine,
  resolveClockVoice,
  speechPlanText,
  talkingClockLine,
} from "./clock-init";
import { SD_SOURCE } from "./posture";
import { ROTATION_PLAN, SPEECH_PLAN_DEFAULT } from "./speech-plan";

const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..");

/** `name -> voice` pairs from a Python source, in file order. */
function pythonVoiceTable(file: string, pattern: RegExp): [string, string][] {
  const text = readFileSync(resolve(REPO_ROOT, file), "utf8");
  return [...text.matchAll(pattern)].map((m) => [m[1], m[2]]);
}

/** Every clip name the production schedule can ask for over a whole UTC day. */
function clipsTheScheduleUses(): Set<string> {
  const sandbox: {
    __CLOCK_SPEECH?: {
      turnClips(t: number, d: () => number): { clip: string }[];
      backchannelForSlot(slot: number, fleetSize: number): { clip: string } | null;
    };
  } = {};
  vm.runInNewContext(CLOCK_SPEECH_SCRIPT, sandbox);
  const speech = sandbox.__CLOCK_SPEECH!;
  const used = new Set<string>();
  for (let t = 0; t < 86_400; t += 10) {
    for (const { clip } of speech.turnClips(t, () => 0)) used.add(clip);
  }
  for (let slot = 0; slot < 86_400 / 4; slot += 1) {
    const backchannel = speech.backchannelForSlot(slot, 4);
    if (backchannel !== null) used.add(backchannel.clip);
  }
  return used;
}

let scratch: string | null = null;
afterEach(() => {
  if (scratch !== null) rmSync(scratch, { recursive: true, force: true });
  scratch = null;
});

describe("talking clock voices (issue 3072)", () => {
  it("mirrors the conversation generator's participant → voice table", () => {
    const conversation = pythonVoiceTable(
      "bot/generate-conversation-edge.py",
      /\{"name":\s*"(\w+)",\s*"voice":\s*"([\w-]+)"/g,
    );
    const clock = pythonVoiceTable(
      "e2e/bots-app/scripts/generate-clock-voices.py",
      /^ {4}"(\w+)": "([\w-]+Neural)",$/gm,
    );
    expect(conversation.length).toBe(20);
    expect(clock.length).toBe(20);
    expect(clock).toEqual(conversation);
  });

  it("ships at least four voices, each with every clip the schedule can play", () => {
    const voices = listClockVoices();
    expect(voices.length).toBeGreaterThanOrEqual(4);
    const needed = clipsTheScheduleUses();
    expect(needed.size).toBe(80);
    for (const voice of voices) {
      const shipped = new Set(Object.keys(resolveClockVoice(voice, 0)!.clips));
      expect(
        [...needed].filter((clip) => !shipped.has(clip)),
        voice,
      ).toEqual([]);
      for (const clip of needed) {
        expect(statSync(join(CLOCK_VOICES_DIR, voice, `${clip}.ogg`)).size).toBeGreaterThan(0);
      }
    }
  });

  it("uses the participant's own voice, case-insensitively", () => {
    const voice = resolveClockVoice("Alice", 3);
    expect(voice?.name).toBe("alice");
    expect(voice?.fallback).toBe(false);
  });

  it("falls back to a shipped voice chosen by fleet index for an unknown participant", () => {
    const voices = listClockVoices();
    for (const index of [0, 1, 5, 42]) {
      const voice = resolveClockVoice(`bot-${index}`, index);
      expect(voice?.fallback).toBe(true);
      expect(voice?.name).toBe(voices[index % voices.length]);
      expect(Object.keys(voice?.clips ?? {}).length).toBeGreaterThan(0);
    }
  });

  it("resolves no voice, and says the bot never speaks, when no clips are shipped", () => {
    scratch = mkdtempSync(join(tmpdir(), "clock-voices-"));
    expect(resolveClockVoice("alice", 0, scratch)).toBeNull();
    expect(resolveClockVoice("alice", 0, join(scratch, "absent"))).toBeNull();
    expect(talkingClockLine({ index: 0, fleetSize: 4 }, null, ROTATION_PLAN)).toContain(
      "never speaks",
    );
  });

  it("reads only .ogg clips into base64", () => {
    scratch = mkdtempSync(join(tmpdir(), "clock-voices-"));
    mkdirSync(join(scratch, "zed"));
    writeFileSync(join(scratch, "zed", "n07.ogg"), "OggS-seven");
    writeFileSync(join(scratch, "zed", "notes.txt"), "ignored");
    expect(resolveClockVoice("zed", 0, scratch)?.clips).toEqual({
      n07: Buffer.from("OggS-seven").toString("base64"),
    });
  });
});

describe("talking clock run-output line and init script (issue 3072)", () => {
  const voice = { name: "bob", fallback: true, clips: { n01: "AAAA" } };

  it("names the voice and the turn, and flags an index outside the rotation", () => {
    expect(talkingClockLine({ index: 1, fleetSize: 4 }, voice, ROTATION_PLAN)).toBe(
      "talking clock: voice=bob (fallback: no voice for this participant), speaks turn 1 of 4",
    );
    expect(talkingClockLine({ index: 4, fleetSize: 4 }, voice, ROTATION_PLAN)).toContain(
      "NEVER speaks",
    );
  });

  it("forwards only the page's talking-clock console lines", () => {
    expect(parseTalkingClockLine("[talking-clock] ready: 70 clips decoded, 0 failed")).toBe(
      "ready: 70 clips decoded, 0 failed",
    );
    expect(parseTalkingClockLine("diagnostics packets: ENABLED")).toBeNull();
  });

  it("sets the slot and clips before the speech library, and the library before the source", () => {
    const script = buildClockInitScript({
      displayName: "Bob",
      sourceGeometry: SD_SOURCE,
      videoNoise: 0.1,
      slot: { index: 2, fleetSize: 5 },
      voice,
      speechPlan: ROTATION_PLAN,
    });
    expect(script).toContain("globalThis.__CLOCK_FLEET_INDEX = 2;\n");
    expect(script).toContain("globalThis.__CLOCK_FLEET_SIZE = 5;\n");
    expect(script).toContain('globalThis.__CLOCK_VOICE_CLIPS = {"n01":"AAAA"};\n');
    const speechAt = script.indexOf(CLOCK_SPEECH_SCRIPT);
    expect(script.indexOf("__CLOCK_VOICE_CLIPS")).toBeLessThan(speechAt);
    expect(speechAt).toBeLessThan(script.indexOf(CLOCK_SOURCE_SCRIPT));
  });

  it("injects the speech plan, so the page runs the weighted schedule and not the bare rotation", () => {
    const script = buildClockInitScript({
      displayName: "Bob",
      sourceGeometry: SD_SOURCE,
      videoNoise: 0.1,
      slot: { index: 2, fleetSize: 8 },
      voice,
      speechPlan: SPEECH_PLAN_DEFAULT,
    });
    const page: { __CLOCK_SPEECH?: ClockSpeech } = {};
    // clock-source.js needs a DOM; the speech library before it has already run.
    expect(() => vm.runInNewContext(script, page)).toThrow();
    const expected = loadClockSpeech(SPEECH_PLAN_DEFAULT);
    const t0 = Date.UTC(2026, 9, 8, 14, 0, 0) / 1_000;
    const turns = (s: ClockSpeech) =>
      Array.from({ length: 360 }, (_, i) => s.turnPlanFor(t0 + 10 * i, 8));
    expect(page.__CLOCK_SPEECH?.model).toBe("weighted");
    expect(turns(page.__CLOCK_SPEECH!)).toEqual(turns(expected));
    expect(turns(page.__CLOCK_SPEECH!)).not.toEqual(turns(loadClockSpeech(null)));
  });

  it("names a weighted bot's share, and flags a silent observer", () => {
    const share = (index: number, observers = 0) =>
      talkingClockLine({ index, fleetSize: 8 }, voice, { ...SPEECH_PLAN_DEFAULT, observers });
    const lines = [0, 1, 2, 3, 4, 5, 6, 7].map((i) => share(i));
    expect(lines.filter((l) => l.includes("weighted speech: share 0.350 of 8"))).toHaveLength(1);
    expect(lines.filter((l) => l.includes("weighted speech: share 0.075 of 8"))).toHaveLength(6);
    expect(lines[0]).toContain("(seed=0 dominant=0.35/0.2 observers=0 keep=0.75 overlap=0.3)");
    const observed = [0, 1, 2, 3, 4, 5, 6, 7].map((i) => share(i, 6));
    expect(observed.filter((l) => l.includes("share 0.000 of 8, takes no turns"))).toHaveLength(6);
  });

  it("says an index outside the fleet NEVER speaks under either model, and no bot inside it does", () => {
    for (const plan of [SPEECH_PLAN_DEFAULT, ROTATION_PLAN]) {
      expect(speechPlanText(plan, { index: 8, fleetSize: 8 })).toBe(
        "index 8 >= fleet size 8, NEVER speaks",
      );
    }
    const inFleet = [0, 1, 2, 3, 4, 5, 6, 7].map((index) =>
      speechPlanText({ ...SPEECH_PLAN_DEFAULT, observers: 6 }, { index, fleetSize: 8 }),
    );
    expect(inFleet.filter((t) => t.includes("NEVER"))).toEqual([]);
  });
});
