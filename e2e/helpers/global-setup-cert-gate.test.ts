import { afterEach, describe, expect, it, vi } from "vitest";

/**
 * Guards the CALL SITE, not the helper. `assertDevCertHashesPresent` has its own
 * unit tests, but deleting the one line that invokes it from `global-setup.ts`
 * left those green — the whole behavioural change was revertible without a
 * failing test (#2159).
 */
describe("globalSetup dev-cert gate", () => {
  afterEach(() => {
    vi.resetModules();
    vi.doUnmock("./wait-for-services");
    vi.doUnmock("./auth-context");
  });

  async function runGlobalSetup(assertImpl: () => void): Promise<void> {
    // Stub the network waits so this is a pure wiring test; the browser warmup
    // is skipped by DIOXUS_UI_URL never being reached before the assert fires.
    vi.doMock("./wait-for-services", () => ({ waitForServices: vi.fn(async () => undefined) }));
    vi.doMock("./auth-context", () => ({ assertDevCertHashesPresent: assertImpl }));
    const mod = await import("../global-setup");
    await mod.default();
  }

  it("fails the run when the dev cert hash is absent", async () => {
    const boom = () => {
      throw new Error("WT dev cert hash missing (stub)");
    };
    await expect(runGlobalSetup(boom)).rejects.toThrow(/WT dev cert hash missing/);
  });

  it("calls the assert before waiting on services", async () => {
    // Ordering is the point: the operator must get the actionable error in about a
    // second, not after the service waits and the wasm warmup.
    const order: string[] = [];
    vi.doMock("./wait-for-services", () => ({
      waitForServices: vi.fn(async () => {
        order.push("waitForServices");
      }),
    }));
    vi.doMock("./auth-context", () => ({
      assertDevCertHashesPresent: () => {
        order.push("assert");
        throw new Error("WT dev cert hash missing (stub)");
      },
    }));
    const mod = await import("../global-setup");
    await expect(mod.default()).rejects.toThrow();
    expect(order).toEqual(["assert"]);
  });
});
