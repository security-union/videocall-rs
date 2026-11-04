import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { loadClockSpeech } from "../clock-init";
import type { SpeechPlan } from "../speech-plan";
import { ReportInputError } from "./parse";

const BLOCK_MS = 10_000;
/** How long after a stretch ends a receiver's first `on` edge still counts as hearing it. */
export const HEARD_TAIL_MS = 3_000;
/** How long before a stretch's start a receiver's first `on` edge still counts, so a receiver clock behind the plan keeps its edge. */
export const HEARD_LEAD_SLACK_MS = 400;

/** A run of consecutive blocks with one speaker; `startMs` includes the first block's lead. */
export interface Stretch {
  speaker: number;
  startMs: number;
  endMs: number;
  leadMs: number;
}

/** Complete stretches with `startMs >= fromMs` and `endMs + HEARD_TAIL_MS <= toMs`. */
export function stretchesBetween(
  plan: SpeechPlan,
  fleetSize: number,
  fromMs: number,
  toMs: number,
  speechScript?: string,
): Stretch[] {
  const schedule = loadClockSpeech(plan, [], speechScript).createSchedule(plan, fleetSize);
  const runs: Stretch[] = [];
  const last = Math.ceil(toMs / BLOCK_MS) + 1;
  for (let block = Math.floor(fromMs / BLOCK_MS) - 1; block <= last; block += 1) {
    const { speaker, lead } = schedule.blockAt(block);
    const run = runs[runs.length - 1];
    if (run?.speaker === speaker) {
      run.endMs = (block + 1) * BLOCK_MS;
      continue;
    }
    const leadMs = Math.round(lead * 1000);
    runs.push({
      speaker,
      startMs: block * BLOCK_MS - leadMs,
      endMs: (block + 1) * BLOCK_MS,
      leadMs,
    });
  }
  return runs.filter((s) => s.startMs >= fromMs && s.endMs + HEARD_TAIL_MS <= toMs);
}

/** The one speech plan every participant record in `dir` carries. */
export function planFromRecords(dir: string): SpeechPlan {
  const plans = new Set<string>();
  let files: string[];
  try {
    files = readdirSync(dir).filter((f) => f.endsWith(".json"));
  } catch (err) {
    throw new ReportInputError(`cannot read ${dir}: ${String(err)}`);
  }
  for (const f of files) {
    const rec = JSON.parse(readFileSync(join(dir, f), "utf8")) as { speech?: unknown };
    if (rec.speech == null || typeof rec.speech !== "object") continue;
    const plan: Record<string, unknown> = { ...rec.speech };
    delete plan.weight;
    plans.add(JSON.stringify(plan));
  }
  if (plans.size !== 1) {
    throw new ReportInputError(
      `${dir}: participant records carry ${plans.size} distinct speech plans, need exactly 1`,
    );
  }
  return JSON.parse([...plans][0]) as SpeechPlan;
}
