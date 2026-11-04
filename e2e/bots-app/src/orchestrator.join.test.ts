import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it, onTestFinished, vi } from "vitest";

const mocks = vi.hoisted(() => ({ launchBot: vi.fn() }));
vi.mock("./bot", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./bot")>()),
  launchBot: mocks.launchBot,
}));

import { classifyHealthz } from "./control/conduct";
import { JoinRejectedError, MeetingNavigatedAwayError, WaitingRoomError } from "./meeting-join";
import { type BotRunOptions, LaunchAbortedError } from "./bot";
import { capTtl, FORCE_CLOSE_MS, runBotsToCompletion, type BotTask } from "./orchestrator";
import { TransportNotAppliedError } from "./transport";
import { SD_SOURCE } from "./posture";
import { type PageReload, PageReloadedError } from "./page-reload";
import { ParticipantRecorder } from "./run-record";
import {
  assignPersonas,
  createPersonaPlanner,
  personaFor,
  resolvePersonaSettings,
} from "./persona";
import type { PersonaTally } from "./persona-driver";
import { SPEECH_PLAN_DEFAULT } from "./speech-plan";

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
    closeBrowserNow: vi.fn(async () => {}),
    userHangupDetected: new Promise<void>(() => {}),
    crashDetected: new Promise<string>(() => {}),
    pageReloaded: new Promise(() => {}),
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

describe("orchestrator aborts a pending launch", () => {
  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
  const abortableLaunch = (seen: AbortSignal[]) => (opts: BotRunOptions) =>
    new Promise((_resolve, reject) => {
      if (opts.signal) seen.push(opts.signal);
      opts.signal?.addEventListener("abort", () => reject(new LaunchAbortedError()));
    });

  it("on SIGTERM, and finishes the bot as a shutdown rather than a failure", async () => {
    mocks.launchBot.mockReset();
    const seen: AbortSignal[] = [];
    mocks.launchBot.mockImplementation(abortableLaunch(seen));
    const reasons: Array<string | undefined> = [];
    const run = runBotsToCompletion({
      tasks: [{ ...task("a"), ttl: 60_000 }],
      onFinish: (_id, _at, reason) => reasons.push(reason),
    });
    await sleep(100);
    process.emit("SIGTERM");
    await expect(
      Promise.race([run.then(() => "done"), sleep(3_000).then(() => "stuck")]),
    ).resolves.toBe("done");
    expect(seen).toHaveLength(1);
    expect(seen[0].aborted).toBe(true);
    expect(reasons).toEqual(["shutdown-signal"]);
  });

  it("at the task's deadline", async () => {
    mocks.launchBot.mockReset();
    const seen: AbortSignal[] = [];
    mocks.launchBot.mockImplementation(abortableLaunch(seen));
    const deadline = Date.now() + 1_200;
    await runBotsToCompletion({
      tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs: deadline }],
    });
    expect(seen).toHaveLength(1);
    expect(seen[0].aborted).toBe(true);
  });
});

describe("orchestrator failure tally for a launch that never joined", () => {
  const tally = async (launch: () => Promise<unknown>, runDeadlineMs?: number) => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(launch);
    const warns: string[] = [];
    const warn = vi
      .spyOn(console, "warn")
      .mockImplementation((...a) => void warns.push(a.join(" ")));
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      await runBotsToCompletion({ tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs }] });
    } finally {
      warn.mockRestore();
      err.mockRestore();
    }
    return warns.filter((l) => l.includes("ended with an error"));
  };

  it("counts a launch error", async () => {
    expect(await tally(() => Promise.reject(new Error("boom")))).toHaveLength(1);
  });

  it.each([
    ["an aborted launch", () => new LaunchAbortedError()],
    ["a hang-up during the join", () => new MeetingNavigatedAwayError("left")],
    ["a waiting room", () => new WaitingRoomError("waiting-room", "parked")],
  ])("does not count %s", async (_what, make) => {
    expect(await tally(() => Promise.reject(make()))).toEqual([]);
  });

  it("does not count a launch the run's deadline overtook", async () => {
    expect(await tally(() => new Promise(() => {}), Date.now() + 1_200)).toEqual([]);
  });
});

describe("orchestrator run deadline", () => {
  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
  const launchUntilAborted = (close: () => void) => (opts: BotRunOptions) =>
    new Promise((resolve, reject) => {
      const timer = setTimeout(() => resolve(fakeBot()), 2_000);
      opts.signal?.addEventListener("abort", () => {
        clearTimeout(timer);
        close();
        reject(new LaunchAbortedError());
      });
    });

  it("ends a bot, and bounds its session cookie, by the run deadline despite a slow launch", async () => {
    mocks.launchBot.mockReset();
    vi.useFakeTimers();
    onTestFinished(() => {
      vi.useRealTimers();
    });
    const deadline = Date.now() + 1_200;
    const seen: (number | null | undefined)[] = [];
    mocks.launchBot.mockImplementation(async (opts: BotRunOptions) => {
      seen.push(opts.sessionNotAfterMs);
      await sleep(400);
      return fakeBot();
    });
    let finishedAt = 0;
    const run = runBotsToCompletion({
      tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs: deadline }],
      onFinish: (_id, at) => {
        finishedAt = at;
      },
    });
    await vi.advanceTimersByTimeAsync(1_200 + FORCE_CLOSE_MS);
    await run;
    expect(seen).toEqual([deadline]);
    expect(finishedAt).toBe(deadline);
  });

  it("bounds the session cookie by a TTL shorter than the run", async () => {
    mocks.launchBot.mockReset();
    const seen: number[] = [];
    mocks.launchBot.mockImplementation(async (opts: BotRunOptions) => {
      seen.push((opts.sessionNotAfterMs ?? Infinity) - Date.now());
      return fakeBot();
    });
    await runBotsToCompletion({ tasks: [{ ...task("a"), ttl: 1_500 }] });
    expect(seen).toHaveLength(1);
    expect(seen[0]).toBeLessThanOrEqual(1_500);
    expect(seen[0]).toBeGreaterThan(1_400);
  });

  it("does not launch once the run deadline is too close", async () => {
    mocks.launchBot.mockReset();
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      await runBotsToCompletion({
        tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs: Date.now() + 500 }],
      });
    } finally {
      err.mockRestore();
    }
    expect(mocks.launchBot).not.toHaveBeenCalled();
  });

  it("aborts a launch that outlasts the deadline, which closes its browser by the deadline", async () => {
    mocks.launchBot.mockReset();
    const close = vi.fn();
    mocks.launchBot.mockImplementation(launchUntilAborted(close));
    const deadline = Date.now() + 1_200;
    let finishedAt = 0;
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      await runBotsToCompletion({
        tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs: deadline }],
        onFinish: (_id, at) => {
          finishedAt = at;
        },
      });
    } finally {
      err.mockRestore();
    }
    expect(finishedAt).toBeLessThanOrEqual(deadline + 250);
    expect(close).toHaveBeenCalledTimes(1);
  });

  it("cancels a launch still joining at its join deadline, with that reason", async () => {
    mocks.launchBot.mockReset();
    vi.useFakeTimers();
    onTestFinished(() => {
      vi.useRealTimers();
    });
    const close = vi.fn();
    mocks.launchBot.mockImplementation(launchUntilAborted(close));
    const deadline = Date.now() + 1_200;
    let finishedAt = 0;
    let reason: string | undefined;
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    onTestFinished(() => err.mockRestore());
    const run = runBotsToCompletion({
      tasks: [
        { ...task("a"), ttl: 60_000, joinDeadlineMs: deadline, runDeadlineMs: deadline + 3_000 },
      ],
      onFinish: (_id, at, why) => {
        finishedAt = at;
        reason = why;
      },
    });
    await vi.advanceTimersByTimeAsync(1_200);
    await run;
    expect(finishedAt).toBe(deadline);
    expect(reason).toBe("launch-error");
    expect(err.mock.calls.map((c) => String(c[0])).join("\n")).toContain(
      "the join deadline passed during the launch",
    );
    expect(close).toHaveBeenCalledTimes(1);
  });

  it.each([
    ["alone", { runDeadlineMs: 0 }],
    ["at the join deadline's instant", { runDeadlineMs: 0, joinDeadlineMs: 0 }],
    ["before a later join deadline, on a task without its own", { joinDeadlineMs: 3_000 }],
  ])(
    "records a launch the run's deadline timer aborts as a TTL expiry, run deadline %s",
    async (_when, deadlines: { runDeadlineMs?: number; joinDeadlineMs?: number }) => {
      mocks.launchBot.mockReset();
      vi.useFakeTimers();
      onTestFinished(() => {
        vi.useRealTimers();
      });
      const close = vi.fn();
      mocks.launchBot.mockImplementation(launchUntilAborted(close));
      const deadline = Date.now() + 1_200;
      const reasons: Array<string | undefined> = [];
      const quiet = [
        vi.spyOn(console, "log").mockImplementation(() => undefined),
        vi.spyOn(console, "error").mockImplementation(() => undefined),
      ];
      onTestFinished(() => quiet.forEach((q) => q.mockRestore()));
      const own = Object.fromEntries(
        Object.entries(deadlines).map(([k, v]) => [k, deadline + v]),
      ) as { runDeadlineMs?: number; joinDeadlineMs?: number };
      const run = runBotsToCompletion({
        tasks: [{ ...task("a"), ttl: 60_000, ...own }],
        runDeadlineMs: deadline,
        onFinish: (_id, _at, why) => reasons.push(why),
      });
      await vi.advanceTimersByTimeAsync(1_200 + FORCE_CLOSE_MS + 1_000);
      await run;
      expect(reasons).toEqual(["ttl-expired"]);
      expect(close).toHaveBeenCalledTimes(1);
    },
  );

  it.each([
    ["the run's deadline", {}, 0],
    ["the task's run deadline", { runDeadlineMs: 0 }, 0],
    ["a join deadline", { joinDeadlineMs: 0 }, 3_000],
  ])(
    "records a SIGTERM abort as a shutdown when %s passes during its slow teardown",
    async (_what, deadlines: { runDeadlineMs?: number; joinDeadlineMs?: number }, runAfter) => {
      mocks.launchBot.mockReset();
      vi.useFakeTimers();
      onTestFinished(() => {
        vi.useRealTimers();
      });
      mocks.launchBot.mockImplementation(
        (opts: BotRunOptions) =>
          new Promise((_resolve, reject) => {
            opts.signal?.addEventListener("abort", () => {
              setTimeout(() => reject(new LaunchAbortedError()), 2_200);
            });
          }),
      );
      const deadline = Date.now() + 2_000;
      const own = Object.fromEntries(
        Object.entries(deadlines).map(([k, v]) => [k, deadline + v]),
      ) as { runDeadlineMs?: number; joinDeadlineMs?: number };
      const reasons: Array<string | undefined> = [];
      const quiet = [
        vi.spyOn(console, "log").mockImplementation(() => undefined),
        vi.spyOn(console, "error").mockImplementation(() => undefined),
      ];
      onTestFinished(() => quiet.forEach((q) => q.mockRestore()));
      const run = runBotsToCompletion({
        tasks: [{ ...task("a"), ttl: 60_000, ...own }],
        runDeadlineMs: deadline + runAfter,
        onFinish: (_id, _at, why) => reasons.push(why),
      });
      await vi.advanceTimersByTimeAsync(100);
      process.emit("SIGTERM");
      await vi.advanceTimersByTimeAsync(5_000 + FORCE_CLOSE_MS + 1_000);
      await run;
      expect(reasons).toEqual(["shutdown-signal"]);
    },
  );

  it("gives up a launch at the run deadline when it comes before the join deadline", async () => {
    mocks.launchBot.mockReset();
    vi.useFakeTimers();
    onTestFinished(() => {
      vi.useRealTimers();
    });
    mocks.launchBot.mockImplementation(() => new Promise(() => {}));
    const runBy = Date.now() + 2_000;
    let finishedAt = 0;
    let reason: string | undefined;
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    onTestFinished(() => err.mockRestore());
    const run = runBotsToCompletion({
      tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs: runBy, joinDeadlineMs: runBy + 3_000 }],
      onFinish: (_id, at, why) => {
        finishedAt = at;
        reason = why;
      },
    });
    await vi.advanceTimersByTimeAsync(2_000 + 3_000 + FORCE_CLOSE_MS);
    await run;
    expect(finishedAt).toBe(runBy);
    expect(reason).toBe("ttl-expired");
    expect(err.mock.calls.map((c) => String(c[0])).join("\n")).toContain(
      "the run's deadline passed during the launch",
    );
  });

  it("closes a launch that finishes past its join deadline before the deadline's timer fires", async () => {
    mocks.launchBot.mockReset();
    vi.useFakeTimers();
    onTestFinished(() => {
      vi.useRealTimers();
    });
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    onTestFinished(() => err.mockRestore());
    const bot = { ...(fakeBot() as object), shutdown: vi.fn(() => new Promise(() => {})) } as {
      shutdown: ReturnType<typeof vi.fn>;
      closeBrowserNow: ReturnType<typeof vi.fn>;
    };
    const deadline = Date.now() + 1_000;
    mocks.launchBot.mockImplementation(async () => {
      vi.setSystemTime(deadline + 50);
      return bot;
    });
    let reason: string | undefined;
    await runBotsToCompletion({
      tasks: [{ ...task("a"), ttl: 2_000, joinDeadlineMs: deadline }],
      onFinish: (_id, _at, why) => {
        reason = why;
      },
    });
    expect(reason).toBe("launch-error");
    expect(err.mock.calls.map((c) => String(c[0])).join("\n")).toContain(
      "the join deadline passed during the launch",
    );
    expect(bot.shutdown).toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(FORCE_CLOSE_MS - 1);
    expect(bot.closeBrowserNow).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(bot.closeBrowserNow).toHaveBeenCalled();
  });

  it("keeps a bot that joined before its join deadline running past it", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => fakeBot());
    const deadline = Date.now() + 300;
    let finishedAt = 0;
    let reason: string | undefined;
    await runBotsToCompletion({
      tasks: [{ ...task("a"), ttl: 1_200, joinDeadlineMs: deadline }],
      onFinish: (_id, at, why) => {
        finishedAt = at;
        reason = why;
      },
    });
    expect(reason).toBe("ttl-expired");
    expect(finishedAt).toBeGreaterThanOrEqual(deadline + 800);
  });

  it("closes the browser at the deadline without waiting for a leave", async () => {
    mocks.launchBot.mockReset();
    const bot = {
      ...(fakeBot() as object),
      leaveMeeting: vi.fn(() => sleep(1_500)),
    };
    mocks.launchBot.mockImplementation(async () => bot);
    vi.useFakeTimers();
    onTestFinished(() => {
      vi.useRealTimers();
    });
    const deadline = Date.now() + 1_200;
    let finishedAt = 0;
    const run = runBotsToCompletion({
      tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs: deadline }],
      onFinish: (_id, at) => {
        finishedAt = at;
      },
    });
    await vi.advanceTimersByTimeAsync(1_200 + FORCE_CLOSE_MS - 500);
    await run;
    expect(mocks.launchBot).toHaveBeenCalledTimes(1);
    expect(bot.leaveMeeting).not.toHaveBeenCalled();
    expect(finishedAt).toBe(deadline);
    expect(
      (bot as unknown as { closeBrowserNow: ReturnType<typeof vi.fn> }).closeBrowserNow,
    ).not.toHaveBeenCalled();
  });

  it("skips the leave when the deadline's timer fires a little early", async () => {
    mocks.launchBot.mockReset();
    const bot = { ...(fakeBot() as object), leaveMeeting: vi.fn(async () => {}) };
    mocks.launchBot.mockImplementation(async () => bot);
    const deadline = Date.now() + 1_200;
    const realNow = Date.now.bind(Date);
    const early = 50;
    let clock: { mockRestore: () => void } | undefined;
    const shift = setTimeout(() => {
      clock = vi.spyOn(Date, "now").mockImplementation(() => realNow() - early);
    }, 900);
    try {
      await runBotsToCompletion({
        tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs: deadline }],
      });
    } finally {
      clearTimeout(shift);
      clock?.mockRestore();
    }
    expect(clock).toBeDefined();
    expect(bot.leaveMeeting).not.toHaveBeenCalled();
  });

  it("skips the leave when the run's deadline timer ends the bot before its TTL does", async () => {
    mocks.launchBot.mockReset();
    const bot = { ...(fakeBot() as object), leaveMeeting: vi.fn(async () => {}) };
    const realNow = Date.now.bind(Date);
    mocks.launchBot.mockImplementation(async () => {
      const clock = vi.spyOn(Date, "now").mockImplementation(() => realNow() - 300);
      setTimeout(() => clock.mockRestore(), 50);
      return bot;
    });
    const deadline = Date.now() + 1_200;
    await runBotsToCompletion({
      tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs: deadline }],
      runDeadlineMs: deadline,
    });
    expect(bot.leaveMeeting).not.toHaveBeenCalled();
  });

  it("leaves gracefully when the TTL ends just before the deadline", async () => {
    mocks.launchBot.mockReset();
    const bot = { ...(fakeBot() as object), leaveMeeting: vi.fn(async () => {}) };
    mocks.launchBot.mockImplementation(async () => bot);
    const deadline = Date.now() + 1_200;
    await runBotsToCompletion({
      tasks: [{ ...task("a"), ttl: 1_150, runDeadlineMs: deadline }],
    });
    expect(bot.leaveMeeting).toHaveBeenCalledTimes(1);
  });

  it("ends a bot whose browser close hangs within FORCE_CLOSE_MS of the deadline", async () => {
    mocks.launchBot.mockReset();
    vi.useFakeTimers();
    onTestFinished(() => {
      vi.useRealTimers();
    });
    const bot = { ...(fakeBot() as object), shutdown: vi.fn(() => sleep(8_000)) };
    mocks.launchBot.mockImplementation(async () => bot);
    const deadline = Date.now() + 1_200;
    let finishedAt = 0;
    const run = runBotsToCompletion({
      tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs: deadline }],
      onFinish: (_id, at) => {
        finishedAt = at;
      },
    });
    await vi.advanceTimersByTimeAsync(1_200 + FORCE_CLOSE_MS + 1_000);
    await run;
    expect(bot.shutdown).toHaveBeenCalledTimes(1);
    expect(finishedAt).toBe(deadline + FORCE_CLOSE_MS);
  });

  it("gives control launches the run deadline and closes the control server at it", async () => {
    mocks.launchBot.mockReset();
    const seen: Array<number | null | undefined> = [];
    mocks.launchBot.mockImplementation(async (opts: BotRunOptions) => {
      seen.push(opts.sessionNotAfterMs);
      return fakeBot();
    });
    const deadline = Date.now() + 2_000;
    let port = 0;
    const run = runBotsToCompletion({
      tasks: [],
      runDeadlineMs: deadline,
      control: {
        port: 0,
        token: "test-token",
        tokenFilePath: join(tmpdir(), "bots-app-deadline-test-ctl.json"),
        onListen: async (info) => {
          port = info.port;
        },
      },
    });
    const launch = () =>
      fetch(`http://127.0.0.1:${port}/launch`, {
        method: "POST",
        headers: { authorization: "Bearer test-token", "content-type": "application/json" },
        body: JSON.stringify({
          meetingURL: "https://example.invalid/meeting/X",
          participant: "late",
          ttl: "1m",
          headless: true,
          network: "none",
          authBackend: "jwt",
          videoMode: "clock",
        }),
      });
    while (port === 0) await sleep(10);
    expect((await launch()).status).toBe(201);
    await run;
    expect(Date.now()).toBeLessThanOrEqual(deadline + FORCE_CLOSE_MS + 500);
    expect(seen).toEqual([deadline]);
    await expect(launch()).rejects.toThrow();
  });

  it("refuses a control launch while the run is closing after its deadline", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => ({
      ...(fakeBot() as object),
      shutdown: vi.fn(() => sleep(1_500)),
    }));
    const deadline = Date.now() + 1_200;
    const first = { ...task("a"), ttl: 60_000, runDeadlineMs: deadline };
    let port = 0;
    const run = runBotsToCompletion({
      tasks: [first],
      runDeadlineMs: deadline,
      control: {
        port: 0,
        token: "test-token",
        tokenFilePath: join(tmpdir(), "bots-app-deadline-late-ctl.json"),
        onListen: async (info) => {
          port = info.port;
        },
      },
    });
    while (port === 0 || Date.now() < deadline + 300) await sleep(10);
    const late = await fetch(`http://127.0.0.1:${port}/launch`, {
      method: "POST",
      headers: { authorization: "Bearer test-token", "content-type": "application/json" },
      body: JSON.stringify({
        meetingURL: "https://example.invalid/meeting/X",
        participant: "late",
        ttl: "1m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        videoMode: "clock",
      }),
    });
    const duplicate = await fetch(`http://127.0.0.1:${port}/bots/${first.botId}/duplicate`, {
      method: "POST",
      headers: { authorization: "Bearer test-token", "content-type": "application/json" },
      body: JSON.stringify({ participant: "copy" }),
    });
    await run;
    expect(late.status).not.toBe(201);
    expect(duplicate.status).not.toBe(201);
    expect(mocks.launchBot).toHaveBeenCalledTimes(1);
  });

  it.each(["a crash", "a SIGTERM before the deadline"] as const)(
    "closes the browser and returns at the deadline when teardown after %s stalls",
    async (cause) => {
      mocks.launchBot.mockReset();
      const close = vi.fn(async () => {});
      mocks.launchBot.mockImplementation(async () => ({
        ...(fakeBot() as object),
        closeBrowserNow: close,
        crashDetected:
          cause === "a crash"
            ? new Promise<string>((r) => setTimeout(() => r("page crashed"), 300))
            : new Promise<string>(() => {}),
        shutdown: vi.fn(() => sleep(8_000)),
      }));
      const deadline = Date.now() + 1_200;
      let port = 0;
      const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
      try {
        const run = runBotsToCompletion({
          tasks: [{ ...task("a"), ttl: 60_000, runDeadlineMs: deadline }],
          runDeadlineMs: deadline,
          control: {
            port: 0,
            token: "test-token",
            tokenFilePath: join(tmpdir(), "bots-app-deadline-stall-ctl.json"),
            onListen: async (info) => {
              port = info.port;
            },
          },
        });
        if (cause !== "a crash") setTimeout(() => process.emit("SIGTERM"), 300);
        await run;
      } finally {
        err.mockRestore();
      }
      expect(Date.now()).toBeLessThanOrEqual(deadline + FORCE_CLOSE_MS + 1_000);
      expect(close).toHaveBeenCalled();
      expect(port).toBeGreaterThan(0);
      await expect(
        fetch(`http://127.0.0.1:${port}/bots`, { headers: { authorization: "Bearer test-token" } }),
      ).rejects.toThrow();
    },
    15_000,
  );

  it("caps an infinite or longer TTL, and leaves one without a deadline alone", () => {
    const now = Date.now();
    expect(capTtl("infinite", null)).toBe("infinite");
    expect(capTtl(5_000, undefined)).toBe(5_000);
    const capped = capTtl("infinite", now + 2_000);
    expect(capped).toBeLessThanOrEqual(2_000);
    expect(capped).toBeGreaterThan(1_900);
    expect(capTtl(500, now + 2_000)).toBe(500);
    expect(capTtl(5_000, now - 10)).toBe(0);
  });
});

describe("orchestrator transport instrumentation (issue 3087)", () => {
  it("tags each bot's transport line with its id and passes its intent to launchBot", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async (opts: BotRunOptions) => {
      opts.onTransport?.({
        kind: "elected",
        active: opts.transport === "websocket" ? "websocket" : "webtransport",
      });
      return fakeBot();
    });
    const seen: string[] = [];
    await runBotsToCompletion({
      tasks: [{ ...task("a"), transport: "websocket" }, task("b")],
      onTransport: (id, e) => seen.push(`${id.slice(-1)}:${e.kind === "elected" ? e.active : ""}`),
    });
    expect(seen.sort()).toEqual(["a:websocket", "b:webtransport"]);
  });
});

describe("orchestrator with a control server attached (issue 3087)", () => {
  it("keeps running after a bot fails its launch, until a shutdown signal", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockRejectedValue(new TransportNotAppliedError("websocket", "test"));
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const finished: string[] = [];
    let returned = false;
    try {
      const run = runBotsToCompletion({
        tasks: [{ ...task("alice"), ttl: 60_000, transport: "websocket" }],
        onFinish: (_id, _at, reason) => finished.push(String(reason)),
        control: {
          port: 0,
          token: "test-token",
          tokenFilePath: join(tmpdir(), "bots-app-idle-test-ctl.json"),
        },
      }).then(() => {
        returned = true;
      });
      for (let i = 0; i < 300 && finished.length === 0; i += 1)
        await new Promise((r) => setTimeout(r, 10));
      expect(finished).toEqual(["launch-error"]);
      await new Promise((r) => setTimeout(r, 100));
      expect(returned).toBe(false);
      process.emit("SIGTERM");
      await run;
      expect(returned).toBe(true);
    } finally {
      err.mockRestore();
    }
  }, 20_000);
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
      skipCanvasPaintReadback: "true",
      speakingGlowReadback: "false",
      simulcastMaxLayersReadback: 3,
    }));
    const events: string[] = [];
    await runBotsToCompletion({
      tasks: [{ ...task("alice"), decodeBudget: "off", speakingGlow: "off" }],
      onRegister: (t) => events.push(`register:${t.participant}`),
      onJoin: (_id, _at, media, userId, readback, paint, glow, layers) =>
        events.push(
          `join:${JSON.stringify(media)}:${userId}:${readback}:${paint}:${glow}:${layers}`,
        ),
      onFinish: (_id, at, reason) => events.push(`finish:${reason}:${at > 0}`),
    });
    expect(events).toEqual([
      "register:alice",
      'join:{"camera":false,"mic":true}:alice@labs.example:all:true:false:3',
      "finish:ttl-expired:true",
    ]);
    expect(mocks.launchBot.mock.calls[0][0].decodeBudget).toBe("off");
    expect(mocks.launchBot.mock.calls[0][0].speakingGlow).toBe("off");
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

  it("relaunches a ctl rejoin with the same speakingGlow and forwards the new readback (#2914)", async () => {
    mocks.launchBot.mockReset();
    const readbacks = ["false", "true"];
    mocks.launchBot.mockImplementation(async () => ({
      ...(fakeBot() as object),
      speakingGlowReadback: readbacks.shift() ?? null,
    }));
    const glows: Array<string | null | undefined> = [];
    const token = "test-token";
    let port = 0;
    let listening!: () => void;
    const listened = new Promise<void>((r) => {
      listening = r;
    });
    const run = runBotsToCompletion({
      tasks: [{ ...task("alice"), ttl: 60_000, speakingGlow: "off" }],
      onJoin: (_id, _at, _media, _user, _budget, _paint, glow) => glows.push(glow),
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
    const api = (path: string, body: object): Promise<Response> =>
      fetch(`http://127.0.0.1:${port}${path}`, {
        method: "POST",
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        body: JSON.stringify(body),
      });
    await until(() => glows.length === 1);
    await api(`/bots/${task("alice").botId}/network`, { network: "lossy_mobile" });
    await until(() => glows.length === 2);
    expect(glows).toEqual(["false", "true"]);
    expect(mocks.launchBot.mock.calls.map((c) => c[0].speakingGlow)).toEqual(["off", "off"]);
    await api(`/bots/${task("alice").botId}/leave`, {});
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

  it("drops a bot whose meeting page reloaded after joining out of inMeeting, not expected", async () => {
    const body = await healthzAfter(
      async () => ({
        ...(fakeBot() as object),
        pageReloaded: new Promise<PageReload>((r) =>
          setTimeout(() => r({ atMs: Date.now(), url: "https://example.com/meeting/X" }), 20),
        ),
      }),
      (b) => b.bots === 0,
    );
    expect(body).toMatchObject({ bots: 0, inMeeting: 0, expected: 1 });
    expect(classifyHealthz(body).state).toBe("not-joined");
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

  it("keeps the run deadline across a ctl rejoin", async () => {
    const deadline = Date.now() + 2_500;
    const seen: Array<number | null | undefined> = [];
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async (opts: BotRunOptions) => {
      seen.push(opts.sessionNotAfterMs);
      return fakeBot();
    });
    const t = { ...task("alice"), ttl: 60_000, runDeadlineMs: deadline };
    let finishedAt = 0;
    await withCtl(
      [t],
      {
        onFinish: (_id, at) => {
          finishedAt = at;
        },
      },
      async (api) => {
        await until(() => seen.length === 1);
        await api(`/bots/${t.botId}/network`, {
          method: "POST",
          body: JSON.stringify({ network: "lossy_mobile" }),
        });
        await until(() => seen.length === 2);
        await until(() => finishedAt > 0);
      },
    );
    expect(seen).toEqual([deadline, deadline]);
    expect(finishedAt).toBeLessThanOrEqual(deadline + 50);
  }, 20_000);

  it("launches a ctl rejoin after the first join's deadline has passed", async () => {
    let launches = 0;
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => {
      launches += 1;
      await new Promise((r) => setTimeout(r, 100));
      return fakeBot();
    });
    const joinBy = Date.now() + 1_000;
    const t = {
      ...task("alice"),
      ttl: 60_000,
      joinDeadlineMs: joinBy,
      runDeadlineMs: joinBy + 2_000,
    };
    let reason: string | undefined;
    await withCtl(
      [t],
      {
        onFinish: (_id, _at, why) => {
          reason = why;
        },
      },
      async (api) => {
        await until(() => launches === 1);
        await new Promise((r) => setTimeout(r, joinBy + 200 - Date.now()));
        await api(`/bots/${t.botId}/network`, {
          method: "POST",
          body: JSON.stringify({ network: "lossy_mobile" }),
        });
        await until(() => reason !== undefined);
      },
    );
    expect(launches).toBe(2);
    expect(reason).not.toBe("launch-error");
  }, 20_000);

  it("rearms the running TTL timer on a ctl TTL extension", async () => {
    let launched = false;
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => {
      launched = true;
      return fakeBot();
    });
    const t = { ...task("alice"), ttl: 1_500 };
    let finishedAt = 0;
    let extendedAt = 0;
    await withCtl(
      [t],
      {
        onFinish: (_id, at) => {
          finishedAt = at;
        },
      },
      async (api) => {
        await until(() => launched);
        extendedAt = Date.now();
        await api(`/bots/${t.botId}/ttl`, { method: "POST", body: JSON.stringify({ ttl: "4s" }) });
        await new Promise((r) => setTimeout(r, 3_000));
        await until(() => finishedAt > 0);
      },
    );
    expect(finishedAt - extendedAt).toBeGreaterThanOrEqual(3_500);
  }, 20_000);

  it("caps a ctl TTL extension at the run deadline", async () => {
    const deadline = Date.now() + 1_500;
    let launched = false;
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => {
      launched = true;
      return fakeBot();
    });
    const t = { ...task("alice"), ttl: 60_000, runDeadlineMs: deadline };
    let finishedAt = 0;
    await withCtl(
      [t],
      {
        onFinish: (_id, at) => {
          finishedAt = at;
        },
      },
      async (api) => {
        await until(() => launched);
        await api(`/bots/${t.botId}/ttl`, { method: "POST", body: JSON.stringify({ ttl: "1h" }) });
        await until(() => finishedAt > 0);
      },
    );
    expect(finishedAt).toBeLessThanOrEqual(deadline + 50);
  }, 20_000);

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
    const silentOn = { ...task("silent-on"), ttl: 60_000, diagPackets: "on" as const };
    const quiet = { ...task("quiet"), ttl: 60_000 };
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const logs = async (api: (p: string) => Promise<Response>, id: string): Promise<string[]> =>
      ((await (await api(`/bots/${id}/log`)).json()) as { lines: string[] }).lines;
    try {
      await withCtl([seen, silent, silentOn, quiet], { diagPacketsVerifyMs: 30 }, async (api) => {
        await until(async () => (await logs(api, silent.botId)).length > 0);
        await until(async () => (await logs(api, silentOn.botId)).length > 0);
        await new Promise((r) => setTimeout(r, 60));
        expect(await logs(api, seen.botId)).toEqual([
          expect.stringContaining("diagnostics packets: DISABLED (source=config)"),
        ]);
        expect(await logs(api, silent.botId)).toEqual([
          expect.stringContaining("no 'diagnostics packets:' line within 30ms of joining"),
        ]);
        expect(await logs(api, silentOn.botId)).toEqual([
          expect.stringContaining("--diag-packets on requested but no 'diagnostics packets:' line"),
        ]);
        expect(await logs(api, quiet.botId)).toEqual([]);
      });
      expect(err.mock.calls.flat().join("\n")).toContain("[silent@");
      expect(err.mock.calls.flat().join("\n")).not.toContain("[quiet@");
    } finally {
      err.mockRestore();
    }
  }, 20_000);

  it("carries decodeBudget, skipCanvasPaint, simulcastMaxLayers, transport and persona into a /duplicate copy and a ctl relaunch", async () => {
    const settings = resolvePersonaSettings(true, {});
    if (settings.kind !== "ok" || settings.value === null) throw new Error("persona defaults");
    const room = assignPersonas(createPersonaPlanner(SPEECH_PLAN_DEFAULT, 3), 3, settings.value);
    const persona = personaFor(room, { index: 0, fleetSize: 3 });
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => fakeBot());
    const recorder = new ParticipantRecorder({
      runDir: mkdtempSync(join(tmpdir(), "bots-join-record-")),
      role: null,
      kernelNetem: "none",
      pod: {
        ordinal: null,
        node: null,
        staggerMs: null,
        staggerIncomplete: false,
        imageRevision: null,
      },
    });
    const registered: BotTask[] = [];
    const token = "test-token";
    let port = 0;
    let listening!: () => void;
    const listened = new Promise<void>((r) => {
      listening = r;
    });
    const run = runBotsToCompletion({
      tasks: [
        {
          ...task("alice"),
          ttl: 60_000,
          decodeBudget: "off",
          skipCanvasPaint: true,
          simulcastMaxLayers: 3,
          transport: "websocket",
          persona,
        },
      ],
      onPersona: (botId, tally) => recorder.personaToggled(botId, tally),
      onRegister: (t) => {
        registered.push(t);
        recorder.register(t);
      },
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
    expect(mocks.launchBot.mock.calls.map((c) => (c[0] as BotRunOptions).transport)).toEqual([
      "websocket",
      "websocket",
      "websocket",
    ]);
    expect(mocks.launchBot.mock.calls.map((c) => (c[0] as BotRunOptions).skipCanvasPaint)).toEqual([
      true,
      true,
      true,
    ]);
    expect(
      mocks.launchBot.mock.calls.map((c) => (c[0] as BotRunOptions).simulcastMaxLayers),
    ).toEqual([3, 3, 3]);
    expect(registered).toHaveLength(2);
    expect(recorder.snapshot(registered[1].botId)?.skip_canvas_paint.requested).toBe(true);
    expect(recorder.snapshot(registered[1].botId)?.simulcast_max_layers.requested).toBe(3);
    const calls = mocks.launchBot.mock.calls.map((c) => c[0] as BotRunOptions);
    expect(calls.map((o) => o.persona)).toEqual([persona, persona, persona]);
    const tally: PersonaTally = {
      ok: 1,
      failed: 0,
      already: 0,
      rendered: 1,
      unrendered: 0,
      medianAbsMs: 5,
      minGapMs: null,
    };
    calls[1].onPersona?.(tally);
    expect(recorder.snapshot(registered[1].botId)?.persona?.toggles).toEqual(tally);
    expect(recorder.snapshot(registered[0].botId)?.persona?.toggles).toBeNull();
    process.emit("SIGTERM");
    await run;
  }, 20_000);
});

describe("orchestrator page reload after join", () => {
  it("fails the bot at the reload and records the outcome, without a leave click", async () => {
    mocks.launchBot.mockReset();
    let reload!: (r: PageReload) => void;
    const bot = {
      ...(fakeBot() as object),
      pageReloaded: new Promise<PageReload>((r) => {
        reload = r;
      }),
    } as unknown as { leaveMeeting: ReturnType<typeof vi.fn>; shutdown: ReturnType<typeof vi.fn> };
    mocks.launchBot.mockImplementation(async () => bot);
    const recorder = new ParticipantRecorder({
      runDir: mkdtempSync(join(tmpdir(), "bots-reload-record-")),
      role: null,
      kernelNetem: "none",
      pod: {
        ordinal: null,
        node: null,
        staggerMs: null,
        staggerIncomplete: false,
        imageRevision: null,
      },
    });
    const reloadAtMs = Date.now() + 60_000;
    const err = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      await runBotsToCompletion({
        tasks: [{ ...task("alice"), ttl: 3_000 }],
        onRegister: (t) => recorder.register(t),
        onJoin: (id, at, media, uid) => {
          recorder.joined(id, at, media ?? null, uid ?? null);
          setTimeout(() => reload({ atMs: reloadAtMs, url: "https://example.com/meeting/X" }), 10);
        },
        onFinish: (id, at, reason) => recorder.finished(id, at, reason),
      });
      expect(err.mock.calls.flat().join("\n")).toContain(
        "left the meeting unasked: the meeting page loaded a new document after join",
      );
    } finally {
      err.mockRestore();
    }
    const rec = recorder.snapshot(task("alice").botId);
    expect(rec?.outcome).toBe("page-reloaded");
    expect(rec?.participant.leave_ts).toBe(reloadAtMs / 1000);
    expect(bot.leaveMeeting).not.toHaveBeenCalled();
    expect(bot.shutdown).toHaveBeenCalledTimes(1);
  }, 20_000);
});

describe("orchestrator teardown of a bot that left unasked", () => {
  it.each([
    [
      "page-reloaded",
      { pageReloaded: Promise.resolve({ atMs: Date.now(), url: "https://example.com/meeting/X" }) },
    ],
    ["browser-crash", { crashDetected: Promise.resolve("page crashed") }],
  ])(
    "records %s and closes the browser even when shutdown never settles",
    async (outcome, signal) => {
      mocks.launchBot.mockReset();
      const bot = {
        ...(fakeBot() as object),
        ...signal,
        shutdown: vi.fn(() => new Promise<void>(() => {})),
      } as unknown as { closeBrowserNow: ReturnType<typeof vi.fn> };
      mocks.launchBot.mockImplementation(async () => bot);
      const reasons: Array<string | undefined> = [];
      const err = vi.spyOn(console, "error").mockImplementation(() => {});
      try {
        await runBotsToCompletion({
          tasks: [{ ...task("alice"), ttl: 60_000 }],
          onFinish: (_id, _at, reason) => reasons.push(reason),
        });
      } finally {
        err.mockRestore();
      }
      expect(reasons).toEqual([outcome]);
      expect(bot.closeBrowserNow).toHaveBeenCalledTimes(1);
    },
    10_000,
  );
});

describe("orchestrator page reload during the launch", () => {
  const recorderFor = (): ParticipantRecorder =>
    new ParticipantRecorder({
      runDir: mkdtempSync(join(tmpdir(), "bots-reload-launch-")),
      role: null,
      kernelNetem: "none",
      pod: {
        ordinal: null,
        node: null,
        staggerMs: null,
        staggerIncomplete: false,
        imageRevision: null,
      },
    });
  const run = async (recorder: ParticipantRecorder, joins: number[]): Promise<void> => {
    const err = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      await runBotsToCompletion({
        tasks: [{ ...task("alice"), ttl: 3_000 }],
        onRegister: (t) => recorder.register(t),
        onJoin: (id, at, media, uid) => {
          joins.push(at);
          recorder.joined(id, at, media ?? null, uid ?? null);
        },
        onFinish: (id, at, reason) => recorder.finished(id, at, reason),
      });
    } finally {
      err.mockRestore();
    }
  };

  it("records a bot whose page reloaded before launchBot returned as never joined", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockRejectedValue(
      new PageReloadedError({ atMs: Date.now(), url: "https://example.com/meeting/X" }),
    );
    const recorder = recorderFor();
    const joins: number[] = [];
    await run(recorder, joins);
    const rec = recorder.snapshot(task("alice").botId);
    expect(joins).toEqual([]);
    expect(rec?.outcome).toBe("page-reloaded");
    expect(rec?.participant.join_ts).toBeNull();
    expect(rec?.participant.leave_ts).toBeNull();
  }, 20_000);

  it("never records leave_ts before join_ts for a reload detected as the launch returned", async () => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(async () => ({
      ...(fakeBot() as object),
      pageReloaded: Promise.resolve({ atMs: Date.now() - 5, url: "https://example.com/meeting/X" }),
    }));
    const recorder = recorderFor();
    await run(recorder, []);
    const p = recorder.snapshot(task("alice").botId)?.participant;
    expect(p?.join_ts).not.toBeNull();
    expect(p?.leave_ts).toBeGreaterThanOrEqual(p?.join_ts ?? Infinity);
  }, 20_000);

  it("records a reload during the relaunch after a ctl rejoin at the time it was seen", async () => {
    const reloadAtMs = Date.now() + 60_000;
    mocks.launchBot.mockReset();
    mocks.launchBot
      .mockResolvedValueOnce(fakeBot())
      .mockRejectedValueOnce(
        new PageReloadedError({ atMs: reloadAtMs, url: "https://example.com/meeting/X" }),
      );
    const recorder = recorderFor();
    const botId = task("alice").botId;
    const finishes: number[] = [];
    let port = 0;
    let listening!: () => void;
    const listened = new Promise<void>((r) => (listening = r));
    const until = async (done: () => boolean): Promise<void> => {
      for (let i = 0; i < 300 && !done(); i += 1) await new Promise((r) => setTimeout(r, 10));
    };
    const err = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const run = runBotsToCompletion({
        tasks: [{ ...task("alice"), ttl: 60_000 }],
        onRegister: (t) => recorder.register(t),
        onJoin: (id, at, media, uid) => recorder.joined(id, at, media ?? null, uid ?? null),
        onFinish: (id, at, reason) => {
          finishes.push(at);
          recorder.finished(id, at, reason);
        },
        control: {
          port: 0,
          token: "test-token",
          tokenFilePath: join(tmpdir(), "bots-app-reload-rejoin-ctl.json"),
          onListen: async (info) => {
            port = info.port;
            listening();
          },
        },
      });
      await listened;
      await until(() => recorder.snapshot(botId)?.participant.join_ts != null);
      const res = await fetch(`http://127.0.0.1:${port}/bots/${botId}/network`, {
        method: "POST",
        headers: { authorization: "Bearer test-token", "content-type": "application/json" },
        body: JSON.stringify({ network: "lossy_mobile" }),
      });
      expect(res.status).toBe(202);
      await until(() => finishes.length > 0);
      process.emit("SIGTERM");
      await run;
    } finally {
      err.mockRestore();
    }
    expect(finishes).toEqual([reloadAtMs]);
    const rec = recorder.snapshot(botId);
    expect(rec?.outcome).toBe("page-reloaded");
    expect(rec?.participant.leave_ts).toBe(reloadAtMs / 1000);
  }, 20_000);
});

describe("orchestrator failure tally for a page reload", () => {
  const ended = async (launch: () => Promise<unknown>) => {
    mocks.launchBot.mockReset();
    mocks.launchBot.mockImplementation(launch);
    const warns: string[] = [];
    const warn = vi
      .spyOn(console, "warn")
      .mockImplementation((...a) => void warns.push(a.join(" ")));
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    let snap: Record<string, unknown> | undefined;
    try {
      let port = 0;
      let listening!: () => void;
      const listened = new Promise<void>((r) => (listening = r));
      const run = runBotsToCompletion({
        tasks: [{ ...task("alice"), ttl: 60_000 }],
        control: {
          port: 0,
          token: "test-token",
          tokenFilePath: join(tmpdir(), "bots-app-reload-tally-ctl.json"),
          onListen: async (info) => {
            port = info.port;
            listening();
          },
        },
      });
      await listened;
      for (let i = 0; i < 300 && snap?.finishReason === undefined; i += 1) {
        const res = await fetch(`http://127.0.0.1:${port}/bots`, {
          headers: { authorization: "Bearer test-token" },
        });
        snap = ((await res.json()) as { bots: Array<Record<string, unknown>> }).bots[0];
        if (snap?.finishReason === undefined) await new Promise((r) => setTimeout(r, 10));
      }
      process.emit("SIGTERM");
      await run;
    } finally {
      warn.mockRestore();
      err.mockRestore();
    }
    return { snap, tally: warns.filter((l) => l.includes("ended with an error")) };
  };

  it("counts a reload during the launch as failed", async () => {
    const url = "https://example.com/meeting/X";
    const { snap, tally } = await ended(() =>
      Promise.reject(new PageReloadedError({ atMs: Date.now(), url })),
    );
    expect(snap).toMatchObject({ status: "failed", finishReason: "page-reloaded" });
    expect(tally).toEqual([expect.stringContaining("1/1 bot(s) ended with an error")]);
  }, 20_000);

  it.each([
    ["https://example.com/meeting/X", "the meeting page loaded a new document after join"],
    ["chrome-error://chromewebdata/", "the meeting page failed to load a new document after join"],
  ])(
    "counts a reload to %s after join as failed",
    async (url, cause) => {
      const { snap, tally } = await ended(async () => ({
        ...(fakeBot() as object),
        pageReloaded: Promise.resolve({ atMs: Date.now(), url }),
      }));
      expect(snap).toMatchObject({
        status: "failed",
        finishReason: "page-reloaded",
        lastError: `${cause} (${url})`,
      });
      expect(tally).toEqual([expect.stringContaining("1/1 bot(s) ended with an error")]);
    },
    20_000,
  );
});
