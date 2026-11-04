import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { loadClockSpeech } from "../../clock-init";
import { SPEECH_PLAN_DEFAULT, type SpeechPlan } from "../../speech-plan";

/** A run whose speech edges follow the real planner; times are epoch ms. */
export const T0 = Date.parse("2026-10-09T10:00:00Z");
export const T1 = T0 + 15 * 60_000;
export const ONSET_MS = 220;
export const DEFAULT_FLEET = 8;
export const sessionId = (bot: number): string => String(9_000_000_000_000_000_000n + BigInt(bot));
const iso = (ms: number): string => new Date(ms).toISOString();

export interface PlannedStretch {
  speaker: number;
  startMs: number;
  endMs: number;
}

/** Maximal same-speaker runs, walked from `blockAt` directly. */
export function plannedStretches(
  fleet: number,
  plan: SpeechPlan = SPEECH_PLAN_DEFAULT,
): PlannedStretch[] {
  const sched = loadClockSpeech(plan).createSchedule(plan, fleet);
  const out: PlannedStretch[] = [];
  for (let b = T0 / 10_000 - 10; b < T1 / 10_000 + 10; b += 1) {
    const { speaker, lead } = sched.blockAt(b);
    const last = out[out.length - 1];
    if (last?.speaker === speaker) last.endMs = (b + 1) * 10_000;
    else
      out.push({ speaker, startMs: b * 10_000 - Math.round(lead * 1000), endMs: (b + 1) * 10_000 });
  }
  return out;
}

export interface SynthOptions {
  fleet?: number;
  plan?: SpeechPlan;
  bots?: number[];
  /** Receiver `on`-edge time for one stretch; null = no edge. Default: start + ONSET_MS. */
  edgeAt?: (receiver: number, s: PlannedStretch) => number | null;
  /** Extra `Host render:` mic states per bot, as [ms, on]. */
  mic?: Record<number, Array<[number, boolean]>>;
  /** Per bot, drop every line after this time. */
  until?: Record<number, number>;
  /** Per bot, the time its first line is written; earlier lines are dropped. Default T0. */
  joinAt?: Record<number, number>;
  /** Per bot, the time it is assigned a second session id, used for its stretches from then on. */
  reconnect?: Record<number, number>;
  /** Bots whose log has no `Host render:` line. */
  noHostRender?: number[];
}

/** Console-log lines per bot, in the uploader's JSON-lines shape. */
export function synthLogs(opts: SynthOptions = {}): Map<number, string[]> {
  const fleet = opts.fleet ?? DEFAULT_FLEET;
  const bots = opts.bots ?? Array.from({ length: fleet }, (_, i) => i);
  const stretches = plannedStretches(fleet, opts.plan);
  const logs = new Map<number, Array<{ ms: number; msg: string }>>();
  for (const bot of bots) {
    const j = opts.joinAt?.[bot] ?? T0;
    const idAt = (speaker: number, ms: number): string => {
      const x = opts.reconnect?.[speaker];
      return x !== undefined && ms >= x ? `${sessionId(speaker)}7` : sessionId(speaker);
    };
    const rows = [
      { ms: j, msg: `Received SESSION_ASSIGNED: session_id=${sessionId(bot)}` },
      {
        ms: j + 1,
        msg: `RENAME: API CALL INITIATED for: bot-${bot} (session_id: Some(${sessionId(bot)}))`,
      },
      ...(opts.noHostRender?.includes(bot)
        ? []
        : [
            {
              ms: j + 2,
              msg: "Host render: video=false prev=false mic=true prev=false screen=false prev=false",
            },
          ]),
      { ms: T1, msg: "end" },
    ];
    const second = opts.reconnect?.[bot];
    if (second !== undefined) {
      rows.push({ ms: second, msg: `Received SESSION_ASSIGNED: session_id=${sessionId(bot)}7` });
    }
    for (const [ms, on] of opts.mic?.[bot] ?? []) {
      rows.push({
        ms,
        msg: `Host render: video=true prev=true mic=${on} prev=${!on} screen=false prev=false`,
      });
    }
    for (const s of stretches) {
      if (s.speaker === bot || s.startMs < T0 + 10_000 || s.endMs > T1 - 10_000) continue;
      const at = opts.edgeAt ? opts.edgeAt(bot, s) : s.startMs + ONSET_MS;
      if (at === null) continue;
      for (const [state, ms] of [
        ["on", at],
        ["off", Math.max(at, s.endMs) + 300],
      ] as const) {
        rows.push({
          ms,
          msg: `[PEER_SPEAKING] speaking=${state} peer=${idAt(s.speaker, s.startMs)} edge_ms=${ms}`,
        });
      }
    }
    logs.set(bot, rows);
  }
  return new Map(
    [...logs].map(([bot, rows]) => [
      bot,
      rows
        .filter(
          (r) =>
            r.ms >= (opts.joinAt?.[bot] ?? -Infinity) && r.ms <= (opts.until?.[bot] ?? Infinity),
        )
        .sort((a, b) => a.ms - b.ms)
        .map((r, seq) => JSON.stringify({ seq, ts: iso(r.ms), level: "info", msg: r.msg })),
    ]),
  );
}

const made: string[] = [];

/** A temp directory that `cleanTempDirs` removes. */
export function tempDir(prefix: string): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  made.push(dir);
  return dir;
}

export function cleanTempDirs(): void {
  for (const dir of made.splice(0)) rmSync(dir, { recursive: true, force: true });
}

/** Writes `synthLogs` as uncompressed chunks; returns the directory. */
export function writeSynthDir(opts: SynthOptions = {}): string {
  const dir = tempDir("delivery-");
  for (const [bot, lines] of synthLogs(opts)) {
    writeFileSync(join(dir, `bot-${bot}_${T0 + bot}_00001.log`), lines.join("\n") + "\n");
  }
  return dir;
}
