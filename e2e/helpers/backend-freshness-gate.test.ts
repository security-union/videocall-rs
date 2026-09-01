import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Own file: the real-fs tests in backend-freshness.test.ts write actual
// temp files, and a module-level fs mock would break them.
vi.mock("node:fs", () => ({
  readFileSync: vi.fn(),
  statSync: vi.fn(),
}));

import * as fs from "node:fs";
import { assertBackendFreshness } from "./backend-freshness";

const COMPOSE_ONE = `services:
  websocket-api:
    command: >
      "/app/docker/e2e-backend.sh supervise websocket_server"
`;

function withStack(compose: string, stampBody: string | null, ageMs = 0) {
  vi.mocked(fs.readFileSync).mockImplementation(((p: string) => {
    if (String(p).endsWith(".yaml")) return compose;
    if (stampBody === null) throw new Error("ENOENT");
    return stampBody;
  }) as never);
  vi.mocked(fs.statSync).mockReturnValue({ mtimeMs: Date.now() - ageMs } as never);
}

const stamp = (build: string) => `{"service":"websocket_server","build":"${build}","at":"x"}`;

describe("assertBackendFreshness", () => {
  beforeEach(() => {
    // The fs mocks are module-level vi.fn()s; restoreAllMocks does not clear
    // their call history, which the skip-env test asserts on.
    vi.clearAllMocks();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    delete process.env.E2E_SKIP_BACKEND_FRESHNESS;
  });

  it("passes when every supervised backend reports a fresh ok build", async () => {
    withStack(COMPOSE_ONE, stamp("ok"));
    await expect(assertBackendFreshness()).resolves.toBeUndefined();
  });

  it("throws rather than passing vacuously when nothing is supervised", async () => {
    withStack('  websocket-api:\n    command: "cargo run --bin websocket_server"', stamp("ok"));
    await expect(assertBackendFreshness()).rejects.toThrow(/No supervised backend services/);
  });

  it("fails fast on a failed build instead of polling", async () => {
    withStack(COMPOSE_ONE, stamp("failed"));
    await expect(assertBackendFreshness()).rejects.toThrow(/FAILED TO BUILD/);
  });

  it("waits out a stale stamp that the starting stack then refreshes", async () => {
    vi.useFakeTimers();
    withStack(COMPOSE_ONE, stamp("ok"), 3_600_000);
    const settled = assertBackendFreshness().then(
      () => "resolved",
      (e) => String(e),
    );
    await vi.advanceTimersByTimeAsync(10_000);
    withStack(COMPOSE_ONE, stamp("ok"), 0);
    await vi.advanceTimersByTimeAsync(5_000);
    expect(await settled).toBe("resolved");
  });

  it("fails a stamp still stale once the grace window has passed", async () => {
    vi.useFakeTimers();
    withStack(COMPOSE_ONE, stamp("ok"), 3_600_000);
    const settled = assertBackendFreshness().then(
      () => "resolved",
      (e) => String(e),
    );
    await vi.advanceTimersByTimeAsync(70_000);
    expect(await settled).toMatch(/no watcher is supervising it/);
  });

  it("surfaces an unreadable compose file rather than skipping the check", async () => {
    vi.mocked(fs.readFileSync).mockImplementation((() => {
      throw new Error("EACCES");
    }) as never);
    await expect(assertBackendFreshness()).rejects.toThrow(/Cannot read/);
  });

  it("returns without checking anything when the skip env var is set", async () => {
    process.env.E2E_SKIP_BACKEND_FRESHNESS = "1";
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    await expect(assertBackendFreshness()).resolves.toBeUndefined();
    expect(fs.readFileSync).not.toHaveBeenCalled();
    expect(warn.mock.calls[0][0]).toContain("staleness checking is OFF");
  });
});
