import { describe, expect, it, vi } from "vitest";

const calls = vi.hoisted(() => ({ blockAt: 0 }));

vi.mock("./clock-init", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./clock-init")>();
  return {
    ...actual,
    loadClockSpeech: (...args: Parameters<typeof actual.loadClockSpeech>) => {
      const speech = actual.loadClockSpeech(...args);
      return {
        ...speech,
        createSchedule: (...a: Parameters<typeof speech.createSchedule>) => {
          const schedule = speech.createSchedule(...a);
          return {
            ...schedule,
            blockAt: (block: number) => {
              calls.blockAt += 1;
              return schedule.blockAt(block);
            },
          };
        },
      };
    },
  };
});

import { assignPersonas, createPersonaPlanner, resolvePersonaSettings } from "./persona";
import { SPEECH_PLAN_DEFAULT } from "./speech-plan";

describe("persona assignment cost (issue 3091)", () => {
  it("reads each block's turn plan about once, whatever the room size", () => {
    const settings = resolvePersonaSettings(true, {});
    if (settings.kind !== "ok" || settings.value === null) throw new Error("persona defaults");
    for (const n of [8, 200]) {
      calls.blockAt = 0;
      assignPersonas(createPersonaPlanner(SPEECH_PLAN_DEFAULT, n), n, settings.value);
      expect(calls.blockAt, `blockAt calls at ${n} bots`).toBeLessThan(3_000);
    }
  });
});
