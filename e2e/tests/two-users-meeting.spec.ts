import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
// The join dance lived here until issue 2135 needed the same harness for the
// raise-hand cross-peer specs. Moved verbatim to helpers/two-user-meeting.ts so
// there is ONE implementation rather than two that drift.
import { enterTwoUserMeeting, joinMeetingFromPage } from "../helpers/two-user-meeting";

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
  /**
   * Extra `newContext` options. Only consumer so far is the issue-2141 touch
   * spec, which needs `hasTouch: true` — that is what makes Chromium report
   * `(pointer: coarse)`, and it can only be set at context creation, not with
   * `setViewportSize` after the fact. Verified empirically before the spec was
   * written: default context is `pointer: fine`, `hasTouch: true` flips it to
   * `pointer: coarse` / `pointer: fine == false` at the SAME 1280x720 viewport.
   */
  extraOptions: Parameters<typeof browser.newContext>[0] = {},
) {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
    ...extraOptions,
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

/**
 * Open the palette (if needed) and reveal the standard-emoji picker (CUSTOM
 * reaction, issue 1884). Idempotent: the "More emoji" toggle reflects its state
 * in aria-expanded, so we only click it when the picker is not already shown.
 */
async function openEmojiPicker(page: Page): Promise<void> {
  await ensureReactionsPaletteOpen(page);
  const picker = page.locator('[data-testid="emoji-picker"]');
  if (await picker.isVisible().catch(() => false)) return;
  await page.locator('[data-testid="emoji-picker-open"]').click();
  await expect(picker).toBeVisible({ timeout: 5000 });
}

/** Join a meeting as the sole host (grid visible) — enough for palette-only
 * specs that need no peer. Returns the host page; the caller closes `browser`. */
async function hostInMeeting(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  uiURL: string,
  meetingId: string,
  extraContextOptions: Parameters<typeof browser.newContext>[0] = {},
): Promise<Page> {
  const ctx = await createAuthenticatedContext(
    browser,
    "host@videocall.rs",
    "HostUser",
    uiURL,
    extraContextOptions,
  );
  const page = await ctx.newPage();
  await fillAndSubmitJoinForm(page, meetingId, "HostUser");
  await page.waitForTimeout(1500);
  expect(await joinMeetingFromPage(page)).toBe("in-meeting");
  return page;
}

/**
 * Switch the meeting page to LIGHT theme via the real Appearance toggle, then
 * close the settings modal. Copied from the proven idiom in
 * peer-toast-light-contrast.spec.ts (which is itself @bvt1): the "Light" icon
 * button sets `html[data-theme]="light"` synchronously.
 *
 * Call this BEFORE opening the reactions palette — the settings modal's clicks
 * bubble to #main-container's onclick, which sets `reactions_open` false.
 */
async function setLightThemeFromMeeting(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.locator('[data-testid="open-settings"]').click();
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });

  await page.getByRole("tab", { name: "Appearance" }).click();
  await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });
  await page.getByRole("button", { name: "Light", exact: true }).click();

  await expect
    .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
      timeout: 3_000,
    })
    .toBe("light");

  await page.locator('button[aria-label="Close settings"]').click();
  await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });
}

/**
 * Rec.709 relative luminance of a computed `color`. Alpha is ignored on purpose:
 * `getComputedStyle().color` is NOT composited against the background, so the raw
 * channels are exactly the fixed-white vs theme-flipped near-black we want to
 * discriminate. Same helper pair as peer-toast-light-contrast.spec.ts.
 */
function parseRgb(value: string): { r: number; g: number; b: number } {
  const match = value.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)/i);
  if (!match) {
    throw new Error(`Unsupported color format: ${value}`);
  }
  return { r: Number(match[1]), g: Number(match[2]), b: Number(match[3]) };
}

function luminance(value: string): number {
  const { r, g, b } = parseRgb(value);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

// Theme-flipped tokens resolve near-black under light theme (--text-secondary
// #636366 → ~99, --text-primary #1a1a1a → ~26); the fixed --on-dark-* values are
// white (255). 180 sits between the two with no overlap.
const LIGHT_LUMINANCE_FLOOR = 180;

/** `data-testid` of the currently focused element, or null. Used to pin the
 * palette's documented Tab order and the post-reset focus handoff. */
async function activeTestId(page: Page): Promise<string | null> {
  return page.evaluate(() => document.activeElement?.getAttribute("data-testid") ?? null);
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

  test("repeated same-emoji reactions each render as their own float (no count badge)", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_repeat_${Date.now()}`;
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

      // Issue 1884 tweak: repeats of the SAME emoji are NO LONGER coalesced —
      // each round-tripped reaction is its own float with its own animation, and
      // there is NO "×N" count badge. The palette stays open (persistence), so we
      // open it once and click the same option repeatedly.
      await ensureReactionsPaletteOpen(hostPage);
      const burstOption = hostPage.locator('[data-testid="reaction-option-thumbs_up"]');
      const REPEATS = 3;
      for (let i = 0; i < REPEATS; i++) {
        await burstOption.click();
        // ~400ms between clicks: clears the client's 350ms send min-interval so
        // EVERY click sends (a tighter spacing would let the self-throttle drop
        // some, weakening the "3 separate floats" assertion). Also clears the
        // ~150ms local press gate.
        await hostPage.waitForTimeout(400);
      }

      // The guest sees a SEPARATE float per received reaction — assert DOM, never
      // pixels. >=2 proves the repeats did not merge into one; the exact count
      // can vary with round-trip timing, so we assert the lower bound + the cap.
      const guestFloats = guestPage.locator('[data-testid="reaction-float"]');
      await expect
        .poll(async () => await guestFloats.count(), { timeout: 10_000 })
        .toBeGreaterThanOrEqual(2);

      // No count badge exists ANYWHERE — the ×N coalescing UI was removed. This
      // is the assertion that FAILS on the old coalescing build (which merged
      // repeats into one float carrying a reaction-float-count badge).
      await expect(guestPage.locator('[data-testid="reaction-float-count"]')).toHaveCount(0);

      // The hard concurrency cap (MAX_CONCURRENT_REACTIONS = 24) is never
      // exceeded. The exact drop-oldest math is pinned by the host test
      // integrate_drops_oldest_at_the_hard_cap; here we assert the DOM bound.
      const floatCount = await guestFloats.count();
      expect(floatCount).toBeLessThanOrEqual(24);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("custom emoji picker: guest sees the picked emoji as a float", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_reaction_custom_${Date.now()}`;
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

      // Issue 1884: open the standard-emoji picker and pick a specific emoji.
      // The grid's first cell in the default category (Smileys & Emotion) is a
      // stable target; we read its glyph rather than hard-coding one so the test
      // does not depend on the emoji-table ordering.
      await openEmojiPicker(hostPage);
      const firstEmoji = hostPage.locator('[data-testid="emoji-option-0"]');
      await expect(firstEmoji).toBeVisible({ timeout: 5000 });
      const glyph = (
        (await firstEmoji.locator(".reaction-option__emoji").textContent()) || ""
      ).trim();
      expect(glyph.length).toBeGreaterThan(0);

      await firstEmoji.click();

      // The sender renders its own local "You" echo (relay self-skips the sender).
      const hostEcho = hostPage
        .locator('[data-testid="reaction-float"]')
        .filter({ hasText: glyph })
        .first();
      await expect(hostEcho).toBeVisible({ timeout: 10_000 });

      // The guest receives a float whose emoji text is exactly the picked glyph
      // (rendered from the validated custom_emoji string, escaped by Dioxus).
      const guestGlyphFloats = guestPage
        .locator('[data-testid="reaction-float"] .reaction-float__emoji')
        .filter({ hasText: glyph });
      await expect(guestGlyphFloats.first()).toBeVisible({ timeout: 10_000 });

      // Issue 1884 (recents): the picked emoji is now a palette quick-pick
      // "recent" on the sender. It carries the same glyph and sits at index 0.
      const recent0 = hostPage.locator('[data-testid="reaction-recent-0"]');
      await expect(recent0).toBeVisible({ timeout: 5000 });
      await expect(recent0.locator(".reaction-option__emoji")).toHaveText(glyph);

      // Clicking the recent re-sends it as a CUSTOM reaction — the guest gets
      // ANOTHER float of that glyph (count strictly increases). Space the click
      // past the client's 350ms send min-interval so it definitely sends.
      const before = await guestGlyphFloats.count();
      await hostPage.waitForTimeout(500);
      await recent0.click();
      await expect
        .poll(async () => await guestGlyphFloats.count(), { timeout: 10_000 })
        .toBeGreaterThan(before);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("recent custom emojis: valid localStorage seed renders a quick-pick, invalid seed does not", async ({
    baseURL,
  }) => {
    // Palette-only (recents render regardless of peers), so a single host is
    // enough. Two contexts in one browser: one seeded with a VALID recent, one
    // with a TAMPERED (non-emoji) recent. localStorage is seeded BEFORE nav (the
    // app loads + sanitizes it at mount), per the repo's addInitScript precedent.
    const uiURL = baseURL || "http://localhost:80";
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      // VALID seed: 🚀 (a standard emoji NOT in the fixed row) must appear as
      // reaction-recent-0 with its glyph.
      const goodCtx = await createAuthenticatedContext(
        browser1,
        "host@videocall.rs",
        "HostUser",
        uiURL,
      );
      await goodCtx.addInitScript(
        `localStorage.setItem("reactions.recent_custom", ${JSON.stringify(JSON.stringify(["🚀"]))});`,
      );
      const goodPage = await goodCtx.newPage();
      await fillAndSubmitJoinForm(goodPage, `e2e_recent_ok_${Date.now()}`, "HostUser");
      await goodPage.waitForTimeout(1500);
      expect(await joinMeetingFromPage(goodPage)).toBe("in-meeting");
      await ensureReactionsPaletteOpen(goodPage);
      const recent0 = goodPage.locator('[data-testid="reaction-recent-0"]');
      await expect(recent0).toBeVisible({ timeout: 5000 });
      await expect(recent0.locator(".reaction-option__emoji")).toHaveText("🚀");
      await goodPage.close();

      // TAMPERED seed: "hello" is not an emoji — sanitize-on-load drops it, so the
      // palette opens with NO recent quick-pick (validation on load; the
      // assertion FAILS if load-validation is removed and "hello" is injected).
      const badCtx = await createAuthenticatedContext(
        browser1,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
      );
      await badCtx.addInitScript(
        `localStorage.setItem("reactions.recent_custom", ${JSON.stringify(JSON.stringify(["hello"]))});`,
      );
      const badPage = await badCtx.newPage();
      await fillAndSubmitJoinForm(badPage, `e2e_recent_bad_${Date.now()}`, "GuestUser");
      await badPage.waitForTimeout(1500);
      expect(await joinMeetingFromPage(badPage)).toBe("in-meeting");
      await ensureReactionsPaletteOpen(badPage);
      await expect(badPage.locator('[data-testid="reactions-palette"]')).toBeVisible();
      await expect(badPage.locator('[data-testid="reaction-recent-0"]')).toHaveCount(0);
    } finally {
      await browser1.close();
    }
  });

  // TAGGED @bvt1 (unlike the untagged two-browser reaction specs above): this one
  // is single-browser, single-join and palette-only, so it is cheap enough for the
  // per-PR smoke superset — which is also how it gets a real run receipt.
  test("reset control clears the custom-reaction quick-picks and the persisted key @bvt1", async ({
    baseURL,
  }) => {
    // Issue 2086. Palette-only (recents render regardless of peers), so a single
    // host is enough.
    //
    // FAILS ON UN-FIXED CODE: without the reset control the
    // `reactions-reset-recents` locator never becomes visible and the test stops
    // at that expect, before it ever presses Enter.
    //
    // Runs in LIGHT theme throughout. The palette paints --glass-popover-bg, a
    // FIXED-DARK surface: issue 2091 moved it into global.css's theme-independent
    // :root block, so it stays dark-navy glass under html[data-theme="light"]
    // too. (Before that it was defined only under html[data-theme="dark"] and the
    // light surface went unpainted — a different mechanism, same conclusion.)
    // Either way light theme is exactly where a theme-flipping colour token would
    // wash the reset glyph out. Setting the theme
    // first (before the palette opens) means every leg below exercises the risky
    // theme, and the luminance assertions catch a regression to
    // --text-secondary/--text-primary (~99 / ~26) against the fixed white (255).
    //
    // The seed is written by an init script guarded on a one-shot marker: init
    // scripts re-run on EVERY navigation in the context, so an unguarded seed
    // would silently re-populate the key on the second page and turn the
    // persistence half of this test vacuous (it would assert "still empty" against
    // a list the harness had just re-seeded).
    const uiURL = baseURL || "http://localhost:80";
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const ctx = await createAuthenticatedContext(
        browser1,
        "host@videocall.rs",
        "HostUser",
        uiURL,
      );
      await ctx.addInitScript(
        `if (!localStorage.getItem("e2e.recents_seeded")) {
           localStorage.setItem("e2e.recents_seeded", "1");
           localStorage.setItem("reactions.recent_custom", ${JSON.stringify(
             JSON.stringify(["🚀", "🎉"]),
           )});
         }`,
      );

      const page = await ctx.newPage();
      await fillAndSubmitJoinForm(page, `e2e_recents_reset_${Date.now()}`, "HostUser");
      await page.waitForTimeout(1500);
      expect(await joinMeetingFromPage(page)).toBe("in-meeting");

      // Light theme BEFORE the palette opens (the modal's clicks would close it).
      await setLightThemeFromMeeting(page);
      await ensureReactionsPaletteOpen(page);

      // The reset control and the quick-picks are SIBLINGS inside the palette
      // (both direct children of .reactions-palette), so both anchor on the
      // palette — never one on the other.
      const palette = page.locator('[data-testid="reactions-palette"]');
      const recent0 = palette.locator('[data-testid="reaction-recent-0"]');
      const recent1 = palette.locator('[data-testid="reaction-recent-1"]');
      const reset = palette.locator('[data-testid="reactions-reset-recents"]');

      // PRESENCE FIRST: both seeded quick-picks and the reset control are really
      // there, so the disappearance assertions below cannot pass vacuously.
      await expect(recent0).toBeVisible({ timeout: 5000 });
      await expect(recent0.locator(".reaction-option__emoji")).toHaveText("🚀");
      await expect(recent1).toBeVisible();
      await expect(reset).toBeVisible();
      await expect(reset).toHaveAttribute("aria-label", "Clear recent emoji");
      await expect(palette.locator(".reactions-recent-sep")).toHaveCount(1);

      // LIGHT-THEME CONTRAST: the glyph stays light-on-dark in BOTH the rest and
      // hover states. Reverting the CSS to --text-secondary/--text-primary drops
      // these to ~99 / ~26, well under the floor — hover is the worse of the two,
      // near-black on a dark surface, so it is asserted explicitly.
      const restColor = await reset.evaluate((el) => getComputedStyle(el).color);
      expect(luminance(restColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);
      await reset.hover();
      const hoverColor = await reset.evaluate((el) => getComputedStyle(el).color);
      expect(luminance(hoverColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);

      // WAIT OUT THE PALETTE'S OWN AUTO-FOCUS BEFORE TOUCHING THE KEYBOARD.
      // Opening the palette arms a deferred focus jump: a `use_effect` on
      // `reactions_open` (attendants.rs) schedules
      // `Timeout::new(100, focus_element_by_id("reaction-opt-thumbs_up"))` so a
      // keyboard user lands inside the menu. 100ms is wall-clock from the OPEN
      // click, and on a loaded runner the wasm main thread stalls long enough
      // for it to land SECONDS later — in the middle of the Tab sequence below,
      // yanking focus back to the first option.
      //
      // That is exactly how this spec failed on PR #2125's `Playwright bvt1`:
      // "expected reactions-reset-recents, received reaction-recent-0" (the
      // jump landed between two Tabs, so the next Tab stepped off thumbs_up
      // instead of off recent-1) and "expected emoji-picker-open, received
      // reaction-option-thumbs_up" (it landed after the Enter, stomping the
      // reset control's focus handoff). Reproduced deliberately with a
      // throwaway probe that ran the leg with no settling wait:
      // `thumbs_up -> recent-0 -> recent-1 -> reactions-reset-recents ->
      // reaction-option-thumbs_up`, the last hop being the timer arriving late.
      //
      // Polling for the jump instead of assuming it has landed closes the race
      // AND pins the auto-focus-on-open behaviour, which nothing else asserted.
      // No assertion below is relaxed by this — the Tab order and the focus
      // handoff are checked exactly as before.
      await expect
        .poll(() => activeTestId(page), { timeout: 10_000 })
        .toBe("reaction-option-thumbs_up");

      // KEYBOARD LEG: the documented Tab order (attendants.rs) runs
      // highlighted-reaction -> recents -> reset -> More emoji. The ten
      // non-highlighted reactions carry tabindex=-1, so Tab skips them. This is
      // the only test behind those Tab-order comments.
      await page.locator('[data-testid="reaction-option-thumbs_up"]').focus();
      await page.keyboard.press("Tab");
      expect(await activeTestId(page)).toBe("reaction-recent-0");
      await page.keyboard.press("Tab");
      expect(await activeTestId(page)).toBe("reaction-recent-1");
      await page.keyboard.press("Tab");
      expect(await activeTestId(page)).toBe("reactions-reset-recents");

      // Activate from the keyboard — which is what makes the focus handoff below
      // load-bearing, since this is the path that leaves focus on a button that is
      // about to unmount.
      await page.keyboard.press("Enter");

      // The whole recents GROUP goes at once — quick-picks, divider, and the
      // reset control itself (they share one emptiness condition in the RSX).
      await expect(recent0).toHaveCount(0);
      await expect(recent1).toHaveCount(0);
      await expect(reset).toHaveCount(0);
      await expect(palette.locator(".reactions-recent-sep")).toHaveCount(0);

      // The palette STAYS OPEN — #main-container's onclick (which sets
      // reactions_open false) never sees this activation, because the palette's own
      // onclick stops propagation for every child. The fixed standard reactions are
      // untouched: reset only removes the custom quick-picks.
      await expect(palette).toBeVisible();
      await expect(palette.locator('[data-testid="reaction-option-thumbs_up"]')).toBeVisible();

      // Focus was on the button that just unmounted; it must have moved to the
      // surviving "More emoji" sibling rather than dropping to <body>. Checked
      // after the disappearance assertions so the re-render has certainly landed.
      expect(await activeTestId(page)).toBe("emoji-picker-open");

      // The persisted key is REMOVED, not left holding the old list.
      const stored = await page.evaluate(() => localStorage.getItem("reactions.recent_custom"));
      expect(stored).toBeNull();

      // A fresh app mount (new page, same context/origin → same localStorage, and
      // the guarded init script does NOT re-seed) still shows no quick-picks: the
      // reset survived the reload, it did not just clear an in-memory signal.
      await page.close();
      const page2 = await ctx.newPage();
      await fillAndSubmitJoinForm(page2, `e2e_recents_reset2_${Date.now()}`, "HostUser");
      await page2.waitForTimeout(1500);
      expect(await joinMeetingFromPage(page2)).toBe("in-meeting");
      await ensureReactionsPaletteOpen(page2);
      const palette2 = page2.locator('[data-testid="reactions-palette"]');
      await expect(palette2).toBeVisible();
      // Sanity: the palette really rendered its fixed row, so the empty-recents
      // assertions below are about the recents and not about a palette that
      // failed to mount.
      await expect(palette2.locator('[data-testid="reaction-option-thumbs_up"]')).toBeVisible();
      await expect(palette2.locator('[data-testid="reaction-recent-0"]')).toHaveCount(0);
      await expect(palette2.locator('[data-testid="reactions-reset-recents"]')).toHaveCount(0);

      // Reset is not a one-way door: the next custom send repopulates the row
      // (and brings the reset control back with it), so the feature cannot wedge
      // recents off permanently.
      await openEmojiPicker(page2);
      const firstEmoji = page2.locator('[data-testid="emoji-option-0"]');
      await expect(firstEmoji).toBeVisible({ timeout: 5000 });
      const glyph = (
        (await firstEmoji.locator(".reaction-option__emoji").textContent()) || ""
      ).trim();
      expect(glyph.length).toBeGreaterThan(0);
      await firstEmoji.click();
      const newRecent0 = palette2.locator('[data-testid="reaction-recent-0"]');
      await expect(newRecent0).toBeVisible({ timeout: 5000 });
      await expect(newRecent0.locator(".reaction-option__emoji")).toHaveText(glyph);
      await expect(palette2.locator('[data-testid="reactions-reset-recents"]')).toBeVisible();
    } finally {
      await browser1.close();
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

  // ── issue 2141: emoji SEARCH inside the picker ──
  // All four are palette-only (no peer needed) except where a send is asserted,
  // which the sender's own local echo covers — so a single host in the grid is
  // enough, matching the persistence specs above.

  /** Open the picker and return its search input, waiting for the auto-focus. */
  async function openEmojiSearch(page: Page) {
    await openEmojiPicker(page);
    const input = page.locator('[data-testid="emoji-search-input"]');
    await expect(input).toBeVisible({ timeout: 5000 });
    // The field focuses itself on mount. Poll rather than assert once: the
    // palette ALSO arms a 100ms focus jump to `reaction-opt-thumbs_up` when it
    // opens, and on a loaded runner that timer can land late (the exact race
    // that broke this file's Tab-order spec on PR #2125). Waiting for the field
    // to hold focus proves the guard added in attendants.rs actually kept it.
    await expect(input).toBeFocused({ timeout: 10_000 });
    return input;
  }

  test("emoji search filters the grid, caps broad queries, and announces the count @bvt1", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_emoji_search_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const page = await hostInMeeting(browser1, uiURL, meetingId);
      const input = await openEmojiSearch(page);
      const options = page.locator('[data-testid="emoji-picker-grid"] button');
      const statusText = page.locator('[data-testid="emoji-search-status-text"]');
      const live = page.locator('[data-testid="emoji-search-live"]');

      // A real accessible name and the right control type. `type="search"`
      // carries the implicit `searchbox` role — an explicit role attribute here
      // would be redundant, so assert there is none rather than assert a value.
      await expect(input).toHaveAttribute("type", "search");
      await expect(input).toHaveAttribute("aria-label", "Search emoji by name or shortcode");
      await expect(input).toHaveAttribute("aria-controls", "emoji-picker-grid");
      await expect(input).not.toHaveAttribute("role", /.*/);
      // NO aria-describedby. It used to name the live region below, which is a
      // documented anti-pattern — a role=status node dual-purposed as an
      // accessible DESCRIPTION is re-read in full (and stale) by NVDA/JAWS on
      // every refocus, double-announces on the tick the region fires, and is
      // EMPTY at mount, which is the one moment focus lands in the field. The
      // count reaches screen readers through the live region instead.
      await expect(input).not.toHaveAttribute("aria-describedby", /.*/);
      // Soft-keyboard affordance for the touch devices whose autofocus the
      // picker now suppresses.
      await expect(input).toHaveAttribute("enterkeyhint", "search");
      // The live region must not contradict itself (#2135: role=status +
      // aria-live=off silences the region it claims to announce).
      await expect(live).toHaveAttribute("role", "status");
      await expect(live).toHaveAttribute("aria-live", "polite");

      const unfiltered = await options.count();
      expect(unfiltered).toBeGreaterThan(60);

      // NARROW QUERY. Typed key-by-key so the debounce is exercised the way a
      // human exercises it, and so a per-keystroke announcement would show up.
      await page.evaluate(() => {
        const el = document.querySelector('[data-testid="emoji-search-live"]');
        (window as Window & { __liveWrites?: string[] }).__liveWrites = [];
        if (!el) return;
        new MutationObserver(() => {
          (window as Window & { __liveWrites?: string[] }).__liveWrites?.push(el.textContent || "");
        }).observe(el, { childList: true, characterData: true, subtree: true });
      });
      await input.pressSequentially("smile", { delay: 60 });

      // ANCHOR ON THE ANNOUNCEMENT, which NAMES the query — it is the only DOM
      // state that can belong to the finished word. Latching on "the grid got
      // smaller" would accept a mid-word render: "smil" alone matches 19 emoji
      // against "smile"'s 6, so every count-derived assertion below would then be
      // measured against a query the user never finished typing.
      await expect(live).toHaveText(/^\d+ emoji found for smile$/, { timeout: 10_000 });

      // Filtered, non-empty, and strictly smaller than the unfiltered category.
      const narrow = await options.count();
      expect(narrow).toBeGreaterThan(0);
      expect(narrow).toBeLessThan(unfiltered);
      await expect(statusText).toHaveText(new RegExp(`^${narrow} emoji$`));
      await expect(live).toHaveText(new RegExp(`^${narrow} emoji found for smile$`));

      // FEWER WRITES THAN KEYSTROKES — the whole point of the debounce. Not
      // pinned to exactly one: the timer restarts per keystroke, so a runner that
      // stalls past the 350ms window mid-word legitimately flushes an extra
      // utterance. An un-debounced build writes once per `input` event (five), so
      // "< 5" still fails on it — and nothing else can inflate the count, because
      // the live region is an isolated memoized child whose text node Dioxus
      // rewrites only when the announcement string actually changes.
      await page.waitForTimeout(700);
      const writes = await page.evaluate(
        () => (window as Window & { __liveWrites?: string[] }).__liveWrites || [],
      );
      expect(writes.length).toBeGreaterThanOrEqual(1);
      expect(writes.length).toBeLessThan(5);

      // BROAD QUERY: the cap is the issue-2141 invariant. "a" matches over a
      // thousand emoji; the grid must still mount exactly 60, and the user must
      // be TOLD the list was truncated rather than shown a silent slice.
      await input.fill("a");
      await expect.poll(async () => await options.count(), { timeout: 5000 }).toBe(60);
      await expect(statusText).toHaveText(/^Showing first 60 of \d+ — refine your search$/);
      const shownTotal = await statusText.textContent();
      const total = Number(/of (\d+)/.exec(shownTotal || "")?.[1]);
      expect(total).toBeGreaterThan(60);
      await expect(live).toHaveText(
        new RegExp(`^${total} emoji found for a, showing the first 60$`),
        { timeout: 5000 },
      );

      // ZERO MATCHES: an explicit empty state, not a blank scroll area.
      await input.fill("zzqqxx");
      const empty = page.locator('[data-testid="emoji-search-empty"]');
      await expect(empty).toBeVisible({ timeout: 5000 });
      await expect(empty).toContainText("zzqqxx");
      // The grid CONTAINER survives (the field's aria-controls names its id, so
      // unmounting it would dangle that IDREF) but holds no options.
      const gridEl = page.locator('[data-testid="emoji-picker-grid"]');
      await expect(gridEl).toHaveCount(1);
      await expect(options).toHaveCount(0);
      await expect(page.locator("#emoji-picker-grid")).toHaveCount(1);

      // ...and it must stop BEING a grid. `visible` + `contains text` is
      // satisfied by a 40px sliver, which is exactly what shipped: the
      // `--empty` modifier was a lone class, tied with `.emoji-picker__grid` on
      // specificity (0,1,0), and lost on source order — so the container kept
      // its `repeat(auto-fill, minmax(40px, 1fr))` tracks and the message was
      // laid out inside ONE column (measured: 44px wide, 320px tall, title
      // wrapped over eight lines in a 28px column). Both assertions below fail
      // on that build and pass on the compound-selector fix.
      await expect(gridEl).not.toHaveCSS("display", "grid");
      const emptyBox = await empty.boundingBox();
      const gridBox = await gridEl.boundingBox();
      expect(emptyBox).not.toBeNull();
      expect(gridBox).not.toBeNull();
      expect(emptyBox!.width).toBeGreaterThan(gridBox!.width * 0.6);
      await expect(live).toHaveText("No emoji found for zzqqxx", { timeout: 5000 });

      // The clear control restores the unfiltered category grid and hands focus
      // back to the field (it unmounts itself on click).
      await page.locator('[data-testid="emoji-search-clear"]').click();
      await expect.poll(async () => await options.count(), { timeout: 5000 }).toBe(unfiltered);
      await expect(input).toBeFocused();
      await expect(statusText).toHaveCount(0);
      await expect(live).toHaveText("");
    } finally {
      await browser1.close();
    }
  });

  test("the search field does not steal focus on a touch-primary device @bvt1", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_emoji_search_touch_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      // `hasTouch` is the whole point: it is what makes Chromium report
      // `(pointer: coarse)`, which is the signal the autofocus gate reads. The
      // viewport is left at the default 1280x720 ON PURPOSE, so this test
      // isolates the POINTER veto — a spec that also shrank the viewport would
      // pass even if the pointer branch were deleted.
      const page = await hostInMeeting(browser1, uiURL, meetingId, { hasTouch: true });
      expect(
        await page.evaluate(() => matchMedia("(pointer: coarse)").matches),
        "premise: the emulated context must present a coarse primary pointer",
      ).toBe(true);

      await openEmojiPicker(page);
      const input = page.locator('[data-testid="emoji-search-input"]');
      await expect(input).toBeVisible({ timeout: 5000 });

      // On touch, focusing the field raises the soft keyboard over a
      // `position: fixed; bottom: 104px` palette — so the picker must open
      // BROWSABLE, not with a keyboard covering it. Settle past the palette's
      // own 100ms focus timer before judging, then confirm focus never landed
      // in the field.
      await page.waitForTimeout(600);
      await expect(input).not.toBeFocused();

      // ...and the field is still fully usable when the user actually asks for
      // it. Suppressing autofocus must not suppress the field.
      await input.click();
      await expect(input).toBeFocused();
      await input.fill("rocket");
      await expect(page.locator('[data-testid="emoji-option-0"]')).toHaveAttribute(
        "aria-label",
        "React with rocket",
        { timeout: 10_000 },
      );
    } finally {
      await browser1.close();
    }
  });

  test("Escape peels one layer at a time: results, then query, then palette @bvt1", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_emoji_search_esc_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const page = await hostInMeeting(browser1, uiURL, meetingId);
      const palette = page.locator('[data-testid="reactions-palette"]');
      const input = await openEmojiSearch(page);

      await input.fill("smile");
      await expect(input).toHaveValue("smile");
      // The clear control renders ONLY while the query signal is non-empty, so
      // its appearance is the proof that the typed value reached Dioxus. Escape's
      // tiering branches on that signal (`query.peek()`), not on the DOM value
      // `fill` just wrote: pressing before the round-trip lands would take the
      // "already empty" branch and close the whole palette.
      await expect(page.locator('[data-testid="emoji-search-clear"]')).toBeVisible({
        timeout: 5000,
      });

      // TIER 1 — from inside the RESULTS. Enter moves focus to the top result;
      // Escape there must climb back to the field, keeping BOTH the palette and
      // the typed query. Before the review fix this Escape bubbled straight to
      // #main-container and tore the whole palette down, which contradicted the
      // picker's own "peels EXACTLY ONE surface" comment.
      const firstResult = page.locator('[data-testid="emoji-option-0"]');
      await expect(firstResult).toBeVisible({ timeout: 5000 });
      await page.keyboard.press("Enter");
      await expect(firstResult).toBeFocused();
      await page.keyboard.press("Escape");
      await expect(input).toBeFocused();
      await expect(input).toHaveValue("smile");
      await expect(palette).toBeVisible();

      // ArrowUp is the non-destructive way to make the same climb, mirroring
      // the field's own ArrowDown-into-results.
      await page.keyboard.press("ArrowDown");
      await expect(firstResult).toBeFocused();
      await page.keyboard.press("ArrowUp");
      await expect(input).toBeFocused();
      await expect(input).toHaveValue("smile");

      // TIER 2 — from the FIELD with a query: peels the query and stays put.
      await page.keyboard.press("Escape");
      await expect(input).toHaveValue("");
      await expect(palette).toBeVisible();
      await expect(input).toBeFocused();

      // TIER 3 — field now empty: bubbles to #main-container's popover chain
      // exactly as before, closing the palette and restoring focus to the
      // action-bar trigger.
      await page.keyboard.press("Escape");
      await expect(palette).toBeHidden({ timeout: 3000 });
      await expect(page.locator('[data-testid="reactions-button"]')).toBeFocused();
    } finally {
      await browser1.close();
    }
  });

  test("category tabs report no selection while searching, and picking one leaves search @bvt1", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_emoji_search_tabs_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const page = await hostInMeeting(browser1, uiURL, meetingId);
      const input = await openEmojiSearch(page);
      const tabs = page.locator('[data-testid^="emoji-group-"]');
      const smileys = page.locator('[data-testid="emoji-group-smileys-and-emotion"]');
      const flags = page.locator('[data-testid="emoji-group-flags"]');

      await expect(tabs).toHaveCount(9);
      await expect(smileys).toHaveAttribute("aria-pressed", "true");

      // While a query is active the grid shows RESULTS, not a category — so no
      // tab may claim to be pressed. An `aria-pressed="true"` on a category that
      // is not on screen is the inverted-state defect of #2123/#2135.
      await input.fill("smile");
      await expect(page.locator('[data-testid="emoji-search-status-text"]')).toBeVisible({
        timeout: 5000,
      });
      const pressedWhileSearching = await tabs.evaluateAll((els) =>
        els.map((el) => el.getAttribute("aria-pressed")),
      );
      expect(pressedWhileSearching).toEqual(new Array(9).fill("false"));
      // The visual state must agree with the ARIA state, not drift from it.
      const activeClassWhileSearching = await tabs.evaluateAll(
        (els) => els.filter((el) => el.classList.contains("active")).length,
      );
      expect(activeClassWhileSearching).toBe(0);

      // Clicking a category is the way OUT of search: the query clears and that
      // category — the one the user actually pressed — is what renders.
      await flags.click();
      await expect(input).toHaveValue("");
      await expect(flags).toHaveAttribute("aria-pressed", "true");
      await expect(smileys).toHaveAttribute("aria-pressed", "false");
      await expect(page.locator('[data-testid="emoji-search-status-text"]')).toHaveCount(0);
      await expect(page.locator('[data-testid="emoji-picker-grid"]')).toHaveAttribute(
        "aria-label",
        "Flags emoji",
      );
    } finally {
      await browser1.close();
    }
  });

  test("Enter moves into the results without sending; the result then sends @bvt1", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_emoji_search_enter_${Date.now()}`;
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const page = await hostInMeeting(browser1, uiURL, meetingId);
      const input = await openEmojiSearch(page);

      // Typing must not disturb the field: if the mount-time focus call re-fired
      // on re-render, the caret/value would be clobbered mid-word.
      await input.pressSequentially("rocket", { delay: 40 });
      await expect(input).toHaveValue("rocket");
      await expect(input).toBeFocused();

      const first = page.locator('[data-testid="emoji-option-0"]');
      await expect(first).toBeVisible({ timeout: 5000 });
      // `emoji-option-0` exists in the CATEGORY grid too, so pin the label of
      // THIS query's top hit ("rocket" is an exact name+shortcode match, and the
      // only one in the table). That is what proves the results grid — not the
      // pre-search category still on screen — is what Enter moves into; without
      // it a late re-render could swap the button out from under the focus and
      // the send below.
      await expect(first).toHaveAttribute("aria-label", "React with rocket", {
        timeout: 10_000,
      });

      // Enter FOCUSES the top result rather than sending it — activating a
      // reaction broadcasts to every attendee, so a reflexive Enter after typing
      // must not become an accidental all-hands emoji.
      await page.keyboard.press("Enter");
      await expect(first).toBeFocused();
      await expect(page.locator('[data-testid="reaction-float"]')).toHaveCount(0);

      // A SECOND Enter, now on the button, does send it — and the glyph that
      // floats is the one the search result showed.
      const glyph = ((await first.locator(".reaction-option__emoji").textContent()) || "").trim();
      expect(glyph.length).toBeGreaterThan(0);
      await page.keyboard.press("Enter");
      await expect(
        page.locator('[data-testid="reaction-float"]').filter({ hasText: glyph }).first(),
      ).toBeVisible({ timeout: 10_000 });
    } finally {
      await browser1.close();
    }
  });
});
