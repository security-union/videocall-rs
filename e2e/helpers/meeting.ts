import { Browser, BrowserContext, Locator, Page, expect } from "@playwright/test";
import { generateSessionToken } from "./auth";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

export const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
];

export type JoinResult = "in-meeting" | "waiting" | "waiting-for-meeting";

export async function createAuthenticatedContext(
  browser: Browser,
  email: string,
  name: string,
  uiURL: string,
): Promise<BrowserContext> {
  const context = await browser.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
  const url = new URL(uiURL);
  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: generateSessionToken(email, name),
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
  return context;
}

export async function navigateToMeeting(
  page: Page,
  meetingId: string,
  username: string,
): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#meeting-id")).toBeVisible({ timeout: 15_000 });
  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").fill("");
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 30 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 30 });
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
}

export async function joinMeetingFromPage(page: Page): Promise<JoinResult> {
  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 20_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 20_000 }).then(() => "waiting-for-meeting" as const),
  ]);
  if (result !== "join") {
    return result;
  }

  await joinButton.click();
  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

export async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: JoinResult,
): Promise<void> {
  if (guestResult !== "waiting") {
    return;
  }
  const admitButton = hostPage.getByTitle("Admit").first();
  await expect(admitButton).toBeVisible({ timeout: 20_000 });
  await admitButton.dispatchEvent("click");

  const guestJoinButton = guestPage.getByText(/Join Meeting|Start Meeting/);
  const guestGrid = guestPage.locator("#grid-container");
  const postAdmit = await Promise.race([
    guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
    guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
  ]);
  if (postAdmit === "join-button") {
    // The button can flash up and be replaced by the grid when the admitted
    // guest auto-joins, so don't wait forever for it.
    await guestJoinButton.click({ timeout: 5_000 }).catch(() => undefined);
  }
  await expect(guestGrid).toBeVisible({ timeout: 15_000 });
}

/** Host starts `meetingId`, guest joins (admitted if needed). */
export async function startTwoPartyMeeting(
  hostPage: Page,
  hostName: string,
  guestPage: Page,
  guestName: string,
  meetingId: string,
): Promise<void> {
  await navigateToMeeting(hostPage, meetingId, hostName);
  expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");
  await joinAsGuest(hostPage, guestPage, guestName, meetingId);
}

export async function joinAsGuest(
  hostPage: Page,
  guestPage: Page,
  guestName: string,
  meetingId: string,
): Promise<void> {
  await navigateToMeeting(guestPage, meetingId, guestName);
  const guestResult = await joinMeetingFromPage(guestPage);
  await admitGuestIfNeeded(hostPage, guestPage, guestResult);
}

/**
 * A bottom-bar control button, found by its tooltip text (e.g. "Hang Up",
 * "Open Diagnostics"). The mic, camera and share toggles have fixed names
 * instead; use `micButton`, `cameraButton` and `shareButton` for those.
 */
export function controlButton(page: Page, tooltip: string): Locator {
  return page.locator("button.video-control-button", {
    has: page.locator(".tooltip", { hasText: new RegExp(`^${tooltip}$`) }),
  });
}

/** The microphone toggle. Its name is fixed; `aria-pressed` says whether the mic is on. */
export function micButton(page: Page): Locator {
  return page.getByRole("button", { name: "Microphone" });
}

/** The camera toggle. Its name is fixed; `aria-pressed` says whether the camera is on. */
export function cameraButton(page: Page): Locator {
  return page.getByRole("button", { name: "Camera", exact: true });
}

/** The screen share toggle. Its name is fixed; `aria-pressed` says whether sharing is on. */
export function shareButton(page: Page): Locator {
  return page.getByRole("button", { name: "Share screen" });
}

/** Turn the local microphone on (the meeting starts with it off). */
export async function unmute(page: Page): Promise<void> {
  await micButton(page).click();
  await expect(micButton(page)).toHaveAttribute("aria-pressed", "true", { timeout: 10_000 });
}

export async function mute(page: Page): Promise<void> {
  await micButton(page).click();
  await expect(micButton(page)).toHaveAttribute("aria-pressed", "false", { timeout: 10_000 });
}

/** The speaking indicator on the remote peer tile whose name is `peerName`. */
export function peerAudioIndicator(page: Page, peerName: string): Locator {
  return page
    .locator("#grid-container .canvas-container", {
      has: page.locator(".floating-name", { hasText: peerName }),
    })
    .first()
    .locator(".audio-indicator")
    .first();
}
