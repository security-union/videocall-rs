import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { openPeerList } from "../helpers/controls";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

const KICK_MESSAGE = "You have been removed from the meeting by the host.";
/** `meeting_ended_overlay.rs`: the card inside `.meeting-ended-overlay`. */
const MEETING_ENDED_CARD = '.meeting-ended-overlay [role="alertdialog"]';
/** `meeting_ended_overlay.rs`: the overlay root (`class: "glass-backdrop meeting-ended-overlay"`). */
const MEETING_ENDED_OVERLAY = ".meeting-ended-overlay";
/** `meeting_ended_overlay.rs`: the card's only button ("Return to Home"). */
const MEETING_ENDED_HOME_BTN = "button.meeting-ended-home-btn";
const PEER_LIST = "#peer-list-container";
const FOOTER_TRIGGER = "#meeting-footer-trigger";
const MEETING_INFO_DIALOG = "#meeting-info-dialog";

// Focus must land on the CARD, not on "Return to Home": the chat composer sends
// on Enter, so a user mid-sentence must not have their next Enter navigate.
async function expectOverlayOwnsFocus(page: Page, message: string, when: string): Promise<void> {
  const card = page.locator(MEETING_ENDED_CARD);
  await expect(card, `${when}: the overlay card must be an alertdialog`).toHaveCount(1, {
    timeout: 20_000,
  });
  await expect(card).toHaveAttribute("aria-modal", "true");
  await expect(card).toHaveAttribute("aria-labelledby", "meeting-ended-title");
  await expect(card).toHaveAttribute("aria-describedby", "meeting-ended-message");
  await expect(card).toHaveAttribute("tabindex", "-1");
  // Neither id the card names may be a dangling reference.
  await expect(card.locator("#meeting-ended-title")).toHaveText("Meeting Ended");
  await expect(card.locator("#meeting-ended-message")).toHaveText(message);

  await expect
    .poll(
      () =>
        page.evaluate((selector) => {
          const overlayCard = document.querySelector(selector);
          if (!overlayCard) {
            throw new Error("the meeting-ended overlay card is not in the DOM");
          }
          const active = document.activeElement;
          if (!active) {
            throw new Error("the document reports no activeElement");
          }
          const name = active.tagName.toLowerCase() + (active.id ? `#${active.id}` : "");
          if (active === overlayCard) {
            return "the overlay card";
          }
          return overlayCard.contains(active) ? `inside the card: ${name}` : name;
        }, MEETING_ENDED_CARD),
      { timeout: 15_000, message: `${when}: the overlay must hold focus` },
    )
    .toBe("the overlay card");
}

/** Where `document.activeElement` sits relative to the ended-meeting overlay. */
type OverlayFocus = {
  where: string;
  inOverlay: boolean;
  onFooterTrigger: boolean;
};

async function readOverlayFocus(page: Page): Promise<OverlayFocus> {
  return page.evaluate(
    (sel: { overlay: string; homeBtn: string; trigger: string }) => {
      const active = document.activeElement;
      if (!active) {
        return { where: "no activeElement", inOverlay: false, onFooterTrigger: false };
      }
      const name = active.tagName.toLowerCase() + (active.id ? `#${active.id}` : "");
      const inOverlay = active.closest(sel.overlay) !== null;
      const where = active.matches(sel.homeBtn)
        ? "the Home button"
        : inOverlay
          ? `inside the overlay: ${name}`
          : `outside the overlay: ${name}`;
      return { where, inOverlay, onFooterTrigger: active.matches(sel.trigger) };
    },
    { overlay: MEETING_ENDED_OVERLAY, homeBtn: MEETING_ENDED_HOME_BTN, trigger: FOOTER_TRIGGER },
  );
}

/**
 * The card's Tab-only `onkeydown` (`meeting_ended_overlay.rs` ->
 * `trap_tab_in_dialog`) cycles Tab and Shift+Tab onto the card's one button.
 */
async function expectHomeButtonKeepsFocus(page: Page, when: string): Promise<void> {
  await expect
    .poll(() => readOverlayFocus(page).then((focus) => focus.where), {
      timeout: 5_000,
      message: `${when}: the Tab trap must park focus on the overlay's Home button`,
    })
    .toBe("the Home button");

  const focus = await readOverlayFocus(page);
  expect(focus.inOverlay, `${when}: focus must stay inside ${MEETING_ENDED_OVERLAY}`).toBe(true);
  expect(
    focus.onFooterTrigger,
    `${when}: ${FOOTER_TRIGGER} sits under the z-9999 overlay and must not take focus`,
  ).toBe(false);
}

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });
  await page.waitForTimeout(1500);
}

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting" || result === "waiting-for-meeting") {
    return result;
  }

  if (result === "auto-joined") {
    return "in-meeting";
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);

  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

/**
 * Admit a guest from the waiting room if needed.
 * Returns once the guest is fully in the meeting (grid visible).
 */
async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult === "in-meeting") {
    return;
  }

  if (guestResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.click();
    await hostPage.waitForTimeout(3000);

    const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
    const guestGrid = guestPage.locator("#grid-container");

    const postAdmit = await Promise.race([
      guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
      guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);

    if (postAdmit === "join-button") {
      await guestPage.waitForTimeout(1000);
      await guestJoinButton.click();
      await guestPage.waitForTimeout(3000);
      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    }
  }
}

/**
 * Open the per-tile host actions menu on a remote peer and click the inner
 * "Remove from meeting" item.
 *
 * The per-tile menu is rendered inside the canvas grid (see
 * `canvas_generator.rs`). Unlike Mute / Disable-video — which are gated on
 * the peer's audio/video state — the "Remove from meeting" item is rendered whenever the
 * viewer is the host and the peer is not themselves (see `peer_tile.rs`:
 * `is_current_user_host && !is_self_peer`). The toggle
 * (`title="Host actions"`, class `.tile-mute-btn`) is hidden via
 * `visibility: hidden` until the parent `.grid-item` is hovered, so the test
 * must hover before interacting.
 *
 * The flow is two-step:
 *   1. Click the toggle to open the tile context menu.
 *   2. Click the inner "Remove from meeting" item (a `.tile-context-menu-item`),
 *      which invokes the host kick action — the server
 *      sets the participant's DB status to `'kicked'` and publishes a
 *      `PARTICIPANT_KICKED` NATS event.
 *
 * We scope to a `.grid-item` that contains a `.tile-mute-btn` to avoid
 * matching the host's own tile (which never renders the host-actions button).
 */
async function hostKickPeerViaTile(page: Page): Promise<void> {
  const guestTile = page.locator(".grid-item:has(.tile-mute-btn)").first();
  await expect(guestTile).toBeVisible({ timeout: 15_000 });

  // Hover to reveal `.tile-mute-btn` (CSS sets visibility:hidden until
  // `.grid-item:hover`).
  await guestTile.hover();

  const hostActionsToggle = guestTile.getByTitle("Host actions");
  await expect(hostActionsToggle).toBeVisible({ timeout: 15_000 });
  await hostActionsToggle.click();

  // Match by text inside
  // `.tile-context-menu-item`.
  const kickItem = guestTile.locator(".tile-context-menu-item", {
    hasText: "Remove from meeting",
  });
  await expect(kickItem).toBeVisible({ timeout: 5_000 });
  await kickItem.click();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Host kick controls", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Test 1: Host removes a participant via the per-tile three-dot menu.
   *
   * Unlike Mute / Disable-video, the "Remove from meeting" item is NOT gated on the peer's
   * audio/video state — it is rendered as soon as the viewer is the host and
   * the tile is not their own. So we do not need to enable mic/camera on the
   * guest before exercising the menu.
   *
   * When the host clicks "Remove from meeting": the server sets the participant's DB status to
   * `'kicked'` and publishes a `PARTICIPANT_KICKED` NATS event. The kicked
   * participant's UI receives the event and shows the `MeetingEndedOverlay`
   * with the kicked-by-host message.
   */
  test("host removes a participant", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostkick_single_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-kick@videocall.rs",
        "KickHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-kick@videocall.rs",
        "KickGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "KickHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "KickGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the peer connection to establish (host sees guest's tile).
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- Host opens the tile menu and clicks "Remove from meeting" ----
      await hostKickPeerViaTile(hostPage);

      // ---- Guest receives PARTICIPANT_KICKED: client disconnects immediately
      // and the MeetingEndedOverlay is shown. ----
      // The overlay's root element carries the `meeting-ended-overlay` class
      // (see `meeting_ended_overlay.rs`).
      const guestKickedOverlay = guestPage.locator(".meeting-ended-overlay");
      await expect(guestKickedOverlay).toBeVisible({ timeout: 20_000 });

      // The overlay's message paragraph carries the configured text.
      await expect(
        guestPage.locator(".meeting-ended-message", { hasText: KICK_MESSAGE }),
      ).toBeVisible({ timeout: 5_000 });

      // The overlay sits above the in-call controls — verify the "Return to
      // Home" button (only present on the overlay) is interactable, confirming
      // the overlay is the top-most interactive layer.
      await expect(guestPage.locator("button.meeting-ended-home-btn")).toBeVisible({
        timeout: 5_000,
      });

      // ...and the top-most FOCUS layer too.
      await expectOverlayOwnsFocus(guestPage, KICK_MESSAGE, "when the overlay appears");

      // ---- Host's grid loses the kicked guest's tile: on_peer_left fires on
      // the host session once the server broadcasts PEER_LEFT. Without this
      // assertion a regression in the PEER_LEFT broadcast path would pass
      // silently (the overlay check only verifies the guest-side path). ----
      await expect(hostPage.locator(".grid-item:has(.tile-mute-btn)")).toHaveCount(0, {
        timeout: 20_000,
      });

      // ---- Host does NOT see the kicked overlay (host is never kicked by
      // their own kick action — server rejects user_id == caller). ----
      await expect(hostPage.locator(".meeting-ended-overlay")).toHaveCount(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 2: A kicked participant can rejoin the meeting by navigating back to
   * the meeting URL — they go through the normal join flow.
   *
   * Implementation note: the overlay's "Return to Home" button navigates the
   * browser to `/`, so we click that and then re-navigate to the meeting via
   * `navigateToMeeting` (the same flow a real user would take).
   */
  test("kicked participant can rejoin", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostkick_rejoin_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-kickrejoin@videocall.rs",
        "KickRejoinHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-kickrejoin@videocall.rs",
        "KickRejoinGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "KickRejoinHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "KickRejoinGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the peer connection (host sees guest's tile).
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- Host kicks the guest ----
      await hostKickPeerViaTile(hostPage);

      // ---- Guest sees the kicked overlay ----
      const guestKickedOverlay = guestPage.locator(".meeting-ended-overlay");
      await expect(guestKickedOverlay).toBeVisible({ timeout: 20_000 });
      await expect(
        guestPage.locator(".meeting-ended-message", {
          hasText: "You have been removed from the meeting by the host.",
        }),
      ).toBeVisible({ timeout: 5_000 });

      // ---- Guest clicks "Return to Home" on the overlay ----
      // The button navigates the browser to `/`, which is the same flow a
      // real user would take to leave and try again.
      const returnHomeBtn = guestPage.locator("button.meeting-ended-home-btn");
      await expect(returnHomeBtn).toBeVisible({ timeout: 5_000 });
      await returnHomeBtn.click();

      // Wait for the home page to load.
      await expect(guestPage).toHaveURL(new RegExp(`^${uiURL}/?$`), { timeout: 10_000 });
      await guestPage.waitForTimeout(1500);

      // ---- Guest re-navigates to the meeting via the normal join flow ----
      await navigateToMeeting(guestPage, meetingId, "KickRejoinGuest");
      const rejoinResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, rejoinResult);

      // ---- Guest is back in the meeting (grid container visible again) ----
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 20_000 });

      // ---- The kicked overlay must not be present after a successful rejoin ----
      await expect(guestPage.locator(".meeting-ended-overlay")).toHaveCount(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 3: The host's own tile never shows the kick (or any host-actions)
   * button — `is_self_peer` is true for the host's own tile, which suppresses
   * the entire `.tile-mute-menu-wrapper` (and its `.tile-mute-btn`).
   *
   * With only the host in the meeting, there should be zero `.tile-mute-btn`
   * elements anywhere on the page.
   */
  test("host tile does not show kick button for self", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostkick_self_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-removefromcallself@videocall.rs",
        "RemoveFromCallSelfHost",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();

      // ---- Host joins alone ----
      await navigateToMeeting(hostPage, meetingId, "KickSelfHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // The host's own session is excluded from display_peers, so when
      // alone in the meeting no tiles render — the invite overlay appears.
      await expect(hostPage.locator("#invite-overlay")).toBeVisible({ timeout: 30_000 });
      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(0);
      await expect(hostPage.locator(".grid-item")).toHaveCount(0);

      // With zero tiles rendered, there must be zero host-actions buttons.
      await expect(hostPage.locator(".tile-mute-btn")).toHaveCount(0);
      await expect(hostPage.getByTitle("Host actions")).toHaveCount(0);
    } finally {
      await browser1.close();
    }
  });

  /**
   * Test 4: `MeetingInfoDialog` is gated on
   * `is_active: meeting_ended_message().is_none()` (`attendants.rs`), so an
   * ended meeting unmounts the dialog and the element that held focus goes
   * with it. The peer list is open so the Escape that follows has a visible
   * target: with no panel open `esc_panel_close_target` returns `None` and the
   * assertion would hold whatever the overlay did with the event.
   */
  test("the meeting-ended overlay takes focus from the Meeting info dialog and swallows Escape", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostkick_focus_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-kickfocus@videocall.rs",
        "KickFocusHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-kickfocus@videocall.rs",
        "KickFocusGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "KickFocusHost");
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "KickFocusGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- Guest opens the peer list, then the Meeting info dialog ----
      const peerList = guestPage.locator(PEER_LIST);
      await openPeerList(guestPage);

      await expect(guestPage.locator(FOOTER_TRIGGER)).toBeVisible({ timeout: 15_000 });
      await guestPage.locator(FOOTER_TRIGGER).click({ timeout: 10_000 });
      const infoDialog = guestPage.locator(MEETING_INFO_DIALOG);
      await expect(infoDialog).toBeVisible({ timeout: 5_000 });
      await expect(infoDialog, "the dialog holds focus before the meeting ends").toBeFocused({
        timeout: 5_000,
      });
      await expect(peerList, "the peer list stays open behind the dialog").toHaveClass(
        /\bvisible\b/,
      );

      // ---- Host kicks the guest ----
      await hostKickPeerViaTile(hostPage);

      // The dialog unmounts with the meeting, so the focused element is gone.
      await expect(infoDialog, "the ended meeting unmounts the dialog").toHaveCount(0, {
        timeout: 20_000,
      });
      await expect(
        guestPage.locator('[data-testid="meeting-footer-ended"]'),
        "the footer line flips to Ended",
      ).toBeVisible({ timeout: 10_000 });

      await expectOverlayOwnsFocus(
        guestPage,
        KICK_MESSAGE,
        "when the meeting ends under the dialog",
      );

      // ---- Escape must not reach the meeting view behind the overlay ----
      await expect(peerList, "the peer list is the Escape chain's target").toHaveClass(
        /\bvisible\b/,
      );
      await guestPage.keyboard.press("Escape");
      await guestPage.waitForTimeout(500); // window for an unwanted close to render
      await expect(
        peerList,
        "Escape in the overlay must not close the panel behind it",
      ).toHaveClass(/\bvisible\b/);
      await expectOverlayOwnsFocus(guestPage, KICK_MESSAGE, "after Escape");

      // ---- Tab must not walk out of the overlay either ----
      // `meeting_footer.rs` renders `#meeting-footer-trigger` whether or not
      // the meeting is active, so it stays mounted and tabbable under the
      // z-9999 overlay. That is the premise of the checks below: without a
      // live trigger, "Tab never reached it" would hold trivially.
      await expect(
        guestPage.locator(FOOTER_TRIGGER),
        "the footer trigger stays mounted under the overlay, so Tab has somewhere to escape to",
      ).toHaveCount(1);

      // The card is `tabindex="-1"`, so the first Tab reaches the Home button
      // either way. The second Tab and the Shift+Tab are what the card's
      // `onkeydown` decides: delete it and both walk off the button into the
      // meeting view behind the overlay.
      await guestPage.keyboard.press("Tab");
      await expectHomeButtonKeepsFocus(guestPage, "after the first Tab inside the overlay");

      await guestPage.keyboard.press("Tab");
      await expectHomeButtonKeepsFocus(guestPage, "after a second Tab inside the overlay");

      await guestPage.keyboard.press("Shift+Tab");
      await expectHomeButtonKeepsFocus(guestPage, "after Shift+Tab inside the overlay");

      // The Home button is left focused but NEVER activated: its onclick sets
      // `location.href = "/"`, which would navigate the page mid-test.
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
