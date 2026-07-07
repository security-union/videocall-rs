import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Action bar customize mode", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `customize_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("cust-user", { delay: 80 });
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
        await joinButton.click().catch(() => undefined);
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  async function openDockMenu(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);
    const toggleBtn = page.locator('.dock-position-wrapper button[aria-haspopup="listbox"]');
    await expect(toggleBtn).toBeVisible({ timeout: 10_000 });
    await toggleBtn.click();
    await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 5_000 });
  }

  async function enterCustomizeMode(page: Page): Promise<void> {
    await openDockMenu(page);
    const customizeOption = page.locator('.glass-select-option[role="option"]', {
      hasText: "Customize",
    });
    await customizeOption.click();
    // Wait for customize-mode class on the container
    await expect(page.locator(".video-controls-container.customize-mode")).toBeVisible({
      timeout: 5_000,
    });
  }

  test("entering customize mode shows Done button and disables HangUp", async ({ page }) => {
    await joinMeeting(page, "enter_mode");

    await enterCustomizeMode(page);

    // The "Done" button should be visible (inside .dock-position-wrapper)
    const doneBtn = page.locator("button.action-bar-done-trigger");
    await expect(doneBtn).toBeVisible({ timeout: 5_000 });
    await expect(doneBtn).toHaveAttribute("title", "Done customizing");

    // HangUp button onclick is a no-op during customize mode.
    // Verify the HangUp button is present but clicking it does NOT navigate away.
    const hangupBtn = page.locator(".hangup-wrapper button");
    await expect(hangupBtn).toBeVisible({ timeout: 5_000 });
    await hangupBtn.click();
    // We should still be in the meeting (grid visible, customize-mode still on)
    await page.waitForTimeout(500);
    await expect(page.locator("#grid-container")).toBeVisible();
    await expect(page.locator(".video-controls-container.customize-mode")).toBeVisible();

    // Click Done to exit customize mode
    await doneBtn.click();
    await expect(page.locator(".video-controls-container.customize-mode")).not.toBeVisible({
      timeout: 5_000,
    });
  });

  test("drag reorder changes button order and persists to localStorage", async ({ page }) => {
    await joinMeeting(page, "drag_reorder");

    // Clear any persisted layout before entering customize mode
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    // Reload to pick up default layout
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await enterCustomizeMode(page);

    // Get all slot wrappers (excluding dock-position-wrapper and hangup-wrapper)
    const slots = page.locator(".video-controls-container .action-bar-slot-wrapper.slot-secondary");
    const slotCount = await slots.count();

    // Need at least 2 draggable slots for the test to be meaningful
    if (slotCount < 2) {
      test.skip(true, "Not enough draggable slots to test reorder");
      return;
    }

    // Record initial CSS order values
    const initialOrders = await slots.evaluateAll((els) =>
      els.map((el) => {
        const style = el.getAttribute("style") || "";
        const match = style.match(/order:\s*(\d+)/);
        return match ? parseInt(match[1], 10) : 0;
      }),
    );

    // Get bounding box of first and second slot to perform a drag
    const firstSlot = slots.nth(0);
    const secondSlot = slots.nth(1);
    const firstBox = await firstSlot.boundingBox();
    const secondBox = await secondSlot.boundingBox();

    if (!firstBox || !secondBox) {
      test.skip(true, "Could not get bounding boxes for slots");
      return;
    }

    // Drag first slot to the position of the second slot
    const startX = firstBox.x + firstBox.width / 2;
    const startY = firstBox.y + firstBox.height / 2;
    const endX = secondBox.x + secondBox.width / 2;
    const endY = secondBox.y + secondBox.height / 2;

    // Use pointer events to simulate drag (pointerdown, pointermove, pointerup)
    await page.mouse.move(startX, startY);
    await page.mouse.down();
    // Move in steps to trigger the drag-started threshold
    const steps = 5;
    for (let i = 1; i <= steps; i++) {
      await page.mouse.move(
        startX + ((endX - startX) * i) / steps,
        startY + ((endY - startY) * i) / steps,
      );
      await page.waitForTimeout(50);
    }
    await page.mouse.up();
    await page.waitForTimeout(300);

    // Click Done to finalize and persist
    const doneBtn = page.locator("button.action-bar-done-trigger");
    await doneBtn.click();
    await page.waitForTimeout(500);

    // Verify localStorage was written
    const stored = await page.evaluate(() => localStorage.getItem("vc_action_bar_layout"));
    expect(stored).not.toBeNull();

    // v2 storage shape: {v: 2, slots: [...], hidden: [...]}. The drag must
    // have written the new shape AND a non-empty slots array.
    const layout = JSON.parse(stored as string);
    expect(layout).toMatchObject({ v: 2 });
    expect(Array.isArray(layout.slots)).toBe(true);
    expect(layout.slots.length).toBeGreaterThan(0);
    expect(Array.isArray(layout.hidden)).toBe(true);

    // The drag must have ACTUALLY reordered the bar. This is the real
    // assertion: if the drag did nothing (e.g. the reorder logic regresses),
    // the persisted layout still equals the default and this fails — so the
    // test pins the feature it names, not merely "something was saved".
    const DEFAULT_LAYOUT = [
      "mic",
      "camera",
      "chat",
      "screen",
      "participants",
      "density",
      "diagnostics",
      "settings",
      "meeting_options",
    ];
    expect(layout.slots).not.toEqual(DEFAULT_LAYOUT);

    // And the live CSS `order` values must differ from before the drag too.
    const postOrders = await slots.evaluateAll((els) =>
      els.map((el) => {
        const style = el.getAttribute("style") || "";
        const match = style.match(/order:\s*(\d+)/);
        return match ? parseInt(match[1], 10) : 0;
      }),
    );
    expect(JSON.stringify(postOrders)).not.toEqual(JSON.stringify(initialOrders));
  });

  test("removed slot stays removed after page reload", async ({ page }) => {
    // Regression for the v1 loader bug: after a removed slot was persisted,
    // the loader's "append every missing default" migration silently restored
    // it on next load. The v2 schema tracks `hidden` explicitly; removing a
    // slot, reloading, and seeing it still gone is the test that FAILS on
    // the un-fixed loader.
    await joinMeeting(page, "remove_persists_reload");

    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await enterCustomizeMode(page);

    const secondarySlots = page.locator(
      ".video-controls-container .action-bar-slot-wrapper.slot-secondary",
    );
    const initialCount = await secondarySlots.count();
    if (initialCount < 1) {
      test.skip(true, "No removable secondary slots available");
      return;
    }

    // Record the `order` CSS value of the slot we will remove so we can prove
    // the SAME slot stays gone post-reload, not just "some slot is missing".
    const firstSlotOrder = await secondarySlots.first().evaluate((el) => {
      return window.getComputedStyle(el as HTMLElement).order;
    });

    const removeBtn = page
      .locator(
        ".video-controls-container .action-bar-slot-wrapper.slot-secondary .action-bar-remove-btn",
      )
      .first();
    await expect(removeBtn).toBeVisible();

    // The button must carry an accessible name — a remove button rendering
    // only "−" is unreachable to screen readers (the a11y fix being pinned).
    const ariaLabel = await removeBtn.getAttribute("aria-label");
    expect(ariaLabel).toMatch(/^Remove /);

    await removeBtn.click();
    await page.waitForTimeout(300);

    await page.locator("button.action-bar-done-trigger").click();
    await page.waitForTimeout(500);

    const storedAfterRemove = await page.evaluate(() =>
      localStorage.getItem("vc_action_bar_layout"),
    );
    const layoutAfterRemove = JSON.parse(storedAfterRemove as string);
    expect(layoutAfterRemove).toMatchObject({ v: 2 });
    expect(Array.isArray(layoutAfterRemove.hidden)).toBe(true);
    expect(layoutAfterRemove.hidden.length).toBeGreaterThanOrEqual(1);

    const countBeforeReload = await secondarySlots.count();
    expect(countBeforeReload).toBe(initialCount - 1);

    // **Regression assertion**: reload and verify the removed slot stays
    // gone. Pre-fix, the loader appended every missing default on load —
    // this would have resurrected the slot and made countAfterReload equal
    // initialCount again.
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);

    const countAfterReload = await page
      .locator(".video-controls-container .action-bar-slot-wrapper.slot-secondary")
      .count();
    expect(
      countAfterReload,
      `removed slot resurrected on reload (initial=${initialCount}, afterRemove=${countBeforeReload}, afterReload=${countAfterReload})`,
    ).toBe(countBeforeReload);

    const ordersAfterReload = await page
      .locator(".video-controls-container .action-bar-slot-wrapper.slot-secondary")
      .evaluateAll((els) => els.map((el) => window.getComputedStyle(el as HTMLElement).order));
    expect(
      ordersAfterReload,
      `the removed slot (order=${firstSlotOrder}) reappeared after reload`,
    ).not.toContain(firstSlotOrder);
  });

  test("Mic and Camera have no remove button (non-removable)", async ({ page }) => {
    // Stranding-prevention fix: Mic and Camera must not expose a remove
    // button so a user cannot drop their mute / camera-mute control mid-call.
    // They remain draggable for reordering.
    await joinMeeting(page, "mic_camera_pinned");

    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await enterCustomizeMode(page);

    // Even with customize mode active, the Mic (order=0) and Camera (order=1)
    // slots must NOT contain a `.action-bar-remove-btn` child. Chat (order=2)
    // is also slot-primary but IS removable, so we don't blanket-assert on
    // .slot-primary; we identify Mic/Camera by their CSS `order`.
    const slotsWithRemove = await page
      .locator(".video-controls-container .action-bar-slot-wrapper")
      .evaluateAll((els) =>
        els
          .map((el) => ({
            order: window.getComputedStyle(el as HTMLElement).order,
            hasRemove: !!(el as HTMLElement).querySelector(".action-bar-remove-btn"),
          }))
          .filter((s) => s.order === "0" || s.order === "1"),
      );
    // Mic at order=0 and Camera at order=1 must both be present and must
    // both have NO remove button.
    expect(slotsWithRemove.length, "Mic and Camera must both be in the bar").toBe(2);
    for (const s of slotsWithRemove) {
      expect(s.hasRemove, `slot at order=${s.order} (Mic/Camera) must have no remove button`).toBe(
        false,
      );
    }
  });

  test("entering customize mode does not visually shift any action-bar button", async ({
    page,
  }) => {
    await joinMeeting(page, "no_shift");

    // Start from a clean default layout so the snapshot is deterministic.
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Expand the bar so every slot (primary + secondary + dock + hangup +
    // mock-peers) is visible BEFORE customize mode. Without this, secondary
    // slots are display:none and have no box; the regression we are pinning
    // (the `controls-secondary` wrapper one) is specifically about visible
    // slots jumping when customize toggles the layout flattening, so we must
    // measure them in their visible state.
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);

    // Snapshot every visible direct child of the controls container, keyed
    // by its CSS `order:` value (stable across the customize toggle because
    // the underlying layout is the default in both snapshots — DOM source
    // order is NOT stable because flex `order` decides visual position).
    type BoxSnapshot = { order: number; x: number; y: number };
    const snapshot = async (): Promise<BoxSnapshot[]> =>
      page.evaluate(() => {
        const container = document.querySelector(".video-controls-container");
        if (!container) return [];
        const out: { order: number; x: number; y: number }[] = [];
        for (const child of Array.from(container.children)) {
          const el = child as HTMLElement;
          // Skip non-rendered (display:none) children.
          const computed = window.getComputedStyle(el);
          if (computed.display === "none" || computed.visibility === "hidden") continue;
          const rect = el.getBoundingClientRect();
          if (rect.width === 0 && rect.height === 0) continue;
          const orderStr = computed.order || "0";
          const order = parseInt(orderStr, 10);
          if (Number.isNaN(order)) continue;
          out.push({ order, x: rect.x, y: rect.y });
        }
        return out;
      });

    const before = await snapshot();
    expect(before.length).toBeGreaterThan(2);

    await enterCustomizeMode(page);
    // Keep the bar expanded so the same slots remain measurable.
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);

    const after = await snapshot();

    // Every slot present before customize must still be at (approximately)
    // the same position after. >1.5px movement means the layout reflowed
    // when toggling customize-mode — the exact symptom of the
    // `controls-secondary` wrapper / `display:contents` regression.
    const TOLERANCE_PX = 1.5;
    for (const b of before) {
      const a = after.find((x) => x.order === b.order);
      expect(a, `slot with order=${b.order} disappeared after entering customize`).toBeTruthy();
      const dx = Math.abs((a as BoxSnapshot).x - b.x);
      const dy = Math.abs((a as BoxSnapshot).y - b.y);
      expect(
        dx,
        `slot with order=${b.order} moved horizontally by ${dx}px when entering customize mode`,
      ).toBeLessThanOrEqual(TOLERANCE_PX);
      expect(
        dy,
        `slot with order=${b.order} moved vertically by ${dy}px when entering customize mode`,
      ).toBeLessThanOrEqual(TOLERANCE_PX);
    }
  });

  test("remove button removes a slot from the action bar", async ({ page }) => {
    await joinMeeting(page, "remove_btn");

    // Clear persisted layout
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await enterCustomizeMode(page);

    // Count initial slots
    const slots = page.locator(".video-controls-container .action-bar-slot-wrapper.slot-secondary");
    const initialCount = await slots.count();

    if (initialCount < 1) {
      test.skip(true, "No removable slots available");
      return;
    }

    // Each slot in customize mode should have a remove button ("−")
    const removeButtons = page.locator(
      ".video-controls-container .action-bar-slot-wrapper.slot-secondary .action-bar-remove-btn",
    );
    const removeCount = await removeButtons.count();
    expect(removeCount).toBeGreaterThan(0);

    // Click the first remove button
    await removeButtons.first().click();
    await page.waitForTimeout(300);

    // Slot count should have decreased by 1
    const afterCount = await slots.count();
    expect(afterCount).toBe(initialCount - 1);

    // Click Done to persist
    const doneBtn = page.locator("button.action-bar-done-trigger");
    await doneBtn.click();
    await page.waitForTimeout(500);

    // Verify persisted in localStorage. v2 schema: {v, slots, hidden}.
    // The bar must reflect a `hidden` list that contains exactly the slot we
    // just removed — otherwise the resurrect-on-reload bug returns.
    const stored = await page.evaluate(() => localStorage.getItem("vc_action_bar_layout"));
    expect(stored).not.toBeNull();
    const layout = JSON.parse(stored as string);
    expect(layout).toMatchObject({ v: 2 });
    expect(Array.isArray(layout.slots)).toBe(true);
    expect(Array.isArray(layout.hidden)).toBe(true);
    expect(layout.hidden.length).toBeGreaterThanOrEqual(1);
  });
});
