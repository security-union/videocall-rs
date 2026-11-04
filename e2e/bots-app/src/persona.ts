import { loadClockSpeech, type TalkingClockSlot, type TurnPlan } from "./clock-init";
import type { Resolved } from "./run-record";
import type { SpeechPlan } from "./speech-plan";

export const PERSONA_ENV = {
  personas: "BOT_PERSONAS",
  camera: "BOT_CAMERA",
  micOn: "BOT_PERSONA_MIC_ON",
  cameraOn: "BOT_PERSONA_CAMERA_ON",
} as const;

export type MicPersona = "open" | "push-to-talk" | "observer";
export type CameraPersona = "always" | "with-voice" | "never";

/** Room-size rows of {@link MIC_ON_DEFAULT} and {@link CAMERA_ON_DEFAULT}. */
export const SIZE_ROWS = ["2-3", "4-5", "6-7", "8-9", "10+"] as const;
export const MIC_ON_DEFAULT = [0.8, 0.65, 0.37, 0.25, 0.11];
export const CAMERA_ON_DEFAULT = [0.53, 0.48, 0.14, 0.01, 0.08];
export const OPEN_MIC_MAX_ROOM = 6;

export const MIN_TOGGLE_GAP_MS = 1_000;
/** Camera counts within this of the target share compete on fewest toggles. */
export const CAMERA_SHARE_SLACK = 0.15;
export const UNMUTE = { medianS: 0.5, maxS: 3, pLate: 0.15 };
export const MUTE = { medianS: 3.2, maxS: 10, pCut: 0.03 };
export const CAMERA_OFFSET = { meanS: -0.6, iqrS: 4.8, maxAbsS: 10 };
export const DELAY_P90_RATIO = 3;
const LOG_SIGMA = Math.log(DELAY_P90_RATIO) / 1.2816;
const IQR_PER_SIGMA = 1.349;
const TURN_MS = 10_000;
/** Blocks either side of `t` whose stretches can reach `t` after the widest draws above. */
const REACH_BLOCKS = 4;
const STRETCH_WALK_CAP = 360;
/** Blocks whose turn plan the planner keeps; the schedule itself caches only a few hours. */
const PLAN_MEMO_BLOCKS = 10_000;
/** Blocks the count choice simulates from epoch block 0, the same in every pod. */
const DUTY_BLOCKS = 1_440;
const SALT = {
  mic: 0x3a1c5,
  camera: 0xca3e7,
  late: 0x1a7e,
  unmute: 0x0b1e,
  cut: 0xc07,
  mute: 0x3b7e,
  camOn: 0xc0a,
  camOff: 0xc0f,
};

export interface PersonaSettings {
  readonly cameraAlwaysOn: boolean;
  readonly micOn: readonly number[];
  readonly cameraOn: readonly number[];
}

function shares(name: string, raw: string, fallback: readonly number[]): number[] | string {
  if (raw === "") return [...fallback];
  const tokens = raw.split(",").map((t) => t.trim());
  const values = tokens.map((t) =>
    /^(?:0(?:\.\d{1,4})?|1(?:\.0{1,4})?|\.\d{1,4})$/.test(t) ? Number(t) : NaN,
  );
  if (tokens.length !== SIZE_ROWS.length || values.some(Number.isNaN)) {
    return `${name} takes ${SIZE_ROWS.length} shares in 0..1 for room sizes ${SIZE_ROWS.join(" / ")}, got "${raw}"`;
  }
  return values;
}

/** `null` = personas off. */
export function resolvePersonaSettings(
  flag: boolean | undefined,
  env: Record<string, string | undefined>,
): Resolved<PersonaSettings | null> {
  const get = (k: keyof typeof PERSONA_ENV): string => (env[PERSONA_ENV[k]] ?? "").trim();
  const invalid = (message: string): Resolved<PersonaSettings | null> => ({
    kind: "invalid",
    message,
  });
  const toggle = get("personas").toLowerCase();
  if (!["", "on", "off"].includes(toggle)) {
    return invalid(`${PERSONA_ENV.personas} must be "on" or "off", got "${get("personas")}"`);
  }
  const camera = get("camera").toLowerCase();
  if (camera !== "" && camera !== "always-on") {
    return invalid(`${PERSONA_ENV.camera} must be "always-on" or unset, got "${get("camera")}"`);
  }
  if (!(flag === true || toggle === "on")) {
    const set = (["camera", "micOn", "cameraOn"] as const).filter((k) => get(k) !== "");
    if (set.length === 0) return { kind: "ok", value: null };
    const names = set.map((k) => PERSONA_ENV[k]).join(", ");
    return invalid(
      `${names} has no effect without --personas / ${PERSONA_ENV.personas}=on; unset it`,
    );
  }
  const micOn = shares(PERSONA_ENV.micOn, get("micOn"), MIC_ON_DEFAULT);
  if (typeof micOn === "string") return invalid(micOn);
  const cameraOn = shares(PERSONA_ENV.cameraOn, get("cameraOn"), CAMERA_ON_DEFAULT);
  if (typeof cameraOn === "string") return invalid(cameraOn);
  return { kind: "ok", value: { cameraAlwaysOn: camera === "always-on", micOn, cameraOn } };
}

export function sizeRow(fleetSize: number): number {
  const row = [3, 5, 7, 9].findIndex((max) => fleetSize <= max);
  return row === -1 ? SIZE_ROWS.length - 1 : row;
}

export interface Interval {
  /** Epoch ms. */
  on: number;
  off: number;
}

export interface VoicePlan {
  mic: Interval[];
  camera: Interval[];
}

export interface PersonaPlanner {
  readonly weights: readonly number[];
  readonly mix: (n: number, salt: number) => number;
  readonly seed: number;
  /** A push-to-talk mic's and a with-voice camera's on-intervals from bot `index`'s stretches in blocks `from..to`. */
  voiceIntervals(index: number, from: number, to: number): VoicePlan;
}

function merge(list: Interval[]): Interval[] {
  const out: Interval[] = [];
  for (const iv of [...list].sort((a, b) => a.on - b.on)) {
    const last = out[out.length - 1];
    if (last !== undefined && iv.on < last.off + MIN_TOGGLE_GAP_MS)
      last.off = Math.max(last.off, iv.off);
    else out.push({ ...iv });
  }
  return out;
}

export function createPersonaPlanner(plan: SpeechPlan, fleetSize: number): PersonaPlanner {
  const speech = loadClockSpeech(plan);
  const schedule = speech.createSchedule(plan, fleetSize);
  const seed = plan.model === "weighted" ? plan.seed : 0;
  const memo = new Map<number, TurnPlan>();
  const blockPlan = (block: number): TurnPlan => {
    let turn = memo.get(block);
    if (turn === undefined) {
      if (memo.size >= PLAN_MEMO_BLOCKS) memo.clear();
      turn = schedule.blockAt(block);
      memo.set(block, turn);
    }
    return turn;
  };
  const speaker = (block: number): number => blockPlan(block).speaker;
  const unitDraw = (index: number, block: number, salt: number, k = 0): number =>
    (speech.mix(block, speech.mix(index * 2 + k, speech.mix(seed, salt))) + 1) / 4_294_967_297;
  const normal = (index: number, block: number, salt: number): number =>
    Math.sqrt(-2 * Math.log(unitDraw(index, block, salt, 0))) *
    Math.cos(2 * Math.PI * unitDraw(index, block, salt, 1));
  const delayMs = (index: number, block: number, salt: number, medianS: number, maxS: number) =>
    Math.min(maxS, medianS * Math.exp(LOG_SIGMA * normal(index, block, salt))) * 1_000;
  const cameraOffsetMs = (index: number, block: number, salt: number): number => {
    const s =
      CAMERA_OFFSET.meanS + (CAMERA_OFFSET.iqrS / IQR_PER_SIGMA) * normal(index, block, salt);
    return Math.max(-CAMERA_OFFSET.maxAbsS, Math.min(CAMERA_OFFSET.maxAbsS, s)) * 1_000;
  };

  function voiceIntervals(index: number, from: number, to: number): VoicePlan {
    const mic: Interval[] = [];
    const camera: Interval[] = [];
    for (let b = from; b <= to; b += 1) {
      if (speaker(b) !== index) continue;
      let first = b;
      while (first > b - STRETCH_WALK_CAP && speaker(first - 1) === index) first -= 1;
      let last = b;
      while (last < b + STRETCH_WALK_CAP && speaker(last + 1) === index) last += 1;
      b = last;
      const start = first * TURN_MS - blockPlan(first).lead * 1_000;
      const end = (last + 1) * TURN_MS;
      const lead = delayMs(index, first, SALT.unmute, UNMUTE.medianS, UNMUTE.maxS);
      const tail = delayMs(index, first, SALT.mute, MUTE.medianS, MUTE.maxS);
      const on = unitDraw(index, first, SALT.late) < UNMUTE.pLate ? start + lead : start - lead;
      const offAt = unitDraw(index, first, SALT.cut) < MUTE.pCut ? end - tail : end + tail;
      const off = Math.max(offAt, on + MIN_TOGGLE_GAP_MS);
      mic.push({ on, off });
      const camOn = on + cameraOffsetMs(index, first, SALT.camOn);
      const camOff = Math.max(
        off + cameraOffsetMs(index, first, SALT.camOff),
        camOn + MIN_TOGGLE_GAP_MS,
      );
      camera.push({ on: camOn, off: camOff });
    }
    return { mic: merge(mic), camera: merge(camera) };
  }

  return { weights: schedule.weights, mix: speech.mix, seed, voiceIntervals };
}

export interface ControlTarget {
  on: boolean;
  /** Epoch ms of the latest planned edge within reach of the lookup; `-Infinity` when none is. */
  since: number;
}

export function stateAt(intervals: readonly Interval[], tMs: number): ControlTarget {
  let since = -Infinity;
  for (const iv of intervals) {
    if (iv.on <= tMs && tMs < iv.off) return { on: true, since: iv.on };
    if (iv.off <= tMs) since = Math.max(since, iv.off);
  }
  return { on: false, since };
}

export interface RoomPersonas {
  readonly fleetSize: number;
  readonly mic: readonly MicPersona[];
  readonly camera: readonly CameraPersona[];
  readonly counts: Readonly<Record<MicPersona | CameraPersona, number>>;
  /** Expected room shares from the planned duty cycles; `target` is the configured row. */
  readonly expected: {
    micOn: number;
    cameraOn: number;
    micTogglesPerMin: number;
    cameraTogglesPerMin: number;
  };
  readonly target: { micOn: number; cameraOn: number };
}

function shuffled(planner: PersonaPlanner, n: number, salt: number): number[] {
  const order = Array.from({ length: n }, (_, i) => i);
  for (let i = n - 1; i > 0; i -= 1) {
    const j = planner.mix(i, planner.mix(planner.seed, salt)) % (i + 1);
    [order[i], order[j]] = [order[j], order[i]];
  }
  return order;
}

const sum = (xs: readonly number[]): number => xs.reduce((a, b) => a + b, 0);

function duty(list: readonly Interval[], from: number, to: number): number {
  return (
    sum(list.map((iv) => Math.max(0, Math.min(iv.off, to) - Math.max(iv.on, from)))) / (to - from)
  );
}

const TARGET_PRECISION = 0.005;

/** Closest to `target`; a later candidate wins only when it is more than TARGET_PRECISION closer. */
function closest<T>(candidates: readonly T[], share: (c: T) => number, target: number): T {
  let best = candidates[0];
  for (const c of candidates) {
    if (Math.abs(share(c) - target) < Math.abs(share(best) - target) - TARGET_PRECISION) best = c;
  }
  return best;
}

/** Every bot computes the same assignment from the plan, N and the settings alone. */
export function assignPersonas(
  planner: PersonaPlanner,
  fleetSize: number,
  settings: PersonaSettings,
): RoomPersonas {
  const n = fleetSize;
  const row = sizeRow(n);
  const target = { micOn: settings.micOn[row], cameraOn: settings.cameraOn[row] };
  const [from, to] = [0, DUTY_BLOCKS * TURN_MS];
  const plans = Array.from({ length: n }, (_, i) => planner.voiceIntervals(i, -1, DUTY_BLOCKS));
  const micDuty = plans.map((p) => duty(p.mic, from, to));
  const camDuty = plans.map((p) => duty(p.camera, from, to));

  const talkers = shuffled(planner, n, SALT.mic).filter((i) => planner.weights[i] > 0);
  const openChoices = n <= OPEN_MIC_MAX_ROOM && talkers.length > 0 ? [0, 1] : [0];
  const micShare = (open: number): number =>
    (open + sum(talkers.slice(open).map((i) => micDuty[i]))) / n;
  const open = closest(openChoices, micShare, target.micOn);
  const mic: MicPersona[] = new Array(n).fill("observer");
  talkers.forEach((i, k) => (mic[i] = k < open ? "open" : "push-to-talk"));

  const order = shuffled(planner, n, SALT.camera);
  const pairs: Array<[number, number]> = [];
  for (let v = 0; v <= n; v += 1) for (let a = 0; a <= n - v; a += 1) pairs.push([a, v]);
  const camShare = ([a, v]: [number, number]): number =>
    (a + sum(order.slice(a, a + v).map((i) => camDuty[i]))) / n;
  const toggles = plans.map((p) => p.camera.filter((iv) => iv.on >= from && iv.off <= to).length);
  const camToggles = ([a, v]: [number, number]): number =>
    sum(order.slice(a, a + v).map((i) => toggles[i]));
  const error = (c: [number, number]): number => Math.abs(camShare(c) - target.cameraOn);
  const slack = pairs.filter((c) => error(c) <= CAMERA_SHARE_SLACK);
  const fewest = slack.reduce<[number, number] | null>((best, c) => {
    if (best === null) return c;
    const d = camToggles(c) - camToggles(best);
    return d < 0 || (d === 0 && error(c) < error(best)) ? c : best;
  }, null);
  const [always, voice] = settings.cameraAlwaysOn
    ? [n, 0]
    : (fewest ?? closest(pairs, camShare, target.cameraOn));
  const camera: CameraPersona[] = new Array(n).fill("never");
  order.forEach(
    (i, k) => (camera[i] = k < always ? "always" : k < always + voice ? "with-voice" : "never"),
  );

  const togglesPerMin = (lists: Interval[][]): number =>
    (sum(lists.map((l) => l.filter((iv) => iv.on >= from && iv.off <= to).length)) * 2) /
    ((to - from) / 60_000) /
    n;
  const count = <T extends string>(xs: readonly T[], v: T): number =>
    xs.filter((x) => x === v).length;
  return {
    fleetSize: n,
    mic,
    camera,
    counts: {
      open: count(mic, "open"),
      "push-to-talk": count(mic, "push-to-talk"),
      observer: count(mic, "observer"),
      always: count(camera, "always"),
      "with-voice": count(camera, "with-voice"),
      never: count(camera, "never"),
    },
    expected: {
      micOn: micShare(open),
      cameraOn: camShare([always, voice]),
      micTogglesPerMin: togglesPerMin(
        plans.filter((_, i) => mic[i] === "push-to-talk").map((p) => p.mic),
      ),
      cameraTogglesPerMin: togglesPerMin(
        plans.filter((_, i) => camera[i] === "with-voice").map((p) => p.camera),
      ),
    },
    target,
  };
}

export interface BotPersona {
  readonly index: number;
  readonly mic: MicPersona;
  readonly camera: CameraPersona;
  readonly room: RoomPersonas;
}

/** Desired mic and camera state of one bot at `tMs`. */
export function personaTargets(
  planner: PersonaPlanner,
  persona: BotPersona,
  tMs: number,
): { mic: ControlTarget; camera: ControlTarget } {
  const block = Math.floor(tMs / TURN_MS);
  const voice = planner.voiceIntervals(persona.index, block - REACH_BLOCKS, block + REACH_BLOCKS);
  const fixed = (on: boolean): ControlTarget => ({ on, since: -Infinity });
  return {
    mic: persona.mic === "push-to-talk" ? stateAt(voice.mic, tMs) : fixed(persona.mic === "open"),
    camera:
      persona.camera === "with-voice"
        ? stateAt(voice.camera, tMs)
        : fixed(persona.camera === "always"),
  };
}

export function personaFor(room: RoomPersonas, slot: TalkingClockSlot): BotPersona {
  return { index: slot.index, mic: room.mic[slot.index], camera: room.camera[slot.index], room };
}

/** Fleet indices whose backchannels the page drops: every bot that is not an open mic. */
export function mutedListeners(room: RoomPersonas): number[] {
  return room.mic.flatMap((m, i) => (m === "open" ? [] : [i]));
}

export function personaRoomLine(room: RoomPersonas): string {
  const c = room.counts;
  const f = (x: number): string => x.toFixed(2);
  return (
    `personas: room of ${room.fleetSize}: mic open=${c.open} push-to-talk=${c["push-to-talk"]} observer=${c.observer}, ` +
    `camera always=${c.always} with-voice=${c["with-voice"]} never=${c.never}; ` +
    `expected mic-on ${f(room.expected.micOn)} (target ${f(room.target.micOn)}), ` +
    `camera-on ${f(room.expected.cameraOn)} (target ${f(room.target.cameraOn)}); ` +
    `toggles per bot-minute mic ${f(room.expected.micTogglesPerMin)}, camera ${f(room.expected.cameraTogglesPerMin)}`
  );
}

/** Shortfalls of more than 0.05 below target, which under-represent publish load. */
export function personaShortfalls(room: RoomPersonas): string[] {
  const gaps: string[] = [];
  for (const [what, kind] of [
    ["mic-on", "audio"],
    ["camera-on", "video"],
  ] as const) {
    const key = what === "mic-on" ? "micOn" : "cameraOn";
    if (room.expected[key] < room.target[key] - 0.05) {
      gaps.push(
        `expected ${what} share ${room.expected[key].toFixed(2)} is below the target ${room.target[key].toFixed(2)} for a room of ${room.fleetSize}: this run under-represents ${kind} publish load`,
      );
    }
  }
  return gaps;
}
