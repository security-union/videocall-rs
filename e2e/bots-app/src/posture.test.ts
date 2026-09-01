import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  captureGeometryToken,
  HD_SOURCE,
  resolveBaseBotIndex,
  SD_SOURCE,
  sourceGeometryForIndex,
} from "./posture";

const README_PATH = fileURLToPath(new URL("../README.md", import.meta.url));

/** Indices of `0..count-1` that capture 1280x720, by literal geometry. */
function hdIndices(count: number): number[] {
  return Array.from({ length: count }, (_, i) => i).filter((i) => {
    const g = sourceGeometryForIndex(i);
    return g.width === 1280 && g.height === 720;
  });
}

describe("sourceGeometryForIndex (#2236)", () => {
  it("pins the two capture geometries", () => {
    expect(SD_SOURCE).toEqual({ width: 640, height: 480 });
    expect(HD_SOURCE).toEqual({ width: 1280, height: 720 });
  });

  it("keeps index 0 — an unset BOT_INDEX — at 640x480", () => {
    expect(sourceGeometryForIndex(0)).toEqual({ width: 640, height: 480 });
  });

  it("puts 1280x720 on 4 of the first 25 indices and 640x480 on the other 21", () => {
    expect(hdIndices(25)).toEqual([1, 7, 13, 19]);
    const sd = Array.from({ length: 25 }, (_, i) => i).filter((i) => !hdIndices(25).includes(i));
    expect(sd).toHaveLength(21);
    expect(sd.every((i) => sourceGeometryForIndex(i).width === 640)).toBe(true);
    expect(sd.every((i) => sourceGeometryForIndex(i).height === 480)).toBe(true);
    expect(hdIndices(600)).toHaveLength(100);
  });

  it("rejects an index that is not a non-negative integer", () => {
    for (const bad of [-1, 1.5, Number.NaN, Number.POSITIVE_INFINITY, 2 ** 53]) {
      expect(() => sourceGeometryForIndex(bad), String(bad)).toThrow(
        "bot index must be a non-negative integer",
      );
    }
  });
});

describe("captureGeometryToken (#2236)", () => {
  it("names what the bot knows — the capture size, not a published rung", () => {
    expect(captureGeometryToken("clock-bot", HD_SOURCE)).toBe(
      "[clock-bot] captures 1280x720 (issue #2236)",
    );
    expect(captureGeometryToken("clock-bot@ab12", SD_SOURCE)).toBe(
      "[clock-bot@ab12] captures 640x480 (issue #2236)",
    );
  });

  it("stays greppable by the README's fleet-mix recipe", () => {
    const recipes = [
      ...readFileSync(README_PATH, "utf8").matchAll(/grep -c "([^"]*1280x720)"/g),
    ].map((m) => m[1]);
    expect(recipes).toEqual(["captures 1280x720"]);
    expect(captureGeometryToken("videocall-bots-1", HD_SOURCE)).toContain(recipes[0]);
  });

  it.each([
    ["LF", "\n"],
    ["CR", "\r"],
  ])("a %s in the label cannot start a second prefixed line", (_name, ctrl) => {
    const token = captureGeometryToken(`bot-0${ctrl}[bot-0] FORGED-BY-POSTURE`, SD_SOURCE);
    expect(token.split(/[\r\n]/)).toHaveLength(1);
    expect(token).toContain("FORGED-BY-POSTURE");
    expect(token).toContain("captures 640x480");
  });
});

describe("resolveBaseBotIndex (#2236)", () => {
  it("defaults to 0 when neither the flag nor the env is usable", () => {
    for (const [flag, env] of [
      [undefined, undefined],
      [undefined, ""],
      ["", undefined],
      [undefined, "   "],
    ] as [string | undefined, string | undefined][]) {
      expect(resolveBaseBotIndex(flag, env), `${String(flag)}/${String(env)}`).toEqual({
        kind: "ok",
        value: 0,
      });
    }
  });

  it("prefers the flag over the env", () => {
    expect(resolveBaseBotIndex("7", "2")).toEqual({ kind: "ok", value: 7 });
    expect(resolveBaseBotIndex(undefined, "2")).toEqual({ kind: "ok", value: 2 });
    expect(resolveBaseBotIndex(" 13 ", undefined)).toEqual({ kind: "ok", value: 13 });
  });

  it("falls back to the env when the flag is present but blank", () => {
    // `flag ?? env` would take the blank flag and silently resolve to 0.
    expect(resolveBaseBotIndex("", "7")).toEqual({ kind: "ok", value: 7 });
    expect(resolveBaseBotIndex("   ", "7")).toEqual({ kind: "ok", value: 7 });
  });

  it("rejects a numeric prefix, a float, a negative and an exponent", () => {
    for (const raw of ["3junk", "1.5", "-2", "1e2", "0x4", "٣"]) {
      const result = resolveBaseBotIndex(raw, undefined);
      expect(result.kind, raw).toBe("invalid");
      expect(result.kind === "invalid" && result.message).toContain(raw);
    }
  });

  it("rejects a token too large to be an exact index", () => {
    expect(resolveBaseBotIndex("99999999999999999999", undefined).kind).toBe("invalid");
  });
});
