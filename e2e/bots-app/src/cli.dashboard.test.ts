import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, expect, it, vi } from "vitest";

import type { ArrivalSpread } from "./resource/arrival";
import type { FpsStats } from "./resource/fps";

const mocks = vi.hoisted(() => ({
  runBotsToCompletion: vi.fn(),
  startDashboardServer: vi.fn(),
  finalizeCalls: [] as Array<[FpsStats, ArrivalSpread | null, number | null]>,
}));

vi.mock("./orchestrator", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./orchestrator")>()),
  runBotsToCompletion: mocks.runBotsToCompletion,
}));

vi.mock("./dashboard", () => ({
  startDashboardServer: mocks.startDashboardServer,
  spawnViteDev: vi.fn(),
  resolveCtlConfig: vi.fn(),
  resolveCtlProxyIdleTimeout: () => ({ value: 600_000, ignored: false }),
}));

vi.mock("./resource/session", () => ({
  ResourceCaptureSession: class {
    readonly label = "dashboard";
    startLocal(): void {}
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

const dirs: string[] = [];

afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
  process.removeAllListeners("SIGTERM");
  process.removeAllListeners("SIGINT");
});

it("hands the self-hosted daemon's receipt no arrival spread (#2294)", async () => {
  const runDir = mkdtempSync(join(tmpdir(), "bots-dash-"));
  dirs.push(runDir);
  mocks.finalizeCalls.length = 0;
  mocks.runBotsToCompletion.mockReset();
  mocks.runBotsToCompletion.mockImplementation(
    async (opts: {
      control?: { onListen?: (a: { port: number; token: string }) => unknown };
      onJoin?: (botId: string, joinedAt: number) => void;
    }) => {
      // Joins must be observed here: an ArrivalTracker that recorded none snapshots null.
      opts.onJoin?.("bot-a", 1_000_000);
      opts.onJoin?.("bot-b", 1_030_000);
      await opts.control?.onListen?.({ port: 45_678, token: "t" });
    },
  );
  mocks.startDashboardServer.mockReset();
  mocks.startDashboardServer.mockResolvedValue({
    port: 45_679,
    server: null,
    close: () => Promise.resolve(),
  });
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
    "dashboard",
    "--no-open",
    "--manifest",
    "",
    "--run-dir",
    runDir,
    "--dist-dir",
    join(runDir, "absent-dist"),
  ];
  try {
    vi.resetModules();
    await import("./cli");
    await vi.waitFor(() => expect(mocks.startDashboardServer.mock.calls.length).toBe(1));
    process.emit("SIGTERM");
    await vi.waitFor(() => expect(mocks.finalizeCalls.length).toBe(1));
  } finally {
    process.argv = argv;
    for (const s of spies) s.mockRestore();
  }
  // The seam itself: the daemon wires no join callback, so nothing downstream can
  // mistake its ad-hoc launches for a ramp.
  expect(mocks.runBotsToCompletion.mock.calls[0][0].onJoin).toBeUndefined();
  const [fps, arrival, joinedBots] = mocks.finalizeCalls[0];
  expect(arrival).toBeNull();
  // Untracked, not zero: a daemon must not banner a no-evidence verdict (#2358).
  expect(joinedBots).toBeNull();
  expect(fps).toBeDefined();
});
