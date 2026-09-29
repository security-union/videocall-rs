import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import {
  addSyntheticAudioInput,
  getEncoderAudioGumCount,
  getEncoderAudioTracks,
  getEncoderVideoGumCount,
  getEncoderVideoTracks,
  getEnumerateDeviceCalls,
  installGetUserMediaMock,
} from "../helpers/media-mock";
import { enableCamera } from "../helpers/controls";
import { enterMeetingAsHost } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 2772: leaving must release the microphone AND the camera, and keep
 * them released. Back, not the hangup button — hangup navigates to "/" after
 * an awaited `leave_meeting`, discarding the in-page mock's records mid-assert.
 */

/** Derived from the 1000 ms `Timeout` behind which `host.rs`'s
 * `on_devices_changed` schedules `microphone.start()`, plus gUM. */
const RECLAIM_BUDGET_MS = 5_000;

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

async function lastEncoderTrackState(page: Page): Promise<string | undefined> {
  const acquisitions = await getEncoderAudioTracks(page);
  return acquisitions[acquisitions.length - 1]?.[0]?.readyState;
}

async function lastCameraTrackState(page: Page): Promise<string | undefined> {
  const acquisitions = await getEncoderVideoTracks(page);
  return acquisitions[acquisitions.length - 1]?.[0]?.readyState;
}

test.describe("issue 2772 media release on leaving the meeting", () => {
  test.describe.configure({ retries: 1 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("leaving releases the mic and the camera, and a devicechange does not re-claim the mic @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const context = await createAuthenticatedContext(
        browser,
        "release-on-leave@videocall.rs",
        "LeaveReleaseUser",
        uiURL,
      );
      const page = await context.newPage();
      await installGetUserMediaMock(page);

      await enterMeetingAsHost(page, `e2e_release_on_leave_${Date.now()}`);
      await enableMicAndAwaitCapture(page);
      await enableCamera(page);
      await expect
        .poll(() => getEncoderVideoGumCount(page), { timeout: 30_000 })
        .toBeGreaterThan(0);
      const gumBeforeLeave = await getEncoderAudioGumCount(page);

      await page.goBack();
      await expect(page.locator("#meeting-id")).toBeVisible({ timeout: 20_000 });

      await expect
        .poll(() => lastEncoderTrackState(page), {
          timeout: 20_000,
          message:
            "issue 2772 phase 1: unmounting `Host` must stop the microphone's capture " +
            "track. 'live' means the claim outlived the meeting, with the OS mic " +
            "indicator still lit on a page the user has left.",
        })
        .toBe("ended");

      await expect
        .poll(() => lastCameraTrackState(page), {
          timeout: 20_000,
          message:
            "issue 2772 phase 1b: unmounting `Host` must stop the camera's capture " +
            "track too. On unmount `release_all_media` is its only release path — the " +
            "level-triggered `in_call` branch covers the microphone alone.",
        })
        .toBe("ended");

      // `on_devices_changed` is on the GLOBAL mediaDevices and never cleared.
      const syntheticId = await addSyntheticAudioInput(page, "e2e-2772-extra-mic");
      const visible = await page.evaluate(
        async (id: string) =>
          (await navigator.mediaDevices.enumerateDevices()).some((d) => d.deviceId === id),
        syntheticId,
      );
      expect(
        visible,
        "the injected audioinput must be visible to the page's own enumerateDevices, " +
          "or the devicechange below cannot change the enumerated ID lists and this " +
          "phase asserts nothing",
      ).toBe(true);

      const enumerateBefore = await getEnumerateDeviceCalls(page);
      await page.evaluate(() => navigator.mediaDevices.dispatchEvent(new Event("devicechange")));
      await expect
        .poll(() => getEnumerateDeviceCalls(page), {
          timeout: 15_000,
          message:
            "the devicechange dispatch must reach a handler that re-enumerates; " +
            "without a re-enumeration nothing downstream of the event can run",
        })
        .toBeGreaterThan(enumerateBefore);
      await page.waitForTimeout(RECLAIM_BUDGET_MS);

      expect(
        await getEncoderAudioGumCount(page),
        "issue 2772 phase 2: a devicechange after leaving must NOT re-acquire the " +
          "microphone. A rising count is a permanent claim on a page with no " +
          "component left to release it.",
      ).toBe(gumBeforeLeave);

      expect(await lastEncoderTrackState(page)).toBe("ended");
    } finally {
      await browser.close();
    }
  });
});
