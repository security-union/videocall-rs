import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  addInitScript: vi.fn(),
  applyJwtCookieAuth: vi.fn(),
  ensureAssetsPrimed: vi.fn(),
  joinMeetingAndEnableMedia: vi.fn(),
  launch: vi.fn(),
  browserClose: vi.fn(),
  contextClose: vi.fn(),
  pageEvaluate: vi.fn(),
  documentIdentity: vi.fn(async (..._args: unknown[]): Promise<unknown> => 1),
  resolveAssetsForParticipant: vi.fn(),
  performFormLogin: vi.fn(),
  resolveFormLoginCredentials: vi.fn(),
}));

vi.mock("@playwright/test", () => ({
  chromium: {
    launch: mocks.launch,
  },
}));

vi.mock("./auth/jwt-cookie", () => ({
  applyJwtCookieAuth: mocks.applyJwtCookieAuth,
}));

vi.mock("./auth/form-login", () => ({
  performFormLogin: mocks.performFormLogin,
  resolveFormLoginCredentials: mocks.resolveFormLoginCredentials,
}));

vi.mock("./assets", () => ({
  resolveAssetsForParticipant: mocks.resolveAssetsForParticipant,
}));

vi.mock("./auto-prime", () => ({
  ensureAssetsPrimed: mocks.ensureAssetsPrimed,
}));

vi.mock("./meeting-join", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./meeting-join")>();
  return {
    ...actual,
    joinMeetingAndEnableMedia: mocks.joinMeetingAndEnableMedia,
  };
});

import {
  BOT_RECEIVE_POSTURE,
  CLOSE_STEP_BUDGET_MS,
  type DiagPacketsObservation,
  ENCODER_FPS_POLL_MS,
  isBenignTeardownError,
  TRANSPORT_APPLIED_WAIT_MS,
  launchBot,
  LaunchAbortedError,
  parseDiagPacketsLine,
} from "./bot";
import { openSsoCaptureBrowser } from "./auth/sso-capture";
import { CAMERA_CYCLE_NEVER_FIRED_BANNER } from "./camera-cycle";
import {
  buildDecodeBudgetInitScript,
  DECODE_BUDGET_OFF_VALUE,
  DECODE_BUDGET_STORAGE_KEY,
  DecodeBudgetNotAppliedError,
} from "./decode-budget";
import {
  buildSpeakingGlowInitScript,
  SPEAKING_GLOW_STORAGE_KEY,
  SpeakingGlowNotAppliedError,
} from "./speaking-glow";
import {
  cameraControlSelector,
  HANG_UP_SELECTOR,
  MIC_UNMUTE_SELECTOR,
  micControlSelector,
  PRE_MARKER_UI_BANNER,
} from "./control-buttons";
import {
  assignPersonas,
  createPersonaPlanner,
  personaFor,
  resolvePersonaSettings,
} from "./persona";
import { JoinMediaNotAppliedError } from "./meeting-join";
import { runBotsToCompletion } from "./orchestrator";
import { HD_SOURCE, SD_SOURCE } from "./posture";
import {
  SIMULCAST_MAX_LAYERS_CONFIG_KEY,
  SIMULCAST_READBACK_TIMEOUT_MS,
  SIMULCAST_READBACK_UNREAD,
  SimulcastMaxLayersNotAppliedError,
  SimulcastMaxLayersReadbackTimeoutError,
} from "./simulcast-layers";
import type { PersonaTally } from "./persona-driver";
import { SPEECH_PLAN_DEFAULT } from "./speech-plan";
import { GRID_SELECTOR, MeetingNavigatedAwayError } from "./meeting-join";
import {
  joinedDocumentIdentity,
  PageReloadedError,
  STAMP_TIMEOUT_MS,
  StampTimeoutError,
} from "./page-reload";
import { buildTransportInitScript, TransportNotAppliedError } from "./transport";
import { HD_VIDEO_NOISE, SD_VIDEO_NOISE } from "./video-noise";
import {
  BROWSER_STEP_TIME_LIMIT_MS,
  PAGE_CALL_TIME_LIMIT_MS,
  TimeLimitExceededError,
} from "./time-limit";

const fpsPolls = (): unknown[][] =>
  mocks.pageEvaluate.mock.calls.filter((c) => c[1] !== SIMULCAST_MAX_LAYERS_CONFIG_KEY);

const CLOCK_SOURCE_PATH = fileURLToPath(new URL("./clock-source.js", import.meta.url));

describe("isBenignTeardownError", () => {
  it.each([
    "Target page, context or browser has been closed",
    "Page has been closed.",
    "Pipe has been closed",
  ])("recognizes Playwright's already-closed error: %s", (message) => {
    expect(isBenignTeardownError(new Error(message), false)).toBe(true);
  });

  it.each([
    new Error("ENOSPC: no space left on device"),
    "Target page, context or browser has been closed",
    undefined,
    null,
  ])("does not classify an unrelated or non-Error throw as benign", (value) => {
    expect(isBenignTeardownError(value, false)).toBe(false);
  });

  it.each([
    "Target page, context or browser has been closed",
    "Page has been closed.",
    "Pipe has been closed",
  ])(
    "keeps an already-closed message at error level after an unexpected browser disconnect: %s",
    (message) => {
      // Playwright emits these SAME strings when Chromium crashes. If the browser
      // disconnected before we asked it to, the close rejection is the teardown
      // DIAGNOSTIC (message + stack) for that crash. The `disconnected` listener
      // logs the crash itself, so this is not the only signal — but the detail
      // belongs at error level beside it, not buried in `debug`.
      expect(isBenignTeardownError(new Error(message), true)).toBe(false);
    },
  );
});

describe("launchBot clock mode", () => {
  let pageOn: ReturnType<typeof vi.fn>;
  let fakePage: {
    goto: ReturnType<typeof vi.fn>;
    on: ReturnType<typeof vi.fn>;
    off: ReturnType<typeof vi.fn>;
    url: ReturnType<typeof vi.fn<() => string>>;
  };
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.browserClose.mockResolvedValue(undefined);
    mocks.contextClose.mockResolvedValue(undefined);

    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/ClockTest"),
    };
    pageOn = page.on;
    fakePage = page;
    const context = {
      close: mocks.contextClose,
      newPage: vi.fn().mockResolvedValue(page),
    };
    const browser = {
      close: mocks.browserClose,
      newContext: vi.fn().mockResolvedValue(context),
      // launchBot attaches a `disconnected` listener to detect a Chromium crash
      // (#2089); the fake must accept it.
      on: vi.fn(),
    };

    mocks.launch.mockResolvedValue(browser);
    mocks.addInitScript.mockResolvedValue(undefined);
    mocks.ensureAssetsPrimed.mockResolvedValue(undefined);
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
    mocks.resolveAssetsForParticipant.mockReturnValue({
      audioPath: "/tmp/audio.wav",
      videoPath: "/tmp/video.y4m",
    });
  });

  it("resolves crashDetected on an unasked browser disconnect or page crash (#2917)", async () => {
    const opts = {
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock" as const,
      authBackend: "none" as const,
      sourceGeometry: SD_SOURCE,
    };
    const handlerOf = (target: { on: ReturnType<typeof vi.fn> }, event: string) =>
      target.on.mock.calls.filter((c) => c[0] === event).at(-1)?.[1] as () => void;
    for (const [pick, event, detail] of [
      ["browser", "disconnected", "browser disconnected unexpectedly"],
      ["page", "crash", "page crashed"],
    ] as const) {
      const bot = await launchBot(opts);
      handlerOf((pick === "browser" ? bot.browser : bot.page) as never, event)();
      await expect(bot.crashDetected).resolves.toBe(detail);
    }
  });

  describe("page reload after join", () => {
    const opts = {
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock" as const,
      authBackend: "none" as const,
      sourceGeometry: SD_SOURCE,
    };
    type JoinArgs = { onInMeeting?: () => Promise<void> };
    const emitCommit = (): void => {
      const removed = new Set(fakePage.off.mock.calls.map((c) => c[1]));
      const frame = { parentFrame: () => null, url: () => fakePage.url() };
      for (const [event, listener] of fakePage.on.mock.calls) {
        if (event === "framenavigated" && !removed.has(listener))
          (listener as (f: unknown) => void)(frame);
      }
    };
    const commit = async (identity: number): Promise<void> => {
      mocks.documentIdentity.mockResolvedValue(identity);
      emitCommit();
      await new Promise((r) => setTimeout(r, 0));
    };
    const state = (p: Promise<unknown>): Promise<string> =>
      Promise.race([
        p.then(() => "reloaded"),
        new Promise<string>((r) => setTimeout(() => r("pending"), 0)),
      ]);
    let err: ReturnType<typeof vi.spyOn>;
    beforeEach(() => {
      mocks.documentIdentity.mockResolvedValue(1);
      mocks.joinMeetingAndEnableMedia.mockImplementation(async (a: JoinArgs) => {
        await a.onInMeeting?.();
      });
      err = vi.spyOn(console, "error").mockImplementation(() => {});
    });
    afterEach(() => {
      err.mockRestore();
      mocks.documentIdentity.mockResolvedValue(1);
      mocks.pageEvaluate.mockReset();
    });

    it("resolves pageReloaded on a commit whose document identity differs from the joined one, not on a same-document navigation or one before the join", async () => {
      mocks.joinMeetingAndEnableMedia.mockImplementationOnce(async (a: JoinArgs) => {
        await commit(7);
        await a.onInMeeting?.();
      });
      const bot = await launchBot(opts);
      await commit(7);
      expect(await state(bot.pageReloaded)).toBe("pending");

      await commit(8);
      await expect(bot.pageReloaded).resolves.toEqual({
        atMs: expect.any(Number),
        url: "https://example.test/meeting/ClockTest",
      });
      expect(err.mock.calls.flat().join(" ")).toContain("failing it as page-reloaded");
    });

    it("catches a reload during the join's rename and media steps", async () => {
      mocks.joinMeetingAndEnableMedia.mockImplementationOnce(async (a: JoinArgs) => {
        await a.onInMeeting?.();
        await commit(2);
      });
      await expect(launchBot(opts)).rejects.toBeInstanceOf(PageReloadedError);
      expect(mocks.browserClose).toHaveBeenCalled();
    });

    it.each([
      [
        "destroys the stamp's context",
        () =>
          mocks.documentIdentity
            .mockRejectedValueOnce(new Error("Execution context was destroyed"))
            .mockResolvedValue(2),
      ],
      [
        "commits during the stamp",
        () =>
          mocks.documentIdentity
            .mockImplementationOnce(async () => {
              emitCommit();
              return 2;
            })
            .mockResolvedValue(2),
      ],
      [
        "replaced the document before the stamp read it",
        () => mocks.documentIdentity.mockResolvedValue(null),
      ],
    ])("fails closed when a reload %s", async (_case, arrange) => {
      arrange();
      await expect(launchBot(opts)).rejects.toBeInstanceOf(PageReloadedError);
    });

    it("fails the launch, unjoined, when the joined document never answers the stamp", async () => {
      vi.useFakeTimers();
      try {
        mocks.documentIdentity.mockImplementation(() => new Promise(() => {}));
        const launch = launchBot(opts).then(
          () => "returned",
          (e: unknown) => e,
        );
        await vi.advanceTimersByTimeAsync(STAMP_TIMEOUT_MS + 1);
        const outcome = await Promise.race([launch, Promise.resolve("still launching")]);
        expect(outcome).toBeInstanceOf(StampTimeoutError);
        expect(mocks.browserClose).toHaveBeenCalled();
      } finally {
        vi.useRealTimers();
      }
    });

    const settleWithin = async (launch: Promise<unknown>, budgetMs: number): Promise<unknown> => {
      const outcome = launch.then(
        () => "returned",
        (e: unknown) => e,
      );
      for (let t = 0; t < budgetMs; t += 1_000) await vi.advanceTimersByTimeAsync(1_000);
      return Promise.race([outcome, Promise.resolve("still launching")]);
    };
    const stampFailures = [
      [
        "times out",
        StampTimeoutError,
        () => mocks.documentIdentity.mockImplementation(() => new Promise(() => {})),
      ],
      [
        "sees a reload",
        PageReloadedError,
        () => mocks.documentIdentity.mockRejectedValueOnce(new Error("context destroyed")),
      ],
    ] as const;

    it.each(stampFailures)(
      "stops the join inside the hook when the stamp %s",
      async (_case, error, arrange) => {
        vi.useFakeTimers();
        try {
          arrange();
          let continued = false;
          mocks.joinMeetingAndEnableMedia.mockImplementationOnce(async (a: JoinArgs) => {
            await a.onInMeeting?.();
            continued = true;
          });
          const outcome = await settleWithin(launchBot(opts), STAMP_TIMEOUT_MS + 1_000);
          expect(outcome).toBeInstanceOf(error);
          expect(continued).toBe(false);
        } finally {
          vi.useRealTimers();
        }
      },
    );

    it.each([
      ...stampFailures,
      [
        "is followed by a reload during the readbacks",
        PageReloadedError,
        () =>
          mocks.pageEvaluate.mockImplementationOnce(async () => {
            mocks.documentIdentity.mockResolvedValue(2);
            emitCommit();
            await new Promise((r) => setTimeout(r, 0));
            return { skipCanvasPaint: "true" };
          }),
      ],
    ] as const)(
      "closes the browser in bounded time when the stamp %s and context.close never settles",
      async (_case, error, arrange) => {
        vi.useFakeTimers();
        try {
          arrange();
          mocks.contextClose.mockImplementation(() => new Promise(() => {}));
          const outcome = await settleWithin(
            launchBot({ ...opts, skipCanvasPaint: true }),
            STAMP_TIMEOUT_MS + 30_000,
          );
          expect(outcome).toBeInstanceOf(error);
          expect(mocks.contextClose).toHaveBeenCalledTimes(1);
          expect(mocks.browserClose).toHaveBeenCalledTimes(1);
        } finally {
          vi.useRealTimers();
        }
      },
    );

    const never = (): Promise<never> => new Promise(() => {});

    it.each([
      ["simulcast max layers", { simulcastMaxLayers: 2 }, SIMULCAST_MAX_LAYERS_CONFIG_KEY],
      ["receiver caps", { skipCanvasPaint: true }, ["skipCanvasPaint"]],
      ["decode budget", { decodeBudget: "off" as const }, DECODE_BUDGET_STORAGE_KEY],
      ["speaking glow", { speakingGlow: "off" as const }, SPEAKING_GLOW_STORAGE_KEY],
    ] as const)(
      "ends a stalled %s readback as soon as a reload is seen",
      async (_case, extra, key) => {
        vi.useFakeTimers();
        let stalled = 0;
        try {
          mocks.pageEvaluate.mockImplementation((_fn: unknown, arg: unknown) => {
            if (JSON.stringify(arg) !== JSON.stringify(key)) return Promise.resolve(undefined);
            stalled += 1;
            setTimeout(() => {
              mocks.documentIdentity.mockResolvedValue(2);
              emitCommit();
            }, 100);
            return never();
          });
          const outcome = await settleWithin(launchBot({ ...opts, ...extra }), 1_000);
          expect(stalled).toBe(1);
          expect(outcome).toBeInstanceOf(PageReloadedError);
          expect(mocks.browserClose).toHaveBeenCalledTimes(1);
        } finally {
          mocks.pageEvaluate.mockReset();
          vi.useRealTimers();
        }
      },
    );

    it("ends the launch at a reload while the join's steps after the stamp never settle", async () => {
      vi.useFakeTimers();
      try {
        mocks.joinMeetingAndEnableMedia.mockImplementationOnce(async (a: JoinArgs) => {
          await a.onInMeeting?.();
          setTimeout(() => {
            mocks.documentIdentity.mockResolvedValue(2);
            emitCommit();
          }, 100);
          return never();
        });
        const outcome = await settleWithin(launchBot(opts), 1_000);
        expect(outcome).toBeInstanceOf(PageReloadedError);
        expect(mocks.browserClose).toHaveBeenCalledTimes(1);
      } finally {
        vi.useRealTimers();
      }
    });

    it("fails the launch for a reload seen after its last readback, before the bot is handed back", async () => {
      const cycle = { onMinMs: 60_000, onMaxMs: 60_000, offMinMs: 60_000, offMaxMs: 60_000 };
      const disarms = (): number =>
        fakePage.off.mock.calls.filter((c) => c[0] === "framenavigated").length;
      let disarmsBefore = 0;
      const end = { seen: null as boolean | null };
      const log = vi.spyOn(console, "log").mockImplementation((...a: unknown[]) => {
        if (a.join(" ").includes("camera cycle configured")) end.seen = disarms() > disarmsBefore;
      });
      let caughtAtEnd = 0;
      try {
        for (let ticks = 0; ticks < 40; ticks++) {
          end.seen = null;
          disarmsBefore = disarms();
          mocks.joinMeetingAndEnableMedia.mockImplementationOnce(async (a: JoinArgs) => {
            await a.onInMeeting?.();
            mocks.documentIdentity.mockImplementationOnce(async () => {
              for (let i = 0; i < ticks; i++) await null;
              return 2;
            });
            emitCommit();
          });
          const outcome = await launchBot({ ...opts, cameraCycle: cycle }).catch((e: unknown) => e);
          if (end.seen === true) {
            expect(outcome, `reload ${ticks} ticks after the commit`).toBeInstanceOf(
              PageReloadedError,
            );
            caughtAtEnd += 1;
          } else if (!(outcome instanceof Error)) {
            await (outcome as Awaited<ReturnType<typeof launchBot>>).shutdown();
          }
        }
      } finally {
        log.mockRestore();
      }
      expect(caughtAtEnd).toBeGreaterThan(0);
    });

    it("does not read an identity check that the failed launch's own close cuts short as a reload", async () => {
      let cut: (e: Error) => void = () => {};
      mocks.pageEvaluate.mockImplementationOnce(async () => {
        mocks.documentIdentity.mockImplementation(
          () => new Promise((_resolve, reject) => void (cut = reject)),
        );
        emitCommit();
        return null;
      });
      mocks.contextClose.mockImplementation(async () => cut(new Error("Target closed")));
      await expect(launchBot({ ...opts, decodeBudget: "off" })).rejects.toBeInstanceOf(
        DecodeBudgetNotAppliedError,
      );
      await new Promise((r) => setTimeout(r, 0));
      expect(err.mock.calls.flat().join(" ")).not.toContain("failing it as page-reloaded");
    });

    it("records page-reloaded and closes the browser, in bounded time, when every Playwright call hangs after join", async () => {
      vi.useFakeTimers();
      try {
        const { runBotsToCompletion } = await import("./orchestrator");
        const { ParticipantRecorder } = await import("./run-record");
        const recorder = new ParticipantRecorder({
          runDir: mkdtempSync(join(tmpdir(), "bots-reload-hang-")),
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
        const hangEverything = (): void => {
          for (const m of [
            mocks.documentIdentity,
            mocks.pageEvaluate,
            mocks.contextClose,
            mocks.browserClose,
            fakePage.goto,
          ]) {
            m.mockImplementation(never);
          }
          (fakePage as unknown as { locator: ReturnType<typeof vi.fn> }).locator.mockImplementation(
            () => new Proxy({}, { get: () => never }),
          );
        };
        const botId = "00000000-0000-0000-0000-00000000hang";
        const run = runBotsToCompletion({
          tasks: [{ ...opts, botId, cameraCycle: null, ttl: 600_000 }],
          onRegister: (t) => recorder.register(t),
          onJoin: (id, at, media, uid) => {
            recorder.joined(id, at, media ?? null, uid ?? null);
            hangEverything();
            emitCommit();
          },
          onFinish: (id, at, reason) => recorder.finished(id, at, reason),
        });
        const done = await settleWithin(run, STAMP_TIMEOUT_MS + 30_000);
        expect(done).toBe("returned");
        const rec = recorder.snapshot(botId);
        expect(rec?.outcome).toBe("page-reloaded");
        expect(rec?.participant.leave_ts).toBeGreaterThanOrEqual(
          rec?.participant.join_ts ?? Infinity,
        );
        expect(mocks.browserClose).toHaveBeenCalledTimes(1);
      } finally {
        vi.useRealTimers();
      }
    });

    it.each([
      ["leaveMeeting", async () => (await launchBot(opts)).leaveMeeting()],
      ["a failed decode-budget verdict", () => launchBot({ ...opts, decodeBudget: "off" })],
    ] as const)("bounds the hang-up of %s on a renderer that never answers", async (_case, act) => {
      vi.useFakeTimers();
      try {
        (fakePage as unknown as { locator: ReturnType<typeof vi.fn> }).locator.mockImplementation(
          () => new Proxy({}, { get: () => never }),
        );
        mocks.pageEvaluate.mockResolvedValue(null);
        const outcome = await settleWithin(act(), 20_000);
        expect(outcome).not.toBe("still launching");
      } finally {
        vi.useRealTimers();
      }
    });

    it("closes the context and the browser once however many teardowns run", async () => {
      const bot = await launchBot(opts);
      await Promise.all([bot.shutdown(), bot.closeBrowserNow(), bot.shutdown()]);
      expect(mocks.contextClose).toHaveBeenCalledTimes(1);
      expect(mocks.browserClose).toHaveBeenCalledTimes(1);
    });

    it("says so when the joined document's identity is unreadable", async () => {
      mocks.documentIdentity.mockResolvedValue(undefined);
      await launchBot(opts);
      expect(err.mock.calls.flat().join(" ")).toContain("a page reload will not be detected");
    });

    it("stamps the document with joinedDocumentIdentity and the grid selector", async () => {
      await launchBot(opts);
      expect(mocks.documentIdentity.mock.calls[0]).toEqual([joinedDocumentIdentity, GRID_SELECTOR]);
    });

    it.each(["leaveMeeting", "shutdown", "closeBrowserNow"] as const)(
      "does not read a new document after the bot's own %s as a reload",
      async (step) => {
        const bot = await launchBot(opts);
        await bot[step]();
        await commit(2);
        expect(await state(bot.pageReloaded)).toBe("pending");
      },
    );

    const ERROR_PAGE = "chrome-error://chromewebdata/";

    it("fails a bot whose page lands on Chromium's load-failure page after join, not as a hang-up", async () => {
      const bot = await launchBot(opts);
      fakePage.url.mockReturnValue(ERROR_PAGE);
      emitCommit();
      await expect(bot.pageReloaded).resolves.toMatchObject({ url: ERROR_PAGE });
      expect(await state(bot.userHangupDetected)).toBe("pending");
    });

    it("fails a launch whose page lands on the load-failure page during the join as a reload", async () => {
      mocks.joinMeetingAndEnableMedia.mockImplementationOnce(async () => {
        fakePage.url.mockReturnValue(ERROR_PAGE);
        emitCommit();
        throw new MeetingNavigatedAwayError("clock-bot");
      });
      const failure = await launchBot(opts).catch((e: unknown) => e);
      expect(failure).toBeInstanceOf(PageReloadedError);
      expect((failure as Error).message).toContain("failed to load a new document");
    });

    it.each([
      ["rejects", () => Promise.reject(new Error("Target crashed"))],
      ["never answers", never],
    ])("files a renderer crash during the stamp as a crash when the read %s", async (_c, read) => {
      vi.useFakeTimers();
      try {
        mocks.documentIdentity.mockImplementationOnce(() => {
          const onCrash = pageOn.mock.calls.filter((c) => c[0] === "crash").at(-1)?.[1];
          (onCrash as () => void)();
          return read();
        });
        const outcome = await settleWithin(launchBot(opts), STAMP_TIMEOUT_MS + 1_000);
        expect(outcome).not.toBeInstanceOf(PageReloadedError);
        expect(outcome).not.toBeInstanceOf(StampTimeoutError);
        expect((outcome as Error).message).toBe("page crashed during the launch");
        expect(mocks.browserClose).toHaveBeenCalledTimes(1);
      } finally {
        vi.useRealTimers();
      }
    });

    it("leaves a new document off the meeting path to the hang-up detector", async () => {
      const bot = await launchBot(opts);
      fakePage.url.mockReturnValue("https://example.test/");
      await commit(2);
      expect(await state(bot.pageReloaded)).toBe("pending");
    });
  });

  it("closes the browser at once without reporting a crash", async () => {
    const bot = await launchBot({
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock",
      authBackend: "none",
      sourceGeometry: SD_SOURCE,
    });
    const browser = bot.browser as unknown as { on: ReturnType<typeof vi.fn> };
    const disconnected = browser.on.mock.calls.filter((c) => c[0] === "disconnected").at(-1)?.[1];
    const err = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      await bot.closeBrowserNow();
      (disconnected as () => void)();
      expect(mocks.browserClose).toHaveBeenCalled();
      expect(err.mock.calls.flat().join(" ")).not.toContain("disconnected unexpectedly");
    } finally {
      err.mockRestore();
    }
  });

  it("registers the participant and clock source without priming or fake-file capture", async () => {
    await launchBot({
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock",
      authBackend: "none",
      sourceGeometry: SD_SOURCE,
      manifest: {
        participants: [{ name: "clock-bot" }],
        lines: [],
        pauseMs: 0,
      },
      manifestDir: "/tmp/manifest",
      runDir: "/tmp/run",
      costumeOverride: "clock.y4m",
      audioOverride: "clock.wav",
    });

    expect(mocks.addInitScript).toHaveBeenCalledTimes(1);
    const script = mocks.addInitScript.mock.calls[0][0] as string;
    expect(script).toMatch(
      /^globalThis\.__CLOCK_PARTICIPANT = "Clock Bot";\nglobalThis\.__CLOCK_WIDTH = 640;\nglobalThis\.__CLOCK_HEIGHT = 480;\n/,
    );
    expect(script).toContain(
      `${readFileSync(CLOCK_SOURCE_PATH, "utf8")}\n//# sourceURL=${CLOCK_SOURCE_PATH}`,
    );

    expect(mocks.ensureAssetsPrimed).not.toHaveBeenCalled();
    expect(mocks.resolveAssetsForParticipant).not.toHaveBeenCalled();
    const launchOptions = mocks.launch.mock.calls[0]?.[0] as { args: string[] };
    expect(launchOptions.args).not.toEqual(
      expect.arrayContaining([
        expect.stringMatching(/^--use-file-for-fake-(?:audio|video)-capture=/),
      ]),
    );
  });

  it("forwards the requested join media and exposes what the join observed (#2914)", async () => {
    const base = {
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock" as const,
      authBackend: "none" as const,
      sourceGeometry: SD_SOURCE,
    };
    mocks.joinMeetingAndEnableMedia.mockResolvedValueOnce({ camera: false, mic: null });
    const handle = await launchBot({ ...base, joinMedia: { camera: false, mic: true } });
    expect(mocks.joinMeetingAndEnableMedia.mock.calls[0][0].media).toEqual({
      camera: false,
      mic: true,
    });
    expect(handle.joinMedia).toEqual({ camera: false, mic: null });

    const onResponse = (pageOn.mock.calls as Array<[string, (r: unknown) => void]>).find(
      ([ev]) => ev === "response",
    )?.[1];
    expect(onResponse).toBeDefined();
    const respond = (url: string, body: unknown, ok = true): void =>
      onResponse?.({ ok: () => ok, url: () => url, json: () => Promise.resolve(body) });
    expect(handle.sessionUserId()).toBeNull();
    respond("https://api.example.test/api/v1/meetings/ClockTest/join", {
      success: true,
      result: { user_id: "bot-7@labs.example", status: "waiting" },
    });
    respond("https://api.example.test/api/v1/meetings/Other/join", {
      success: true,
      result: { user_id: "wrong@labs.example" },
    });
    await new Promise((r) => setTimeout(r, 0));
    expect(handle.sessionUserId()).toBe("bot-7@labs.example");

    await launchBot(base);
    expect(mocks.joinMeetingAndEnableMedia.mock.calls[1][0].media).toEqual({
      camera: true,
      mic: true,
    });
  });

  it("injects the talking-clock slot and a voice, defaulting to index 0 of 1 (issue 3072)", async () => {
    const base = {
      meetingURL: "https://example.test/meeting/ClockTest",
      displayName: "Bot 5",
      headless: true,
      videoMode: "clock" as const,
      authBackend: "none" as const,
      sourceGeometry: SD_SOURCE,
    };
    await launchBot({ ...base, participant: "bot-5", talkingClock: { index: 5, fleetSize: 8 } });
    await launchBot({ ...base, participant: "alice" });
    const [slotted, unslotted] = mocks.addInitScript.mock.calls.map((c) => c[0] as string);
    expect(slotted).toContain(
      "globalThis.__CLOCK_FLEET_INDEX = 5;\nglobalThis.__CLOCK_FLEET_SIZE = 8;\n",
    );
    expect(slotted).toMatch(/globalThis\.__CLOCK_VOICE_CLIPS = \{[^\n]*"n00":"[A-Za-z0-9+/=]+"/);
    expect(unslotted).toContain(
      "globalThis.__CLOCK_FLEET_INDEX = 0;\nglobalThis.__CLOCK_FLEET_SIZE = 1;\n",
    );
  });

  it("injects the task's source geometry in the label's own init script (#2236)", async () => {
    await launchBot({
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock",
      authBackend: "none",
      sourceGeometry: { width: 1280, height: 720 },
    });

    expect(mocks.addInitScript).toHaveBeenCalledTimes(1);
    expect(mocks.addInitScript.mock.calls[0][0]).toContain(
      "globalThis.__CLOCK_WIDTH = 1280;\nglobalThis.__CLOCK_HEIGHT = 720;\n",
    );
  });

  it("injects the geometry's noise alpha, or the override, before the clock source (issue 3089)", async () => {
    const logSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    const base = {
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock" as const,
      authBackend: "none" as const,
    };
    try {
      await launchBot({ ...base, sourceGeometry: HD_SOURCE });
      await launchBot({ ...base, sourceGeometry: SD_SOURCE });
      await launchBot({ ...base, sourceGeometry: HD_SOURCE, videoNoise: 0 });
      const scripts = mocks.addInitScript.mock.calls.map((c) => c[0] as string);
      const alphas = scripts.map((script) => {
        const m = /globalThis\.__CLOCK_VIDEO_NOISE = ([^;]+);\n/.exec(script);
        expect(m?.index).toBeLessThan(script.indexOf(readFileSync(CLOCK_SOURCE_PATH, "utf8")));
        return Number(m?.[1]);
      });
      expect(alphas).toEqual([HD_VIDEO_NOISE, SD_VIDEO_NOISE, 0]);
      expect(logSpy.mock.calls.map((c) => String(c[0]))).toEqual(
        expect.arrayContaining([expect.stringContaining("video noise: alpha 0 (override;")]),
      );
    } finally {
      logSpy.mockRestore();
    }
  });

  it("prints the greppable capture token only past a join AND a positive encoder fps (#2236)", async () => {
    vi.useFakeTimers();
    const logSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    try {
      const { WaitingRoomError } = await import("./meeting-join");
      const opts = {
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock" as const,
        authBackend: "none" as const,
        sourceGeometry: { width: 1280, height: 720 },
        onEncoderFps: (): void => {},
      };
      const token = "[clock-bot] captures 1280x720 (issue #2236)";
      const tokenCount = (): number => logSpy.mock.calls.flat().filter((l) => l === token).length;
      mocks.pageEvaluate.mockReset();
      mocks.pageEvaluate.mockResolvedValue(9);

      // A bot the meeting never admitted never reaches the poll, so it cannot
      // be counted.
      mocks.joinMeetingAndEnableMedia.mockRejectedValueOnce(
        new WaitingRoomError("waiting-for-host", "still waiting for the host"),
      );
      await expect(launchBot(opts)).rejects.toThrow(WaitingRoomError);
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS * 2);
      expect(tokenCount()).toBe(0);

      const handle = await launchBot(opts);
      // Joined, but no reading has landed yet.
      expect(tokenCount()).toBe(0);
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS);
      expect(tokenCount()).toBe(1);

      // Once per join, though the chain keeps reading positives.
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS * 3);
      expect(tokenCount()).toBe(1);
      await handle.shutdown();
    } finally {
      logSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("prints no capture token when the encoder never reports fps (#2236)", async () => {
    vi.useFakeTimers();
    const logSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    try {
      // The join is mocked, so it succeeds with no encoder at all.
      mocks.pageEvaluate.mockReset();
      mocks.pageEvaluate.mockResolvedValue(undefined);
      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: { width: 1280, height: 720 },
        onEncoderFps: (): void => {},
      });
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS * 5);
      expect(mocks.pageEvaluate.mock.calls.length).toBeGreaterThan(1);
      expect(logSpy.mock.calls.flat().join("\n")).not.toContain("captures");
      await handle.shutdown();
    } finally {
      logSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("prints no capture token in costume mode (#2236)", async () => {
    vi.useFakeTimers();
    const logSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    try {
      mocks.pageEvaluate.mockReset();
      mocks.pageEvaluate.mockResolvedValue(9);
      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "costume",
        authBackend: "none",
        sourceGeometry: { width: 1280, height: 720 },
        onEncoderFps: (): void => {},
      });
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS * 3);
      expect(logSpy.mock.calls.flat().join("\n")).not.toContain("captures");
      await handle.shutdown();
    } finally {
      logSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("logs an already-closed context as expected teardown rather than an error", async () => {
    const closeError = new Error("Target page, context or browser has been closed");
    mocks.contextClose.mockRejectedValueOnce(closeError);
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const debugSpy = vi.spyOn(console, "debug").mockImplementation(() => {});
    try {
      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
      });

      await handle.shutdown();

      expect(errorSpy).not.toHaveBeenCalled();
      expect(debugSpy).toHaveBeenCalledWith(
        "[clock-bot] context was already closed during teardown:",
        closeError,
      );
    } finally {
      errorSpy.mockRestore();
      debugSpy.mockRestore();
    }
  });

  it("keeps a novel context close failure at error level", async () => {
    const closeError = new Error("ENOSPC: no space left on device");
    mocks.contextClose.mockRejectedValueOnce(closeError);
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const debugSpy = vi.spyOn(console, "debug").mockImplementation(() => {});
    try {
      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
      });

      await handle.shutdown();

      expect(errorSpy).toHaveBeenCalledWith("[clock-bot] context.close failed:", closeError);
      expect(debugSpy).not.toHaveBeenCalled();
    } finally {
      errorSpy.mockRestore();
      debugSpy.mockRestore();
    }
  });

  // Every early-exit teardown (missing form-login creds, form-login failure, manual
  // hang-up, waiting-room / join-rejected) closes the browser WITHOUT going through
  // shutdown(). Playwright fires `disconnected` on any normal close, so a path that
  // skips the intentional-close flag logs a spurious "browser disconnected
  // unexpectedly" on a perfectly graceful exit — the exact noise #2089 removes.
  // Parameterized because a single-path test left three of the four call sites
  // unlocked. Each case asserts browser.close was actually reached, so a case that
  // silently stops exercising its path fails instead of passing vacuously.
  it.each([
    ["waiting-room", "waiting-room"],
    ["manual hang-up", "navigated-away"],
    ["join-rejected", "join-rejected"],
    ["missing form-login creds", "missing-creds"],
    ["form-login failure", "form-login-failed"],
  ])(
    "does not report a crash when the %s early exit closes the browser (#2089)",
    async (_label, kind) => {
      const { WaitingRoomError, MeetingNavigatedAwayError, JoinRejectedError } =
        await import("./meeting-join");
      let opts: Parameters<typeof launchBot>[0] = {
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
      };
      if (kind === "waiting-room") {
        mocks.joinMeetingAndEnableMedia.mockRejectedValueOnce(
          new WaitingRoomError("waiting-for-host", "still waiting for the host"),
        );
      } else if (kind === "navigated-away") {
        mocks.joinMeetingAndEnableMedia.mockRejectedValueOnce(
          new MeetingNavigatedAwayError("operator hung up"),
        );
      } else if (kind === "join-rejected") {
        mocks.joinMeetingAndEnableMedia.mockRejectedValueOnce(
          new JoinRejectedError("rejected", "host denied the join"),
        );
      } else if (kind === "missing-creds") {
        // The form-login path bails BEFORE joining when the creds are absent.
        mocks.resolveFormLoginCredentials.mockReturnValueOnce(undefined);
        opts = { ...opts, authBackend: "form-login" };
      } else {
        // Creds present, but the IdP form drive itself fails — a SEPARATE close
        // site from the missing-creds bail (five in total, not four).
        mocks.resolveFormLoginCredentials.mockReturnValueOnce({
          email: "bot@example.test",
          password: "pw",
        });
        mocks.performFormLogin.mockRejectedValueOnce(new Error("idp form login failed"));
        opts = { ...opts, authBackend: "form-login" };
      }

      const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
      try {
        // Fire `disconnected` from INSIDE browser.close(), the way real Playwright
        // does. Invoking it after the close had settled would leave this green even
        // if the flag were raised too late — a mutation it must catch.
        mocks.browserClose.mockImplementationOnce(async () => {
          const browser = await mocks.launch.mock.results[0]?.value;
          const onDisconnected = (browser.on as ReturnType<typeof vi.fn>).mock.calls.find(
            (call: unknown[]) => call[0] === "disconnected",
          )?.[1] as () => void;
          onDisconnected();
        });

        await expect(launchBot(opts)).rejects.toBeTruthy();

        // Guard against a vacuous pass: this path must really have closed the
        // browser, otherwise the "no crash logged" assertion proves nothing.
        expect(mocks.browserClose).toHaveBeenCalled();
        expect(errorSpy).not.toHaveBeenCalledWith(
          expect.stringContaining("browser disconnected unexpectedly"),
        );
      } finally {
        errorSpy.mockRestore();
      }
    },
  );

  // The `browser.close()` catch carries the SAME classification as the context one
  // and was previously unguarded: deleting the whole branch left every test green.
  it.each([
    ["benign", new Error("Pipe has been closed"), { expectDebug: true }],
    ["novel", new Error("EACCES: permission denied"), { expectDebug: false }],
  ])(
    "classifies a %s browser.close failure during shutdown (#2089)",
    async (_label, closeError, { expectDebug }) => {
      mocks.browserClose.mockRejectedValueOnce(closeError);
      const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
      const debugSpy = vi.spyOn(console, "debug").mockImplementation(() => {});
      try {
        const handle = await launchBot({
          meetingURL: "https://example.test/meeting/ClockTest",
          participant: "clock-bot",
          displayName: "Clock Bot",
          headless: true,
          videoMode: "clock",
          authBackend: "none",
          sourceGeometry: SD_SOURCE,
        });

        await handle.shutdown();

        if (expectDebug) {
          expect(debugSpy).toHaveBeenCalledWith(
            "[clock-bot] browser was already closed during teardown:",
            closeError,
          );
          expect(errorSpy).not.toHaveBeenCalled();
        } else {
          expect(errorSpy).toHaveBeenCalledWith("[clock-bot] browser.close failed:", closeError);
          expect(debugSpy).not.toHaveBeenCalled();
        }
      } finally {
        errorSpy.mockRestore();
        debugSpy.mockRestore();
      }
    },
  );

  it("keeps a benign browser.close failure at error level after an unexpected disconnect (#2089)", async () => {
    // A crash mid-teardown must not let the browser.close rejection be demoted
    // either — the same guarantee the context.close branch has.
    const closeError = new Error("Pipe has been closed");
    mocks.contextClose.mockImplementationOnce(async () => {
      const browser = await mocks.launch.mock.results[0]?.value;
      const onDisconnected = (browser.on as ReturnType<typeof vi.fn>).mock.calls.find(
        (call: unknown[]) => call[0] === "disconnected",
      )?.[1] as () => void;
      onDisconnected();
    });
    mocks.browserClose.mockRejectedValueOnce(closeError);
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const debugSpy = vi.spyOn(console, "debug").mockImplementation(() => {});
    try {
      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
      });

      await handle.shutdown();

      expect(errorSpy).toHaveBeenCalledWith("[clock-bot] browser.close failed:", closeError);
      expect(debugSpy).not.toHaveBeenCalled();
    } finally {
      errorSpy.mockRestore();
      debugSpy.mockRestore();
    }
  });

  it("opts out of Playwright's SIGTERM/SIGHUP browser close (#2089)", async () => {
    // Pins the launch options. Playwright's default SIGTERM handler races the
    // orchestrator's leaveMeeting()->shutdown() teardown and produces the
    // already-closed context this issue is about. Its default SIGHUP handler also
    // overrides Node's normal termination even though the orchestrator has no
    // SIGHUP path. The mechanism is invisible at every other call site, so it
    // needs an explicit pin against a refactor that rebuilds the launch object.
    await launchBot({
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock",
      authBackend: "none",
      sourceGeometry: SD_SOURCE,
    });

    expect(mocks.launch).toHaveBeenCalledWith(
      expect.objectContaining({ handleSIGTERM: false, handleSIGHUP: false }),
    );
  });

  it("keeps a coexisting SSO recapture out of Playwright's global signal handlers (#2089)", async () => {
    await launchBot({
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock",
      authBackend: "none",
      sourceGeometry: SD_SOURCE,
    });

    const capturePage = {
      goto: vi.fn().mockResolvedValue(undefined),
    };
    const captureContext = {
      close: vi.fn().mockResolvedValue(undefined),
      newPage: vi.fn().mockResolvedValue(capturePage),
      storageState: vi.fn().mockResolvedValue(undefined),
    };
    const captureBrowser = {
      close: vi.fn().mockResolvedValue(undefined),
      newContext: vi.fn().mockResolvedValue(captureContext),
    };
    mocks.launch.mockResolvedValueOnce(captureBrowser);

    const capture = await openSsoCaptureBrowser({
      startUrl: "https://example.test/",
    });

    expect(mocks.launch).toHaveBeenCalledTimes(2);
    expect(mocks.launch).toHaveBeenNthCalledWith(
      1,
      expect.objectContaining({ handleSIGTERM: false, handleSIGHUP: false }),
    );
    expect(mocks.launch).toHaveBeenNthCalledWith(
      2,
      expect.objectContaining({ handleSIGTERM: false, handleSIGHUP: false }),
    );
    await capture.close();
  });

  it("demotes an already-closed context and suppresses our intentional browser disconnect (#2089)", async () => {
    // Pins the two shutdown steps in their real order: context.close() rejects
    // first and is demoted while no unexpected disconnect has occurred; then our
    // browser.close() fires `disconnected` after the intentional-close flag is
    // raised, so it must not be reported as a crash.
    //
    // `@playwright/test` is mocked here, so this does not reproduce Playwright's
    // signal handler. The preceding launch-option test is the mutation-sensitive
    // guard for the SIGTERM root cause; this test independently guards shutdown's
    // intentional-close flag and benign-error classification.
    const closeError = new Error("Target page, context or browser has been closed");
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const debugSpy = vi.spyOn(console, "debug").mockImplementation(() => {});
    try {
      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
      });

      // context.close() runs first and reports that the target was already closed.
      // browser.close() then emits the normal intentional `disconnected` event.
      mocks.contextClose.mockRejectedValueOnce(closeError);
      mocks.browserClose.mockImplementationOnce(async () => {
        const browser = await mocks.launch.mock.results[0]?.value;
        const onDisconnected = (browser.on as ReturnType<typeof vi.fn>).mock.calls.find(
          (call: unknown[]) => call[0] === "disconnected",
        )?.[1] as () => void;
        onDisconnected();
      });

      await handle.shutdown();

      expect(errorSpy).not.toHaveBeenCalledWith(
        expect.stringContaining("browser disconnected unexpectedly"),
      );
      expect(errorSpy).not.toHaveBeenCalled();
      expect(debugSpy).toHaveBeenCalledWith(
        "[clock-bot] context was already closed during teardown:",
        closeError,
      );
    } finally {
      errorSpy.mockRestore();
      debugSpy.mockRestore();
    }
  });

  it("reports a crash that arrives while the context is closing (#2089)", async () => {
    // The flag must NOT be raised before `context.close()`: closing a context does
    // not disconnect the browser, so a `disconnected` during that window is a real
    // crash. Raising it too early blinds the listener across exactly the window
    // where a crash is most likely. Asserts the listener's OWN log directly, so
    // deleting that log fails this test.
    mocks.contextClose.mockImplementationOnce(async () => {
      const browser = await mocks.launch.mock.results[0]?.value;
      const onDisconnected = (browser.on as ReturnType<typeof vi.fn>).mock.calls.find(
        (call: unknown[]) => call[0] === "disconnected",
      )?.[1] as () => void;
      onDisconnected();
    });
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
      });

      await handle.shutdown();

      expect(errorSpy).toHaveBeenCalledWith(
        "[clock-bot] browser disconnected unexpectedly (crash or external kill)",
      );
    } finally {
      errorSpy.mockRestore();
    }
  });

  it("keeps an already-closed message at error level when the browser crashed first (#2089)", async () => {
    // The crash path end-to-end: Playwright reports `disconnected` BEFORE we call
    // shutdown(), then context.close() rejects with a message that is normally
    // benign. The listener already logged the crash; this rejection carries WHAT
    // failed during teardown, so it must stay at error level rather than be
    // demoted to `debug` away from the crash line.
    const closeError = new Error("Target page, context or browser has been closed");
    mocks.contextClose.mockRejectedValueOnce(closeError);
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const debugSpy = vi.spyOn(console, "debug").mockImplementation(() => {});
    try {
      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
      });

      // Fire the `disconnected` listener launchBot registered, simulating a crash
      // that happens before any teardown request.
      const browser = await mocks.launch.mock.results[0]?.value;
      const onDisconnected = (browser.on as ReturnType<typeof vi.fn>).mock.calls.find(
        (call: unknown[]) => call[0] === "disconnected",
      )?.[1] as () => void;
      expect(onDisconnected).toBeTypeOf("function");
      onDisconnected();

      await handle.shutdown();

      // The listener's OWN log must fire — asserting only the secondary
      // context.close error would let a deleted crash log stay green.
      expect(errorSpy).toHaveBeenCalledWith(
        "[clock-bot] browser disconnected unexpectedly (crash or external kill)",
      );
      // ...and the close rejection must NOT be demoted to debug.
      expect(errorSpy).toHaveBeenCalledWith("[clock-bot] context.close failed:", closeError);
      expect(debugSpy).not.toHaveBeenCalled();
    } finally {
      errorSpy.mockRestore();
      debugSpy.mockRestore();
    }
  });

  it("polls window.__videocall_encoder_fps: forwards readings and no-data, stops on shutdown (#2062, #2070)", async () => {
    vi.useFakeTimers();
    try {
      // Client contract (videocall-client #2057): a positive number when the
      // encoder is active; `undefined` (cleared) when off/warming; `0` is
      // "not started / not diagnostic" per health_reporter.rs (encoder > 0).
      mocks.pageEvaluate.mockReset();
      // positive -> forwarded; absent -> null (no data); zero -> null (not a starvation reading)
      const readings: unknown[] = [12, undefined, 0];
      mocks.pageEvaluate.mockImplementation(async (_fn: unknown, arg: unknown) =>
        arg === SIMULCAST_MAX_LAYERS_CONFIG_KEY
          ? undefined
          : readings.length
            ? readings.shift()
            : 4,
      );
      const onEncoderFps = vi.fn();

      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
        manifest: { participants: [{ name: "clock-bot" }], lines: [], pauseMs: 0 },
        manifestDir: "/tmp/manifest",
        runDir: "/tmp/run",
        costumeOverride: "clock.y4m",
        audioOverride: "clock.wav",
        onEncoderFps,
      });

      // launchBot returns with the first poll scheduled but not yet fired.
      expect(fpsPolls()).toHaveLength(0);

      // Tick 1: reads 12 -> forwarded.
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS);
      expect(onEncoderFps).toHaveBeenCalledTimes(1);
      expect(onEncoderFps).toHaveBeenLastCalledWith(12);

      // Tick 2: reads undefined -> explicit no-data, and the chain keeps polling.
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS);
      expect(onEncoderFps).toHaveBeenCalledTimes(2);
      expect(onEncoderFps).toHaveBeenLastCalledWith(null);

      // Tick 3: reads 0 -> no-data, per the client's >0 convention.
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS);
      expect(onEncoderFps).toHaveBeenCalledTimes(3);
      expect(onEncoderFps).toHaveBeenLastCalledWith(null);

      // Tick 4: reads 4 -> forwarded (proves no-data did not stop polling).
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS);
      expect(onEncoderFps).toHaveBeenCalledTimes(4);
      expect(onEncoderFps).toHaveBeenLastCalledWith(4);

      // shutdown() is the authoritative stop: no further evaluate or callback.
      await handle.shutdown();
      const evaluateCallsAtShutdown = fpsPolls().length;
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS * 3);
      expect(fpsPolls().length).toBe(evaluateCallsAtShutdown);
      expect(onEncoderFps).toHaveBeenCalledTimes(4);
    } finally {
      vi.useRealTimers();
    }
  });

  it("throttles: no second poll while one page.evaluate is in flight, and shutdown blocks a late reschedule (#2062)", async () => {
    vi.useFakeTimers();
    try {
      // First poll: an evaluate that stays PENDING longer than the interval —
      // simulates a CPU-saturated renderer where the CDP round-trip is slow.
      mocks.pageEvaluate.mockReset();
      let resolveFirst: ((v: unknown) => void) | undefined;
      mocks.pageEvaluate.mockImplementation((_fn: unknown, arg: unknown) => {
        if (arg === SIMULCAST_MAX_LAYERS_CONFIG_KEY) return Promise.resolve(undefined);
        if (resolveFirst !== undefined) return Promise.resolve(8);
        return new Promise((resolve) => {
          resolveFirst = resolve;
        });
      });
      const onEncoderFps = vi.fn();

      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
        manifest: { participants: [{ name: "clock-bot" }], lines: [], pauseMs: 0 },
        manifestDir: "/tmp/manifest",
        runDir: "/tmp/run",
        costumeOverride: "clock.y4m",
        audioOverride: "clock.wav",
        onEncoderFps,
      });

      // (1) Anti-pile-up: while the first evaluate is pending, advancing several
      // intervals must NOT start more evaluates. A fixed-rate setInterval would
      // have fired ~4 more times (this assertion fails on that un-fixed code).
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS * 4);
      expect(fpsPolls()).toHaveLength(1);
      expect(onEncoderFps).not.toHaveBeenCalled();

      // (2) fpsPollStopped: shut down WHILE the evaluate is in flight, THEN let
      // it settle. The late `.then` must not re-arm the chain. Without the flag
      // (clearTimeout alone), the late reschedule would arm a new timer and a
      // second evaluate would fire on advance — so this pins the flag, not just
      // the clearTimeout.
      await handle.shutdown();
      resolveFirst?.(8);
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS * 3);
      expect(fpsPolls()).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("launchBot form-login mode (#2035)", () => {
  let context: { close: ReturnType<typeof vi.fn>; newPage: ReturnType<typeof vi.fn> };
  let browser: {
    close: ReturnType<typeof vi.fn>;
    newContext: ReturnType<typeof vi.fn>;
    // launchBot registers a `disconnected` listener for crash detection (#2089).
    on: ReturnType<typeof vi.fn>;
  };

  beforeEach(() => {
    vi.clearAllMocks();

    const page = {
      addInitScript: mocks.addInitScript.mockResolvedValue(undefined),
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://app.videocall.labsworkspace.fnxlabs.com/meeting/bottest"),
    };
    context = {
      close: vi.fn().mockResolvedValue(undefined),
      newPage: vi.fn().mockResolvedValue(page),
    };
    browser = {
      close: vi.fn().mockResolvedValue(undefined),
      newContext: vi.fn().mockResolvedValue(context),
      on: vi.fn(),
    };
    mocks.launch.mockResolvedValue(browser);
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
    mocks.performFormLogin.mockResolvedValue(undefined);
  });

  const baseOpts = {
    meetingURL: "https://app.videocall.labsworkspace.fnxlabs.com/meeting/bottest",
    participant: "bot",
    displayName: "Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "form-login" as const,
    sourceGeometry: SD_SOURCE,
  };

  it("drives the login form BEFORE the join flow, using the app origin as appBaseUrl", async () => {
    mocks.resolveFormLoginCredentials.mockReturnValue({
      email: "bot@example.test",
      password: "secret-pw",
    });

    await launchBot(baseOpts);

    expect(mocks.performFormLogin).toHaveBeenCalledTimes(1);
    const call = mocks.performFormLogin.mock.calls[0][0];
    expect(call).toMatchObject({
      email: "bot@example.test",
      password: "secret-pw",
      appBaseUrl: "https://app.videocall.labsworkspace.fnxlabs.com",
      // Derived from the meeting URL path so phase 3 waits for the right
      // /meeting/<id> — the callback→meeting handoff fix (#2035).
      meetingId: "bottest",
    });

    // Ordering guard: form-login must complete before the join begins.
    // Move the form-login block after joinMeetingAndEnableMedia in bot.ts
    // and this assertion goes red.
    expect(mocks.joinMeetingAndEnableMedia).toHaveBeenCalled();
    expect(mocks.performFormLogin.mock.invocationCallOrder[0]).toBeLessThan(
      mocks.joinMeetingAndEnableMedia.mock.invocationCallOrder[0],
    );
  });

  it("forwards the login timeout overrides to performFormLogin (#2356)", async () => {
    mocks.resolveFormLoginCredentials.mockReturnValue({
      email: "bot@example.test",
      password: "secret-pw",
    });

    await launchBot({ ...baseOpts, formLoginTimeoutMs: 90_000, formLoginActionTimeoutMs: 45_000 });

    expect(mocks.performFormLogin.mock.calls[0][0]).toMatchObject({
      timeoutMs: 90_000,
      actionTimeoutMs: 45_000,
    });
  });

  it("passes no timeout override when unset, leaving the form-login defaults (#2356)", async () => {
    mocks.resolveFormLoginCredentials.mockReturnValue({
      email: "bot@example.test",
      password: "secret-pw",
    });

    await launchBot(baseOpts);

    const call = mocks.performFormLogin.mock.calls[0][0];
    expect(call.timeoutMs).toBeUndefined();
    expect(call.actionTimeoutMs).toBeUndefined();
  });

  it("throws and tears the browser down when BOT_EMAIL / BOT_PASSWORD are absent", async () => {
    mocks.resolveFormLoginCredentials.mockReturnValue(null);

    await expect(launchBot(baseOpts)).rejects.toThrow(/BOT_EMAIL and BOT_PASSWORD/);

    // No login attempt, no join, and the browser was cleaned up.
    expect(mocks.performFormLogin).not.toHaveBeenCalled();
    expect(mocks.joinMeetingAndEnableMedia).not.toHaveBeenCalled();
    expect(context.close).toHaveBeenCalledTimes(1);
    expect(browser.close).toHaveBeenCalledTimes(1);
  });

  it("tears the browser down and rethrows when the login-form drive fails", async () => {
    mocks.resolveFormLoginCredentials.mockReturnValue({
      email: "bot@example.test",
      password: "secret-pw",
    });
    mocks.performFormLogin.mockRejectedValue(new Error("identity login form did not appear"));

    await expect(launchBot(baseOpts)).rejects.toThrow(/identity login form did not appear/);

    expect(mocks.joinMeetingAndEnableMedia).not.toHaveBeenCalled();
    expect(context.close).toHaveBeenCalledTimes(1);
    expect(browser.close).toHaveBeenCalledTimes(1);
  });
});

describe("launchBot hardwareConcurrency spoof (#2035)", () => {
  // Kept describe-local so the before/after-navigation ordering assertion can
  // compare this mock's invocation order against the addInitScript spoof.
  let goto: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    vi.clearAllMocks();
    goto = vi.fn().mockResolvedValue(undefined);

    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto,
      locator: vi.fn(),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/HwTest"),
    };
    const context = {
      close: vi.fn().mockResolvedValue(undefined),
      newPage: vi.fn().mockResolvedValue(page),
    };
    const browser = {
      close: vi.fn().mockResolvedValue(undefined),
      newContext: vi.fn().mockResolvedValue(context),
      on: vi.fn(),
    };

    mocks.launch.mockResolvedValue(browser);
    mocks.addInitScript.mockResolvedValue(undefined);
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
  });

  // Clock mode reuses the existing minimal harness (no priming / asset
  // resolution). The spoof is injected regardless of video mode.
  const baseOpts = {
    meetingURL: "https://example.test/meeting/HwTest",
    participant: "hw-bot",
    displayName: "HW Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
  };

  // The exact init script the injection is required to emit. This mirrors the
  // literal `Object.defineProperty(navigator, 'hardwareConcurrency', …)` form
  // the production code in bot.ts writes (same convention as the clock test's
  // literal-script assertions above), so a mutation that changes the property
  // name, the getter, the `configurable` flag, or the value breaks this match.
  const spoofScript = (n: number): string =>
    `Object.defineProperty(navigator, 'hardwareConcurrency', { get: () => ${n}, configurable: true });`;

  it("injects the hardwareConcurrency spoof BEFORE the first navigation when set", async () => {
    await launchBot({ ...baseOpts, hardwareConcurrency: 2 });

    // (1) The spoof init script must have been registered. Reverting the
    // injection in bot.ts makes this fail (the clock scripts never match this
    // literal), so the assertion is wired to the real production output.
    expect(mocks.addInitScript).toHaveBeenCalledWith(spoofScript(2));

    // (2) It must be registered BEFORE page.goto. addInitScript only takes
    // effect on the NEXT navigation, so a spoof registered after goto would
    // silently miss the client's capability sniff on the loaded page. Vitest's
    // invocationCallOrder is a shared monotonic counter across mocks, so the
    // two orders are directly comparable. Moving the injection after page.goto
    // in bot.ts flips this and the test goes red.
    const calls = mocks.addInitScript.mock.calls;
    const spoofIdx = calls.findIndex((c) => c[0] === spoofScript(2));
    expect(spoofIdx).toBeGreaterThanOrEqual(0);
    const spoofOrder = mocks.addInitScript.mock.invocationCallOrder[spoofIdx];
    const gotoOrder = goto.mock.invocationCallOrder[0];
    expect(spoofOrder).toBeLessThan(gotoOrder);
  });

  it("does NOT inject any hardwareConcurrency script when the value is unset (default behavior)", async () => {
    await launchBot(baseOpts);

    // Clock mode still adds its own init script; assert specifically that none
    // of the registered scripts touch navigator.hardwareConcurrency.
    const touchedHwc = mocks.addInitScript.mock.calls.some(
      (c) => typeof c[0] === "string" && c[0].includes("hardwareConcurrency"),
    );
    expect(touchedHwc).toBe(false);
  });

  it("does NOT inject when the value is <= 0 (locks the `> 0` guard; 0 means no-data)", async () => {
    await launchBot({ ...baseOpts, hardwareConcurrency: 0 });

    const touchedHwc = mocks.addInitScript.mock.calls.some(
      (c) => typeof c[0] === "string" && c[0].includes("hardwareConcurrency"),
    );
    expect(touchedHwc).toBe(false);
  });
});

describe("launchBot decode budget (#2914)", () => {
  let goto: ReturnType<typeof vi.fn>;
  let browserClose: ReturnType<typeof vi.fn>;
  let hangUpClick: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    vi.clearAllMocks();
    goto = vi.fn().mockResolvedValue(undefined);
    browserClose = vi.fn().mockResolvedValue(undefined);
    hangUpClick = vi.fn().mockResolvedValue(undefined);
    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto,
      locator: vi.fn((selector: string) => ({
        isVisible: async () => selector === HANG_UP_SELECTOR,
        click: hangUpClick,
      })),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/DbTest"),
      waitForURL: vi.fn().mockResolvedValue(undefined),
    };
    const context = {
      close: vi.fn().mockResolvedValue(undefined),
      newPage: vi.fn().mockResolvedValue(page),
    };
    mocks.launch.mockResolvedValue({
      close: browserClose,
      newContext: vi.fn().mockResolvedValue(context),
      on: vi.fn(),
    });
    mocks.addInitScript.mockResolvedValue(undefined);
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
  });

  const baseOpts = {
    meetingURL: "https://example.test/meeting/DbTest",
    participant: "db-bot",
    displayName: "DB Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
  };
  const SEED = buildDecodeBudgetInitScript("https://example.test");
  const storage = (value: string | null | Error) =>
    mocks.pageEvaluate.mockImplementation(async (_fn: unknown, arg: unknown) => {
      if (arg !== DECODE_BUDGET_STORAGE_KEY) return undefined;
      if (value instanceof Error) throw value;
      return value;
    });
  const readbacks = () =>
    mocks.pageEvaluate.mock.calls.filter((c) => c[1] === DECODE_BUDGET_STORAGE_KEY);

  it("off seeds the override before the first navigation and reads it back after join", async () => {
    storage("all");
    const bot = await launchBot({ ...baseOpts, decodeBudget: "off" });
    const idx = mocks.addInitScript.mock.calls.findIndex((c) => c[0] === SEED);
    expect(idx).toBeGreaterThanOrEqual(0);
    expect(mocks.addInitScript.mock.invocationCallOrder[idx]).toBeLessThan(
      goto.mock.invocationCallOrder[0],
    );
    expect(readbacks()).toHaveLength(1);
    expect(readbacks()[0][0].toString()).toContain("localStorage.getItem");
    expect(mocks.pageEvaluate.mock.invocationCallOrder[0]).toBeGreaterThan(
      mocks.joinMeetingAndEnableMedia.mock.invocationCallOrder[0],
    );
    expect(bot.decodeBudgetReadback).toBe("all");
    expect(browserClose).not.toHaveBeenCalled();
  });

  it.each([undefined, null, "client"] as const)(
    "decodeBudget %j installs nothing and reads nothing back",
    async (decodeBudget) => {
      storage("all");
      const bot = await launchBot({ ...baseOpts, decodeBudget });
      const touched = mocks.addInitScript.mock.calls.some(
        (c) => typeof c[0] === "string" && c[0].includes(DECODE_BUDGET_STORAGE_KEY),
      );
      expect(touched).toBe(false);
      expect(readbacks()).toHaveLength(0);
      expect(bot.decodeBudgetReadback).toBeNull();
    },
  );

  it.each([
    ["absent", null],
    ["a different value", "auto"],
    ["an evaluate failure", new Error("Execution context was destroyed")],
  ] as const)("hangs up, closes the browser and fails the launch on %s", async (_what, value) => {
    storage(value);
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const launched = launchBot({ ...baseOpts, decodeBudget: "off" });
      await expect(launched).rejects.toBeInstanceOf(DecodeBudgetNotAppliedError);
      await expect(launched).rejects.toMatchObject({
        observed: value instanceof Error ? null : value,
      });
      expect(browserClose).toHaveBeenCalledTimes(1);
      expect(hangUpClick).toHaveBeenCalledTimes(1);
      expect(hangUpClick.mock.invocationCallOrder[0]).toBeLessThan(
        browserClose.mock.invocationCallOrder[0],
      );
    } finally {
      errSpy.mockRestore();
    }
  });

  describe("client with a replayed SSO state", () => {
    let dir: string;
    beforeEach(() => {
      dir = mkdtempSync(join(tmpdir(), "bots-db-"));
      writeFileSync(join(dir, "sso.json"), '{"cookies":[],"origins":[]}');
    });
    afterEach(() => rmSync(dir, { recursive: true, force: true }));
    const ssoOpts = () => ({
      ...baseOpts,
      authBackend: "jwt" as const,
      ssoStateFile: join(dir, "sso.json"),
      decodeBudget: "client" as const,
    });

    it("reads a stray override back, warns, and keeps the bot running", async () => {
      storage("4");
      const errors: string[] = [];
      const errSpy = vi.spyOn(console, "error").mockImplementation((...a: unknown[]) => {
        errors.push(a.map(String).join(" "));
      });
      try {
        const bot = await launchBot(ssoOpts());
        expect(bot.decodeBudgetReadback).toBe("4");
        expect(browserClose).not.toHaveBeenCalled();
        expect(errors.join("\n")).toContain(
          "decode budget client requested but vc_decode_budget_override=4 is in storage",
        );
      } finally {
        errSpy.mockRestore();
      }
    });

    it("mints the session cookie to expire by the given deadline, rounded down to a second", async () => {
      storage(null);
      mocks.applyJwtCookieAuth.mockClear();
      await launchBot({ ...ssoOpts(), sessionNotAfterMs: 1_791_000_000_999 });
      await launchBot(ssoOpts());
      expect(mocks.applyJwtCookieAuth.mock.calls.map((c) => c[1].notAfterSecs)).toEqual([
        1_791_000_000,
        undefined,
      ]);
    });

    it("stays quiet when the replayed storage has no override", async () => {
      storage(null);
      const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
      try {
        const bot = await launchBot(ssoOpts());
        expect(readbacks()).toHaveLength(1);
        expect(bot.decodeBudgetReadback).toBeNull();
        expect(errSpy).not.toHaveBeenCalled();
      } finally {
        errSpy.mockRestore();
      }
    });
  });
});

describe("launchBot join media not applied", () => {
  let browserClose: ReturnType<typeof vi.fn>;
  let contextClose: ReturnType<typeof vi.fn>;
  let hangUpVisible: () => Promise<boolean>;
  let goto: ReturnType<typeof vi.fn>;
  let newContext: ReturnType<typeof vi.fn>;
  const hangUpClick = vi.fn();
  const opts = {
    meetingURL: "https://example.test/meeting/MediaTest",
    participant: "media-bot",
    displayName: "Media Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
  };

  beforeEach(() => {
    vi.clearAllMocks();
    browserClose = vi.fn().mockResolvedValue(undefined);
    contextClose = vi.fn().mockResolvedValue(undefined);
    hangUpClick.mockReset().mockResolvedValue(undefined);
    hangUpVisible = async () => true;
    goto = vi.fn().mockResolvedValue(undefined);
    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto,
      locator: vi.fn((selector: string) => ({
        isVisible: () => (selector === HANG_UP_SELECTOR ? hangUpVisible() : Promise.resolve(false)),
        click: hangUpClick,
      })),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/MediaTest"),
      waitForURL: vi.fn().mockResolvedValue(undefined),
    };
    newContext = vi.fn().mockResolvedValue({
      close: contextClose,
      newPage: vi.fn().mockResolvedValue(page),
    });
    mocks.launch.mockResolvedValue({ close: browserClose, newContext, on: vi.fn() });
    mocks.addInitScript.mockResolvedValue(undefined);
    mocks.pageEvaluate.mockReset().mockResolvedValue(undefined);
    mocks.joinMeetingAndEnableMedia.mockRejectedValue(
      new JoinMediaNotAppliedError("camera", true, false),
    );
  });
  const settle = (launched: Promise<unknown>): { outcome: () => unknown } => {
    let outcome: unknown = "pending";
    void launched.then(
      () => (outcome = "resolved"),
      (e: unknown) => (outcome = e),
    );
    return { outcome: () => outcome };
  };

  it("stops the drivers it started before rejecting when the abort lands after the join", async () => {
    vi.useFakeTimers();
    const lines: string[] = [];
    const log = vi.spyOn(console, "log").mockImplementation((...a) => void lines.push(a.join(" ")));
    const err = vi
      .spyOn(console, "error")
      .mockImplementation((...a) => void lines.push(a.join(" ")));
    const abort = new AbortController();
    mocks.joinMeetingAndEnableMedia.mockImplementation(async () => {
      abort.abort();
      return { camera: true, mic: true };
    });
    mocks.pageEvaluate.mockImplementation(async (_fn: unknown, arg: unknown) => {
      await new Promise((r) => setTimeout(r, 100));
      return arg === DECODE_BUDGET_STORAGE_KEY ? DECODE_BUDGET_OFF_VALUE : 30;
    });
    let receiptAtReject = false;
    const launched = launchBot({
      ...opts,
      signal: abort.signal,
      onEncoderFps: () => {},
      decodeBudget: "off",
      cameraCycle: { onMinMs: 5_000, onMaxMs: 15_000, offMinMs: 20_000, offMaxMs: 60_000 },
    });
    launched.catch(() => {
      receiptAtReject = lines.some((l) => l.includes(CAMERA_CYCLE_NEVER_FIRED_BANNER));
    });
    const run = settle(launched);
    try {
      await vi.advanceTimersByTimeAsync(20_000);
      expect(run.outcome()).toBeInstanceOf(LaunchAbortedError);
      expect(receiptAtReject).toBe(true);
      const polls = mocks.pageEvaluate.mock.calls.length;
      await vi.advanceTimersByTimeAsync(5 * ENCODER_FPS_POLL_MS);
      expect(mocks.pageEvaluate.mock.calls.length).toBe(polls);
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      log.mockRestore();
      err.mockRestore();
    }
  });

  it.each([
    ["browser.close", () => browserClose.mockReturnValue(new Promise(() => {}))],
    ["context.close", () => contextClose.mockReturnValue(new Promise(() => {}))],
  ] as const)(
    "shuts down a bot whose launch completed just before the abort, then rejects, with %s stalled",
    async (_what, stall) => {
      vi.useFakeTimers();
      const lines: string[] = [];
      const log = vi
        .spyOn(console, "log")
        .mockImplementation((...a) => void lines.push(a.join(" ")));
      const err = vi
        .spyOn(console, "error")
        .mockImplementation((...a) => void lines.push(a.join(" ")));
      mocks.joinMeetingAndEnableMedia.mockResolvedValue({ camera: true, mic: true });
      mocks.pageEvaluate.mockResolvedValue(30);
      stall();
      // The body's last check reads false; the abort fires on the wrapper's next read.
      const listeners: Array<() => void> = [];
      let readsAfterStart = 0;
      const signal = {
        get aborted(): boolean {
          if (!lines.some((l) => l.includes("camera cycle configured"))) return false;
          readsAfterStart++;
          if (readsAfterStart === 2) listeners.splice(0).forEach((fn) => fn());
          return readsAfterStart >= 2;
        },
        addEventListener: (_type: string, fn: () => void) => void listeners.push(fn),
        removeEventListener: (_type: string, fn: () => void) => {
          const i = listeners.indexOf(fn);
          if (i >= 0) listeners.splice(i, 1);
        },
      } as unknown as AbortSignal;
      const startedAt = Date.now();
      let rejectedAt = Infinity;
      const launched = launchBot({
        ...opts,
        signal,
        onEncoderFps: () => {},
        cameraCycle: { onMinMs: 5_000, onMaxMs: 15_000, offMinMs: 20_000, offMaxMs: 60_000 },
      });
      launched.catch(() => {
        rejectedAt = Date.now();
      });
      const run = settle(launched);
      try {
        await vi.advanceTimersByTimeAsync(20_000);
        expect(run.outcome()).toBeInstanceOf(LaunchAbortedError);
        expect(rejectedAt - startedAt).toBeLessThanOrEqual(10_000);
        expect(vi.getTimerCount()).toBe(0);
        expect(browserClose).toHaveBeenCalledTimes(1);
      } finally {
        log.mockRestore();
        err.mockRestore();
      }
    },
  );

  it.each([
    ["settles", () => {}],
    ["stalls", () => contextClose.mockReturnValue(new Promise(() => {}))],
  ] as const)(
    "closes the browser once, within the close budget, when the abort lands during a pending read-back and context.close %s",
    async (_what, contextBehaviour) => {
      vi.useFakeTimers();
      contextBehaviour();
      const quiet = (["log", "warn", "error"] as const).map((m) =>
        vi.spyOn(console, m).mockImplementation(() => {}),
      );
      mocks.joinMeetingAndEnableMedia.mockResolvedValue({ camera: true, mic: true });
      mocks.pageEvaluate.mockReturnValue(new Promise(() => {}));
      const abort = new AbortController();
      let rejectedAfterAbort = Infinity;
      const launched = launchBot({ ...opts, decodeBudget: "off", signal: abort.signal });
      const run = settle(launched);
      try {
        await vi.advanceTimersByTimeAsync(10);
        expect(mocks.pageEvaluate).toHaveBeenCalled();
        const abortedAt = Date.now();
        launched.catch(() => (rejectedAfterAbort = Date.now() - abortedAt));
        abort.abort();
        await vi.advanceTimersByTimeAsync(30_000);
        expect(run.outcome()).toBeInstanceOf(LaunchAbortedError);
        expect(rejectedAfterAbort).toBeLessThanOrEqual(2 * CLOSE_STEP_BUDGET_MS);
        expect(contextClose).toHaveBeenCalledTimes(1);
        expect(browserClose).toHaveBeenCalledTimes(1);
      } finally {
        for (const spy of quiet) spy.mockRestore();
      }
    },
  );

  it("shutdown closes the browser once, within the close budget, when context.close stalls", async () => {
    vi.useFakeTimers();
    mocks.joinMeetingAndEnableMedia.mockResolvedValue({ camera: true, mic: true });
    mocks.pageEvaluate.mockResolvedValue(30);
    const quiet = (["log", "error"] as const).map((m) =>
      vi.spyOn(console, m).mockImplementation(() => {}),
    );
    try {
      const bot = await launchBot(opts);
      contextClose.mockReturnValue(new Promise(() => {}));
      let done = false;
      void bot.shutdown().then(() => (done = true));
      await vi.advanceTimersByTimeAsync(2 * CLOSE_STEP_BUDGET_MS);
      expect(done).toBe(true);
      await bot.closeBrowserNow();
      expect(browserClose).toHaveBeenCalledTimes(1);
    } finally {
      for (const spy of quiet) spy.mockRestore();
    }
  });

  it("bounds the hang-up on the decode-budget failure path too", async () => {
    vi.useFakeTimers();
    hangUpVisible = () => new Promise<boolean>(() => {});
    mocks.joinMeetingAndEnableMedia.mockResolvedValue({ camera: true, mic: true });
    mocks.pageEvaluate.mockImplementation(async (_fn: unknown, arg: unknown) =>
      arg === DECODE_BUDGET_STORAGE_KEY ? "auto" : undefined,
    );
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const run = settle(launchBot({ ...opts, decodeBudget: "off" }));
      await vi.advanceTimersByTimeAsync(15_000);
      expect(run.outcome()).toBeInstanceOf(DecodeBudgetNotAppliedError);
      expect(browserClose).toHaveBeenCalledTimes(1);
    } finally {
      errSpy.mockRestore();
    }
  });

  it("closes the browser of a launch still joining at the run deadline", async () => {
    mocks.joinMeetingAndEnableMedia.mockImplementation(
      () =>
        new Promise((_resolve, reject) => {
          browserClose.mockImplementation(async () => {
            reject(new Error("Target page, context or browser has been closed"));
          });
        }),
    );
    const reasons: Array<string | undefined> = [];
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      await runBotsToCompletion({
        tasks: [
          {
            ...opts,
            botId: "00000000-0000-0000-0000-00000000dead",
            ttl: 60_000,
            cameraCycle: null,
            runDeadlineMs: Date.now() + 1_500,
          },
        ],
        onFinish: (_id, _at, reason) => reasons.push(reason),
      });
    } finally {
      errSpy.mockRestore();
    }
    expect(browserClose).toHaveBeenCalled();
    expect(reasons).toHaveLength(1);
  });

  it("closes the browser and rejects as aborted while the context is still being created", async () => {
    vi.useFakeTimers();
    newContext.mockReturnValue(new Promise(() => {}));
    const abort = new AbortController();
    const run = settle(launchBot({ ...opts, signal: abort.signal }));
    await vi.advanceTimersByTimeAsync(10);
    abort.abort();
    await vi.advanceTimersByTimeAsync(20_000);
    expect(run.outcome()).toBeInstanceOf(LaunchAbortedError);
    expect(browserClose).toHaveBeenCalled();
  });

  it("closes a browser that finishes launching after the abort before creating its context", async () => {
    let browserReady!: () => void;
    const browser = { close: browserClose, newContext, on: vi.fn() };
    mocks.launch.mockReturnValue(
      new Promise((resolve) => {
        browserReady = () => resolve(browser);
      }),
    );
    const abort = new AbortController();
    const launched = launchBot({ ...opts, signal: abort.signal });
    abort.abort();
    browserReady();
    await expect(launched).rejects.toBeInstanceOf(LaunchAbortedError);
    expect(browserClose).toHaveBeenCalled();
    expect(newContext.mock.invocationCallOrder[0] ?? Infinity).toBeGreaterThan(
      browserClose.mock.invocationCallOrder[0],
    );
  });

  it("hands its signal to the join", async () => {
    vi.useFakeTimers();
    const abort = new AbortController();
    mocks.joinMeetingAndEnableMedia.mockImplementation(
      ({ signal }: { signal?: AbortSignal }) =>
        new Promise((_resolve, reject) => {
          signal?.addEventListener("abort", () => reject(new Error("join stopped")));
        }),
    );
    const run = settle(launchBot({ ...opts, signal: abort.signal }));
    await vi.advanceTimersByTimeAsync(10);
    expect(mocks.joinMeetingAndEnableMedia).toHaveBeenCalledTimes(1);
    abort.abort();
    await vi.advanceTimersByTimeAsync(100);
    expect(run.outcome()).toBeInstanceOf(LaunchAbortedError);
  });

  it("does not hang up or report a media failure when an abort stopped the join's retries", async () => {
    vi.useFakeTimers();
    const abort = new AbortController();
    mocks.joinMeetingAndEnableMedia.mockImplementation(
      ({ signal }: { signal?: AbortSignal }) =>
        new Promise((_resolve, reject) => {
          signal?.addEventListener("abort", () =>
            reject(new JoinMediaNotAppliedError("camera", true, null)),
          );
        }),
    );
    const errors: string[] = [];
    const err = vi
      .spyOn(console, "error")
      .mockImplementation((...a) => void errors.push(a.join(" ")));
    try {
      const run = settle(launchBot({ ...opts, signal: abort.signal }));
      await vi.advanceTimersByTimeAsync(10);
      abort.abort();
      await vi.advanceTimersByTimeAsync(20_000);
      expect(run.outcome()).toBeInstanceOf(LaunchAbortedError);
    } finally {
      err.mockRestore();
    }
    expect(hangUpClick).not.toHaveBeenCalled();
    expect(errors.filter((l) => l.includes("requested on but reads"))).toEqual([]);
  });

  it("rejects an aborted launch only once its browser has closed", async () => {
    vi.useFakeTimers();
    const abort = new AbortController();
    let stopJoin!: (e: Error) => void;
    mocks.joinMeetingAndEnableMedia.mockImplementation(
      () => new Promise((_resolve, reject) => (stopJoin = reject)),
    );
    let closed = false;
    browserClose.mockImplementation(async () => {
      stopJoin(new Error("Target page, context or browser has been closed"));
      await new Promise((r) => setTimeout(r, 1_000));
      closed = true;
    });
    let closedAtReject: boolean | null = null;
    const launched = launchBot({ ...opts, signal: abort.signal });
    launched.catch(() => (closedAtReject = closed));
    const run = settle(launched);
    await vi.advanceTimersByTimeAsync(10);
    abort.abort();
    await vi.advanceTimersByTimeAsync(20_000);
    expect(run.outcome()).toBeInstanceOf(LaunchAbortedError);
    expect(closedAtReject).toBe(true);
  });

  it("rejects as aborted, not as a launch error, when the abort surfaces as a navigation failure", async () => {
    const abort = new AbortController();
    goto.mockImplementation(async () => {
      abort.abort();
      throw new Error("page.goto: net::ERR_ABORTED");
    });
    await expect(launchBot({ ...opts, signal: abort.signal })).rejects.toBeInstanceOf(
      LaunchAbortedError,
    );
  });

  it("drops its abort listener once a launch has failed", async () => {
    const abort = new AbortController();
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      await expect(launchBot({ ...opts, signal: abort.signal })).rejects.toBeInstanceOf(
        JoinMediaNotAppliedError,
      );
    } finally {
      errSpy.mockRestore();
    }
    const closes = browserClose.mock.calls.length;
    abort.abort();
    await new Promise((r) => setTimeout(r, 10));
    expect(browserClose.mock.calls.length).toBe(closes);
  });
  afterEach(() => {
    mocks.joinMeetingAndEnableMedia.mockReset();
    vi.useRealTimers();
  });

  it("hangs up, closes the browser and fails the launch", async () => {
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const launched = launchBot(opts);
      await expect(launched).rejects.toBeInstanceOf(JoinMediaNotAppliedError);
      expect(hangUpClick).toHaveBeenCalledTimes(1);
      expect(browserClose).toHaveBeenCalledTimes(1);
      expect(hangUpClick.mock.invocationCallOrder[0]).toBeLessThan(
        browserClose.mock.invocationCallOrder[0],
      );
    } finally {
      errSpy.mockRestore();
    }
  });

  it.each([
    ["a hang-up whose control read never returns", "hang-up"],
    ["a context close that never returns", "context"],
  ] as const)("still closes the browser and fails the launch after %s", async (_what, stall) => {
    vi.useFakeTimers();
    if (stall === "hang-up") hangUpVisible = () => new Promise<boolean>(() => {});
    else contextClose.mockReturnValue(new Promise<void>(() => {}));
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      let outcome: unknown = "pending";
      void launchBot(opts).then(
        () => (outcome = "resolved"),
        (e: unknown) => (outcome = e),
      );
      await vi.advanceTimersByTimeAsync(15_000);
      expect(outcome).toBeInstanceOf(JoinMediaNotAppliedError);
      expect(browserClose).toHaveBeenCalledTimes(1);
    } finally {
      errSpy.mockRestore();
    }
  });

  it("rejects as aborted when the abort lands after the join returned", async () => {
    const abort = new AbortController();
    mocks.joinMeetingAndEnableMedia.mockImplementation(async () => {
      abort.abort();
      return { camera: true, mic: true };
    });
    await expect(launchBot({ ...opts, signal: abort.signal })).rejects.toBeInstanceOf(
      LaunchAbortedError,
    );
    expect(browserClose).toHaveBeenCalled();
  });

  it("closes the browser at once and rejects as aborted when the launch is aborted mid-join", async () => {
    const abort = new AbortController();
    mocks.joinMeetingAndEnableMedia.mockImplementation(
      () =>
        new Promise((_resolve, reject) => {
          browserClose.mockImplementation(async () => {
            reject(new Error("Target page, context or browser has been closed"));
          });
        }),
    );
    let outcome: unknown = "pending";
    void launchBot({ ...opts, signal: abort.signal }).then(
      () => (outcome = "resolved"),
      (e: unknown) => (outcome = e),
    );
    await new Promise((r) => setTimeout(r, 10));
    abort.abort();
    await new Promise((r) => setTimeout(r, 50));
    expect(outcome).toBeInstanceOf(LaunchAbortedError);
    expect(browserClose).toHaveBeenCalled();
    expect(hangUpClick).not.toHaveBeenCalled();
  });

  it("rejects an already-aborted launch without starting Chromium", async () => {
    const abort = new AbortController();
    abort.abort();
    await expect(launchBot({ ...opts, signal: abort.signal })).rejects.toBeInstanceOf(
      LaunchAbortedError,
    );
    expect(mocks.launch).not.toHaveBeenCalled();
  });

  it("does not report the browser close of an aborted launch as a crash", async () => {
    const abort = new AbortController();
    mocks.joinMeetingAndEnableMedia.mockImplementation(
      () =>
        new Promise((_resolve, reject) => {
          browserClose.mockImplementation(async () => {
            reject(new Error("Target page, context or browser has been closed"));
          });
        }),
    );
    const errors: string[] = [];
    const errSpy = vi.spyOn(console, "error").mockImplementation((...a: unknown[]) => {
      errors.push(a.map(String).join(" "));
    });
    try {
      const launched = launchBot({ ...opts, signal: abort.signal });
      await vi.waitFor(() => expect(mocks.joinMeetingAndEnableMedia).toHaveBeenCalled());
      abort.abort();
      await expect(launched).rejects.toBeInstanceOf(LaunchAbortedError);
      expect(browserClose).toHaveBeenCalledTimes(1);
      const browser = (await mocks.launch.mock.results[0].value) as {
        on: ReturnType<typeof vi.fn>;
      };
      const disconnected = browser.on.mock.calls.find((c) => c[0] === "disconnected")?.[1];
      (disconnected as () => void)();
      expect(errors.join("\n")).not.toContain("disconnected unexpectedly");
    } finally {
      errSpy.mockRestore();
    }
  });
});

describe("launchBot speaking glow (#2914)", () => {
  let goto: ReturnType<typeof vi.fn>;
  let browserClose: ReturnType<typeof vi.fn>;
  let hangUpClick: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    vi.clearAllMocks();
    goto = vi.fn().mockResolvedValue(undefined);
    browserClose = vi.fn().mockResolvedValue(undefined);
    hangUpClick = vi.fn().mockResolvedValue(undefined);
    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto,
      locator: vi.fn((selector: string) => ({
        isVisible: async () => selector === HANG_UP_SELECTOR,
        click: hangUpClick,
      })),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/GlowTest"),
      waitForURL: vi.fn().mockResolvedValue(undefined),
    };
    const context = {
      close: vi.fn().mockResolvedValue(undefined),
      newPage: vi.fn().mockResolvedValue(page),
    };
    mocks.launch.mockResolvedValue({
      close: browserClose,
      newContext: vi.fn().mockResolvedValue(context),
      on: vi.fn(),
    });
    mocks.addInitScript.mockResolvedValue(undefined);
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
  });

  const baseOpts = {
    meetingURL: "https://example.test/meeting/GlowTest",
    participant: "glow-bot",
    displayName: "Glow Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
  };
  const SEED = buildSpeakingGlowInitScript("https://example.test");
  // Runs the production readback callback against a stub page localStorage.
  const storage = (value: string | null | Error) =>
    mocks.pageEvaluate.mockImplementation(async (fn: unknown, arg: unknown) => {
      if (arg !== SPEAKING_GLOW_STORAGE_KEY) return undefined;
      const stub = {
        getItem: (key: string) => {
          if (value instanceof Error) throw value;
          return key === SPEAKING_GLOW_STORAGE_KEY ? value : null;
        },
      };
      const prior = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
      Object.defineProperty(globalThis, "localStorage", { value: stub, configurable: true });
      try {
        return (fn as (key: string) => unknown)(arg);
      } finally {
        if (prior) Object.defineProperty(globalThis, "localStorage", prior);
        else delete (globalThis as { localStorage?: unknown }).localStorage;
      }
    });
  const readbacks = () =>
    mocks.pageEvaluate.mock.calls.filter((c) => c[1] === SPEAKING_GLOW_STORAGE_KEY);

  it("off seeds the preference before the first navigation and reads it back after join", async () => {
    storage("false");
    const bot = await launchBot({ ...baseOpts, speakingGlow: "off" });
    const idx = mocks.addInitScript.mock.calls.findIndex((c) => c[0] === SEED);
    expect(idx).toBeGreaterThanOrEqual(0);
    expect(mocks.addInitScript.mock.invocationCallOrder[idx]).toBeLessThan(
      goto.mock.invocationCallOrder[0],
    );
    expect(readbacks()).toHaveLength(1);
    const read = mocks.pageEvaluate.mock.calls.indexOf(readbacks()[0]);
    expect(mocks.pageEvaluate.mock.invocationCallOrder[read]).toBeGreaterThan(
      mocks.joinMeetingAndEnableMedia.mock.invocationCallOrder[0],
    );
    expect(bot.speakingGlowReadback).toBe("false");
    expect(browserClose).not.toHaveBeenCalled();
  });

  it.each([undefined, null, "client"] as const)(
    "speakingGlow %j installs nothing and reads nothing back",
    async (speakingGlow) => {
      storage("false");
      const bot = await launchBot({ ...baseOpts, speakingGlow });
      const touched = mocks.addInitScript.mock.calls.some(
        (c) => typeof c[0] === "string" && c[0].includes(SPEAKING_GLOW_STORAGE_KEY),
      );
      expect(touched).toBe(false);
      expect(readbacks()).toHaveLength(0);
      expect(bot.speakingGlowReadback).toBeNull();
    },
  );

  it.each([
    ["absent", null],
    ["a different value", "true"],
    ["a storage read that throws", new Error("SecurityError: localStorage is not available")],
  ] as const)("hangs up, closes the browser and fails the launch on %s", async (_what, value) => {
    storage(value);
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const launched = launchBot({ ...baseOpts, speakingGlow: "off" });
      await expect(launched).rejects.toBeInstanceOf(SpeakingGlowNotAppliedError);
      await expect(launched).rejects.toMatchObject({
        observed: value instanceof Error ? null : value,
      });
      expect(browserClose).toHaveBeenCalledTimes(1);
      expect(hangUpClick).toHaveBeenCalledTimes(1);
      expect(hangUpClick.mock.invocationCallOrder[0]).toBeLessThan(
        browserClose.mock.invocationCallOrder[0],
      );
    } finally {
      errSpy.mockRestore();
    }
  });

  describe("client with a replayed SSO state", () => {
    let dir: string;
    beforeEach(() => {
      dir = mkdtempSync(join(tmpdir(), "bots-glow-"));
      writeFileSync(join(dir, "sso.json"), '{"cookies":[],"origins":[]}');
    });
    afterEach(() => rmSync(dir, { recursive: true, force: true }));
    const ssoOpts = () => ({
      ...baseOpts,
      authBackend: "jwt" as const,
      ssoStateFile: join(dir, "sso.json"),
      speakingGlow: "client" as const,
    });

    it("reads a stored glow-off back, warns, and keeps the bot running", async () => {
      storage("false");
      const errors: string[] = [];
      const errSpy = vi.spyOn(console, "error").mockImplementation((...a: unknown[]) => {
        errors.push(a.map(String).join(" "));
      });
      try {
        const bot = await launchBot(ssoOpts());
        expect(bot.speakingGlowReadback).toBe("false");
        expect(browserClose).not.toHaveBeenCalled();
        expect(errors.join("\n")).toContain(
          "speaking glow client requested but vc_appearance_glow_enabled=false is in storage",
        );
      } finally {
        errSpy.mockRestore();
      }
    });
  });
});

describe("launchBot simulcast max layers (#3246)", () => {
  let goto: ReturnType<typeof vi.fn>;
  let browserClose: ReturnType<typeof vi.fn>;
  let hangUpClick: ReturnType<typeof vi.fn>;
  let hangUpVisible: ReturnType<typeof vi.fn<(selector: string) => Promise<boolean>>>;
  let contextClose: ReturnType<typeof vi.fn>;
  const g = globalThis as { __APP_CONFIG?: Record<string, unknown> };

  beforeEach(() => {
    vi.clearAllMocks();
    goto = vi.fn().mockResolvedValue(undefined);
    browserClose = vi.fn().mockResolvedValue(undefined);
    hangUpClick = vi.fn().mockResolvedValue(undefined);
    hangUpVisible = vi.fn<(selector: string) => Promise<boolean>>(
      async (selector) => selector === HANG_UP_SELECTOR,
    );
    contextClose = vi.fn().mockResolvedValue(undefined);
    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto,
      locator: vi.fn((selector: string) => ({
        isVisible: () => hangUpVisible(selector),
        click: hangUpClick,
      })),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/SimTest"),
      waitForURL: vi.fn().mockResolvedValue(undefined),
    };
    mocks.launch.mockResolvedValue({
      close: browserClose,
      newContext: vi.fn().mockResolvedValue({
        close: contextClose,
        newPage: vi.fn().mockResolvedValue(page),
      }),
      on: vi.fn(),
    });
    mocks.addInitScript.mockResolvedValue(undefined);
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
    // Runs the real readback function against a stand-in page global.
    mocks.pageEvaluate.mockReset();
    mocks.pageEvaluate.mockImplementation(async (fn: unknown, arg: unknown) =>
      (fn as (a: unknown) => unknown)(arg),
    );
  });
  afterEach(() => {
    delete g.__APP_CONFIG;
    mocks.pageEvaluate.mockReset();
  });

  const baseOpts = {
    meetingURL: "https://example.test/meeting/SimTest",
    participant: "sim-bot",
    displayName: "Sim Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
  };
  const injected = (): string[] =>
    mocks.addInitScript.mock.calls
      .map((c) => String(c[0]))
      .filter((s) => s.includes(SIMULCAST_MAX_LAYERS_CONFIG_KEY));

  it("injects N into the __APP_CONFIG setter-merge before navigating and reads it back after join", async () => {
    g.__APP_CONFIG = { [SIMULCAST_MAX_LAYERS_CONFIG_KEY]: 3 };
    const bot = await launchBot({ ...baseOpts, simulcastMaxLayers: 3 });
    expect(injected()).toHaveLength(1);
    expect(injected()[0]).toContain(`"${SIMULCAST_MAX_LAYERS_CONFIG_KEY}":3`);
    expect(injected()[0]).toContain("Object.defineProperty(window, '__APP_CONFIG'");
    const idx = mocks.addInitScript.mock.calls.findIndex((c) => String(c[0]) === injected()[0]);
    expect(mocks.addInitScript.mock.invocationCallOrder[idx]).toBeLessThan(
      goto.mock.invocationCallOrder[0],
    );
    const readback = mocks.pageEvaluate.mock.calls.findIndex(
      (c) => c[1] === SIMULCAST_MAX_LAYERS_CONFIG_KEY,
    );
    expect(mocks.pageEvaluate.mock.invocationCallOrder[readback]).toBeGreaterThan(
      mocks.joinMeetingAndEnableMedia.mock.invocationCallOrder[0],
    );
    expect(bot.simulcastMaxLayersReadback).toBe(3);
    expect(browserClose).not.toHaveBeenCalled();
  });

  it.each([undefined, null] as const)(
    "simulcastMaxLayers %j injects nothing and still reports the deployment's value",
    async (simulcastMaxLayers) => {
      g.__APP_CONFIG = { [SIMULCAST_MAX_LAYERS_CONFIG_KEY]: 1 };
      const bot = await launchBot({ ...baseOpts, simulcastMaxLayers });
      expect(injected()).toEqual([]);
      expect(bot.simulcastMaxLayersReadback).toBe(1);
      delete g.__APP_CONFIG;
      expect(
        (await launchBot({ ...baseOpts, simulcastMaxLayers })).simulcastMaxLayersReadback,
      ).toBe(null);
      expect(browserClose).not.toHaveBeenCalled();
    },
  );

  it.each([
    ["the deployment's value", { [SIMULCAST_MAX_LAYERS_CONFIG_KEY]: 1 }, 1],
    ["an absent key", {}, null],
    ["a string", { [SIMULCAST_MAX_LAYERS_CONFIG_KEY]: "3" }, null],
    [
      "an evaluate failure",
      new Error("Execution context was destroyed"),
      SIMULCAST_READBACK_UNREAD,
    ],
  ] as const)(
    "hangs up, closes the browser and fails the launch on %s",
    async (_what, config, observed) => {
      if (config instanceof Error) {
        mocks.pageEvaluate.mockImplementation(async (fn: unknown, arg: unknown) => {
          if (arg === SIMULCAST_MAX_LAYERS_CONFIG_KEY) throw config;
          return (fn as (a: unknown) => unknown)(arg);
        });
      } else {
        g.__APP_CONFIG = { ...config };
      }
      const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
      try {
        const launched = launchBot({ ...baseOpts, simulcastMaxLayers: 3 });
        await expect(launched).rejects.toBeInstanceOf(SimulcastMaxLayersNotAppliedError);
        await expect(launched).rejects.toMatchObject({ requested: 3, observed });
        expect(hangUpClick).toHaveBeenCalledTimes(1);
        expect(browserClose).toHaveBeenCalledTimes(1);
        expect(hangUpClick.mock.invocationCallOrder[0]).toBeLessThan(
          browserClose.mock.invocationCallOrder[0],
        );
      } finally {
        errSpy.mockRestore();
      }
    },
  );

  const NEVER = (): Promise<never> => new Promise<never>(() => {});
  async function launchWithStalledReadback(
    simulcastMaxLayers: number | undefined,
  ): Promise<unknown> {
    mocks.pageEvaluate.mockImplementation((fn: unknown, arg: unknown) =>
      arg === SIMULCAST_MAX_LAYERS_CONFIG_KEY
        ? NEVER()
        : Promise.resolve((fn as (a: unknown) => unknown)(arg)),
    );
    const PENDING = Symbol("pending");
    const settled = launchBot({ ...baseOpts, simulcastMaxLayers }).then(
      (bot) => bot,
      (e: unknown) => e,
    );
    await vi.advanceTimersByTimeAsync(SIMULCAST_READBACK_TIMEOUT_MS + 5_000);
    return Promise.race([settled, Promise.resolve(PENDING)]);
  }

  it("a requested N on a stalled renderer fails at the read-back limit, with every evaluate, hang-up probe and context close stalled", async () => {
    vi.useFakeTimers();
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      mocks.pageEvaluate.mockImplementation(NEVER);
      hangUpVisible.mockImplementation(NEVER);
      contextClose.mockImplementation(NEVER);
      const PENDING = Symbol("pending");
      const settled = launchBot({ ...baseOpts, simulcastMaxLayers: 3, skipCanvasPaint: true }).then(
        (bot) => bot,
        (e: unknown) => e,
      );
      await vi.advanceTimersByTimeAsync(SIMULCAST_READBACK_TIMEOUT_MS);
      const outcome = await Promise.race([settled, Promise.resolve(PENDING)]);
      expect(outcome).toBeInstanceOf(SimulcastMaxLayersReadbackTimeoutError);
      expect(browserClose).toHaveBeenCalledTimes(1);
    } finally {
      errSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("reads N back once and leaves the receiver-caps check to the other keys", async () => {
    const logSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const lines = (spy: typeof logSpy): string[] => spy.mock.calls.map((c) => String(c[0]));
    try {
      g.__APP_CONFIG = { [SIMULCAST_MAX_LAYERS_CONFIG_KEY]: 3 };
      await launchBot({ ...baseOpts, simulcastMaxLayers: 3 });
      expect(mocks.pageEvaluate).toHaveBeenCalledTimes(1);
      expect(lines(logSpy).filter((l) => l.includes("verified"))).toHaveLength(1);

      g.__APP_CONFIG = { [SIMULCAST_MAX_LAYERS_CONFIG_KEY]: 1, skipCanvasPaint: "true" };
      await expect(
        launchBot({ ...baseOpts, simulcastMaxLayers: 3, skipCanvasPaint: true }),
      ).rejects.toBeInstanceOf(SimulcastMaxLayersNotAppliedError);
      expect(lines(errSpy).filter((l) => l.includes("may not be capped"))).toEqual([]);
    } finally {
      logSpy.mockRestore();
      errSpy.mockRestore();
    }
  });

  it("an unrequested bot whose read-back stalls keeps running and reports it unread", async () => {
    vi.useFakeTimers();
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const outcome = (await launchWithStalledReadback(undefined)) as {
        simulcastMaxLayersReadback?: unknown;
      };
      expect(outcome).not.toBeInstanceOf(Error);
      expect(outcome.simulcastMaxLayersReadback).toBe(SIMULCAST_READBACK_UNREAD);
      expect(hangUpClick).not.toHaveBeenCalled();
      expect(browserClose).not.toHaveBeenCalled();
    } finally {
      errSpy.mockRestore();
      vi.useRealTimers();
    }
  });

  it("an unrequested bot whose read-back throws reports it unread, not absent", async () => {
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      mocks.pageEvaluate.mockImplementation(async (fn: unknown, arg: unknown) => {
        if (arg === SIMULCAST_MAX_LAYERS_CONFIG_KEY)
          throw new Error("Execution context was destroyed");
        return (fn as (a: unknown) => unknown)(arg);
      });
      const bot = await launchBot(baseOpts);
      expect(bot.simulcastMaxLayersReadback).toBe(SIMULCAST_READBACK_UNREAD);
      expect(browserClose).not.toHaveBeenCalled();
    } finally {
      errSpy.mockRestore();
    }
  });
});

describe("launchBot startup time limits", () => {
  const NEVER = (): Promise<never> => new Promise<never>(() => {});
  const PENDING = Symbol("pending");
  let browserClose: ReturnType<typeof vi.fn>;
  let contextClose: ReturnType<typeof vi.fn>;
  let newContext: ReturnType<typeof vi.fn>;
  let newPage: ReturnType<typeof vi.fn>;
  let quiet: ReturnType<typeof vi.spyOn>[];

  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
    quiet = [
      vi.spyOn(console, "log").mockImplementation(() => {}),
      vi.spyOn(console, "error").mockImplementation(() => {}),
    ];
    browserClose = vi.fn().mockResolvedValue(undefined);
    contextClose = vi.fn().mockResolvedValue(undefined);
    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(() => ({ isVisible: async () => false, click: vi.fn() })),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/LimitTest"),
      waitForURL: vi.fn().mockResolvedValue(undefined),
    };
    newPage = vi.fn().mockResolvedValue(page);
    newContext = vi.fn().mockResolvedValue({ close: contextClose, newPage });
    mocks.launch.mockResolvedValue({ close: browserClose, newContext, on: vi.fn() });
    mocks.addInitScript.mockResolvedValue(undefined);
    mocks.applyJwtCookieAuth.mockResolvedValue(undefined);
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
    mocks.pageEvaluate.mockReset();
    mocks.pageEvaluate.mockResolvedValue(undefined);
  });
  afterEach(() => {
    for (const spy of quiet) spy.mockRestore();
    mocks.pageEvaluate.mockReset();
    vi.useRealTimers();
  });

  const baseOpts = {
    meetingURL: "https://example.test/meeting/LimitTest",
    participant: "limit-bot",
    displayName: "Limit Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
  };
  /** The launch's outcome `limitMs - 1` and `limitMs` after it starts. */
  async function aroundLimit(
    opts: Parameters<typeof launchBot>[0],
    limitMs: number,
  ): Promise<[unknown, unknown]> {
    const settled = launchBot(opts).then(
      (bot) => bot,
      (e: unknown) => e,
    );
    const peek = (): Promise<unknown> => Promise.race([settled, Promise.resolve(PENDING)]);
    await vi.advanceTimersByTimeAsync(limitMs - 1);
    const before = await peek();
    await vi.advanceTimersByTimeAsync(1);
    return [before, await peek()];
  }
  const stallEvaluateOn = (arg: unknown): void => {
    mocks.pageEvaluate.mockImplementation((_fn: unknown, a: unknown) =>
      JSON.stringify(a) === JSON.stringify(arg) ? NEVER() : Promise.resolve(undefined),
    );
  };

  it.each([
    ["browser.newContext", {}, () => newContext.mockImplementation(NEVER)],
    [
      "session cookie injection",
      { authBackend: "jwt" as const },
      () => mocks.applyJwtCookieAuth.mockImplementation(NEVER),
    ],
    ["context.newPage", {}, () => newPage.mockImplementation(NEVER)],
  ] as const)(
    "fails a stalled %s at its limit and closes the browser",
    async (what, extra, stall) => {
      stall();
      const [before, after] = await aroundLimit(
        { ...baseOpts, ...extra },
        BROWSER_STEP_TIME_LIMIT_MS,
      );
      expect(before).toBe(PENDING);
      expect(after).toBeInstanceOf(TimeLimitExceededError);
      expect(after).toMatchObject({ what });
      expect(browserClose).toHaveBeenCalledTimes(1);
    },
  );

  const ORIGIN = "https://example.test";
  it.each([
    [
      "hardware-concurrency",
      { hardwareConcurrency: 10 },
      (s: string) => s.includes("hardwareConcurrency"),
    ],
    [
      "transport",
      { transport: "websocket" as const },
      (s: string) => s === buildTransportInitScript(ORIGIN),
    ],
    [
      "__APP_CONFIG",
      { skipCanvasPaint: true },
      (s: string) => s.includes("Object.defineProperty(window, '__APP_CONFIG'"),
    ],
    [
      "decode-budget",
      { decodeBudget: "off" as const },
      (s: string) => s === buildDecodeBudgetInitScript(ORIGIN),
    ],
    [
      "speaking-glow",
      { speakingGlow: "off" as const },
      (s: string) => s === buildSpeakingGlowInitScript(ORIGIN),
    ],
    ["clock", {}, (s: string) => s.includes("__CLOCK_PARTICIPANT")],
  ] as const)(
    "fails a launch whose %s init script stalls at its limit",
    async (_what, extra, isSite) => {
      let stalled = 0;
      mocks.addInitScript.mockImplementation((source: string) => {
        if (!isSite(source)) return Promise.resolve();
        stalled += 1;
        return NEVER();
      });
      const [before, after] = await aroundLimit(
        { ...baseOpts, ...extra },
        BROWSER_STEP_TIME_LIMIT_MS,
      );
      expect(stalled).toBe(1);
      expect(before).toBe(PENDING);
      expect(after).toMatchObject({ what: "page.addInitScript" });
      expect(browserClose).toHaveBeenCalledTimes(1);
    },
  );

  it("keeps a bot whose receiver-caps read-back stalls running past its limit", async () => {
    stallEvaluateOn(["skipCanvasPaint"]);
    const [before, after] = await aroundLimit(
      { ...baseOpts, skipCanvasPaint: true },
      PAGE_CALL_TIME_LIMIT_MS,
    );
    expect(before).toBe(PENDING);
    expect(after).toMatchObject({ skipCanvasPaintReadback: null });
    expect(browserClose).not.toHaveBeenCalled();
  });

  it.each([
    [
      "decode-budget",
      { decodeBudget: "off" as const },
      DECODE_BUDGET_STORAGE_KEY,
      DecodeBudgetNotAppliedError,
    ],
    [
      "speaking-glow",
      { speakingGlow: "off" as const },
      SPEAKING_GLOW_STORAGE_KEY,
      SpeakingGlowNotAppliedError,
    ],
  ] as const)(
    "fails an off %s bot whose read-back stalls at its limit",
    async (_what, extra, key, Err) => {
      stallEvaluateOn(key);
      const [before, after] = await aroundLimit({ ...baseOpts, ...extra }, PAGE_CALL_TIME_LIMIT_MS);
      expect(before).toBe(PENDING);
      expect(after).toBeInstanceOf(Err);
      expect(browserClose).toHaveBeenCalledTimes(1);
    },
  );

  it.each([
    ["context.close", () => contextClose.mockImplementation(NEVER)],
    ["browser.close", () => browserClose.mockImplementation(NEVER)],
  ] as const)("finishes a failed launch whose %s stalls at its limit", async (_what, stall) => {
    stall();
    mocks.pageEvaluate.mockResolvedValue(1);
    const [before, after] = await aroundLimit(
      { ...baseOpts, simulcastMaxLayers: 3 },
      CLOSE_STEP_BUDGET_MS,
    );
    expect(before).toBe(PENDING);
    expect(after).toBeInstanceOf(SimulcastMaxLayersNotAppliedError);
    expect(browserClose).toHaveBeenCalledTimes(1);
  });
});

describe("launchBot cleanup on every startup failure", () => {
  let browserClose: ReturnType<typeof vi.fn>;
  let browserOn: ReturnType<typeof vi.fn>;
  let goto: ReturnType<typeof vi.fn>;
  let newContext: ReturnType<typeof vi.fn>;
  let newPage: ReturnType<typeof vi.fn>;
  let errSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    vi.clearAllMocks();
    errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    browserClose = vi.fn().mockResolvedValue(undefined);
    browserOn = vi.fn();
    goto = vi.fn().mockResolvedValue(undefined);
    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      goto,
      locator: vi.fn(() => ({ isVisible: async () => false, click: vi.fn() })),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/CleanupTest"),
      waitForURL: vi.fn().mockResolvedValue(undefined),
    };
    newPage = vi.fn().mockResolvedValue(page);
    newContext = vi.fn().mockResolvedValue({
      close: vi.fn().mockResolvedValue(undefined),
      newPage,
    });
    mocks.launch.mockResolvedValue({ close: browserClose, newContext, on: browserOn });
    mocks.addInitScript.mockResolvedValue(undefined);
    mocks.applyJwtCookieAuth.mockResolvedValue(undefined);
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
    mocks.pageEvaluate.mockReset();
    mocks.pageEvaluate.mockResolvedValue(undefined);
  });
  afterEach(() => {
    errSpy.mockRestore();
    mocks.pageEvaluate.mockReset();
  });

  const baseOpts = {
    meetingURL: "https://example.test/meeting/CleanupTest",
    participant: "cleanup-bot",
    displayName: "Cleanup Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
  };
  const boom = new Error("boom");

  it.each([
    [
      "a navigation timeout",
      {},
      () => goto.mockRejectedValue(new Error("page.goto: Timeout 30000ms exceeded")),
    ],
    [
      "a missing storage state",
      {
        authBackend: "storage-state" as const,
        storageStateFile: "/nonexistent/bots-app-state.json",
      },
      () => {},
    ],
    ["a generic join failure", {}, () => mocks.joinMeetingAndEnableMedia.mockRejectedValue(boom)],
    ["a newContext rejection", {}, () => newContext.mockRejectedValue(boom)],
    [
      "a session cookie rejection",
      { authBackend: "jwt" as const },
      () => mocks.applyJwtCookieAuth.mockRejectedValue(boom),
    ],
    ["a newPage rejection", {}, () => newPage.mockRejectedValue(boom)],
    ["an init script rejection", {}, () => mocks.addInitScript.mockRejectedValue(boom)],
  ] as const)("closes the browser, once and as intended, on %s", async (_what, extra, fail) => {
    fail();
    await expect(launchBot({ ...baseOpts, ...extra })).rejects.toBeInstanceOf(Error);
    expect(browserClose).toHaveBeenCalledTimes(1);
    const disconnected = browserOn.mock.calls.find(
      (c) => c[0] === "disconnected",
    )?.[1] as () => void;
    disconnected();
    expect((errSpy.mock.calls as unknown[][]).map((c) => String(c[0]))).not.toContainEqual(
      expect.stringContaining("disconnected unexpectedly"),
    );
  });

  it("rejects as aborted when the abort lands while a failed launch closes its browser", async () => {
    vi.useFakeTimers();
    try {
      const abort = new AbortController();
      goto.mockRejectedValue(new Error("page.goto: Timeout 30000ms exceeded"));
      browserClose.mockImplementation(async () => {
        setTimeout(() => abort.abort(), 1_000);
        await new Promise((r) => setTimeout(r, 2_000));
      });
      const outcome = launchBot({ ...baseOpts, signal: abort.signal }).catch((e: unknown) => e);
      await vi.advanceTimersByTimeAsync(20_000);
      expect(await outcome).toBeInstanceOf(LaunchAbortedError);
      expect(browserClose).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not reject until the browser close it started has settled", async () => {
    let finishClose!: () => void;
    browserClose.mockImplementation(() => new Promise<void>((r) => (finishClose = r)));
    goto.mockRejectedValue(boom);
    let settled = false;
    const launching = launchBot(baseOpts).catch(() => (settled = true));
    await vi.waitFor(() => expect(browserClose).toHaveBeenCalledTimes(1));
    await new Promise((r) => setTimeout(r, 20));
    expect(settled).toBe(false);
    finishClose();
    await launching;
    expect(settled).toBe(true);
  });
});

describe("launchBot receive posture (#2235)", () => {
  // Why the posture matters at all: see `BOT_RECEIVE_POSTURE`.
  let newContext: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    vi.clearAllMocks();
    mocks.browserClose.mockResolvedValue(undefined);
    mocks.contextClose.mockResolvedValue(undefined);
    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/Posture"),
    };
    newContext = vi.fn().mockResolvedValue({
      close: mocks.contextClose,
      newPage: vi.fn().mockResolvedValue(page),
    });
    mocks.launch.mockResolvedValue({ close: mocks.browserClose, newContext, on: vi.fn() });
    mocks.addInitScript.mockResolvedValue(undefined);
    mocks.ensureAssetsPrimed.mockResolvedValue(undefined);
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
    mocks.resolveAssetsForParticipant.mockReturnValue({
      audioPath: "/tmp/audio.wav",
      videoPath: "/tmp/video.y4m",
    });
  });

  async function launch(): Promise<void> {
    await launchBot({
      meetingURL: "https://example.test/meeting/Posture",
      participant: "posture-bot",
      displayName: "Posture Bot",
      headless: true,
      videoMode: "clock",
      authBackend: "none",
      sourceGeometry: SD_SOURCE,
      manifest: { participants: [{ name: "posture-bot" }], lines: [], pauseMs: 0 },
      manifestDir: "/tmp/manifest",
      runDir: "/tmp/run",
      costumeOverride: "clock.y4m",
      audioOverride: "clock.wav",
    });
  }

  /** Device pixels are what the lid measures; CSS px alone cannot express it. */
  function devicePx(opts: {
    viewport?: { width: number; height: number } | null;
    deviceScaleFactor?: number;
  }): { width: number; height: number } {
    // A null viewport is a defect either way, so refuse to score it: Playwright
    // rejects it outright once deviceScaleFactor is set (validateBrowserContextOptions),
    // and without one a headless context is a fixed 800x600 — smaller still.
    expect(opts.viewport).toBeTruthy();
    const dpr = opts.deviceScaleFactor ?? 1;
    return { width: opts.viewport!.width * dpr, height: opts.viewport!.height * dpr };
  }

  // Asserted on the newContext CALL, not on BOT_RECEIVE_POSTURE: spreading the
  // const into the options is the step that can regress, and a test that reads
  // the const would stay green if the spread were dropped.
  it("renders a posture bracketing the 1080p desktop it models", async () => {
    await launch();
    expect(newContext).toHaveBeenCalledTimes(1);
    const opts = newContext.mock.calls[0]![0] as Parameters<typeof devicePx>[0];
    // devicePx FIRST so its null-viewport guard is the assertion that fires; the
    // non-null asserts below would otherwise TypeError ahead of it.
    const px = devicePx(opts);
    // BOTH axes, because a device-pixel product alone cannot express the lid: tile
    // count and column packing are decided in CSS px and only then scaled by dpr,
    // so 960x540@dpr2 matches this device-pixel floor while rendering fewer tiles,
    // or lidding harder, depending on the viewer's density mode.
    expect(opts.viewport!.width).toBeGreaterThanOrEqual(1920);
    expect(opts.viewport!.height).toBeGreaterThanOrEqual(1080);
    expect(px.width).toBeGreaterThanOrEqual(1920);
    expect(px.height).toBeGreaterThanOrEqual(1080);
    // Bounded above as well: on a load-test fleet an oversized posture inflates
    // per-bot decode and raster, which skews the measurement this tool exists to
    // take. Growing the posture should be a reviewed decision, not a silent one.
    expect(px.width).toBeLessThanOrEqual(2560);
    expect(px.height).toBeLessThanOrEqual(1440);
  });

  it("keeps the posture the launch path actually applies in sync with the export", async () => {
    await launch();
    const opts = newContext.mock.calls[0]![0] as Record<string, unknown>;
    // Premise first: the loop below asserts nothing at all over an emptied const,
    // and comparing each side to that same const cannot detect it either.
    expect(Object.keys(BOT_RECEIVE_POSTURE)).toEqual(["viewport", "deviceScaleFactor"]);
    for (const [k, v] of Object.entries(BOT_RECEIVE_POSTURE)) {
      expect(opts[k]).toEqual(v);
    }
  });

  it("still passes the auth and TLS options the fleet depends on", async () => {
    // The spread is added to an options object that already carries these; a
    // careless edit that replaced rather than extended it would break auth.
    await launch();
    const opts = newContext.mock.calls[0]![0] as Record<string, unknown>;
    expect(opts.ignoreHTTPSErrors).toBe(true);
    expect(opts).toHaveProperty("storageState");
  });
});

describe("launchBot log-line forgery (#2375)", () => {
  /** A complete, correctly-prefixed bot line an operator value must not be able to start. */
  const FORGED = "[bot-0] FORGED-BY-BOT";

  beforeEach(() => {
    vi.clearAllMocks();
    const page = {
      addInitScript: mocks.addInitScript.mockResolvedValue(undefined),
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/ClockTest"),
    };
    mocks.launch.mockResolvedValue({
      close: vi.fn().mockResolvedValue(undefined),
      newContext: vi.fn().mockResolvedValue({
        close: vi.fn().mockResolvedValue(undefined),
        newPage: vi.fn().mockResolvedValue(page),
      }),
      on: vi.fn(),
    });
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
  });

  /** Every line the run wrote to stdout/stderr, split on either terminator. */
  async function linesFrom(overrides: Record<string, unknown>): Promise<string[]> {
    const written: string[] = [];
    const sink = (...args: unknown[]): void => {
      written.push(args.map((a) => (typeof a === "string" ? a : String(a))).join(" "));
    };
    const spies = (["log", "warn", "error", "debug"] as const).map((m) =>
      vi.spyOn(console, m).mockImplementation(sink),
    );
    try {
      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "bot-0",
        displayName: "Bot 0",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
        ...overrides,
      } as Parameters<typeof launchBot>[0]);
      await handle.shutdown();
    } finally {
      for (const s of spies) s.mockRestore();
    }
    return written.join("\n").split(/[\r\n]/);
  }

  const CASES: Array<[string, Record<string, unknown>]> = [
    ["participant (LF)", { participant: `bot-0\n${FORGED}` }],
    ["participant (CR)", { participant: `bot-0\r${FORGED}` }],
    ["botIdShort (LF)", { participant: "bot-0", botIdShort: `abc123\n${FORGED}` }],
    ["network (LF)", { network: `congested_wifi\n${FORGED}` }],
    ["ssoStateFile (CR)", { authBackend: "jwt", ssoStateFile: `/nope/sso-state.json\r${FORGED}` }],
  ];

  it.each(CASES)("keeps %s from starting a second bot-prefixed line", async (_label, overrides) => {
    const lines = await linesFrom(overrides);
    expect(lines.filter((l) => l.startsWith(FORGED))).toEqual([]);
    // Truncated instead ⇒ the sanitiser dropped the tail rather than collapsing it.
    expect(lines.filter((l) => l.includes(FORGED)).length).toBeGreaterThan(0);
  });
});

describe("launchBot camera cycle wiring (#2362)", () => {
  const BASE = {
    meetingURL: "https://example.test/meeting/ClockTest",
    participant: "clock-bot",
    displayName: "Clock Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
  };

  beforeEach(() => {
    vi.clearAllMocks();
    mocks.browserClose.mockResolvedValue(undefined);
    mocks.contextClose.mockResolvedValue(undefined);
    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => BASE.meetingURL),
    };
    const context = { close: mocks.contextClose, newPage: vi.fn().mockResolvedValue(page) };
    mocks.launch.mockResolvedValue({
      close: mocks.browserClose,
      newContext: vi.fn().mockResolvedValue(context),
      on: vi.fn(),
    });
    mocks.addInitScript.mockResolvedValue(undefined);
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
  });

  const runAndCollect = async (
    cameraCycle: { onMinMs: number; onMaxMs: number; offMinMs: number; offMaxMs: number } | null,
  ): Promise<string[]> => {
    const lines: string[] = [];
    const log = vi.spyOn(console, "log").mockImplementation((...a) => void lines.push(a.join(" ")));
    const err = vi
      .spyOn(console, "error")
      .mockImplementation((...a) => void lines.push(a.join(" ")));
    try {
      const bot = await launchBot({ ...BASE, cameraCycle });
      await bot.shutdown();
    } finally {
      log.mockRestore();
      err.mockRestore();
    }
    return lines;
  };

  it("stays silent and starts no loop when cameraCycle is null — the default", async () => {
    const lines = await runAndCollect(null);
    expect(lines.filter((l) => l.includes("camera cycle"))).toEqual([]);
    expect(lines.filter((l) => l.includes("CAMERA_CYCLE_"))).toEqual([]);
  });

  it("announces the configured cycle and emits the receipt from shutdown()", async () => {
    const lines = await runAndCollect({
      onMinMs: 5_000,
      onMaxMs: 15_000,
      offMinMs: 20_000,
      offMaxMs: 60_000,
    });
    expect(lines.some((l) => l.includes("camera cycle configured — on=[5-15]s off=[20-60]s"))).toBe(
      true,
    );
    // No timer advanced, so no toggle fired — and the receipt must say so
    // rather than being absent.
    expect(lines.some((l) => l.includes(CAMERA_CYCLE_NEVER_FIRED_BANNER))).toBe(true);
  });
});

describe("launchBot leaveMeeting fallback banner (#2486)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    const button = (selector: string): unknown => ({
      isVisible: async () => selector !== HANG_UP_SELECTOR,
      click: async () => {},
    });
    const page = {
      addInitScript: mocks.addInitScript.mockResolvedValue(undefined),
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(button),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/ClockTest"),
      waitForURL: vi.fn().mockResolvedValue(undefined),
    };
    mocks.launch.mockResolvedValue({
      close: vi.fn().mockResolvedValue(undefined),
      newContext: vi.fn().mockResolvedValue({
        close: vi.fn().mockResolvedValue(undefined),
        newPage: vi.fn().mockResolvedValue(page),
      }),
      on: vi.fn(),
    });
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
  });

  it("tags the tooltip-fallback banner with the bot label", async () => {
    const bot = await launchBot({
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock",
      authBackend: "none",
      sourceGeometry: SD_SOURCE,
    });
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    try {
      await bot.leaveMeeting();
      const banners = warn.mock.calls
        .map((c) => String(c[0]))
        .filter((l) => l.includes(PRE_MARKER_UI_BANNER));
      expect(banners).toEqual([
        expect.stringMatching(new RegExp(`^\\[clock-bot\\] ${PRE_MARKER_UI_BANNER} leaveMeeting:`)),
      ]);
    } finally {
      warn.mockRestore();
      await bot.shutdown();
    }
  });
});

describe("launchBot persona driver and hang-up (issue 3091)", () => {
  const events: string[] = [];
  const media = { mic: true, camera: true };
  const shows: Record<string, [keyof typeof media, boolean]> = {
    [micControlSelector("on")]: ["mic", true],
    [MIC_UNMUTE_SELECTOR]: ["mic", false],
    [cameraControlSelector("on")]: ["camera", true],
    [cameraControlSelector("off")]: ["camera", false],
  };
  const settings = resolvePersonaSettings(true, {});
  if (settings.kind !== "ok" || settings.value === null) throw new Error("persona defaults");
  const room = assignPersonas(createPersonaPlanner(SPEECH_PLAN_DEFAULT, 3), 3, settings.value);
  const base = {
    meetingURL: "https://example.test/meeting/ClockTest",
    participant: "clock-bot",
    displayName: "Clock Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
    talkingClock: { index: 2, fleetSize: 3 },
  };
  const openMicAlwaysOn = personaFor(room, { index: 2, fleetSize: 3 });
  beforeEach(() => {
    vi.clearAllMocks();
    events.length = 0;
    Object.assign(media, { mic: true, camera: true });
    const locator = (selector: string): unknown => {
      const self = {
        first: () => self,
        hover: async () => {},
        isVisible: async () =>
          selector in shows ? media[shows[selector][0]] === shows[selector][1] : true,
        waitFor: async () => {},
        click: async () => {
          if (selector === HANG_UP_SELECTOR) events.push("hang-up");
          if (!(selector in shows)) return;
          const [kind, on] = shows[selector];
          media[kind] = !on;
          events.push(`${kind} ${media[kind] ? "on" : "off"}`);
        },
      };
      return self;
    };
    const page = {
      addInitScript: mocks.addInitScript.mockResolvedValue(undefined),
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(locator),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/ClockTest"),
      waitForURL: vi.fn().mockResolvedValue(undefined),
    };
    mocks.launch.mockResolvedValue({
      close: vi.fn().mockResolvedValue(undefined),
      newContext: vi.fn().mockResolvedValue({
        close: vi.fn().mockResolvedValue(undefined),
        newPage: vi.fn().mockResolvedValue(page),
      }),
      on: vi.fn(),
    });
    mocks.joinMeetingAndEnableMedia.mockResolvedValue({ camera: true, mic: true });
  });

  it("injects the room's muted listeners for a persona bot and none otherwise", async () => {
    const scriptOf = (): string => String(mocks.addInitScript.mock.calls.at(-1)?.[0] ?? "");
    const persona = await launchBot({ ...base, persona: openMicAlwaysOn });
    expect(scriptOf()).toContain("globalThis.__CLOCK_MUTED_LISTENERS = [0,1];\n");
    await persona.shutdown();
    const plain = await launchBot(base);
    expect(scriptOf()).toContain("globalThis.__CLOCK_MUTED_LISTENERS = [];\n");
    await plain.shutdown();
  });

  it("starts the persona driver from the join state: a muted, camera-off join of an open mic turns both on", async () => {
    mocks.joinMeetingAndEnableMedia.mockResolvedValue({ camera: false, mic: false });
    Object.assign(media, { mic: false, camera: false });
    const bot = await launchBot({ ...base, persona: openMicAlwaysOn });
    for (let i = 0; i < 100 && events.length < 2; i += 1)
      await new Promise((r) => setTimeout(r, 20));
    await bot.shutdown();
    expect(events).toEqual(["mic on", "camera on"]);
  });

  it.each([
    ["open", "always", { mic: true, camera: true }],
    ["open", "never", { mic: true, camera: false }],
    ["observer", "always", { mic: false, camera: true }],
  ] as const)(
    "leaves a %s mic and %s camera that joined in that state alone",
    async (mic, camera, joined) => {
      mocks.joinMeetingAndEnableMedia.mockResolvedValue(joined);
      Object.assign(media, joined);
      let last: PersonaTally | null = null;
      const bot = await launchBot({
        ...base,
        persona: { ...openMicAlwaysOn, mic, camera },
        onPersona: (t) => (last = t),
      });
      await new Promise((r) => setTimeout(r, 700));
      await bot.shutdown();
      expect(events).toEqual([]);
      expect(last).toMatchObject({ ok: 0, already: 0, failed: 0 });
    },
  );

  it("stops the persona driver, once, before it clicks hang-up", async () => {
    const log = vi.spyOn(console, "log").mockImplementation((...a: unknown[]) => {
      if (String(a[0]).includes("PERSONA_")) events.push("receipt");
    });
    try {
      const bot = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        sourceGeometry: SD_SOURCE,
        talkingClock: { index: 2, fleetSize: 3 },
        persona: personaFor(room, { index: 2, fleetSize: 3 }),
      });
      await bot.leaveMeeting();
      await bot.shutdown();
      expect(events).toEqual(["receipt", "hang-up"]);
    } finally {
      log.mockRestore();
    }
  });
});

describe("launchBot page-event writers compose before sanitising (#2484)", () => {
  const FORGED = "[clock-bot] FORGED-BY-PAGE-EVENT";
  const on = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    on.mockReset();
    const page = {
      addInitScript: mocks.addInitScript.mockResolvedValue(undefined),
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(),
      off: vi.fn(),
      on,
      url: vi.fn(() => "https://example.test/meeting/ClockTest"),
    };
    mocks.launch.mockResolvedValue({
      close: vi.fn().mockResolvedValue(undefined),
      newContext: vi.fn().mockResolvedValue({
        close: vi.fn().mockResolvedValue(undefined),
        newPage: vi.fn().mockResolvedValue(page),
      }),
      on: vi.fn(),
    });
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
  });

  async function linesFrom(event: string, payload: unknown): Promise<string[]> {
    const bot = await launchBot({
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock",
      authBackend: "none",
      sourceGeometry: SD_SOURCE,
    });
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      const handler = on.mock.calls.find(([e]) => e === event)?.[1] as (p: unknown) => void;
      handler(payload);
      return err.mock.calls.map((c) => c.map(String).join(" ")).flatMap((l) => l.split(/[\r\n]/));
    } finally {
      err.mockRestore();
      await bot.shutdown();
    }
  }

  function expectCollapsed(lines: string[], site: string): void {
    expect(lines.filter((l) => l.startsWith(FORGED))).toEqual([]);
    expect(lines.filter((l) => l.includes(site) && l.includes(FORGED))).toHaveLength(1);
  }

  it("pageerror", async () => {
    const lines = await linesFrom("pageerror", new Error(`boom\n${FORGED}`));
    expectCollapsed(lines, "[clock-bot] pageerror:");
  });

  it("console.error", async () => {
    const msg = { type: () => "error", text: () => `boom\n${FORGED}` };
    const lines = await linesFrom("console", msg);
    expectCollapsed(lines, "[clock-bot] console.error:");
  });
});

describe("diagnostics packets switch in launchBot (#2970)", () => {
  const on = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    on.mockReset();
    const page = {
      addInitScript: mocks.addInitScript.mockResolvedValue(undefined),
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(),
      off: vi.fn(),
      on,
      url: vi.fn(() => "https://app.example.test/meeting/DiagTest"),
    };
    mocks.launch.mockResolvedValue({
      close: vi.fn().mockResolvedValue(undefined),
      newContext: vi.fn().mockResolvedValue({
        close: vi.fn().mockResolvedValue(undefined),
        newPage: vi.fn().mockResolvedValue(page),
      }),
      on: vi.fn(),
    });
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
  });

  const baseOpts = {
    meetingURL: "https://app.example.test/meeting/DiagTest?x=1",
    participant: "diag-bot",
    displayName: "Diag Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
  };

  const appConfigScripts = (): string[] =>
    mocks.addInitScript.mock.calls
      .map((c) => c[0])
      .filter((s): s is string => typeof s === "string" && s.includes("__APP_CONFIG"));

  function emitConsole(type: string, text: string): void {
    for (const [event, handler] of on.mock.calls) {
      if (event === "console")
        (handler as (m: unknown) => void)({ type: () => type, text: () => text });
    }
  }

  it("adds the key to the ONE receiver-caps script, guarded to the meeting's origin", async () => {
    const bot = await launchBot({ ...baseOpts, maxReceivedLayer: 0, diagPackets: "off" });
    await bot.shutdown();
    const scripts = appConfigScripts();
    expect(scripts).toHaveLength(1);
    expect(scripts[0]).toContain('"diagnosticsPacketsEnabled":"0"');
    expect(scripts[0]).toContain('"maxReceivedLayer":0');
    expect(scripts[0]).toContain('location.origin !== "https://app.example.test"');
  });

  it("injects no __APP_CONFIG script when unset", async () => {
    const bot = await launchBot(baseOpts);
    await bot.shutdown();
    expect(appConfigScripts()).toEqual([]);
  });

  it.each([
    [true, { skipCanvasPaint: "true" }, "true"],
    [true, {}, null],
    [undefined, { skipCanvasPaint: "true" }, null],
  ] as const)(
    "skipCanvasPaint %j with __APP_CONFIG %j after join reads back %j (issue 3086)",
    async (skipCanvasPaint, cfg, expected) => {
      (globalThis as { __APP_CONFIG?: unknown }).__APP_CONFIG = cfg;
      mocks.pageEvaluate.mockImplementation(async (fn: unknown, keys: unknown) =>
        Array.isArray(keys) ? (fn as (k: string[]) => unknown)(keys) : undefined,
      );
      try {
        const bot = await launchBot({ ...baseOpts, skipCanvasPaint });
        await bot.shutdown();
        expect(bot.skipCanvasPaintReadback).toBe(expected);
      } finally {
        delete (globalThis as { __APP_CONFIG?: unknown }).__APP_CONFIG;
        mocks.pageEvaluate.mockReset();
      }
    },
  );

  it("forwards the client's line from an info console message", async () => {
    const seen: DiagPacketsObservation[] = [];
    const bot = await launchBot({ ...baseOpts, onDiagPackets: (o) => seen.push(o) });
    emitConsole(
      "info",
      "INFO src/components/attendants.rs:4418 diagnostics packets: DISABLED (source=config)",
    );
    await bot.shutdown();
    expect(seen).toEqual([{ state: "DISABLED", source: "config" }]);
  });

  it("logs every observation with its state and source, not only ENABLED", async () => {
    const log = vi.spyOn(console, "log").mockImplementation(() => undefined);
    const bot = await launchBot({ ...baseOpts, diagPackets: "off" });
    try {
      log.mockClear();
      emitConsole("info", "diagnostics packets: DISABLED (source=config)");
      expect(log.mock.calls.map((c) => c.map(String).join(" "))).toEqual([
        expect.stringMatching(/^\[diag-bot\] diagnostics packets: DISABLED \(source=config\)$/),
      ]);
    } finally {
      log.mockRestore();
      await bot.shutdown();
    }
  });

  it("logs an error when off was requested and the client still sends", async () => {
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const bot = await launchBot({ ...baseOpts, diagPackets: "off", onDiagPackets: () => {} });
    try {
      emitConsole("log", "diagnostics packets: ENABLED (source=default)");
      expect(err.mock.calls.map((c) => c.map(String).join(" ")).join("\n")).toContain(
        "--diag-packets off requested but the client logged ENABLED (source=default)",
      );
    } finally {
      err.mockRestore();
      await bot.shutdown();
    }
  });

  it("logs an error when on was requested and the client sends none", async () => {
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const bot = await launchBot({ ...baseOpts, diagPackets: "on" });
    try {
      emitConsole("info", "diagnostics packets: ENABLED (source=config)");
      expect(err.mock.calls.flat().map(String).join("\n")).not.toContain("--diag-packets");
      emitConsole("info", "diagnostics packets: DISABLED (source=url)");
      expect(err.mock.calls.map((c) => c.map(String).join(" ")).join("\n")).toContain(
        "--diag-packets on requested but the client logged DISABLED (source=url)",
      );
      expect(appConfigScripts()[0]).toContain('"diagnosticsPacketsEnabled":"1"');
    } finally {
      err.mockRestore();
      await bot.shutdown();
    }
  });

  it("prints no off-requested error for a default bot that logs ENABLED", async () => {
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const bot = await launchBot(baseOpts);
    try {
      emitConsole("info", "diagnostics packets: ENABLED (source=default)");
      expect(err.mock.calls.flat().map(String).join("\n")).not.toContain("--diag-packets off");
    } finally {
      err.mockRestore();
      await bot.shutdown();
    }
  });

  it("rejects near-miss lines", () => {
    expect(parseDiagPacketsLine("diagnostics packets: ENABLED (source=url)")).toEqual({
      state: "ENABLED",
      source: "url",
    });
    for (const text of [
      "diagnostics packets: disabled (source=config)",
      "diagnostics packets: DISABLED (source=env)",
      "diagnostics packets: DISABLED",
      "diagnostic packets: DISABLED (source=config)",
    ]) {
      expect(parseDiagPacketsLine(text)).toBeNull();
    }
  });
});

describe("transport pin in launchBot (issue 3087)", () => {
  const on = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    on.mockReset();
    const page = {
      addInitScript: mocks.addInitScript.mockResolvedValue(undefined),
      evaluate: mocks.pageEvaluate,
      mainFrame: () => ({ evaluate: mocks.documentIdentity }),
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(),
      off: vi.fn(),
      on,
      url: vi.fn(() => "https://app.example.test/meeting/TransportTest"),
    };
    mocks.launch.mockResolvedValue({
      close: vi.fn().mockResolvedValue(undefined),
      newContext: vi.fn().mockResolvedValue({
        close: vi.fn().mockResolvedValue(undefined),
        newPage: vi.fn().mockResolvedValue(page),
      }),
      on: vi.fn(),
    });
    mocks.joinMeetingAndEnableMedia.mockResolvedValue(undefined);
  });

  afterEach(() => vi.useRealTimers());

  const baseOpts = {
    meetingURL: "https://app.example.test/meeting/TransportTest",
    participant: "ws-bot",
    displayName: "WS Bot",
    headless: true,
    videoMode: "clock" as const,
    authBackend: "none" as const,
    sourceGeometry: SD_SOURCE,
  };

  const scripts = (): string[] =>
    mocks.addInitScript.mock.calls.map((c) => c[0]).filter((s) => typeof s === "string");

  function emitConsole(text: string): void {
    for (const [event, handler] of on.mock.calls) {
      if (event === "console")
        (handler as (m: unknown) => void)({ type: () => "info", text: () => text });
    }
  }

  const joinLogging = (line: string): void => {
    mocks.joinMeetingAndEnableMedia.mockImplementation(async () => {
      emitConsole(line);
      return undefined;
    });
  };

  it("injects defaultTransport and clears remembered choices when pinned", async () => {
    joinLogging("Transport preference applied: pref=websocket source=default wt_urls=0 ws_urls=1");
    const bot = await launchBot({ ...baseOpts, transport: "websocket" });
    await bot.shutdown();
    expect(scripts()).toContain(buildTransportInitScript("https://app.example.test"));
    expect(scripts().find((s) => s.includes("__APP_CONFIG"))).toContain(
      '"defaultTransport":"websocket"',
    );
  });

  it("injects nothing for auto", async () => {
    const bot = await launchBot({ ...baseOpts, transport: "auto" });
    await bot.shutdown();
    expect(scripts().some((s) => s.includes("vc_transport") || s.includes("__APP_CONFIG"))).toBe(
      false,
    );
  });

  it("fails a pinned bot whose client applied a remembered choice", async () => {
    joinLogging("Transport preference applied: pref=websocket source=sticky wt_urls=0 ws_urls=1");
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      await expect(launchBot({ ...baseOpts, transport: "websocket" })).rejects.toThrow(
        TransportNotAppliedError,
      );
    } finally {
      err.mockRestore();
    }
  });

  it("fails a pinned bot whose client applied a different default", async () => {
    joinLogging(
      "Transport preference applied: pref=webtransport source=default wt_urls=1 ws_urls=1",
    );
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      await expect(launchBot({ ...baseOpts, transport: "websocket" })).rejects.toThrow(
        TransportNotAppliedError,
      );
    } finally {
      err.mockRestore();
    }
  });

  it("waits for an applied line that arrives after join and fails on a mismatch", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      const outcome = launchBot({ ...baseOpts, transport: "websocket" }).then(
        () => null,
        (e: unknown) => e,
      );
      await vi.advanceTimersByTimeAsync(5_000);
      emitConsole(
        "Transport preference applied: pref=webtransport source=default wt_urls=1 ws_urls=1",
      );
      expect(await outcome).toBeInstanceOf(TransportNotAppliedError);
    } finally {
      err.mockRestore();
    }
  });

  it("ends the wait for an applied line as soon as the page reloads", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      mocks.documentIdentity.mockResolvedValue(1);
      const outcome = launchBot({ ...baseOpts, transport: "websocket" }).then(
        () => "returned",
        (e: unknown) => e,
      );
      await vi.advanceTimersByTimeAsync(100);
      mocks.documentIdentity.mockResolvedValue(2);
      const frame = { parentFrame: () => null, url: () => baseOpts.meetingURL };
      for (const [event, handler] of on.mock.calls) {
        if (event === "framenavigated") (handler as (f: unknown) => void)(frame);
      }
      await vi.advanceTimersByTimeAsync(100);
      const result = await Promise.race([outcome, Promise.resolve("still waiting")]);
      expect(result).toBeInstanceOf(PageReloadedError);
    } finally {
      mocks.documentIdentity.mockResolvedValue(1);
      err.mockRestore();
    }
  });

  it("forwards election lines and warns, without failing, when no applied line arrives", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const err = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const seen: unknown[] = [];
    try {
      const pending = launchBot({
        ...baseOpts,
        transport: "webtransport",
        onTransport: (e) => seen.push(e),
      });
      await vi.advanceTimersByTimeAsync(TRANSPORT_APPLIED_WAIT_MS);
      const bot = await pending;
      emitConsole(
        "Election decision: reason=x outcome=elected elected=ws_0 active=ws_0 wt_samples=0",
      );
      await bot.shutdown();
      expect(err.mock.calls.flat().map(String).join("\n")).toContain(
        "no 'Transport preference applied:' line",
      );
      expect(seen).toEqual([{ kind: "elected", active: "websocket" }]);
    } finally {
      err.mockRestore();
    }
  });
});
