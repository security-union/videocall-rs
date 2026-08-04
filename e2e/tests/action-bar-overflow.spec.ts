import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E tests for the action bar overflow menu.
 *
 * The action bar (bottom toolbar in the meeting view) dynamically hides
 * secondary buttons behind a horizontal three-dot "..." popover when the
 * viewport is too narrow for all buttons to fit. "Sacred" buttons (Mic,
 * Camera, Hangup) remain visible at any width.
 *
 * Selectors:
 *   - Overflow trigger: `#overflow-menu-trigger` / `.action-bar-overflow-trigger`
 *   - Overflow popover: `.action-bar-overflow-popover`
 *   - Popover items:    `.overflow-item`
 *   - Slot wrappers:    `.action-bar-slot-wrapper[data-slot="<name>"]`
 *   - Hangup wrapper:   `.hangup-wrapper`
 */

test.describe("Action bar overflow menu", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  /** Navigate home, create+join a meeting, and wait for the call grid. */
  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `overflow_test_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("overflow-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {
          // Swallow click-after-detach: the auto-join effect has already
          // transitioned past NotJoined and unmounted the button.
        });
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  /** Hover the action bar to ensure it is visible for interaction. */
  async function hoverActionBar(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
  }

  // -- Wide viewport: no overflow trigger visible --------------------------

  test("at wide viewport all buttons are visible and no overflow trigger appears @bvt1", async ({
    page,
  }) => {
    // Default Playwright viewport is 1280x720 (Desktop Chrome), which is wide.
    await joinMeeting(page, "wide_no_overflow");
    await hoverActionBar(page);

    // Sacred buttons visible
    await expect(page.locator('.action-bar-slot-wrapper[data-slot="mic"]')).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.locator('.action-bar-slot-wrapper[data-slot="camera"]')).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.locator(".hangup-wrapper")).toBeVisible({ timeout: 5_000 });

    // Overflow trigger should NOT be visible at wide viewport
    await expect(page.locator("#overflow-menu-trigger")).not.toBeVisible({ timeout: 5_000 });
  });

  // -- Narrow viewport: overflow trigger appears ---------------------------

  test("at narrow viewport the overflow trigger appears and some buttons are hidden @bvt1", async ({
    page,
  }) => {
    await joinMeeting(page, "narrow_overflow");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    // Overflow trigger must be visible
    await expect(page.locator("#overflow-menu-trigger")).toBeVisible({ timeout: 5_000 });

    // At least one overflowable slot should be hidden from the bar.
    // We check a few known overflowable slots; at 400px at least some should be gone.
    const overflowableSlots = ["chat", "screen", "participants", "density", "diagnostics"];
    let hiddenCount = 0;
    for (const slot of overflowableSlots) {
      const wrapper = page.locator(`.action-bar-slot-wrapper[data-slot="${slot}"]`);
      if ((await wrapper.count()) > 0 && !(await wrapper.isVisible())) {
        hiddenCount++;
      }
    }
    expect(hiddenCount).toBeGreaterThan(0);
  });

  // -- Sacred buttons always visible ---------------------------------------

  test("sacred buttons (mic, camera, hangup) remain visible at narrow viewport @bvt1", async ({
    page,
  }) => {
    await joinMeeting(page, "sacred_visible");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await expect(page.locator('.action-bar-slot-wrapper[data-slot="mic"]')).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.locator('.action-bar-slot-wrapper[data-slot="camera"]')).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.locator(".hangup-wrapper")).toBeVisible({ timeout: 5_000 });
  });

  // -- Clicking the overflow trigger opens the popover ---------------------

  test("clicking the overflow trigger opens the overflow popover @bvt1", async ({ page }) => {
    await joinMeeting(page, "open_popover");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await page.locator("#overflow-menu-trigger").click();
    await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });
  });

  // -- Popover contains overflow items -------------------------------------

  test("overflow popover contains items with labels @bvt1", async ({ page }) => {
    await joinMeeting(page, "popover_items");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await page.locator("#overflow-menu-trigger").click();
    await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });

    // There should be at least one overflow item in the popover
    const items = page.locator(".action-bar-overflow-popover .overflow-item");
    await expect(items.first()).toBeVisible({ timeout: 5_000 });
    const count = await items.count();
    expect(count).toBeGreaterThan(0);
  });

  // -- Clicking an overflow item performs its action -----------------------

  test("clicking Chat in overflow popover opens the chat sidebar @bvt1", async ({ page }) => {
    await joinMeeting(page, "overflow_chat");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await page.locator("#overflow-menu-trigger").click();
    await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });

    // Find the Chat overflow item and click it
    const chatItem = page.locator(".action-bar-overflow-popover .overflow-item", {
      hasText: /Chat/i,
    });

    // If Chat is in the overflow, click it and verify the sidebar opens.
    // If Chat is NOT in the overflow at this width (unlikely at 400px but
    // possible depending on priority order), skip gracefully.
    if ((await chatItem.count()) > 0) {
      await chatItem.click();
      await expect(page.locator("#chat-sidebar")).toHaveClass(/visible/, { timeout: 5_000 });
    } else {
      test.skip();
    }
  });

  // -- Escape closes the overflow popover ----------------------------------

  test("pressing Escape closes the overflow popover @bvt1", async ({ page }) => {
    await joinMeeting(page, "escape_close");
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await page.locator("#overflow-menu-trigger").click();
    await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });

    await page.keyboard.press("Escape");
    await expect(page.locator(".action-bar-overflow-popover")).not.toBeVisible({ timeout: 3_000 });
  });

  // -- Resizing wider hides the overflow trigger ---------------------------

  test("resizing viewport wider hides the overflow trigger and restores buttons @bvt1", async ({
    page,
  }) => {
    await joinMeeting(page, "resize_wider");

    // Start narrow so overflow trigger appears
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);
    await expect(page.locator("#overflow-menu-trigger")).toBeVisible({ timeout: 5_000 });

    // Resize back to wide
    await page.setViewportSize({ width: 1280, height: 720 });
    await hoverActionBar(page);

    // Presence-before-measurement: assert the bar itself is up (a sacred button
    // is visible) FIRST, so the "trigger not visible" check below cannot pass
    // vacuously by the whole bar being hidden — it must genuinely mean the
    // overflow cleared.
    await expect(page.locator('.action-bar-slot-wrapper[data-slot="mic"]')).toBeVisible({
      timeout: 5_000,
    });

    // Overflow trigger should disappear
    await expect(page.locator("#overflow-menu-trigger")).not.toBeVisible({ timeout: 5_000 });

    // Previously hidden buttons must be visible again — this is the restore
    // regression: a slot that overflowed at 400px must un-hide once it fits,
    // not stay stuck behind a stale inline display:none.
    const chatSlot = page.locator('.action-bar-slot-wrapper[data-slot="chat"]');
    if ((await chatSlot.count()) > 0) {
      await expect(chatSlot).toBeVisible({ timeout: 5_000 });
    }
  });

  // -- Widening never re-hides buttons (issue 2044 monotonic restore) -------
  //
  // The reported bug: the action bar compacts as the window narrows but does
  // NOT expand back as it widens. Root cause was a non-monotonic fit — the
  // overflow calc budgeted each button with the CSS's *tightened* spacing at
  // ≤540px but the *full* 1.2rem spacing above 540px, so widening the window
  // ACROSS the 540px breakpoint shrank the fit budget faster than the width
  // grew and pushed already-visible buttons back into the "..." menu. Widening
  // must only ever REVEAL buttons; it may never hide more.
  //
  // We count the secondary (non-sacred) slots currently shown in the bar at a
  // width just at the breakpoint, then again at a clearly wider width straddling
  // it. On the un-fixed code the wider viewport showed FEWER buttons; the fix
  // makes the count monotonically non-decreasing as the window widens.

  test("widening the window never hides more action-bar buttons @bvt1", async ({ page }) => {
    await joinMeeting(page, "widen_monotonic");

    // Count secondary slot wrappers currently visible (not tucked into the
    // overflow menu). Hover keeps the bar expanded so only overflow — not the
    // collapse animation — governs visibility.
    async function visibleSecondaryCount(): Promise<number> {
      const wrappers = page.locator(".action-bar-slot-wrapper.slot-secondary");
      const total = await wrappers.count();
      let shown = 0;
      for (let i = 0; i < total; i++) {
        if (await wrappers.nth(i).isVisible()) {
          shown++;
        }
      }
      return shown;
    }

    // At the 540px breakpoint (tightened spacing) a batch of buttons fit.
    await page.setViewportSize({ width: 540, height: 800 });
    await hoverActionBar(page);
    await page.waitForTimeout(300); // let the rAF-throttled resize settle
    const atBreakpoint = await visibleSecondaryCount();

    // Widen clearly across the breakpoint. The un-fixed spacing jump hid MORE
    // buttons here; the fix must keep the visible count from dropping.
    await page.setViewportSize({ width: 620, height: 800 });
    await hoverActionBar(page);
    await page.waitForTimeout(300);
    const atWider = await visibleSecondaryCount();

    expect(atWider).toBeGreaterThanOrEqual(atBreakpoint);
  });

  // -- Widen restores WITHOUT hover and BEYOND the arithmetic dead zone ------
  //
  // Two field-report hypotheses in one guard:
  //  (a) No-hover: the overflow recompute is reactive to the resize signal, so
  //      buttons must come back on widen WITHOUT any pointer interaction (auto-
  //      hide defaults off, so the bar is already visible + expanded). If the
  //      recompute were gated on hover/interaction the trigger would linger.
  //  (b) Beyond the dead zone: widen to 1000px — well past the ~830px point
  //      where even the un-fixed arithmetic restores — so a *frozen viewport
  //      signal* (a listener-lifecycle ratchet) would surface as buttons never
  //      coming back at any width. Deliberately no hover, no other input.
  //  (c) FULL restoration at a reference desktop width.
  //
  // (b) is asserted as a strict INCREASE in the number of visible secondary
  // slots rather than "the trigger is gone". Those are not the same claim, and
  // the difference is what issue 2135 exposed: 1000px was chosen for the
  // reactivity property, but "trigger gone" quietly also asserted that the
  // whole default bar FITS in 1000px — an incidental property that held with
  // ~35px to spare, i.e. half a button. Adding one default slot (RaiseHand)
  // spent it, and a test about resize reactivity failed for a reason that had
  // nothing to do with resize reactivity.
  //
  // Counting visible slots pins the reactivity property directly and does not
  // care how many slots the default layout has. The "everything is back"
  // claim keeps its own step (c) at a width the bar genuinely fits, which
  // `default_action_bar_fits_entirely_at_the_e2e_wide_width` (attendants.rs)
  // verifies in milliseconds so this spec cannot be blindsided again.

  test("widening restores buttons with no hover, well beyond the dead zone @bvt1", async ({
    page,
  }) => {
    await joinMeeting(page, "widen_no_hover");

    // Overflowed slots stay in the DOM and are hidden by an `is-overflow-hidden`
    // class, so `count()` is the FULL secondary set at every width and
    // `isVisible()` is what distinguishes. That is what makes "visible === total"
    // below a real assertion rather than a tautology.
    const secondaryWrappers = page.locator(".action-bar-slot-wrapper.slot-secondary");
    async function visibleSecondaryCount(): Promise<number> {
      const total = await secondaryWrappers.count();
      let shown = 0;
      for (let i = 0; i < total; i++) {
        if (await secondaryWrappers.nth(i).isVisible()) {
          shown++;
        }
      }
      return shown;
    }

    // Narrow so the overflow trigger appears — no hover.
    await page.setViewportSize({ width: 540, height: 800 });
    await page.waitForTimeout(300); // let the rAF-throttled resize settle
    await expect(page.locator("#overflow-menu-trigger")).toBeVisible({ timeout: 5_000 });
    const atNarrow = await visibleSecondaryCount();

    // (b) Widen far past the dead zone, WITHOUT hovering or any other input.
    // The recompute alone must bring buttons back. A frozen viewport signal
    // leaves this count exactly where it was.
    await page.setViewportSize({ width: 1000, height: 800 });
    await page.waitForTimeout(300);
    const atWide = await visibleSecondaryCount();
    expect(
      atWide,
      `widening 540px -> 1000px with no hover revealed no additional secondary buttons ` +
        `(${atNarrow} -> ${atWide}); the overflow recompute is not tracking the resize`,
    ).toBeGreaterThan(atNarrow);

    // (c) At a reference desktop width the bar must be FULLY restored. Asserting
    // that every secondary wrapper is visible is strictly stronger than asserting
    // the trigger is absent — the trigger is `display:none` exactly when nothing
    // overflows, so an absent trigger is implied by, but does not imply, this.
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.waitForTimeout(300);
    const total = await secondaryWrappers.count();
    expect(total).toBeGreaterThan(0);
    expect(
      await visibleSecondaryCount(),
      `not every secondary action-bar button is visible at 1280px (${total} rendered); ` +
        `either the default layout outgrew a reference desktop viewport or a slot is stuck hidden`,
    ).toBe(total);
    await expect(page.locator("#overflow-menu-trigger")).not.toBeVisible({ timeout: 5_000 });
    // … and the representative overflowable button is shown again.
    const chatSlot = page.locator('.action-bar-slot-wrapper[data-slot="chat"]');
    if ((await chatSlot.count()) > 0) {
      await expect(chatSlot).toBeVisible({ timeout: 5_000 });
    }
  });

  // -- Light theme: popover text is readable on the dark glass surface -----

  test("overflow popover items have readable contrast in light theme @bvt1", async ({ page }) => {
    await joinMeeting(page, "light_theme_contrast");

    // Switch to light theme
    await page.evaluate(() => {
      document.documentElement.setAttribute("data-theme", "light");
    });
    await page.setViewportSize({ width: 400, height: 720 });
    await hoverActionBar(page);

    await page.locator("#overflow-menu-trigger").click();
    await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });

    // Verify overflow items use --on-dark-text (#ffffff) or a light color,
    // not the light-theme --text-primary (#1a1a1a) which is unreadable on
    // the dark glass popover surface.
    const firstItem = page.locator(".action-bar-overflow-popover .overflow-item").first();
    const color = await firstItem.evaluate((el) => getComputedStyle(el).color);
    // Parse the RGB — luminance should be high (light text on dark surface).
    const match = color.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)/);
    expect(match).not.toBeNull();
    if (match) {
      const [r, g, b] = [parseInt(match[1]), parseInt(match[2]), parseInt(match[3])];
      // Relative luminance > 0.5 means it's a light color.
      const luminance = (0.299 * r + 0.587 * g + 0.114 * b) / 255;
      expect(luminance).toBeGreaterThan(0.5);
    }
  });

  // -- Action bar never exceeds viewport width (no horizontal clip) --------

  test("action bar does not overflow the viewport at any narrow width @bvt1", async ({ page }) => {
    await joinMeeting(page, "no_horizontal_clip");
    await hoverActionBar(page);

    // Sweep through a range of widths including the dead-zone band
    // where the old two-pass algorithm could leave the bar wider than
    // the viewport.
    const widths = [320, 360, 400, 440, 480, 520, 560, 600];
    for (const w of widths) {
      await page.setViewportSize({ width: w, height: 720 });
      // Let the rAF-throttled resize settle
      await page.waitForTimeout(200);
      await hoverActionBar(page);

      const barBox = await page.locator(".video-controls-container").boundingBox();
      expect(barBox).not.toBeNull();
      if (barBox) {
        // The bar's right edge must not exceed the viewport width.
        expect(barBox.x + barBox.width).toBeLessThanOrEqual(w + 1); // 1px tolerance
      }
    }
  });

  // -- iOS UA: ScreenShare absent from overflow popover --------------------
  //
  // On iOS devices, `is_ios()` returns true (UA contains "iPhone" / "iPad" /
  // "iPod") and `visible_action_bar_slots()` filters out ScreenShare
  // (attendants.rs:1646). At narrow viewports this means the ScreenShare
  // slot must NOT appear as an overflow popover item. This test guards that
  // filter — removing the `is_ios()` argument from the overflow effect
  // (attendants.rs:2018) would re-introduce a dead popover item on iOS.
  //
  // Uses `test.use()` to set an iPhone UA string BEFORE the page navigates,
  // so the wasm `is_ios()` (cached via `OnceLock`) picks it up on first read.

  test.describe("iOS user agent", () => {
    test.use({
      userAgent:
        "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) " +
        "AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 " +
        "Mobile/15E148 Safari/604.1",
    });

    test("ScreenShare overflow item is absent on iOS @bvt1", async ({ page }) => {
      await joinMeeting(page, "ios_no_screenshare");
      await page.setViewportSize({ width: 400, height: 720 });
      await hoverActionBar(page);

      // Open the overflow popover
      await expect(page.locator("#overflow-menu-trigger")).toBeVisible({ timeout: 5_000 });
      await page.locator("#overflow-menu-trigger").click();
      await expect(page.locator(".action-bar-overflow-popover")).toBeVisible({ timeout: 5_000 });

      // The popover must NOT contain a "Screen" item (ScreenShare slot).
      const screenItem = page.locator(".action-bar-overflow-popover .overflow-item", {
        hasText: /Screen/i,
      });
      await expect(screenItem).toHaveCount(0);

      // Guard against vacuous pass: at least one other overflow item must be
      // present (e.g. Chat), proving the popover isn't simply empty.
      const anyItem = page.locator(".action-bar-overflow-popover .overflow-item");
      expect(await anyItem.count()).toBeGreaterThan(0);
    });
  });
});
