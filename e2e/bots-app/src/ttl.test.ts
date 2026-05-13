import { describe, it, expect, vi, afterEach } from "vitest";

import { formatDuration, parseDuration, waitForTtl } from "./ttl";

describe("parseDuration", () => {
  it("parses seconds", () => {
    expect(parseDuration("30s")).toBe(30_000);
    expect(parseDuration("1s")).toBe(1_000);
  });

  it("parses minutes", () => {
    expect(parseDuration("5m")).toBe(300_000);
    expect(parseDuration("1m")).toBe(60_000);
  });

  it("parses hours", () => {
    expect(parseDuration("2h")).toBe(7_200_000);
    expect(parseDuration("1h")).toBe(3_600_000);
  });

  it("accepts the `infinite` sentinel", () => {
    expect(parseDuration("infinite")).toBe("infinite");
  });

  it("tolerates surrounding whitespace and uppercase", () => {
    expect(parseDuration("  5M  ")).toBe(300_000);
    expect(parseDuration("INFINITE")).toBe("infinite");
  });

  it("rejects empty input", () => {
    expect(() => parseDuration("")).toThrow(/must not be empty/);
    expect(() => parseDuration("   ")).toThrow(/must not be empty/);
  });

  it("rejects zero or negative durations", () => {
    expect(() => parseDuration("0s")).toThrow(/must be positive/);
    // The regex requires \d+, so "-5m" doesn't match the value group; we
    // expect the "not valid" branch instead of the positivity check. Either
    // is a rejection, which is what matters operationally.
    expect(() => parseDuration("-5m")).toThrow();
  });

  it("rejects unknown suffixes", () => {
    expect(() => parseDuration("5d")).toThrow(/not valid/);
    expect(() => parseDuration("5")).toThrow(/not valid/);
    expect(() => parseDuration("5ms")).toThrow(/not valid/);
  });

  it("rejects garbage input", () => {
    expect(() => parseDuration("forever")).toThrow(/not valid/);
    expect(() => parseDuration("5m30s")).toThrow(/not valid/);
  });
});

describe("formatDuration", () => {
  it("round-trips through parseDuration for canonical inputs", () => {
    expect(formatDuration(parseDuration("5m"))).toBe("5m");
    expect(formatDuration(parseDuration("30s"))).toBe("30s");
    expect(formatDuration(parseDuration("2h"))).toBe("2h");
    expect(formatDuration(parseDuration("infinite"))).toBe("infinite");
  });

  it("prefers the largest exact unit", () => {
    expect(formatDuration(60_000)).toBe("1m");
    expect(formatDuration(3_600_000)).toBe("1h");
    expect(formatDuration(7_200_000)).toBe("2h");
  });

  it("falls back to seconds for non-aligned values", () => {
    expect(formatDuration(45_000)).toBe("45s");
  });
});

describe("waitForTtl", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("resolves after the configured delay (finite case)", async () => {
    vi.useFakeTimers();
    const { done } = waitForTtl(parseDuration("30s"));
    let resolved = false;
    void done.then(() => {
      resolved = true;
    });
    await vi.advanceTimersByTimeAsync(29_999);
    expect(resolved).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    expect(resolved).toBe(true);
  });

  it("never resolves for the infinite case", async () => {
    vi.useFakeTimers();
    const { done } = waitForTtl("infinite");
    let resolved = false;
    void done.then(() => {
      resolved = true;
    });
    // Advance an arbitrarily large amount of fake time — should still be unresolved.
    await vi.advanceTimersByTimeAsync(365 * 24 * 3_600_000);
    expect(resolved).toBe(false);
  });

  it("can be cancelled early to clean up its timer", async () => {
    vi.useFakeTimers();
    const { done, cancel } = waitForTtl(parseDuration("30s"));
    let resolved = false;
    void done.then(() => {
      resolved = true;
    });
    cancel();
    // After cancel the timer is gone, so advancing past the original ttl
    // should not fire the resolution.
    await vi.advanceTimersByTimeAsync(60_000);
    expect(resolved).toBe(false);
  });
});
