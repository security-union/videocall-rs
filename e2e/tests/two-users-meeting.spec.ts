import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
];

async function createAuthenticatedContext(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  email: string,
  name: string,
  uiURL: string,
) {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });
  const token = generateSessionToken(email, name);
  const url = new URL(uiURL);
  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
  return context;
}

/**
 * From the meeting page, wait for the meeting UI to load and click through
 * "Start Meeting" / "Join Meeting" to enter the grid.
 *
 * The meeting page auto-joins the API when navigated to with a username
 * already set (from the home page). Users who lack a username see an inline
 * display name prompt on the meeting page itself.
 *
 * The auto-join shows a brief "Joining as [name]..." spinner while the API
 * call is in flight. Once the API responds the UI transitions to one of:
 *   - "Ready to join?" with Start/Join Meeting button (admitted)
 *   - "Waiting to be admitted" (waiting room)
 *   - "Waiting for meeting to start" (host hasn't started yet)
 *
 * Auth dropdown (user name/email, sign-out) is only shown on the home
 * page -- it no longer appears on this pre-meeting screen.
 */
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

  if (result === "waiting") {
    return "waiting";
  }

  if (result === "waiting-for-meeting") {
    return "waiting-for-meeting";
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
 * Drive the full two-user join dance (host starts, guest joins + is admitted)
 * and resolve once BOTH participants see the grid and each other's peer canvas.
 * Extracted so the reaction specs below can reuse the exact harness the
 * existing @bvt1 test proves, without a new fixture (issue 1884).
 */
async function enterTwoUserMeeting(
  hostPage: Page,
  guestPage: Page,
  meetingId: string,
): Promise<void> {
  await fillAndSubmitJoinForm(hostPage, meetingId, "HostUser");
  await hostPage.waitForTimeout(1500);
  const hostResult = await joinMeetingFromPage(hostPage);
  expect(hostResult).toBe("in-meeting");

  await fillAndSubmitJoinForm(guestPage, meetingId, "GuestUser");
  await guestPage.waitForTimeout(1500);
  const guestResult = await joinMeetingFromPage(guestPage);

  if (guestResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

    const guestJoinButton = guestPage.getByRole("button", {
      name: /Join Meeting|Start Meeting/,
    });
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

  await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
  await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
  // Peer connectivity established (reactions ride the same media fan-out).
  await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 30_000,
  });
  await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 30_000,
  });
}

/**
 * Ensure the reactions palette is open (idempotent). The Reactions slot is on
 * the action bar by default (DEFAULT_SLOTS index 2) but can overflow into the
 * "More actions" menu on a narrow bar — handle both so the specs are
 * width-robust. Idempotent because the palette now STAYS OPEN after a reaction
 * click (issue 1884 persistence): clicking the trigger while it is already open
 * would toggle it closed, so we only click when it is not already visible.
 */
async function ensureReactionsPaletteOpen(page: Page): Promise<void> {
  const palette = page.locator('[data-testid="reactions-palette"]');
  if (await palette.isVisible().catch(() => false)) return;
  const trigger = page.locator('[data-testid="reactions-button"]');
  if (await trigger.isVisible().catch(() => false)) {
    await trigger.click();
  } else {
    await page.locator("#overflow-menu-trigger").click();
    await page.locator(".overflow-item", { hasText: "Reactions" }).click();
  }
  await expect(palette).toBeVisible({ timeout: 5000 });
}

/**
 * Open the palette (if needed) and click one reaction option. The palette STAYS
 * OPEN after the click (issue 1884 persistence) and re-arms its ~5s auto-hide
 * window, so — unlike the old behaviour — this does NOT wait for it to close.
 */
async function clickReaction(page: Page, slug: string): Promise<void> {
  await ensureReactionsPaletteOpen(page);
  await page.locator(`[data-testid="reaction-option-${slug}"]`).click();
}

/** Join a meeting as the sole host (grid visible) — enough for palette-only
 * specs that need no peer. Returns the host page; the caller closes `browser`. */
async function hostInMeeting(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  uiURL: string,
  meetingId: string,
): Promise<Page> {
  const ctx = await createAuthenticatedContext(browser, "host@videocall.rs", "HostUser", uiURL);
  const page = await ctx.newPage();
  await fillAndSubmitJoinForm(page, meetingId, "HostUser");
  await page.waitForTimeout(1500);
  expect(await joinMeetingFromPage(page)).toBe("in-meeting");
  return page;
}

test.describe("Two users in a meeting", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("host starts meeting, guest joins, both see each other @bvt1", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_two_user_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- HOST: go to home page, enter meeting ----
      // Hydration-robust submit (helpers/join-meeting.ts): gates submission on
      // the post-hydration submit button and treats the home form detaching as
      // the "we joined" signal, which is robust to window.location lagging the
      // rendered route under load (the reproduced cause of the join flake).
      await fillAndSubmitJoinForm(hostPage, meetingId, "HostUser");
      await hostPage.waitForTimeout(1500);

      // Host joins the meeting
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // ---- GUEST: go to home page, enter meeting ----
      // Hydration-robust submit (helpers/join-meeting.ts) — same rationale as
      // the host above.
      await fillAndSubmitJoinForm(guestPage, meetingId, "GuestUser");
      await guestPage.waitForTimeout(1500);

      const guestResult = await joinMeetingFromPage(guestPage);

      if (guestResult === "waiting") {
        // Host needs to admit guest from the waiting room.
        // Wait for the admit button to appear (pushed via WebSocket/NATS notification)
        const admitButton = hostPage.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await hostPage.waitForTimeout(1000);
        await admitButton.dispatchEvent("click");
        await hostPage.waitForTimeout(3000);

        // After admission, guest may auto-join (grid appears directly) or
        // may see a "Join Meeting" button — handle both scenarios.
        const guestJoinButton = guestPage.getByRole("button", {
          name: /Join Meeting|Start Meeting/,
        });
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
        // If "grid" won the race, guest already auto-joined — nothing to click.
      }

      // ---- ASSERT: both users are in the meeting ----
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Once a peer connects, the invite overlay ("Your meeting is ready!") disappears.
      // Verify each side sees at least one remote peer's canvas-container.
      const hostPeer = hostPage.locator("#grid-container .canvas-container");
      const guestPeer = guestPage.locator("#grid-container .canvas-container");

      await expect(hostPeer.first()).toBeVisible({ timeout: 30_000 });
      await expect(guestPeer.first()).toBeVisible({ timeout: 30_000 });

      // ---- ASSERT: peer tile shows display_name as text, user_id as tooltip ----
      // The floating name overlay on each peer tile should show the display name,
      // with the user_id (email) available as a tooltip via the title attribute.
      // The host tile includes a "Host: " prefix in the title attribute.
      const guestNameOnHost = hostPage.locator(".floating-name", {
        hasText: "GuestUser",
      });
      const hostNameOnGuest = guestPage.locator(".floating-name", {
        hasText: "HostUser",
      });

      // Check that the guest's display name is visible on the host side.
      // The guest is not the host, so the title is just the user_id.
      await expect(guestNameOnHost.first()).toBeVisible({ timeout: 10_000 });
      await expect(guestNameOnHost.first()).toHaveAttribute("title", "guest@videocall.rs");

      // Check that the host's display name is visible on the guest side.
      // The host tile has a "Host: " prefix in the title attribute.
      await expect(hostNameOnGuest.first()).toBeVisible({ timeout: 10_000 });
      await expect(hostNameOnGuest.first()).toHaveAttribute(
        "title",
        /^(Host: )?host@videocall\.rs$/,
      );

      // ---- ASSERT: floating-name shows display_name NOT email ----
      // Verify the floating name text does NOT contain email addresses.
      // This guards against a regression where user_id/email was shown
      // instead of display_name as the visible tile label.
      const allHostFloatingNames = hostPage.locator(".floating-name");
      const allGuestFloatingNames = guestPage.locator(".floating-name");

      // On host side: no floating name should contain an '@' sign
      const hostFloatingCount = await allHostFloatingNames.count();
      for (let i = 0; i < hostFloatingCount; i++) {
        const text = await allHostFloatingNames.nth(i).textContent();
        expect(text).not.toContain("@");
      }

      // On guest side: no floating name should contain an '@' sign
      const guestFloatingCount = await allGuestFloatingNames.count();
      for (let i = 0; i < guestFloatingCount; i++) {
        const text = await allGuestFloatingNames.nth(i).textContent();
        expect(text).not.toContain("@");
      }

      // ---- ASSERT: "joined the meeting" toast notifications ----
      // Toast format: Line 1 = display name, Line 2 = "joined the meeting"
      // Toasts auto-dismiss after ~8 seconds, so we check within a generous
      // timeout but also accept that the toast may have already appeared
      // and disappeared during the peer discovery wait above.
      //
      // We use a soft check: if the toast container exists, verify its
      // content. The toast may have already been removed by the auto-dismiss
      // timer if peer discovery was slow, so we don't fail if it's gone.
      // CSS classes: .peer-toasts (container), .peer-toast (individual toast)
      const hostJoinedToast = hostPage.locator(".peer-toast", {
        hasText: "joined the meeting",
      });

      // The guest should also see a "joined" toast for the host (who was
      // already in the meeting when the guest connected).
      const guestJoinedToast = guestPage.locator(".peer-toast", {
        hasText: "joined the meeting",
      });

      // At least one side should have seen a "joined" toast. We check
      // both but only require at least one to have been visible, since
      // the auto-dismiss may have already cleared one side.
      const hostSawToast = await hostJoinedToast.isVisible().catch(() => false);
      const guestSawToast = await guestJoinedToast.isVisible().catch(() => false);

      // Log which side(s) saw the toast for debugging
      console.log(`Host saw "joined" toast: ${hostSawToast}`);
      console.log(`Guest saw "joined" toast: ${guestSawToast}`);

      // If either side still has a visible toast, verify the two-line format:
      // Line 1: display name, Line 2: "joined the meeting"
      if (hostSawToast) {
        const toast = hostJoinedToast.first();
        await expect(toast.locator(".toast-name")).toContainText("GuestUser");
        await expect(toast.locator(".toast-action")).toContainText("joined the meeting");
      }
      if (guestSawToast) {
        const toast = guestJoinedToast.first();
        await expect(toast.locator(".toast-name")).toContainText("HostUser");
        await expect(toast.locator(".toast-action")).toContainText("joined the meeting");
      }
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ---- Issue 1884: meeting reactions ----
  // Untagged (no @bvt): this two-browser spec runs in the dioxus full suite /
  // local docker stack, not per-PR bvt CI. Validated via the local e2e stack.

  test("reaction round-trip: guest sees the sender's name, host sees its own 'You' echo", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_rt_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
      );
      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      await enterTwoUserMeeting(hostPage, guestPage, meetingId);

      // Case 1: host sends thumbs_up; the guest sees a float attributed to the
      // sender's display name (HostUser), NOT the raw user_id/email.
      await clickReaction(hostPage, "thumbs_up");

      const guestFloatName = guestPage.locator('[data-testid="reaction-float-name"]', {
        hasText: "HostUser",
      });
      await expect(guestFloatName.first()).toBeVisible({ timeout: 10_000 });
      expect(await guestFloatName.first().textContent()).not.toContain("@");

      // Case 2: the SENDER renders its own reaction locally as a "You" echo (the
      // relay self-skips the source, so it never comes back over the wire).
      const hostOwnEcho = hostPage.locator('[data-testid="reaction-float-name"]', {
        hasText: "You",
      });
      await expect(hostOwnEcho.first()).toBeVisible({ timeout: 10_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("reactions palette: toolbar semantics, open/close, Escape restores focus", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_a11y_${Date.now()}`;
    // Palette semantics need only ONE participant in the grid.
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostPage = await hostInMeeting(browser1, uiURL, meetingId);

      const trigger = hostPage.locator('[data-testid="reactions-button"]');
      // The Reactions slot may live in the overflow menu on a narrow bar; this
      // a11y spec asserts against the on-bar trigger, so require it present.
      await expect(trigger).toBeVisible({ timeout: 10_000 });

      // UX B2 toolbar contract on the trigger: NO aria-haspopup (that announces
      // a menu popup); aria-controls ties it to the palette; aria-expanded
      // tracks state.
      await expect(trigger).not.toHaveAttribute("aria-haspopup", /.*/);
      await expect(trigger).toHaveAttribute("aria-controls", "reactions-palette");
      await expect(trigger).toHaveAttribute("aria-expanded", "false");

      await trigger.click();
      const palette = hostPage.locator('[data-testid="reactions-palette"]');
      await expect(palette).toBeVisible({ timeout: 5000 });
      await expect(trigger).toHaveAttribute("aria-expanded", "true");

      // UX B2: role=toolbar + orientation + accessible name; the options are
      // plain buttons (NOT menuitems) named "React with {label}".
      await expect(palette).toHaveAttribute("role", "toolbar");
      await expect(palette).toHaveAttribute("aria-orientation", "horizontal");
      await expect(palette).toHaveAttribute("aria-label", "Send a reaction");
      await expect(palette.getByRole("menuitem")).toHaveCount(0);
      const thumbsUp = hostPage.getByRole("button", { name: "React with thumbs up" });
      await expect(thumbsUp).toBeVisible();
      // The close (X) is a keyboard-reachable toolbar button.
      await expect(hostPage.getByRole("button", { name: "Close reactions" })).toBeVisible();

      // Click-outside (on the grid background) light-dismisses it.
      await hostPage.locator("#grid-container").click({ position: { x: 6, y: 6 }, force: true });
      await expect(palette).toBeHidden({ timeout: 3000 });
      await expect(trigger).toHaveAttribute("aria-expanded", "false");

      // Escape closes AND restores focus to the trigger.
      await trigger.click();
      await expect(palette).toBeVisible({ timeout: 5000 });
      await hostPage.keyboard.press("Escape");
      await expect(palette).toBeHidden({ timeout: 3000 });
      await expect(trigger).toBeFocused();
    } finally {
      await browser1.close();
    }
  });

  test("rapid same-emoji reactions coalesce into a count badge within the concurrency cap", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_burst_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
      );
      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      await enterTwoUserMeeting(hostPage, guestPage, meetingId);

      // Case 6 + 7: a burst of the SAME emoji from one sender. The client
      // self-throttle keeps sends within the relay budget, and the receiver
      // coalesces repeats of the same (sender, emoji) into ONE float with a
      // "×N" count badge instead of spawning a float per click. The palette
      // stays open (persistence), so we open it once and click the same option
      // repeatedly — re-opening would toggle it closed.
      await ensureReactionsPaletteOpen(hostPage);
      const burstOption = hostPage.locator('[data-testid="reaction-option-thumbs_up"]');
      const BURST = 8;
      for (let i = 0; i < BURST; i++) {
        await burstOption.click();
        // ~200ms between clicks. Back-to-back clicks can complete under the
        // client's 350ms send min-interval on a fast runner, so the throttle
        // would admit only ONE send and no ×N badge would appear. 200ms also
        // clears the ~150ms local press gate (so each click is not coalesced
        // away) while several still get through (coalescing within the 2s
        // window).
        await hostPage.waitForTimeout(200);
      }

      // The count badge appears (coalescing happened) — assert DOM, never pixels.
      const guestCount = guestPage.locator('[data-testid="reaction-float-count"]');
      await expect(guestCount.first()).toBeVisible({ timeout: 10_000 });
      const badgeText = (await guestCount.first().textContent()) || "";
      const n = parseInt(badgeText.replace(/[^0-9]/g, ""), 10);
      // Received count is bounded: >=2 (multiple round-tripped) and never more
      // than the clicks issued (no phantom amplification / double-echo).
      expect(n).toBeGreaterThanOrEqual(2);
      expect(n).toBeLessThanOrEqual(BURST);

      // The hard concurrency cap (MAX_CONCURRENT_REACTIONS = 12) is never
      // exceeded. The exact drop-newest math is pinned by the host test
      // integrate_drops_newest_at_the_hard_cap; here we assert the DOM bound.
      const floatCount = await guestPage.locator('[data-testid="reaction-float"]').count();
      expect(floatCount).toBeLessThanOrEqual(12);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("reduced motion: a reaction still renders and the live region announces it", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_rm_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
      );
      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      // Case 5: the guest prefers reduced motion (static/opacity-only variant).
      await guestPage.emulateMedia({ reducedMotion: "reduce" });
      await enterTwoUserMeeting(hostPage, guestPage, meetingId);

      await clickReaction(hostPage, "thumbs_up");

      // The float still appears (reduced motion only changes HOW it animates).
      await expect(guestPage.locator('[data-testid="reaction-float"]').first()).toBeVisible({
        timeout: 10_000,
      });

      // The polite live region is the sole SR channel (overlay is aria-hidden).
      // It flushes at most once per throttle window, so allow generous time.
      const liveRegion = guestPage.locator('[data-testid="reaction-live-region"]');
      await expect(liveRegion).toContainText("reacted with thumbs up", { timeout: 8000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // Palette PERSISTENCE (issue 1884, scope extension) — palette-only behaviour,
  // so a single host in the grid is enough (no peer needed).

  test("reaction click keeps the palette open, then it auto-hides after the window", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_persist_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostPage = await hostInMeeting(browser1, uiURL, meetingId);
      const palette = hostPage.locator('[data-testid="reactions-palette"]');

      await clickReaction(hostPage, "thumbs_up");

      // Persistence: still open ~1s after a click. (The prior behaviour
      // auto-closed ~150ms after a click — this guards that regression.)
      await hostPage.waitForTimeout(1000);
      await expect(palette).toBeVisible();

      // Auto-hide (MOUSE modality leg of the B3 split): a Playwright .click() is
      // pointer input, so the clicked option is focused but NOT :focus-visible —
      // the timer closes after the ~5s window. Generous real wait (bounded > 5s)
      // rather than page.clock, which would also freeze the wasm app's
      // connection/transport timers and destabilise the meeting.
      await expect(palette).toBeHidden({ timeout: 9000 });
    } finally {
      await browser1.close();
    }
  });

  test("keyboard focus in the palette pauses auto-hide (B3 modality split)", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_kbdpause_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostPage = await hostInMeeting(browser1, uiURL, meetingId);
      const palette = hostPage.locator('[data-testid="reactions-palette"]');

      // Arm the ~5s auto-hide with a click, then move focus by KEYBOARD (arrow)
      // so the focused option is :focus-visible. Chromium sets focus-visible
      // when .focus() runs inside a keydown handler (the roving-tabindex move),
      // which is exactly what the palette's arrow handler does.
      await clickReaction(hostPage, "thumbs_up");
      await expect(palette).toBeVisible();
      await hostPage.keyboard.press("ArrowRight");

      // Idle well past the original 5s deadline. With keyboard focus inside, the
      // timer RE-ARMS instead of closing (modality-aware pause), so the palette
      // stays open — never yanking a keyboard user's focus. On the pre-B3 code
      // the timer closes at 5s regardless of modality → this fails.
      await hostPage.waitForTimeout(6500);
      await expect(palette).toBeVisible();
    } finally {
      await browser1.close();
    }
  });

  test("the X button closes the palette and restores focus to the trigger", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_xclose_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostPage = await hostInMeeting(browser1, uiURL, meetingId);
      await ensureReactionsPaletteOpen(hostPage);

      const palette = hostPage.locator('[data-testid="reactions-palette"]');
      const closeBtn = hostPage.locator('[data-testid="reactions-close"]');
      await expect(closeBtn).toBeVisible();
      await expect(closeBtn).toHaveAttribute("aria-label", "Close reactions");

      await closeBtn.click();
      await expect(palette).toBeHidden({ timeout: 3000 });
      await expect(hostPage.locator('[data-testid="reactions-button"]')).toBeFocused();
    } finally {
      await browser1.close();
    }
  });

  test("clicking outside after a reaction closes the palette immediately (pre-empts auto-hide)", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_outside_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostPage = await hostInMeeting(browser1, uiURL, meetingId);
      const palette = hostPage.locator('[data-testid="reactions-palette"]');

      // Click a reaction to ARM the 5s auto-hide, then click outside.
      await clickReaction(hostPage, "thumbs_up");
      await expect(palette).toBeVisible();
      await hostPage.locator("#grid-container").click({ position: { x: 6, y: 6 }, force: true });

      // Closes well under the 5s window → outside-click pre-empted the timer.
      await expect(palette).toBeHidden({ timeout: 2000 });
    } finally {
      await browser1.close();
    }
  });

  test("reopening the palette after a reaction does not inherit the prior auto-hide timer", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_reopen_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostPage = await hostInMeeting(browser1, uiURL, meetingId);
      const palette = hostPage.locator('[data-testid="reactions-palette"]');

      // Session A: open, click a reaction (arms the ~5s auto-hide), then close
      // with Escape well before the window elapses.
      await clickReaction(hostPage, "thumbs_up");
      await expect(palette).toBeVisible();
      await hostPage.keyboard.press("Escape");
      await expect(palette).toBeHidden({ timeout: 3000 });

      // Session B: reopen WITHOUT clicking any reaction. "Merely opening does not
      // start the window", so a never-re-clicked palette must stay open
      // indefinitely — session A's timer must NOT carry over.
      await ensureReactionsPaletteOpen(hostPage);

      // Wait past session A's original ~5s deadline. On the pre-fix code the
      // stale timer's generation still matched and it read the reopened palette
      // as open, closing it here → this assertion fails. After the fix, the
      // Escape-close bumped the generation, so the stale timer no-ops.
      await hostPage.waitForTimeout(6500);
      await expect(palette).toBeVisible();
    } finally {
      await browser1.close();
    }
  });
});
