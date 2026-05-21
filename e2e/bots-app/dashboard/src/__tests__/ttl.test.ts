import { describe, expect, it } from "vitest";

import { formatRemaining, isValidTtl, parseDurationClient } from "../lib/ttl";

describe("parseDurationClient", () => {
  it.each([
    ["5m", 300_000],
    ["30s", 30_000],
    ["1h", 3_600_000],
    ["  2H  ", 7_200_000],
  ])("parses %s into %d ms", (input, expected) => {
    expect(parseDurationClient(input)).toBe(expected);
  });

  it("recognizes 'infinite' (case-insensitive)", () => {
    expect(parseDurationClient("infinite")).toBe("infinite");
    expect(parseDurationClient("INFINITE")).toBe("infinite");
  });

  it.each(["", "0m", "5", "5x", "abc"])("rejects %p", (bad) => {
    expect(() => parseDurationClient(bad)).toThrow();
  });
});

describe("isValidTtl", () => {
  it("returns true for valid inputs", () => {
    expect(isValidTtl("5m")).toBe(true);
    expect(isValidTtl("infinite")).toBe(true);
  });

  it("returns false for invalid inputs", () => {
    expect(isValidTtl("")).toBe(false);
    expect(isValidTtl("5x")).toBe(false);
  });
});

describe("formatRemaining", () => {
  it("renders mm:ss for sub-hour durations", () => {
    expect(formatRemaining(305_000)).toBe("5:05");
    expect(formatRemaining(60_000)).toBe("1:00");
  });

  it("renders h:mm:ss for ≥1h durations", () => {
    expect(formatRemaining(3_605_000)).toBe("1:00:05");
  });

  it("renders 0s for non-positive remaining", () => {
    expect(formatRemaining(0)).toBe("0s");
    expect(formatRemaining(-1)).toBe("0s");
  });

  it("renders 'infinite' for null", () => {
    expect(formatRemaining(null)).toBe("infinite");
  });
});
