import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { gunzipSync } from "node:zlib";

import { HOST_RENDER } from "../persona-driver";

export const PEER_SPEAKING = /^\[PEER_SPEAKING\] speaking=(on|off) peer=(\d+) edge_ms=(\d+)$/;
const SESSION_ASSIGNED = /^Received SESSION_ASSIGNED: session_id=(\d+)$/;
const RENAME = /^RENAME: API CALL INITIATED for: bot-(\d+) \(session_id: (?:Some\((\d+)\)|None)\)$/;
/** `<user>_<session epoch ms>_<chunk>`; the chunk is a 5-digit seq or a 16-hex beacon id. */
const CHUNK = /^(.+_\d+)_[0-9a-f]+\.(?:log\.gz|log|jsonl)$/;

export class ReportInputError extends Error {}

/** One session's console log, deduplicated by `seq`. Times are epoch ms. */
export interface SessionLog {
  /** N of its `RENAME ... bot-<N>` line; null without one. */
  ordinal: number | null;
  stem: string;
  sessionIds: string[];
  firstMs: number;
  lastMs: number;
  /** Publisher session id → receiver-clock `edge_ms` of each `speaking=on` edge. */
  onEdges: Map<string, number[]>;
  peerSpeakingLines: number;
  /** Mic state from each `Host render:` state line, by line `ts`. */
  mic: Array<{ ms: number; on: boolean }>;
  /** Camera state from the same lines; absent reads as camera off. */
  camera?: Array<{ ms: number; on: boolean }>;
}

export type BotLog = SessionLog & { ordinal: number };

interface Entry {
  seq: number;
  ts: string;
  msg: string;
}

/** `[PEER_SPEAKING]` on edges outside `[fromMs, toMs]` are dropped; every other line is read in full. */
export interface EdgeRange {
  fromMs: number;
  toMs: number;
}

export function parseSession(
  stem: string,
  lines: Iterable<string>,
  keep: EdgeRange = { fromMs: -Infinity, toMs: Infinity },
): SessionLog {
  const seen = new Set<number>();
  const ids = new Set<string>();
  const ordinals = new Set<number>();
  const onEdges = new Map<string, number[]>();
  const mic: SessionLog["mic"] = [];
  const camera: Array<{ ms: number; on: boolean }> = [];
  let firstMs = Infinity;
  let lastMs = -Infinity;
  let peerSpeakingLines = 0;
  for (const line of lines) {
    let e: Entry;
    try {
      e = JSON.parse(line) as Entry;
    } catch {
      continue;
    }
    if (typeof e?.seq !== "number" || typeof e.msg !== "string" || seen.has(e.seq)) continue;
    seen.add(e.seq);
    const ms = Date.parse(e.ts);
    if (!Number.isFinite(ms)) continue;
    firstMs = Math.min(firstMs, ms);
    lastMs = Math.max(lastMs, ms);
    let m: RegExpExecArray | null;
    if ((m = PEER_SPEAKING.exec(e.msg))) {
      peerSpeakingLines += 1;
      if (m[1] !== "on") continue;
      const edgeMs = Number(m[3]);
      if (edgeMs < keep.fromMs || edgeMs > keep.toMs) continue;
      const list = onEdges.get(m[2]) ?? [];
      list.push(edgeMs);
      onEdges.set(m[2], list);
    } else if ((m = HOST_RENDER.exec(e.msg))) {
      mic.push({ ms, on: m[3] === "true" });
      camera.push({ ms, on: m[1] === "true" });
    } else if ((m = SESSION_ASSIGNED.exec(e.msg))) {
      ids.add(m[1]);
    } else if ((m = RENAME.exec(e.msg))) {
      ordinals.add(Number(m[1]));
      if (m[2] !== undefined) ids.add(m[2]);
    }
  }
  if (ordinals.size > 1) {
    throw new ReportInputError(
      `${stem}: RENAME lines name ${[...ordinals].map((o) => `bot-${o}`).join(", ")}`,
    );
  }
  for (const list of onEdges.values()) list.sort((a, b) => a - b);
  mic.sort((a, b) => a.ms - b.ms);
  camera.sort((a, b) => a.ms - b.ms);
  const [ordinal = null] = ordinals;
  return {
    ordinal,
    stem,
    sessionIds: [...ids],
    firstMs,
    lastMs,
    onEdges,
    peerSpeakingLines,
    mic,
    camera,
  };
}

function* chunkLines(dir: string, files: readonly string[]): Generator<string> {
  for (const f of files) {
    const raw = readFileSync(join(dir, f));
    yield* (f.endsWith(".gz") ? gunzipSync(raw) : raw).toString("utf8").split("\n");
  }
}

/** Every session in `dir`, one per `<user>_<epochms>` stem. */
export function readLogDir(dir: string, keep?: EdgeRange): SessionLog[] {
  let names: string[];
  try {
    names = readdirSync(dir).sort();
  } catch (err) {
    throw new ReportInputError(`cannot read ${dir}: ${String(err)}`);
  }
  const chunks = new Map<string, string[]>();
  for (const name of names) {
    const m = CHUNK.exec(name);
    if (!m) continue;
    const files = chunks.get(m[1]);
    if (files) files.push(name);
    else chunks.set(m[1], [name]);
  }
  if (chunks.size === 0) throw new ReportInputError(`no console-log chunks in ${dir}`);
  return [...chunks].map(([stem, files]) => parseSession(stem, chunkLines(dir, files), keep));
}

/** One log per fleet ordinal, ordered by ordinal; throws on an unnamed session, or two sessions for one bot or one session id. */
export function botsByOrdinal(sessions: readonly SessionLog[]): BotLog[] {
  const unnamed = sessions.filter((s) => s.ordinal === null).map((s) => s.stem);
  if (unnamed.length > 0) {
    throw new ReportInputError(
      `no 'RENAME: API CALL INITIATED for: bot-<N>' line in: ${unnamed.join(", ")}`,
    );
  }
  const byOrdinal = new Map<number, BotLog>();
  const owner = new Map<string, number>();
  for (const s of sessions as readonly BotLog[]) {
    const other = byOrdinal.get(s.ordinal);
    if (other) {
      throw new ReportInputError(
        `bot-${s.ordinal} has two sessions (${other.stem}, ${s.stem}): more than one run in the directory? Narrow with --from/--to`,
      );
    }
    byOrdinal.set(s.ordinal, s);
    for (const id of s.sessionIds) {
      if (owner.has(id) && owner.get(id) !== s.ordinal) {
        throw new ReportInputError(
          `session id ${id} claimed by bot-${owner.get(id)} and bot-${s.ordinal}`,
        );
      }
      owner.set(id, s.ordinal);
    }
  }
  return [...byOrdinal.values()].sort((a, b) => a.ordinal - b.ordinal);
}
