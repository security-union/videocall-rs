import { describe, expect, it } from "vitest";

import {
  HEARTBEAT_KEEPALIVE_INTERVAL_MS,
  SCREEN_STATIC_HEARTBEAT_FRESH_MS,
} from "./rust-mirrored-constants";
import {
  SETTLED_WINDOW_SAMPLES,
  classifyScreenText,
  settledStaticWindow,
} from "./static-screen-window";

const S = "Screen 1280x720 (HD) · 10.0fps (static) · 10kbps (static)";
const L = "Screen 1280x720 (HD) · 2.5fps · 28kbps";
const F = "Screen 1280x720 (HD) · 0.0fps (no frames) · 0kbps (no frames)";

const times = (line: string, n: number): string[] => Array<string>(n).fill(line);

function longestStaticRun(screens: string[]): number {
  let best = 0;
  let run = 0;
  for (const s of screens) {
    run = classifyScreenText(s) === "static" ? run + 1 : 0;
    best = Math.max(best, run);
  }
  return best;
}

describe("classifyScreenText", () => {
  it("reads the tooltip's three screen states", () => {
    expect(classifyScreenText(S)).toBe("static");
    expect(classifyScreenText(L)).toBe("live");
    expect(classifyScreenText(F)).toBe("no-frames");
  });

  it("treats a missing Screen line and a half-static line as live", () => {
    expect(classifyScreenText("<no Screen line> Time: 0m 29s")).toBe("live");
    expect(classifyScreenText("Screen 1280x720 (HD) · 10.0fps (static) · 8kbps")).toBe("live");
  });
});

describe("settledStaticWindow", () => {
  it("settles on a healthy publisher whose heartbeat is phase-locked to the sample clock", () => {
    const sampleMs = 1002;
    const beatMs = HEARTBEAT_KEEPALIVE_INTERVAL_MS + 10;
    const ages: number[] = [];
    const screens: string[] = [];
    for (let k = 0; k < 30; k++) {
      const t = 995 + k * sampleMs;
      const age = t - Math.floor(t / beatMs) * beatMs;
      ages.push(age);
      screens.push(age <= SCREEN_STATIC_HEARTBEAT_FRESH_MS ? S : F);
    }

    expect(ages.slice(0, 10)).toEqual([995, 1997, 2999, 4001, 5003, 995, 1997, 2999, 4001, 5003]);
    expect(longestStaticRun(screens)).toBe(4);
    expect(settledStaticWindow(screens)).toEqual([S, S, S, S, F, S]);
  });

  it("settles after the static keyframe floor's live blips end", () => {
    const screens = [S, L, L, S, S, L, L, S, S, L, S, S, ...times(S, SETTLED_WINDOW_SAMPLES)];
    expect(settledStaticWindow(screens)).toEqual(times(S, SETTLED_WINDOW_SAMPLES));
  });

  it("rejects a share that never goes static", () => {
    expect(settledStaticWindow(times(L, 30))).toBeNull();
  });

  it("rejects static that turns live before the window fills", () => {
    expect(settledStaticWindow([S, ...times(L, 29)])).toBeNull();
    expect(
      settledStaticWindow([...times(S, SETTLED_WINDOW_SAMPLES - 1), ...times(L, 25)]),
    ).toBeNull();
  });

  it("accepts a window that ends on the latest sample", () => {
    expect(settledStaticWindow(times(S, SETTLED_WINDOW_SAMPLES))).toEqual(
      times(S, SETTLED_WINDOW_SAMPLES),
    );
  });

  it("accepts a static run exactly one window long, whatever follows", () => {
    expect(settledStaticWindow([...times(S, SETTLED_WINDOW_SAMPLES), ...times(L, 24)])).toEqual(
      times(S, SETTLED_WINDOW_SAMPLES),
    );
  });

  it("rejects persistent no-frames", () => {
    expect(settledStaticWindow([S, ...times(F, 29)])).toBeNull();
    expect(settledStaticWindow([S, F, F, S, S, S, S, S])).toBeNull();
  });

  it("rejects no-frames on every other sample", () => {
    expect(settledStaticWindow(Array.from({ length: 30 }, (_, i) => (i % 2 ? F : S)))).toBeNull();
  });

  it("rejects a window that ends on no-frames", () => {
    expect(settledStaticWindow([S, S, S, S, S, F])).toBeNull();
  });
});
