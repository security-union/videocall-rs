import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { DRAWER } from "../helpers/rust-mirrored-constants";

/**
 * Drawer resize + both-open (#1296), budget-aware since #2701. Dragging
 * `.drawer-resize-handle` sets the inline `width`, clamped to
 * [DRAWER_MIN_WIDTH, drawer_max_for_side(...)] — a cap that now also subtracts
 * what the OTHER open drawers keep. Width persists on drag-END and survives
 * reload, never latches to a later hover, and the grip hides below 568px.
 * Drags use REAL `page.mouse.*`; untrusted events no-op `set_pointer_capture`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

const { DRAWER_MIN_WIDTH, DRAWER_MAX_ABS, MIN_GRID_BAND } = DRAWER;

const DESKTOP = { width: 1280, height: 720 };
const MOBILE = { width: 400, height: 800 };

// localStorage keys — must EXACTLY match the save_*/load_* call sites in
// attendants.rs. A typo here would make the persistence test pass against the
// wrong key, so they are pinned as named constants and re-used everywhere.
const LS_LEFT_WIDTH = "vc_drawer_left_width";
const LS_RIGHT_WIDTH = "vc_drawer_right_width";

type Side = "left" | "right";

interface DrawerSpec {
  side: Side;
  containerId: string;
  openTooltip: string;
  /** `ActionBarSlot::display_name()`, the label in the "More actions" menu. */
  overflowLabel: string;
  widthKey: string;
}

const DRAWERS: Record<Side, DrawerSpec> = {
  left: {
    side: "left",
    containerId: "peer-list-container",
    openTooltip: "Open Peers",
    overflowLabel: "Participants",
    widthKey: LS_LEFT_WIDTH,
  },
  right: {
    side: "right",
    containerId: "diagnostics-sidebar",
    openTooltip: "Open Diagnostics",
    overflowLabel: "Diagnostics",
    widthKey: LS_RIGHT_WIDTH,
  },
};

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
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);
}

async function joinMeetingFromPage(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const which = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
  ]);

  if (which === "join") {
    await page.waitForTimeout(500);
    if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
      await joinButton.click().catch(() => undefined);
    }
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/** Open a drawer, waking the auto-hiding bar first. At 400px every secondary
 *  slot is shed behind `#overflow-menu-trigger` (pre-dates #2701), so the
 *  inline button reports `hidden` and the fallback uses "More actions". */
async function openDrawer(page: Page, spec: DrawerSpec): Promise<void> {
  await page.locator(".video-controls-container").hover();
  const vp = page.viewportSize() ?? { width: 800, height: 600 };
  await page.mouse.move(Math.floor(vp.width / 2), Math.floor(vp.height / 2));
  await page.waitForTimeout(300);

  const openBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: spec.openTooltip }),
  });
  if (await openBtn.isVisible()) {
    await openBtn.click();
  } else {
    const trigger = page.locator("#overflow-menu-trigger");
    await expect(trigger).toBeVisible({ timeout: 10_000 });
    await trigger.click();
    const item = page.locator(".action-bar-overflow-popover button.overflow-item", {
      has: page.locator(`span:text-is("${spec.overflowLabel}")`),
    });
    await expect(item).toBeVisible({ timeout: 10_000 });
    await item.click();
  }
  await expect(page.locator(`#${spec.containerId}`)).toHaveClass(/visible/, { timeout: 10_000 });
}

async function inlineStylePx(page: Page, selector: string, prop: string): Promise<number> {
  const handle = page.locator(selector);
  const value = await handle.evaluate(
    (el, p) => (el as HTMLElement).style.getPropertyValue(p),
    prop,
  );
  const n = parseFloat(value);
  return Number.isNaN(n) ? 0 : n;
}

function containerWidthPx(page: Page, spec: DrawerSpec): Promise<number> {
  return inlineStylePx(page, `#${spec.containerId}`, "width");
}

/**
 * Drag the drawer's resize handle so the pointer ends at viewport `targetX`,
 * driving the resize with REAL trusted mouse events.
 */
async function dragResizeHandleTo(page: Page, spec: DrawerSpec, targetX: number): Promise<void> {
  const handle = page.locator(`#${spec.containerId} .drawer-resize-handle`);
  await expect(handle).toBeVisible({ timeout: 10_000 });
  const box = await handle.boundingBox();
  expect(box).not.toBeNull();
  if (!box) throw new Error(`${spec.side} resize handle has no bounding box`);

  const grabX = box.x + box.width / 2;
  const grabY = box.y + box.height / 2;

  await page.mouse.move(grabX, grabY);
  await page.mouse.down();
  // Intermediate move lets the onpointermove handler fire + inline width
  // repaint before the final position is read.
  await page.mouse.move((grabX + targetX) / 2, grabY);
  await page.mouse.move(targetX, grabY);
  await page.mouse.up();
  // Let Dioxus flush the final signal write + re-render and the pointerup
  // persistence.
  await page.waitForTimeout(300);
}

/**
 * The cap with the dragged drawer ALONE, for viewports at or above 800 ONLY.
 *
 * `drawer_max_for_side` is `min(max_total_reserve(vw), vw*0.5, DRAWER_MAX_ABS)`
 * floored at `DRAWER_MIN_WIDTH`, and `max_total_reserve` is
 * `min(vw*0.60, vw - MIN_GRID_BAND)`. The 0.60 term never binds, since `vw*0.5`
 * is always smaller. The `vw - 400` term binds whenever `vw < 800`, which the
 * shorthand below does NOT model: at 700 the real cap is 300 and at 768 it is
 * 368, where this would answer 350 and 384. Every caller runs at DESKTOP, so
 * the precondition is asserted rather than the whole rule re-implemented.
 */
function maxForSide(viewportWidth: number): number {
  expect(
    viewportWidth,
    "maxForSide only models vw >= 800; below that `vw - MIN_GRID_BAND` binds",
  ).toBeGreaterThanOrEqual(2 * MIN_GRID_BAND);
  return Math.min(Math.max(viewportWidth * 0.5, DRAWER_MIN_WIDTH), DRAWER_MAX_ABS);
}

/** The public build strips the chat integration, so `#chat-sidebar` never
 *  mounts. Keyed on the sidebar, not its action-bar button: the button is also
 *  absent when a narrow band sheds it into the overflow menu. */
const NO_CHAT = "chat integration is not part of the public build";

function chatIsBuiltIn(page: Page): Promise<boolean> {
  return page
    .locator("#chat-sidebar")
    .count()
    .then((n) => n > 0);
}

/** The live `window.innerWidth` the Rust handlers read, not the requested size. */
function pageInnerWidth(page: Page): Promise<number> {
  return page.evaluate(() => window.innerWidth);
}

/**
 * The pointer `client_x` that yields RIGHT-drawer width `w` for a given inner
 * width `vw`: the right handler computes `width = drag_start_vw - client_x`, so
 * to land width `w` the pointer must end at `vw - w`.
 */
function vwMinus(vw: number, w: number): number {
  return vw - w;
}

async function boxOf(page: Page, selector: string): Promise<{ x: number; width: number }> {
  const b = await page.locator(selector).boundingBox();
  if (!b) throw new Error(`missing bounding box for ${selector}`);
  return { x: b.x, width: b.width };
}

function computedStyle(page: Page, selector: string, prop: string): Promise<string> {
  return page
    .locator(selector)
    .evaluate((el, p) => window.getComputedStyle(el).getPropertyValue(p), prop);
}

function computedPseudoStyle(
  page: Page,
  selector: string,
  pseudo: string,
  prop: string,
): Promise<string> {
  return page
    .locator(selector)
    .evaluate((el, args) => window.getComputedStyle(el, args.pseudo).getPropertyValue(args.prop), {
      pseudo,
      prop,
    });
}

/**
 * Begin a REAL captured drag on a drawer's resize handle and MOVE the pointer
 * (trusted pointerdown so `set_pointer_capture` actually takes; pointermove
 * registers a width change + sets the per-drag "valid" flag) but do NOT release
 * with a clean `mouse.up`. Leaves the real pointer button DOWN at `endX`. The
 * caller then ends the drag via a LOST-CAPTURE path (release the real capture +
 * dispatch `lostpointercapture`) — exercising the shared `on_resize_end` path
 * that a clean pointerup would otherwise mask. Returns the width observed
 * mid-drag (after the move, before the lost-capture end).
 */
async function startCapturedDragNoRelease(
  page: Page,
  spec: DrawerSpec,
  endX: number,
): Promise<{ widthDuringDrag: number }> {
  const handle = page.locator(`#${spec.containerId} .drawer-resize-handle`);
  await expect(handle).toBeVisible({ timeout: 10_000 });
  const box = await handle.boundingBox();
  if (!box) throw new Error(`${spec.side} resize handle has no bounding box`);

  const grabX = box.x + box.width / 2;
  const grabY = box.y + box.height / 2;

  await page.mouse.move(grabX, grabY);
  await page.mouse.down();
  // Intermediate + final move so the onpointermove handler fires (sets the
  // valid flag + stashes client_x) and the width repaints.
  await page.mouse.move((grabX + endX) / 2, grabY);
  await page.mouse.move(endX, grabY);
  // Let the rAF-coalesced width.set() flush so the mid-drag width is observable.
  await page.waitForTimeout(150);
  const widthDuringDrag = await containerWidthPx(page, spec);
  // NOTE: pointer button intentionally left DOWN — the caller ends the drag via
  // a lost-capture event, not mouse.up.
  return { widthDuringDrag };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Drawer resize + both-open (#1296)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // Run the resize/persistence/latch battery for BOTH drawers. Each iteration
  // is an isolated single-host meeting. The drawers are overlay-only, so the
  // resize handle is exposed without any pin step.
  for (const side of ["left", "right"] as const) {
    const spec = DRAWERS[side];

    test(`${side} drawer: resize handle drag changes width, clamped to [min, max]`, async ({
      baseURL,
    }) => {
      test.setTimeout(90_000);
      const uiURL = baseURL || DEFAULT_UI_URL;
      const meetingId = `e2e_drawer_resize_${side}_${Date.now()}`;
      const browser = await chromium.launch({ args: BROWSER_ARGS });

      try {
        const ctx = await createAuthenticatedContext(
          browser,
          `resize-${side}@videocall.rs`,
          `Resize${side}`,
          uiURL,
        );
        const page = await ctx.newPage();
        await page.setViewportSize(DESKTOP);

        await navigateToMeeting(page, meetingId, `Resize${side}`);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        const vw = await pageInnerWidth(page);
        const maxW = maxForSide(vw); // min(vw*0.5, 720) — 640 at vw=1280

        // Defaults are 320 left / 560 right, so 360 proves a real change.
        const defaultWidth = await containerWidthPx(page, spec);

        // Width math (final pointer client_x):
        // Pick a clientX that lands a known in-range target of ~360px so we can
        // assert an exact, non-default width.
        const targetWidth = 360;
        const inRangeX = side === "left" ? targetWidth : vw - targetWidth;
        await dragResizeHandleTo(page, spec, inRangeX);
        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(targetWidth, -1); // within ~5px
        // The width genuinely changed from its default (guards against a no-op
        // drag whose result coincidentally matched the default width). 360 is
        // chosen to differ from BOTH defaults (320 left / 560 right) by > 10px.
        expect(Math.abs(targetWidth - defaultWidth)).toBeGreaterThan(10);

        // Drag PAST the lower bound: width clamps to DRAWER_MIN_WIDTH (300).
        const belowMinX = side === "left" ? 50 : vw - 50;
        await dragResizeHandleTo(page, spec, belowMinX);
        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(DRAWER_MIN_WIDTH, -1);

        // Drag PAST the upper bound: width clamps to maxW (640 here, < 720 abs).
        //   left  -> a large clientX (vw-1) clamps down to maxW.
        //   right -> a clientX near 0 -> width ~vw -> clamps to maxW.
        // maxW is itself <= DRAWER_MAX_ABS and >= DRAWER_MIN_WIDTH, so asserting
        // the width lands at maxW proves the clamp held on the upper side. This
        // FAILS if the resize handler does not clamp (width would blow past maxW
        // toward the viewport width).
        const aboveMaxX = side === "left" ? vw - 1 : 1;
        await dragResizeHandleTo(page, spec, aboveMaxX);
        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(maxW, -1);
        // Belt-and-braces: the settled clamped width never exceeds the absolute
        // cap (read AFTER the poll above has confirmed it landed at maxW).
        const clamped = await containerWidthPx(page, spec);
        expect(clamped).toBeLessThanOrEqual(DRAWER_MAX_ABS + 1);
      } finally {
        await browser.close();
      }
    });

    test(`${side} drawer: no-move pointer interaction on resize handle leaves width + storage untouched`, async ({
      baseURL,
    }) => {
      test.setTimeout(90_000);
      const uiURL = baseURL || DEFAULT_UI_URL;
      const meetingId = `e2e_drawer_nomove_${side}_${Date.now()}`;
      const browser = await chromium.launch({ args: BROWSER_ARGS });

      try {
        const ctx = await createAuthenticatedContext(
          browser,
          `nomove-${side}@videocall.rs`,
          `Nomove${side}`,
          uiURL,
        );
        const page = await ctx.newPage();
        await page.setViewportSize(DESKTOP);

        await navigateToMeeting(page, meetingId, `Nomove${side}`);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        // Pins the per-drag "valid" flush gates (left_raf_valid / right_raf_valid,
        // set only by a real pointermove). Without them a no-move pointerup
        // flushes the 0.0 rAF stash and persists it.
        const widthBefore = await containerWidthPx(page, spec);
        const storedBefore = await page.evaluate((k) => localStorage.getItem(k), spec.widthKey);

        const handle = page.locator(`#${spec.containerId} .drawer-resize-handle`);
        await expect(handle).toBeVisible({ timeout: 10_000 });
        const box = await handle.boundingBox();
        expect(box).not.toBeNull();
        if (!box) throw new Error(`${spec.side} resize handle has no bounding box`);

        const centerX = box.x + box.width / 2;
        const centerY = box.y + box.height / 2;

        await page.mouse.move(centerX, centerY); // positioning only (no buttons)
        await page.mouse.down();
        await page.mouse.up(); // no move between down/up => no-move interaction
        // Let any (non-)flush + persistence settle, mirroring dragResizeHandleTo.
        await page.waitForTimeout(300);

        // Deleting either gate shifts LEFT 320 -> 300 and RIGHT 560 -> ~640,
        // both far outside the ~0.5px tolerance.
        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(widthBefore, 0);

        const storedAfter = await page.evaluate((k) => localStorage.getItem(k), spec.widthKey);
        expect(storedAfter).toBe(storedBefore);
      } finally {
        await browser.close();
      }
    });

    test(`${side} drawer: resized width persists across reload (localStorage)`, async ({
      baseURL,
    }) => {
      test.setTimeout(90_000);
      const uiURL = baseURL || DEFAULT_UI_URL;
      const meetingId = `e2e_drawer_persist_${side}_${Date.now()}`;
      const browser = await chromium.launch({ args: BROWSER_ARGS });

      try {
        const ctx = await createAuthenticatedContext(
          browser,
          `persist-${side}@videocall.rs`,
          `Persist${side}`,
          uiURL,
        );
        const page = await ctx.newPage();
        await page.setViewportSize(DESKTOP);

        await navigateToMeeting(page, meetingId, `Persist${side}`);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        const container = page.locator(`#${spec.containerId}`);

        // 400, not the old 300: 300 is now the clamp FLOOR, and a target on a
        // bound cannot separate "the drag landed here" from "the clamp did it".
        const vw = await pageInnerWidth(page);
        const targetWidth = 400;
        const inRangeX = side === "left" ? targetWidth : vw - targetWidth;
        await dragResizeHandleTo(page, spec, inRangeX);
        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(targetWidth, -1);

        // Source-of-truth check: localStorage holds the expected width under the
        // EXACT key. This FAILS if the key string is wrong or the value was not
        // written on drag-end.
        const storedWidth = await page.evaluate((k) => localStorage.getItem(k), spec.widthKey);
        expect(storedWidth).not.toBeNull();
        expect(parseFloat(storedWidth as string)).toBeCloseTo(targetWidth, -1);

        // Reload: the drawer is closed after a reload, so re-open it. The width
        // is read from localStorage on mount, so the restored container must
        // come back at the restored width. This FAILS if load_f64 reads the
        // wrong key or the restore is dropped.
        await page.reload();
        await page.waitForTimeout(1500);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        await expect(container).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(targetWidth, -1);
      } finally {
        await browser.close();
      }
    });
  }

  // =========================================================================
  // Resize requires a REAL grab; there is no hover-latch; the grip is visible.
  // The first sub-test (no hover-latch after a CLEAN drag) and the second (end
  // the drag via a LOST-CAPTURE path, then hover) are distinct:
  //
  //   A clean down/move/up always fires `onpointerup`, whose flush + reset to
  //   ResizingDrawer::None is DUPLICATED in `onlostpointercapture`/
  //   `onpointercancel` (attendants.rs LEFT handle / diagnostics.rs RIGHT
  //   handle). So a clean-pointerup-only test would still PASS if
  //   `onlostpointercapture` were deleted — it does NOT cover lost-capture.
  //   The lost-capture sub-test ENDS the drag WITHOUT any pointerup reaching
  //   the handle (it releases the real pointer capture + dispatches
  //   `lostpointercapture`, and deliberately does NOT send `pointercancel` so
  //   nothing masks a deletion of `onlostpointercapture`), so it is the only
  //   thing that fails if that handler is removed and the drawer latches to a
  //   later hover.
  // =========================================================================
  for (const side of ["left", "right"] as const) {
    const spec = DRAWERS[side];
    test(`${side} resize needs a real grab — hovering the handle after a clean drag does NOT change width (no hover-latch)`, async ({
      baseURL,
    }) => {
      test.setTimeout(90_000);
      const uiURL = baseURL || DEFAULT_UI_URL;
      const meetingId = `e2e_drawer_nolatch_clean_${side}_${Date.now()}`;
      const browser = await chromium.launch({ args: BROWSER_ARGS });

      try {
        const ctx = await createAuthenticatedContext(
          browser,
          `nolatch-${side}@videocall.rs`,
          `NoLatch${side}`,
          uiURL,
        );
        const page = await ctx.newPage();
        await page.setViewportSize(DESKTOP);

        await navigateToMeeting(page, meetingId, `NoLatch${side}`);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        const vw = await pageInnerWidth(page);

        // A real, complete drag (down -> move -> UP) changes the width.
        const targetWidth = 360;
        const dragX = side === "left" ? targetWidth : vwMinus(vw, targetWidth);
        await dragResizeHandleTo(page, spec, dragX);
        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(targetWidth, -1);
        const widthAfterDrag = await containerWidthPx(page, spec);

        // Now move the mouse back OVER the handle WITHOUT pressing any button,
        // then sweep across the drawer. With the drag ended (ResizingDrawer set
        // back to None on pointerup), the onpointermove guard (`== <side>`) is
        // false, so NO width change may occur. A hover-latch regression (drag
        // state never reset) would resize on this buttonless move.
        const handle = page.locator(`#${spec.containerId} .drawer-resize-handle`);
        const box = await handle.boundingBox();
        if (!box) throw new Error(`${spec.side} resize handle has no bounding box`);
        await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
        // Sweep well away from the handle (toward viewport center) — a latched
        // handler would map this X to a brand-new width.
        await page.mouse.move(Math.floor(vw / 2), box.y + box.height / 2);
        await page.waitForTimeout(300);

        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(widthAfterDrag, 0);
      } finally {
        await browser.close();
      }
    });

    test(`${side} resize ends cleanly via LOST-CAPTURE (not pointerup) — later hover does NOT change width`, async ({
      baseURL,
    }) => {
      test.setTimeout(90_000);
      const uiURL = baseURL || DEFAULT_UI_URL;
      const meetingId = `e2e_drawer_lostcap_${side}_${Date.now()}`;
      const browser = await chromium.launch({ args: BROWSER_ARGS });

      try {
        const ctx = await createAuthenticatedContext(
          browser,
          `lostcap-${side}@videocall.rs`,
          `LostCap${side}`,
          uiURL,
        );
        const page = await ctx.newPage();
        await page.setViewportSize(DESKTOP);

        await navigateToMeeting(page, meetingId, `LostCap${side}`);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        const vw = await pageInnerWidth(page);
        const endTarget = 360;
        const endX = side === "left" ? endTarget : vwMinus(vw, endTarget);

        // Begin a REAL captured drag (trusted pointerdown -> set_pointer_capture
        // actually takes; pointermove changes width + sets the valid flag) but
        // leave the button DOWN — do NOT clean-release with mouse.up.
        const { widthDuringDrag } = await startCapturedDragNoRelease(page, spec, endX);
        expect(widthDuringDrag).toBeCloseTo(endTarget, -1);

        // End the drag via a LOST-CAPTURE path instead of a clean pointerup.
        //
        // WHY this is distinct coverage (CLAUDE.md Check 2): a clean down/move/UP
        // always fires `onpointerup`, whose flush + reset-to-None is DUPLICATED in
        // the `onlostpointercapture` handler (attendants.rs LEFT handle /
        // diagnostics.rs RIGHT handle). So a clean-pointerup test would still pass
        // with `onlostpointercapture` deleted. This test must end WITHOUT any
        // pointerup ever reaching the handle, so the ONLY thing that can reset the
        // drag is the lost-capture handler.
        //
        // Mechanism: Dioxus-web registers `lostpointercapture` as a BUBBLING,
        // root-delegated listener (dioxus-core-types `event_bubbles` => true; the
        // interpreter does `root.addEventListener("lostpointercapture", handler)`
        // with NO isTrusted gate — verified in dioxus-web 0.7.3 dom.rs/core.js).
        // So both of the following route to the Rust `onlostpointercapture`
        // closure -> shared `on_resize_end` -> `resizing_drawer = None`:
        //   1) `releasePointerCapture(1)` on the handle while the real mouse
        //      button is down — fires a REAL, TRUSTED `lostpointercapture` AND
        //      detaches the browser capture (Chromium's mouse pointerId is 1, the
        //      same id the Rust onpointerdown captured). Detaching capture is what
        //      keeps the trailing real `mouse.up` from being delivered to the
        //      handle (no masking `onpointerup`).
        //   2) a synthetic `lostpointercapture` dispatch — a deterministic backstop
        //      that the handler ran even if the release in (1) is a no-op.
        // We deliberately do NOT dispatch `pointercancel` here: that would also
        // reset the drag and would MASK a deletion of `onlostpointercapture`,
        // defeating the point of this test. (`onpointercancel` shares the same
        // `on_resize_end` path and is covered implicitly by the no-latch test's
        // reset semantics.)
        await page.locator(`#${spec.containerId} .drawer-resize-handle`).evaluate((el) => {
          try {
            // Chromium mouse pointerId is 1; guard in case capture isn't held.
            (el as Element & { releasePointerCapture(id: number): void }).releasePointerCapture(1);
          } catch {
            /* capture already released / different id — synthetic dispatch below
               still drives the handler. */
          }
          el.dispatchEvent(new PointerEvent("lostpointercapture", { bubbles: true, pointerId: 1 }));
        });
        await page.waitForTimeout(150);

        // Off the handle so its `onpointerup` cannot mask the lost-capture
        // reset, but INSIDE the drawer: releasing on the grid synthesises a
        // `click` on `#main-container`, whose light-dismiss (#1790) closes the
        // drawer and hangs the next `boundingBox()` (90s timeout on the base).
        const vh = page.viewportSize()?.height ?? DESKTOP.height;
        const releaseX = side === "left" ? 100 : vw - 100;
        await page.mouse.move(releaseX, Math.floor(vh / 2));
        await page.mouse.up();
        await page.waitForTimeout(150);
        await expect(page.locator(`#${spec.containerId}`)).toHaveClass(/\bvisible\b/, {
          timeout: 5_000,
        });

        const widthAfterEnd = await containerWidthPx(page, spec);

        // Now hover the handle (NO button down) and sweep toward center. With the
        // drag reset by the lost-capture path, the buttonless `pointermove` hits
        // the `== <side>` guard (now false) and changes nothing. If
        // `onlostpointercapture` were deleted, `resizing_drawer` would still be
        // `<side>` and this hover would resize the drawer to the sweep's client_x
        // — so this assertion is what fails when the lost-capture fix is reverted.
        const handle = page.locator(`#${spec.containerId} .drawer-resize-handle`);
        const box = await handle.boundingBox();
        if (!box) throw new Error(`${spec.side} resize handle has no bounding box`);
        await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
        await page.mouse.move(Math.floor(vw / 2), box.y + box.height / 2);
        await page.waitForTimeout(300);

        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(widthAfterEnd, 0);
      } finally {
        await browser.close();
      }
    });

    test(`${side} resize handle shows a visible centered grip on desktop`, async ({ baseURL }) => {
      test.setTimeout(90_000);
      const uiURL = baseURL || DEFAULT_UI_URL;
      const meetingId = `e2e_drawer_grip_${side}_${Date.now()}`;
      const browser = await chromium.launch({ args: BROWSER_ARGS });

      try {
        const ctx = await createAuthenticatedContext(
          browser,
          `grip-${side}@videocall.rs`,
          `Grip${side}`,
          uiURL,
        );
        const page = await ctx.newPage();
        await page.setViewportSize(DESKTOP);

        await navigateToMeeting(page, meetingId, `Grip${side}`);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        const handleSel = `#${spec.containerId} .drawer-resize-handle`;
        await expect(page.locator(handleSel)).toBeVisible({ timeout: 10_000 });

        // The grip is a `::before` pseudo-element with a real background
        // (var(--border-emphasis)) and non-zero size (3x32px). Assert the
        // computed ::before background is neither `none` nor fully transparent
        // and the grip has width/height. FAILS if the grip styling is removed (a
        // bare transparent edge with no affordance).
        const bg = await computedPseudoStyle(page, handleSel, "::before", "background-color");
        expect(bg).not.toBe("");
        expect(bg).not.toBe("none");
        expect(bg).not.toBe("transparent");
        expect(bg).not.toBe("rgba(0, 0, 0, 0)");

        const gripW = await computedPseudoStyle(page, handleSel, "::before", "width");
        const gripH = await computedPseudoStyle(page, handleSel, "::before", "height");
        expect(parseFloat(gripW)).toBeGreaterThan(0);
        expect(parseFloat(gripH)).toBeGreaterThan(0);
      } finally {
        await browser.close();
      }
    });
  }

  test("resize handle (and its grip) is hidden on a mobile viewport (<568px)", async ({
    baseURL,
  }) => {
    test.setTimeout(90_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const spec = DRAWERS.left;
    const meetingId = `e2e_drawer_grip_mobile_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "grip-mobile@videocall.rs",
        "GripMobile",
        uiURL,
      );
      const page = await ctx.newPage();
      await page.setViewportSize(MOBILE);

      await navigateToMeeting(page, meetingId, "GripMobile");
      await joinMeetingFromPage(page);
      await openDrawer(page, spec);

      // On mobile the drawers are forced full-width, so the handle is a no-op
      // affordance and is `display: none !important`. The element is hidden
      // (not interactable), so its ::before grip is gone too. FAILS if the
      // mobile hide is removed.
      const handleSel = `#${spec.containerId} .drawer-resize-handle`;
      await expect(page.locator(handleSel)).toBeHidden({ timeout: 10_000 });
      const display = await computedStyle(page, handleSel, "display");
      expect(display).toBe("none");
    } finally {
      await browser.close();
    }
  });

  // =========================================================================
  // Both drawers can be open at the SAME time. On the OLD code, opening one
  // drawer closed the other (DiagnosticsButton/PeerListButton each set the
  // other's open signal to false), so the "both visible" assertion must FAIL on
  // the pre-fix code.
  // =========================================================================
  test("opening the peer list then diagnostics leaves BOTH drawers visible at once", async ({
    baseURL,
  }) => {
    test.setTimeout(90_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_drawer_both_open_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "both-open@videocall.rs",
        "BothOpen",
        uiURL,
      );
      const page = await ctx.newPage();
      await page.setViewportSize(DESKTOP);

      await navigateToMeeting(page, meetingId, "BothOpen");
      await joinMeetingFromPage(page);

      const left = page.locator(`#${DRAWERS.left.containerId}`);
      const right = page.locator(`#${DRAWERS.right.containerId}`);

      // Open the peer list FIRST, then diagnostics. On the old code, opening
      // diagnostics closed the peer list — so this asserts BOTH stay visible.
      await openDrawer(page, DRAWERS.left);
      await expect(left).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await openDrawer(page, DRAWERS.right);

      // Both visible simultaneously — the core independence guarantee.
      await expect(left).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expect(right).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expect(left).toBeVisible();
      await expect(right).toBeVisible();

      // Closing ONE (via its own close button) leaves the OTHER open. Close
      // DIAGNOSTICS first; the peer list must remain. (Each close button is
      // inside its own drawer, so no action-bar click is needed here.)
      await right.locator("button.close-button").click();
      await expect(right).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expect(left).toHaveClass(/\bvisible\b/, { timeout: 10_000 });

      // Reverse: re-open DIAGNOSTICS, then close the PEER LIST — diagnostics must
      // remain. We re-open the RIGHT drawer (not the left) on purpose: at this
      // point only the LEFT peer-list panel is open, and on desktop it sits at
      // the left edge and does NOT occlude the centered bottom action bar, so the
      // "Open Diagnostics" control is reachable. (Re-opening while the wide right
      // panel was open could occlude the bar — z 9000 < the drawer's.)
      await openDrawer(page, DRAWERS.right);
      await expect(left).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expect(right).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await left.locator("button.close-button").click();
      await expect(left).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expect(right).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
    } finally {
      await browser.close();
    }
  });

  test("mobile both-open — diagnostics stacks above peer-list (z 9301 > 9300); each close button dismisses only its own drawer", async ({
    baseURL,
  }) => {
    test.setTimeout(90_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_drawer_both_mobile_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "both-mobile@videocall.rs",
        "BothMobile",
        uiURL,
      );
      const page = await ctx.newPage();
      // Start on DESKTOP to open both drawers via the action bar, then resize to
      // mobile. WHY: on mobile each open drawer is a full-screen fixed overlay at
      // z-index 9300/9301, which sits ABOVE the action bar (z 9000). Once the
      // first drawer is open it would occlude the bar, so the second drawer's
      // "Open" button could not be clicked. The open state is component state
      // (independent of viewport), so opening both at desktop width and then
      // shrinking the viewport faithfully reproduces the mobile both-open layout
      // — and the mobile `@media (max-width:568px)` z-index rules apply purely
      // from CSS on resize.
      await page.setViewportSize(DESKTOP);

      await navigateToMeeting(page, meetingId, "BothMobile");
      await joinMeetingFromPage(page);

      const left = page.locator(`#${DRAWERS.left.containerId}`);
      const right = page.locator(`#${DRAWERS.right.containerId}`);

      // Open both (side panels at desktop width — neither covers the bar).
      await openDrawer(page, DRAWERS.left);
      await openDrawer(page, DRAWERS.right);
      await expect(left).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expect(right).toHaveClass(/\bvisible\b/, { timeout: 10_000 });

      // Now shrink to a mobile viewport: both drawers stay open (component state)
      // and the mobile CSS turns them into stacked full-screen overlays.
      await page.setViewportSize(MOBILE);
      await expect(left).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expect(right).toHaveClass(/\bvisible\b/, { timeout: 10_000 });

      // Deterministic stacking: diagnostics (9301) sits above peer-list (9300)
      // so the right drawer is on top regardless of DOM order. FAILS if the
      // mobile z-index split is removed (both would be 9300).
      const leftZ = parseInt(
        await computedStyle(page, `#${DRAWERS.left.containerId}`, "z-index"),
        10,
      );
      const rightZ = parseInt(
        await computedStyle(page, `#${DRAWERS.right.containerId}`, "z-index"),
        10,
      );
      expect(rightZ).toBe(9301);
      expect(leftZ).toBe(9300);
      expect(rightZ).toBeGreaterThan(leftZ);

      // Each close button dismisses ONLY its own drawer. Close diagnostics
      // (top) first; peer-list remains.
      await right.locator("button.close-button").click();
      await expect(right).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expect(left).toHaveClass(/\bvisible\b/, { timeout: 10_000 });

      // Then close peer-list; nothing else to dismiss.
      await left.locator("button.close-button").click();
      await expect(left).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
    } finally {
      await browser.close();
    }
  });

  // Before #2701 the grid was `left: 0` in every state, so `grid.x` read 0 here
  // whatever the drawer measured.
  test("a dragged left drawer hugs x=0 and the grid starts at its inner edge", async ({
    baseURL,
  }) => {
    test.setTimeout(90_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const spec = DRAWERS.left;
    const meetingId = `e2e_drawer_edge_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "edge-left@videocall.rs",
        "EdgeLeft",
        uiURL,
      );
      const page = await ctx.newPage();
      await page.setViewportSize(DESKTOP);

      await navigateToMeeting(page, meetingId, "EdgeLeft");
      await joinMeetingFromPage(page);
      await openDrawer(page, spec);

      const atDefault = await boxOf(page, `#${spec.containerId}`);
      expect(atDefault.x).toBeLessThanOrEqual(1);

      const gridAtDefault = await boxOf(page, "#grid-container");
      expect(gridAtDefault.x).toBeCloseTo(atDefault.width, 0);

      await dragResizeHandleTo(page, spec, 420);
      await expect
        .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
        .toBeCloseTo(420, -1);

      const dragged = await boxOf(page, `#${spec.containerId}`);
      expect(dragged.x).toBeLessThanOrEqual(1);
      const gridAfterDrag = await boxOf(page, "#grid-container");
      expect(gridAfterDrag.x).toBeCloseTo(dragged.width, 0);
      expect(gridAfterDrag.x).toBeGreaterThan(gridAtDefault.x + 50);
    } finally {
      await browser.close();
    }
  });

  // The wrapper is tagged while a handle is held so chat's transition can be
  // suppressed for the duration; it must come off again when the drag ends, or
  // chat never animates again.
  test("the wrapper carries drawers-dragging only while a handle is held", async ({ baseURL }) => {
    test.setTimeout(90_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const spec = DRAWERS.left;
    const meetingId = `e2e_drawer_dragging_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    // The wrapper has no id; it is `#main-container`'s parent.
    const wrapperClass = (page: Page) =>
      page.evaluate(
        () => document.querySelector("#main-container")?.parentElement?.className ?? "",
      );

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "dragging@videocall.rs",
        "Dragging",
        uiURL,
      );
      const page = await ctx.newPage();
      await page.setViewportSize(DESKTOP);

      await navigateToMeeting(page, meetingId, "Dragging");
      await joinMeetingFromPage(page);
      await openDrawer(page, spec);

      expect(await wrapperClass(page)).not.toContain("drawers-dragging");

      const handle = page.locator(`#${spec.containerId} .drawer-resize-handle`);
      const box = await handle.boundingBox();
      if (!box) throw new Error("left resize handle has no bounding box");
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await page.mouse.down();
      await page.mouse.move(420, box.y + box.height / 2);
      await expect.poll(() => wrapperClass(page), { timeout: 5_000 }).toContain("drawers-dragging");

      await page.mouse.up();
      await expect
        .poll(() => wrapperClass(page), { timeout: 5_000 })
        .not.toContain("drawers-dragging");
    } finally {
      await browser.close();
    }
  });

  // Literal px at 1280 (budget 768), from the acceptance checklist: diagnostics
  // beside chat 408, beside chat + peer list 300 (the floor), peer list beside
  // diagnostics 468. All three clamped at 640 before #2701.
  test("the resize clamp shrinks as other drawers open (budget-aware cap)", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_drawer_budget_cap_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "budget-cap@videocall.rs",
        "BudgetCap",
        uiURL,
      );
      const page = await ctx.newPage();
      await page.setViewportSize(DESKTOP);

      await navigateToMeeting(page, meetingId, "BudgetCap");
      await joinMeetingFromPage(page);
      test.skip(!(await chatIsBuiltIn(page)), NO_CHAT);

      const vw = await pageInnerWidth(page);

      await page.locator(".video-controls-container").hover();
      await page.mouse.move(Math.floor(vw / 2), 360);
      await page.waitForTimeout(300);
      await page.getByRole("button", { name: "Chat", exact: true }).click();
      await expect(page.locator("#chat-sidebar")).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await openDrawer(page, DRAWERS.right);

      await dragResizeHandleTo(page, DRAWERS.right, 1);
      await expect
        .poll(() => containerWidthPx(page, DRAWERS.right), { timeout: 10_000 })
        .toBeCloseTo(408, -1);

      // All three open: the cap is down to the floor, so the handle goes inert
      // and a drag on it must be a true no-op — the stored 408 survives rather
      // than being overwritten with the floor it renders at.
      await openDrawer(page, DRAWERS.left);
      const rightHandle = page.locator(`#${DRAWERS.right.containerId} .drawer-resize-handle`);
      await expect(rightHandle).toHaveAttribute("aria-disabled", "true", { timeout: 10_000 });
      await expect
        .poll(() => containerWidthPx(page, DRAWERS.right), { timeout: 10_000 })
        .toBeCloseTo(DRAWER_MIN_WIDTH, -1);
      // Release INSIDE the drawer: ending on the grid would light-dismiss the
      // peer list (#1790) and silently unwind the three-drawer state.
      await dragResizeHandleTo(page, DRAWERS.right, vw - 80);
      await expect(page.locator(`#${DRAWERS.left.containerId}`)).toHaveClass(/visible/, {
        timeout: 5_000,
      });
      expect(await page.evaluate((k) => localStorage.getItem(k), LS_RIGHT_WIDTH)).toBe("408");

      // 468, because the left cap reserves only the diagnostics FLOOR: it
      // shrinks first and absorbs the drag, rather than holding its live 560.
      await page.locator("#chat-sidebar").getByRole("button", { name: "Close chat" }).click();
      await expect(page.locator("#chat-sidebar")).not.toHaveClass(/\bvisible\b/, {
        timeout: 10_000,
      });
      await dragResizeHandleTo(page, DRAWERS.left, vw - 1);
      await expect
        .poll(() => containerWidthPx(page, DRAWERS.left), { timeout: 10_000 })
        .toBeCloseTo(468, -1);
    } finally {
      await browser.close();
    }
  });
});
