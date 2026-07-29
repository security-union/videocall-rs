import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  addInitScript: vi.fn(),
  applyJwtCookieAuth: vi.fn(),
  ensureAssetsPrimed: vi.fn(),
  joinMeetingAndEnableMedia: vi.fn(),
  launch: vi.fn(),
  resolveAssetsForParticipant: vi.fn(),
}));

vi.mock("@playwright/test", () => ({
  chromium: {
    launch: mocks.launch,
  },
}));

vi.mock("./auth/jwt-cookie", () => ({
  applyJwtCookieAuth: mocks.applyJwtCookieAuth,
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

import { launchBot } from "./bot";

describe("launchBot clock mode", () => {
  beforeEach(() => {
    vi.clearAllMocks();

    const page = {
      addInitScript: mocks.addInitScript,
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
});
