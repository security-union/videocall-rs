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

    // Record initial slot sequence by slug (DOM order == visual order in the
    // keyed render model).
    const initialSequence = await slots.evaluateAll((els) =>
      els.map((el) => (el as HTMLElement).getAttribute("data-slot") || ""),
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
      "screen",
      "participants",
      "density",
      "diagnostics",
      "settings",
      "meeting_options",
    ];
    expect(layout.slots).not.toEqual(DEFAULT_LAYOUT);

    // The visible slot sequence must have changed too.
    const postSequence = await slots.evaluateAll((els) =>
      els.map((el) => (el as HTMLElement).getAttribute("data-slot") || ""),
    );
    expect(JSON.stringify(postSequence)).not.toEqual(JSON.stringify(initialSequence));

    // Acceptance guard for #1761: after drag reorder, DOM traversal order
    // must match visual order for the draggable slot set.
    const dragOrderParity = await slots.evaluateAll((els) => {
      const entries = els.map((el, domIdx) => {
        const node = el as HTMLElement;
        const rect = node.getBoundingClientRect();
        return {
          slot: node.getAttribute("data-slot") || "",
          domIdx,
          x: rect.left + rect.width / 2,
          y: rect.top + rect.height / 2,
        };
      });
      const dom = entries.map((e) => e.slot);
      if (entries.length <= 1) {
        return { dom, visual: dom };
      }
      const nav = (els[0] as HTMLElement).closest(
        ".video-controls-container",
      ) as HTMLElement | null;
      const flexDirection = nav ? window.getComputedStyle(nav).flexDirection : "row";
      const isRow = flexDirection.startsWith("row");
      const isReverse = flexDirection.endsWith("reverse");
      const visual = [...entries]
        .sort((a, b) => {
          if (isRow) {
            return isReverse ? b.x - a.x : a.x - b.x;
          }
          return isReverse ? b.y - a.y : a.y - b.y;
        })
        .map((e) => e.slot);
      return { dom, visual };
    });
    expect(
      dragOrderParity.dom,
      `after drag reorder, DOM order must match visual order (DOM=${dragOrderParity.dom.join(" -> ")}, visual=${dragOrderParity.visual.join(" -> ")})`,
    ).toEqual(dragOrderParity.visual);
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

    // Record the slug of the slot we remove so we can prove the SAME slot
    // stays gone post-reload, not just "some slot is missing".
    const firstSlotSlug = await secondarySlots
      .first()
      .evaluate((el) => (el as HTMLElement).getAttribute("data-slot") || "");

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

    const slotsAfterReload = await page
      .locator(".video-controls-container .action-bar-slot-wrapper.slot-secondary")
      .evaluateAll((els) => els.map((el) => (el as HTMLElement).getAttribute("data-slot") || ""));
    expect(
      slotsAfterReload,
      `the removed slot (slot=${firstSlotSlug}) reappeared after reload`,
    ).not.toContain(firstSlotSlug);
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

    // Even with customize mode active, Mic and Camera slots must NOT contain
    // a `.action-bar-remove-btn` child.
    const slotsWithRemove = await page
      .locator(".video-controls-container .action-bar-slot-wrapper")
      .evaluateAll((els) =>
        els
          .map((el) => ({
            slot: (el as HTMLElement).getAttribute("data-slot") || "",
            hasRemove: !!(el as HTMLElement).querySelector(".action-bar-remove-btn"),
          }))
          .filter((s) => s.slot === "mic" || s.slot === "camera"),
      );
    // Mic and Camera must both be present and must
    // both have NO remove button.
    expect(slotsWithRemove.length, "Mic and Camera must both be in the bar").toBe(2);
    for (const s of slotsWithRemove) {
      expect(s.hasRemove, `slot=${s.slot} (Mic/Camera) must have no remove button`).toBe(false);
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

    // Snapshot every visible direct child of the controls container keyed by
    // stable identifiers (`data-slot` for slots, semantic tags for fixed
    // wrappers) so comparisons are robust without CSS `order`.
    type BoxSnapshot = { tag: string; x: number; y: number };
    const snapshot = async (): Promise<BoxSnapshot[]> =>
      page.evaluate(() => {
        const container = document.querySelector(".video-controls-container");
        if (!container) return [];
        const out: { tag: string; x: number; y: number }[] = [];
        for (const child of Array.from(container.children)) {
          const el = child as HTMLElement;
          // Skip non-rendered (display:none) children.
          const computed = window.getComputedStyle(el);
          if (computed.display === "none" || computed.visibility === "hidden") continue;
          const rect = el.getBoundingClientRect();
          if (rect.width === 0 && rect.height === 0) continue;
          const dataSlot = el.getAttribute("data-slot");
          const cls = el.className || "";
          let tag = dataSlot ?? "";
          if (!tag) {
            if (cls.includes("dock-position-wrapper")) tag = "__done_or_dock";
            else if (cls.includes("hangup-wrapper")) tag = "__hangup";
            else if (cls.includes("action-bar-mock-peers-wrapper")) tag = "__mockpeers";
            else tag = `__unknown(${cls})`;
          }
          out.push({ tag, x: rect.x, y: rect.y });
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
      const a = after.find((x) => x.tag === b.tag);
      expect(a, `slot ${b.tag} disappeared after entering customize`).toBeTruthy();
      const dx = Math.abs((a as BoxSnapshot).x - b.x);
      const dy = Math.abs((a as BoxSnapshot).y - b.y);
      expect(
        dx,
        `slot ${b.tag} moved horizontally by ${dx}px when entering customize mode`,
      ).toBeLessThanOrEqual(TOLERANCE_PX);
      expect(
        dy,
        `slot ${b.tag} moved vertically by ${dy}px when entering customize mode`,
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

  test("keyboard arrow keys reorder a focused slot and persist to localStorage", async ({
    page,
  }) => {
    // WCAG 2.1.1 keyboard equivalent for the pointer drag-to-reorder feature.
    // Focus on any slot's real `<button>` and pressing Arrow keys moves that
    // slot within the bar; the change must persist to v2 storage AND update
    // the aria-live region so screen-reader users hear the new position.
    // Reverting either the onkeydown handler or the `data-slot` attribute
    // hook on the wrappers breaks one of these assertions.
    await joinMeeting(page, "kbd_reorder");

    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await enterCustomizeMode(page);

    // Every customizable slot wrapper carries a `data-slot` attribute that
    // the nav-level onkeydown resolves via `closest()`. The wrapper is
    // deliberately NOT focusable (see the "wrappers are not focusable"
    // test) — focus lives on the inner button so a11y users get exactly
    // one tab stop per slot.
    const screenWrapper = page.locator(
      '.video-controls-container .action-bar-slot-wrapper[data-slot="screen"]',
    );
    await expect(screenWrapper).toBeVisible({ timeout: 5_000 });
    const screenInnerButton = screenWrapper.locator("> button.video-control-button").first();
    await expect(screenInnerButton).toBeVisible({ timeout: 5_000 });

    // Capture the original slot order so the after-state is a real delta,
    // not just "some value was saved" (mutation-sensitivity).
    const readOrder = async () =>
      page
        .locator(".video-controls-container .action-bar-slot-wrapper[data-slot]")
        .evaluateAll((els) =>
          els
            .map((el) => ({
              slot: el.getAttribute("data-slot") as string,
              order: parseInt(window.getComputedStyle(el as HTMLElement).order || "0", 10),
            }))
            .sort((a, b) => a.order - b.order)
            .map((s) => s.slot),
        );
    const before = await readOrder();
    expect(before.length).toBeGreaterThan(2);

    // Focus Screen share's inner button and press Right arrow. The event bubbles to
    // the nav's onkeydown, which resolves the slot via `.closest([data-slot])`.
    await screenInnerButton.focus();
    await expect(screenInnerButton).toBeFocused();
    await page.keyboard.press("ArrowRight");
    await page.waitForTimeout(150);

    const after = await readOrder();
    // The *order in which Screen share appears* must have moved by exactly one to
    // the right (single-step per key — a live-tester report said arrows
    // could "jump to position 9 then walk back" when OS auto-repeat or
    // modifier keys were involved; the handler now blocks both, so a single
    // press moves by exactly one).
    const beforeScreenIdx = before.indexOf("screen");
    const afterScreenIdx = after.indexOf("screen");
    expect(
      afterScreenIdx,
      `Screen share did not move right by exactly one on a single ArrowRight (before=${beforeScreenIdx}, after=${afterScreenIdx})`,
    ).toBe(beforeScreenIdx + 1);

    // Focus must stay on the moved slot so Tab continues from that control
    // instead of restarting navigation from the beginning of the bar.
    const movedScreenButton = page
      .locator('.video-controls-container .action-bar-slot-wrapper[data-slot="screen"] > button')
      .first();
    await expect(movedScreenButton).toBeFocused({ timeout: 2_000 });

    // The keyboard move must persist without needing to press Done — every
    // arrow keystroke saves. Verifies the handler calls save_action_bar_layout.
    const stored = await page.evaluate(() => localStorage.getItem("vc_action_bar_layout"));
    expect(stored).not.toBeNull();
    const layout = JSON.parse(stored as string);
    expect(layout).toMatchObject({ v: 2 });
    expect(layout.slots).toEqual(after);
    // The persisted layout must actually differ from the default — mutation
    // check for the save call.
    expect(layout.slots).not.toEqual(before);

    // Screen-reader announcement must reference the new position.
    // The live regions are direct children of `.controls`, siblings of the
    // `nav.video-controls-container` — NOT descendants of the nav.
    const liveRegions = page.locator(
      '.controls .visually-hidden[role="status"][aria-live="polite"]',
    );
    const liveTexts = await liveRegions.allTextContents();
    const combined = liveTexts.join(" | ");
    expect(combined).toMatch(new RegExp(`Screen share moved to position ${afterScreenIdx + 1} of `));

    // ArrowLeft at the leftmost slot must NOT overflow into a negative
    // index (clamp behaviour) — the announcement should say "already at
    // position 1".
    const micInner = page
      .locator('.video-controls-container .action-bar-slot-wrapper[data-slot="mic"] > button')
      .first();
    await micInner.focus();
    await page.keyboard.press("ArrowLeft");
    await page.waitForTimeout(150);
    const afterMic = await readOrder();
    expect(afterMic.indexOf("mic")).toBe(0);
    const liveTexts2 = await liveRegions.allTextContents();
    expect(liveTexts2.join(" | ")).toMatch(/Microphone is already at position 1 of/);

    // Close the persistence claim end-to-end: reload the page and confirm
    // the moved slot is still where we left it.  Asserting localStorage
    // alone only proves `save_action_bar_layout` wrote — it doesn't prove
    // the migration loader reads it back into the same visual order.
    // `after` is captured just after the ArrowRight move; the subsequent
    // ArrowLeft on Mic is a no-op (clamp at index 0), so the on-reload
    // visual order must equal `after`.
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
    // Re-enter customize mode so all slots (including secondary ones that
    // are otherwise `display:none`) are visible for the `readOrder` walk.
    await enterCustomizeMode(page);
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(200);
    const afterReload = await readOrder();
    expect(
      afterReload,
      `Layout did not survive reload: expected ${after.join(",")} but got ${afterReload.join(",")}`,
    ).toEqual(after);
  });

  test("modifier + Arrow does NOT reorder a slot (Cmd/Ctrl+Arrow is a browser shortcut)", async ({
    page,
  }) => {
    // Live-tester report: pressing Cmd+ArrowRight (macOS "jump to end of
    // line") produced a slot jump to position 9 because the handler read
    // that as End. Any modifier now cancels the reorder — the browser
    // keeps its own shortcut behaviour instead.
    await joinMeeting(page, "kbd_no_modifier_reorder");
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
    await enterCustomizeMode(page);

    const screenInner = page
      .locator('.video-controls-container .action-bar-slot-wrapper[data-slot="screen"] > button')
      .first();
    await screenInner.focus();

    // Try every reasonable modifier + ArrowRight combination. None must move
    // Screen share and none must persist a layout change.
    for (const mod of ["Meta", "Control", "Alt", "Shift"] as const) {
      await page.keyboard.press(`${mod}+ArrowRight`);
      await page.waitForTimeout(80);
    }

    const order = await page
      .locator(".video-controls-container .action-bar-slot-wrapper[data-slot]")
      .evaluateAll((els) =>
        els
          .map((el) => ({
            slot: el.getAttribute("data-slot") as string,
            order: parseInt(window.getComputedStyle(el as HTMLElement).order || "0", 10),
          }))
          .sort((a, b) => a.order - b.order)
          .map((s) => s.slot),
      );
    expect(order.indexOf("screen")).toBe(2); // still at default position 3 (0-indexed 2)

    // Nothing was persisted (storage still absent or reflects default).
    const stored = await page.evaluate(() => localStorage.getItem("vc_action_bar_layout"));
    if (stored) {
      const layout = JSON.parse(stored);
      expect(layout.slots?.[2]).toBe("screen");
    }
  });

  test("customize-mode slot wrappers do NOT add a second tab stop", async ({ page }) => {
    // Live-tester report: "focus only moves after Tab twice". Root cause was
    // wrapper `tabindex=0` layered on top of the inner button that is
    // already a real `<button>` — two tab stops per slot. The fix removed
    // tabindex from the wrapper entirely; the wrapper must expose NO
    // tabindex attribute at all, in either mode.
    await joinMeeting(page, "kbd_single_tab_stop");
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Snapshot outside customize mode first: hover to reveal all slots.
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);
    const wrappers = page.locator(".video-controls-container .action-bar-slot-wrapper[data-slot]");
    const count = await wrappers.count();
    expect(count).toBeGreaterThan(0);
    const attrsBefore = await wrappers.evaluateAll((els) =>
      els.map((el) => el.getAttribute("tabindex")),
    );
    for (const t of attrsBefore) {
      expect(t, "wrapper must have no tabindex outside customize mode").toBeNull();
    }

    // Same guarantee in customize mode: NO extra tab stop is added.
    await enterCustomizeMode(page);
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);
    const attrsAfter = await wrappers.evaluateAll((els) =>
      els.map((el) => el.getAttribute("tabindex")),
    );
    for (const t of attrsAfter) {
      expect(t, "wrapper must NOT add a tab stop in customize mode").toBeNull();
    }
  });

  test("every customizable inner button is keyboard-focusable in customize mode", async ({
    page,
  }) => {
    // Live-tester report: "Tab doesn't work for screen share (only for its
    // remove)". Root cause was ScreenShareButton being called with
    // `disabled: is_disabled || customize_mode()`, so the HTML `disabled`
    // attribute stripped the button from the tab order in customize mode.
    // No other slot did that. Guard the whole set: for every slot present
    // in the bar in customize mode, the inner main button must NOT be
    // disabled and MUST accept programmatic focus (a disabled button
    // silently rejects `.focus()` — Playwright's toBeFocused fails).
    await joinMeeting(page, "kbd_all_slots_focusable");
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await enterCustomizeMode(page);
    // Hover to expand so secondary slots exist in the DOM.
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);

    const slotWrappers = page.locator(
      ".video-controls-container .action-bar-slot-wrapper[data-slot]",
    );
    const slotCount = await slotWrappers.count();
    expect(slotCount).toBeGreaterThan(2);

    for (let i = 0; i < slotCount; i++) {
      const wrapper = slotWrappers.nth(i);
      const slotName = await wrapper.getAttribute("data-slot");
      const mainBtn = wrapper.locator("> button.video-control-button").first();
      // The main control button must exist (every slot renders one) and
      // must NOT carry the HTML `disabled` attribute in customize mode.
      await expect(
        mainBtn,
        `slot="${slotName}" is missing its main .video-control-button`,
      ).toBeVisible({ timeout: 3_000 });
      const disabled = await mainBtn.evaluate((b) => (b as HTMLButtonElement).disabled);
      // Mic/Camera may legitimately be `disabled` if the underlying device
      // isn't available (see MicButton/CameraButton `disabled: !available`).
      // On the E2E stack the browser exposes fake devices so `available` is
      // true for both — but be resilient to a headless quirk by only pinning
      // the non-hardware slots strictly. ScreenShare, PeerList,
      // DensityMode, Diagnostics, DeviceSettings, MeetingOptions must never
      // be disabled in customize mode.
      const hardwareGated = slotName === "mic" || slotName === "camera";
      if (!hardwareGated) {
        expect(
          disabled,
          `slot="${slotName}" main button is disabled in customize mode; Tab will skip it`,
        ).toBe(false);
      }

      // Programmatic focus must land on the button — a disabled button
      // silently refuses focus, which is precisely how Tab skipped Screen
      // Share before the fix.
      if (!disabled) {
        await mainBtn.focus();
        await expect(
          mainBtn,
          `slot="${slotName}" main button did not accept keyboard focus`,
        ).toBeFocused({ timeout: 2_000 });
      }
    }
  });

  test("Tab order follows the visual left-to-right bar order after keyboard reorder", async ({
    page,
  }) => {
    // Regression guard for customized layouts: after a real keyboard reorder,
    // the DOM (Tab) sequence must still match the visual left-to-right order.
    // This fails on the old source-order-only rendering where only CSS order
    // changed after reorder.
    await joinMeeting(page, "kbd_tab_order_matches_visual");
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await enterCustomizeMode(page);
    // Hover to expand so all slots render.
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);

    const readSlotOrder = async () =>
      page
        .locator(".video-controls-container .action-bar-slot-wrapper[data-slot]")
        .evaluateAll((els) =>
          els
            .map((el) => ({
              slot: el.getAttribute("data-slot") as string,
              order: parseInt(window.getComputedStyle(el as HTMLElement).order || "0", 10),
            }))
            .sort((a, b) => a.order - b.order)
            .map((s) => s.slot),
        );

    const before = await readSlotOrder();
    const screenBtn = page
      .locator('.video-controls-container .action-bar-slot-wrapper[data-slot="screen"] > button')
      .first();
    await expect(screenBtn).toBeVisible({ timeout: 5_000 });
    await screenBtn.focus();
    await expect(screenBtn).toBeFocused();
    await page.keyboard.press("ArrowRight");
    await page.waitForTimeout(150);

    const after = await readSlotOrder();
    expect(after.indexOf("screen")).toBe(before.indexOf("screen") + 1);
    expect(after).not.toEqual(before);

    // Gather (DOM index, visual order, tag) for every focusable button
    // inside the controls container. Then assert that iterating DOM order
    // yields the same sequence as sorting by visual `order:`.
    type Btn = { domIdx: number; order: number; tag: string };
    const buttons: Btn[] = await page.evaluate(() => {
      const container = document.querySelector(".video-controls-container");
      if (!container) return [];
      // All native <button>s in DOM order; those inside disabled or hidden
      // wrappers are naturally skipped by focus() but we still want them
      // in the assertion because the user sees them in the visual bar.
      const out: { domIdx: number; order: number; tag: string }[] = [];
      const allBtns = Array.from(container.querySelectorAll("button"));
      allBtns.forEach((btn, domIdx) => {
        // Walk up to the direct child of .video-controls-container to read
        // its computed `order:` (which is what CSS uses for visual layout).
        let el: HTMLElement | null = btn;
        while (el && el.parentElement !== container) el = el.parentElement;
        if (!el) return;
        const computed = window.getComputedStyle(el);
        if (computed.display === "none") return;
        const orderStr = computed.order || "0";
        const order = parseInt(orderStr, 10);
        if (Number.isNaN(order)) return;
        // Use a stable tag: data-slot on slot wrappers, or the wrapper class
        // for fixed items.
        const dataSlot = el.getAttribute("data-slot");
        const cls = el.className || "";
        let tag = dataSlot ?? "";
        if (!tag) {
          if (cls.includes("dock-position-wrapper")) tag = "__done_or_dock";
          else if (cls.includes("hangup-wrapper")) tag = "__hangup";
          else if (cls.includes("action-bar-mock-peers-wrapper")) tag = "__mockpeers";
          else tag = `__unknown(${cls})`;
        }
        // Only keep the FIRST button per wrapper (main control button) —
        // the remove `−` buttons are a secondary tab stop inside the same
        // wrapper and would duplicate the entry.
        if (out.some((b) => b.tag === tag)) return;
        out.push({ domIdx, order, tag });
      });
      return out;
    });

    expect(buttons.length).toBeGreaterThan(3);

    // Assert: sort by DOM index and sort by visual order produce IDENTICAL
    // tag sequences. If Done or MockPeers regresses back to source order
    // between DensityMode and Diagnostics, this fails because the DOM
    // sequence would contain "__done_or_dock" before "diagnostics" while
    // the visual sequence contains "diagnostics" before "__done_or_dock".
    const byDom = [...buttons].sort((a, b) => a.domIdx - b.domIdx).map((b) => b.tag);
    const byVisual = [...buttons].sort((a, b) => a.order - b.order).map((b) => b.tag);
    expect(
      byDom,
      `Tab order (DOM) does not match visual order (CSS order:).\n` +
        `  Tab visits:  ${byDom.join(" → ")}\n` +
        `  Visual bar:  ${byVisual.join(" → ")}`,
    ).toEqual(byVisual);

    // Explicit spot-check for the exact regression the live tester hit:
    // Done must NOT appear between DensityMode and Diagnostics in DOM.
    const domIdxDensity = buttons.find((b) => b.tag === "density")?.domIdx;
    const domIdxDiag = buttons.find((b) => b.tag === "diagnostics")?.domIdx;
    const domIdxDone = buttons.find((b) => b.tag === "__done_or_dock")?.domIdx;
    if (
      typeof domIdxDensity === "number" &&
      typeof domIdxDiag === "number" &&
      typeof domIdxDone === "number"
    ) {
      expect(
        domIdxDone,
        "Done wrapper appears in DOM between DensityMode and Diagnostics — Tab will skip 3 buttons",
      ).toBeGreaterThan(domIdxDiag);
      expect(domIdxDensity).toBeLessThan(domIdxDiag);
    }
  });

  test("dock menu options are keyboard-operable (Enter/Space activates, tabindex present)", async ({
    page,
  }) => {
    // WCAG 2.1.1 regression: before this fix the dock-menu options
    // (Bottom/Left/Right, Turn Hiding On/Off, Customize, Reset to Default,
    // Action Bar…) were `<div role="option">` with only `onclick` — no
    // `tabindex`, no `onkeydown`. A keyboard-only user could not enter
    // customize mode or reset the bar at all.
    await joinMeeting(page, "kbd_dock_menu_options");
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Hover to reveal the action bar, then focus the dock-menu trigger
    // (button with id="dock-menu-trigger", newly added by this fix).
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);
    const trigger = page.locator("#dock-menu-trigger");
    await expect(trigger, "dock-menu trigger must expose a stable id").toBeVisible({
      timeout: 5_000,
    });
    await trigger.focus();
    await expect(trigger).toBeFocused();

    // Space opens the menu (native <button> semantics).
    await page.keyboard.press("Space");
    await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 3_000 });

    // Every rendered .glass-select-option must be tab-focusable
    // (tabindex="0") — the presence of that attribute is exactly what was
    // missing pre-fix. Separators are naturally excluded from the selector.
    const options = page.locator(".dock-position-wrapper .glass-select-menu .glass-select-option");
    const optionCount = await options.count();
    expect(optionCount).toBeGreaterThanOrEqual(6); // Bottom, Left, Right, autohide, Customize, Reset, Action Bar…
    const tabindices = await options.evaluateAll((els) =>
      els.map((el) => el.getAttribute("tabindex")),
    );
    for (const t of tabindices) {
      expect(t, 'every .glass-select-option must have tabindex="0"').toBe("0");
    }

    // The three dock-position options must carry aria-selected reflecting
    // the current dock_position (Bottom is the default).
    const bottomAriaSelected = await options
      .filter({ hasText: "Bottom" })
      .first()
      .getAttribute("aria-selected");
    expect(bottomAriaSelected).toBe("true");
    const leftAriaSelected = await options
      .filter({ hasText: "Left" })
      .first()
      .getAttribute("aria-selected");
    expect(leftAriaSelected).toBe("false");

    // ArrowDown from the trigger focuses the first option; ArrowDown twice
    // more must advance across two options (arrow navigation via the
    // menu-level onkeydown). Focus starts on trigger post-Space press.
    // Sanity: ArrowDown on trigger (menu already open) focuses first option.
    await page.keyboard.press("ArrowDown");
    await expect(options.first()).toBeFocused({ timeout: 2_000 });
    const firstText = (await options.first().textContent())?.trim();
    expect(firstText).toBe("Bottom");

    // ArrowDown advances to Left.
    await page.keyboard.press("ArrowDown");
    await expect(options.nth(1)).toBeFocused({ timeout: 2_000 });

    // ArrowUp goes back to Bottom.
    await page.keyboard.press("ArrowUp");
    await expect(options.first()).toBeFocused({ timeout: 2_000 });

    // Escape closes the menu and returns focus to the trigger.
    await page.keyboard.press("Escape");
    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 3_000 });
    await expect(trigger).toBeFocused({ timeout: 2_000 });
  });

  test("keyboard user can enter customize mode via the dock menu (Enter on Customize)", async ({
    page,
  }) => {
    // The whole customize feature is unreachable to a keyboard-only user
    // if the "Customize" option is not activatable by keyboard. This test
    // exercises the full path: focus trigger → Space to open → arrow to
    // Customize → Enter to activate → assert customize-mode is on and
    // focus lands on the first slot button (Mic/Sound by default).
    await joinMeeting(page, "kbd_enter_customize");
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);
    const trigger = page.locator("#dock-menu-trigger");
    await trigger.focus();
    await page.keyboard.press("Space");
    await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 3_000 });

    // Focus the Customize option directly (arrow-walk would work too, but
    // focusing it directly makes the test independent of option order).
    const customize = page.locator(".dock-position-wrapper .glass-select-option", {
      hasText: "Customize",
    });
    await customize.focus();
    await expect(customize).toBeFocused();

    // Enter activates the option — before the fix this was a no-op because
    // the `<div role="option">` had no onkeydown handler.
    await page.keyboard.press("Enter");

    // Customize mode is now on.
    await expect(page.locator(".video-controls-container.customize-mode")).toBeVisible({
      timeout: 5_000,
    });

    // The dock menu is closed.
    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 3_000 });

    // Focus should land on the first user-facing slot button (Mic/Sound)
    // so keyboard navigation proceeds left-to-right from the start of the
    // action bar, not from Done.
    const micButton = page.locator(
      '.video-controls-container .action-bar-slot-wrapper[data-slot="mic"] > button.video-control-button',
    );
    await expect(
      micButton,
      "Focus must land on the Mic/Sound slot after entering customize mode",
    ).toBeFocused({
      timeout: 3_000,
    });
  });

  test("re-entering customize mode still focuses Mic/Sound (not Done)", async ({ page }) => {
    await joinMeeting(page, "kbd_customize_reenter_focus");
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    const enterCustomizeViaKeyboard = async () => {
      await page.locator(".video-controls-container").hover();
      await page.waitForTimeout(250);
      const trigger = page.locator("#dock-menu-trigger");
      await trigger.focus();
      await page.keyboard.press("Space");
      await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 3_000 });
      const customize = page.locator(".dock-position-wrapper .glass-select-option", {
        hasText: "Customize",
      });
      await customize.focus();
      await page.keyboard.press("Enter");
      await expect(page.locator(".video-controls-container.customize-mode")).toBeVisible({
        timeout: 5_000,
      });
    };

    const micButton = page.locator(
      '.video-controls-container .action-bar-slot-wrapper[data-slot="mic"] > button.video-control-button',
    );

    await enterCustomizeViaKeyboard();
    await expect(micButton).toBeFocused({ timeout: 3_000 });

    // Exit customize mode through Done and verify focus is restored to trigger.
    const doneBtn = page.locator("button.action-bar-done-trigger");
    await expect(doneBtn).toBeVisible({ timeout: 3_000 });
    await doneBtn.focus();
    await page.keyboard.press("Enter");
    await expect(page.locator(".video-controls-container.customize-mode")).not.toBeVisible({
      timeout: 5_000,
    });
    await expect(page.locator("#dock-menu-trigger")).toBeFocused({ timeout: 3_000 });

    // Second entry must behave exactly like the first.
    await enterCustomizeViaKeyboard();
    await expect(
      micButton,
      "Focus must still land on Mic/Sound after re-entering customize mode",
    ).toBeFocused({ timeout: 3_000 });
  });

  test("keyboard user can reset the action bar via the dock menu (Space on Reset to Default)", async ({
    page,
  }) => {
    // Same reachability guarantee for "Reset to Default": before the fix
    // this option was unreachable by keyboard. A regression here would
    // silently strand a screen-reader user who removed a slot they wanted
    // back — they had no way to undo without a pointer.
    await joinMeeting(page, "kbd_reset_default");

    // Seed a non-default layout so the Reset assertion is meaningful:
    // pre-populate localStorage with a modified layout.
    await page.evaluate(() => {
      localStorage.setItem(
        "vc_action_bar_layout",
        JSON.stringify({
          v: 2,
          slots: ["camera", "mic", "screen"], // reordered + missing several defaults
          hidden: [
            "participants",
            "density",
            "diagnostics",
            "settings",
            "meeting_options",
          ],
        }),
      );
    });
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Verify the seeded layout took effect.
    const seeded = await page.evaluate(() => localStorage.getItem("vc_action_bar_layout"));
    expect(seeded).toContain('"camera","mic"');

    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(300);
    const trigger = page.locator("#dock-menu-trigger");
    await trigger.focus();
    await page.keyboard.press("Space");
    await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 3_000 });

    // Focus Reset to Default and press Space to activate.
    const reset = page.locator(".dock-position-wrapper .glass-select-option", {
      hasText: "Reset to Default",
    });
    await reset.focus();
    await expect(reset).toBeFocused();
    await page.keyboard.press("Space");

    // Menu is closed; focus returned to trigger.
    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 3_000 });
    await expect(trigger).toBeFocused({ timeout: 2_000 });

    // Layout was cleared from localStorage (Reset calls remove_action_bar_layout).
    const after = await page.evaluate(() => localStorage.getItem("vc_action_bar_layout"));
    expect(
      after,
      "Reset to Default must clear the persisted layout when activated via keyboard",
    ).toBeNull();
  });

  test("customize-mode aria-live regions stay mounted and only their text toggles", async ({
    page,
  }) => {
    // Robustness hardening: the enter-customize `role="status"` region
    // must be in the DOM even OUTSIDE customize mode (with empty text),
    // and its text toggled on mode-enter — not conditionally mounted
    // together with its text.  Some older AT (JAWS, some NVDA builds) do
    // not announce a live region whose content was present at the moment
    // it entered the DOM; they only fire on subsequent text mutations.
    // Mounting empty and mutating to text preserves that mutation shape.
    //
    // Reverting the fix (wrapping the two `role="status"` divs inside
    // `if customize_mode()`) makes this test fail at the pre-customize
    // count assertion because the regions disappear from the DOM until
    // the mode is entered.
    await joinMeeting(page, "aria_live_always_mounted");
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Hover just to make sure the controls container is in the DOM.
    await page.locator(".video-controls-container").hover();
    await page.waitForTimeout(200);

    // The two live regions are direct children of `.controls`, siblings of
    // `nav.video-controls-container` (attendants.rs renders them before the
    // nav inside `div.controls`). A `.video-controls-container .visually-hidden`
    // descendant selector matches nothing.
    const liveRegions = page.locator(
      '.controls .visually-hidden[role="status"][aria-live="polite"]',
    );

    // BEFORE customize mode: both regions must already be mounted with
    // empty text.  The regression this pins is: they were previously
    // rendered only inside `if customize_mode()` and did not exist here.
    const preCount = await liveRegions.count();
    expect(
      preCount,
      "both aria-live regions must be mounted before customize mode is entered",
    ).toBe(2);
    const preTexts = await liveRegions.allTextContents();
    for (const t of preTexts) {
      expect(
        t.trim(),
        "aria-live regions must start with empty text so the enter-customize mutation is observable to AT",
      ).toBe("");
    }

    // Enter customize mode.  The enter-customize region's text must
    // flip from "" to the instructions string, exactly the mutation
    // shape older AT relies on for a "polite" announcement.
    await enterCustomizeMode(page);
    await page.waitForTimeout(200);

    const midCount = await liveRegions.count();
    expect(midCount, "regions stay mounted after enter (same count)").toBe(2);
    const midTexts = await liveRegions.allTextContents();
    expect(midTexts.some((t) => /Customizing action bar/i.test(t))).toBe(true);

    // Exit customize mode via the Done button.  Text must return to
    // empty on both regions, so re-entering later produces the same
    // observable "" → text mutation.
    await page.locator("button.action-bar-done-trigger").click();
    await expect(page.locator(".video-controls-container.customize-mode")).not.toBeVisible({
      timeout: 5_000,
    });
    await page.waitForTimeout(200);

    const postCount = await liveRegions.count();
    expect(postCount, "regions must stay mounted after customize-mode exit").toBe(2);
    const postTexts = await liveRegions.allTextContents();
    for (const t of postTexts) {
      expect(
        t.trim(),
        "aria-live text must be cleared on customize-mode exit so a stale message isn't re-announced next time",
      ).toBe("");
    }
  });

  test("clicking Done returns focus to the dock-menu trigger (does not drop to body)", async ({
    page,
  }) => {
    // Reviewer report: "Done's onclick sets customize_mode(false) + saves, and
    // the Done button then unmounts — focus falls to <body>, so a keyboard
    // user finishes customizing and is dumped to the top of the document."
    // The entry path already moves focus TO Done (see the "keyboard user can
    // enter customize mode…" test); this pins the missing half of the round
    // trip. Reverting the `Timeout::new(0, || focus_element_by_id("dock-menu-trigger"))`
    // block on Done's onclick makes this test fail because focus lands on
    // <body> instead of #dock-menu-trigger.
    await joinMeeting(page, "kbd_done_focus_restore");
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await enterCustomizeMode(page);
    const done = page.locator("button.action-bar-done-trigger");
    await expect(done).toBeVisible({ timeout: 5_000 });
    await done.focus();
    await expect(done).toBeFocused();

    // Activate via Enter — same as a keyboard user would.
    await page.keyboard.press("Enter");

    // Customize mode is off; Done has unmounted; the dock-menu trigger
    // took its place.
    await expect(page.locator(".video-controls-container.customize-mode")).not.toBeVisible({
      timeout: 5_000,
    });
    const trigger = page.locator("#dock-menu-trigger");
    await expect(trigger).toBeVisible({ timeout: 3_000 });

    // The regression assertion: focus is on the dock-menu trigger, NOT on
    // <body>. Reverting the deferred `focus_element_by_id` call on Done's
    // onclick trips this.
    await expect(
      trigger,
      "Focus must return to #dock-menu-trigger after Done; a keyboard user must not be dumped to <body>",
    ).toBeFocused({ timeout: 3_000 });
  });

  test("Escape in customize mode exits and returns focus to the dock-menu trigger", async ({
    page,
  }) => {
    // Reviewer recommendation: Escape is the expected exit idiom for a
    // modal-ish mode. Handled by the nav-level onkeydown (which also owns
    // arrow-key reorder). Reverting the `if evt.key() == Key::Escape { ... }`
    // branch in that handler makes Escape a no-op and this test fails.
    await joinMeeting(page, "kbd_escape_exits_customize");
    await page.evaluate(() => localStorage.removeItem("vc_action_bar_layout"));
    await page.reload();
    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await enterCustomizeMode(page);

    // Focus a slot button inside the bar so the Escape event fires on a
    // realistic target (not on Done itself, which would also close the
    // menu via its own click semantics).
    const screenBtn = page
      .locator('.video-controls-container .action-bar-slot-wrapper[data-slot="screen"] > button')
      .first();
    await expect(screenBtn).toBeVisible({ timeout: 5_000 });
    await screenBtn.focus();
    await expect(screenBtn).toBeFocused();

    await page.keyboard.press("Escape");

    // Customize mode exited.
    await expect(page.locator(".video-controls-container.customize-mode")).not.toBeVisible({
      timeout: 5_000,
    });
    // Focus landed on the dock-menu trigger (same restore target as Done).
    const trigger = page.locator("#dock-menu-trigger");
    await expect(trigger).toBeVisible({ timeout: 3_000 });
    await expect(
      trigger,
      "Escape must exit customize mode AND return focus to #dock-menu-trigger",
    ).toBeFocused({ timeout: 3_000 });
  });
});
