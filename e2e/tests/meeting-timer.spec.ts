/**
 * Host meeting timer — issue 2136.
 *
 * The issue's promise is a countdown the HOST sets and EVERY participant sees,
 * ending in a sound the whole room hears. Everything load-bearing about that
 * promise is cross-peer, so every test here runs two real browsers:
 *
 *   1. start / extend / cancel, seen by a second participant, plus the
 *      HOST-ONLY gate on the control itself
 *   2. late-join convergence on the ~5s heartbeat — a participant who arrives
 *      AFTER the timer started still sees it
 *   3. urgency escalation to expired, and the expiry cue, on the NON-HOST
 *
 * WHY NO SOLO TESTS. The host renders its own timer from an optimistic local
 * echo (`apply_meeting_timer_transition` — the relay self-skips the sender, so
 * the host never receives its own packet back). A solo test therefore exercises
 * the echo and nothing else: not `send_meeting_timer`, not the relay's
 * `ForwardHostOnly` gate, not `on_meeting_timer`, not `MeetingTimerState::
 * from_packet`. The wire is the whole feature, so a second browser is not
 * optional here.
 *
 * TAGGING: deliberately UNTAGGED (no `@bvt0` / `@bvt1`), matching
 * `raise-hand.spec.ts` and the two-browser reaction specs in
 * `two-users-meeting.spec.ts`. bvt is the fast per-PR smoke set and already pays
 * for exactly one two-browser meeting boot; these add three more, one of which
 * additionally spends ~60s watching a real countdown reach zero. These therefore
 * run ONLY under `--project=dioxus`:
 *
 *     make e2e SPEC=meeting-timer
 *
 * and must be validated that way (local docker stack) or via a `/run-e2e`
 * dioxus dispatch on the PR — a per-PR "Playwright bvt1" green does NOT cover
 * them.
 *
 * HARNESS: `enterTwoUserMeeting` / `enterMeetingAsHost` / `guestJoinsMeeting`
 * come from `helpers/two-user-meeting.ts` — the same join dance the `@bvt1`
 * "host starts meeting, guest joins, both see each other" test proves. Nothing
 * about the join flow is re-invented here.
 *
 * CAMERA: intentionally NOT seeded camera-on (`vc_prejoin_camera_on`). Every
 * surface asserted here — the action-bar control, the popover, the fixed-
 * position countdown chip, the live region — renders over the camera-off
 * placeholder tile exactly as it does over video, and the harness already waits
 * on `.canvas-container` for peer connectivity.
 *
 * NO CONFIG FLAG: the feature ships unconditionally (there is no `config.js`
 * entry for it), so nothing here has to intercept `/config.js` or the
 * `/config.local.js` that would clobber such an override on a local serve.
 */

import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import {
  enterMeetingAsHost,
  enterTwoUserMeeting,
  guestJoinsMeeting,
} from "../helpers/two-user-meeting";

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

/** The host's action-bar trigger, by its stable DOM id. */
const TRIGGER = "#meeting-timer-trigger";
/**
 * The same button by test id, used for the ABSENCE assertions. Deliberately
 * broader than `TRIGGER`: the customize-mode drag preview renders a second
 * `MeetingTimerButton` with no id at all, so only the test id catches the
 * control appearing anywhere for a participant who must not have it.
 */
const TRIGGER_ANY = '[data-testid="meeting-timer-button"]';
/** The action-bar slot wrapper, keyed by the persisted layout slug. */
const SLOT = '.action-bar-slot-wrapper[data-slot="meeting_timer"]';
const POPOVER = '[data-testid="meeting-timer-popover"]';
const EXTEND = '[data-testid="meeting-timer-extend"]';
const CANCEL = '[data-testid="meeting-timer-cancel"]';
const CHIP = '[data-testid="meeting-timer-chip"]';
const CHIP_VALUE = '[data-testid="meeting-timer-chip"] .meeting-timer-chip-value';
const LIVE_REGION = '[data-testid="meeting-timer-live-region"]';

/** Preset button for a duration, in ms — mirrors `MEETING_TIMER_PRESETS_MS`. */
const preset = (ms: number) => `[data-testid="meeting-timer-preset-${ms}"]`;

// ---------------------------------------------------------------------------
// Constants mirrored from the production source
// ---------------------------------------------------------------------------

/** `MEETING_TIMER_PRESETS_MS[1]` — the 5-minute preset. */
const FIVE_MIN_MS = 300_000;
/** `MEETING_TIMER_PRESETS_MS[2]` — the 10-minute preset. */
const TEN_MIN_MS = 600_000;
/** `MEETING_TIMER_PRESETS_MS[3]` — the 15-minute preset. */
const FIFTEEN_MIN_MS = 900_000;
/**
 * `MEETING_TIMER_PRESETS_MS[0]` — the 1-minute preset. The ONLY preset whose
 * whole escalation fits inside a test.
 *
 * Its thresholds are PROPORTIONAL, not the flat ones: `urgency()` takes
 * `flat.min(proportion)`, so a 60s timer warns at 30s (`duration/2`, under the
 * flat 120s) and goes critical at 15s (`duration/4`, under the flat 30s). Full
 * ramp: Normal 60->30, Warning 30->15, Critical 15->0.
 *
 * Before that fix the flat 120s warning floor was satisfied at t=0 for a 60s
 * timer, so this preset STARTED amber and Normal was unreachable on it — which
 * is why a Normal->Warning transition could not be tested cheaply at all.
 */
const ONE_MIN_MS = 60_000;
/** `MEETING_TIMER_EXTEND_STEP_MS` — what one press of "Add 1 minute" adds. */
const EXTEND_STEP_MS = 60_000;

/**
 * Cross-peer settle window.
 *
 * A host transition is debounced 500ms (`MEETING_TIMER_DEBOUNCE_MS`), then the
 * send pump polls at 250ms (`MEETING_TIMER_PUMP_MS`), then the burst is spread
 * ~1s apart (`MEETING_TIMER_REPEAT_SPACING_MS`) — so ~1s before the first packet
 * is even on the wire, before relay fan-out. 20s comfortably clears that plus a
 * missed first packet repaired by the second burst repeat.
 */
const CROSS_PEER_TIMEOUT = 20_000;

/**
 * Convergence window for a LATE JOINER, whose only delivery mechanism is the
 * heartbeat: `MEETING_TIMER_HEARTBEAT_MS` is 5000, so one interval plus fan-out
 * plus a lost-packet retry fits inside 20s with room to spare.
 */
const HEARTBEAT_TIMEOUT = 20_000;

/**
 * Wall-clock room for one urgency step of the 1-minute timer. Normal->Warning is
 * 30 s of real time and Warning->Critical is 15 s; 60 s absorbs a slow tick and
 * a coalesced background frame without ever masking a step that never happens.
 */
const URGENCY_STEP_TIMEOUT = 60_000;

// ---------------------------------------------------------------------------
// Init scripts
// ---------------------------------------------------------------------------

/**
 * Record every `AudioParam.setValueAtTime` value the page sets, so the expiry
 * cue can be observed.
 *
 * WHAT THIS PROVES AND WHAT IT DOES NOT. No browser test can assert that sound
 * left a speaker; what it CAN assert is that the app built the audio graph for
 * the cue. `play_timer_expired_sound` sets a frequency of exactly 880 Hz on each
 * of THREE oscillators (`FREQ`/`TONES` in `meeting_timer.rs`), so counting 880s
 * is an unambiguous fingerprint of that function having run to completion.
 *
 * 880 is unique to it. The only other synthesized cues in the meeting are the
 * join and leave tone pairs (`play_tone_pair(523.25, 659.25, ...)` and
 * `(659.25, 440.0, ...)` in `attendants.rs`), and the gain values this also
 * records are 0.35 / 0.25 — nothing else in the app touches 880.
 *
 * Patching the PROTOTYPE rather than the constructor means the recording is
 * transparent: the real `AudioContext` still runs, so nothing about the app's
 * behaviour changes, and the spy cannot itself be the reason a later assertion
 * passes.
 */
const AUDIO_TONE_SPY = `
  (() => {
    window.__vcTimerTones = [];
    const proto = window.AudioParam && window.AudioParam.prototype;
    if (!proto || typeof proto.setValueAtTime !== 'function') return;
    const original = proto.setValueAtTime;
    proto.setValueAtTime = function (value, when) {
      try { window.__vcTimerTones.push(value); } catch (_) { /* never break audio */ }
      return original.call(this, value, when);
    };
  })();
`;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Wake the auto-hiding video-controls bar so a subsequent visibility PROBE
 * (`isVisible()`, which takes a snapshot and does not auto-wait) reads the real
 * layout rather than a bar mid-hide. Copied verbatim from `raise-hand.spec.ts`,
 * which took it from `drawer-resize.spec.ts` — including its reason for deriving
 * the centre point from the measured viewport rather than a fixed (400, 400).
 */
async function wakeControls(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  const vp = page.viewportSize() ?? { width: 800, height: 600 };
  await page.mouse.move(Math.floor(vp.width / 2), Math.floor(vp.height / 2));
  await page.waitForTimeout(300);
}

/** Launch a browser + authenticated context + page in one step. */
async function newParticipant(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  email: string,
  name: string,
  uiURL: string,
  initScripts: string[] = [],
): Promise<Page> {
  const ctx = await createAuthenticatedContext(browser, email, name, uiURL);
  for (const script of initScripts) {
    await ctx.addInitScript(script);
  }
  return ctx.newPage();
}

/**
 * Open the host's meeting-timer popover, idempotently.
 *
 * Idempotent because the trigger is a TOGGLE: clicking it while the popover is
 * already open would close it. `data-open` is the button's own mirror of the
 * `meeting_timer_open` signal, so it is the local truth rather than a proxy for
 * it, and reading it before clicking makes a call that finds the popover already
 * open a no-op instead of a silent close.
 *
 * Asserting `aria-expanded` here rather than in every caller keeps the
 * disclosure contract checked on every open: this control uses `aria-expanded`
 * (it discloses a dialog) and must never drift to `aria-pressed`, which would
 * claim the TIMER is toggled on — a different fact, and the #2123 shape where a
 * state attribute announces the inverse of reality.
 */
async function openTimerPopover(page: Page): Promise<void> {
  const trigger = page.locator(TRIGGER);
  await wakeControls(page);
  await expect(trigger).toBeVisible({ timeout: 15_000 });

  if ((await trigger.getAttribute("data-open")) !== "true") {
    await trigger.click();
  }

  await expect(page.locator(POPOVER)).toBeVisible({ timeout: 10_000 });
  await expect(trigger).toHaveAttribute("aria-expanded", "true");
  await expect(trigger).toHaveAttribute("aria-haspopup", "dialog");
}

/**
 * Current `data-remaining-ms` on the countdown chip, as a number.
 *
 * The attribute is written by `MeetingTimerChip` from the same `remaining_ms`
 * signal that renders the visible value, so it is the rendered truth and not a
 * separate computation that could agree by accident.
 */
async function remainingMs(page: Page, who: string): Promise<number> {
  const raw = await page.locator(CHIP).getAttribute("data-remaining-ms");
  expect(raw, `${who}: chip must carry data-remaining-ms`).not.toBeNull();
  const value = Number(raw);
  expect(Number.isFinite(value), `${who}: data-remaining-ms must be numeric`).toBe(true);
  return value;
}

/** How many 880 Hz tones the page has synthesized (see `AUDIO_TONE_SPY`). */
async function expiryTonesPlayed(page: Page): Promise<number> {
  return page.evaluate(() => {
    const tones = (window as Window & { __vcTimerTones?: number[] }).__vcTimerTones ?? [];
    return tones.filter((v) => v === 880).length;
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Meeting timer (issue 2136)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * THE ISSUE'S REQUIREMENT, plus the gate that decides who may drive it.
   *
   * The host starts a countdown; a SECOND participant — a different browser, a
   * different session — must see the same countdown, must see it change when the
   * host extends it, and must lose it when the host cancels. The same test pins
   * that the second participant never gets the CONTROL, which is the
   * security-visible half: the relay authorizes MEETING_TIMER against its own
   * live host mirror and silently drops a non-host's packets, so a control
   * rendered for them would look live and do nothing.
   *
   * Everything is in one boot on purpose — a two-browser meeting is the
   * expensive part, and start / extend / cancel are one continuous lifecycle
   * that reads better asserted in order than split across three of them.
   *
   * What a regression here would look like: `send_meeting_timer` not reaching
   * the wire, the relay's `PacketKind::MeetingTimer` arm dropping instead of
   * forwarding, `on_meeting_timer` never registered on a non-host client
   * (it is registered UNCONDITIONALLY for exactly this reason),
   * `would_apply_change` rejecting the first state, `extend_state` failing to
   * measure the extension from the ORIGINAL start, a cancel that never clears
   * because `running` stopped being treated as an absolute LEVEL, or
   * `meeting_timer_slot_visible` collapsing to a constant.
   */
  test("the host starts a countdown every participant sees, and extend / cancel reach them too", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_meeting_timer_rt_${Date.now()}`;
    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostPage = await newParticipant(hostBrowser, "host@videocall.rs", "HostUser", uiURL);
      const guestPage = await newParticipant(
        guestBrowser,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
      );
      await enterTwoUserMeeting(hostPage, guestPage, meetingId);

      // ── Arrange: no timer anywhere. The chip is SELF-GATING — it emits zero
      // element nodes while nothing is running — so this is an absence, not a
      // hidden element. That property is load-bearing for the specs that address
      // `#grid-container`'s children positionally.
      await expect(hostPage.locator(CHIP)).toHaveCount(0);
      await expect(guestPage.locator(CHIP)).toHaveCount(0);

      // ── THE HOST-ONLY GATE. The guest must have no control at all: not the
      // button, not its action-bar slot, not the popover. Asserted BEFORE the
      // timer exists and again after it is running (below), because "hidden while
      // idle" and "hidden while a timer runs" are different renders — the
      // button's `running` prop feeds a `data-` hook and its tooltip copy.
      await wakeControls(guestPage);
      await expect(guestPage.locator(TRIGGER_ANY)).toHaveCount(0);
      await expect(guestPage.locator(SLOT)).toHaveCount(0);
      await expect(guestPage.locator(POPOVER)).toHaveCount(0);

      // ...and the host does have it. Stated explicitly so a run in which the
      // control was missing for EVERYONE — a far more likely breakage than a
      // leak to the guest — fails here rather than passing the absence check
      // above for the wrong reason.
      await wakeControls(hostPage);
      await expect(hostPage.locator(SLOT)).toHaveCount(1);
      const hostTrigger = hostPage.locator(TRIGGER);
      await expect(hostTrigger).toBeVisible({ timeout: 15_000 });
      await expect(hostTrigger).toHaveAttribute("data-running", "false");
      // The stable NOUN. A name that flipped with state would cancel out
      // `aria-expanded` and announce the inverse of reality half the time.
      await expect(hostTrigger).toHaveAttribute("aria-label", "Meeting timer");
      // WCAG 2.5.3 Label in Name: the visible tooltip title matches it verbatim.
      await expect(hostPage.locator(`${TRIGGER} .tooltip-title`)).toHaveText("Meeting timer");

      // ── Act: open the idle popover and start a 5-minute timer.
      await openTimerPopover(hostPage);
      const popover = hostPage.locator(POPOVER);
      await expect(popover).toHaveAttribute("data-running", "false");
      await expect(popover).toHaveAttribute("role", "dialog");
      // Every preset is its own one-click button in the idle branch.
      for (const ms of [ONE_MIN_MS, FIVE_MIN_MS, TEN_MIN_MS, FIFTEEN_MIN_MS]) {
        await expect(hostPage.locator(preset(ms))).toBeVisible();
      }
      // Idle offers no extend / cancel — there is nothing to extend or cancel.
      await expect(hostPage.locator(EXTEND)).toHaveCount(0);
      await expect(hostPage.locator(CANCEL)).toHaveCount(0);

      await hostPage.locator(preset(FIVE_MIN_MS)).click();

      // The popover closes on start, so the host is not left staring at a panel
      // whose branch has just changed under them.
      await expect(popover).toHaveCount(0, { timeout: 10_000 });
      await expect(hostTrigger).toHaveAttribute("aria-expanded", "false");
      await expect(hostTrigger).toHaveAttribute("data-running", "true");

      // ── Assert: THE GUEST SEES IT. This is the requirement.
      const guestChip = guestPage.locator(CHIP);
      await expect(guestChip).toBeVisible({ timeout: CROSS_PEER_TIMEOUT });
      // A 5-minute timer is comfortably inside Normal: its warning threshold is
      // `min(120s flat, duration/2).max(duration/4)` = 120s, so Normal holds for
      // the first three minutes.
      await expect(guestChip).toHaveAttribute("data-urgency", "normal");
      // The chip is a STATUS, not a control: a `role="img"` with a noun-plus-value
      // accessible name, and nothing about it toggles.
      await expect(guestChip).toHaveAttribute("role", "img");
      await expect(guestChip).toHaveAttribute("aria-label", /^Meeting timer: .+ remaining$/);

      // It is the SAME timer on both pages, not two clients each counting their
      // own. Both derive from one `ends_at_ms`, so they agree to within the read
      // gap; a guest that started a fresh 5:00 on receipt would be seconds ahead.
      await expect(hostPage.locator(CHIP)).toBeVisible({ timeout: 10_000 });
      const guestRemaining = await remainingMs(guestPage, "guest");
      const hostRemaining = await remainingMs(hostPage, "host");
      expect(guestRemaining).toBeGreaterThan(FIVE_MIN_MS - 60_000);
      expect(guestRemaining).toBeLessThanOrEqual(FIVE_MIN_MS);
      expect(
        Math.abs(guestRemaining - hostRemaining),
        "host and guest must be counting down the same timer",
      ).toBeLessThan(5_000);

      // The screen-reader channel exists for the NON-HOST too, on a milestone
      // cadence rather than per second, and announces the transition.
      const guestLiveRegion = guestPage.locator(LIVE_REGION);
      await expect(guestLiveRegion).toHaveCount(1);
      await expect(guestLiveRegion).toHaveAttribute("role", "status");
      // `polite` and NOT `off`: a `role="status"` with `aria-live="off"` is
      // self-cancelling — the role sets up a live region the attribute switches
      // back off, so nothing is ever announced.
      await expect(guestLiveRegion).toHaveAttribute("aria-live", "polite");
      await expect(guestLiveRegion).toContainText("Meeting timer set.", {
        timeout: CROSS_PEER_TIMEOUT,
      });

      // The gate holds while a timer RUNS, which is a different render from the
      // idle one asserted above — and the moment a non-host would most plausibly
      // be handed controls.
      await expect(guestPage.locator(TRIGGER_ANY)).toHaveCount(0);
      await expect(guestPage.locator(SLOT)).toHaveCount(0);

      // ── Act: EXTEND. The running branch replaces the presets — starting a
      // fresh timer mid-run is not a thing a host can want, and a row of dead
      // buttons reads as breakage rather than as a deliberate restriction.
      await openTimerPopover(hostPage);
      await expect(popover).toHaveAttribute("data-running", "true");
      await expect(hostPage.locator(EXTEND)).toBeVisible();
      await expect(hostPage.locator(CANCEL)).toBeVisible();
      await expect(hostPage.locator(preset(FIVE_MIN_MS))).toHaveCount(0);

      const guestBeforeExtend = await remainingMs(guestPage, "guest");
      await hostPage.locator(EXTEND).click();

      // ── Assert: the extension reaches the guest. The threshold is half the
      // step rather than the step itself because the countdown keeps ticking
      // DOWN across the settle window; anything above ~0 already discriminates a
      // real extension from a no-op (a no-op leaves the value falling), and
      // half the step leaves no room for a slow fan-out to fake a pass.
      await expect
        .poll(async () => remainingMs(guestPage, "guest"), {
          timeout: CROSS_PEER_TIMEOUT,
          message: "the guest's countdown must jump forward when the host adds a minute",
        })
        .toBeGreaterThan(guestBeforeExtend + EXTEND_STEP_MS / 2);

      // The host's own view moved too — it renders from the local echo, since the
      // relay self-skips the sender and it never receives its own packet back.
      expect(await remainingMs(hostPage, "host")).toBeGreaterThan(
        guestBeforeExtend + EXTEND_STEP_MS / 2,
      );

      // ── Act: CANCEL. The popover stays open across the extend (nothing about
      // adding time closes it), so it is still up.
      await expect(popover).toBeVisible();
      await hostPage.locator(CANCEL).click();

      // ── Assert: the chip UNMOUNTS on both pages rather than lingering at 0:00
      // over the video. A cancel has no heartbeat behind it — once `running` is
      // false the host goes quiet — so this is carried entirely by the transition
      // repeat burst, which is the reason that burst exists.
      await expect(popover).toHaveCount(0, { timeout: 10_000 });
      await expect(hostPage.locator(CHIP)).toHaveCount(0, { timeout: 10_000 });
      await expect(guestChip).toHaveCount(0, { timeout: CROSS_PEER_TIMEOUT });
      await expect(hostTrigger).toHaveAttribute("data-running", "false");
      // The cancellation is ANNOUNCED. Clearing a live region announces nothing,
      // so a screen-reader user would otherwise never learn the timer stopped.
      await expect(guestLiveRegion).toHaveText("Meeting timer cancelled.", {
        timeout: CROSS_PEER_TIMEOUT,
      });
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  /**
   * A participant who joins AFTER the timer started must still see it.
   *
   * This is what the ~5s heartbeat exists for, and the heartbeat is the ONLY
   * thing that can deliver it: the relay keeps no timer registry, there is no
   * join-event re-announce for this packet type (unlike raise-hand's
   * `on_peer_joined` path), and `MeetingTimerCtx` is purely live. The host
   * re-sending its current state on a fixed cadence is the entire mechanism.
   *
   * THE 10-SECOND SETTLE IS LOAD-BEARING, not padding. The transition repeat
   * burst finishes ~2.5s after the click (debounce 500ms, then 3 repeats ~1s
   * apart). Waiting past that guarantees the guest's session is established
   * strictly AFTER the last burst packet, so nothing the guest sees can have
   * come from the burst — only from a heartbeat. Without the wait a guest that
   * happened to connect early would pass this test with the heartbeat deleted.
   *
   * Mutation sensitivity: delete step 3 of `MeetingTimerScheduler::poll` (the
   * `current.running && now_ms >= self.next_heartbeat_at_ms` arm) and the guest's
   * chip never appears at all. There is no timing window in which it passes
   * anyway.
   */
  test("a participant who joins after the timer started converges on the heartbeat", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_meeting_timer_latejoin_${Date.now()}`;
    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostPage = await newParticipant(hostBrowser, "host@videocall.rs", "HostUser", uiURL);
      const guestPage = await newParticipant(
        guestBrowser,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
      );

      // ── Host starts a 10-minute timer while ALONE. No peer exists to receive
      // the transition burst, so nothing the guest later sees can have come from
      // it — the long duration simply keeps the timer comfortably alive across
      // the whole join dance.
      await enterMeetingAsHost(hostPage, meetingId);
      await openTimerPopover(hostPage);
      await hostPage.locator(preset(TEN_MIN_MS)).click();
      await expect(hostPage.locator(CHIP)).toBeVisible({ timeout: 15_000 });

      // Put the burst definitively in the past (see the doc comment).
      await hostPage.waitForTimeout(10_000);

      // ── Guest joins only now. Resolves once both see each other's canvas.
      await guestJoinsMeeting(hostPage, guestPage, meetingId);

      // ── The heartbeat is the only way this can be true.
      const guestChip = guestPage.locator(CHIP);
      await expect(guestChip).toBeVisible({ timeout: HEARTBEAT_TIMEOUT });
      await expect(guestChip).toHaveAttribute("aria-label", /^Meeting timer: .+ remaining$/);

      // ...and it is the ORIGINAL timer, not a fresh 10:00 minted on receipt.
      // The 10s settle plus the join dance guarantees well over 15s has elapsed,
      // so a client that restarted the clock would read visibly higher than the
      // host. Both read from one `ends_at_ms`, so the true gap is the read gap.
      const hostRemaining = await remainingMs(hostPage, "host");
      const guestRemaining = await remainingMs(guestPage, "guest");
      expect(
        hostRemaining,
        "the join dance must have consumed enough of the timer for the comparison below to discriminate",
      ).toBeLessThan(TEN_MIN_MS - 15_000);
      expect(
        Math.abs(guestRemaining - hostRemaining),
        "the late joiner must adopt the running timer, not start its own",
      ).toBeLessThan(5_000);

      // The late joiner is still not the host, so it still gets no control.
      await wakeControls(guestPage);
      await expect(guestPage.locator(TRIGGER_ANY)).toHaveCount(0);
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  /**
   * The countdown escalates through its urgency states and lands on EXPIRED —
   * asserted on the NON-HOST, together with the sound the issue promises.
   *
   * "When it reaches zero every participant hears a sound" is the clause this
   * test exists for, and the participant that matters is the one who did not set
   * the timer: the host reaches expiry through its own local echo, whereas the
   * guest has to receive the state over the wire, sample it against its own
   * clock (`CountdownSample`), and drive its own countdown to zero. Only the
   * second path can break in a way the first hides.
   *
   * WHY THE 1-MINUTE PRESET. It is the only preset whose full escalation fits in
   * a test. A 1-minute timer starts already inside the 120s ABSOLUTE warning
   * floor, goes critical at 30s remaining, and expires at 60s — so warning ->
   * critical -> expired is ~60s of real time. The NORMAL state is covered in the
   * first test, where a 5-minute timer sits comfortably above both the absolute
   * floor and the 25% proportional term; a normal -> warning transition on one
   * timer would need a 3-minute wait for the shortest preset that has one, which
   * buys nothing the two assertions together do not already give.
   *
   * NO CLOCK PATCHING. Driving `performance.now()` forward would be faster, but
   * every other subsystem in the meeting reads the same clock (fps accounting,
   * freeze detection, decode budget, connection quality), so a jump would
   * destabilise the very peer connection this test needs and could turn a real
   * failure into a confusing one. 60 s of honest wall time is cheaper than that.
   *
   * WHAT THE AUDIO ASSERTION PROVES: that the page built the 3-tone 880 Hz graph
   * `play_timer_expired_sound` describes — not that audio reached a speaker,
   * which no browser test can observe. See `AUDIO_TONE_SPY`.
   */
  test("the countdown escalates to expired for a non-host, and the expiry cue fires", async ({
    baseURL,
  }) => {
    // ~60s of countdown plus a ~12s latch re-check on top of a two-browser boot.
    test.setTimeout(300_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_meeting_timer_expiry_${Date.now()}`;
    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostPage = await newParticipant(hostBrowser, "host@videocall.rs", "HostUser", uiURL);
      // Only the guest needs the tone spy — it is the participant under test.
      const guestPage = await newParticipant(
        guestBrowser,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
        [AUDIO_TONE_SPY],
      );
      await enterTwoUserMeeting(hostPage, guestPage, meetingId);

      // Nothing has sounded yet. Pinned before the timer starts so the count
      // asserted after expiry cannot be inherited from something earlier — the
      // join cue in particular runs on this page moments before.
      expect(
        await expiryTonesPlayed(guestPage),
        "no expiry tone may have sounded before a timer exists",
      ).toBe(0);

      // ── Host starts the 1-minute timer.
      await openTimerPopover(hostPage);
      await hostPage.locator(preset(ONE_MIN_MS)).click();

      const guestChip = guestPage.locator(CHIP);
      await expect(guestChip).toBeVisible({ timeout: CROSS_PEER_TIMEOUT });

      // ── WARNING at 30s remaining (`duration/2` for this preset — the flat
      // 120s floor does not apply to a 60s timer, or it would start amber and
      // never render Normal). Note this is ~30s of REAL TIME after the start,
      // so it needs the urgency-step budget, not the cross-peer one.
      await expect(guestChip).toHaveAttribute("data-urgency", "warning", {
        timeout: URGENCY_STEP_TIMEOUT,
      });
      expect(await remainingMs(guestPage, "guest")).toBeLessThanOrEqual(30_000);

      // ── CRITICAL at 15s remaining (`duration/4`).
      await expect(guestChip).toHaveAttribute("data-urgency", "critical", {
        timeout: URGENCY_STEP_TIMEOUT,
      });
      expect(await remainingMs(guestPage, "guest")).toBeLessThanOrEqual(15_000);

      // ── EXPIRED at zero. The chip STAYS mounted — the host never cancelled, so
      // `running` is still true — and reads 0:00, which is reserved for this
      // state alone (`format_remaining` rounds UP so a running timer can never
      // display it).
      await expect(guestChip).toHaveAttribute("data-urgency", "expired", {
        timeout: URGENCY_STEP_TIMEOUT,
      });
      await expect(guestPage.locator(CHIP_VALUE)).toHaveText("0:00");
      expect(await remainingMs(guestPage, "guest")).toBe(0);
      // The accessible name switches to words rather than leaving a screen-reader
      // user to infer expiry from "0 seconds remaining".
      await expect(guestChip).toHaveAttribute("aria-label", "Meeting timer: time is up");

      // ...and the SPOKEN channel says so too. This is the milestone path (the
      // guest watched zero arrive, so it is a real crossing), and it is the only
      // receipt for the live region's wiring — the effect that drives it lives in
      // a `#[component]` body and is unreachable from a `#[test]`.
      //
      // Note the composer has a THIRD branch that this cannot reach: a client
      // arriving at an ALREADY-expired timer has no crossing to detect, so its
      // "Meeting timer: time is up." comes from the transition composer instead.
      // That branch is pure and is pinned by
      // `transition_text_distinguishes_cancel_from_start_from_already_expired`.
      await expect(guestPage.locator(LIVE_REGION)).toContainText("Time is up.", {
        timeout: URGENCY_STEP_TIMEOUT,
      });

      // ── THE SOUND. Three tones at 880 Hz, and exactly three: the cue is
      // latched to the timer's identity so a heartbeat, a transition repeat, or a
      // re-render cannot make the room beep twice. The host keeps heartbeating
      // the expired-but-running state every ~5s, so an unlatched implementation
      // would keep firing — hence the settle before the count is read.
      await expect
        .poll(async () => expiryTonesPlayed(guestPage), {
          timeout: 15_000,
          message: "the expiry cue must be synthesized on a non-host participant",
        })
        .toBe(3);
      await guestPage.waitForTimeout(12_000);
      expect(
        await expiryTonesPlayed(guestPage),
        "the expiry cue must fire ONCE per timer — two heartbeats have landed since",
      ).toBe(3);
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
  /**
   * THE OVERFLOW ROUTE. The action-bar item and the overflow item are two
   * different code paths, and only one of them was wired.
   *
   * This matters more than it looks. `migrate_stored_layout` APPENDS a
   * newly-shipped slot to every existing user's saved layout, so for everyone
   * already using the product the overflow menu is the FIRST place this control
   * appears. It is also the only place it appears at all on a bar narrower than
   * the 1103px full-fit threshold that adding this slot pushed the default
   * layout to — and the other tests in this file run wide, which is precisely
   * why the missing arm survived them.
   *
   * What a regression looks like: the `ActionBarSlot::MeetingTimer` arm removed
   * from the overflow activation match, leaving the item to render (its icon and
   * label arms live elsewhere and would still work) and do nothing but close the
   * menu.
   */
  test("the host can open the timer controls from the overflow menu on a narrow bar", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_meeting_timer_overflow_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newParticipant(browser, "host@videocall.rs", "HostUser", uiURL);
      await enterMeetingAsHost(page, meetingId);

      // The same 400x720 the action-bar overflow spec proves forces overflow.
      await page.setViewportSize({ width: 400, height: 720 });
      await page.waitForTimeout(500);

      // PRECONDITION, not decoration: if the bar did not actually overflow, every
      // assertion below would pass against the wrong thing.
      await expect(page.locator(TRIGGER)).toBeHidden();

      await wakeControls(page);
      await page.locator("#overflow-menu-trigger").click();
      const item = page.locator(".overflow-item", { hasText: "Meeting timer" });
      await expect(item).toBeVisible();
      await item.click();

      // THE assertion. Before the fix this closed the menu and did nothing.
      await expect(page.locator(POPOVER)).toBeVisible({ timeout: 10_000 });
      await expect(page.locator(POPOVER)).toHaveAttribute("data-running", "false");

      // And the control it opens actually works from this route.
      await page.locator(preset(ONE_MIN_MS)).click();
      await expect(page.locator(CHIP)).toBeVisible({ timeout: 15_000 });
    } finally {
      await browser.close();
    }
  });

  /**
   * NORMAL -> WARNING on a single timer.
   *
   * Only affordable since the urgency thresholds became proportional: with the
   * old flat 120s warning floor the 60s preset satisfied it at t=0, so the
   * shortest preset we ship STARTED amber and Normal was unreachable on it. The
   * shortest preset with a Normal phase was 5 minutes, which needed a 3-minute
   * wait to observe a transition — so this was an accepted coverage gap.
   *
   * Now the 60s preset is Normal 60->30, Warning 30->15, Critical 15->0, and the
   * whole ramp fits inside one minute of real time. Deliberately NOT
   * fast-forwarded: `performance.now()` is also read by fps accounting, freeze
   * detection, the decode budget and connection quality, so moving it would
   * perturb four unrelated subsystems to save 45 seconds.
   */
  test("a short timer walks normal to warning to critical for a non-host", async ({ baseURL }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_meeting_timer_ramp_${Date.now()}`;
    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostPage = await newParticipant(hostBrowser, "host@videocall.rs", "HostUser", uiURL);
      const guestPage = await newParticipant(
        guestBrowser,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
      );
      await enterTwoUserMeeting(hostPage, guestPage, meetingId);

      await openTimerPopover(hostPage);
      await hostPage.locator(preset(ONE_MIN_MS)).click();

      const guestChip = guestPage.locator(CHIP);
      await expect(guestChip).toBeVisible({ timeout: CROSS_PEER_TIMEOUT });

      // NORMAL first -- the assertion that was impossible before the threshold
      // fix, because a 60s timer used to start amber.
      await expect(guestChip).toHaveAttribute("data-urgency", "normal");

      // -> WARNING at 30s remaining.
      await expect(guestChip).toHaveAttribute("data-urgency", "warning", {
        timeout: URGENCY_STEP_TIMEOUT,
      });

      // -> CRITICAL at 15s remaining.
      await expect(guestChip).toHaveAttribute("data-urgency", "critical", {
        timeout: URGENCY_STEP_TIMEOUT,
      });
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});
