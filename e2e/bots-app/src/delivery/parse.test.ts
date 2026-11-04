import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

import { afterAll, describe, expect, it } from "vitest";

import { edgeRange, runReport } from "./command";
import { cleanTempDirs, synthLogs, T0, T1, tempDir } from "./fixtures/synthetic";
import { botsByOrdinal, parseSession, readLogDir, type SessionLog } from "./parse";

afterAll(cleanTempDirs);

const line = (seq: number, msg: string, ts = "2026-10-09T17:00:00Z"): string =>
  JSON.stringify({ seq, ts, msg });
const rename = (bot: number, id: string): string =>
  `RENAME: API CALL INITIATED for: bot-${bot} (session_id: Some(${id}))`;
const on = (peer: string, ms: number): string =>
  `[PEER_SPEAKING] speaking=on peer=${peer} edge_ms=${ms}`;

describe("console-log parsing", () => {
  it("dedupes overlapping gzip chunks by seq and groups them by session stem", () => {
    const dir = tempDir("chunks-");
    const a = [line(0, rename(2, "7")), line(1, on("9", 100)), line(2, on("9", 200))];
    const b = [line(2, on("9", 200)), line(3, on("9", 300)), "{truncated"];
    writeFileSync(join(dir, "bot-2_1700_00001.log.gz"), gzipSync(a.join("\n")));
    writeFileSync(join(dir, "bot-2_1700_00002.log.gz"), gzipSync(b.join("\n")));
    writeFileSync(join(dir, "notes.txt"), "ignored");
    const [log, ...rest] = readLogDir(dir);
    expect(rest).toEqual([]);
    expect(log.ordinal).toBe(2);
    expect(log.onEdges.get("9")).toEqual([100, 200, 300]);
  });

  it("takes session ids from both SESSION_ASSIGNED and RENAME, and the mic and camera state from Host render", () => {
    const log = parseSession("s", [
      line(0, "Received SESSION_ASSIGNED: session_id=11"),
      line(1, "SESSION_ASSIGNED received on connection ws_0: 99"),
      line(2, rename(4, "12")),
      line(3, "Host render: video=true prev=false mic=false prev=true screen=false prev=false"),
      line(4, "Host render: camera ON, auto-select device_id='x'"),
    ]);
    expect(log.sessionIds.sort()).toEqual(["11", "12"]);
    expect(log.mic.map((m) => m.on)).toEqual([false]);
    expect(log.camera?.map((c) => c.on)).toEqual([true]);
  });

  it("names every session with no bot-<N> RENAME line", () => {
    const unnamed = (stem: string) => parseSession(stem, [line(0, on("9", 1))]);
    const named = parseSession("b_1", [line(0, rename(1, "5"))]);
    expect(() => botsByOrdinal([unnamed("alice_1700"), named, unnamed("eve_1800")])).toThrow(
      /line in: alice_1700, eve_1800$/,
    );
    expect(() => parseSession("x", [line(0, rename(1, "5")), line(1, rename(2, "5"))])).toThrow(
      /x: RENAME lines name bot-1, bot-2/,
    );
  });

  it("refuses two sessions for one ordinal or one session id for two bots", () => {
    const s = (stem: string, bot: number, id: string) =>
      parseSession(stem, [line(0, rename(bot, id))]);
    expect(() => botsByOrdinal([s("a", 1, "5"), s("b", 1, "6")])).toThrow(
      /bot-1 has two sessions \(a, b\)/,
    );
    expect(() => botsByOrdinal([s("a", 1, "5"), s("b", 2, "5")])).toThrow(
      /claimed by bot-1 and bot-2/,
    );
  });

  it("keeps on edges from 0.4 s before --from to --to, and reads every other line in full", () => {
    const [from, to] = [10_000, 20_000];
    const lines = [
      line(0, rename(2, "7"), "2026-10-09T17:00:00Z"),
      ...[from - 401, from - 400, to, to + 1].map((ms, i) => line(1 + i, on("9", ms))),
      line(5, "Host render: video=true prev=false mic=true prev=false screen=false prev=false"),
      line(6, "end", "2026-10-09T18:00:00Z"),
    ];
    const all = parseSession("s", lines);
    const kept = parseSession("s", lines, edgeRange(from, to));
    expect(kept.onEdges.get("9")).toEqual([from - 400, to]);
    const facts = (l: SessionLog) => [
      l.ordinal,
      l.sessionIds,
      l.firstMs,
      l.lastMs,
      l.mic,
      l.peerSpeakingLines,
    ];
    expect(facts(kept)).toEqual(facts(all));
    expect([kept.peerSpeakingLines, kept.mic.length, kept.ordinal]).toEqual([4, 1, 2]);
  });

  it("--from/--to picks one run out of a directory holding two", () => {
    const dir = tempDir("two-runs-");
    const later = 60 * 60_000;
    for (const [bot, lines] of synthLogs()) {
      writeFileSync(join(dir, `bot-${bot}_${T0}_00001.log`), lines.join("\n"));
      const shifted = lines.map((l) => {
        const j = JSON.parse(l) as { ts: string };
        j.ts = new Date(Date.parse(j.ts) + later).toISOString();
        return JSON.stringify(j);
      });
      writeFileSync(join(dir, `bot-${bot}_${T0 + later}_00001.log`), shifted.join("\n"));
    }
    expect(() => runReport({ logs: dir, fleetSize: "8", lateMs: "1000" }, () => {})).toThrow(
      /has two sessions/,
    );
    const iso = (ms: number): string => new Date(ms).toISOString();
    const code = runReport(
      { logs: dir, fleetSize: "8", lateMs: "1000", from: iso(T0), to: iso(T1) },
      () => {},
    );
    expect(code).toBe(0);
    const second = runReport(
      { logs: dir, fleetSize: "8", lateMs: "1000", from: iso(T0 + later), to: iso(T1 + later) },
      () => {},
    );
    expect(second).toBe(0);
  });
});
