import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

import { afterAll, describe, expect, it } from "vitest";

import { ROTATION_PLAN, SPEECH_PLAN_DEFAULT } from "../speech-plan";
import {
  analyze,
  type DeliveryReport,
  END_MARGIN_MS,
  JOIN_SETTLE_MS,
  MIN_RECEIVERS_FOR_LATE,
  MUTED_OFF_MS,
  type StretchResult,
} from "./analyze";
import { runReport, type ReportOptions } from "./command";
import {
  cleanTempDirs,
  DEFAULT_FLEET,
  ONSET_MS,
  plannedStretches,
  type PlannedStretch,
  T0,
  T1,
  tempDir,
  type SynthOptions,
  writeSynthDir,
} from "./fixtures/synthetic";
import { type BotLog, ReportInputError } from "./parse";
import { HEARD_LEAD_SLACK_MS, HEARD_TAIL_MS } from "./schedule";

afterAll(cleanTempDirs);

const MIN = 60_000;
const iso = (ms: number): string => new Date(ms).toISOString();

function report(
  synth: SynthOptions = {},
  extra: Partial<ReportOptions> = {},
): { code: number; r: DeliveryReport } {
  let text = "";
  const code = runReport(
    {
      logs: writeSynthDir(synth),
      fleetSize: String(synth.fleet ?? DEFAULT_FLEET),
      lateMs: "1000",
      json: true,
      ...extra,
    },
    (t) => (text = t),
  );
  return { code, r: JSON.parse(text) as DeliveryReport };
}

const scored = (r: DeliveryReport): StretchResult[] =>
  r.stretches.filter((s) => s.status === "scored");
const flagged = (s: StretchResult, flag: string): number[] =>
  s.receivers.filter((x) => x.flag === flag).map((x) => x.receiver);

describe("delivery report on planner-driven synthetic logs", () => {
  it("scores every complete planned stretch in the default window, all on time, with no flags", () => {
    const { code, r } = report();
    const from = T0 + JOIN_SETTLE_MS;
    const to = T1 - END_MARGIN_MS;
    expect([r.fromMs, r.toMs]).toEqual([from, to]);
    const expected = plannedStretches(8).filter(
      (s) => s.startMs >= from && s.endMs + HEARD_TAIL_MS <= to,
    );
    expect(r.stretches.map((s) => [s.speaker, s.startMs, s.endMs])).toEqual(
      expected.map((s) => [s.speaker, s.startMs, s.endMs]),
    );
    // Onsets land before the block boundary on these, so they count only because the lead is in the window.
    expect(r.stretches.some((s) => s.leadMs > ONSET_MS)).toBe(true);
    expect(r.flags).toBe(0);
    expect(code).toBe(0);
    expect(r.receivers.every((x) => x.heard === x.scored && x.scored > 0)).toBe(true);
  });

  it("counts an on edge up to 400 ms before the stretch start, and not 1 ms earlier", () => {
    const { r } = report({
      edgeAt: (rx, s) => s.startMs + (rx === 3 ? -400 : rx === 4 ? -401 : ONSET_MS),
    });
    expect(scored(r).length).toBeGreaterThan(5);
    for (const s of scored(r)) expect(flagged(s, "MISSED")).toEqual(s.speaker === 4 ? [] : [4]);
  });

  it("does not flag a receiver whose clock runs 300 ms behind the plan", () => {
    const { r } = report({ edgeAt: (rx, s) => s.startMs + ONSET_MS - (rx === 3 ? 300 : 0) });
    expect(r.flags).toBe(0);
  });

  it("flags a receiver delayed 2 s from some stretch onward on every later stretch and on none before", () => {
    const X = T0 + 7 * MIN;
    const { r } = report({
      edgeAt: (rx, s) => s.startMs + ONSET_MS + (rx === 3 && s.startMs >= X ? 2_000 : 0),
    });
    const after = scored(r).filter((s) => s.startMs >= X && s.speaker !== 3);
    expect(after.length).toBeGreaterThan(5);
    expect(scored(r).some((s) => s.startMs < X && s.speaker !== 3)).toBe(true);
    for (const s of scored(r)) {
      expect(flagged(s, "LATE_RECEIVER")).toEqual(after.includes(s) ? [3] : []);
      expect(s.latePublisher).toBe(false);
    }
    expect(r.receivers.find((x) => x.receiver === 3)?.lateReceiver).toBe(after.length);
  });

  it("flags a publisher seen 2 s late by every receiver as LATE_PUBLISHER, not LATE_RECEIVER", () => {
    const { r } = report({
      edgeAt: (_rx, s) => s.startMs + ONSET_MS + (s.speaker === 1 ? 2_000 : 0),
    });
    expect(scored(r).filter((s) => s.speaker === 1).length).toBeGreaterThan(0);
    for (const s of scored(r)) {
      expect(s.latePublisher).toBe(s.speaker === 1);
      expect(flagged(s, "LATE_RECEIVER")).toEqual([]);
    }
  });

  it("marks a receiver with no on edge MISSED", () => {
    const { r } = report({
      edgeAt: (rx, s) => (rx === 5 && s.speaker === 2 ? null : s.startMs + ONSET_MS),
    });
    expect(scored(r).filter((s) => s.speaker === 2).length).toBeGreaterThan(0);
    for (const s of scored(r)) expect(flagged(s, "MISSED")).toEqual(s.speaker === 2 ? [5] : []);
  });

  it("lists a muted publisher's stretches as not scored, never MISSED", () => {
    const M1 = T0 + 3 * MIN + 5_000;
    const M2 = T0 + 4 * MIN + 5_000;
    const muted = (s: PlannedStretch): boolean =>
      s.speaker === 2 && Math.min(s.endMs, M2) - Math.max(s.startMs, M1) > 3_000;
    const { r } = report({
      mic: {
        2: [
          [M1, false],
          [M2, true],
        ],
      },
      edgeAt: (_rx, s) => (muted(s) ? null : s.startMs + ONSET_MS),
    });
    const listed = r.stretches.filter((s) => s.status === "publisher-muted");
    expect(listed.length).toBeGreaterThan(0);
    expect(listed).toEqual(r.stretches.filter(muted));
    expect(scored(r).some((s) => s.speaker === 2)).toBe(true);
    expect(r.flags).toBe(0);
  });

  it("treats a publisher that joined muted as muted until its mic turns on", () => {
    const { r } = report({
      mic: { 2: [[T0 + 3, false]] },
      edgeAt: (_rx, s) => (s.speaker === 2 ? null : s.startMs + ONSET_MS),
    });
    expect(
      r.stretches.filter((s) => s.speaker === 2).every((s) => s.status === "publisher-muted"),
    ).toBe(true);
    expect(r.flags).toBe(0);
  });

  it.each([
    [5, 0],
    [6, 1],
  ])("fleet of %i, bot-1 2 s late: %i LATE_RECEIVER per stretch (needs 5 onsets)", (fleet, n) => {
    const { r } = report({
      fleet,
      edgeAt: (rx, s) =>
        rx === 2 && s.speaker === 0 ? null : s.startMs + ONSET_MS + (rx === 1 ? 2_000 : 0),
    });
    for (const s of scored(r)) {
      expect(flagged(s, "MISSED")).toEqual(s.speaker === 0 ? [2] : []);
      const late = s.speaker === 1 || (s.speaker === 0 && fleet - 2 < 5) ? 0 : n;
      expect(flagged(s, "LATE_RECEIVER")).toHaveLength(late);
    }
  });

  it("names the bot whose short log ends the default window", () => {
    const { r } = report({ until: { 5: T0 + 6 * MIN } });
    expect(r.toMs).toBeLessThan(T0 + 6 * MIN - END_MARGIN_MS + 1);
    expect([r.fromBy, r.toBy]).toEqual(["bot-0 first line + 60 s", "bot-5 last line - 5 s"]);
    let text = "";
    runReport(
      { logs: writeSynthDir({ until: { 5: T0 + 6 * MIN } }), fleetSize: "8", lateMs: "1000" },
      (t) => (text = t),
    );
    expect(text.split("\n")[0]).toContain("(bot-5 last line - 5 s)");
  });

  it("narrows the window with --from/--to and skips stretches that straddle it", () => {
    const from = T0 + 5 * MIN + 3_000;
    const to = T0 + 8 * MIN + 7_000;
    const { r } = report({}, { from: iso(from), to: iso(to) });
    expect([r.fromMs, r.toMs]).toEqual([from, to]);
    const inside = plannedStretches(8).filter(
      (s) => s.startMs >= from && s.endMs + HEARD_TAIL_MS <= to,
    );
    expect(r.stretches.map((s) => s.startMs)).toEqual(inside.map((s) => s.startMs));
    expect(plannedStretches(8).some((s) => s.startMs < from && s.endMs > from)).toBe(true);
  });

  it("exits 1 on a flag only with --fail-on-flag", () => {
    const missing: SynthOptions = { edgeAt: (rx, s) => (rx === 5 ? null : s.startMs + ONSET_MS) };
    expect(report(missing).code).toBe(0);
    expect(report(missing, { failOnFlag: true }).code).toBe(1);
    expect(report({}, { failOnFlag: true }).code).toBe(0);
  });

  it("exits 1 with --fail-on-flag when a publisher is muted on all its stretches, and names it", () => {
    const synth: SynthOptions = {
      mic: { 2: [[T0 + 3, false]] },
      edgeAt: (_rx, s) => (s.speaker === 2 ? null : s.startMs + ONSET_MS),
    };
    const { r, code } = report(synth, { failOnFlag: true });
    expect(r.flags).toBe(0);
    expect(r.mutedPublishers).toEqual([2]);
    expect(code).toBe(1);
    expect(report(synth).code).toBe(0);
    let text = "";
    runReport({ logs: writeSynthDir(synth), fleetSize: "8", lateMs: "1000" }, (t) => (text = t));
    expect(text).toContain("muted on all its stretches: bot-2\n");
  });

  it("exits 0 with --fail-on-flag when a publisher is muted on only some of its stretches", () => {
    const own = plannedStretches(8).filter(
      (s) => s.speaker === 2 && s.startMs >= T0 + JOIN_SETTLE_MS && s.endMs < T1 - 2 * MIN,
    );
    const { r, code } = report(
      {
        mic: { 2: [[own[0].endMs, false]] },
        edgeAt: (_rx, s) =>
          s.speaker === 2 && s.startMs > own[0].startMs ? null : s.startMs + ONSET_MS,
      },
      { failOnFlag: true },
    );
    expect(r.publishers.find((p) => p.publisher === 2)).toMatchObject({ scored: 1 });
    expect(r.publishers.find((p) => p.publisher === 2)?.muted).toBeGreaterThan(0);
    expect([r.flags, r.mutedPublishers, code]).toEqual([0, [], 0]);
  });

  it("refuses a report that scored no stretch", () => {
    const from = iso(T0 + 5 * MIN);
    expect(() => report({}, { from, to: iso(T0 + 5 * MIN + 5_000) })).toThrow(
      /no stretch was scored/,
    );
  });

  it("requires --fleet-size, so a missing top ordinal is listed unscored instead of shrinking the fleet", () => {
    const synth: SynthOptions = { fleet: 5, bots: [0, 1, 2, 3] };
    expect(() => report(synth, { fleetSize: undefined })).toThrow(/--fleet-size is required/);
    expect(() => report(synth, { fleetSize: "abc" })).toThrow(
      /--fleet-size must be a positive integer/,
    );
    const { r } = report(synth);
    expect(r.fleetSize).toBe(5);
    expect(r.stretches.filter((s) => s.speaker === 4).length).toBeGreaterThan(0);
    for (const s of r.stretches) expect(s.status === "publisher-no-log").toBe(s.speaker === 4);
  });

  it("rejects an invalid --fleet-size before reading the log directory", () => {
    const logs = join(tempDir("missing-"), "absent");
    expect(() => runReport({ logs, fleetSize: "abc", lateMs: "1000" }, () => {})).toThrow(
      /--fleet-size must be a positive integer/,
    );
    expect(() => runReport({ logs, fleetSize: "8", lateMs: "1000" }, () => {})).toThrow(
      /cannot read/,
    );
  });

  it("rejects a bot outside --fleet-size, and lists a missing bot's stretches unscored", () => {
    const synth: SynthOptions = { fleet: 5, bots: [0, 1, 2, 4] };
    expect(() => report(synth, { fleetSize: "4" })).toThrow(
      /bot-4 outside a fleet of 4; pass the replica count as --fleet-size/,
    );
    const { r } = report(synth);
    expect(r.stretches.filter((s) => s.speaker === 3).length).toBeGreaterThan(0);
    for (const s of r.stretches) expect(s.status === "publisher-no-log").toBe(s.speaker === 3);
    expect(r.flags).toBe(0);
    expect(r.mutedPublishers).toEqual([]);
  });

  it("reads the speech plan from --records", () => {
    const records = tempDir("records-");
    const write = (name: string, speech: unknown): void =>
      writeFileSync(join(records, name), JSON.stringify({ bot_id: name, speech }));
    write("a.json", { ...ROTATION_PLAN, weight: 0.125 });
    write("b.json", { ...ROTATION_PLAN, weight: 0.125 });
    const synth: SynthOptions = { plan: ROTATION_PLAN };
    expect(report(synth, { records }).r.flags).toBe(0);
    expect(report(synth).r.flags).toBeGreaterThan(0);
    write("c.json", { ...SPEECH_PLAN_DEFAULT, weight: 0.2 });
    expect(() => report(synth, { records })).toThrow(/2 distinct speech plans/);
  });

  it("rejects a malformed --from and an empty log directory", () => {
    expect(() => report({}, { from: "10:00" })).toThrow(ReportInputError);
    expect(() => report({}, { from: "2026-13-45T00:00Z" })).toThrow(/not a valid date/);
    const empty = tempDir("empty-");
    mkdirSync(join(empty, "sub"));
    expect(() => runReport({ logs: empty, fleetSize: "8", lateMs: "1000" }, () => {})).toThrow(
      /no console-log chunks/,
    );
  });
});

describe("report decision points", () => {
  it("counts an on edge up to 3 s after the stretch ends and not 1 ms later", () => {
    const { r } = report({
      edgeAt: (rx, s) =>
        rx === 3 ? s.endMs + 3_000 : rx === 4 ? s.endMs + 3_001 : s.startMs + ONSET_MS,
    });
    expect(scored(r).length).toBeGreaterThan(5);
    for (const s of scored(r)) expect(flagged(s, "MISSED")).toEqual(s.speaker === 4 ? [] : [4]);
  });

  it("flags a receiver or a publisher only when the offset exceeds --late-ms", () => {
    const rx = (extra: number) =>
      report({ edgeAt: (r, s) => s.startMs + ONSET_MS + (r === 3 ? extra : 0) });
    expect(rx(1_000).r.flags).toBe(0);
    expect(scored(rx(1_001).r).some((s) => flagged(s, "LATE_RECEIVER").includes(3))).toBe(true);
    const pub = (extra: number) =>
      report({ edgeAt: (_r, s) => s.startMs + ONSET_MS + (s.speaker === 1 ? extra : 0) });
    expect(pub(1_000).r.flags).toBe(0);
    expect(scored(pub(1_001).r).some((s) => s.latePublisher)).toBe(true);
  });

  it("measures a publisher against the run's own baseline, so a uniform offset flags nothing", () => {
    const { r, code } = report(
      { edgeAt: (_r, s) => s.startMs + ONSET_MS + 1_500 },
      { failOnFlag: true },
    );
    expect(r.flags).toBe(0);
    expect(code).toBe(0);
  });

  it("exits 1 with --fail-on-flag when only a publisher is flagged", () => {
    const { r, code } = report(
      { edgeAt: (_r, s) => s.startMs + ONSET_MS + (s.speaker === 1 ? 2_000 : 0) },
      { failOnFlag: true },
    );
    expect(scored(r).every((s) => flagged(s, "LATE_RECEIVER").length === 0)).toBe(true);
    expect(code).toBe(1);
  });

  it("reports the 90th percentile of a receiver's offsets", () => {
    const { r } = report({
      edgeAt: (rx, s) =>
        s.startMs + ONSET_MS + (rx === 3 && Math.floor(s.startMs / 10_000) % 3 === 0 ? 500 : 0),
    });
    const row = r.receivers.find((x) => x.receiver === 3);
    expect([row?.medianOffsetMs, row?.p90OffsetMs]).toEqual([0, 500]);
  });

  it("scores a stretch whose publisher unmutes at exactly its start", () => {
    const own = plannedStretches(8).filter(
      (s) =>
        s.speaker === 2 &&
        s.startMs >= T0 + JOIN_SETTLE_MS + 10_000 &&
        s.endMs + 3_000 <= T1 - END_MARGIN_MS - 10_000,
    );
    expect(own.length).toBeGreaterThan(0);
    const target = own[0];
    const { r } = report({
      mic: {
        2: [
          [T0 + 1_000, false],
          [target.startMs, true],
        ],
      },
    });
    expect(r.stretches.find((s) => s.startMs === target.startMs)?.status).toBe("scored");
  });

  it.each([
    [500, "scored"],
    [3_000, "scored"],
    [3_001, "publisher-muted"],
  ])("a publisher unmuted %i ms into its stretch is %s", (late, status) => {
    const own = plannedStretches(8).filter(
      (s) =>
        s.speaker === 2 &&
        s.startMs >= T0 + JOIN_SETTLE_MS + 10_000 &&
        s.endMs + 3_000 <= T1 - END_MARGIN_MS - 10_000,
    );
    expect(own.length).toBeGreaterThan(1);
    const [before, target] = own;
    const { r } = report({
      mic: {
        2: [
          [T0 + 1_000, false],
          [target.startMs + late, true],
        ],
      },
    });
    const at = (s: PlannedStretch) => r.stretches.find((x) => x.startMs === s.startMs);
    expect(at(before)?.status).toBe("publisher-muted");
    expect(at(target)?.status).toBe(status);
    if (status === "scored") expect(flagged(at(target)!, "MISSED")).toEqual([]);
  });

  it.each([
    ["mic on 1.2 s in, heard 0.4 s after it: not late", 1_200, 400, false],
    ["mic on 1.2 s in, heard 2 s after it: late", 1_200, 2_000, true],
    ["mic on at the start, heard 2 s in: late", null, 2_000, true],
  ])("LATE_PUBLISHER, %s", (_name, micOn, heard, late) => {
    const own = plannedStretches(8).filter(
      (s) =>
        s.speaker === 2 &&
        s.startMs >= T0 + JOIN_SETTLE_MS + 10_000 &&
        s.endMs + 3_000 <= T1 - END_MARGIN_MS - 10_000,
    );
    expect(own.length).toBeGreaterThan(1);
    const target = own[1];
    const synth: SynthOptions = {
      mic:
        micOn === null
          ? {}
          : {
              2: [
                [T0 + 1_000, false],
                [target.startMs + micOn, true],
              ],
            },
      edgeAt: (_rx, s) =>
        s.startMs + (s.startMs === target.startMs ? (micOn ?? 0) + heard : ONSET_MS),
    };
    const { r } = report(synth);
    const s = r.stretches.find((x) => x.startMs === target.startMs);
    expect(s?.status).toBe("scored");
    expect(s?.medianOnsetMs).toBe((micOn ?? 0) + heard);
    expect(s?.latePublisher).toBe(late);
    expect(r.flags).toBe(late ? 1 : 0);
    let text = "";
    runReport({ logs: writeSynthDir(synth), fleetSize: "8", lateMs: "1000" }, (t) => (text = t));
    expect(text.includes("measure a stretch whose publisher's mic came on after its start")).toBe(
      micOn !== null,
    );
  });

  it("measures from the edge the mic was on at the start, not a later re-unmute inside the stretch", () => {
    const target = plannedStretches(8).filter(
      (s) =>
        s.speaker === 2 &&
        s.startMs >= T0 + JOIN_SETTLE_MS + 10_000 &&
        s.endMs + 3_000 <= T1 - END_MARGIN_MS - 10_000,
    )[1];
    const { r } = report({
      mic: {
        2: [
          [T0 + 1_000, false],
          [target.startMs - 500, true],
          [target.startMs + 1_000, false],
          [target.startMs + 2_000, true],
        ],
      },
      edgeAt: (_rx, s) => s.startMs + (s.startMs === target.startMs ? 2_000 : ONSET_MS),
    });
    const s = r.stretches.find((x) => x.startMs === target.startMs);
    expect(s?.status).toBe("scored");
    expect(s?.micOnMs).toBe(0);
    expect(s?.latePublisher).toBe(true);
  });

  it("takes the LATE_PUBLISHER baseline from the mic-on edge too", () => {
    const mic: Record<number, Array<[number, boolean]>> = {};
    for (const s of plannedStretches(8)) {
      if (s.speaker === 5) continue;
      mic[s.speaker] ??= [[T0 + 1_000, false]];
      mic[s.speaker].push([s.startMs + 1_200, true], [s.endMs, false]);
    }
    const { r } = report({ mic, edgeAt: (_rx, s) => s.startMs + 1_600 });
    expect(scored(r).filter((s) => s.speaker === 5).length).toBeGreaterThan(0);
    for (const s of scored(r)) expect(s.latePublisher).toBe(s.speaker === 5);
  });

  it("judges neither flag, and says so, when fewer than 5 receivers have an onset", () => {
    const edgeAt = (rx: number, st: PlannedStretch): number =>
      st.startMs + ONSET_MS + (rx === 1 ? 2_000 : 0);
    const { r } = report({ fleet: 2, edgeAt });
    expect(scored(r).length).toBeGreaterThan(5);
    expect(r.unjudged).toBe(scored(r).length);
    expect(r.flags).toBe(0);
    let text = "";
    runReport(
      { logs: writeSynthDir({ fleet: 2, edgeAt }), fleetSize: "2", lateMs: "1000" },
      (t) => (text = t),
    );
    expect(text).toContain(
      "fewer than 5 receiver onsets: LATE_RECEIVER and LATE_PUBLISHER were not judged",
    );
  });

  it("refuses logs with no [PEER_SPEAKING] line", () => {
    expect(() => report({ edgeAt: () => null })).toThrow(/no \[PEER_SPEAKING\] lines/);
  });

  it("the README states the window and threshold constants", () => {
    const md = readFileSync(resolve(import.meta.dirname, "../../README.md"), "utf8");
    const section = md.slice(md.indexOf("## Delivery report"), md.indexOf("## Resource capture"));
    for (const phrase of [
      `${JOIN_SETTLE_MS / 1000} s after the last session's first line`,
      `${END_MARGIN_MS / 1000} s before the earliest-ending session's last line`,
      `from ${HEARD_LEAD_SLACK_MS / 1000} s before the stretch start to ${HEARD_TAIL_MS / 1000} s after its end`,
      `with their ${HEARD_TAIL_MS / 1000} s tail`,
      `off for more than ${MUTED_OFF_MS / 1000} s of it`,
    ]) {
      expect(section).toContain(phrase);
    }
    const minimums = [...section.matchAll(/at least (\d+) receiver onsets/g)].map((m) => m[1]);
    expect(minimums.length).toBeGreaterThan(1);
    expect(new Set(minimums)).toEqual(new Set([String(MIN_RECEIVERS_FOR_LATE)]));
  });

  it("judges LATE_PUBLISHER and its baseline only on stretches with 5 receiver onsets", () => {
    const { r } = report({
      edgeAt: (rx, s) => {
        if (s.speaker === 5) return s.startMs + ONSET_MS + 2_000;
        if (s.speaker === 6 || s.speaker === 7) return s.startMs + ONSET_MS;
        return rx < 3 ? s.startMs + ONSET_MS + 2_000 : null;
      },
    });
    const late = scored(r).filter((s) => s.speaker === 5);
    const thin = scored(r).filter((s) => s.speaker <= 4);
    expect(late.length).toBeGreaterThan(0);
    expect(thin.length).toBeGreaterThan(late.length + 5);
    expect(late.every((s) => s.latePublisher)).toBe(true);
    expect(thin.some((s) => s.latePublisher)).toBe(false);
    expect(r.unjudged).toBe(thin.length);
  });

  it("hears a publisher that was assigned a second session id mid-run", () => {
    const X = T0 + 8 * MIN;
    const { r } = report({ reconnect: { 3: X } });
    expect(scored(r).filter((s) => s.speaker === 3 && s.startMs >= X).length).toBeGreaterThan(0);
    expect(scored(r).filter((s) => s.speaker === 3 && s.startMs < X).length).toBeGreaterThan(0);
    expect(r.flags).toBe(0);
  });

  it("narrows the window with --from/--to but never widens it", () => {
    const { r } = report({}, { from: iso(T0 - 5 * MIN), to: iso(T1 + 5 * MIN) });
    expect([r.fromMs, r.toMs]).toEqual([T0 + JOIN_SETTLE_MS, T1 - END_MARGIN_MS]);
    expect([r.fromBy, r.toBy]).toEqual([
      expect.stringContaining("first line"),
      expect.stringContaining("last line"),
    ]);
  });

  it("starts the window after the last session's first line", () => {
    const late = T0 + 5 * MIN;
    const { r } = report({ joinAt: { 5: late } });
    expect(r.fromMs).toBe(late + JOIN_SETTLE_MS);
    expect(r.fromBy).toContain("bot-5 first line");
  });

  it("lists a publisher whose log has no Host render line as muted, never MISSED", () => {
    const { r } = report({ noHostRender: [2] });
    const own = r.stretches.filter((s) => s.speaker === 2);
    expect(own.length).toBeGreaterThan(0);
    expect(own.every((s) => s.status === "publisher-muted")).toBe(true);
    expect(r.flags).toBe(0);
  });
});

describe("muted rule on Host render lines that repeat the mic state", () => {
  it("counts only the time the mic is off: camera-toggle lines with the mic on keep a stretch scored", () => {
    const START = 100_000;
    const stretch = { speaker: 0, startMs: START, endMs: START + 30_000, leadMs: 0 };
    const bot = (ordinal: number, mic: Array<[number, boolean]>): BotLog => ({
      ordinal,
      stem: `bot-${ordinal}`,
      sessionIds: [`s${ordinal}`],
      firstMs: 0,
      lastMs: START + 60_000,
      onEdges: new Map([["s0", [START + ONSET_MS]]]),
      peerSpeakingLines: 1,
      mic: mic.map(([ms, on]) => ({ ms, on })),
    });
    const bots = [
      bot(0, [
        [0, true],
        [START + 1_000, true],
        [START + 5_000, true],
        [START + 28_000, false],
      ]),
      ...[1, 2, 3, 4, 5].map((i) => bot(i, [])),
    ];
    const window = { fromMs: 0, toMs: START + 60_000, fromBy: "", toBy: "" };
    const r = analyze(bots, [stretch], window, 6, 1_000);
    expect(r.stretches[0].status).toBe("scored");
  });
});
