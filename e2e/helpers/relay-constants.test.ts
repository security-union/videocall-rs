import { describe, expect, it } from "vitest";

import { relayReconnectGraceMs, rustDurationMs } from "./relay-constants";

describe("relay constants read from actix-api source", () => {
  it("reads RECONNECT_GRACE_PERIOD from the relay", () => {
    expect(relayReconnectGraceMs()).toBeGreaterThan(0);
  });

  it("converts from_secs and from_millis to milliseconds", () => {
    const src = [
      "pub const A: Duration = Duration::from_secs(3);",
      "pub const B: Duration = Duration::from_millis(2_500);",
    ].join("\n");
    expect(rustDurationMs(src, "A")).toBe(3000);
    expect(rustDurationMs(src, "B")).toBe(2500);
  });

  it("throws instead of guessing when the constant is absent or not a literal", () => {
    expect(() => rustDurationMs("", "A")).toThrow(/found 0/);
    expect(() =>
      rustDurationMs("pub const A: Duration = Duration::from_secs(N * 2);", "A"),
    ).toThrow(/found 0/);
  });
});
