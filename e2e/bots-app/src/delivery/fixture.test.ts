import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { SPEECH_PLAN_DEFAULT } from "../speech-plan";
import type { DeliveryReport } from "./analyze";
import { runReport } from "./command";
import { stretchesBetween } from "./schedule";

const LOGS = fileURLToPath(new URL("./fixtures/run3", import.meta.url));
const RUN3_SPEECH = readFileSync(
  fileURLToPath(new URL("./fixtures/run3-clock-speech.js", import.meta.url)),
  "utf8",
);
const SLICES = {
  before: ["2026-10-09T17:36:15Z", "2026-10-09T17:38:45Z"],
  during: ["2026-10-09T17:46:55Z", "2026-10-09T17:49:25Z"],
  after: ["2026-10-09T17:59:25Z", "2026-10-09T18:01:15Z"],
} as const;

function slice(name: keyof typeof SLICES): DeliveryReport {
  let text = "";
  const [from, to] = SLICES[name];
  runReport(
    { logs: LOGS, fleetSize: "8", from, to, lateMs: "1000", json: true },
    (t) => (text = t),
    undefined,
    RUN3_SPEECH,
  );
  return JSON.parse(text) as DeliveryReport;
}

const receiverFlags = (r: DeliveryReport): string[] =>
  r.stretches.flatMap((s) =>
    s.receivers.filter((x) => x.flag !== null).map((x) => `${x.flag} bot-${x.receiver}`),
  );

describe("delivery report on run-3 console logs", () => {
  it("rebuilds the schedule from the injected planner, not the checkout's", () => {
    const [from, to] = SLICES.during.map((t) => Date.parse(t));
    const run3 = stretchesBetween(SPEECH_PLAN_DEFAULT, 8, from, to, RUN3_SPEECH);
    expect(run3).not.toEqual(stretchesBetween(SPEECH_PLAN_DEFAULT, 8, from, to));
  });

  it.each(["before", "after"] as const)("flags nothing %s the delay", (name) => {
    const r = slice(name);
    expect(r.stretches.filter((s) => s.status === "scored").length).toBeGreaterThanOrEqual(5);
    expect(r.stretches.some((s) => s.speaker === 4)).toBe(true);
    expect(r.flags).toBe(0);
  });

  it("flags bot-4 as LATE_RECEIVER on every stretch during the delay, and no other receiver", () => {
    const r = slice("during");
    const heardBy4 = r.stretches.filter((s) => s.status === "scored" && s.speaker !== 4);
    expect(heardBy4.length).toBeGreaterThanOrEqual(5);
    expect(receiverFlags(r)).toEqual(heardBy4.map(() => "LATE_RECEIVER bot-4"));
    for (const s of heardBy4) {
      expect(s.receivers.find((x) => x.receiver === 4)?.offsetMs).toBeGreaterThan(1_000);
    }
  });

  it("flags bot-4's own stretches during the delay as LATE_PUBLISHER, and no other", () => {
    const r = slice("during");
    expect(r.stretches.some((s) => s.speaker === 4)).toBe(true);
    for (const s of r.stretches) expect(s.latePublisher).toBe(s.speaker === 4);
  });
});
