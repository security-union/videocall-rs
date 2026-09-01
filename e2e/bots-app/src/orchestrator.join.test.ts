import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({ launchBot: vi.fn() }));
vi.mock("./bot", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./bot")>()),
  launchBot: mocks.launchBot,
}));

import { runBotsToCompletion, type BotTask } from "./orchestrator";
import { SD_SOURCE } from "./posture";

function task(participant: string): BotTask {
  return {
    botId: `00000000-0000-0000-0000-${participant.padStart(12, "0")}`,
    meetingURL: "https://example.com/meeting/X",
    participant,
    displayName: participant,
    headless: true,
    authBackend: "none",
    videoMode: "clock",
    sourceGeometry: SD_SOURCE,
    cameraCycle: null,
    ttl: 10,
  };
}

function fakeBot(): unknown {
  return {
    userHangupDetected: new Promise<void>(() => {}),
    leaveMeeting: vi.fn(async () => {}),
    shutdown: vi.fn(async () => {}),
  };
}

describe("orchestrator join instrumentation (#2294)", () => {
  it("fires onJoin once per bot with the instant it reached the meeting", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => fakeBot());
    const joins: Array<[string, number]> = [];

    const before = Date.now();
    await runBotsToCompletion({
      tasks: [task("alice"), task("bob")],
      onJoin: (botId, joinedAt) => joins.push([botId, joinedAt]),
    });
    const after = Date.now();

    expect(joins.map(([, at]) => at)).toHaveLength(2);
    expect(new Set(joins.map(([id]) => id)).size).toBe(2);
    for (const [, at] of joins) {
      expect(at).toBeGreaterThanOrEqual(before);
      expect(at).toBeLessThanOrEqual(after);
    }
  });

  it("does not fire onJoin for a bot whose launch failed", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => {
      throw new Error("chrome crashed");
    });
    const joins: string[] = [];

    await runBotsToCompletion({ tasks: [task("alice")], onJoin: (botId) => joins.push(botId) });

    expect(joins).toEqual([]);
  });

  it("stamps the join AFTER launchBot resolves, not before the browser comes up", async () => {
    mocks.launchBot.mockReset();
    let launchReturnedAt = 0;
    mocks.launchBot.mockImplementation(async () => {
      await new Promise((r) => setTimeout(r, 20));
      launchReturnedAt = Date.now();
      return fakeBot();
    });
    const joins: number[] = [];

    await runBotsToCompletion({
      tasks: [task("alice")],
      onJoin: (_botId, joinedAt) => joins.push(joinedAt),
    });

    expect(joins).toHaveLength(1);
    expect(joins[0]).toBeGreaterThanOrEqual(launchReturnedAt);
  });

  it("holds joinedAt at the FIRST join across a ctl-driven rejoin", async () => {
    // A netsim change re-enters `launchBot`; GET /bots is the field #2337 aggregates.
    mocks.launchBot.mockReset();
    let launches = 0;
    mocks.launchBot.mockImplementation(async () => {
      launches += 1;
      await new Promise((r) => setTimeout(r, 20));
      return fakeBot();
    });
    const joins: number[] = [];
    const token = "test-token";
    let port = 0;
    let listening!: () => void;
    const listened = new Promise<void>((r) => {
      listening = r;
    });
    const run = runBotsToCompletion({
      tasks: [{ ...task("alice"), ttl: 60_000 }],
      onJoin: (_botId, joinedAt) => joins.push(joinedAt),
      control: {
        port: 0,
        token,
        // Never written: the CLI, not the orchestrator, persists the token file.
        tokenFilePath: join(tmpdir(), "bots-app-join-test-ctl.json"),
        onListen: async (info) => {
          port = info.port;
          listening();
        },
      },
    });
    await listened;

    const api = async (path: string, init?: RequestInit): Promise<Response> =>
      fetch(`http://127.0.0.1:${port}${path}`, {
        ...init,
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      });
    const bots = async (): Promise<Array<{ botId: string; joinedAt: number | null }>> =>
      (
        (await (await api("/bots")).json()) as {
          bots: Array<{ botId: string; joinedAt: number | null }>;
        }
      ).bots;
    const until = async (pred: () => boolean): Promise<void> => {
      for (let i = 0; i < 300 && !pred(); i += 1) await new Promise((r) => setTimeout(r, 10));
      if (!pred()) throw new Error("condition never held");
    };

    await until(() => joins.length === 1);
    const [{ botId, joinedAt: firstJoin }] = await bots();
    expect(firstJoin).toBe(joins[0]);

    await api(`/bots/${botId}/network`, {
      method: "POST",
      body: JSON.stringify({ network: "lossy_mobile" }),
    });
    await until(() => launches === 2 && joins.length === 2);

    expect((await bots())[0].joinedAt).toBe(firstJoin);
    expect(joins[1]).toBe(firstJoin);

    await api(`/bots/${botId}/leave`, { method: "POST" });
    // With a control server attached the run parks until a shutdown signal, so
    // closing it is what lets the listening socket go.
    process.emit("SIGTERM");
    await run;
  }, 20_000);
});
