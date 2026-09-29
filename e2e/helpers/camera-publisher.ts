// The camera AQ axes only tick while the camera encoder is alive.
import { Page, expect } from "@playwright/test";
import { wakeControls } from "./controls";
import { enterMeetingAsHost, guestJoinsMeeting } from "./two-user-meeting";

async function armPrejoinCamera(page: Page): Promise<void> {
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("vc_prejoin_camera_on", "true");
      window.localStorage.setItem("vc_prejoin_mic_on", "true");
    } catch {
      /* storage unavailable pre-navigation; the app origin seeds it */
    }
  });
}

async function confirmCameraPublishing(page: Page): Promise<void> {
  await wakeControls(page);

  const startBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Start Video" }),
  });
  if (await startBtn.isVisible().catch(() => false)) {
    await startBtn.click();
  }
  await expect(
    page.locator("button.video-control-button", {
      has: page.locator("span.tooltip", { hasText: "Stop Video" }),
    }),
    "the camera is not publishing, so the camera AQ monitor loop is not running and no axis " +
      "can read the injected counters.",
  ).toBeVisible({ timeout: 30_000 });
}

export async function joinAndStartCamera(page: Page, meetingId: string): Promise<void> {
  await armPrejoinCamera(page);
  await enterMeetingAsHost(page, meetingId);
  await confirmCameraPublishing(page);
}

export async function joinAsGuestAndStartCamera(
  hostPage: Page,
  guestPage: Page,
  meetingId: string,
): Promise<void> {
  await armPrejoinCamera(guestPage);
  await guestJoinsMeeting(hostPage, guestPage, meetingId);
  await confirmCameraPublishing(guestPage);
}
