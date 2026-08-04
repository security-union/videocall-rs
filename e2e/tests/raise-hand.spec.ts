/**
 * Raise / lower hand — issue 2135.
 *
 * The issue's load-bearing clause is that peers must see WHO has a hand raised
 * "even if the tile is not been displayed". The surface that answers it is the
 * persistent banner (`[data-testid="raised-hands-banner"]`), which is
 * independent of tile visibility; the tile / roster badges are locality
 * affordances on top. The tests below are ordered by how much of that promise
 * they pin:
 *
 *   1. cross-peer banner  — the requirement itself (wire + attribution + clear)
 *   2. toggle + self surfaces — the control's a11y contract and the local echo
 *   3. shared rendering   — two hands up read IDENTICALLY on both participants
 *   4. departure cleanup  — a leaver's hand is dropped with no wire message
 *   5. late-join re-announce — a hand raised BEFORE you arrived still reaches
 *      you, which is the other half of the "tile not displayed" clause
 *
 * TAGGING: deliberately UNTAGGED (no `@bvt0` / `@bvt1`), matching the sibling
 * two-browser reaction specs in `two-users-meeting.spec.ts`, which carry the
 * same note. bvt is the fast per-PR smoke set and already pays for exactly one
 * two-browser meeting boot; three more would roughly double it. These therefore
 * run ONLY under `--project=dioxus`:
 *
 *     make e2e SPEC=raise-hand
 *
 * and must be validated that way (local docker stack) or via a `/run-e2e`
 * dioxus dispatch on the PR — a per-PR "Playwright bvt1" green does NOT cover
 * them.
 *
 * HARNESS: `enterTwoUserMeeting` / `joinMeetingFromPage` come from
 * `helpers/two-user-meeting.ts` — the same join dance the `@bvt1`
 * "host starts meeting, guest joins, both see each other" test proves. Nothing
 * about the join flow is re-invented here.
 *
 * CAMERA: intentionally NOT seeded camera-on (`vc_prejoin_camera_on`). Every
 * surface asserted here — banner, `.floating-name` tile badge, roster row,
 * self-tile chrome — renders on the camera-off placeholder tile too, and the
 * feature's whole point is that it works when there is no video to look at.
 */

import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { waitForServices } from "../helpers/wait-for-services";
import {
  enterMeetingAsHost,
  enterTwoUserMeeting,
  guestJoinsMeeting,
  joinMeetingFromPage,
} from "../helpers/two-user-meeting";

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

const TRIGGER = '[data-testid="raise-hand-button"]';
const BANNER = '[data-testid="raised-hands-banner"]';
const BANNER_TEXT = '[data-testid="raised-hands-banner"] .raised-hands-banner-text';
const BANNER_COMPACT = '[data-testid="raised-hands-banner"] .raised-hands-banner-compact';
const LIVE_REGION = '[data-testid="raised-hands-live-region"]';
const PEER_TILE_BADGE = '[data-testid="peer-raised-hand-badge"]';
const SELF_TILE_BADGE = '[data-testid="self-raised-hand-badge"]';
const ROSTER_BADGE = '#peer-list-container [data-testid="peer-list-raised-hand"]';
// The OTHER top-centre overlay that can be on screen at the same time as the
// banner. In grid mode it is bottom-anchored above the controls dock; during a
// screen share it moves to the top, which is where it can collide.
const PAUSED_PILL = '[data-testid="decode-paused-pill"]';
const PAUSED_PILL_ACTION = '[data-testid="decode-paused-pill-show-all"]';

// A decode budget of exactly ONE decoded tile. With mock peers present this
// sheds every other tile immediately and DETERMINISTICALLY — no fps injection,
// no control-loop timing — which is what makes the pill reachable without the
// pressure dance decode-budget.spec.ts has to perform. Crucially it also leaves
// `pressured` FALSE, so the decode-budget BANNER never appears and therefore
// never suppresses the pill (the two are mutually exclusive).
const FORCED_BUDGET_SEED = `localStorage.setItem("vc_decode_budget_override", "1");`;

// Mock peers needed to push the shed count above zero. Any number > the forced
// budget works; 8 keeps the SS panel small enough to render quickly.
const SS_MOCK_PEERS = 8;

/**
 * Synthetic `getDisplayMedia`, injected before navigation so the share needs no
 * system picker.
 *
 * ANIMATED on purpose. A static canvas mock re-encodes only on demand
 * (post-#1841), so the viewer can sit waiting for a frame that never arrives and
 * the split layout never activates — a known flake class in this repo. A source
 * that repaints every frame removes it outright.
 */
const MOCK_ANIMATED_DISPLAY_MEDIA = `
  (() => {
    const md = navigator.mediaDevices;
    if (!md) return;
    Object.defineProperty(md, 'getDisplayMedia', {
      configurable: true,
      value: async () => {
        const canvas = document.createElement('canvas');
        canvas.width = 640;
        canvas.height = 480;
        const ctx = canvas.getContext('2d');
        let frame = 0;
        const paint = () => {
          frame += 1;
          ctx.fillStyle = '#1a1a2e';
          ctx.fillRect(0, 0, 640, 480);
          ctx.fillStyle = '#ffffff';
          ctx.font = '24px sans-serif';
          ctx.fillText('Mock Screen Share ' + frame, 120, 240);
          requestAnimationFrame(paint);
        };
        paint();
        return canvas.captureStream(15);
      },
    });
  })();
`;

// The throttled screen-reader region flushes at most once per
// RAISED_HANDS_SR_THROTTLE_MS (4000 ms, `raised_hands.rs`), and the wire send is
// rate-gated / join-wave-coalesced by up to RAISE_HAND_REANNOUNCE_COALESCE_MS
// (750 ms, `raise_hand.rs`). Cross-peer assertions therefore get a window that
// comfortably clears both plus fan-out latency rather than the 10 s default.
const CROSS_PEER_TIMEOUT = 20_000;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Wake the auto-hiding video-controls bar so a subsequent visibility PROBE
 * (`isVisible()`, which takes a snapshot and does not auto-wait) reads the real
 * layout rather than a bar that happens to be mid-hide. Copied from `openDrawer`
 * in drawer-resize.spec.ts, including its reason for deriving the centre point
 * from the measured viewport rather than a fixed (400, 400).
 */
async function wakeControls(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  const vp = page.viewportSize() ?? { width: 800, height: 600 };
  await page.mouse.move(Math.floor(vp.width / 2), Math.floor(vp.height / 2));
  await page.waitForTimeout(300);
}

/**
 * Flip the raise/lower-hand toggle and wait for the local state to settle on
 * `want`.
 *
 * Width-robust in the same shape as `ensureReactionsPaletteOpen` in
 * `two-users-meeting.spec.ts`: the RaiseHand slot is `DEFAULT_SLOTS` index 3 and
 * is NOT sacred (only Mic/Camera/HangUp never overflow), so on a narrow action
 * bar it moves into the "More actions" menu and its own button is hidden. At the
 * 1280x720 Desktop Chrome viewport these specs run at nothing overflows (see
 * action-bar-overflow.spec.ts), so the direct path is the one exercised here.
 *
 * The overflow branch is live: writing this spec surfaced that `attendants.rs`'s
 * overflow `match slot` had no `ActionBarSlot::RaiseHand` arm, so the menu item
 * rendered and did nothing but close the menu. That arm now exists (it calls the
 * same `toggle_raise_hand` handler as the action-bar button). It matters more
 * than it looks: existing users get the new slot APPENDED to the end of their
 * saved action-bar layout by the forward-compat migration, so overflow is their
 * most likely first encounter with the control.
 *
 * Asserting `data-raised` BEFORE and AFTER makes the helper self-checking: every
 * call site declares the state it expects to be in, and a click that goes
 * nowhere — an overflow item that loses its handler again, a `customize_mode`
 * early return — fails here rather than one confusing assertion later.
 * `data-raised` comes from the same `self_hand_raised` signal that drives the
 * button's class, so it is the local truth and not a proxy for it.
 */
async function setHandRaised(page: Page, want: boolean): Promise<void> {
  const trigger = page.locator(TRIGGER);
  await expect(trigger).toHaveAttribute("data-raised", want ? "false" : "true");

  await wakeControls(page);
  if (await trigger.isVisible().catch(() => false)) {
    await trigger.click();
  } else {
    await page.locator("#overflow-menu-trigger").click();
    await page.locator(".overflow-item", { hasText: "Raise hand" }).click();
  }

  await expect(trigger).toHaveAttribute("data-raised", want ? "true" : "false");
}

/**
 * Open the participants roster. Uses the class-based open marker
 * (`#peer-list-container.visible`) that drawer-resize.spec.ts proves, rather
 * than `toBeVisible` — the container div renders in BOTH states, so only the
 * class discriminates open from closed.
 */
async function openRoster(page: Page): Promise<void> {
  await wakeControls(page);
  await page.locator("#peer-list-trigger").click();
  await expect(page.locator("#peer-list-container")).toHaveClass(/visible/, { timeout: 10_000 });
}

/** Close the roster again (the trigger is a toggle). */
async function closeRoster(page: Page): Promise<void> {
  await wakeControls(page);
  await page.locator("#peer-list-trigger").click();
  await expect(page.locator("#peer-list-container")).not.toHaveClass(/visible/, {
    timeout: 10_000,
  });
}

/** Launch a browser + authenticated context + page in one step. */
async function newParticipant(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  email: string,
  name: string,
  uiURL: string,
): Promise<Page> {
  const ctx = await createAuthenticatedContext(browser, email, name, uiURL);
  return ctx.newPage();
}

/**
 * Same, but runs `initScripts` on the context before the first navigation —
 * the only way to seed localStorage / stub a browser API for a page that has
 * not loaded yet.
 */
async function newParticipantWithInit(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  email: string,
  name: string,
  uiURL: string,
  initScripts: string[],
): Promise<Page> {
  const ctx = await createAuthenticatedContext(browser, email, name, uiURL);
  for (const script of initScripts) {
    await ctx.addInitScript(script);
  }
  return ctx.newPage();
}

/**
 * Set the mock-peer count via the Mock Peers popover. Returns false when the
 * control is absent (MOCK_PEERS_ENABLED off in this stack), so the caller can
 * skip rather than fail.
 *
 * Same interaction decode-budget.spec.ts drives, including the "click the grid
 * to dismiss the popover" close. Call it BEFORE a screen share starts: in grid
 * mode that dismiss click lands on empty container, whereas in the split layout
 * the same point is inside the shared-screen pane.
 */
async function setMockPeers(page: Page, count: number): Promise<boolean> {
  await page.locator(".video-controls-container").hover();
  const mockBtn = page
    .locator(".video-controls-container button")
    .filter({ has: page.locator('.tooltip:has-text("Mock Peers")') });

  if ((await mockBtn.count()) === 0) {
    return false;
  }

  await mockBtn.first().click();
  await expect(page.locator(".mock-peers-popover")).toBeVisible({ timeout: 5_000 });
  const input = page.locator("#mock-count-input");
  await input.fill(String(count));
  await input.dispatchEvent("input");
  await page.waitForTimeout(300);

  await page.locator("#grid-container").click({ position: { x: 10, y: 10 } });
  await expect(page.locator(".mock-peers-popover")).not.toBeVisible({ timeout: 3_000 });
  return true;
}

/**
 * Start a screen share on `sharerPage` and resolve true once `viewerPage` has
 * switched to the split layout. False (rather than a failure) when the share
 * could not be established, so the caller can skip.
 *
 * The split layout is a property of the VIEWER, not the sharer: `attendants.rs`
 * skips self when building the screen-share stack, so a local share never puts
 * the sharer's own `#grid-container` into `.has-screen-share`. That is why this
 * takes two pages and why the collision under test can only be observed on the
 * viewer.
 */
async function startScreenShare(sharerPage: Page, viewerPage: Page): Promise<boolean> {
  await wakeControls(sharerPage);
  await sharerPage.waitForTimeout(300);
  const shareButton = sharerPage.locator("button.video-control-button", {
    has: sharerPage.locator(".tooltip", { hasText: "Share Screen" }),
  });
  await expect(shareButton).toBeVisible({ timeout: 10_000 });
  await shareButton.click();

  try {
    await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({ timeout: 20_000 });
    return true;
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Raise hand (issue 2135)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * THE ISSUE'S REQUIREMENT. The host raises; the GUEST — a different browser,
   * a different session — must learn who it was.
   *
   * What a regression here would look like: `send_raise_hand` not putting the
   * packet on the wire, the relay dropping/not stamping RAISE_HAND, the
   * receiving `on_raise_hand` arm self-skipping too aggressively, the display
   * name failing to resolve (a raw email or "Someone" instead of "HostUser"),
   * or `compose_raised_hands_banner` losing its singular-peer branch. The
   * lower half additionally pins that `raised` is treated as an absolute LEVEL:
   * a `false` packet must CLEAR the entry, and the banner must then render
   * nothing at all rather than empty chrome parked over the video.
   *
   * The `You raised your hand` assertion on the host's own page covers a
   * genuinely separate code path — the relay self-skips the sender, so the
   * raiser never receives its own packet and renders from the optimistic local
   * echo in `toggle_raise_hand`.
   */
  test("a peer sees who raised their hand, and lowering clears it everywhere", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_raise_hand_rt_${Date.now()}`;
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

      // ── Arrange: no hand up anywhere, so the banner is ABSENT (not merely
      // hidden) on both pages. The live region is the only thing that renders.
      await expect(hostPage.locator(BANNER)).toHaveCount(0);
      await expect(guestPage.locator(BANNER)).toHaveCount(0);
      await expect(guestPage.locator(LIVE_REGION)).toHaveCount(1);
      // ...and it is SILENT. Entering a meeting where nobody has raised a hand
      // must announce nothing: the banner mounts with an empty roster, and its
      // first throttle window would otherwise speak "All hands lowered" about
      // hands that never went up. Deterministic on the un-fixed code — the join
      // dance above takes far longer than RAISED_HANDS_SR_THROTTLE_MS (4000 ms),
      // so a mount-armed flush has long since fired by the time this runs.
      // Guards `should_announce_roster`'s first clause.
      await expect(guestPage.locator(LIVE_REGION)).toHaveText("");

      // Tag the live region's DOM NODE so the assertion after the raise/lower
      // cycle can tell "diffed in place" from "destroyed and recreated". An
      // expando survives attribute and text updates but not a node replacement.
      //
      // This is the mechanism behind a real defect, not a style preference: the
      // banner used to be two separate `rsx!` templates with different root
      // counts (1 when empty, 2 when not), which makes Dioxus REPLACE rather
      // than diff. The live region was therefore torn down and re-inserted on
      // every empty↔non-empty transition — and re-inserted ALREADY CONTAINING
      // the previous announcement, which several AT/browser pairs speak on
      // insertion. The last hand lowering could announce "1 hand raised.
      // HostUser." at the instant HostUser lowered.
      await guestPage
        .locator(LIVE_REGION)
        .evaluate((el) => ((el as HTMLElement).dataset.identity = "original"));

      // ── Act: the host raises.
      await setHandRaised(hostPage, true);

      // ── Assert: the GUEST is told who, by name, in the persistent banner.
      const guestBanner = guestPage.locator(BANNER);
      await expect(guestBanner).toBeVisible({ timeout: CROSS_PEER_TIMEOUT });
      await expect(guestBanner).toHaveAttribute("data-hand-count", "1");
      await expect(guestPage.locator(BANNER_TEXT)).toHaveText("HostUser raised their hand", {
        timeout: CROSS_PEER_TIMEOUT,
      });
      // Not the raw user_id / email — the same resolver the reaction float uses.
      expect(await guestPage.locator(BANNER_TEXT).textContent()).not.toContain("@");

      // The tile badge is the locality affordance on the same event, attributed
      // to the raiser's relay-stamped session id (the unforgeable anchor).
      const guestTileBadge = guestPage.locator(PEER_TILE_BADGE);
      await expect(guestTileBadge).toHaveCount(1, { timeout: CROSS_PEER_TIMEOUT });
      await expect(guestTileBadge).toHaveAttribute("data-session-id", /^\d+$/);

      // The raiser's OWN banner is the optimistic local echo, with its own copy.
      await expect(hostPage.locator(BANNER)).toBeVisible();
      await expect(hostPage.locator(BANNER)).toHaveAttribute("data-hand-count", "1");
      await expect(hostPage.locator(BANNER_TEXT)).toHaveText("You raised your hand");

      // The throttled screen-reader channel announces CURRENT STATE, not the
      // event: one utterance that names the count AND the raiser.
      await expect(guestPage.locator(LIVE_REGION)).toHaveText("1 hand raised. HostUser.", {
        timeout: CROSS_PEER_TIMEOUT,
      });

      // ── Act: the host lowers.
      await setHandRaised(hostPage, false);

      // ── Assert: the level-false packet clears the guest's roster, and the
      // banner unmounts rather than lingering empty.
      await expect(guestPage.locator(BANNER)).toHaveCount(0, { timeout: CROSS_PEER_TIMEOUT });
      await expect(guestTileBadge).toHaveCount(0);
      await expect(hostPage.locator(BANNER)).toHaveCount(0);
      // A drained roster must be announced EXPLICITLY — clearing a live region
      // announces nothing, so the user would never learn the queue emptied.
      await expect(guestPage.locator(LIVE_REGION)).toHaveText("All hands lowered", {
        timeout: CROSS_PEER_TIMEOUT,
      });

      // ...and it must have been the SAME element throughout. The roster went
      // empty → non-empty → empty above, which is two chances to remount. The
      // expando set before the raise proves the node was diffed, not replaced.
      // Also assert the visible banner is NOT itself a live region: it used to
      // carry `role="status"` + `aria-live="off"`, a self-cancelling pair that
      // advertises a status region which never speaks.
      await expect(guestPage.locator(LIVE_REGION)).toHaveAttribute("data-identity", "original");
      await setHandRaised(hostPage, true);
      await expect(guestPage.locator(BANNER)).toBeVisible({ timeout: CROSS_PEER_TIMEOUT });
      await expect(guestPage.locator(LIVE_REGION)).toHaveAttribute("data-identity", "original");
      await expect(guestPage.locator(BANNER)).not.toHaveAttribute("role", "status");
      await expect(guestPage.locator(BANNER)).not.toHaveAttribute("aria-live", /.*/);
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  /**
   * The control's own contract, plus every SELF-facing surface, in one solo
   * meeting (no peer needed — the local echo is applied on click, not from the
   * wire, so a second browser would prove nothing extra here and cost a boot).
   *
   * What a regression here would look like: the toggle switching to
   * `aria-expanded` (wrong — it has no popup), the accessible name going back to
   * flipping with the state (see below), the self badge being added to
   * `peer_tile.rs` where `display_peers` filters `own_session` out so it would
   * never render, or the roster losing the visible queue ordinal that is the
   * only place "who is next" is legible once the banner collapses names.
   *
   * THE NAME MUST NOT FLIP. This spec previously asserted the opposite — that
   * `aria-label` became "Lower hand" once pressed — with a comment defending it
   * as telling the user what the control does. That is the issue #2123
   * auto-hide-menu defect: the name and `aria-pressed` flip TOGETHER, so raised
   * announced "Lower hand, toggle button, pressed", from which the only reading
   * available is that the hand is DOWN. The name is now the stable noun "Raise
   * hand" in both states and `aria-pressed` is the single source of truth, which
   * is what `DOCK_AUTOHIDE_LABEL` in `attendants.rs` already documents for the
   * same class of control. It is not a marginal path: the live region is
   * throttled to 4 s, so this control's own announcement is the ONLY immediate
   * feedback a screen-reader user gets on their own raise.
   */
  test("the toggle keeps a stable name and carries its state in aria-pressed, and drives every self surface", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_raise_hand_toggle_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newParticipant(browser, "solo@videocall.rs", "SoloHost", uiURL);
      await fillAndSubmitJoinForm(page, meetingId, "SoloHost");
      // Same settle the proven `hostInMeeting` idiom uses before reading the
      // pre-meeting screen's state.
      await page.waitForTimeout(1500);
      expect(await joinMeetingFromPage(page)).toBe("in-meeting");
      await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      const trigger = page.locator(TRIGGER);

      // ── Down: pressed=false and the name describes the action available.
      await expect(trigger).toHaveAttribute("aria-pressed", "false");
      await expect(trigger).toHaveAttribute("data-raised", "false");
      await expect(trigger).toHaveAttribute("aria-label", "Raise hand");
      await expect(page.locator(BANNER)).toHaveCount(0);
      await expect(page.locator(SELF_TILE_BADGE)).toHaveCount(0);

      // ── Up.
      await setHandRaised(page, true);
      await expect(trigger).toHaveAttribute("aria-pressed", "true");
      // The name must NOT flip. Combined with the pressed=true above, a flipping
      // name would announce "Lower hand … pressed" — i.e. tell the user their
      // hand is down at the moment it is up. This assertion is the guard against
      // that regression returning; see the test's doc comment.
      await expect(trigger).toHaveAttribute("aria-label", "Raise hand");
      // WCAG 2.5.3 Label in Name: the visible tooltip title must match the
      // accessible name, so a voice-control user can say what they read.
      await expect(page.locator(`${TRIGGER} .tooltip-title`)).toHaveText("Raise hand");
      // The imperative copy survives where it is additive rather than a name.
      await expect(page.locator(`${TRIGGER} .tooltip-desc`)).toContainText("take it down");

      // Local echo: second-person copy, not "SoloHost raised their hand".
      await expect(page.locator(BANNER)).toBeVisible();
      await expect(page.locator(BANNER)).toHaveAttribute("data-hand-count", "1");
      await expect(page.locator(BANNER_TEXT)).toHaveText("You raised your hand");
      // The compact mobile form is CSS-gated to <=639px, so at this viewport it
      // is in the DOM but not shown — the sentence is what a sighted user reads.
      await expect(page.locator(BANNER_COMPACT)).toBeHidden();

      // Self tile badge lives in the host tile's chrome cluster.
      await expect(page.locator(`.host-tile-chrome ${SELF_TILE_BADGE}`)).toHaveCount(1);

      // Roster row carries the 1-based queue position, as VISIBLE text (a
      // sighted user should not have to count rows) and — with its TOTAL — in
      // the accessible name. "position 1 of 1", not "1 in the queue": the latter
      // reads as a count of people rather than a rank, and a rank without its
      // total tells the user where they are but not in what.
      await openRoster(page);
      const rosterBadge = page.locator(ROSTER_BADGE);
      await expect(rosterBadge).toHaveCount(1, { timeout: 10_000 });
      await expect(rosterBadge).toHaveAttribute("data-hand-position", "1");
      await expect(rosterBadge).toHaveAttribute("data-hand-total", "1");
      await expect(rosterBadge.locator(".raised-hand-badge-position")).toHaveText("1");
      await expect(rosterBadge.locator("svg")).toHaveAttribute(
        "aria-label",
        "Hand raised, position 1 of 1",
      );
      // Close the drawer again so it cannot overlap the action bar.
      await closeRoster(page);

      // ── Down again: both directions, not just the raise.
      await setHandRaised(page, false);
      await expect(trigger).toHaveAttribute("aria-pressed", "false");
      await expect(trigger).toHaveAttribute("aria-label", "Raise hand");
      await expect(page.locator(BANNER)).toHaveCount(0);
      await expect(page.locator(SELF_TILE_BADGE)).toHaveCount(0);
    } finally {
      await browser.close();
    }
  });

  /**
   * NARROW VIEWPORT: the two surfaces that only exist below 639px / on an
   * overflowing action bar. One boot covers both because they need the same
   * viewport.
   *
   * 1. THE MOBILE BANNER MUST NAME SOMEONE. Below 639px the CSS swaps the full
   *    sentence for the compact form. That form used to be a bare count
   *    ("1 hand raised"), so on mobile the banner answered *how many* and never
   *    *who* — on exactly the devices where the roster drawer and the tile
   *    badges are hardest to reach, and "who" is the literal ask in the issue.
   *    It now renders the head of the queue plus "+N".
   *
   * 2. THE OVERFLOW ITEM MUST CARRY ITS STATE. At 400px the RaiseHand slot
   *    overflows into the "More actions" menu, and by the layout migration's
   *    append-at-the-end rule that is where EXISTING users meet the control
   *    first — the highest-traffic path into this feature. The item was a plain
   *    button with a static label, so with the hand up it still announced "Raise
   *    hand, button" while activating it performed a LOWER. It now carries
   *    `aria-pressed` beside its (already stable) noun label.
   *
   * Mutation sensitivity: revert `compose_raised_hands_compact` to
   * `compose_raised_hands_count` and the first assertion goes red; drop the
   * `aria-pressed` attribute from the overflow button and the second does.
   */
  test("on a narrow viewport the banner names the head of the queue and the overflow item carries its state", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_raise_hand_narrow_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newParticipant(browser, "solo@videocall.rs", "SoloHost", uiURL);
      await fillAndSubmitJoinForm(page, meetingId, "SoloHost");
      await page.waitForTimeout(1500);
      expect(await joinMeetingFromPage(page)).toBe("in-meeting");
      await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Same 400x720 the action-bar overflow spec proves forces overflow, and
      // comfortably inside the <=639px banner breakpoint.
      await page.setViewportSize({ width: 400, height: 720 });
      await page.waitForTimeout(500);

      const trigger = page.locator(TRIGGER);
      await expect(trigger).toHaveAttribute("data-raised", "false");
      // Precondition, not decoration: if the bar did NOT overflow here, the
      // overflow assertions below would silently pass against nothing.
      await expect(trigger).toBeHidden();

      // ── Raise from the overflow menu — the path this test exists for.
      await setHandRaised(page, true);

      // ── 1. The compact banner names the raiser. The full sentence is the one
      // hidden now, so this is the ONLY text a mobile user gets.
      await expect(page.locator(BANNER)).toBeVisible();
      await expect(page.locator(BANNER_TEXT)).toBeHidden();
      const compact = page.locator(BANNER_COMPACT);
      await expect(compact).toBeVisible();
      // Second person for our own hand — "SoloHost +0" would read as a stranger.
      await expect(compact).toHaveText("You");

      // ── 2. The overflow item reflects the state it can change.
      await wakeControls(page);
      await page.locator("#overflow-menu-trigger").click();
      const overflowItem = page.locator(".overflow-item", { hasText: "Raise hand" });
      await expect(overflowItem).toHaveAttribute("aria-pressed", "true");
      // The NAME stays the stable noun; `aria-pressed` alone carries the state.
      // A label that flipped to "Lower hand" here would cancel the pressed state
      // out, exactly as it did on the action-bar button (issue #2123).
      await expect(overflowItem).toHaveText("Raise hand");
      await page.keyboard.press("Escape");

      // ── Down again, and the state follows in both directions.
      await setHandRaised(page, false);
      await expect(page.locator(BANNER)).toHaveCount(0);
      await wakeControls(page);
      await page.locator("#overflow-menu-trigger").click();
      await expect(page.locator(".overflow-item", { hasText: "Raise hand" })).toHaveAttribute(
        "aria-pressed",
        "false",
      );
    } finally {
      await browser.close();
    }
  });

  /**
   * With two hands up, both participants must render the same ORDER — that is
   * what makes the banner a shared "who is next" rather than two clients quietly
   * disagreeing — but each in their OWN voice.
   *
   * This assertion is deliberately inverted from the one it replaces. The
   * earlier version demanded a byte-identical sentence on both pages and its doc
   * called rendering the local participant as "You" the "obvious way to improve
   * the copy" that must be resisted. The UX review of issue 2135 established the
   * opposite, and it is right: with `join_names` spelling the reader's resolved
   * display name, your own banner read "Antonio and Alice raised their hands" —
   * your own name, in the third person, on your own screen. Byte-identity across
   * peers was never the product requirement; a shared ORDER was, and that is
   * what survives here.
   *
   * So the guest sees "HostUser and You" and the host sees "You and GuestUser":
   * different strings, one queue, host first on both. The possessive follows the
   * subject — "your hands" once the reader is one of the names spelled out
   * (`compose_raised_hands_banner`), because "You and GuestUser raised THEIR
   * hands" is not English.
   *
   * What this pins that no unit test can: the two hands reach each page from
   * DIFFERENT sources (the host applies its own locally and the guest's over the
   * wire; the guest the reverse), and both must still agree on the order. It
   * also pins that a second raise ADDS to the roster rather than replacing the
   * first (count 2).
   *
   * What this deliberately does NOT pin: `sort_raised_hands`. With two
   * participants raising in a strict, observed sequence, arrival order and raise
   * order coincide on BOTH pages, so an unsorted list would still render this
   * expectation. Raise-time ordering, the session-id tie-break, and re-raise
   * re-ordering are covered by the `#[test]`s in `raised_hands.rs`, which can
   * feed arrival orders a two-browser harness cannot produce.
   */
  test("both participants render the same two-hand order, each in their own voice", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_raise_hand_order_${Date.now()}`;
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

      // Host raises FIRST, and we wait until the guest has actually SEEN it
      // before the guest raises. That serialisation is what makes the expected
      // sentence deterministic: the two `raised_at_ms` stamps are unambiguously
      // ordered, so the assertion never depends on the session-id tie-break.
      await setHandRaised(hostPage, true);
      await expect(guestPage.locator(BANNER)).toHaveAttribute("data-hand-count", "1", {
        timeout: CROSS_PEER_TIMEOUT,
      });

      // Guest raises SECOND.
      await setHandRaised(guestPage, true);

      // Host raised first, so HostUser leads on BOTH pages — that is the
      // shared property. Only the reader's own entry differs.
      for (const [who, page, sentence] of [
        ["guest", guestPage, "HostUser and You raised your hands"],
        ["host", hostPage, "You and GuestUser raised your hands"],
      ] as const) {
        await expect(page.locator(BANNER), `${who} banner shows two hands`).toHaveAttribute(
          "data-hand-count",
          "2",
          { timeout: CROSS_PEER_TIMEOUT },
        );
        await expect(page.locator(BANNER_TEXT), `${who} banner order`).toHaveText(sentence, {
          timeout: CROSS_PEER_TIMEOUT,
        });
        // Nobody is ever shown their own display name in the third person.
        expect(
          await page.locator(BANNER_TEXT).textContent(),
          `${who} must not be named in its own banner`,
        ).not.toContain(who === "guest" ? "GuestUser" : "HostUser");
      }
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  /**
   * A participant who LEAVES with their hand up must not leave it up forever.
   *
   * This is the highest-risk path in the feature because there is no wire
   * message to lean on: a departing client broadcasts nothing and the relay
   * holds no hand registry, so the remaining participant must clear the entry
   * locally off `PARTICIPANT_LEFT` (`OnPeerLeftAction::ClearRaisedHand`). Delete
   * that action — or emit it AFTER the mid-reconnect early return — and the
   * banner keeps naming someone who is no longer in the meeting, with no user
   * action able to clear it.
   */
  test("a participant who leaves with their hand up is dropped from the banner", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_raise_hand_leave_${Date.now()}`;
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

      // Guest raises; the host must see it before we can prove it got cleared.
      await setHandRaised(guestPage, true);
      await expect(hostPage.locator(BANNER)).toBeVisible({ timeout: CROSS_PEER_TIMEOUT });
      await expect(hostPage.locator(BANNER_TEXT)).toHaveText("GuestUser raised their hand", {
        timeout: CROSS_PEER_TIMEOUT,
      });
      await expect(hostPage.locator(PEER_TILE_BADGE)).toHaveCount(1);

      // Guest hangs up. Same control guest-leave.spec.ts drives: the toolbar's
      // only `.danger` button, which has no title (its tooltip is a child span).
      await wakeControls(guestPage);
      const hangUp = guestPage.locator("button.video-control-button.danger").first();
      await expect(hangUp).toBeVisible({ timeout: 10_000 });
      await hangUp.click();
      await expect(guestPage).toHaveURL("/", { timeout: 15_000 });

      // The host clears the hand purely from PARTICIPANT_LEFT. Same reconnect
      // grace window guest-leave.spec.ts allows for the tile itself.
      await expect(hostPage.locator(BANNER)).toHaveCount(0, { timeout: 30_000 });
      await expect(hostPage.locator(PEER_TILE_BADGE)).toHaveCount(0);
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  /**
   * A participant who joins AFTER a hand went up must still be told about it.
   *
   * This is the join-wave RE-ANNOUNCE, and it is the second half of the issue's
   * "even if the tile is not been displayed" clause: the hand you most need
   * surfaced is the one that went up before you arrived. Nothing else can
   * deliver it — the relay keeps no hand registry and `RaisedHandsCtx` is
   * documented as purely live ("Nothing seeds it from a roster"), so the ONLY
   * path from the host's raised hand to a late joiner's banner is the host's
   * `on_peer_joined` handler re-sending its current level.
   *
   * Mutation sensitivity (deterministic, not probabilistic): delete the
   * `drive_raise_hand(..., RaiseHandTrigger::ReAnnounce)` call in
   * `on_peer_joined`, or make `RaiseHandAnnouncer::request` answer `Skip` for a
   * raised hand on the ReAnnounce trigger (drop the `self.raised &&
   * self.reannounce_wanted` clause from `has_work`), and the guest's banner
   * never appears at all. There is no timing window in which it passes anyway.
   *
   * WHAT THIS DOES **NOT** PIN — and the reason no cheap variant of it does.
   * It is tempting to read the final two-hand assertion as proof that a
   * re-announce carries the ORIGINAL `raised_at_ms` rather than re-stamping at
   * re-announce time. It does not, and the ordering here would be identical
   * either way: the re-announce necessarily fires at the guest's JOIN, which is
   * before the guest can raise, so a re-stamped host timestamp is still smaller
   * than the guest's and the queue still reads [Host, Guest]. Making the two
   * disagree needs either a third browser — where both peers re-announce inside
   * the same ~750 ms coalesce window, so a re-stamped order is decided by the
   * session-id tie-break and the mutation is caught only about half the time —
   * or the guest raising before the re-announce lands, an uncontrolled race
   * whose losing side is a silent false green. Neither clears the "must fail if
   * you break the thing it names" bar, so `raised_at_ms` preservation stays with
   * the `#[test]`s in `raise_hand.rs`, which can set the clock directly.
   *
   * The two-hand assertion is kept anyway, as a cheap consistency check that
   * both pages still agree in the late-join topology — not as an ordering gate.
   */
  test("a participant who joins after a hand went up is still told about it", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_raise_hand_latejoin_${Date.now()}`;
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

      // ── Host raises while ALONE. No peer exists to receive the live packet,
      // so nothing the guest later sees can have come from this send.
      await enterMeetingAsHost(hostPage, meetingId);
      await setHandRaised(hostPage, true);
      await expect(hostPage.locator(BANNER_TEXT)).toHaveText("You raised your hand");

      // ── Guest joins only now. Resolves once both see each other's canvas, so
      // the host's `on_peer_joined` has fired by the time this returns.
      await guestJoinsMeeting(hostPage, guestPage, meetingId);

      // ── The re-announce is the only way this can be true.
      const guestBanner = guestPage.locator(BANNER);
      await expect(guestBanner).toBeVisible({ timeout: CROSS_PEER_TIMEOUT });
      await expect(guestBanner).toHaveAttribute("data-hand-count", "1");
      await expect(guestPage.locator(BANNER_TEXT)).toHaveText("HostUser raised their hand", {
        timeout: CROSS_PEER_TIMEOUT,
      });
      await expect(guestPage.locator(PEER_TILE_BADGE)).toHaveCount(1, {
        timeout: CROSS_PEER_TIMEOUT,
      });

      // ── A hand raised after arriving queues behind the one already up, and
      // both pages agree on the result.
      await setHandRaised(guestPage, true);
      // Same queue on both pages, each reader in the second person — see
      // "both participants render the same two-hand order, each in their own
      // voice" for why these two strings differ.
      await expect(guestPage.locator(BANNER_TEXT)).toHaveText(
        "HostUser and You raised your hands",
        { timeout: CROSS_PEER_TIMEOUT },
      );
      await expect(hostPage.locator(BANNER_TEXT)).toHaveText(
        "You and GuestUser raised your hands",
        { timeout: CROSS_PEER_TIMEOUT },
      );
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  /**
   * THE SCREEN-SHARE COLLISION (issue 2135 UX review, blocker).
   *
   * `.raised-hands-banner` and, during a screen share, `.decode-paused-pill` are
   * both pinned top-centre at `top: 12px`. The sibling rule that separates the
   * banner from the other top-centre overlay named only `.decode-budget-banner`,
   * so the pill was never pushed down: with any hand raised during a screen
   * share the banner (z-index 61) painted straight over the pill (z-index 55).
   *
   * The failure mode is worse than a plain occlusion. The banner is
   * `pointer-events: none`, so the pill's "Show all" button stayed fully
   * CLICKABLE while completely invisible — an affordance the user cannot see,
   * cannot find, and can only hit by accident. Being hidden outright would have
   * been better.
   *
   * WHY THIS IS A GEOMETRY ASSERTION AND NOT `elementFromPoint`. Hit-testing
   * cannot see this bug: `pointer-events: none` takes the banner OUT of hit
   * testing, so `document.elementFromPoint` over the pill's button returns the
   * button whether or not the banner is drawn on top of it. It would pass on the
   * broken CSS. Rect containment is what actually distinguishes the two states,
   * so that is what is asserted.
   *
   * WHY IT BITES: on the un-fixed stylesheet both elements resolve to
   * `top: 12px`, so the pill's top (12) is far above the banner's bottom (~48)
   * and the `toBeGreaterThanOrEqual` fails. Revert the `.decode-paused-pill`
   * arm of the sibling rule in `style.css` and this test goes red; the
   * ~16px separation the fix produces leaves no room for a sub-pixel pass.
   *
   * HARNESS NOTES:
   *  - The split layout is a property of the VIEWER (`attendants.rs` skips self
   *    when building the screen-share stack), so the host shares and every
   *    assertion runs on the guest.
   *  - The pill is reached WITHOUT the fps-injection pressure dance
   *    decode-budget.spec.ts performs: a seeded `Fixed(1)` decode budget plus
   *    mock peers sheds tiles deterministically, and because `pressured` stays
   *    false the decode-budget banner never appears to suppress the pill.
   *  - `.has-screen-share` is asserted explicitly. Without that guard a run that
   *    silently stayed in grid mode would pass trivially — the pill is
   *    bottom-anchored there and can never collide — i.e. the test would report
   *    success for the one reason it must not.
   */
  test("during a screen share the banner does not cover the decode-paused pill", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_raise_hand_ss_pill_${Date.now()}`;
    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      // The host is the sharer, so only it needs the display-media stub.
      const hostPage = await newParticipantWithInit(
        hostBrowser,
        "host@videocall.rs",
        "HostUser",
        uiURL,
        [MOCK_ANIMATED_DISPLAY_MEDIA],
      );
      // The guest is the viewer: it carries the forced decode budget, sees the
      // split layout, shows the pill, and raises the hand.
      const guestPage = await newParticipantWithInit(
        guestBrowser,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
        [FORCED_BUDGET_SEED],
      );

      await enterTwoUserMeeting(hostPage, guestPage, meetingId);

      // Mock peers BEFORE the share: the popover's dismiss click targets the
      // grid container, which is empty space in grid mode and the shared-screen
      // pane once the split layout is up.
      if (!(await setMockPeers(guestPage, SS_MOCK_PEERS))) {
        test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize shed tiles");
        return;
      }

      if (!(await startScreenShare(hostPage, guestPage))) {
        test.skip(true, "Screen share could not be established; getDisplayMedia stub ineffective");
        return;
      }

      // The regime the bug lives in. Assert it before anything else, so a run
      // that never entered the split layout fails HERE rather than passing a
      // geometry check the grid layout satisfies for free.
      await expect(guestPage.locator("#grid-container")).toHaveClass(/has-screen-share/, {
        timeout: 15_000,
      });

      // Fixed(1) + 8 mock peers means shed tiles from the first render; the pill
      // still owes PILL_APPEAR_MS (2 s) of sustained non-zero count plus its
      // 1 Hz publish poll before it surfaces.
      const pill = guestPage.locator(PAUSED_PILL);
      await expect(pill).toBeVisible({ timeout: 30_000 });

      // Now put a hand up. Nothing about the pill changes — the raised-hands
      // banner is not the decode-budget banner and does not suppress it.
      await setHandRaised(guestPage, true);
      const banner = guestPage.locator(BANNER);
      await expect(banner).toBeVisible({ timeout: 10_000 });
      await expect(pill).toBeVisible();

      // Both surfaces animate in (the banner lifts 10px, the pill drops 2px), so
      // let the 0.3 s entrances finish before reading boxes. The transients are
      // too small to flip this assertion in either direction, but measuring a
      // moving element is a flake waiting to be rediscovered.
      await guestPage.waitForTimeout(500);

      const bannerBox = await banner.boundingBox();
      const pillBox = await pill.boundingBox();
      expect(bannerBox, "banner must have a layout box").not.toBeNull();
      expect(pillBox, "pill must have a layout box").not.toBeNull();

      // THE ASSERTION. The pill starts at or below where the banner ends.
      expect(
        pillBox!.y,
        "the paused pill must start below the raised-hands banner, not underneath it",
      ).toBeGreaterThanOrEqual(bannerBox!.y + bannerBox!.height);

      // And the affordance the collision hid — the pill's action button — is
      // clear of the banner's rect too, stated separately because it is the part
      // a user actually loses.
      const action = guestPage.locator(PAUSED_PILL_ACTION);
      await expect(action).toBeVisible();
      const actionBox = await action.boundingBox();
      expect(actionBox, "pill action must have a layout box").not.toBeNull();
      expect(
        actionBox!.y,
        "the pill's Show all button must not sit under the banner",
      ).toBeGreaterThanOrEqual(bannerBox!.y + bannerBox!.height);
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});
