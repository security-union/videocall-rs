import { describe, expect, it } from "vitest";

import { arrivalSpread, ArrivalTracker } from "./arrival";

describe("arrivalSpread", () => {
  it("returns null with no joins, distinguishing that from a zero spread", () => {
    expect(arrivalSpread([])).toBeNull();
    expect(arrivalSpread([1000])).toEqual({
      count: 1,
      firstJoinMs: 1000,
      lastJoinMs: 1000,
      spreadMs: 0,
    });
  });

  it("spans first to last regardless of the order joins were recorded", () => {
    expect(arrivalSpread([5000, 1000, 3000])?.spreadMs).toBe(4000);
  });

  it("ignores non-finite instants rather than poisoning the min/max", () => {
    expect(arrivalSpread([1000, Number.NaN, 4000, Number.POSITIVE_INFINITY])).toEqual({
      count: 2,
      firstJoinMs: 1000,
      lastJoinMs: 4000,
      spreadMs: 3000,
    });
  });
});

describe("ArrivalTracker", () => {
  it("keeps each bot's FIRST join so a rejoin cannot widen the ramp", () => {
    const t = new ArrivalTracker();
    t.record("a", 1000);
    t.record("b", 2000);
    t.record("a", 90_000);

    expect(t.snapshot()).toEqual({
      count: 2,
      firstJoinMs: 1000,
      lastJoinMs: 2000,
      spreadMs: 1000,
    });
  });

  it("times a join with the injected clock when the caller passes none", () => {
    const t = new ArrivalTracker(() => 4242);
    t.record("a");
    expect(t.snapshot()?.firstJoinMs).toBe(4242);
  });
});
