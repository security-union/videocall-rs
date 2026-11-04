import { test, expect, chromium, Page, Browser } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { enableCamera } from "../helpers/controls";
import {
  getEncoderAudioGumCount,
  getEncoderAudioTracks,
  getEncoderVideoGumCount,
  getEncoderVideoTracks,
  installGetUserMediaMock,
} from "../helpers/media-mock";
import { createMeeting, fetchWaiting } from "../helpers/meeting-api";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { expectSelfRecording } from "../helpers/recording-status";
import {
  enterMeetingAsHost,
  guestJoinsMeeting,
  joinMeetingFromPage,
} from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 3163: a Ctrl-K jump from meeting A to meeting B must leave A — its
 * peers see the user go, A's camera and microphone are released, a recording
 * of A stops — and put the user through B's own join flow. Leaving a waiting
 * room in-app withdraws the join request.
 */

const HOST = { email: "jump-host@videocall.rs", name: "JumpHost" };
const GUEST = { email: "jump-guest@videocall.rs", name: "JumpGuest" };

/** No-op save picker, so a recording streams to a fake file without a dialog. */
const STUB_FILE_PICKER_SCRIPT = `
  window.showSaveFilePicker = async function () {
    return {
      name: "jump.webm",
      createWritable: async function () {
        return { write: async function () {}, close: async function () {}, abort: async function () {} };
      },
    };
  };
`;

async function enableMicAndAwaitCapture(page: Page): Promise<void> {
  await page.mouse.move(400, 400);
  const toggle = page.locator('[data-testid="mic-toggle-button"]');
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  if (!((await toggle.getAttribute("class")) || "").includes("active")) {
    await toggle.click();
  }
  await expect(toggle).toHaveClass(/\bactive\b/, { timeout: 15_000 });
  await expect.poll(() => getEncoderAudioGumCount(page), { timeout: 30_000 }).toBeGreaterThan(0);
}

async function lastMicTrackState(page: Page): Promise<string | undefined> {
  const acquisitions = await getEncoderAudioTracks(page);
  return acquisitions[acquisitions.length - 1]?.[0]?.readyState;
}

async function lastCameraTrackState(page: Page): Promise<string | undefined> {
  const acquisitions = await getEncoderVideoTracks(page);
  return acquisitions[acquisitions.length - 1]?.[0]?.readyState;
}

async function recordingState(page: Page): Promise<string> {
  return page.evaluate(() =>
    (window as unknown as { __vcRecording: { getState(): string } }).__vcRecording.getState(),
  );
}

async function jumpTo(page: Page, meetingId: string): Promise<void> {
  await page.keyboard.press("Control+k");
  const input = page.getByPlaceholder("Search meetings...");
  await expect(input).toBeVisible({ timeout: 5_000 });
  await input.fill(meetingId);
  const result = page.locator(`a[href="/meeting/${meetingId}"]`);
  await expect(result).toBeVisible({ timeout: 10_000 });
  await result.click();
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}$`), { timeout: 10_000 });
}

async function newPage(browser: Browser, user: typeof HOST, uiURL: string): Promise<Page> {
  const context = await createAuthenticatedContext(browser, user.email, user.name, uiURL);
  await context.addInitScript(STUB_FILE_PICKER_SCRIPT);
  return context.newPage();
}

test.describe("issue 3163 Ctrl-K jump between meetings", () => {
  test.describe.configure({ retries: 1 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("jumping from a call leaves it, releases its camera and mic, and starts the new join flow", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const stamp = Date.now();
    const meetingA = `e2e_jump_a_${stamp}`;
    const meetingB = `e2e_jump_b_${stamp}`;
    await createMeeting(HOST.email, HOST.name, { meetingId: meetingB });

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostPage = await newPage(browser, HOST, uiURL);
      const guestPage = await newPage(browser, GUEST, uiURL);
      await installGetUserMediaMock(hostPage);

      await enterMeetingAsHost(hostPage, meetingA, HOST.name);
      await guestJoinsMeeting(hostPage, guestPage, meetingA, GUEST.name);
      await enableMicAndAwaitCapture(hostPage);
      await enableCamera(hostPage);
      await expect
        .poll(() => getEncoderVideoGumCount(hostPage), { timeout: 30_000 })
        .toBeGreaterThan(0);
      const guestSeesHost = guestPage.locator("#grid-container .canvas-container");
      await expect(guestSeesHost).toHaveCount(1, { timeout: 30_000 });

      await jumpTo(hostPage, meetingB);

      await expect(guestSeesHost, "meeting A's peers must see the user leave").toHaveCount(0, {
        timeout: 30_000,
      });
      await expect
        .poll(() => lastMicTrackState(hostPage), {
          timeout: 20_000,
          message: "the microphone captured for meeting A must be released",
        })
        .toBe("ended");
      await expect
        .poll(() => lastCameraTrackState(hostPage), {
          timeout: 20_000,
          message: "the camera captured for meeting A must be released",
        })
        .toBe("ended");

      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");
      await expect(guestSeesHost).toHaveCount(0);
    } finally {
      await browser.close();
    }
  });

  test("jumping while recording stops the recording of the meeting left", async ({ baseURL }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const stamp = Date.now();
    const meetingA = `e2e_jump_rec_a_${stamp}`;
    const meetingB = `e2e_jump_rec_b_${stamp}`;
    await createMeeting(HOST.email, HOST.name, { meetingId: meetingB });

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostPage = await newPage(browser, HOST, uiURL);
      await enterMeetingAsHost(hostPage, meetingA, HOST.name);
      await hostPage.getByTestId("record-button").click();
      await expectSelfRecording(hostPage, "You are recording");
      expect(await recordingState(hostPage)).toBe("recording");

      await jumpTo(hostPage, meetingB);

      await expect
        .poll(() => recordingState(hostPage), {
          timeout: 20_000,
          message: "a recording of meeting A must not keep capturing after the jump",
        })
        .not.toMatch(/^(recording|activating)$/);
    } finally {
      await browser.close();
    }
  });

  // The search modal (z-index 50) renders beneath the waiting room (1000), so
  // this leaves the waiting room the other in-app way: browser Back.
  test("leaving a waiting room in-app withdraws the request to join", async ({ baseURL }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingA = `e2e_jump_wait_a_${Date.now()}`;
    await createMeeting(HOST.email, HOST.name, { meetingId: meetingA, waitingRoomEnabled: true });

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostPage = await newPage(browser, HOST, uiURL);
      const guestPage = await newPage(browser, GUEST, uiURL);
      await enterMeetingAsHost(hostPage, meetingA, HOST.name);

      await fillAndSubmitJoinForm(guestPage, meetingA, GUEST.name);
      expect(await joinMeetingFromPage(guestPage)).toBe("waiting");
      await expect
        .poll(async () => (await fetchWaiting(HOST.email, HOST.name, meetingA)).length, {
          timeout: 20_000,
        })
        .toBe(1);

      await guestPage.goBack();
      await expect(guestPage.locator("#meeting-id")).toBeVisible({ timeout: 20_000 });

      await expect
        .poll(async () => (await fetchWaiting(HOST.email, HOST.name, meetingA)).length, {
          timeout: 20_000,
          message: "the user who left must leave meeting A's waiting room",
        })
        .toBe(0);
    } finally {
      await browser.close();
    }
  });
});
