import { type Page, type Locator } from "@playwright/test";

/**
 * Sentinel thrown by `joinMeetingAndEnableMedia` when the page navigates
 * away from `/meeting/...` while we're still trying to enter the grid —
 * almost always because the user clicked the in-browser HangUp control.
 * The orchestrator catches this and routes the bot through the
 * "graceful early exit" path instead of counting it as a launch error.
 */
export class MeetingNavigatedAwayError extends Error {
  public readonly kind = "meeting-navigated-away" as const;
  constructor(message: string) {
    super(message);
    this.name = "MeetingNavigatedAwayError";
  }
}

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
 * The pre-join card in `dioxus-ui/src/components/pre_join_settings_card.rs`
 * and the blocked-device variant in `dioxus-ui/src/components/attendants.rs`
 * use the **same label text** ("Start Meeting" / "Join Meeting") — the
 * blocked variant adds `disabled: true` + `aria-disabled: "true"`. The
 * locator below restricts to the **enabled** variant so a no-op click
 * on the blocked card can't waste the join budget.
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
  // Restrict the join button locator to the **enabled** variant. The
  // blocked-device card in `attendants.rs` renders an identically
  // labelled but disabled button — clicking that one silently does
  // nothing and was the proximate cause of the user's 30s `grid.waitFor`
  // timeout when the bot was the meeting owner.
  const joinButton = page
    .getByRole("button", { name: /Start Meeting|Join Meeting/ })
    .and(page.locator(':not([disabled]):not([aria-disabled="true"])'));
  const grid = page.locator("#grid-container");

  // Subscribe to top-frame navigations so a manual in-browser hang-up
  // (which routes the page back to `/`) doesn't strand us inside a
  // 45-second `grid.waitFor`. The handler stays installed for the full
  // duration of the join flow.
  const meetingPathPrefix = `/meeting/${meetingId}`;
  let navigatedAway = false;
  const onFrameNavigated = (frame: { parentFrame: () => unknown; url: () => string }): void => {
    // Top frame only.
    if (frame.parentFrame() !== null) return;
    let pathname: string;
    try {
      pathname = new URL(frame.url()).pathname;
    } catch {
      return;
    }
    // Tolerate trailing slashes / query — only flag if we've left the
    // meeting path altogether.
    if (!pathname.startsWith(meetingPathPrefix)) {
      navigatedAway = true;
    }
  };
  page.on("framenavigated", onFrameNavigated);

  try {
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

    throwIfNavigatedAway(navigatedAway, participant);

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

    throwIfNavigatedAway(navigatedAway, participant);

    // Best-effort: if this bot is the meeting owner (first participant
    // in a non-existing meeting), the pre-join card renders a
    // "Waiting Room" toggle that defaults to ON. Leaving it on would
    // park every subsequent peer in a waiting room the bot has no
    // admit logic for. Disable it before clicking Join. The toggle is
    // only present when `is_owner = true` — see
    // `dioxus-ui/src/components/pre_join_settings_card.rs:102-159` —
    // so this is a no-op (skipped silently) when joining an existing
    // meeting.
    if (await joinButton.isVisible({ timeout: 2_000 }).catch(() => false)) {
      await disableWaitingRoomIfOwner(page, participant);
    }

    throwIfNavigatedAway(navigatedAway, participant);

    // Either we just filled a display name (which arms the Join button)
    // or we landed straight on a Join button. Click it if present, then
    // race the grid against a re-appearance of the (enabled) Join
    // button — if the button comes back, the previous click was a
    // no-op (the button was still disabled, the click landed off-target,
    // or the connection failed mid-attempt). Cap retries at 3.
    await joinWithRetries({
      participant,
      joinButton,
      grid,
      isNavigatedAway: () => navigatedAway,
    });

    throwIfNavigatedAway(navigatedAway, participant);

    console.log(`[${participant}] in-meeting (grid visible)`);

    // ── Step 2: enable mic + camera so the prep'd fake devices flow ───

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
  } finally {
    page.off("framenavigated", onFrameNavigated);
  }
}

function throwIfNavigatedAway(navigatedAway: boolean, participant: string): void {
  if (navigatedAway) {
    console.log(
      `[${participant}] page navigated away from meeting (likely manual hang-up) — exiting cleanly`,
    );
    throw new MeetingNavigatedAwayError(
      "page navigated away from /meeting/ during join (likely manual hang-up)",
    );
  }
}

/**
 * Best-effort attempt to flip the pre-join card's "Waiting Room" toggle
 * to OFF when the bot is the meeting owner.
 *
 * Context: the toggle renders only when `is_owner = true` in
 * `dioxus-ui/src/components/pre_join_settings_card.rs` (lines 102-159).
 * `is_owner` is true when the bot is the first participant in a
 * non-existing meeting. Its default value is ON — leaving it that way
 * would strand any peer (human or bot) that joins afterwards because the
 * bot has no admit logic. The "Admitted can admit" toggle is automatically
 * disabled by the UI when Waiting Room is off (see the same file,
 * lines 139-141), so we don't need to touch it here.
 *
 * The toggle is the `ToggleSwitch` component
 * (`dioxus-ui/src/components/toggle_switch.rs`), which renders as a
 * `<button role="switch" aria-checked="true|false">`. Clicking it triggers
 * an async PATCH against the meeting settings API, so we wait ~300ms for
 * the save to settle.
 *
 * This step is best-effort: a missing toggle (the common case where the
 * bot is joining an existing meeting), a click failure, or a `aria-checked`
 * read failure must not block the join. We log a warning and move on.
 */
async function disableWaitingRoomIfOwner(page: Page, participant: string): Promise<void> {
  const waitingRoomRow = page.locator(".settings-option-row").filter({ hasText: "Waiting Room" });
  const waitingRoomToggle = waitingRoomRow.locator('[role="switch"]').first();

  // Short visibility timeout: in the common case the toggle isn't
  // present (bot is joining an existing meeting), we don't want to
  // burn 30s waiting on a UI element that won't appear.
  const toggleVisible = await waitingRoomToggle.isVisible({ timeout: 2_000 }).catch(() => false);
  if (!toggleVisible) {
    console.log(
      `[${participant}] Waiting Room toggle not present — skipping (bot is not meeting owner)`,
    );
    return;
  }

  try {
    const ariaChecked = await waitingRoomToggle.getAttribute("aria-checked");
    if (ariaChecked === "false") {
      // Already off — nothing to do.
      return;
    }
    if (ariaChecked !== "true") {
      console.warn(
        `[${participant}] Waiting Room toggle has unexpected aria-checked="${ariaChecked}" — skipping`,
      );
      return;
    }
    console.log(`[${participant}] disabling Waiting Room (bot is meeting owner)`);
    await waitingRoomToggle.click({ timeout: 2_000 });
    // The toggle's onclick triggers an async PATCH against the meeting
    // settings API (see pre_join_settings_card.rs:127-156). Give it a
    // moment to settle so the click isn't lost to a re-render.
    await page.waitForTimeout(300);
  } catch (e) {
    console.warn(
      `[${participant}] could not disable Waiting Room toggle (proceeding with join):`,
      (e as Error).message,
    );
  }
}

/**
 * Click the (enabled) Join button up to `maxAttempts` times, racing the
 * grid against the Join button reappearing after each click. A
 * reappearance signals the previous click was consumed by the UI but
 * the join did not complete (e.g. transient connection error,
 * disabled-mid-click, blocked-card switch).
 *
 * The grid wait per-attempt is 45s — the netsim'd `lossy_mobile` profile
 * regularly takes 20-35s to bring the WebTransport stream up.
 */
async function joinWithRetries(args: {
  participant: string;
  joinButton: Locator;
  grid: Locator;
  isNavigatedAway: () => boolean;
}): Promise<void> {
  const { participant, joinButton, grid, isNavigatedAway } = args;
  const maxAttempts = 3;
  const perAttemptGridTimeout = 45_000;

  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    if (isNavigatedAway()) return; // surfaced by caller's throwIfNavigatedAway
    // If the grid is already up (rare but possible if we raced past it),
    // we're done.
    if (await grid.isVisible({ timeout: 200 }).catch(() => false)) {
      return;
    }

    const sawEnabledButton = await joinButton.isVisible({ timeout: 5_000 }).catch(() => false);
    if (!sawEnabledButton) {
      // No enabled join button on screen. Could mean: (a) the click
      // already consumed the form and we're waiting for the grid, or
      // (b) only the disabled variant is showing. Log + fall through
      // to the grid wait — if the grid never shows, the outer
      // `grid.waitFor` throws.
      if (attempt === 1) {
        console.log(`[${participant}] join button not enabled yet, waiting for grid`);
      } else {
        console.log(
          `[${participant}] retrying join click (attempt ${attempt}) — no enabled button visible`,
        );
      }
      try {
        await grid.waitFor({ timeout: perAttemptGridTimeout });
        return;
      } catch {
        if (attempt === maxAttempts)
          throw new Error("grid did not become visible after join click");
        continue;
      }
    }

    if (attempt === 1) {
      console.log(`[${participant}] clicking Join Meeting`);
    } else {
      console.log(`[${participant}] retrying join click (attempt ${attempt})`);
    }
    // Playwright's auto-waiting + actionability checks effectively
    // cover "stable" — the locator already has `:not([disabled])` and
    // Playwright waits for the element to be stable before clicking.
    try {
      await joinButton.click({ timeout: 5_000 });
    } catch (e) {
      console.warn(`[${participant}] join-click warning:`, (e as Error).message);
    }

    // Race the grid against a re-appearance of the enabled Join
    // button. Reappearance ⇒ click was a no-op or the UI rolled back.
    const outcome = await Promise.race([
      grid.waitFor({ timeout: perAttemptGridTimeout }).then(() => "grid" as const),
      joinButton
        .waitFor({ timeout: perAttemptGridTimeout, state: "visible" })
        .then(() => "button-reappeared" as const)
        .catch(() => null),
    ]).catch(() => null);

    if (outcome === "grid") {
      console.log(`[${participant}] join click consumed`);
      return;
    }
    if (outcome === "button-reappeared") {
      if (attempt === maxAttempts) {
        throw new Error(
          `join button reappeared after ${maxAttempts} click attempts — grid never became visible`,
        );
      }
      // Loop and retry. The retry log line fires at the top of the
      // next iteration so the attempt number is consistent.
      continue;
    }
    // outcome === null ⇒ neither resolved within the per-attempt
    // timeout. Treat as failure of this attempt.
    if (attempt === maxAttempts) {
      throw new Error(
        `grid did not become visible within ${perAttemptGridTimeout}ms after join click`,
      );
    }
  }
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
