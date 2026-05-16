import { type Page, type Locator } from "@playwright/test";

/**
 * The mode the pre-join card was rendered in:
 *   - "start" — the bot is the meeting owner. Button label is "Start Meeting"
 *     and the Waiting Room toggle is visible (and defaults to ON).
 *   - "join"  — the bot is joining an existing meeting. Button label is
 *     "Join Meeting"; no Waiting Room toggle is rendered.
 *   - "unknown" — locator returned text that doesn't match either label.
 *     Bot falls through and clicks the matched button anyway (legacy
 *     behaviour) so a future relabel doesn't strand the bot.
 *
 * Centralized so the helper, the orchestrator logs, and the unit tests
 * all agree on the discriminator strings.
 */
export type JoinMode = "start" | "join" | "unknown";

/**
 * Inspect the visible text of the (already-resolved enabled) join
 * button and return whether it's rendering in Start or Join mode.
 *
 * The pre-join card in `dioxus-ui/src/components/pre_join_settings_card.rs`
 * uses the label "Start Meeting" when `is_owner = true` and "Join Meeting"
 * otherwise. The button label is the **only** signal in the DOM that's
 * stable across renders — `is_owner` itself is Rust-side state and the
 * Waiting Room toggle only exists in Start mode (so its absence is a
 * weaker signal than the button text).
 *
 * Exported so unit tests can exercise the regex without spinning up
 * Chrome.
 */
export async function detectJoinMode(joinButton: Locator): Promise<JoinMode> {
  const text = await joinButton.innerText().catch(() => "");
  return classifyJoinModeText(text);
}

/**
 * Pure function for the join-mode classification — split out so the
 * test harness can drive it with literal strings instead of mocking a
 * full Locator. The label match is anchored at the start of the
 * trimmed string and case-insensitive so the bot tolerates a future
 * trailing-icon or trailing-text change to the button.
 */
export function classifyJoinModeText(rawText: string): JoinMode {
  const normalized = rawText.trim();
  if (/^Start Meeting/i.test(normalized)) return "start";
  if (/^Join Meeting/i.test(normalized)) return "join";
  return "unknown";
}

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
 * Thrown when the bot's join request succeeded at the API level but the
 * meeting has Waiting Room enabled and the host has not yet admitted us.
 *
 * Two sub-states resolve to this error:
 *   - `MeetingStatus::Waiting` (host's Waiting Room is on and we landed
 *     in the lobby; identified by `[data-testid="meeting-waiting-room"]`).
 *   - `MeetingStatus::WaitingForMeeting` (the host hasn't started the
 *     meeting yet; identified by `[data-testid="meeting-waiting-for-host"]`).
 *
 * The orchestrator treats this as a graceful exit (not a failure) — the
 * bot DID join, it's simply parked. Counting it as an error generates
 * misleading "ended with an error" tallies for runs where the operator
 * deliberately joins a meeting they can't admit themselves into.
 */
export class WaitingRoomError extends Error {
  public readonly kind = "waiting-room" as const;
  public readonly variant: "waiting-room" | "waiting-for-host";
  constructor(variant: "waiting-room" | "waiting-for-host", message: string) {
    super(message);
    this.name = "WaitingRoomError";
    this.variant = variant;
  }
}

/**
 * Thrown when the join attempt landed on a terminal failure screen:
 *   - `MeetingStatus::Rejected` (host denied the join request).
 *   - `MeetingStatus::Error(...)` (server-side join error — meeting closed,
 *     host left, etc.).
 *
 * Surfaces to the orchestrator as a real failure (counts toward the
 * "ended with an error" tally) but with a clean per-bot diagnostic
 * instead of the misleading "join button reappeared" message that
 * the legacy grid-only `waitFor` produced when the page transitioned
 * to a non-grid terminal state.
 */
export class JoinRejectedError extends Error {
  public readonly kind = "join-rejected" as const;
  public readonly reason: "rejected" | "error";
  constructor(reason: "rejected" | "error", message: string) {
    super(message);
    this.name = "JoinRejectedError";
    this.reason = reason;
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

    // Detect whether the bot is about to click "Start Meeting" (bot is
    // the meeting owner — Waiting Room toggle is visible and defaults
    // ON) or "Join Meeting" (joining an existing meeting — no toggle).
    // The detection is done once here, BEFORE the click, so the log
    // explicitly records which path the bot took. In Start mode we
    // also verify (and if necessary flip) the Waiting Room toggle to
    // OFF — leaving it ON would strand every subsequent peer (human or
    // bot) because the bot has no admit logic.
    //
    // `mode` is "unknown" when the matched button's label doesn't
    // match either canonical string — almost certainly a future
    // relabel. The bot still clicks the button (legacy behaviour) so
    // a label rename doesn't immediately strand the bot; it just
    // skips the Waiting Room verification because the toggle's
    // presence is correlated with the Start label.
    let mode: JoinMode = "unknown";
    if (await joinButton.isVisible({ timeout: 2_000 }).catch(() => false)) {
      mode = await detectJoinMode(joinButton);
      if (mode === "start") {
        console.log(
          `[${participant}] detected mode: Start Meeting (bot is meeting owner — verifying Waiting Room is OFF before starting)`,
        );
        await ensureWaitingRoomOff(page, participant);
      } else if (mode === "join") {
        console.log(`[${participant}] detected mode: Join Meeting (joining existing meeting)`);
      } else {
        console.log(
          `[${participant}] detected mode: unknown — falling back to clicking the matched button as-is`,
        );
      }
    }

    throwIfNavigatedAway(navigatedAway, participant);

    // Either we just filled a display name (which arms the Join button)
    // or we landed straight on a Join button. Click it if present, then
    // race the grid against a re-appearance of the (enabled) Join
    // button — if the button comes back, the previous click was a
    // no-op (the button was still disabled, the click landed off-target,
    // or the connection failed mid-attempt). Cap retries at 3.
    //
    // The race also includes the three non-grid terminal states surfaced
    // by `dioxus-ui/src/pages/meeting.rs`:
    //   - `[data-testid="meeting-waiting-room"]`     (Waiting — Waiting Room ON)
    //   - `[data-testid="meeting-waiting-for-host"]` (WaitingForMeeting — host hasn't started)
    //   - `[data-testid="meeting-rejected"]`         (Rejected — host denied)
    //   - `[data-testid="meeting-error"]`            (Error — server-side join failure)
    // Detection short-circuits the retry loop with a typed error so the
    // orchestrator can report the right thing.
    await joinWithRetries({
      page,
      participant,
      joinButton,
      grid,
      mode,
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
 * Verify (and if necessary flip) the pre-join card's "Waiting Room"
 * toggle to OFF before the bot clicks Start Meeting.
 *
 * Replaces the v1.6.x `disableWaitingRoomIfOwner` helper. The behaviour
 * is the same on the happy path (toggle present + ON → click → wait
 * for `aria-checked="false"`), but the post-condition assertion + log
 * lines are explicit so operators can tell from the bot's log whether
 * the toggle was already off, was just flipped off, or wasn't present
 * at all. Without this an operator reading the log can't distinguish
 * "Start mode, toggle was already off" from "Join mode, toggle never
 * existed" — both produced the same log silence before.
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
 * an async PATCH against the meeting settings API; we wait for
 * `aria-checked` to flip to `"false"` (up to 5s) as the post-condition.
 *
 * This step is best-effort: a missing toggle (the common case where the
 * bot is joining an existing meeting), a click failure, or an
 * `aria-checked` read failure must not block the join. We log a warning
 * and move on.
 *
 * Exported so the caller's pre-Start verification path can short-circuit
 * cleanly without duplicating the locator query.
 */
export async function ensureWaitingRoomOff(page: Page, participant: string): Promise<void> {
  const waitingRoomRow = page.locator(".settings-option-row").filter({ hasText: "Waiting Room" });
  const toggle = waitingRoomRow.locator('[role="switch"]').first();

  // Short visibility timeout: in the common case the toggle isn't
  // present (bot is joining an existing meeting), we don't want to
  // burn 30s waiting on a UI element that won't appear. Absent toggle
  // ⇒ not in Start mode ⇒ nothing to do; not an error.
  const toggleVisible = await toggle.isVisible({ timeout: 2_000 }).catch(() => false);
  if (!toggleVisible) {
    console.log(`[${participant}] Waiting Room toggle not present — skipping`);
    return;
  }

  try {
    const current = await toggle.getAttribute("aria-checked");
    if (current === "false") {
      console.log(`[${participant}] Waiting Room is already OFF`);
      return;
    }
    if (current !== "true") {
      console.warn(
        `[${participant}] Waiting Room toggle has unexpected aria-checked="${current}" — skipping`,
      );
      return;
    }
    console.log(`[${participant}] Waiting Room is ON — disabling`);
    await toggle.click({ timeout: 2_000 });
    // Wait for `aria-checked` to flip. This is the explicit
    // post-condition: the click only matters if it lands the toggle
    // in the OFF state. The async PATCH against the meeting settings
    // API (see pre_join_settings_card.rs:127-156) settles inside
    // this window.
    await waitingRoomRow
      .locator('[role="switch"][aria-checked="false"]')
      .first()
      .waitFor({ timeout: 5_000 });
    console.log(`[${participant}] Waiting Room is now OFF`);
  } catch (e) {
    console.warn(
      `[${participant}] could not disable Waiting Room toggle (proceeding with join):`,
      (e as Error).message,
    );
  }
}

/** Selectors for the non-grid terminal screens rendered by
 * `dioxus-ui/src/pages/meeting.rs` and `components/waiting_room.rs`.
 * Centralized so unit tests and the join helper agree on the strings.
 */
export const MEETING_STATE_SELECTORS = {
  waitingRoom: '[data-testid="meeting-waiting-room"]',
  waitingForHost: '[data-testid="meeting-waiting-for-host"]',
  rejected: '[data-testid="meeting-rejected"]',
  error: '[data-testid="meeting-error"]',
} as const;

type RaceOutcome = "grid" | "waiting-room" | "waiting-for-host" | "rejected" | "error";

/**
 * Build a `Promise.race` that resolves with the first of the five
 * post-join terminal-or-success screens to become visible. Resolves
 * `null` if none appear before `timeout`.
 *
 * Note: each child `.waitFor` swallows its own timeout via `.catch`. The
 * outer race resolves with the first non-null value; if all four
 * children resolve to `null` the race itself resolves to `null`
 * (Promise.race + uniformly-resolving promises ⇒ first-to-resolve wins).
 */
async function raceJoinOutcome(args: {
  grid: Locator;
  waitingRoom: Locator;
  waitingForHost: Locator;
  rejected: Locator;
  errorScreen: Locator;
  timeout: number;
}): Promise<RaceOutcome | null> {
  const { grid, waitingRoom, waitingForHost, rejected, errorScreen, timeout } = args;
  return await Promise.race<RaceOutcome | null>([
    grid
      .waitFor({ timeout })
      .then(() => "grid" as const)
      .catch(() => null),
    waitingRoom
      .waitFor({ timeout })
      .then(() => "waiting-room" as const)
      .catch(() => null),
    waitingForHost
      .waitFor({ timeout })
      .then(() => "waiting-for-host" as const)
      .catch(() => null),
    rejected
      .waitFor({ timeout })
      .then(() => "rejected" as const)
      .catch(() => null),
    errorScreen
      .waitFor({ timeout })
      .then(() => "error" as const)
      .catch(() => null),
  ]);
}

/**
 * Read the visible error text from the `[data-testid="meeting-error"]`
 * screen so the bot's log carries the actual server-reported reason
 * (e.g. "The host has left and no one can admit new participants.").
 * Falls back to a generic message on read failure.
 */
async function readMeetingErrorText(page: Page): Promise<string> {
  try {
    const errorBlock = page.locator(MEETING_STATE_SELECTORS.error).first();
    const text = (await errorBlock.innerText({ timeout: 1_000 })).trim();
    return text.length > 0 ? text : "meeting page reported an unspecified error";
  } catch {
    return "meeting page reached the error screen (text could not be read)";
  }
}

/**
 * Translate a non-grid race outcome into the appropriate typed error.
 * Centralized so the pre-click + per-attempt paths produce identical
 * diagnostics.
 */
async function throwForOutcome(
  outcome: Exclude<RaceOutcome, "grid">,
  participant: string,
  page: Page,
): Promise<never> {
  switch (outcome) {
    case "waiting-room": {
      const msg = `parked in waiting room — not a bug, host must admit`;
      console.log(`[${participant}] ${msg}`);
      throw new WaitingRoomError("waiting-room", msg);
    }
    case "waiting-for-host": {
      const msg = `waiting for host to start the meeting — not a bug, host hasn't joined yet`;
      console.log(`[${participant}] ${msg}`);
      throw new WaitingRoomError("waiting-for-host", msg);
    }
    case "rejected": {
      const msg = "host denied the join request";
      console.log(`[${participant}] meeting rejected: ${msg}`);
      throw new JoinRejectedError("rejected", msg);
    }
    case "error": {
      const reportedText = await readMeetingErrorText(page);
      console.log(`[${participant}] meeting error: ${reportedText}`);
      throw new JoinRejectedError("error", reportedText);
    }
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
 *
 * Each per-attempt wait *also* races against the four non-grid
 * terminal screens (`meeting-waiting-room`, `meeting-waiting-for-host`,
 * `meeting-rejected`, `meeting-error`). Detecting any of those throws a
 * typed error (`WaitingRoomError` / `JoinRejectedError`) instead of
 * looping the join click — the API already accepted us, the UI is just
 * telling us we're parked or denied.
 */
async function joinWithRetries(args: {
  page: Page;
  participant: string;
  joinButton: Locator;
  grid: Locator;
  mode: JoinMode;
  isNavigatedAway: () => boolean;
}): Promise<void> {
  const { page, participant, joinButton, grid, mode, isNavigatedAway } = args;
  const maxAttempts = 3;
  const perAttemptGridTimeout = 45_000;

  const waitingRoom = page.locator(MEETING_STATE_SELECTORS.waitingRoom).first();
  const waitingForHost = page.locator(MEETING_STATE_SELECTORS.waitingForHost).first();
  const rejected = page.locator(MEETING_STATE_SELECTORS.rejected).first();
  const errorScreen = page.locator(MEETING_STATE_SELECTORS.error).first();

  // The Waiting Room toggle is verified once, BEFORE this loop (see
  // the caller in `joinMeetingAndEnableMedia`). On retries 2 + 3 the
  // bot is still in the Start Meeting flow, but the meeting page may
  // already have transitioned past the pre-join card — re-checking
  // the toggle would either no-op silently or hit the disabled
  // post-card state. Track the flag here so future refactors that
  // move verification inside the loop short-circuit cleanly and so
  // the log makes the skip explicit for "start" mode.
  const waitingRoomVerified = mode === "start";

  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    if (isNavigatedAway()) return; // surfaced by caller's throwIfNavigatedAway

    // Fast-path: if any of the five post-join screens is already up,
    // resolve immediately without burning the per-attempt budget.
    const earlyOutcome = await raceJoinOutcome({
      grid,
      waitingRoom,
      waitingForHost,
      rejected,
      errorScreen,
      timeout: 200,
    });
    if (earlyOutcome === "grid") return;
    if (earlyOutcome !== null) {
      await throwForOutcome(earlyOutcome, participant, page);
    }

    const sawEnabledButton = await joinButton.isVisible({ timeout: 5_000 }).catch(() => false);
    if (!sawEnabledButton) {
      // No enabled join button on screen. Could mean: (a) the click
      // already consumed the form and we're waiting for the grid, or
      // (b) only the disabled variant is showing. Log + fall through
      // to the multi-state race — if no screen shows, throw the
      // pre-existing "grid did not become visible" error so the
      // outer behaviour is preserved.
      if (attempt === 1) {
        console.log(`[${participant}] join button not enabled yet, waiting for grid`);
      } else {
        console.log(
          `[${participant}] retrying join click (attempt ${attempt}) — no enabled button visible`,
        );
      }
      const noClickOutcome = await raceJoinOutcome({
        grid,
        waitingRoom,
        waitingForHost,
        rejected,
        errorScreen,
        timeout: perAttemptGridTimeout,
      });
      if (noClickOutcome === "grid") return;
      if (noClickOutcome !== null) {
        await throwForOutcome(noClickOutcome, participant, page);
      }
      if (attempt === maxAttempts) throw new Error("grid did not become visible after join click");
      continue;
    }

    if (attempt === 1) {
      // Log the button label that's actually being clicked so the
      // operator can tell from the log which mode the bot saw. Falls
      // back to "Join Meeting" when the detection was inconclusive
      // (preserves the legacy log shape for the unknown-label path).
      const label =
        mode === "start" ? "Start Meeting" : mode === "join" ? "Join Meeting" : "Join Meeting";
      console.log(`[${participant}] clicking ${label}`);
    } else {
      if (mode === "start" && waitingRoomVerified) {
        console.log(`[${participant}] Waiting Room already verified — skipping on retry`);
      }
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

    // Race the grid + four terminal screens against a re-appearance of
    // the enabled Join button. Reappearance ⇒ click was a no-op or the
    // UI rolled back; the four terminal screens short-circuit with a
    // typed error.
    type ClickedOutcome = RaceOutcome | "button-reappeared";
    const outcome = (await Promise.race<ClickedOutcome | null>([
      grid
        .waitFor({ timeout: perAttemptGridTimeout })
        .then(() => "grid" as ClickedOutcome)
        .catch(() => null),
      waitingRoom
        .waitFor({ timeout: perAttemptGridTimeout })
        .then(() => "waiting-room" as ClickedOutcome)
        .catch(() => null),
      waitingForHost
        .waitFor({ timeout: perAttemptGridTimeout })
        .then(() => "waiting-for-host" as ClickedOutcome)
        .catch(() => null),
      rejected
        .waitFor({ timeout: perAttemptGridTimeout })
        .then(() => "rejected" as ClickedOutcome)
        .catch(() => null),
      errorScreen
        .waitFor({ timeout: perAttemptGridTimeout })
        .then(() => "error" as ClickedOutcome)
        .catch(() => null),
      joinButton
        .waitFor({ timeout: perAttemptGridTimeout, state: "visible" })
        .then(() => "button-reappeared" as ClickedOutcome)
        .catch(() => null),
    ])) as ClickedOutcome | null;

    if (outcome === "grid") {
      console.log(`[${participant}] join click consumed`);
      return;
    }
    if (outcome !== null && outcome !== "button-reappeared") {
      await throwForOutcome(outcome, participant, page);
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
    // outcome === null ⇒ none resolved within the per-attempt
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
