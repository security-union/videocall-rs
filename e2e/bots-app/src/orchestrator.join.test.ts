import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({ launchBot: vi.fn() }));
vi.mock("./bot", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./bot")>()),
  launchBot: mocks.launchBot,
}));

import { classifyHealthz } from "./control/conduct";
import { JoinRejectedError } from "./meeting-join";
import { type BotRunOptions } from "./bot";
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
    crashDetected: new Promise<string>(() => {}),
    leaveMeeting: vi.fn(async () => {}),
    shutdown: vi.fn(async () => {}),
    sessionUserId: () => null,
  };
}

describe("orchestrator diagnostics-packets instrumentation (#2970)", () => {
  it("tags each bot's console observation with its id", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async (opts: BotRunOptions) => {
      opts.onDiagPackets?.({
        state: "DISABLED",
        source: opts.participant === "a" ? "config" : "url",
      });
      return fakeBot();
    });
    const seen: string[] = [];
    await runBotsToCompletion({
      tasks: [task("a"), task("b")],
      onDiagPackets: (id, obs) => seen.push(`${id.slice(-1)}:${obs.state}:${obs.source}`),
    });
    expect(seen.sort()).toEqual(["a:DISABLED:config", "b:DISABLED:url"]);
  });
});

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

  it("hands onJoin the join media and brackets each task with onRegister / onFinish (#2914)", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => ({
      ...(fakeBot() as object),
      joinMedia: { camera: false, mic: true },
      sessionUserId: () => "alice@labs.example",
      decodeBudgetReadback: "all",
    }));
    const events: string[] = [];
    await runBotsToCompletion({
      tasks: [{ ...task("alice"), decodeBudget: "off" }],
      onRegister: (t) => events.push(`register:${t.participant}`),
      onJoin: (_id, _at, media, userId, readback) =>
        events.push(`join:${JSON.stringify(media)}:${userId}:${readback}`),
      onFinish: (_id, at, reason) => events.push(`finish:${reason}:${at > 0}`),
    });
    expect(events).toEqual([
      "register:alice",
      'join:{"camera":false,"mic":true}:alice@labs.example:all',
      "finish:ttl-expired:true",
    ]);
    expect(mocks.launchBot.mock.calls[0][0].decodeBudget).toBe("off");
  });

  it("fires onFinish with the failure reason for a bot that never joined (#2914)", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => {
      throw new Error("chrome crashed");
    });
    const finished: Array<string | undefined> = [];
    await runBotsToCompletion({
      tasks: [task("alice")],
      onFinish: (_id, _at, reason) => finished.push(reason),
    });
    expect(finished).toEqual(["launch-error"]);
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

  it("fires onRejoin for a ctl network change even when the relaunch fails (#2914)", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot
      .mockImplementationOnce(async () => fakeBot())
      .mockRejectedValueOnce(new Error("relaunch failed"));
    const events: string[] = [];
    const token = "test-token";
    let port = 0;
    let listening!: () => void;
    const listened = new Promise<void>((r) => {
      listening = r;
    });
    const run = runBotsToCompletion({
      tasks: [{ ...task("alice"), ttl: 60_000 }],
      onJoin: () => events.push("join"),
      onRejoin: (_id, network) => events.push(`rejoin:${network}`),
      onFinish: () => events.push("finish"),
      control: {
        port: 0,
        token,
        tokenFilePath: join(tmpdir(), "bots-app-join-test-ctl.json"),
        onListen: async (info) => {
          port = info.port;
          listening();
        },
      },
    });
    await listened;
    const until = async (pred: () => boolean): Promise<void> => {
      for (let i = 0; i < 300 && !pred(); i += 1) await new Promise((r) => setTimeout(r, 10));
      if (!pred()) throw new Error("condition never held");
    };
    await until(() => events.includes("join"));
    await fetch(`http://127.0.0.1:${port}/bots/${task("alice").botId}/network`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify({ network: "lossy_mobile" }),
    });
    await until(() => events.includes("finish"));
    expect(events).toEqual(["join", "rejoin:lossy_mobile", "finish"]);
    process.emit("SIGTERM");
    await run;
  }, 20_000);
});

describe("orchestrator onNetem (#2358)", () => {
  it("reports a control-API netem action so the participant record can re-stamp", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => fakeBot());
    const seen: Array<[string, string, boolean]> = [];
    let tcFails = false;
    let port = 0;
    let listening!: () => void;
    const listened = new Promise<void>((r) => {
      listening = r;
    });
    const run = runBotsToCompletion({
      tasks: [{ ...task("alice"), ttl: 60_000 }],
      onNetem: (action, _at, error) => seen.push([action.op, action.label, error !== undefined]),
      control: {
        port: 0,
        token: "test-token",
        tokenFilePath: join(tmpdir(), "bots-app-onnetem-test-ctl.json"),
        netem: {
          exec: async (_file, args) => {
            if (tcFails) throw new Error("tc: RTNETLINK answers: Operation not permitted");
            const shaped = args[1] === "show" ? "qdisc netem 8001: root\nqdisc ingress ffff:" : "";
            return { stdout: shaped, stderr: "" };
          },
        },
        onListen: async (info) => {
          port = info.port;
          listening();
        },
      },
    });
    await listened;
    const res = await fetch(`http://127.0.0.1:${port}/netem`, {
      method: "POST",
      headers: { authorization: "Bearer test-token", "content-type": "application/json" },
      body: JSON.stringify({ profile: "good_4g" }),
    });
    tcFails = true;
    const failed = await fetch(`http://127.0.0.1:${port}/netem`, {
      method: "DELETE",
      headers: { authorization: "Bearer test-token" },
    });
    process.emit("SIGTERM");
    await run;
    expect(res.status).toBe(200);
    expect(failed.ok).toBe(false);
    expect(seen).toEqual([
      ["shape", "good_4g", false],
      ["clear", "clear", true],
    ]);
  }, 20_000);
});

describe("orchestrator register instrumentation (#2407)", () => {
  it("fires onRegister for a local bot even when it never joins", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => {
      throw new Error("chrome crashed");
    });
    const registered: string[] = [];

    await runBotsToCompletion({
      tasks: [task("alice"), task("bob")],
      onRegister: (t) => registered.push(t.botId),
    });

    expect(registered).toEqual([task("alice").botId, task("bob").botId]);
  });

  it("fires onRegister for a bot launched through the control server's /launch", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => {
      throw new Error("chrome crashed");
    });
    const registered: string[] = [];
    const token = "test-token";
    let port = 0;
    let listening!: () => void;
    const listened = new Promise<void>((r) => {
      listening = r;
    });
    const run = runBotsToCompletion({
      tasks: [],
      onRegister: (t) => registered.push(t.botId),
      control: {
        port: 0,
        token,
        tokenFilePath: join(tmpdir(), "bots-app-register-test-ctl.json"),
        onListen: async (info) => {
          port = info.port;
          listening();
        },
      },
    });
    await listened;

    const res = await fetch(`http://127.0.0.1:${port}/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify({
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "none",
        videoMode: "clock",
      }),
    });
    expect(res.status).toBe(201);
    const { botId } = (await res.json()) as { botId: string };

    expect(registered).toEqual([botId]);
    process.emit("SIGTERM");
    await run;
  }, 20_000);
});

describe("orchestrator /healthz in-meeting readiness (#2917)", () => {
  async function healthzAfter(
    launch: (opts: { participant: string }) => Promise<unknown>,
    settled: (body: Record<string, number>) => boolean,
    tasks: BotTask[] = [{ ...task("alice"), ttl: 60_000 }],
    during: (port: number) => Promise<void> = async () => {},
  ): Promise<Record<string, number>> {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(launch);
    let port = 0;
    let listening!: () => void;
    const listened = new Promise<void>((r) => {
      listening = r;
    });
    const run = runBotsToCompletion({
      tasks,
      control: {
        port: 0,
        token: "test-token",
        tokenFilePath: join(tmpdir(), "bots-app-healthz-test-ctl.json"),
        onListen: async (info) => {
          port = info.port;
          listening();
        },
      },
    });
    await listened;
    let body: Record<string, number> = {};
    for (let i = 0; i < 300; i += 1) {
      const res = await fetch(`http://127.0.0.1:${port}/healthz`);
      body = (await res.json()) as Record<string, number>;
      if (settled(body)) break;
      await new Promise((r) => setTimeout(r, 10));
    }
    await during(port);
    process.emit("SIGTERM");
    await run;
    return body;
  }

  it("keeps the pod's expected bot counted after its join is rejected", async () => {
    const body = await healthzAfter(
      async () => {
        throw new JoinRejectedError("rejected", "host denied");
      },
      (b) => b.bots === 0 && b.expected === 1,
    );
    expect(body).toMatchObject({ bots: 0, inMeeting: 0, pending: 0, expected: 1 });
  }, 20_000);

  it("counts the bot in-meeting only once launchBot has resolved", async () => {
    const body = await healthzAfter(
      async () => fakeBot(),
      (b) => b.inMeeting === 1,
    );
    expect(body).toMatchObject({ bots: 1, inMeeting: 1, pending: 0, expected: 1 });
  }, 20_000);

  it("releases a bot ended on purpose from expected, so the pod stays ready (#2917)", async () => {
    const body = await healthzAfter(
      async () => fakeBot(),
      (b) => b.bots === 1 && b.inMeeting === 1,
      [
        { ...task("alice"), ttl: 30 },
        { ...task("bob"), ttl: 60_000 },
      ],
    );
    expect(body).toMatchObject({ bots: 1, inMeeting: 1, expected: 1 });
    expect(classifyHealthz(body).state).toBe("in-meeting");
  }, 20_000);

  it("drops a bot whose browser crashed after joining out of inMeeting, not expected (#2917)", async () => {
    const body = await healthzAfter(
      async () => ({
        ...(fakeBot() as object),
        crashDetected: new Promise<string>((r) => setTimeout(() => r("page crashed"), 20)),
      }),
      (b) => b.bots === 0,
    );
    expect(body).toMatchObject({ bots: 0, inMeeting: 0, expected: 1 });
    expect(classifyHealthz(body).state).toBe("not-joined");
  }, 20_000);

  it("ends a crashed bot as failed/browser-crash and answers 409 to a later leave or mute (#2917)", async () => {
    let snap: Record<string, unknown> | undefined;
    let leave = 0;
    let mute = 0;
    await healthzAfter(
      async () => ({
        ...(fakeBot() as object),
        crashDetected: new Promise<string>((r) => setTimeout(() => r("page crashed"), 20)),
      }),
      (b) => b.bots === 0,
      undefined,
      async (port) => {
        const auth = { authorization: "Bearer test-token" };
        for (let i = 0; i < 300 && snap?.finishReason === undefined; i += 1) {
          const res = await fetch(`http://127.0.0.1:${port}/bots`, { headers: auth });
          snap = ((await res.json()) as { bots: Array<Record<string, unknown>> }).bots[0];
          if (snap?.finishReason === undefined) await new Promise((r) => setTimeout(r, 10));
        }
        const res = await fetch(`http://127.0.0.1:${port}/bots/${task("alice").botId}/leave`, {
          method: "POST",
          headers: auth,
        });
        leave = res.status;
        const m = await fetch(`http://127.0.0.1:${port}/bots/${task("alice").botId}/mute`, {
          method: "POST",
          headers: { ...auth, "content-type": "application/json" },
          body: JSON.stringify({ mic: true }),
        });
        mute = m.status;
      },
    );
    expect(snap).toMatchObject({
      status: "failed",
      finishReason: "browser-crash",
      lastError: "page crashed",
      finishedAt: expect.any(Number),
    });
    expect(leave).toBe(409);
    expect(mute).toBe(409);
  }, 20_000);

  it("stops counting a crashed bot, and refuses leave, while its shutdown is still running (#2917)", async () => {
    let release = (): void => {};
    let shuttingDown = false;
    let leave = 0;
    const body = await healthzAfter(
      async () => ({
        ...(fakeBot() as object),
        crashDetected: new Promise<string>((r) => setTimeout(() => r("page crashed"), 20)),
        shutdown: () => {
          shuttingDown = true;
          return new Promise<void>((r) => {
            release = r;
          });
        },
      }),
      (b) => shuttingDown && b.inMeeting === 0,
      undefined,
      async (port) => {
        const res = await fetch(`http://127.0.0.1:${port}/bots/${task("alice").botId}/leave`, {
          method: "POST",
          headers: { authorization: "Bearer test-token" },
        });
        leave = res.status;
        release();
      },
    );
    expect(shuttingDown).toBe(true);
    expect(body).toMatchObject({ inMeeting: 0, expected: 1 });
    expect(leave).toBe(409);
  }, 20_000);

  it("an operator dropping a failed entry releases it, so the pod's joined bot reads ready (#2917)", async () => {
    const after: Array<Record<string, number>> = [];
    const alice = task("alice");
    await healthzAfter(
      async (opts: { participant: string }) => {
        if (opts.participant !== "bob") throw new JoinRejectedError("rejected", "host denied");
        return fakeBot();
      },
      (b) => b.bots === 1 && b.inMeeting === 1,
      [
        { ...alice, ttl: 60_000 },
        { ...task("bob"), ttl: 60_000 },
        { ...task("carol"), ttl: 60_000 },
      ],
      async (port) => {
        const get = async () =>
          (await (await fetch(`http://127.0.0.1:${port}/healthz`)).json()) as Record<
            string,
            number
          >;
        after.push(await get());
        await fetch(`http://127.0.0.1:${port}/bots/${alice.botId}`, {
          method: "DELETE",
          headers: { authorization: "Bearer test-token" },
        });
        after.push(await get());
        await fetch(`http://127.0.0.1:${port}/bots/terminated`, {
          method: "DELETE",
          headers: { authorization: "Bearer test-token" },
        });
        after.push(await get());
      },
    );
    expect(after.map((b) => [b.inMeeting, b.expected])).toEqual([
      [1, 3],
      [1, 2],
      [1, 1],
    ]);
    expect(classifyHealthz(after[2]).state).toBe("in-meeting");
  }, 20_000);

  it.each([
    ["ctl-leave", "POST", "leave"],
    ["ctl-kill", "DELETE", ""],
  ] as const)(
    "%s releases a joined bot from expected exactly once (#2917)",
    async (_r, method, route) => {
      const seen: Array<Record<string, number>> = [];
      const alice = task("alice");
      await healthzAfter(
        async () => fakeBot(),
        (b) => b.inMeeting === 1,
        undefined,
        async (port) => {
          const auth = { authorization: "Bearer test-token" };
          const url = `http://127.0.0.1:${port}/bots/${alice.botId}${route ? `/${route}` : ""}`;
          const get = async () =>
            (await (await fetch(`http://127.0.0.1:${port}/healthz`)).json()) as Record<
              string,
              number
            >;
          await fetch(url, { method, headers: auth });
          for (let i = 0; i < 300 && (await get()).bots !== 0; i += 1) {
            await new Promise((r) => setTimeout(r, 10));
          }
          seen.push(await get());
          await fetch(`http://127.0.0.1:${port}/bots/${alice.botId}`, {
            method: "DELETE",
            headers: auth,
          });
          seen.push(await get());
        },
      );
      expect(seen.map((b) => [b.bots, b.expected])).toEqual([
        [0, 0],
        [0, 0],
      ]);
    },
    20_000,
  );

  it("answers 409 to a meeting control or kill on a bot that has not joined (#2386)", async () => {
    const statuses: number[] = [];
    let release = (): void => {};
    await healthzAfter(
      () =>
        new Promise((_, reject) => {
          release = () => reject(new Error("released"));
        }),
      (b) => b.pending === 1,
      undefined,
      async (port) => {
        for (const [route, body] of [
          ["mute", { mic: true }],
          ["video", { camera: true }],
          ["share", { share: true }],
          ["leave", {}],
        ] as const) {
          const res = await fetch(`http://127.0.0.1:${port}/bots/${task("alice").botId}/${route}`, {
            method: "POST",
            headers: { authorization: "Bearer test-token", "content-type": "application/json" },
            body: JSON.stringify(body),
          });
          statuses.push(res.status);
        }
        const kill = await fetch(`http://127.0.0.1:${port}/bots/${task("alice").botId}`, {
          method: "DELETE",
          headers: { authorization: "Bearer test-token" },
        });
        statuses.push(kill.status);
        release();
      },
    );
    expect(statuses).toEqual([409, 409, 409, 409, 409]);
  }, 20_000);
});

describe("orchestrator diagnostics-packets surfacing (#2970)", () => {
  async function withCtl(
    tasks: BotTask[],
    extra: Partial<Parameters<typeof runBotsToCompletion>[0] & object>,
    body: (api: (path: string, init?: RequestInit) => Promise<Response>) => Promise<void>,
  ): Promise<void> {
    const token = "test-token";
    let port = 0;
    let listening!: () => void;
    const listened = new Promise<void>((r) => {
      listening = r;
    });
    const run = runBotsToCompletion({
      tasks,
      ...extra,
      control: {
        port: 0,
        token,
        tokenFilePath: join(tmpdir(), "bots-app-diag-test-ctl.json"),
        onListen: async (info) => {
          port = info.port;
          listening();
        },
      },
    });
    await listened;
    try {
      await body((path, init) =>
        fetch(`http://127.0.0.1:${port}${path}`, {
          ...init,
          headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        }),
      );
    } finally {
      process.emit("SIGTERM");
      await run;
    }
  }

  const until = async (pred: () => boolean | Promise<boolean>): Promise<void> => {
    for (let i = 0; i < 300 && !(await pred()); i += 1) await new Promise((r) => setTimeout(r, 10));
    if (!(await pred())) throw new Error("condition never held");
  };

  it("re-applies diagPackets on a ctl rejoin", async () => {
    const applied: Array<string | null | undefined> = [];
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async (opts: BotRunOptions) => {
      applied.push(opts.diagPackets);
      return fakeBot();
    });
    const t = { ...task("alice"), ttl: 60_000, diagPackets: "off" as const };
    await withCtl([t], {}, async (api) => {
      await until(() => applied.length === 1);
      await api(`/bots/${t.botId}/network`, {
        method: "POST",
        body: JSON.stringify({ network: "lossy_mobile" }),
      });
      await until(() => applied.length === 2);
    });
    expect(applied).toEqual(["off", "off"]);
  }, 20_000);

  it.each(["leave", "rejoin", "crash"] as const)(
    "a %s before the window ends cancels that launch's missing-line warning",
    async (action) => {
      const VERIFY_MS = 500;
      let launches = 0;
      mocks.launchBot.mockReset();
      mocks.launchBot.mockImplementation(async (opts: BotRunOptions) => {
        launches += 1;
        if (launches > 1) opts.onDiagPackets?.({ state: "DISABLED", source: "config" });
        if (action !== "crash") return fakeBot();
        return {
          ...(fakeBot() as object),
          crashDetected: new Promise<string>((r) => setTimeout(() => r("page crashed"), 20)),
        };
      });
      const t = { ...task("alice"), ttl: 60_000, diagPackets: "off" as const };
      const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
      let lines: string[] = [];
      try {
        await withCtl([t], { diagPacketsVerifyMs: VERIFY_MS }, async (api) => {
          await until(() => launches === 1);
          if (action !== "crash") {
            await api(
              `/bots/${t.botId}/${action === "leave" ? "leave" : "network"}`,
              action === "leave"
                ? { method: "POST" }
                : { method: "POST", body: JSON.stringify({ network: "lossy_mobile" }) },
            );
          }
          await new Promise((r) => setTimeout(r, VERIFY_MS * 2));
          lines = ((await (await api(`/bots/${t.botId}/log`)).json()) as { lines: string[] }).lines;
        });
        expect(lines.join("\n")).not.toContain("unverified");
        expect(err.mock.calls.flat().map(String).join("\n")).not.toContain("unverified");
        if (action === "rejoin") expect(launches).toBe(2);
      } finally {
        err.mockRestore();
      }
    },
    20_000,
  );

  it("puts each observation, and a missing one, in the bot's dashboard log", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async (opts: BotRunOptions) => {
      if (opts.participant === "seen")
        opts.onDiagPackets?.({ state: "DISABLED", source: "config" });
      return fakeBot();
    });
    const seen = { ...task("seen"), ttl: 60_000, diagPackets: "off" as const };
    const silent = { ...task("silent"), ttl: 60_000, diagPackets: "off" as const };
    const quiet = { ...task("quiet"), ttl: 60_000 };
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const logs = async (api: (p: string) => Promise<Response>, id: string): Promise<string[]> =>
      ((await (await api(`/bots/${id}/log`)).json()) as { lines: string[] }).lines;
    try {
      await withCtl([seen, silent, quiet], { diagPacketsVerifyMs: 30 }, async (api) => {
        await until(async () => (await logs(api, silent.botId)).length > 0);
        await new Promise((r) => setTimeout(r, 60));
        expect(await logs(api, seen.botId)).toEqual([
          expect.stringContaining("diagnostics packets: DISABLED (source=config)"),
        ]);
        expect(await logs(api, silent.botId)).toEqual([
          expect.stringContaining("no 'diagnostics packets:' line within 30ms of joining"),
        ]);
        expect(await logs(api, quiet.botId)).toEqual([]);
      });
      expect(err.mock.calls.flat().join("\n")).toContain("[silent@");
      expect(err.mock.calls.flat().join("\n")).not.toContain("[quiet@");
    } finally {
      err.mockRestore();
    }
  }, 20_000);

  it("carries decodeBudget into a /duplicate copy and a ctl relaunch (#2914)", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => fakeBot());
    const token = "test-token";
    let port = 0;
    let listening!: () => void;
    const listened = new Promise<void>((r) => {
      listening = r;
    });
    const run = runBotsToCompletion({
      tasks: [{ ...task("alice"), ttl: 60_000, decodeBudget: "off" }],
      control: {
        port: 0,
        token,
        tokenFilePath: join(tmpdir(), "bots-app-join-test-ctl.json"),
        onListen: async (info) => {
          port = info.port;
          listening();
        },
      },
    });
    await listened;
    const post = (path: string, body: unknown): Promise<Response> =>
      fetch(`http://127.0.0.1:${port}${path}`, {
        method: "POST",
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        body: JSON.stringify(body),
      });
    const until = async (pred: () => boolean): Promise<void> => {
      for (let i = 0; i < 300 && !pred(); i += 1) await new Promise((r) => setTimeout(r, 10));
      if (!pred()) throw new Error("condition never held");
    };
    const id = task("alice").botId;
    await until(() => mocks.launchBot.mock.calls.length === 1);
    expect((await post(`/bots/${id}/duplicate`, {})).ok).toBe(true);
    await until(() => mocks.launchBot.mock.calls.length === 2);
    await post(`/bots/${id}/network`, { network: "lossy_mobile" });
    await until(() => mocks.launchBot.mock.calls.length === 3);
    expect(mocks.launchBot.mock.calls.map((c) => (c[0] as BotRunOptions).decodeBudget)).toEqual([
      "off",
      "off",
      "off",
    ]);
    process.emit("SIGTERM");
    await run;
  }, 20_000);
});
