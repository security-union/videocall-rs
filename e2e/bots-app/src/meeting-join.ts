import {
  type Page,
  type Locator,
  type ConsoleMessage,
  type Request,
  type Response,
} from "@playwright/test";

import {
  ACTION_BAR_SELECTOR,
  CAMERA_TOOLTIP,
  MIC_UNMUTE_SELECTOR,
  cameraButtonSelector,
  peerListCandidates,
  resolveControlSelector,
} from "./control-buttons";
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
  const text = await joinButton.innerText({ timeout: 5_000 }).catch(() => "");
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
 * to the human peer. Both controls are matched by the product's own
 * `data-testid` + `aria-label`, drift-locked against `video_control_buttons.rs`.
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
  let resolveNavigatedAway!: () => void;
  const navigatedAwayPromise = new Promise<void>((resolve) => {
    resolveNavigatedAway = resolve;
  });
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
      resolveNavigatedAway();
    }
  };
  page.on("framenavigated", onFrameNavigated);

  try {
    await driveJoinStateMachine({
      page,
      participant,
      displayName,
      meetingId,
      homepageMeetingInput,
      meetingPageDisplayNameInput,
      joinButton,
      grid,
      isNavigatedAway: () => navigatedAway,
      navigatedAwayPromise,
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
    const controlsContainer = page.locator(ACTION_BAR_SELECTOR).first();
    await controlsContainer.hover({ timeout: 5_000 }).catch(() => {
      // Fine — some layouts may not need the hover.
    });
    await page.waitForTimeout(200);

    await clickWhenVisible(page, participant, "microphone", [MIC_UNMUTE_SELECTOR]);
    await clickWhenVisible(page, participant, "camera", [cameraButtonSelector(CAMERA_TOOLTIP.off)]);
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
    const current = await toggle.getAttribute("aria-checked", { timeout: 2_000 });
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
 * the duration of a single click + wait iteration in the join state machine.
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
 * Exported for the unit tests; in production this is called after a
 * timed-out attempt or a real hide-then-reappear button transition.
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
 * One bounded state machine drives every pre-grid state. Each attempt
 * gets the existing 45-second budget; a consumed click arms the button
 * branch only for a real hide-then-reappear transition.
 */
type JoinState =
  | RaceOutcome
  | "homepage-form"
  | "display-name-prompt"
  | "join-button"
  | "navigated-away";

const JOIN_ACTION_TIMEOUT_MS = 5_000;
const JOIN_ATTEMPTS = 3;
const JOIN_ATTEMPT_TIMEOUT_MS = 45_000;

// Exported for unit testing the #865 gate (see meeting-join.test.ts).
export async function waitForJoinButton(
  joinButton: Locator,
  homepageMeetingInput: Locator,
  displayNameInput: Locator,
  timeout: number,
  requireReappearance: boolean,
  blockWhileFormsPresent: boolean,
): Promise<void> {
  const startedAt = Date.now();
  if (blockWhileFormsPresent) {
    await homepageMeetingInput.waitFor({ state: "hidden", timeout });
    const afterHomepage = Math.max(1, timeout - (Date.now() - startedAt));
    await displayNameInput.waitFor({ state: "hidden", timeout: afterHomepage });
  }
  const remaining = Math.max(1, timeout - (Date.now() - startedAt));
  if (!requireReappearance) {
    await joinButton.waitFor({ state: "visible", timeout: remaining });
    return;
  }

  await joinButton.waitFor({ state: "hidden", timeout: remaining });
  const afterHidden = Math.max(1, timeout - (Date.now() - startedAt));
  await joinButton.waitFor({ state: "visible", timeout: afterHidden });
}

/**
 * Race every mutually-exclusive page state that can advance or finish
 * the join flow. The prompt arm is listed before the button arm because
 * the prompt's submit button also matches the enabled Join locator.
 */
async function raceJoinState(args: {
  homepageMeetingInput: Locator;
  displayNameInput: Locator;
  joinButton: Locator;
  grid: Locator;
  waitingRoom: Locator;
  waitingForHost: Locator;
  rejected: Locator;
  errorScreen: Locator;
  navigatedAwayPromise: Promise<void>;
  includePrompt: boolean;
  requireButtonReappearance: boolean;
  timeout: number;
}): Promise<JoinState | null> {
  const {
    homepageMeetingInput,
    displayNameInput,
    joinButton,
    grid,
    waitingRoom,
    waitingForHost,
    rejected,
    errorScreen,
    navigatedAwayPromise,
    includePrompt,
    requireButtonReappearance,
    timeout,
  } = args;
  const waits: Array<Promise<JoinState | null>> = [
    grid
      .waitFor({ state: "visible", timeout })
      .then(() => "grid" as const)
      .catch(() => null),
    waitingRoom
      .waitFor({ state: "visible", timeout })
      .then(() => "waiting-room" as const)
      .catch(() => null),
    waitingForHost
      .waitFor({ state: "visible", timeout })
      .then(() => "waiting-for-host" as const)
      .catch(() => null),
    rejected
      .waitFor({ state: "visible", timeout })
      .then(() => "rejected" as const)
      .catch(() => null),
    errorScreen
      .waitFor({ state: "visible", timeout })
      .then(() => "error" as const)
      .catch(() => null),
    homepageMeetingInput
      .waitFor({ state: "visible", timeout })
      .then(() => "homepage-form" as const)
      .catch(() => null),
    navigatedAwayPromise.then(() => "navigated-away" as const),
  ];
  if (includePrompt) {
    waits.push(
      displayNameInput
        .waitFor({ state: "visible", timeout })
        .then(() => "display-name-prompt" as const)
        .catch(() => null),
    );
  }
  waits.push(
    waitForJoinButton(
      joinButton,
      homepageMeetingInput,
      displayNameInput,
      timeout,
      requireButtonReappearance,
      includePrompt,
    )
      .then(() => "join-button" as const)
      .catch(() => null),
  );
  return await Promise.race(waits);
}

function isTransientJoinActionError(error: unknown): boolean {
  if (!(error instanceof Error)) return false;
  return (
    error.name === "TimeoutError" ||
    /timeout|timed out|detached|not attached|element is not attached/i.test(error.message)
  );
}

async function fillHomepageForm(args: {
  page: Page;
  meetingInput: Locator;
  meetingId: string;
  displayName: string;
}): Promise<void> {
  const { page, meetingInput, meetingId, displayName } = args;
  const usernameInput = page.locator("#username");
  await meetingInput.click({ timeout: JOIN_ACTION_TIMEOUT_MS });
  await meetingInput.pressSequentially(meetingId, {
    delay: 30,
    timeout: JOIN_ACTION_TIMEOUT_MS,
  });
  await usernameInput.click({ timeout: JOIN_ACTION_TIMEOUT_MS });
  await usernameInput.fill("", { timeout: JOIN_ACTION_TIMEOUT_MS });
  await usernameInput.pressSequentially(displayName, {
    delay: 30,
    timeout: JOIN_ACTION_TIMEOUT_MS,
  });
  await page.waitForTimeout(300);
  await usernameInput.press("Enter", { timeout: JOIN_ACTION_TIMEOUT_MS });
}

async function fillMeetingDisplayName(input: Locator, displayName: string): Promise<void> {
  await input.click({ timeout: JOIN_ACTION_TIMEOUT_MS });
  await input.fill("", { timeout: JOIN_ACTION_TIMEOUT_MS });
  await input.pressSequentially(displayName, {
    delay: 30,
    timeout: JOIN_ACTION_TIMEOUT_MS,
  });
}

async function driveJoinStateMachine(args: {
  page: Page;
  participant: string;
  displayName: string;
  meetingId: string;
  homepageMeetingInput: Locator;
  meetingPageDisplayNameInput: Locator;
  joinButton: Locator;
  grid: Locator;
  isNavigatedAway: () => boolean;
  navigatedAwayPromise: Promise<void>;
}): Promise<void> {
  const {
    page,
    participant,
    displayName,
    meetingId,
    homepageMeetingInput,
    meetingPageDisplayNameInput,
    joinButton,
    grid,
    isNavigatedAway,
    navigatedAwayPromise,
  } = args;
  const waitingRoom = page.locator(MEETING_STATE_SELECTORS.waitingRoom).first();
  const waitingForHost = page.locator(MEETING_STATE_SELECTORS.waitingForHost).first();
  const rejected = page.locator(MEETING_STATE_SELECTORS.rejected).first();
  const errorScreen = page.locator(MEETING_STATE_SELECTORS.error).first();
  let waitingRoomVerified = false;
  let lastFailure = "grid did not become visible";

  for (let attempt = 1; attempt <= JOIN_ATTEMPTS; attempt++) {
    const deadline = Date.now() + JOIN_ATTEMPT_TIMEOUT_MS;
    let includePrompt = true;
    let requireButtonReappearance = false;
    let diagnostics: ReturnType<typeof installClickDiagnostics> | null = null;
    let logDiagnostics = false;

    try {
      while (Date.now() < deadline) {
        throwIfNavigatedAway(isNavigatedAway(), participant);
        const state = await raceJoinState({
          homepageMeetingInput,
          displayNameInput: meetingPageDisplayNameInput,
          joinButton,
          grid,
          waitingRoom,
          waitingForHost,
          rejected,
          errorScreen,
          navigatedAwayPromise,
          includePrompt,
          requireButtonReappearance,
          timeout: Math.max(1, deadline - Date.now()),
        });

        if (state === "grid") return;
        if (state === "navigated-away") {
          throwIfNavigatedAway(true, participant);
        }
        if (
          state === "waiting-room" ||
          state === "waiting-for-host" ||
          state === "rejected" ||
          state === "error"
        ) {
          await throwForOutcome(state, participant, page);
        }
        if (state === null) {
          lastFailure = `no join state reached within ${JOIN_ATTEMPT_TIMEOUT_MS}ms`;
          logDiagnostics = diagnostics !== null;
          break;
        }

        if (state === "homepage-form") {
          console.log(`[${participant}] homepage form detected — filling`);
          try {
            await fillHomepageForm({
              page,
              meetingInput: homepageMeetingInput,
              meetingId,
              displayName,
            });
          } catch (error) {
            if (!isTransientJoinActionError(error)) throw error;
            console.warn(
              `[${participant}] homepage form advanced during input; re-checking join state`,
            );
          }
          continue;
        }

        if (state === "display-name-prompt") {
          console.log(`[${participant}] meeting-page display-name prompt detected — filling`);
          try {
            await fillMeetingDisplayName(meetingPageDisplayNameInput, displayName);
            includePrompt = false;
          } catch (error) {
            if (!isTransientJoinActionError(error)) throw error;
            console.warn(
              `[${participant}] display-name prompt advanced during input; re-checking join state`,
            );
          }
          continue;
        }

        if (requireButtonReappearance) {
          lastFailure = "join button reappeared after the click";
          logDiagnostics = diagnostics !== null;
          break;
        }

        const mode = await detectJoinMode(joinButton);
        if (mode === "start" && !waitingRoomVerified) {
          console.log(
            `[${participant}] detected mode: Start Meeting (bot is meeting owner — verifying Waiting Room is OFF before starting)`,
          );
          await ensureWaitingRoomOff(page, participant);
          waitingRoomVerified = true;
        } else if (mode === "join") {
          console.log(`[${participant}] detected mode: Join Meeting (joining existing meeting)`);
        } else if (mode === "unknown") {
          console.log(
            `[${participant}] detected mode: unknown — falling back to clicking the matched button as-is`,
          );
        }

        console.log(
          `[${participant}] ${attempt === 1 ? "clicking" : `attempt ${attempt}: clicking`} ${
            mode === "start" ? "Start Meeting" : "Join Meeting"
          }`,
        );
        diagnostics ??= installClickDiagnostics(page);
        try {
          await joinButton.click({ timeout: JOIN_ACTION_TIMEOUT_MS });
          requireButtonReappearance = true;
          includePrompt = false;
        } catch (error) {
          if (!isTransientJoinActionError(error)) throw error;
          console.warn(
            `[${participant}] join button advanced during click; re-checking join state`,
          );
        }
      }
    } finally {
      if (diagnostics !== null) {
        if (logDiagnostics) {
          logPostClickDiagnostics(participant, attempt, diagnostics.diag, page.url());
        }
        diagnostics.teardown();
      }
    }
  }

  throw new Error(
    `${lastFailure} after ${JOIN_ATTEMPTS} attempts of ${JOIN_ATTEMPT_TIMEOUT_MS}ms each`,
  );
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
 *   1. Toggle the peer-list panel via `peerListControlSelector`.
 *   2. Identify the self-row by the `(You)` / `(You/Host)` indicator
 *      (`is_self` in `peer_list_item.rs`) AND the edit pencil. The
 *      double-filter is defensive:
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
  // peer-list toggle is interactable.
  await page.mouse.move(400, 400).catch(() => {});
  await page.waitForTimeout(300);

  const openSelector = await resolveControlSelector(
    page,
    peerListCandidates("off"),
    "in-meeting rename: open peer list",
    (m) => console.warn(`[${participant}] ${m}`),
  );
  if (openSelector === null) {
    console.warn(`[${participant}] in-meeting rename: no visible peer-list toggle — skipping`);
    return;
  }

  try {
    await page.locator(openSelector).click({ timeout: 5_000 });
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
    await closePeerList(page, participant);
    return;
  }

  const rawText =
    (await selfRow
      .first()
      .textContent({ timeout: 5_000 })
      .catch(() => null)) ?? "";
  // Strip indicator suffixes the row template appends — "(You)",
  // "(Host)", "(You/Host)", and "Guest" — so the comparison is on the
  // raw display-name text only.
  const cleaned = rawText.replace(/\(You\/Host\)|\(You\)|\(Host\)|Guest/g, "").trim();

  if (cleaned === displayName) {
    console.log(
      `[${participant}] in-meeting rename: display name already "${displayName}" — skipping`,
    );
    await closePeerList(page, participant);
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
    await closePeerList(page, participant);
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
    await nameInput.fill("", { timeout: 5_000 });
    await nameInput.pressSequentially(displayName, { delay: 30, timeout: 5_000 });

    await saveBtn.waitFor({ state: "visible", timeout: 5_000 });
    await saveBtn.click({ timeout: 5_000 });

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

  await closePeerList(page, participant);
}

/**
 * Idempotent: the panel already being closed is the `isVisible` false path.
 * Post-condition is verified, so a click that lands off-target cannot leave the
 * panel covering the action-bar buttons the enable-media step needs.
 */
export async function closePeerList(page: Page, participant: string): Promise<void> {
  // Re-reveal the action bar in case it auto-hid while the rename
  // modal was open.
  await page.mouse.move(400, 400).catch(() => undefined);
  await page.waitForTimeout(150);

  const closeSelector = await resolveControlSelector(
    page,
    peerListCandidates("on"),
    "in-meeting rename: close peer list",
    (m) => console.warn(`[${participant}] ${m}`),
  );
  if (closeSelector === null) return;
  const closePeersBtn = page.locator(closeSelector);

  try {
    await closePeersBtn.click({ timeout: 5_000 });

    // The aria-label swaps with panel state, so the open-state locator going
    // hidden IS the confirmation that the panel closed.
    await closePeersBtn.waitFor({ state: "hidden", timeout: 5_000 });
  } catch (e) {
    console.warn(
      `[${participant}] in-meeting rename: peer-list close did not confirm ` +
        `(${(e as Error).message}). The subsequent enable-media step may have ` +
        `to fight a still-open panel — check the bot's mic / camera enable logs.`,
    );
  }
}

async function clickWhenVisible(
  page: Page,
  participant: string,
  label: string,
  selectors: readonly string[],
): Promise<void> {
  for (const sel of selectors) {
    const candidate: Locator = page.locator(sel);
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
    `[${participant}] could not find a visible ${label} enable button — selectors tried: ${selectors.join(" | ")}. The action bar may have autohidden, the device may be unavailable, or the aria-label changed.`,
  );
}
