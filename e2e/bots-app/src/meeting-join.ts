import {
  type Page,
  type Locator,
  type ConsoleMessage,
  type Request,
  type Response,
} from "@playwright/test";

import { isDevServerNoise } from "./dev-noise";

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

    // ── Step 2: ensure display name is set via the in-meeting rename ───
    //
    // The display-name prompt branch at the top of this function only
    // fires when the bot lands in `meeting-name-prompt` state — i.e.
    // when the bot is the first participant and the prompt actually
    // renders. When the meeting has already been started by another
    // participant (e.g. the operator pressed Start Meeting themselves
    // and then launched the bot to join), the bot lands on the
    // "Join Meeting" button directly and the prompt is never shown.
    // In that case the prompt-fill branch above never ran, and the
    // bot ends up in the grid with no display name set — visible to
    // every other peer as the user_id-derived default.
    //
    // Use the in-meeting attendee-list edit button to guarantee the
    // bot's display name matches `opts.displayName` regardless of how
    // it entered the meeting. Idempotent: reads the current self-row's
    // display name first and skips the rename if it already matches.
    // Tolerant of all failure modes — never blocks the launch.
    await ensureDisplayNameInMeeting({ page, participant, displayName });

    // ── Step 3: enable mic + camera so the prep'd fake devices flow ───

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
 * Hard cap on captured-event counters. Prevents a noisy SPA (or a
 * pathological infinite-error loop) from filling the bot's log with
 * hundreds of repeats of the same diagnostic when only the first few
 * carry signal. 20 is enough to see distinct root causes; anything
 * beyond is overwhelmingly likely to be the same error re-firing.
 */
const CLICK_DIAGNOSTICS_CAP = 20;

/**
 * Per-attempt diagnostic bag, filled by `installClickDiagnostics` for
 * the duration of a single click + wait iteration in `joinWithRetries`.
 * Emitted via `logPostClickDiagnostics` only when the attempt fails
 * (button reappeared) — successful joins stay quiet.
 */
export interface ClickAttemptDiagnostics {
  /** `Date.now()` at the moment the recorder was installed. */
  startedAt: number;
  /** `page.url()` just before the click was issued. */
  startUrl: string;
  /**
   * Up to `CLICK_DIAGNOSTICS_CAP` filtered `console.error` lines
   * observed since the click. Dioxus dev-server cosmetic noise is
   * filtered via `isDevServerNoise` so real server-side errors aren't
   * drowned out.
   */
  consoleErrors: string[];
  /**
   * Up to `CLICK_DIAGNOSTICS_CAP` failed network events: hard transport
   * failures (Playwright `requestfailed`) and HTTP responses with
   * status >= 400. Both surface the WHY behind a non-transitioning
   * click — the meeting-api 4xx case is the canonical example.
   */
  failedRequests: Array<{ url: string; status?: number; failure?: string }>;
}

/**
 * Install per-attempt event listeners on the Playwright Page that
 * capture the post-click failure signal: filtered `console.error` lines,
 * `requestfailed` events, and HTTP >= 400 responses.
 *
 * Caller MUST call the returned `teardown` exactly once (typically in a
 * `finally`) so the listeners don't leak across retry attempts.
 *
 * The dev-noise filter from `dev-noise.ts` is applied to console events
 * so trunk-serve cosmetic errors (PR #808) don't displace real
 * diagnostics from the 20-entry budget.
 *
 * Exported so the unit tests can drive it with a fake Page emitter.
 */
export function installClickDiagnostics(page: Page): {
  diag: ClickAttemptDiagnostics;
  teardown: () => void;
} {
  const diag: ClickAttemptDiagnostics = {
    startedAt: Date.now(),
    startUrl: page.url(),
    consoleErrors: [],
    failedRequests: [],
  };

  const onConsole = (msg: ConsoleMessage): void => {
    if (msg.type() !== "error") return;
    if (diag.consoleErrors.length >= CLICK_DIAGNOSTICS_CAP) return;
    const text = msg.text();
    if (isDevServerNoise(text, { pageUrl: page.url() })) return;
    diag.consoleErrors.push(text);
  };
  const onRequestFailed = (req: Request): void => {
    if (diag.failedRequests.length >= CLICK_DIAGNOSTICS_CAP) return;
    diag.failedRequests.push({
      url: req.url(),
      failure: req.failure()?.errorText,
    });
  };
  const onResponse = (resp: Response): void => {
    if (resp.status() < 400) return;
    if (diag.failedRequests.length >= CLICK_DIAGNOSTICS_CAP) return;
    diag.failedRequests.push({
      url: resp.url(),
      status: resp.status(),
    });
  };

  page.on("console", onConsole);
  page.on("requestfailed", onRequestFailed);
  page.on("response", onResponse);

  return {
    diag,
    teardown: () => {
      page.off("console", onConsole);
      page.off("requestfailed", onRequestFailed);
      page.off("response", onResponse);
    },
  };
}

/**
 * Emit a structured, one-line-per-piece-of-evidence diagnostic block to
 * the bot's log when a click attempt failed to transition to the grid.
 *
 * The shape is fixed (not free-form) so the dashboard's View Logs
 * dialog renders it cleanly without dominating the panel. The lines
 * are intentionally prefixed with `[participant]` so log demuxing in
 * the orchestrator pipeline keeps them attributed to the right bot.
 *
 * Fires the meeting-api hint when a `/api/v1/meetings/<id>/join` URL
 * with status >= 400 is captured — that's the canonical "server
 * rejected the join" pattern operators need to pivot away from "the
 * bot is broken" debugging.
 *
 * Exported for the unit tests; in production this is called from
 * `joinWithRetries` after a `button-reappeared` outcome.
 */
export function logPostClickDiagnostics(
  participant: string,
  attempt: number,
  diag: ClickAttemptDiagnostics,
  currentUrl: string,
): void {
  const elapsedMs = Date.now() - diag.startedAt;
  const urlChanged = currentUrl !== diag.startUrl;

  console.log(
    `[${participant}] attempt ${attempt} diagnostics: ${elapsedMs}ms elapsed since click; url ${urlChanged ? `CHANGED to ${currentUrl}` : `unchanged (${currentUrl})`}`,
  );
  if (diag.consoleErrors.length > 0) {
    console.log(`[${participant}]   captured ${diag.consoleErrors.length} console.error(s):`);
    diag.consoleErrors.forEach((err, i) => {
      console.log(`[${participant}]     [${i + 1}] ${err}`);
    });
  } else {
    console.log(`[${participant}]   captured 0 console.error(s)`);
  }
  if (diag.failedRequests.length > 0) {
    console.log(`[${participant}]   captured ${diag.failedRequests.length} failed request(s):`);
    diag.failedRequests.forEach((req, i) => {
      const detail =
        req.status !== undefined ? `HTTP ${req.status}` : (req.failure ?? "unknown failure");
      console.log(`[${participant}]     [${i + 1}] ${detail}  ${req.url}`);
    });
  } else {
    console.log(`[${participant}]   captured 0 failed request(s)`);
  }

  // Server-side hint: if the meeting-api itself rejected the join,
  // surface it explicitly so operators know to look at the meeting-api
  // logs instead of treating the bot as broken. The URL pattern is
  // intentionally narrow — only `/api/v1/meetings/.../join` qualifies.
  const meetingApiFailure = diag.failedRequests.find(
    (r) => r.url.includes("/api/v1/meetings/") && r.url.includes("/join") && (r.status ?? 0) >= 400,
  );
  if (meetingApiFailure !== undefined) {
    console.log(
      `[${participant}]   meeting-api join request failed with HTTP ${meetingApiFailure.status} — this is why the page didn't transition. Check the meeting-api server-side logs for the matching request.`,
    );
  }
}

/**
 * Phase-A race outcome: either the click was *not* consumed (button
 * stayed visible the full timeout — caller retries) or it was consumed
 * (`button-hidden`), in which case the caller runs Phase B. The four
 * non-grid terminal screens + `grid` short-circuit the loop entirely.
 */
type PhaseAOutcome = RaceOutcome | "button-hidden";

/**
 * Phase-B race outcome: the click WAS consumed in Phase A; we now wait
 * for one of the five post-join screens or for the button to reappear
 * (genuine retry signal — UI rolled back the click).
 */
type PhaseBOutcome = RaceOutcome | "button-reappeared";

/**
 * Race Phase A: wait for any post-join terminal/success screen, OR for
 * the click to be visibly consumed by the page (the join button goes
 * from visible to hidden / detached). Resolves to `null` if NONE of the
 * conditions resolve within `timeout` — that's the "click did not
 * transition the page" signal which used to be silently mis-detected as
 * "button-reappeared" before this fix.
 *
 * IMPORTANT: this races `joinButton.waitFor({state: "hidden"})`, not
 * `state: "visible"`. The previous implementation raced
 * `state: "visible"` against a locator that was *already* visible (the
 * just-clicked button); Playwright resolves "already in target state"
 * immediately, which collapsed the entire 45s budget to ~80ms.
 *
 * Exported so the unit tests can drive it with mocked locators.
 */
export async function racePhaseAClickConsumed(args: {
  joinButton: Locator;
  grid: Locator;
  waitingRoom: Locator;
  waitingForHost: Locator;
  rejected: Locator;
  errorScreen: Locator;
  timeout: number;
}): Promise<PhaseAOutcome | null> {
  const { joinButton, grid, waitingRoom, waitingForHost, rejected, errorScreen, timeout } = args;
  return await Promise.race<PhaseAOutcome | null>([
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
    // CORRECT: wait for the button to go HIDDEN / detached, not for it
    // to "still be visible". The latter form (`state: "visible"`) was
    // the bug we're fixing — Playwright treats an already-visible
    // locator as success and resolves immediately, defeating the whole
    // race.
    joinButton
      .waitFor({ timeout, state: "hidden" })
      .then(() => "button-hidden" as const)
      .catch(() => null),
  ]);
}

/**
 * Race Phase B: the click was already consumed (Phase A saw the button
 * go hidden). We now wait for one of the five post-join screens OR for
 * the button to *reappear* — the UI rolled back the click (e.g. a
 * transient connection error flipped the pre-join card back on).
 *
 * Resolves to `null` if nothing resolves within `timeout` — "click was
 * consumed but no grid/waiting/error state followed", which is the
 * unusual-but-distinct failure mode caller surfaces with a dedicated
 * error message.
 *
 * Exported so the unit tests can drive it with mocked locators.
 */
export async function racePhaseBPostClick(args: {
  joinButton: Locator;
  grid: Locator;
  waitingRoom: Locator;
  waitingForHost: Locator;
  rejected: Locator;
  errorScreen: Locator;
  timeout: number;
}): Promise<PhaseBOutcome | null> {
  const { joinButton, grid, waitingRoom, waitingForHost, rejected, errorScreen, timeout } = args;
  return await Promise.race<PhaseBOutcome | null>([
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
    joinButton
      .waitFor({ timeout, state: "visible" })
      .then(() => "button-reappeared" as const)
      .catch(() => null),
  ]);
}

/**
 * Click the (enabled) Join button up to `maxAttempts` times. Per
 * attempt, run a two-phase wait so the helper can correctly distinguish
 * "click did nothing" from "click went through but the UI rolled back".
 *
 * **Phase A (click-consumption check).** After the click, race for:
 *   - one of the five post-join terminal/success screens
 *     (`grid`, `waiting-room`, `waiting-for-host`, `rejected`, `error`),
 *     OR
 *   - the join button going hidden / detached.
 * Times out at `perAttemptGridTimeout` (45s). If Phase A times out with
 * the button still visible, the click was a no-op — we retry.
 *
 * **Phase B (button-reappearance check).** Only runs when Phase A
 * resolved with `button-hidden` (the click DID transition the page).
 * Race for the same five post-join screens against a re-appearance of
 * the (enabled) join button. Reappearance ⇒ the UI rolled back the
 * click; we retry. Any of the four non-grid terminal screens short-
 * circuits with a typed error.
 *
 * The grid wait per-attempt is 45s — the netsim'd `lossy_mobile`
 * profile regularly takes 20-35s to bring the WebTransport stream up.
 *
 * On retry-trigger outcomes (`button-reappeared` and the
 * `click-not-consumed` Phase-A timeout) the per-attempt diagnostic
 * recorder (`installClickDiagnostics` / `logPostClickDiagnostics`) emits
 * a structured block of captured console errors, failed requests, and
 * the URL diff so operators can see WHY the click didn't transition
 * (commonly: a meeting-api 4xx response). Diagnostics fire ONLY on
 * retry triggers; successful joins stay quiet.
 *
 * Background: the v1.7.2 implementation raced
 * `joinButton.waitFor({state: "visible"})` against a locator that was
 * already visible (the just-clicked button). Playwright resolves
 * "already in target state" immediately, so the race collapsed to
 * ~80ms per attempt and the helper triple-retried in under 250ms total.
 * The two-phase wait above fixes that by checking for the button going
 * HIDDEN as the click-consumption signal, then separately watching for
 * a hide→reappear cycle as the genuine retry signal.
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
    // Install the per-attempt diagnostic recorder BEFORE the click so
    // any `console.error` / `requestfailed` / >=400 response between
    // here and the race outcome is captured. Teardown in `finally` so
    // listeners never leak across attempts.
    const { diag, teardown } = installClickDiagnostics(page);
    try {
      // Playwright's auto-waiting + actionability checks effectively
      // cover "stable" — the locator already has `:not([disabled])` and
      // Playwright waits for the element to be stable before clicking.
      try {
        await joinButton.click({ timeout: 5_000 });
      } catch (e) {
        console.warn(`[${participant}] join-click warning:`, (e as Error).message);
      }

      // ── Phase A: click-consumption check ────────────────────────────
      // Race the five post-join screens against the button going
      // HIDDEN. If Phase A times out with the button still visible the
      // click was a no-op — diagnostics + retry.
      const phaseA = await racePhaseAClickConsumed({
        joinButton,
        grid,
        waitingRoom,
        waitingForHost,
        rejected,
        errorScreen,
        timeout: perAttemptGridTimeout,
      });

      if (phaseA === null) {
        // 45s elapsed, button still visible, no terminal state
        // reached. Click did nothing. This is the bug-fix path: the
        // v1.7.2 implementation would have resolved in ~80ms here via
        // the broken `state: "visible"` race and triple-retried in
        // under 250ms total. Now we genuinely waited the full budget.
        logPostClickDiagnostics(participant, attempt, diag, page.url());
        if (attempt === maxAttempts) {
          throw new Error(
            `click did not transition the page after ${maxAttempts} attempts of ${perAttemptGridTimeout}ms each — see captured diagnostics`,
          );
        }
        continue;
      }
      if (phaseA === "grid") {
        console.log(`[${participant}] join click consumed`);
        return;
      }
      if (phaseA !== "button-hidden") {
        // Non-grid terminal outcome (waiting-room / waiting-for-host /
        // rejected / error) — short-circuit with a typed error.
        await throwForOutcome(phaseA, participant, page);
      }

      // ── Phase B: button-reappearance check ──────────────────────────
      // Phase A saw the button go hidden. The click WAS consumed by the
      // UI. Now race the five post-join screens against a re-appearance
      // of the (enabled) join button. Reappearance ⇒ the UI rolled
      // back — genuine retry signal.
      const phaseB = await racePhaseBPostClick({
        joinButton,
        grid,
        waitingRoom,
        waitingForHost,
        rejected,
        errorScreen,
        timeout: perAttemptGridTimeout,
      });

      if (phaseB === null) {
        // Click was consumed but the page settled in a state that
        // isn't any of the five screens we expect. Unusual; surface
        // diagnostics + a dedicated error message so the operator
        // can tell this apart from the "click did nothing" path.
        logPostClickDiagnostics(participant, attempt, diag, page.url());
        if (attempt === maxAttempts) {
          throw new Error(
            `click consumed but no grid/waiting/error state reached within ${perAttemptGridTimeout}ms — see diagnostics`,
          );
        }
        continue;
      }
      if (phaseB === "grid") {
        console.log(`[${participant}] join click consumed`);
        return;
      }
      if (phaseB === "button-reappeared") {
        // The actual scenario the v1.7.2 "button-reappeared" branch
        // was trying to detect: a real hide-then-reappear cycle. Now
        // it only fires when there's an actual transition + rollback.
        logPostClickDiagnostics(participant, attempt, diag, page.url());
        if (attempt === maxAttempts) {
          throw new Error(
            `join button reappeared after ${maxAttempts} click attempts — grid never became visible (see logs for captured console + network errors)`,
          );
        }
        // Loop and retry. The retry log line fires at the top of the
        // next iteration so the attempt number is consistent.
        continue;
      }
      // Non-grid terminal outcome.
      await throwForOutcome(phaseB, participant, page);
    } finally {
      teardown();
    }
  }
}

/**
 * Regex matching characters the dioxus UI's `validate_display_name`
 * accepts (defined in `videocall-types/src/validation.rs`): ASCII
 * letters, numbers, spaces, underscores, hyphens, and apostrophes.
 * Anything else is rejected at form submission with an inline error,
 * leaving the rename modal stuck open. We pre-check the displayName
 * here so the bot doesn't even try (and doesn't leave the modal in a
 * stranded state).
 *
 * Exported for the unit-test that pins the contract.
 */
export const ALLOWED_DISPLAY_NAME_CHARS_RE = /^[a-zA-Z0-9 _'-]+$/;

/**
 * Set the bot's display name via the in-meeting attendee-list edit
 * button. Used as a fallback for the case where the bot landed on
 * "Join Meeting" without the display-name prompt rendering — e.g.
 * the operator started the meeting themselves and the bot joined as a
 * guest. In that case the prompt-fill branch in
 * {@link joinMeetingAndEnableMedia} never fired and the bot has no
 * display name set.
 *
 * Idempotent: reads the current self-row's display name first and
 * skips the rename if it already matches `displayName`. Tolerant of
 * every failure mode — logs a warning and returns; never throws.
 *
 * UI surface used (matches the rename flow exercised by
 * `same-user-multi-session.spec.ts`):
 *   1. Toggle the peer-list panel via the action-bar button whose
 *      tooltip reads "Open Peers" (sourced from
 *      `dioxus-ui/src/components/peer_list_button.rs`).
 *   2. Identify the self-row by the `(You)` / `(You/Host)` text
 *      indicator (sourced from `peer_list_item.rs:64-69` —
 *      `is_self == true` renders one of those labels) AND the
 *      presence of the edit pencil. The double-filter is defensive:
 *      when multiple sessions of the same authenticated user are in
 *      the meeting, every row carries the same `name` text but only
 *      one row (the local self-row) has `is_self == true`. Filtering
 *      by `(You)` text gives a stable, name-independent identifier
 *      for "this is MY row" regardless of how many siblings share
 *      the user_id.
 *   3. Fill the modal input (`input.input-apple`) with the desired
 *      name and click the "Save" button.
 *   4. Verify the modal actually closed — `validate_display_name` in
 *      `videocall-types/src/validation.rs` rejects any character
 *      outside `[a-zA-Z0-9 _'-]` and leaves the modal OPEN with an
 *      inline error message. We catch that case and close the modal
 *      via Escape (the Cancel-equivalent) so the bot's next step
 *      doesn't fight a stuck modal.
 *   5. Toggle the peer-list panel closed so the bot starts the
 *      enable-media step from a known state.
 */
export async function ensureDisplayNameInMeeting(args: {
  page: Page;
  participant: string;
  displayName: string;
}): Promise<void> {
  const { page, participant, displayName } = args;

  if (displayName.trim() === "") {
    console.log(`[${participant}] in-meeting rename: skipped (no displayName supplied)`);
    return;
  }

  // Pre-validate: if displayName contains characters the dioxus UI
  // will reject, skip the rename entirely instead of stranding the
  // modal open. The most common cause of this is a `{participant}`
  // template that wasn't substituted on the server side — typically
  // means an older bots-app version that doesn't apply template
  // substitution to the single-bot launch path.
  if (!ALLOWED_DISPLAY_NAME_CHARS_RE.test(displayName)) {
    console.warn(
      `[${participant}] in-meeting rename: skipped — displayName "${displayName}" ` +
        `contains characters the meeting UI rejects (allowed: ASCII letters, ` +
        `numbers, spaces, '_', '-', apostrophe). If you typed a "{participant}" ` +
        `template, make sure the server-side substitution applied.`,
    );
    return;
  }

  // The action bar auto-hides; nudge the mouse to reveal it so the
  // "Open Peers" toggle is interactable.
  await page.mouse.move(400, 400).catch(() => {});
  await page.waitForTimeout(300);

  const openPeers = page.locator("button.video-control-button", {
    has: page.locator(".tooltip", { hasText: "Open Peers" }),
  });

  try {
    await openPeers.click({ timeout: 5_000 });
  } catch (e) {
    console.warn(
      `[${participant}] in-meeting rename: could not open peer list (${(e as Error).message}) — skipping`,
    );
    return;
  }

  // Identify the self-row by the `(You)` / `(You/Host)` indicator AND
  // the presence of the edit pencil. The text-marker filter is the
  // robust signal — when multiple same-auth sessions are in the room
  // their `peer_item_name_container`s carry the same display-name
  // text but only the local self-row has the `.peer-indicator` text
  // matching one of the You-variants. The edit-pencil filter is the
  // belt-and-suspenders check (also self-only per
  // `peer_list_item.rs:87`).
  const selfRow = page
    .locator("#peer-list-container li")
    .filter({ has: page.locator("button.peer_item_edit_btn") })
    .filter({
      has: page.locator(".peer-indicator", { hasText: /\(You(?:\/Host)?\)/ }),
    });

  try {
    await selfRow.waitFor({ state: "visible", timeout: 5_000 });
  } catch {
    console.warn(`[${participant}] in-meeting rename: self-row not visible — skipping`);
    await openPeers.click().catch(() => undefined);
    return;
  }

  const rawText =
    (await selfRow
      .first()
      .textContent()
      .catch(() => null)) ?? "";
  // Strip indicator suffixes the row template appends — "(You)",
  // "(Host)", "(You/Host)", and "Guest" — so the comparison is on the
  // raw display-name text only.
  const cleaned = rawText.replace(/\(You\/Host\)|\(You\)|\(Host\)|Guest/g, "").trim();

  if (cleaned === displayName) {
    console.log(
      `[${participant}] in-meeting rename: display name already "${displayName}" — skipping`,
    );
    await openPeers.click().catch(() => undefined);
    return;
  }

  console.log(`[${participant}] in-meeting rename: "${cleaned}" → "${displayName}"`);

  // Click the edit pencil INSIDE the identified self-row (not the
  // page-wide locator) so a hypothetical future render that places a
  // pencil on more than one row can't misfire here.
  const editBtn = selfRow.locator("button.peer_item_edit_btn").first();
  try {
    await editBtn.click({ timeout: 5_000 });
  } catch (e) {
    console.warn(
      `[${participant}] in-meeting rename: edit-pencil click failed (${(e as Error).message})`,
    );
    await openPeers.click().catch(() => undefined);
    return;
  }

  // Scope the input + Save selectors to the rename modal's backdrop
  // (`.glass-backdrop` per `update_display_name_modal.rs:36`) so we
  // can't accidentally match a different modal that also uses
  // `.input-apple` / a "Save" button somewhere on the page.
  const modal = page.locator(".glass-backdrop").last();
  const nameInput = modal.locator("input.input-apple");
  const saveBtn = modal.getByRole("button", { name: "Save" });

  try {
    await nameInput.waitFor({ state: "visible", timeout: 5_000 });
    await nameInput.fill("");
    await nameInput.pressSequentially(displayName, { delay: 30 });

    await saveBtn.waitFor({ state: "visible", timeout: 5_000 });
    await saveBtn.click();

    // Verify the modal closed — the onsubmit handler in
    // `update_display_name_modal.rs:86-125` keeps the modal open and
    // renders an inline error when `validate_display_name` rejects
    // the input. If we reach the post-click state with the modal
    // still visible, the rename did NOT succeed; close the modal via
    // Escape so the bot doesn't fight it on the next step.
    try {
      await nameInput.waitFor({ state: "hidden", timeout: 5_000 });
      console.log(`[${participant}] in-meeting rename submitted (modal closed)`);
    } catch {
      console.warn(
        `[${participant}] in-meeting rename: modal still open after Save click — ` +
          `validation likely rejected "${displayName}". Closing modal via Escape.`,
      );
      await page.keyboard.press("Escape").catch(() => undefined);
    }
  } catch (e) {
    console.warn(
      `[${participant}] in-meeting rename: modal interaction failed (${(e as Error).message})`,
    );
    // Best-effort: try to close any stuck modal before continuing.
    await page.keyboard.press("Escape").catch(() => undefined);
  }

  // Close the peer-list panel so the enable-media step starts from a
  // known state.
  await openPeers.click().catch(() => undefined);
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
