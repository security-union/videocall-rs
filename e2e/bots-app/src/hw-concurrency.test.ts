import { describe, expect, it } from "vitest";

import { resolveHardwareConcurrency } from "./hw-concurrency";

describe("resolveHardwareConcurrency (#2035)", () => {
  it("returns undefined (no spoof) when unset, empty, or whitespace", () => {
    expect(resolveHardwareConcurrency(undefined)).toEqual({ kind: "ok", value: undefined });
    expect(resolveHardwareConcurrency("")).toEqual({ kind: "ok", value: undefined });
    expect(resolveHardwareConcurrency("   ")).toEqual({ kind: "ok", value: undefined });
  });

  it("accepts a strict positive integer (spoof enabled)", () => {
    expect(resolveHardwareConcurrency("6")).toEqual({ kind: "ok", value: 6 });
    expect(resolveHardwareConcurrency("2")).toEqual({ kind: "ok", value: 2 });
    expect(resolveHardwareConcurrency(" 10 ")).toEqual({ kind: "ok", value: 10 });
  });

  it("treats <= 0 as the documented disable sentinel → no spoof, NOT an error", () => {
    // The CLI help + bot.ts both say "<= 0 → the browser's real value is used".
    // bot.ts only injects when > 0, so 0/negative must resolve to undefined
    // (not exit 2). Reverting to the old `parsed <= 0 → invalid` breaks this.
    expect(resolveHardwareConcurrency("0")).toEqual({ kind: "ok", value: undefined });
    expect(resolveHardwareConcurrency("-3")).toEqual({ kind: "ok", value: undefined });
  });

  it("rejects a value that is not a whole integer (parseInt would silently accept a prefix)", () => {
    // Number.parseInt("6junk", 10) === 6 — accepting a numeric prefix would
    // launch with an unintended core count. The whole token must be an integer.
    for (const bad of ["6junk", "1.5", "1e2", "abc", "0x10", "+ 5"]) {
      const r = resolveHardwareConcurrency(bad);
      expect(r.kind).toBe("invalid");
      if (r.kind === "invalid") {
        expect(r.message).toContain(bad);
      }
    }
  });
});
