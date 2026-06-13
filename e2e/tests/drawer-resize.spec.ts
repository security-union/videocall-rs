import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Drawer resize + both-open (#1296).
 *
 * Two slide-in overlay drawers support a drag-to-RESIZE handle and can be open
 * at the same time. They are OVERLAY-ONLY: they float over the tiles and never
 * reflow the grid.
 *
 *   - LEFT  drawer = Attendants / peer list (`#peer-list-container`).
 *   - RIGHT drawer = Performance & Diagnostics (`#diagnostics-sidebar`).
 *
 * Behavior under test (sourced from
 * `dioxus-ui/src/components/{attendants,diagnostics,peer_list}.rs` +
 * `local_storage.rs` + `static/style.css`):
 *
 *  1. RESIZE: dragging `.drawer-resize-handle` changes the container's inline
 *     `width` px, clamped to [240, min(vw*0.5, 720)]. The left handle reads
 *     pointer `client_x` directly (left edge at x=0); the right handle width is
 *     `drag_start_vw - client_x` (drag its left-edge handle leftward to widen).
 *  2. PERSISTENCE via localStorage keys (EXACT strings, see `local_storage.rs`
 *     callers in `attendants.rs`):
 *       - `vc_drawer_left_width`  (f64)
 *       - `vc_drawer_right_width` (f64)
 *     Width persists on drag-END (pointerup / pointercancel / lostpointercapture)
 *     and is restored after reload.
 *  3. NO HOVER-LATCH: a drag must end cleanly (pointerup OR lost-capture); a
 *     later buttonless hover over the handle must NOT resize.
 *  4. VISIBLE GRIP on desktop; the handle (and grip) is hidden on mobile
 *     (< 568px), where drawers are forced full-width.
 *  5. BOTH drawers can be open at the SAME time (independent open/close).
 *
 * Drag mechanism: the resize handle is driven by POINTER events with pointer
 * capture (`set_pointer_capture`) in the Rust source. We drive it with REAL
 * Playwright `mouse.down/move/up` (NOT synthetic `dispatchEvent`) so the
 * generated pointer events are TRUSTED — synthetic untrusted events would make
 * `set_pointer_capture` a no-op and may not route through Dioxus's delegated
 * listeners. This mirrors the proven drag approach in
 * `signal-meter-popup-drag-and-reanchor.spec.ts`. Chromium emits
 * pointerdown/move/up for mouse input by default, so the Rust onpointer*
 * handlers fire, and the FINAL pointer `client_x` of the move alone determines
 * the resulting width (the handlers integrate no deltas).
 */

const DEFAULT_UI_URL = "http://localhost:3001";

// Clamp bounds mirrored from attendants.rs: DRAWER_MIN_WIDTH / DRAWER_MAX_ABS.
const DRAWER_MIN_WIDTH = 240;
const DRAWER_MAX_ABS = 720;

// Desktop viewport (>= 568px so the resize handle is shown). 1280x720 matches
// the "Desktop Chrome" device default used by the dioxus project.
const DESKTOP = { width: 1280, height: 720 };
// Mobile viewport (< 568px so the resize handle is hidden).
const MOBILE = { width: 400, height: 800 };

// localStorage keys — must EXACTLY match the save_*/load_* call sites in
// attendants.rs. A typo here would make the persistence test pass against the
// wrong key, so they are pinned as named constants and re-used everywhere.
const LS_LEFT_WIDTH = "vc_drawer_left_width";
const LS_RIGHT_WIDTH = "vc_drawer_right_width";

type Side = "left" | "right";

interface DrawerSpec {
  side: Side;
  /** Container element id. */
  containerId: string;
  /** "Open <X>" tooltip text on the video-controls button that opens it. */
  openTooltip: string;
  /** localStorage key holding the width f64. */
  widthKey: string;
}

const DRAWERS: Record<Side, DrawerSpec> = {
  left: {
    side: "left",
    containerId: "peer-list-container",
    openTooltip: "Open Peers",
    widthKey: LS_LEFT_WIDTH,
  },
  right: {
    side: "right",
    containerId: "diagnostics-sidebar",
    openTooltip: "Open Diagnostics",
    widthKey: LS_RIGHT_WIDTH,
  },
};

// ---------------------------------------------------------------------------
// Meeting-entry helpers (mirror host-controls-menu-ux.spec.ts)
// ---------------------------------------------------------------------------

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

/**
 * Open a drawer by clicking its "Open <X>" video-controls button. The
 * controls bar auto-hides after ~1s of mouse inactivity, so wake it with a
 * hover + mouse move first (mirrors `openPeerListSidebar` in
 * host-controls-menu-ux.spec.ts).
 */
async function openDrawer(page: Page, spec: DrawerSpec): Promise<void> {
  await page.locator(".video-controls-container").hover();
  // Nudge the pointer to an INTERIOR point of the live viewport to wake the
  // auto-hiding controls bar. A fixed (400, 400) lands on / past the right edge
  // of the 400px mobile viewport (valid x is 0..399), so derive the centre from
  // the measured size — works for both the 1280 desktop and 400 mobile runs.
  const vp = page.viewportSize() ?? { width: 800, height: 600 };
  await page.mouse.move(Math.floor(vp.width / 2), Math.floor(vp.height / 2));
  await page.waitForTimeout(300);

  const openBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: spec.openTooltip }),
  });
  await expect(openBtn).toBeVisible({ timeout: 10_000 });
  await openBtn.click();
  await expect(page.locator(`#${spec.containerId}`)).toHaveClass(/visible/, { timeout: 10_000 });
}

/** Read a number-valued inline style prop (e.g. `width`) in px. */
async function inlineStylePx(page: Page, selector: string, prop: string): Promise<number> {
  const handle = page.locator(selector);
  const value = await handle.evaluate(
    (el, p) => (el as HTMLElement).style.getPropertyValue(p),
    prop,
  );
  // "" when unset, "320px" when set. parseFloat("") === NaN -> treat as 0.
  const n = parseFloat(value);
  return Number.isNaN(n) ? 0 : n;
}

/** Read the container's inline `width` in px. */
function containerWidthPx(page: Page, spec: DrawerSpec): Promise<number> {
  return inlineStylePx(page, `#${spec.containerId}`, "width");
}

/**
 * Drag the drawer's resize handle so the pointer ends at viewport `targetX`,
 * driving the resize with REAL trusted mouse events.
 *
 * The Rust move handler sets the width from the single `client_x` of the
 * pointermove (it integrates NO deltas), and persists to localStorage only on
 * pointerup, so the FINAL mouse X alone determines the resulting width:
 *   left:  width = clamp(targetX, 240, maxForSide)
 *   right: width = clamp(dragStartVw - targetX, 240, maxForSide)
 *
 * Sequence: move to the handle's center -> mouse.down (pointerdown: begin-drag,
 * cache start-vw for the right side, set_pointer_capture) -> move toward targetX
 * in two steps (pointermove: width updates; the intermediate step lets the
 * signal write + repaint flush) -> mouse.up (pointerup: persist + end-drag).
 *
 * Synthetic `dispatchEvent` is deliberately NOT used: untrusted events make
 * `set_pointer_capture` a no-op and may not route through Dioxus's delegated
 * listeners. Real `page.mouse.*` emits trusted pointer events (Chromium fires
 * pointer events for mouse input by default), mirroring the proven drag in
 * `signal-meter-popup-drag-and-reanchor.spec.ts`.
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

/** The per-side max width cap mirrored from attendants.rs `max_for_side`. */
function maxForSide(viewportWidth: number): number {
  return Math.min(Math.max(viewportWidth * 0.5, DRAWER_MIN_WIDTH), DRAWER_MAX_ABS);
}

/**
 * Read the page's REAL `window.innerWidth`.
 *
 * The Rust width math uses the live viewport width, NOT the value we passed to
 * `setViewportSize`: the right drawer caches `window.inner_width` into
 * `drag_start_vw` on pointerdown (`width = clamp(inner_width - client_x, …)`),
 * and BOTH drawers derive `max_for_side = (inner_width * 0.5).clamp(240, 720)`.
 * Chromium's `innerWidth` after `setViewportSize({width: 1280})` is normally
 * 1280 (these full-bleed call pages have no scrollbar), but deriving the drag
 * targets and the max cap from the *measured* width — rather than the 1280
 * constant — makes every width assertion reference the SAME source of truth the
 * Rust handler reads, instead of a value that could silently diverge.
 */
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

/** Bounding box for a selector, asserting it is present. */
async function boxOf(page: Page, selector: string): Promise<{ x: number; width: number }> {
  const b = await page.locator(selector).boundingBox();
  if (!b) throw new Error(`missing bounding box for ${selector}`);
  return { x: b.x, width: b.width };
}

/** Computed style value for a selector (resolved, e.g. z-index "9301"). */
function computedStyle(page: Page, selector: string, prop: string): Promise<string> {
  return page
    .locator(selector)
    .evaluate((el, p) => window.getComputedStyle(el).getPropertyValue(p), prop);
}

/** Computed pseudo-element (`::before`) style value for a selector. */
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

        // Use the page's MEASURED inner width — the same value the Rust handler
        // reads (right drawer: `clamp(inner_width - client_x, …)`; both sides:
        // `max_for_side = (inner_width * 0.5).clamp(240, 720)`). Deriving the
        // targets + cap from this, not the 1280 constant, keeps the assertions
        // pinned to the engine's source of truth (see `pageInnerWidth`).
        const vw = await pageInnerWidth(page);
        const maxW = maxForSide(vw); // min(vw*0.5, 720) — 640 at vw=1280

        // Capture the default (pre-drag) width so the in-range drag below proves
        // a real CHANGE, not merely that the width coincidentally equals a
        // default. Defaults from attendants.rs use_signal initializers: left
        // 320px, right 560px (both overlay-rendered at the raw width signal).
        const defaultWidth = await containerWidthPx(page, spec);

        // Width math (final pointer client_x):
        //   left:  width = clamp(clientX, 240, maxW)
        //   right: width = clamp(vw - clientX, 240, maxW)
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

        // Drag PAST the lower bound: width clamps to DRAWER_MIN_WIDTH (240).
        //   left  -> a small clientX (e.g. 50) clamps up to 240.
        //   right -> a clientX near the right edge (vw-50) -> width 50 -> 240.
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

        // Protects the per-drag "valid" flush gate in attendants.rs
        // (left_raf_valid / right_raf_valid, Rc<Cell<bool>> reset to false on
        // pointerdown / on_resize_start, set true only on a real pointermove):
        // the `if lv.get()` guards (LEFT pointerup / pointercancel /
        // lostpointercapture) and the `if rv.get()` guard (RIGHT on_resize_end).
        // The rAF stash defaults are 0.0. Deleting these gates makes a NO-MOVE
        // pointerup flush the default 0.0 stash -> LEFT clamps to
        // DRAWER_MIN_WIDTH (240, vs the 320 default), RIGHT clamps
        // `drag_start_vw - 0.0` to max_for_side (~640 at vw=1280, vs the 560
        // default) and persists it via save_f64 — CHANGING the inline width AND
        // writing the width key. With the gates intact a no-move interaction
        // leaves both the width signal and localStorage untouched, which is what
        // this test pins.

        // Capture the pre-interaction inline width and the persisted width
        // value (may be null if nothing was ever dragged this session — the
        // gate must hold in that case too).
        const widthBefore = await containerWidthPx(page, spec);
        const storedBefore = await page.evaluate((k) => localStorage.getItem(k), spec.widthKey);

        // Perform a NO-MOVE pointer interaction on the resize handle: locate it
        // exactly like dragResizeHandleTo (bounding-box center), then
        // down -> up at the SAME coords with NO `page.mouse.move` strictly
        // between down() and up(). The pre-down positioning move presses no
        // buttons, so it is not a captured drag pointermove and does not set the
        // valid flag; only a move WHILE the button is down would. This mirrors a
        // user clicking / focus-tapping the handle without dragging.
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

        // Width must be UNCHANGED. Tolerance is tight (0 digits => within
        // ~0.5px): the gate-removed no-move flush would shift LEFT by 80px
        // (320 -> 240) and RIGHT by ~80px (560 -> ~640), so this assertion
        // FAILS hard if either gate is deleted. expect.poll allows the
        // (non-)flush to settle while still pinning "did not change".
        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(widthBefore, 0);

        // Storage must be UNCHANGED. With the gate intact, save_f64 is never
        // called for a no-move interaction, so the width key keeps its prior
        // value (null or a real number). With the gate removed it would be
        // written ("240" left / "~640" right), so `toBe(storedBefore)` FAILS.
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

        // Resize to a known in-range width (writes the width key on pointerup).
        // Measured inner width = the value the right drawer's handler reads for
        // `clamp(inner_width - client_x, …)` (see `pageInnerWidth`).
        const vw = await pageInnerWidth(page);
        const targetWidth = 300;
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

        // Release the still-down real button OFF the handle. Capture was detached
        // above, so this real `pointerup` targets the center element, NOT the
        // handle — the handle's `onpointerup` does not fire, so it cannot mask the
        // lost-capture reset.
        const vh = page.viewportSize()?.height ?? DESKTOP.height;
        await page.mouse.move(Math.floor(vw / 2), Math.floor(vh / 2));
        await page.mouse.up();
        await page.waitForTimeout(150);

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

  // Reference the `boxOf` helper so it is exercised by a real assertion (it
  // underpins the both-open layout reasoning above): on desktop, the resized
  // left peer-list panel sits flush against the left viewport edge.
  test("left drawer hugs the left viewport edge (overlay, no reflow)", async ({ baseURL }) => {
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

      // The overlay left drawer is anchored at viewport x=0. FAILS if the
      // drawer is shifted inward (e.g. a stray inset positioning regression).
      const b = await boxOf(page, `#${spec.containerId}`);
      expect(b.x).toBeLessThanOrEqual(1);
    } finally {
      await browser.close();
    }
  });
});
