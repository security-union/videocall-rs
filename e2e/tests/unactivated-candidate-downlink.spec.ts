import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { joinAndStartCamera } from "../helpers/camera-publisher";
import { samplePeerVideoChecksum } from "../helpers/frame-liveness";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { installMediaSocketRecorder, severMediaWebSocket } from "../helpers/media-socket-sever";
import { createMeeting } from "../helpers/meeting-api";
import { waitForServices } from "../helpers/wait-for-services";

const HOST_EMAIL = "downlink-3058-host@videocall.rs";
const HOST_NAME = "Downlink3058Host";
const GUEST_EMAIL = "downlink-3058-guest@videocall.rs";
const GUEST_NAME = "Downlink3058Guest";

const CONNECTED_LOG = "Connection state changed: Connected";

const FIRST_LIVE_FRAME_BUDGET_MS = 3_500;
const RECONNECT_LIVE_FRAME_BUDGET_MS = 3_500;

interface CallResources {
  browsers: Browser[];
  contexts: BrowserContext[];
}

interface LateJoinCall {
  guestPage: Page;
  guestConsole: string[];
  firstLiveFrameMs: number;
}

async function waitForLiveHostVideo(page: Page, budgetMs: number, what: string): Promise<number> {
  const start = Date.now();
  const seen = new Set<string>();
  await expect
    .poll(
      async () => {
        const sample = await samplePeerVideoChecksum(page);
        if (sample !== null) seen.add(sample);
        return seen.size >= 3;
      },
      {
        timeout: budgetMs,
        intervals: [100],
        message: `the guest painted no changing host video frame within ${budgetMs}ms ${what}`,
      },
    )
    .toBe(true);
  return Date.now() - start;
}

async function lateJoinWithCameraAndMicOff(
  uiURL: string,
  meetingId: string,
  opts: { pinWs: boolean; recordSockets: boolean },
  res: CallResources,
): Promise<LateJoinCall> {
  await createMeeting(HOST_EMAIL, HOST_NAME, { meetingId, waitingRoomEnabled: false });
  const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
  res.browsers.push(hostBrowser);
  const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
  res.browsers.push(guestBrowser);
  const host = await createAuthenticatedContext(hostBrowser, HOST_EMAIL, HOST_NAME, uiURL);
  res.contexts.push(host);
  const guest = await createAuthenticatedContext(guestBrowser, GUEST_EMAIL, GUEST_NAME, uiURL);
  res.contexts.push(guest);
  if (opts.pinWs) {
    await pinWebSocketTransport(host);
    await pinWebSocketTransport(guest);
  }
  await guest.addInitScript(() => {
    window.localStorage.setItem("vc_prejoin_camera_on", "false");
    window.localStorage.setItem("vc_prejoin_mic_on", "false");
  });
  if (opts.recordSockets) {
    await installMediaSocketRecorder(guest);
  }

  const hostPage = await host.newPage();
  await joinAndStartCamera(hostPage, meetingId);

  const guestPage = await guest.newPage();
  const guestConsole: string[] = [];
  guestPage.on("console", (msg) => guestConsole.push(msg.text()));
  await fillAndSubmitJoinForm(guestPage, meetingId, GUEST_NAME);
  const joinButton = guestPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  await joinButton.waitFor({ timeout: 30_000 });
  await joinButton.click();
  const clickedAt = Date.now();
  await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
  const gridAfterMs = Date.now() - clickedAt;

  const liveAfterGridMs = await waitForLiveHostVideo(
    guestPage,
    FIRST_LIVE_FRAME_BUDGET_MS,
    "after the late joiner's grid appeared",
  );
  console.log(
    `[3058] pinWs=${opts.pinWs} grid ${gridAfterMs}ms after Join, first live host frame ` +
      `${liveAfterGridMs}ms after grid (${gridAfterMs + liveAfterGridMs}ms after Join)`,
  );

  await expect(
    guestPage.locator('[data-testid="mic-toggle-button"]'),
    "the guest must publish no audio",
  ).toHaveAttribute("aria-label", "Microphone — Unmute");
  await expect(
    guestPage.locator("button.video-control-button", {
      has: guestPage.locator("span.tooltip", { hasText: "Start Video" }),
    }),
    "the guest must publish no video",
  ).toHaveCount(1);

  return { guestPage, guestConsole, firstLiveFrameMs: liveAfterGridMs };
}

async function closeAll(res: CallResources): Promise<void> {
  for (const c of res.contexts) await c.close().catch(() => undefined);
  await Promise.all(res.browsers.map((b) => b.close().catch(() => undefined)));
}

test.describe("#3058 a non-publishing late joiner receives media once elected", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  for (const pinWs of [false, true]) {
    const transport = pinWs ? "WebSocket pinned" : "default transport";
    test(`camera and mic off late joiner paints live host video promptly (${transport}) @bvt1`, async ({
      baseURL,
    }) => {
      test.setTimeout(180_000);
      const res: CallResources = { browsers: [], contexts: [] };
      try {
        await lateJoinWithCameraAndMicOff(
          baseURL || "http://localhost:3001",
          `e2e_3058_late_${pinWs ? "ws" : "default"}_${Date.now()}`,
          { pinWs, recordSockets: false },
          res,
        );
      } finally {
        await closeAll(res);
      }
    });
  }

  test("camera and mic off guest resumes live host video after its media socket is severed @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const res: CallResources = { browsers: [], contexts: [] };
    try {
      const { guestPage, guestConsole } = await lateJoinWithCameraAndMicOff(
        baseURL || "http://localhost:3001",
        `e2e_3058_reconnect_${Date.now()}`,
        { pinWs: true, recordSockets: true },
        res,
      );
      const connectedMark = guestConsole.filter((l) => l.includes(CONNECTED_LOG)).length;

      const sever = await severMediaWebSocket(guestPage);
      expect(
        sever.severed,
        `no live media socket closed on the guest (recorder saw ${sever.recorded})`,
      ).toBeGreaterThanOrEqual(1);

      await expect
        .poll(() => guestConsole.filter((l) => l.includes(CONNECTED_LOG)).length, {
          timeout: 60_000,
          intervals: [100],
          message: "the guest never reached Connected again after its media socket was severed",
        })
        .toBeGreaterThan(connectedMark);

      const resumedMs = await waitForLiveHostVideo(
        guestPage,
        RECONNECT_LIVE_FRAME_BUDGET_MS,
        "after the guest reconnected",
      );
      console.log(`[3058] reconnect: live host frame ${resumedMs}ms after Connected`);
    } finally {
      await closeAll(res);
    }
  });
});
