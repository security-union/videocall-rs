import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  addInitScript: vi.fn(),
  applyJwtCookieAuth: vi.fn(),
  ensureAssetsPrimed: vi.fn(),
  joinMeetingAndEnableMedia: vi.fn(),
  launch: vi.fn(),
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

import { ENCODER_FPS_POLL_MS, launchBot } from "./bot";

describe("launchBot clock mode", () => {
  beforeEach(() => {
    vi.clearAllMocks();

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
      close: vi.fn().mockResolvedValue(undefined),
      newPage: vi.fn().mockResolvedValue(page),
    };
    const browser = {
      close: vi.fn().mockResolvedValue(undefined),
      newContext: vi.fn().mockResolvedValue(context),
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
  let browser: { close: ReturnType<typeof vi.fn>; newContext: ReturnType<typeof vi.fn> };

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
