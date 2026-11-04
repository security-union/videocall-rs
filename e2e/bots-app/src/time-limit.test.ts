import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { TimeLimitExceededError, withTimeLimit } from "./time-limit";

const PENDING = Symbol("pending");
const NEVER = (): Promise<never> => new Promise<never>(() => {});

describe("withTimeLimit", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("rejects a stalled call at the limit and not a millisecond before", async () => {
    const settled = withTimeLimit("page.evaluate", 500, NEVER).then(
      (v) => v,
      (e: unknown) => e,
    );
    const peek = (): Promise<unknown> => Promise.race([settled, Promise.resolve(PENDING)]);
    await vi.advanceTimersByTimeAsync(499);
    expect(await peek()).toBe(PENDING);
    await vi.advanceTimersByTimeAsync(1);
    const err = await peek();
    expect(err).toBeInstanceOf(TimeLimitExceededError);
    expect(err).toMatchObject({ what: "page.evaluate", limitMs: 500 });
  });

  it("passes a value or a rejection through and clears its timer", async () => {
    await expect(withTimeLimit("a", 500, async () => 7)).resolves.toBe(7);
    const boom = new Error("boom");
    await expect(withTimeLimit("b", 500, () => Promise.reject(boom))).rejects.toBe(boom);
    await expect(
      withTimeLimit("c", 500, () => {
        throw boom;
      }),
    ).rejects.toBe(boom);
    expect(vi.getTimerCount()).toBe(0);
  });
});
