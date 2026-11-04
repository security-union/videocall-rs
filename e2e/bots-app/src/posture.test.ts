import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  captureGeometryToken,
  HD_SOURCE,
  resolveBaseBotIndex,
  resolveFleetSize,
  resolveTransportWsEvery,
  SD_SOURCE,
  sourceGeometryForIndex,
  strideHdSkew,
  transportForIndex,
  websocketIndices,
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

describe("resolveFleetSize (issue 3072)", () => {
  it("uses the fallback when neither flag nor env is set", () => {
    expect(resolveFleetSize(undefined, undefined, 4)).toEqual({ kind: "ok", value: 4 });
    expect(resolveFleetSize(" ", "", 4)).toEqual({ kind: "ok", value: 4 });
  });

  it("lets the flag win over the env", () => {
    expect(resolveFleetSize("6", "9", 1)).toEqual({ kind: "ok", value: 6 });
    expect(resolveFleetSize(undefined, "9", 1)).toEqual({ kind: "ok", value: 9 });
  });

  it("rejects zero, a numeric prefix, a float and a negative", () => {
    for (const raw of ["0", "3junk", "1.5", "-2"]) {
      expect(resolveFleetSize(raw, undefined, 1).kind, raw).toBe("invalid");
    }
  });
});

describe("transportForIndex (issue 3087)", () => {
  it.each([
    [4, 8, [3, 7]],
    [4, 4, [3]],
    [4, 3, []],
    [5, 12, [2, 7]],
    [7, 8, [0, 7]],
    [8, 16, [7, 15]],
    [1, 3, [0, 1, 2]],
  ])("every %i over a fleet of %i puts websocket on %j", (k, n, ws) => {
    expect(websocketIndices(k, n)).toEqual(ws);
    for (let i = 0; i < n; i++) {
      expect(transportForIndex(i, k)).toBe(ws.includes(i) ? "websocket" : "webtransport");
    }
  });

  it("refuses k > 1 exactly when it divides or is a multiple of 6, for every k up to 400", () => {
    for (let k = 1; k <= 400; k++) {
      const refused = resolveTransportWsEvery(String(k)).kind === "invalid";
      expect(refused, `k=${k}`).toBe(k > 1 && (6 % k === 0 || k % 6 === 0));
    }
  });

  it("puts an HD and a non-HD bot on each transport in a fleet of k + 8, for every accepted k up to 400", () => {
    for (let k = 1; k <= 400; k++) {
      const r = resolveTransportWsEvery(String(k));
      if (r.kind === "invalid") continue;
      const n = k + 8;
      const kinds = new Set(
        Array.from({ length: n }, (_, i) =>
          [transportForIndex(i, r.value as number), sourceGeometryForIndex(i) === HD_SOURCE].join(
            ":",
          ),
        ),
      );
      const expected =
        k === 1
          ? ["websocket:true", "websocket:false"]
          : ["websocket:true", "websocket:false", "webtransport:true", "webtransport:false"];
      expect([...kinds].sort(), `k=${k}`).toEqual(expected.sort());
    }
  });

  it("names the balanced strides in the refusal and warns only for a stride sharing a factor with 6", () => {
    const r = resolveTransportWsEvery("12");
    expect(r.kind === "invalid" && r.message).toContain("e.g. 5, 7, 11, 13 (");
    expect(strideHdSkew(5, 8)).toBeNull();
    expect(strideHdSkew(4, 8)).toContain("at 1 of 2 websocket bots against 2 of 8 in the fleet");
    expect(strideHdSkew(9, 8)).toContain("at 1 of 1 websocket bots against 2 of 8 in the fleet");
    expect(strideHdSkew(4, 8)).not.toContain("(5, 7");
  });

  it("rejects a non-positive stride and a bad index", () => {
    expect(() => transportForIndex(0, 0)).toThrow();
    expect(() => transportForIndex(-1, 4)).toThrow();
  });

  it("resolves BOT_TRANSPORT_WS_EVERY", () => {
    expect(resolveTransportWsEvery(undefined)).toEqual({ kind: "ok", value: null });
    expect(resolveTransportWsEvery(" ")).toEqual({ kind: "ok", value: null });
    expect(resolveTransportWsEvery(" 4 ")).toEqual({ kind: "ok", value: 4 });
    for (const raw of ["0", "4x", "-1", "1.5"]) {
      expect(resolveTransportWsEvery(raw).kind, raw).toBe("invalid");
    }
  });
});
