import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import type { BotTask } from "./orchestrator";
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

const registered = (botId: string): BotTask => ({ botId }) as BotTask;

interface DaemonSinks {
  onRegister?: (task: BotTask) => void;
  onJoin?: (botId: string, joinedAt: number) => void;
}

async function runDaemon(
  drive: (sinks: DaemonSinks) => void,
): Promise<[FpsStats, ArrivalSpread | null, number | null]> {
  const runDir = mkdtempSync(join(tmpdir(), "bots-dash-"));
  dirs.push(runDir);
  mocks.finalizeCalls.length = 0;
  mocks.runBotsToCompletion.mockReset();
  mocks.runBotsToCompletion.mockImplementation(
    async (
      opts: DaemonSinks & {
        control?: { onListen?: (a: { port: number; token: string }) => unknown };
      },
    ) => {
      drive(opts);
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
    await vi.waitFor(() => expect(mocks.startDashboardServer.mock.calls.length).toBe(1), {
      timeout: 10_000,
    });
    process.emit("SIGTERM");
    await vi.waitFor(() => expect(mocks.finalizeCalls.length).toBe(1), { timeout: 10_000 });
  } finally {
    process.argv = argv;
    for (const s of spies) s.mockRestore();
  }
  return mocks.finalizeCalls[0];
}

describe("bots-app dashboard — self-hosted daemon receipt", () => {
  it("hands finalize joins but no arrival spread (#2294)", async () => {
    const [fps, arrival, joinedBots] = await runDaemon(({ onRegister, onJoin }) => {
      onRegister?.(registered("bot-a"));
      onRegister?.(registered("bot-b"));
      onJoin?.("bot-a", 1_000_000);
      onJoin?.("bot-b", 1_030_000);
    });
    expect(fps).toBeDefined();
    expect(arrival).toBeNull();
    expect(joinedBots).toBe(2);
  });

  it("hands finalize a zero join count once a single launched bot never joined (#2407)", async () => {
    const [, arrival, joinedBots] = await runDaemon(({ onRegister }) =>
      onRegister?.(registered("bot-a")),
    );
    expect(arrival).toBeNull();
    expect(joinedBots).toBe(0);
  });

  it("leaves joins untracked for an idle daemon that launched nothing (#2407)", async () => {
    const [, , joinedBots] = await runDaemon(() => {});
    expect(joinedBots).toBeNull();
  });
});
