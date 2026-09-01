import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import type { ArrivalSpread } from "./resource/arrival";
import type { FpsStats } from "./resource/fps";

const mocks = vi.hoisted(() => ({
  runBotsToCompletion: vi.fn(),
  finalizeCalls: [] as Array<[FpsStats, ArrivalSpread | null, number | null]>,
  order: [] as string[],
}));

vi.mock("./orchestrator", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./orchestrator")>()),
  runBotsToCompletion: mocks.runBotsToCompletion,
}));

vi.mock("./resource/session", () => ({
  ResourceCaptureSession: class {
    readonly label = "cli-run-test";
    startLocal(): void {
      mocks.order.push("startLocal");
    }
    finalize(
      fps: FpsStats,
      arrival: ArrivalSpread | null,
      joinedBots: number | null,
    ): Promise<null> {
      mocks.finalizeCalls.push([fps, arrival, joinedBots]);
      return Promise.resolve(null);
    }
  },
  RemoteResourceManager: class {},
}));

const FIRST_JOIN = 1_700_000_000_000;
const LAST_JOIN = FIRST_JOIN + 61_000;

const dirs: string[] = [];

afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});

async function runBots(joins: Array<[string, number]>): Promise<void> {
  const assetsDir = mkdtempSync(join(tmpdir(), "bots-run-arr-"));
  dirs.push(assetsDir);
  mocks.finalizeCalls.length = 0;
  mocks.order.length = 0;
  mocks.runBotsToCompletion.mockReset();
  mocks.runBotsToCompletion.mockImplementation(
    async (opts: { onJoin?: (botId: string, joinedAt: number) => void }) => {
      mocks.order.push("runBotsToCompletion");
      for (const [botId, at] of joins) opts.onJoin?.(botId, at);
    },
  );
  const spies = [
    vi.spyOn(process, "exit").mockImplementation((() => undefined as never) as never),
    vi.spyOn(console, "log").mockImplementation(() => {}),
    vi.spyOn(console, "warn").mockImplementation(() => {}),
    vi.spyOn(console, "error").mockImplementation(() => {}),
  ];
  const argv = process.argv;
  process.argv = [
    "node",
    "cli.ts",
    "run",
    "--manifest",
    "",
    "--meeting-url",
    "http://127.0.0.1:3001/meeting/ArrivalTest",
    "--assets-dir",
    assetsDir,
    "--participant",
    "alice",
    "--participant",
    "bob",
  ];
  try {
    vi.resetModules();
    await import("./cli");
    await vi.waitFor(() => expect(mocks.finalizeCalls.length).toBe(1));
  } finally {
    process.argv = argv;
    for (const s of spies) s.mockRestore();
  }
}

async function runTwoBots(): Promise<void> {
  await runBots([
    ["bot-1", FIRST_JOIN],
    ["bot-2", LAST_JOIN],
  ]);
}

describe("bots-app run — arrival spread reaches the receipt (#2294)", () => {
  it("hands finalize the spread built from the joins the orchestrator reported", async () => {
    await runTwoBots();
    const [fps, arrival, joinedBots] = mocks.finalizeCalls[0];
    expect(fps).toBeDefined();
    expect(arrival).toEqual({
      count: 2,
      firstJoinMs: FIRST_JOIN,
      lastJoinMs: LAST_JOIN,
      spreadMs: 61_000,
    });
    expect(joinedBots).toBe(2);
  });

  it("hands finalize a zero join count when no bot joined (#2358)", async () => {
    await runBots([]);
    const [, arrival, joinedBots] = mocks.finalizeCalls[0];
    expect(arrival).toBeNull();
    expect(joinedBots).toBe(0);
  });

  it("starts the sampler before the first launch, which the receipt's note asserts", async () => {
    await runTwoBots();
    expect(mocks.order).toEqual(["startLocal", "runBotsToCompletion"]);
  });
});
