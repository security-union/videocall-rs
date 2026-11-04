import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

import { type SourceGeometry } from "./posture";
import type { SpeechPlan } from "./speech-plan";

function sourceText(file: string): string {
  const path = fileURLToPath(new URL(`./${file}`, import.meta.url));
  return `${readFileSync(path, "utf8")}\n//# sourceURL=${path}`;
}

export const CLOCK_SPEECH_SCRIPT = sourceText("clock-speech.js");
export const CLOCK_SOURCE_SCRIPT = sourceText("clock-source.js");
export const CLOCK_VOICES_DIR = fileURLToPath(new URL("../assets/clock-voices", import.meta.url));

/** This bot's index in a talking-clock fleet of `fleetSize` (issue 3072). */
export interface TalkingClockSlot {
  readonly index: number;
  readonly fleetSize: number;
}

/** The slot of a bot launched without one: alone, so it speaks every turn. */
export const SOLO_CLOCK_SLOT: TalkingClockSlot = { index: 0, fleetSize: 1 };

export interface TurnPlan {
  speaker: number;
  /** Seconds the speaker opens before the block boundary; 0 unless it is an overlapped handoff. */
  lead: number;
}

export interface ClockSpeech {
  /** Longest handoff lead (ms) the schedule can draw. */
  MAX_LEAD_MS: number;
  model: SpeechPlan["model"];
  mix(n: number, salt: number): number;
  createSchedule(
    plan: SpeechPlan,
    fleetSize: number,
  ): { weights: readonly number[]; blockAt(block: number): TurnPlan };
  turnPlanFor(epochSeconds: number, fleetSize: number): TurnPlan;
  backchannelForSlot(
    slot: number,
    fleetSize: number,
  ): { start: number; listener: number; clip: string } | null;
}

export function loadClockSpeech(
  plan: SpeechPlan | null,
  mutedListeners: readonly number[] = [],
  script: string = CLOCK_SPEECH_SCRIPT,
): ClockSpeech {
  const sandbox: {
    __CLOCK_SPEECH?: ClockSpeech;
    __CLOCK_SPEECH_PLAN?: SpeechPlan;
    __CLOCK_MUTED_LISTENERS: readonly number[];
  } = { __CLOCK_MUTED_LISTENERS: mutedListeners };
  if (plan !== null) sandbox.__CLOCK_SPEECH_PLAN = plan;
  vm.runInNewContext(script, sandbox);
  if (sandbox.__CLOCK_SPEECH === undefined) throw new Error("clock-speech.js exported nothing");
  return sandbox.__CLOCK_SPEECH;
}

/** This bot's talking share under `plan`; 0 for an index outside the fleet. */
export function speechWeightFor(plan: SpeechPlan, slot: TalkingClockSlot): number {
  if (slot.index >= slot.fleetSize) return 0;
  return loadClockSpeech(plan).createSchedule(plan, slot.fleetSize).weights[slot.index];
}

export function speechPlanText(plan: SpeechPlan, slot: TalkingClockSlot): string {
  if (slot.index >= slot.fleetSize) {
    return `index ${slot.index} >= fleet size ${slot.fleetSize}, NEVER speaks`;
  }
  if (plan.model === "rotation") return `speaks turn ${slot.index} of ${slot.fleetSize}`;
  const weight = speechWeightFor(plan, slot);
  const who = weight === 0 ? ", takes no turns" : "";
  return (
    `weighted speech: share ${weight.toFixed(3)} of ${slot.fleetSize}${who} ` +
    `(seed=${plan.seed} dominant=${plan.dominant.join("/")} observers=${plan.observers} keep=${plan.keep} overlap=${plan.overlap})`
  );
}

export interface ClockVoice {
  readonly name: string;
  /** True when the participant has no voice of its own and took one by fleet index. */
  readonly fallback: boolean;
  /** Clip name (e.g. `n41`) → base64 Ogg/Opus bytes. */
  readonly clips: Readonly<Record<string, string>>;
}

export function listClockVoices(dir: string = CLOCK_VOICES_DIR): string[] {
  if (!existsSync(dir)) return [];
  return readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort();
}

export function resolveClockVoice(
  participant: string,
  fleetIndex: number,
  dir: string = CLOCK_VOICES_DIR,
): ClockVoice | null {
  const voices = listClockVoices(dir);
  if (voices.length === 0) return null;
  const own = participant.toLowerCase();
  const fallback = !voices.includes(own);
  const name = fallback ? voices[fleetIndex % voices.length] : own;
  const clips: Record<string, string> = {};
  for (const file of readdirSync(join(dir, name)).sort()) {
    if (!file.endsWith(".ogg")) continue;
    clips[file.slice(0, -".ogg".length)] = readFileSync(join(dir, name, file)).toString("base64");
  }
  return { name, fallback, clips };
}

export function buildClockInitScript(params: {
  displayName: string;
  sourceGeometry: SourceGeometry;
  videoNoise: number;
  slot: TalkingClockSlot;
  voice: ClockVoice | null;
  speechPlan: SpeechPlan;
  mutedListeners?: readonly number[];
}): string {
  return (
    `globalThis.__CLOCK_PARTICIPANT = ${JSON.stringify(params.displayName)};\n` +
    `globalThis.__CLOCK_WIDTH = ${params.sourceGeometry.width};\n` +
    `globalThis.__CLOCK_HEIGHT = ${params.sourceGeometry.height};\n` +
    `globalThis.__CLOCK_VIDEO_NOISE = ${params.videoNoise};\n` +
    `globalThis.__CLOCK_FLEET_INDEX = ${params.slot.index};\n` +
    `globalThis.__CLOCK_FLEET_SIZE = ${params.slot.fleetSize};\n` +
    `globalThis.__CLOCK_VOICE_CLIPS = ${JSON.stringify(params.voice?.clips ?? null)};\n` +
    `globalThis.__CLOCK_SPEECH_PLAN = ${JSON.stringify(params.speechPlan)};\n` +
    `globalThis.__CLOCK_MUTED_LISTENERS = ${JSON.stringify(params.mutedListeners ?? [])};\n` +
    `${CLOCK_SPEECH_SCRIPT}\n${CLOCK_SOURCE_SCRIPT}`
  );
}

/** One greppable run-output line naming what this bot will say, or that it never will. */
export function talkingClockLine(
  slot: TalkingClockSlot,
  voice: ClockVoice | null,
  plan: SpeechPlan,
): string {
  if (voice === null) return "talking clock: NO voice clips found — silent mic, never speaks";
  const name = voice.fallback
    ? `${voice.name} (fallback: no voice for this participant)`
    : voice.name;
  return `talking clock: voice=${name}, ${speechPlanText(plan, slot)}`;
}

/** The page's `[talking-clock] <status>` console line, minus its prefix; null for any other line. */
export function parseTalkingClockLine(text: string): string | null {
  return text.startsWith("[talking-clock] ") ? text.slice("[talking-clock] ".length) : null;
}
