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
// Issue 2329's chime pitches, imported rather than copied — see
// `handChimeTonesPlayed` for why a local copy here was a silent-failure hazard.
import { HAND_TONE_HIGH, TONE_EPSILON } from "../helpers/hand-chime-tones";

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

/**
 * ISSUE 2172 — the typed-duration row, which renders only in the popover's IDLE
 * branch. Each of these is anchored to exactly one element in
 * `MeetingTimerPopover`'s RSX (`dioxus-ui/src/components/meeting_timer.rs`):
 *
 *   input  { "data-testid": "meeting-timer-custom-value" }
 *   select { "data-testid": "meeting-timer-custom-unit" }
 *   button { "data-testid": "meeting-timer-custom-start" }
 *   p      { "data-testid": "meeting-timer-custom-hint" }
 *
 * Deliberately UNSCOPED rather than `${POPOVER} …`, and that is the safer of the
 * two here: each string appears exactly once in the whole UI (nothing outside
 * this component writes `meeting-timer-custom-`), and an unscoped test id cannot
 * be scoped into the WRONG container — which is how #1756 failed. The popover
 * itself is mounted at most once (`if meeting_timer_open() && local_is_host()`
 * in `attendants.rs`), so there is no second copy to disambiguate.
 */
const CUSTOM_VALUE = '[data-testid="meeting-timer-custom-value"]';
const CUSTOM_UNIT = '[data-testid="meeting-timer-custom-unit"]';
const CUSTOM_START = '[data-testid="meeting-timer-custom-start"]';
const CUSTOM_HINT = '[data-testid="meeting-timer-custom-hint"]';

/**
 * The visible `<label>`, addressed BY THE ID IT POINTS AT rather than by its
 * class. `MEETING_TIMER_CUSTOM_VALUE_ID` feeds both the label's `for` and the
 * input's `id`, so a selector written this way stops matching the moment those
 * two drift apart — which is exactly the defect worth catching, since the label
 * is also the field's click target.
 */
const CUSTOM_LABEL = 'label[for="meeting-timer-custom-value"]';

/**
 * `MEETING_TIMER_CUSTOM_HINT_ID` — the target of the `aria-describedby` on BOTH
 * the field and the confirm button, so the reason the button is inert is
 * reachable from whichever of the two the user is on.
 */
const CUSTOM_HINT_ID = "meeting-timer-custom-hint";

/**
 * The amount and its unit are ONE value split across two controls, exposed as a
 * single labelled `role="group"`. `MEETING_TIMER_CUSTOM_LABEL_ID` is what the
 * group's `aria-labelledby` points at.
 */
const CUSTOM_GROUP = ".meeting-timer-custom-group";
const CUSTOM_LABEL_ID = "meeting-timer-custom-label";

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
 * 880 IS UNIQUE TO IT. Every synthesized cue in the meeting (`attendants.rs`,
 * `meeting_timer.rs`) is accounted for here:
 *
 *     join          play_tone_pair(523.25,  659.25, ...)
 *     leave         play_tone_pair(659.25,  440.0,  ...)
 *     hand raised   play_tone_pair(987.77,  1318.51, ...)
 *     hand lowered  play_tone_pair(1318.51, 987.77,  ...)
 *     timer expired 880 x 3
 *
 * Nothing but the expiry cue touches 880, so counting 880s remains an
 * unambiguous fingerprint for `play_timer_expired_sound`.
 *
 * THIS PARAGRAPH HAS BEEN WRONG ONCE, which is why the guard below exists.
 * Issue 2329 briefly pitched the hand chime at A5 (880 Hz) and D6, and for that
 * window the claim above was false while still reading as true — the counts here
 * stayed correct only because this spec happens never to raise a hand. The
 * chime was then moved to B5/E6 specifically to restore this discriminator
 * rather than leave it depending on an accident. `handChimeTonesPlayed` now
 * enforces the separation on every run of the expiry test, so a future cue that
 * lands on 880 fails loudly here instead of silently corrupting the counts.
 *
 * The gain values this also records are 0.35 / 0.25 for join/leave and
 * 0.15 / 0.12 for the hand chimes — all well below 880, so none of them can be
 * mistaken for a frequency by the filters here.
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

/**
 * How many raise-hand chime tones (issue 2329) the page has synthesized.
 *
 * A STANDING DECOUPLING GUARD, kept deliberately even though the hand chime no
 * longer shares a pitch with the expiry cue. Its job is to make sure it never
 * does again: `expiryTonesPlayed` is a bare `=== 880`, so the counts asserted in
 * the expiry test are attributable to the timer only while no other cue emits
 * 880. That was briefly untrue during issue 2329 (see `AUDIO_TONE_SPY`), and it
 * was untrue in the worst possible way — invisibly. Five lines that turn a
 * future re-collision into a loud failure are cheaper than discovering it from
 * a passing test.
 *
 * Detects the hand pair by its HIGH endpoint (E6), which appears in both
 * directions of the chime, so one filter covers a raise and a lower alike.
 *
 * The pitch and tolerance are imported from `helpers/hand-chime-tones.ts`
 * rather than written here. That is the whole point of the shared module: this
 * used to be a second hardcoded copy, and a stale copy would match nothing,
 * count zero, and PASS — reporting "no hand chime interfered" for exactly the
 * reason that makes the report worthless.
 */
async function handChimeTonesPlayed(page: Page): Promise<number> {
  return page.evaluate(
    ({ high, eps }) => {
      const tones = (window as Window & { __vcTimerTones?: number[] }).__vcTimerTones ?? [];
      return tones.filter((v) => Math.abs(v - high) < eps).length;
    },
    { high: HAND_TONE_HIGH, eps: TONE_EPSILON },
  );
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
      // ...and every one of those 880s belongs to the timer. Issue 2329's hand
      // chime uses A5 too, so this is what makes the count above attributable
      // rather than merely correct-looking. See `AUDIO_TONE_SPY`.
      expect(
        await handChimeTonesPlayed(guestPage),
        "no hand chime may sound in this spec — the 880 Hz count above is the timer's alone",
      ).toBe(0);
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

/**
 * The host TYPES a duration — issue 2172.
 *
 * The four presets are 1 / 5 / 10 / 15 minutes, so before this change the
 * shortest timer the product could produce was a minute and nothing between the
 * presets was reachable at all. The issue's own example ("such as 30 seconds")
 * was therefore unreachable by any sequence of clicks. The popover's idle branch
 * now carries a number field, a minutes/seconds select and a confirm button
 * beneath the preset row, all feeding the SAME `on_start` prop the presets use.
 *
 * WHY EVERY TEST HERE FAILS ON THE UN-FIXED CODE. Trivially, and worth stating
 * plainly: the entire row is new. On the pre-change popover
 * `[data-testid="meeting-timer-custom-value"]`, `-unit`, `-start` and `-hint`
 * match zero elements, so the first `toBeVisible()` in each test fails before it
 * reaches anything subtler. Reverting `meeting_timer.rs` breaks all three. The
 * assertions past that point are what make them worth more than a smoke test:
 * a sub-minute countdown NO PRESET CAN PRODUCE crossing the wire, the confirm
 * button's disabled state tracking `custom_duration_ms`, and Enter reaching the
 * same submit path as the click.
 *
 * TAGGING: untagged, matching every test in this file and the two-browser specs
 * it was modelled on. They run under `--project=dioxus` only —
 *
 *     make e2e SPEC=meeting-timer
 *
 * — or a `/run-e2e` dioxus dispatch on the PR. A per-PR "Playwright bvt1" green
 * does NOT cover them.
 *
 * CAMERA: not seeded (`vc_prejoin_camera_on`), for the reason the file header
 * gives — every surface asserted here renders over the camera-off placeholder
 * exactly as it does over video.
 *
 * WHY TWO OF THE THREE ARE SOLO, given this file's "no solo tests" rule. That
 * rule is about the WIRE: a host renders its own timer from a local echo, so a
 * solo test proves nothing about `send_meeting_timer`, the relay's host gate, or
 * `on_meeting_timer`. The first test below is therefore two-browser, and it is
 * the one that proves a typed duration reaches the room. The other two are about
 * a control's LOCAL behaviour — whether the confirm button is disabled, whether
 * Enter reaches the same submit closure the click does — which is settled
 * entirely inside the host's own document, exactly like the overflow-route test
 * above. Booting a second browser to watch would add cost and prove nothing.
 */
test.describe("Meeting timer — typed custom duration (issue 2172)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * THE ISSUE'S HEADLINE CASE: 30 seconds, typed, seen by everyone.
   *
   * The cross-peer half is what makes this more than a UI test, and the
   * DISCRIMINATOR is the duration itself. Every preset is at least a minute, so
   * a guest chip reading under 60 s cannot have come from one — the sub-minute
   * value is proof that the number the host typed is the number that crossed the
   * wire, not merely that *some* timer started. That is asserted three ways: the
   * chip's rendered `M:SS` has a zero minutes field, `data-remaining-ms` is at
   * most 30 000, and the guest's spoken announcement is in SECONDS.
   *
   * What a regression looks like: the confirm button wired to its own handler
   * instead of the shared `on_start` prop (the popover would stay open and focus
   * would never return to the trigger), or `custom_duration_ms` scaling by the
   * wrong unit (the guest would get 30 minutes and the chip would read `30:00`).
   * A unit select that failed to write `custom_unit` fails EARLIER, at the label
   * assertion before the click — the button would still advertise minutes — and
   * that is the intended order: a control whose label disagrees with what it
   * starts is worth failing on before anything is sent to the room.
   */
  test("the host types a 30-second timer and every participant sees it", async ({ baseURL }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_meeting_timer_custom_${Date.now()}`;
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

      // ── Arrange: nothing is running anywhere.
      await expect(hostPage.locator(CHIP)).toHaveCount(0);
      await expect(guestPage.locator(CHIP)).toHaveCount(0);

      await openTimerPopover(hostPage);
      const popover = hostPage.locator(POPOVER);
      await expect(popover).toHaveAttribute("data-running", "false");

      // The typed row is an ADDITION to the one-click path, not a replacement:
      // a change that moved the presets behind the field would regress the
      // common case, so both are asserted present before either is used.
      await expect(hostPage.locator(preset(FIVE_MIN_MS))).toBeVisible();
      const value = hostPage.locator(CUSTOM_VALUE);
      const unit = hostPage.locator(CUSTOM_UNIT);
      const start = hostPage.locator(CUSTOM_START);
      await expect(value).toBeVisible();
      await expect(unit).toBeVisible();
      await expect(start).toBeVisible();

      // ── Act: type 30, switch the unit to seconds.
      await value.pressSequentially("30", { delay: 60 });
      await unit.selectOption("seconds");

      // The control SAYS what it is about to start, before it is pressed. Both
      // strings are rendered from the one `custom_duration_ms` result that also
      // decides what gets sent, so this pins that they cannot disagree — a host
      // reading "30 seconds" and getting 30 minutes is the failure it rules out.
      //
      // `aria-disabled`, not the `disabled` property: the button stays focusable
      // on purpose (see the validation test), so `aria-disabled="false"` is
      // where "live" is actually written.
      await expect(start).toHaveAttribute("aria-disabled", "false");
      await expect(start).toHaveAttribute("aria-label", "Start a 30 seconds timer");
      await expect(hostPage.locator(CUSTOM_HINT)).toHaveText("Timer will run for 30 seconds.");

      await start.click();

      // ── The popover closes and focus returns to the trigger, because the
      // confirm button goes through the SAME `on_start` prop as a preset — the
      // one that closes the panel and calls `focus_element_by_id`. A bespoke
      // handler that only started the timer would leave both of these wrong.
      const hostTrigger = hostPage.locator(TRIGGER);
      await expect(popover).toHaveCount(0, { timeout: 10_000 });
      await expect(hostTrigger).toHaveAttribute("aria-expanded", "false");
      await expect(hostTrigger).toHaveAttribute("data-running", "true");
      await expect(hostTrigger).toBeFocused();

      // ── Assert: THE GUEST SEES IT, and sees a SUB-MINUTE timer.
      const guestChip = guestPage.locator(CHIP);
      await expect(guestChip).toBeVisible({ timeout: CROSS_PEER_TIMEOUT });
      // Announced in seconds. No preset can produce this wording: the shortest
      // is a minute, which reads "1 minute remaining".
      //
      // Nothing replaces this text before expiry, so the assertion is not racing
      // a later milestone: `announces_milestone` DROPS the 10-second call for a
      // timer of a minute or less, and the 30-second one needs a crossing from
      // above 30 s that a 30-second timer never has. The only milestone left is
      // zero, ~30 s away.
      await expect(guestPage.locator(LIVE_REGION)).toHaveText(
        /^Meeting timer set\. \d{1,2} seconds remaining\.$/,
        { timeout: CROSS_PEER_TIMEOUT },
      );
      // `format_remaining` renders `M:SS`, so a zero minutes field is the
      // rendered proof of a sub-minute timer.
      await expect(guestPage.locator(CHIP_VALUE)).toHaveText(/^0:\d{2}$/);

      const guestRemaining = await remainingMs(guestPage, "guest");
      expect(
        guestRemaining,
        "the typed timer must not have expired before it was read",
      ).toBeGreaterThan(0);
      expect(
        guestRemaining,
        "30 seconds is what the host typed, and no preset could have produced it",
      ).toBeLessThanOrEqual(30_000);

      // It is the SAME timer on both pages, not two clients each counting their
      // own — both derive from one `ends_at_ms`, so they agree to the read gap.
      await expect(hostPage.locator(CHIP)).toBeVisible({ timeout: 10_000 });
      const hostRemaining = await remainingMs(hostPage, "host");
      expect(
        Math.abs(guestRemaining - hostRemaining),
        "host and guest must be counting down the same timer",
      ).toBeLessThan(5_000);

      // ...and it is COUNTING DOWN rather than a chip frozen at its start value.
      // One tick is 1 s, so this settles almost immediately; the 30 s timer has
      // no way to reach zero inside the window and make this pass vacuously.
      await expect
        .poll(async () => remainingMs(guestPage, "guest"), {
          timeout: 10_000,
          message: "the typed timer must actually count down on the guest",
        })
        .toBeLessThan(guestRemaining);
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  /**
   * The confirm button refuses anything that is not a whole count of units.
   *
   * A live-looking button that starts something the host did not type is the
   * failure `custom_duration_ms` returning `None` prevents. Every case below is
   * driven through the real field, so what is under test is the production
   * parser wired to the real control — not a re-implementation of its rules in
   * TypeScript.
   *
   * TWO DELIBERATE IMPLEMENTATION CHOICES SHAPE THE ASSERTIONS, and both are
   * asserted as written rather than through a matcher that would paper over
   * them:
   *
   *  * The refusal is `aria-disabled`, NOT the `disabled` property, so the
   *    button keeps its place in the tab order and can still describe itself to
   *    someone who tabbed to it. `submit_custom` is the thing that makes it
   *    inert: it re-parses and no-ops on `None`. So the state is read off the
   *    attribute, and — because an aria-disabled button is still clickable —
   *    this test also CLICKS it while inert and proves nothing starts. A real
   *    `disabled` here would fail these assertions, which is the point: it
   *    would be a regression of the keyboard behaviour, not a tidy-up.
   *  * The field is `type="text"` with `inputmode="numeric"`, not
   *    `type="number"`, so junk STAYS ON SCREEN instead of being blanked by the
   *    browser's value sanitization. That is what makes the rejection visible
   *    (and what makes it testable by keyboard at all): "1.5" typed character
   *    by character is still "1.5" when the typing stops. The junk cases below
   *    are therefore typed, not `fill`ed — a `fill` would prove the parser
   *    rejects a value but not that the host can still SEE what they typed.
   *
   * The exhaustive junk list ("", "   ", "000", "+5", "1e3", "5m", non-ASCII
   * digits …) is pinned in `meeting_timer.rs`'s own
   * `an_entry_that_is_not_a_duration_disables_the_confirm_button`. What this
   * test adds is that the parser is actually WIRED to the button's state, its
   * accessible name and the hint — which no `#[test]` can reach.
   */
  test("the confirm button refuses anything that is not a whole duration", async ({ baseURL }) => {
    test.setTimeout(120_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_meeting_timer_custom_invalid_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newParticipant(browser, "host@videocall.rs", "HostUser", uiURL);
      await enterMeetingAsHost(page, meetingId);
      await openTimerPopover(page);

      const value = page.locator(CUSTOM_VALUE);
      const unit = page.locator(CUSTOM_UNIT);
      const start = page.locator(CUSTOM_START);
      const hint = page.locator(CUSTOM_HINT);
      await expect(value).toBeVisible();
      await expect(unit).toBeVisible();
      await expect(start).toBeVisible();
      await expect(hint).toBeVisible();

      // ── The field is LABELLED and DESCRIBED. The label is a real `<label for>`
      // rather than an `aria-label`, so it is also a click target that focuses
      // the field. The amount and the unit are one value, so they sit in one
      // labelled group; the hint is reachable from the field AND from the button,
      // which is what makes the inert button's REASON available from either.
      await expect(page.locator(CUSTOM_LABEL)).toHaveText("Custom length");
      await expect(page.locator(CUSTOM_LABEL)).toHaveAttribute("id", CUSTOM_LABEL_ID);
      await expect(page.locator(CUSTOM_GROUP)).toHaveAttribute("role", "group");
      await expect(page.locator(CUSTOM_GROUP)).toHaveAttribute("aria-labelledby", CUSTOM_LABEL_ID);
      await expect(value).toHaveAttribute("aria-describedby", CUSTOM_HINT_ID);
      await expect(start).toHaveAttribute("aria-describedby", CUSTOM_HINT_ID);
      await expect(hint).toHaveAttribute("id", CUSTOM_HINT_ID);
      await expect(unit).toHaveAttribute("aria-label", "Custom length unit");

      // ── It opens on MINUTES, the unit every preset directly above it is
      // labelled in. Opening on seconds would make a host who types "5" beside a
      // row of "min" buttons get a timer that buzzes at the room in five
      // seconds.
      await expect(unit).toHaveValue("minutes");

      // ── EMPTY: nothing typed, nothing to start.
      await expect(value).toHaveValue("");
      await expect(start).toHaveAttribute("aria-disabled", "true");
      await expect(start).toHaveAttribute("aria-label", "Start a custom timer");
      await expect(hint).toHaveText("Enter a whole number, then pick minutes or seconds.");

      // ── ZERO. A "0 seconds" timer would expire the instant it started.
      await value.pressSequentially("0", { delay: 60 });
      await expect(value).toHaveValue("0");
      await expect(start).toHaveAttribute("aria-disabled", "true");
      await expect(hint).toHaveText("Enter a whole number, then pick minutes or seconds.");

      // ── A FRACTION, typed. Asserting the VALUE first is what makes the inert
      // assertion mean something: it proves the refusal came from
      // `custom_duration_ms` rather than from a field the browser had quietly
      // emptied — the exact failure `type="text"` was chosen to avoid.
      await value.fill("");
      await value.pressSequentially("1.5", { delay: 60 });
      await expect(value).toHaveValue("1.5");
      await expect(start).toHaveAttribute("aria-disabled", "true");
      await expect(hint).toHaveText("Enter a whole number, then pick minutes or seconds.");

      // ── A NEGATIVE, same shape.
      await value.fill("");
      await value.pressSequentially("-5", { delay: 60 });
      await expect(value).toHaveValue("-5");
      await expect(start).toHaveAttribute("aria-disabled", "true");

      // ── LETTERS. A text input keeps them, so this is the parser's refusal
      // being observed, not the browser's.
      await value.fill("");
      await value.pressSequentially("abc", { delay: 60 });
      await expect(value).toHaveValue("abc");
      await expect(start).toHaveAttribute("aria-disabled", "true");

      // ── AND THE INERT BUTTON IS REALLY INERT. `aria-disabled` does not stop a
      // real click the way the `disabled` property does, so the only thing
      // standing between junk and a broadcast START is `submit_custom` re-parsing
      // and no-oping. Clicking here with "abc" in the field is the one way to
      // prove it: drop that guard and the popover closes and a chip appears.
      //
      // `force: true` is REQUIRED and is itself corroboration. Playwright's
      // actionability treats `aria-disabled="true"` on a button role as disabled
      // (`getAriaDisabled` in its injected script), so an ordinary `click()` would
      // wait for the button to become enabled and time out — the same reading an
      // assistive technology takes. Forcing skips that wait and dispatches the
      // real click anyway, which is exactly the adversarial case worth covering.
      await start.click({ force: true });
      await expect(page.locator(POPOVER)).toBeVisible();
      await expect(page.locator(CHIP)).toHaveCount(0);
      await expect(value).toHaveValue("abc");

      // ── VALID: the button comes alive and names the duration it will start.
      await value.fill("");
      await value.pressSequentially("7", { delay: 60 });
      await expect(start).toHaveAttribute("aria-disabled", "false");
      await expect(start).toHaveAttribute("aria-label", "Start a 7 minutes timer");
      await expect(hint).toHaveText("Timer will run for 7 minutes.");

      // ── Changing the UNIT re-resolves the same number without retyping it.
      // Both the label and the hint move, because both read the one parse.
      await unit.selectOption("seconds");
      await expect(start).toHaveAttribute("aria-label", "Start a 7 seconds timer");
      await expect(hint).toHaveText("Timer will run for 7 seconds.");

      // ── OVER THE CAP: clamped, and SAID SO before it is applied. This is not
      // cosmetic — the relay DROPS an over-cap START at ingress rather than
      // clamping it (`packet_handler.rs` forwards only while
      // `duration_ms <= MEETING_TIMER_MAX_DURATION_MS`), so an unclamped send
      // would leave the host watching a local echo of a timer no one else can
      // see. 2000 minutes is over the 24 h ceiling, and the host reads the
      // clamped value while the field still has focus.
      await unit.selectOption("minutes");
      await value.fill("2000");
      await expect(start).toHaveAttribute("aria-disabled", "false");
      await expect(hint).toHaveText("Timer will run for 24 hours.");
      await expect(start).toHaveAttribute("aria-label", "Start a 24 hours timer");

      // ── Back to invalid. The refusal is not a one-way latch: a host who clears
      // the field must not be left with a live button carrying the previous
      // entry's duration.
      await value.fill("");
      await expect(start).toHaveAttribute("aria-disabled", "true");
      await expect(start).toHaveAttribute("aria-label", "Start a custom timer");

      // Nothing was started by any of the above — including the deliberate click
      // on the inert button — and a stray start would have closed the popover out
      // from under the test.
      await expect(page.locator(CHIP)).toHaveCount(0);
      await expect(page.locator(POPOVER)).toBeVisible();
    } finally {
      await browser.close();
    }
  });

  /**
   * ENTER in the field starts the timer, exactly as the confirm button does.
   *
   * A text field inside a dialog is a place people press Enter, and the routes
   * are separate code: the button's `onclick` and a `Key::Enter` arm on EACH of
   * the two fields all call one `submit_custom` closure. This is the only thing
   * that can prove the keyboard routes reach it. Delete either `Key::Enter` arm
   * and the matching phase below fails on its first assertion after the key
   * press: the popover never closes, and no chip ever appears.
   *
   * BOTH FIELDS, because the second is the one that is easy to forget. The unit
   * `<select>` carries its own copy of the handler for a reason its RSX states:
   * Tab-to-unit-then-Enter is the single path a keyboard user takes to change
   * the unit and commit, and without the handler it is a dead key. Phase 3
   * covers it, and it costs a cancel-and-reopen rather than a second browser.
   *
   * SECONDS on purpose, again: 45 s and 20 s are durations no preset can
   * produce, so each resulting chip is proof that the typed entry — not some
   * fallback — is what started.
   *
   * WHAT THIS DOES NOT COVER, stated so the next reader does not assume it does:
   * the field is not inside a `<form>` (nothing in `attendants.rs` wraps it in
   * one), so Enter has no implicit submission to suppress and the handler's
   * `prevent_default` is defensive rather than load-bearing here. Nor does this
   * exercise the reason `submit_custom` reads its signals through `peek` at
   * submit time instead of closing over the render's value — the test lets the
   * control settle before pressing Enter, so it never races a keystroke against
   * the render that follows it. That hazard is a fast typist's, and it is not
   * observable at this granularity.
   */
  test("Enter starts the typed timer from either field, like the confirm button", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_meeting_timer_custom_enter_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newParticipant(browser, "host@videocall.rs", "HostUser", uiURL);
      await enterMeetingAsHost(page, meetingId);
      await openTimerPopover(page);

      const value = page.locator(CUSTOM_VALUE);
      const unit = page.locator(CUSTOM_UNIT);
      const start = page.locator(CUSTOM_START);
      const trigger = page.locator(TRIGGER);
      const chip = page.locator(CHIP);
      await expect(value).toBeVisible();
      await expect(unit).toBeVisible();
      await expect(start).toBeVisible();

      // ── PHASE 1: Enter in the AMOUNT field.
      await unit.selectOption("seconds");
      await value.pressSequentially("45", { delay: 60 });

      // The state Enter is about to commit, read off the control rather than
      // assumed — so a failure below is unambiguously about the key handler.
      await expect(start).toHaveAttribute("aria-disabled", "false");
      await expect(start).toHaveAttribute("aria-label", "Start a 45 seconds timer");

      // `press` focuses the field first, so the keydown lands on the input that
      // carries the handler — not on the select the unit was chosen from.
      await value.press("Enter");

      // Identical to the click path, down to the focus return.
      await expect(page.locator(POPOVER)).toHaveCount(0, { timeout: 10_000 });
      await expect(trigger).toHaveAttribute("aria-expanded", "false");
      await expect(trigger).toHaveAttribute("data-running", "true");
      await expect(trigger).toBeFocused();

      await expect(chip).toBeVisible({ timeout: 15_000 });
      // Sub-minute: 45 s came from the field, and no preset could have made it.
      await expect(page.locator(CHIP_VALUE)).toHaveText(/^0:\d{2}$/);
      const remaining = await remainingMs(page, "host");
      expect(remaining, "the timer must still be running when it is read").toBeGreaterThan(0);
      expect(
        remaining,
        "45 seconds is what Enter committed, and no preset could have produced it",
      ).toBeLessThanOrEqual(45_000);

      // ── PHASE 2: cancel, to get the idle branch (and the custom row) back.
      await openTimerPopover(page);
      await page.locator(CANCEL).click();
      await expect(chip).toHaveCount(0, { timeout: 10_000 });
      await expect(trigger).toHaveAttribute("data-running", "false");

      // ── PHASE 3: Enter from the UNIT select — the Tab-to-unit-then-commit
      // path, which is a dead key without the select's own handler.
      await openTimerPopover(page);
      await expect(value).toBeVisible();
      // The popover is mounted only while open, so the field is a fresh signal
      // on every open: a host who cancels and reopens must not find the previous
      // entry still sitting there.
      await expect(value).toHaveValue("");
      await expect(unit).toHaveValue("minutes");

      await value.pressSequentially("20", { delay: 60 });
      await unit.selectOption("seconds");
      await expect(start).toHaveAttribute("aria-label", "Start a 20 seconds timer");

      // Focus the SELECT and press there. This is the assertion the extra
      // cancel-and-reopen buys.
      await unit.press("Enter");

      await expect(page.locator(POPOVER)).toHaveCount(0, { timeout: 10_000 });
      await expect(trigger).toHaveAttribute("data-running", "true");
      await expect(chip).toBeVisible({ timeout: 15_000 });
      await expect(page.locator(CHIP_VALUE)).toHaveText(/^0:\d{2}$/);
      const secondRemaining = await remainingMs(page, "host");
      expect(secondRemaining, "the second timer must still be running").toBeGreaterThan(0);
      expect(
        secondRemaining,
        "20 seconds is what Enter on the unit select committed",
      ).toBeLessThanOrEqual(20_000);
    } finally {
      await browser.close();
    }
  });
});
