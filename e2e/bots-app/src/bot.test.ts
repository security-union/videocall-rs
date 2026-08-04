import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  addInitScript: vi.fn(),
  applyJwtCookieAuth: vi.fn(),
  ensureAssetsPrimed: vi.fn(),
  joinMeetingAndEnableMedia: vi.fn(),
  launch: vi.fn(),
  browserClose: vi.fn(),
  contextClose: vi.fn(),
  pageEvaluate: vi.fn(),
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
  browserEnvWithoutFleetCreds,
  ENCODER_FPS_POLL_MS,
  isBenignTeardownError,
  launchBot,
} from "./bot";
import { openSsoCaptureBrowser } from "./auth/sso-capture";

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
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.browserClose.mockResolvedValue(undefined);
    mocks.contextClose.mockResolvedValue(undefined);

    const page = {
      addInitScript: mocks.addInitScript,
      evaluate: mocks.pageEvaluate,
      goto: vi.fn().mockResolvedValue(undefined),
      locator: vi.fn(),
      off: vi.fn(),
      on: vi.fn(),
      url: vi.fn(() => "https://example.test/meeting/ClockTest"),
    };
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

  it("registers the participant and clock source without priming or fake-file capture", async () => {
    await launchBot({
      meetingURL: "https://example.test/meeting/ClockTest",
      participant: "clock-bot",
      displayName: "Clock Bot",
      headless: true,
      videoMode: "clock",
      authBackend: "none",
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

    expect(mocks.addInitScript).toHaveBeenCalledTimes(2);
    expect(mocks.addInitScript).toHaveBeenNthCalledWith(
      1,
      'globalThis.__CLOCK_PARTICIPANT = "Clock Bot";',
    );
    expect(mocks.addInitScript).toHaveBeenNthCalledWith(
      2,
      expect.objectContaining({
        path: expect.stringMatching(/clock-source\.js$/),
      }),
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
      mocks.pageEvaluate
        .mockResolvedValueOnce(12) // positive -> forwarded
        .mockResolvedValueOnce(undefined) // absent -> null (no data)
        .mockResolvedValueOnce(0) // zero -> null (not a starvation reading)
        .mockResolvedValue(4); // positive -> forwarded
      const onEncoderFps = vi.fn();

      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
        manifest: { participants: [{ name: "clock-bot" }], lines: [], pauseMs: 0 },
        manifestDir: "/tmp/manifest",
        runDir: "/tmp/run",
        costumeOverride: "clock.y4m",
        audioOverride: "clock.wav",
        onEncoderFps,
      });

      // launchBot returns with the first poll scheduled but not yet fired.
      expect(mocks.pageEvaluate).not.toHaveBeenCalled();

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
      const evaluateCallsAtShutdown = mocks.pageEvaluate.mock.calls.length;
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS * 3);
      expect(mocks.pageEvaluate.mock.calls.length).toBe(evaluateCallsAtShutdown);
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
      mocks.pageEvaluate
        .mockImplementationOnce(
          () =>
            new Promise((resolve) => {
              resolveFirst = resolve;
            }),
        )
        .mockResolvedValue(8);
      const onEncoderFps = vi.fn();

      const handle = await launchBot({
        meetingURL: "https://example.test/meeting/ClockTest",
        participant: "clock-bot",
        displayName: "Clock Bot",
        headless: true,
        videoMode: "clock",
        authBackend: "none",
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
      expect(mocks.pageEvaluate).toHaveBeenCalledTimes(1);
      expect(onEncoderFps).not.toHaveBeenCalled();

      // (2) fpsPollStopped: shut down WHILE the evaluate is in flight, THEN let
      // it settle. The late `.then` must not re-arm the chain. Without the flag
      // (clearTimeout alone), the late reschedule would arm a new timer and a
      // second evaluate would fire on advance — so this pins the flag, not just
      // the clearTimeout.
      await handle.shutdown();
      resolveFirst?.(8);
      await vi.advanceTimersByTimeAsync(ENCODER_FPS_POLL_MS * 3);
      expect(mocks.pageEvaluate).toHaveBeenCalledTimes(1);
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

    // Clock mode still adds its own two init scripts; assert specifically that
    // none of the registered scripts touch navigator.hardwareConcurrency.
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

describe("browserEnvWithoutFleetCreds (#2035)", () => {
  // The k8s fleet injects the WHOLE bot-accounts Secret into every pod, so the
  // orchestrator's process.env carries every ordinal's creds. The browser
  // subprocess must launch WITHOUT them (defense-in-depth). Reverting the filter
  // (returning the source unchanged) makes these fail.
  it("strips BOT_EMAIL/BOT_PASSWORD, their ordinal-suffixed variants, and BOT_CTL_TOKEN", () => {
    const filtered = browserEnvWithoutFleetCreds({
      PATH: "/usr/bin",
      DISPLAY: ":0",
      BOT_EMAIL: "single@example.test",
      BOT_PASSWORD: "single-pw",
      BOT_EMAIL_0: "alice@example.test",
      BOT_PASSWORD_0: "pw0",
      BOT_EMAIL_19: "tina@example.test",
      BOT_PASSWORD_19: "pw19",
      // #2072: the fleet-wide control-API bearer token is a higher-value secret
      // than any single account (drives /netem + /leave on every pod) — the
      // browser subprocess must not carry it either.
      BOT_CTL_TOKEN: "fleet-ctl-secret",
    });
    expect(filtered).toEqual({ PATH: "/usr/bin", DISPLAY: ":0" });
  });

  it("keeps non-credential vars, including unrelated BOT_* config", () => {
    const filtered = browserEnvWithoutFleetCreds({
      PATH: "/usr/bin",
      BOT_HW_CONCURRENCY: "6", // NOT a credential — must survive
      BOT_AUTH: "form-login", // NOT a credential — must survive
      BOT_CTL_PORT: "8080", // ctl CONFIG, not the token — must survive
      BOT_CTL_BIND: "0.0.0.0", // ctl CONFIG, not the token — must survive
      BOT_EMAIL_2: "carol@example.test", // credential — must be stripped
    });
    expect(filtered).toEqual({
      PATH: "/usr/bin",
      BOT_HW_CONCURRENCY: "6",
      BOT_AUTH: "form-login",
      BOT_CTL_PORT: "8080",
      BOT_CTL_BIND: "0.0.0.0",
    });
  });

  it("drops undefined values", () => {
    const filtered = browserEnvWithoutFleetCreds({ PATH: "/usr/bin", MAYBE: undefined });
    expect(filtered).toEqual({ PATH: "/usr/bin" });
  });
});
