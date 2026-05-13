import { type Page } from "@playwright/test";

/**
 * Steer the bot's Chrome from "just navigated to the meeting URL" into
 * "I'm in the grid with media flowing." Mirrors the join flow the
 * existing `e2e/tests/host-*.spec.ts` helpers walk through, but runs as
 * part of the bot's main launch path so the bot doesn't need a human
 * to type a display name or click "Start camera".
 *
 * Three branches the bot may land in after navigation:
 *   1. **Homepage form** (`#meeting-id` + `#username` visible). The bot
 *      fills both and presses Enter.
 *   2. **"Start Meeting" / "Join Meeting" button** is visible — typical
 *      on a fresh meeting URL once the display name is known. Bot clicks.
 *   3. **In-meeting** (`#grid-container` already visible) — direct entry,
 *      nothing to do.
 *
 * After landing in the grid the bot clicks "Start camera" and
 * "Unmute Microphone" so the prep'd fake-device files (PR-1c/1d)
 * actually surface as audio + video to the human peer.
 */
export async function joinMeetingAndEnableMedia(args: {
  page: Page;
  participant: string;
  displayName: string;
  meetingId: string;
}): Promise<void> {
  const { page, participant, displayName, meetingId } = args;

  // ── Step 1: get past the homepage form / join button into the grid ──

  const homepageMeetingInput = page.locator("#meeting-id");
  const homepageUsernameInput = page.locator("#username");
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const landed = await Promise.race([
    homepageMeetingInput
      .waitFor({ timeout: 15_000 })
      .then(() => "homepage-form" as const)
      .catch(() => null),
    joinButton
      .waitFor({ timeout: 15_000 })
      .then(() => "join-button" as const)
      .catch(() => null),
    grid
      .waitFor({ timeout: 15_000 })
      .then(() => "in-meeting" as const)
      .catch(() => null),
  ]);

  if (landed === "homepage-form") {
    console.log(`[${participant}] homepage form detected — filling and submitting`);
    await homepageMeetingInput.click();
    await homepageMeetingInput.pressSequentially(meetingId, { delay: 30 });
    await homepageUsernameInput.click();
    await homepageUsernameInput.fill("");
    await homepageUsernameInput.pressSequentially(displayName, { delay: 30 });
    await page.waitForTimeout(300);
    await homepageUsernameInput.press("Enter");
  }

  // After the form (or arriving here directly), wait for either the
  // grid (auto-joined) or a "Join Meeting" button. Re-evaluate because
  // the homepage submit can transition through either state.
  const postForm = await Promise.race([
    joinButton
      .waitFor({ timeout: 15_000 })
      .then(() => "join-button" as const)
      .catch(() => null),
    grid
      .waitFor({ timeout: 15_000 })
      .then(() => "in-meeting" as const)
      .catch(() => null),
  ]);

  if (postForm === "join-button") {
    console.log(`[${participant}] clicking Join Meeting`);
    await joinButton.click();
  }

  await grid.waitFor({ timeout: 30_000 });
  console.log(`[${participant}] in-meeting (grid visible)`);

  // ── Step 2: enable mic + camera so the prep'd fake devices flow ──

  await enableMicIfPresent(page, participant);
  await enableCameraIfPresent(page, participant);
}

async function enableMicIfPresent(page: Page, participant: string): Promise<void> {
  const unmuteBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Unmute Microphone" }),
  });
  try {
    if (await unmuteBtn.isVisible({ timeout: 5_000 }).catch(() => false)) {
      await unmuteBtn.click({ timeout: 2_000 });
      console.log(`[${participant}] microphone enabled`);
      await page.waitForTimeout(300);
      return;
    }
  } catch (e) {
    console.warn(`[${participant}] enableMic failed:`, (e as Error).message);
    return;
  }
  console.log(`[${participant}] microphone already enabled (or button not found)`);
}

async function enableCameraIfPresent(page: Page, participant: string): Promise<void> {
  const startCamBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Start camera" }),
  });
  try {
    if (await startCamBtn.isVisible({ timeout: 5_000 }).catch(() => false)) {
      await startCamBtn.click({ timeout: 2_000 });
      console.log(`[${participant}] camera enabled`);
      await page.waitForTimeout(300);
      return;
    }
  } catch (e) {
    console.warn(`[${participant}] enableCamera failed:`, (e as Error).message);
    return;
  }
  console.log(`[${participant}] camera already enabled (or button not found)`);
}
