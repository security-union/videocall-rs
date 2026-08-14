import { describe, expect, it } from "vitest";

import { buildNetemRequest } from "./ctl";

describe("buildNetemRequest", () => {
  it("maps --clear to DELETE with no body", () => {
    expect(buildNetemRequest({ clear: true })).toEqual({ method: "DELETE" });
  });

  it("maps --profile to POST { profile }", () => {
    expect(buildNetemRequest({ profile: "satellite" })).toEqual({
      method: "POST",
      body: { profile: "satellite" },
    });
  });

  it("maps raw flags to POST with parsed numeric params", () => {
    expect(buildNetemRequest({ delay: "150", jitter: "50", loss: "5", rate: "800" })).toEqual({
      method: "POST",
      body: { delayMs: 150, jitterMs: 50, lossPct: 5, rateKbit: 800 },
    });
  });

  it("omits absent raw flags", () => {
    expect(buildNetemRequest({ loss: "2" })).toEqual({
      method: "POST",
      body: { lossPct: 2 },
    });
  });

  it("prefers --clear over any other flag", () => {
    // Defensive: if both are somehow set, clear wins and produces no body.
    expect(buildNetemRequest({ clear: true, profile: "dialup" })).toEqual({ method: "DELETE" });
  });
});
