import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { afterEach, describe, expect, it, vi } from "vitest";

import type { OrchestratorControlSurface } from "./control/server";

const mocks = vi.hoisted(() => ({
  launchBot: vi.fn(),
  surface: null as OrchestratorControlSurface | null,
  serverClose: vi.fn(async () => {}),
}));
vi.mock("./bot", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./bot")>()),
  launchBot: mocks.launchBot,
}));
vi.mock("./control/server", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./control/server")>()),
  startControlServer: vi.fn(async (opts: { surface: OrchestratorControlSurface }) => {
    mocks.surface = opts.surface;
    return { port: 0, close: mocks.serverClose, closeSsoRecaptureSessions: async () => {} };
  }),
}));

import { newRegistryEntry, type BotRegistryEntry } from "./control/registry";
import { JoinRejectedError } from "./meeting-join";
import {
  runBotsToCompletion,
  toggleMicrophone,
  type BotTask,
  type RunOptions,
} from "./orchestrator";
import { SD_SOURCE } from "./posture";
import type { RemoteResourceManager } from "./resource/session";

const FORGED = "[orchestrator] FORGED-BY-ERROR-MESSAGE";
const PAYLOAD = `boom\n${FORGED}`;

function task(overrides: Partial<BotTask> = {}): BotTask {
  return {
    botId: "00000000-0000-0000-0000-00000000a11c",
    meetingURL: "https://example.com/meeting/X",
    participant: "alice",
    displayName: "alice",
    headless: true,
    authBackend: "none",
    videoMode: "clock",
    sourceGeometry: SD_SOURCE,
    cameraCycle: null,
    ttl: 10,
    ...overrides,
  };
}

function fakeBot(overrides: Record<string, unknown> = {}): unknown {
  return {
    userHangupDetected: new Promise<void>(() => {}),
    crashDetected: new Promise<string>(() => {}),
    leaveMeeting: vi.fn(async () => {}),
    shutdown: vi.fn(async () => {}),
    ...overrides,
  };
}

const control = (onListen?: () => Promise<void>): RunOptions["control"] => ({
  port: 0,
  token: "t",
  tokenFilePath: "/nonexistent/ctl.json",
  onListen,
});

/** Every line the run wrote, split the way a pod-log collector splits. */
async function linesFrom(run: () => Promise<void>): Promise<string[]> {
  const written: string[] = [];
  const sink = (...args: unknown[]): void => {
    written.push(args.map(String).join(" "));
  };
  const spies = (["log", "warn", "error"] as const).map((m) =>
    vi.spyOn(console, m).mockImplementation(sink),
  );
  try {
    await run();
  } finally {
    for (const s of spies) s.mockRestore();
  }
  return written.join("\n").split(/[\r\n]/);
}

function expectCollapsed(lines: string[], site: string): void {
  expect(lines.filter((l) => l.startsWith(FORGED))).toEqual([]);
  expect(lines.filter((l) => l.includes(site) && l.includes(FORGED))).toHaveLength(1);
}

afterEach(() => {
  mocks.launchBot.mockReset();
  mocks.serverClose.mockReset();
  mocks.surface = null;
});

describe("orchestrator multi-argument writers compose before sanitising (#2484)", () => {
  it("launch failed", async () => {
    mocks.launchBot.mockRejectedValue(new Error(PAYLOAD));
    const lines = await linesFrom(() => runBotsToCompletion({ tasks: [task()] }));
    expectCollapsed(lines, "launch failed:");
  });

  it("leaveMeeting failed", async () => {
    mocks.launchBot.mockResolvedValue(
      fakeBot({ leaveMeeting: vi.fn().mockRejectedValue(new Error(PAYLOAD)) }),
    );
    const lines = await linesFrom(() => runBotsToCompletion({ tasks: [task()] }));
    expectCollapsed(lines, "leaveMeeting failed:");
  });

  it("bot threw", async () => {
    mocks.launchBot.mockResolvedValue(
      fakeBot({ shutdown: vi.fn().mockRejectedValue(new Error(PAYLOAD)) }),
    );
    const lines = await linesFrom(() => runBotsToCompletion({ tasks: [task()] }));
    expectCollapsed(lines, "threw:");
  });

  it("remote resource finalize failed", async () => {
    mocks.launchBot.mockResolvedValue(fakeBot());
    const remoteResource = {
      finalizeAll: vi.fn().mockRejectedValue(new Error(PAYLOAD)),
      ensureForHost: vi.fn(async () => {}),
    } as unknown as RemoteResourceManager;
    const lines = await linesFrom(() => runBotsToCompletion({ tasks: [task()], remoteResource }));
    expectCollapsed(lines, "remote resource finalize failed:");
  });

  it("control server close failed", async () => {
    mocks.serverClose.mockRejectedValue(new Error(PAYLOAD));
    const lines = await linesFrom(() =>
      runBotsToCompletion({
        tasks: [],
        control: control(async () => {
          process.emit("SIGTERM");
        }),
      }),
    );
    expectCollapsed(lines, "control server close failed:");
  });

  it("leaveMeeting (rejoin) failed", async () => {
    mocks.launchBot
      .mockResolvedValueOnce(
        fakeBot({ leaveMeeting: vi.fn().mockRejectedValue(new Error(PAYLOAD)) }),
      )
      .mockResolvedValue(fakeBot());
    const until = async (pred: () => boolean): Promise<void> => {
      for (let i = 0; i < 300 && !pred(); i += 1) await new Promise((r) => setTimeout(r, 5));
      if (!pred()) throw new Error("condition never held");
    };
    const lines = await linesFrom(async () => {
      const run = runBotsToCompletion({
        tasks: [task({ ttl: 60_000 })],
        control: control(),
      });
      const bot = task().botId;
      await until(() => mocks.surface?.getRegistry().get(bot)?.status === "in-meeting");
      await mocks.surface!.changeNetwork(bot, "lossy_mobile");
      await until(() => mocks.launchBot.mock.calls.length === 2);
      process.emit("SIGTERM");
      await run;
    });
    expectCollapsed(lines, "leaveMeeting (rejoin) failed:");
  });
});

describe("orchestrator single-argument writers (#2484)", () => {
  it("join rejected", async () => {
    mocks.launchBot.mockRejectedValue(new JoinRejectedError("rejected", PAYLOAD));
    const lines = await linesFrom(() => runBotsToCompletion({ tasks: [task()] }));
    expectCollapsed(lines, "join rejected:");
  });

  it("dashboard manifest not found", async () => {
    const lines = await linesFrom(() =>
      runBotsToCompletion({
        tasks: [],
        control: {
          ...control(async () => {
            process.emit("SIGTERM");
          })!,
          manifestPath: `/nonexistent/${PAYLOAD}`,
        },
      }),
    );
    expectCollapsed(lines, "dashboard manifest not found");
  });

  it("ctl click failed", async () => {
    const btn = {
      first: () => btn,
      hover: async () => {},
      isVisible: async () => true,
      click: async () => {
        throw new Error(PAYLOAD);
      },
    };
    const entry: BotRegistryEntry = newRegistryEntry(task());
    entry.handle = { page: { locator: () => btn } } as unknown as BotRegistryEntry["handle"];
    const lines = await linesFrom(() => toggleMicrophone(entry, true));
    expectCollapsed(lines, "click failed:");
  });
});

describe("in-pod writer routing (#2484)", () => {
  const files = ["./orchestrator.ts", "./control/server.ts", "./control/ssh-launcher.ts"];

  it.each(files)("%s builds no bracket-tagged literal outside taggedLine", (rel) => {
    const code = readFileSync(fileURLToPath(new URL(rel, import.meta.url)), "utf8")
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .split("\n")
      .filter((l) => !l.trimStart().startsWith("//"));
    const offending = code.filter((l) => /["'`]\[(?:orchestrator\]|control\]|\$\{)/.test(l));
    expect(offending).toEqual([]);
    expect(code.join("\n")).toContain("taggedLine(");
  });
});
