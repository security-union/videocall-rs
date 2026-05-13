import { type Page, type Locator } from "@playwright/test";

/**
 * Steer the bot's Chrome from "just navigated to the meeting URL" into
 * "I'm in the grid with media flowing." Runs as part of the bot's main
 * launch path so the bot doesn't need a human to type a display name or
 * click the start-mic / start-camera controls.
 *
 * Post-navigation states the bot may land in:
 *   1. **Homepage form** (`#meeting-id` + `#username` visible) — when
 *      the goto URL resolved to `/`. The bot fills both fields.
 *   2. **Meeting-page display-name prompt** — when on `/meeting/<id>`
 *      without a stored display name. The input has no `id` and is
 *      matched by `placeholder="Enter your display name"` (defined in
 *      `dioxus-ui/src/pages/meeting.rs`).
 *   3. **"Start Meeting" / "Join Meeting" button** visible without an
 *      input — when the display name is already known. Bot clicks.
 *   4. **In-meeting** (`#grid-container` already visible) — nothing to do.
 *
 * After landing in the grid the bot hovers the action bar (it autohides
 * by default) and clicks the "Unmute" + "Start Video" controls so the
 * prep'd fake-device files (PR-1c/1d) actually surface as audio + video
 * to the human peer. The control tooltips are sourced from
 * `dioxus-ui/src/components/video_control_buttons.rs`; if those rename
 * in the future, update the selector list below.
 */
export async function joinMeetingAndEnableMedia(args: {
  page: Page;
  participant: string;
  displayName: string;
  meetingId: string;
}): Promise<void> {
  const { page, participant, displayName, meetingId } = args;

  // ── Step 1: detect where the navigation landed ──────────────────────

  const homepageMeetingInput = page.locator("#meeting-id");
  const meetingPageDisplayNameInput = page
    .locator('input[placeholder="Enter your display name"]')
    .first();
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const landed = await Promise.race([
    homepageMeetingInput
      .waitFor({ timeout: 15_000 })
      .then(() => "homepage-form" as const)
      .catch(() => null),
    meetingPageDisplayNameInput
      .waitFor({ timeout: 15_000 })
      .then(() => "meeting-name-prompt" as const)
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
    console.log(`[${participant}] homepage form detected — filling`);
    const homepageUsernameInput = page.locator("#username");
    await homepageMeetingInput.click();
    await homepageMeetingInput.pressSequentially(meetingId, { delay: 30 });
    await homepageUsernameInput.click();
    await homepageUsernameInput.fill("");
    await homepageUsernameInput.pressSequentially(displayName, { delay: 30 });
    await page.waitForTimeout(300);
    await homepageUsernameInput.press("Enter");
  } else if (landed === "meeting-name-prompt") {
    console.log(`[${participant}] meeting-page display-name prompt detected — filling`);
    await meetingPageDisplayNameInput.click();
    await meetingPageDisplayNameInput.fill("");
    await meetingPageDisplayNameInput.pressSequentially(displayName, { delay: 30 });
    await page.waitForTimeout(300);
  }

  // Either we just filled a display name (which arms the Join button)
  // or we landed straight on a Join button. Click it if present.
  if (await joinButton.isVisible({ timeout: 5_000 }).catch(() => false)) {
    console.log(`[${participant}] clicking Join Meeting`);
    await joinButton.click({ timeout: 5_000 }).catch((e: unknown) => {
      console.warn(`[${participant}] join-click warning:`, (e as Error).message);
    });
  }

  await grid.waitFor({ timeout: 30_000 });
  console.log(`[${participant}] in-meeting (grid visible)`);

  // ── Step 2: enable mic + camera so the prep'd fake devices flow ─────

  // The action bar auto-hides by default; hover it so the buttons are
  // visible to Playwright's isVisible check.
  const controlsContainer = page.locator(".video-controls-container").first();
  await controlsContainer.hover().catch(() => {
    // Fine — some layouts may not need the hover.
  });
  await page.waitForTimeout(200);

  await clickWhenVisible(page, participant, "microphone", [
    'button.video-control-button:has(span.tooltip:has-text("Unmute"))',
    'button.video-control-button:has(span.tooltip:has-text("Unmute Microphone"))',
    'button.video-control-button:has(span.tooltip:has-text("Start microphone"))',
  ]);
  await clickWhenVisible(page, participant, "camera", [
    'button.video-control-button:has(span.tooltip:has-text("Start Video"))',
    'button.video-control-button:has(span.tooltip:has-text("Start camera"))',
  ]);
}

async function clickWhenVisible(
  page: Page,
  participant: string,
  label: string,
  selectors: readonly string[],
): Promise<void> {
  for (const sel of selectors) {
    const candidate: Locator = page.locator(sel).first();
    try {
      if (await candidate.isVisible({ timeout: 2_000 }).catch(() => false)) {
        await candidate.click({ timeout: 2_000 });
        console.log(`[${participant}] ${label} enabled`);
        await page.waitForTimeout(300);
        return;
      }
    } catch (e) {
      console.warn(
        `[${participant}] enable ${label} failed for selector ${sel}:`,
        (e as Error).message,
      );
    }
  }
  console.warn(
    `[${participant}] could not find a visible ${label} enable button — selectors tried: ${selectors.join(" | ")}. The action bar may have autohidden or the tooltip text changed.`,
  );
}
