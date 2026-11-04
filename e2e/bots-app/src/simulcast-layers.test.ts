import { describe, expect, it } from "vitest";

import {
  resolveSimulcastMaxLayers,
  SIMULCAST_LADDER_DEPTH,
  simulcastMaxLayersReadback,
} from "./simulcast-layers";

describe("resolveSimulcastMaxLayers (#3246)", () => {
  it.each([undefined, "", "  "])("inherits the deployment for %j", (raw) => {
    expect(resolveSimulcastMaxLayers(raw)).toEqual({ kind: "ok", value: undefined });
  });

  it("accepts every video layer count from 1 to the ladder depth", () => {
    expect(SIMULCAST_LADDER_DEPTH).toBe(3);
    for (const n of [1, 2, 3]) {
      expect(resolveSimulcastMaxLayers(` ${n} `)).toEqual({ kind: "ok", value: n });
    }
  });

  it.each(["0", "4", "-1", "2.5", "3x", "1e0", "three", "+2"])("refuses %j", (raw) => {
    const r = resolveSimulcastMaxLayers(raw);
    expect(r.kind).toBe("invalid");
    expect(r.kind === "invalid" && r.message).toBe(
      `--simulcast-max-layers (or BOT_SIMULCAST_MAX_LAYERS) must be an integer from 1 to 3, got "${raw}"`,
    );
  });
});

describe("simulcastMaxLayersReadback", () => {
  it("keeps a number and maps anything else to null", () => {
    expect(simulcastMaxLayersReadback(2)).toBe(2);
    for (const v of [undefined, null, "3", {}]) expect(simulcastMaxLayersReadback(v)).toBeNull();
  });
});
