import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Drawer pin + resize (#1296).
 *
 * Two slide-in drawers now support a PIN toggle and a drag-to-RESIZE handle:
 *
 *   - LEFT  drawer = Attendants / peer list (`#peer-list-container`).
 *   - RIGHT drawer = Performance & Diagnostics (`#diagnostics-sidebar`).
 *
 * NEW user-facing behavior under test (all sourced from
 * `dioxus-ui/src/components/{attendants,diagnostics,peer_list}.rs` +
 * `local_storage.rs` + `static/style.css`):
 *
 *  1. PIN button (`button.pin-button`) in each drawer header toggles
 *     `aria-pressed` and flips its `aria-label`/`title` between "Pin panel"
 *     and "Unpin panel", and toggles the `pinned` CSS class on the container.
 *  2. Pinning on a DESKTOP viewport (>= 568px) reflows the tile grid:
 *     `#grid-container` gets a non-zero inline `left:` (left drawer) /
 *     `right:` (right drawer) px inset so tiles shrink rather than being
 *     covered. Overlay (unpinned) → that side's inset is 0.
 *  3. RESIZE: dragging `.drawer-resize-handle` changes the container's inline
 *     `width` px, clamped to [240, min(vw*0.5, 720)]. The left handle reads
 *     pointer `client_x` directly (left edge at x=0); the right handle width is
 *     `drag_start_vw - client_x` (drag its left-edge handle leftward to widen).
 *  4. PERSISTENCE via localStorage keys (EXACT strings, see `local_storage.rs`
 *     callers in `attendants.rs`):
 *       - `vc_drawer_left_pinned`  (bool)
 *       - `vc_drawer_left_width`   (f64)
 *       - `vc_drawer_right_pinned` (bool)
 *       - `vc_drawer_right_width`  (f64)
 *     Pin state persists on toggle; width persists on drag-END (pointerup).
 *     After reload the pinned class + width are restored.
 *  5. MOBILE (viewport < 568px): pinning is IGNORED — the `pinned` class is NOT
 *     applied even if the stored pin pref is true, and `#grid-container` is NOT
 *     inset.
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

// Desktop viewport (>= 568px so pin is honoured). 1280x720 matches the
// "Desktop Chrome" device default used by the dioxus project.
const DESKTOP = { width: 1280, height: 720 };
// Mobile viewport (< 568px so pin is ignored).
const MOBILE = { width: 400, height: 800 };

// localStorage keys — must EXACTLY match the save_*/load_* call sites in
// attendants.rs. A typo here would make the persistence test pass against the
// wrong key, so they are pinned as named constants and re-used everywhere.
const LS_LEFT_PINNED = "vc_drawer_left_pinned";
const LS_LEFT_WIDTH = "vc_drawer_left_width";
const LS_RIGHT_PINNED = "vc_drawer_right_pinned";
const LS_RIGHT_WIDTH = "vc_drawer_right_width";

type Side = "left" | "right";

interface DrawerSpec {
  side: Side;
  /** Container element id. */
  containerId: string;
  /** "Open <X>" tooltip text on the video-controls button that opens it. */
  openTooltip: string;
  /** localStorage key holding the pinned bool. */
  pinnedKey: string;
  /** localStorage key holding the width f64. */
  widthKey: string;
}

const DRAWERS: Record<Side, DrawerSpec> = {
  left: {
    side: "left",
    containerId: "peer-list-container",
    openTooltip: "Open Peers",
    pinnedKey: LS_LEFT_PINNED,
    widthKey: LS_LEFT_WIDTH,
  },
  right: {
    side: "right",
    containerId: "diagnostics-sidebar",
    openTooltip: "Open Diagnostics",
    pinnedKey: LS_RIGHT_PINNED,
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

/** Read a number-valued inline style prop (e.g. `left`, `right`, `width`) in px. */
async function inlineStylePx(page: Page, selector: string, prop: string): Promise<number> {
  const handle = page.locator(selector);
  const value = await handle.evaluate(
    (el, p) => (el as HTMLElement).style.getPropertyValue(p),
    prop,
  );
  // "" when unset, "320px" when set. parseFloat("") === NaN -> treat as 0
  // (absent inset == no reflow on that side).
  const n = parseFloat(value);
  return Number.isNaN(n) ? 0 : n;
}

/** Read the container's inline `width` in px. */
function containerWidthPx(page: Page, spec: DrawerSpec): Promise<number> {
  return inlineStylePx(page, `#${spec.containerId}`, "width");
}

/** The grid inset that this drawer's side should produce when pinned. */
function gridInsetPx(page: Page, spec: DrawerSpec): Promise<number> {
  return inlineStylePx(page, "#grid-container", spec.side);
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
 * Rust handler reads, instead of a value that could silently diverge. A test
 * that assumed 1280 while the engine used a different inner_width would pin the
 * wrong number; reading it here removes that hidden coupling.
 */
function pageInnerWidth(page: Page): Promise<number> {
  return page.evaluate(() => window.innerWidth);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Drawer pin + resize (#1296)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // Run the same battery of pin/reflow/resize/persistence assertions for BOTH
  // drawers. Each iteration is an isolated single-host meeting.
  for (const side of ["left", "right"] as const) {
    const spec = DRAWERS[side];

    test(`${side} drawer: pin button toggles aria-pressed, label, and pinned class`, async ({
      baseURL,
    }) => {
      test.setTimeout(90_000);
      const uiURL = baseURL || DEFAULT_UI_URL;
      const meetingId = `e2e_drawer_pin_${side}_${Date.now()}`;
      const browser = await chromium.launch({ args: BROWSER_ARGS });

      try {
        const ctx = await createAuthenticatedContext(
          browser,
          `pin-${side}@videocall.rs`,
          `Pin${side}`,
          uiURL,
        );
        const page = await ctx.newPage();
        await page.setViewportSize(DESKTOP);

        await navigateToMeeting(page, meetingId, `Pin${side}`);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        const container = page.locator(`#${spec.containerId}`);
        const pinBtn = container.locator("button.pin-button");

        // Unpinned (overlay) initial state: aria-pressed=false, label "Pin
        // panel", no `pinned` class. This FAILS if the button defaults to the
        // wrong state or the class is wrongly applied while unpinned.
        await expect(pinBtn).toHaveAttribute("aria-pressed", "false");
        await expect(pinBtn).toHaveAttribute("aria-label", "Pin panel");
        await expect(pinBtn).toHaveAttribute("title", "Pin panel");
        await expect(container).not.toHaveClass(/\bpinned\b/);

        // Toggle -> pinned. aria-pressed flips to true, label/title to "Unpin
        // panel", and the container gains the `pinned` class. This FAILS if the
        // click does not toggle pin state or the class is not added.
        await pinBtn.click();
        await expect(pinBtn).toHaveAttribute("aria-pressed", "true");
        await expect(pinBtn).toHaveAttribute("aria-label", "Unpin panel");
        await expect(pinBtn).toHaveAttribute("title", "Unpin panel");
        await expect(container).toHaveClass(/\bpinned\b/);

        // Toggle back -> overlay again. FAILS if unpin does not remove the class.
        await pinBtn.click();
        await expect(pinBtn).toHaveAttribute("aria-pressed", "false");
        await expect(pinBtn).toHaveAttribute("aria-label", "Pin panel");
        await expect(container).not.toHaveClass(/\bpinned\b/);
      } finally {
        await browser.close();
      }
    });

    test(`${side} drawer: pinning on desktop reflows the grid (non-zero ${side} inset)`, async ({
      baseURL,
    }) => {
      test.setTimeout(90_000);
      const uiURL = baseURL || DEFAULT_UI_URL;
      const meetingId = `e2e_drawer_reflow_${side}_${Date.now()}`;
      const browser = await chromium.launch({ args: BROWSER_ARGS });

      try {
        const ctx = await createAuthenticatedContext(
          browser,
          `reflow-${side}@videocall.rs`,
          `Reflow${side}`,
          uiURL,
        );
        const page = await ctx.newPage();
        await page.setViewportSize(DESKTOP);

        await navigateToMeeting(page, meetingId, `Reflow${side}`);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        const container = page.locator(`#${spec.containerId}`);
        const pinBtn = container.locator("button.pin-button");

        // Overlay (unpinned): the drawer floats over tiles, so #grid-container
        // has NO inset on this side. FAILS if an inset leaks while overlaid.
        expect(await gridInsetPx(page, spec)).toBe(0);

        // Drawer-vs-grid OVERLAP proxy (deterministic across browsers without
        // pinning exact pixel widths): the horizontal intersection of the
        // drawer's box and the grid's box. Overlaid => the drawer fully covers
        // tile area (intersection ~= the drawer's width); pinned => the grid is
        // carved out so the drawer sits flush beside it (intersection ~= 0). We
        // return the intersection WIDTH (not a bool) and apply an epsilon so
        // sub-pixel rounding where the drawer's inner edge meets the grid's
        // inset edge (both derived from the same `left_inset`/`right_inset`)
        // never counts as a real overlap.
        const OVERLAP_EPSILON_PX = 4;
        const overlapWidthPx = async (): Promise<number> => {
          const a = await container.boundingBox();
          const b = await page.locator("#grid-container").boundingBox();
          if (!a || !b) throw new Error("missing bounding box");
          const left = Math.max(a.x, b.x);
          const right = Math.min(a.x + a.width, b.x + b.width);
          return Math.max(0, right - left);
        };
        // Overlaid: the drawer covers tile area by far more than epsilon.
        expect(await overlapWidthPx()).toBeGreaterThan(OVERLAP_EPSILON_PX);

        // Pin it: #grid-container must gain a non-zero inset on THIS side so the
        // tiles physically shrink rather than being covered. This FAILS if the
        // Rust grid-inset reflow (avail_w + left/right px) is missing.
        await pinBtn.click();
        await expect(container).toHaveClass(/\bpinned\b/);
        await expect.poll(() => gridInsetPx(page, spec), { timeout: 10_000 }).toBeGreaterThan(0);

        // The opposite side must NOT be inset (only this drawer is pinned).
        const otherSide: Side = side === "left" ? "right" : "left";
        expect(await inlineStylePx(page, "#grid-container", otherSide)).toBe(0);

        // Pinned => no overlap: the grid is carved out so the drawer sits flush
        // beside it (intersection <= epsilon). This is the bounding-box proxy
        // for "tiles reflow rather than being covered". FAILS if pinning floats
        // the drawer over the tiles instead of reflowing them.
        await expect
          .poll(() => overlapWidthPx(), { timeout: 10_000 })
          .toBeLessThanOrEqual(OVERLAP_EPSILON_PX);

        // Unpin: the inset returns to 0 (overlay). FAILS if unpin leaves a
        // stale inset behind.
        await pinBtn.click();
        await expect(container).not.toHaveClass(/\bpinned\b/);
        await expect.poll(() => gridInsetPx(page, spec), { timeout: 10_000 }).toBe(0);
        // And the overlap is back (overlay mode floats over the tiles again).
        await expect
          .poll(() => overlapWidthPx(), { timeout: 10_000 })
          .toBeGreaterThan(OVERLAP_EPSILON_PX);
      } finally {
        await browser.close();
      }
    });

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

    test(`${side} drawer: pin + resize persist across reload (localStorage)`, async ({
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
        const pinBtn = container.locator("button.pin-button");

        // Pin it (writes the pinned key on toggle).
        await pinBtn.click();
        await expect(container).toHaveClass(/\bpinned\b/);

        // Resize to a known in-range width (writes the width key on pointerup).
        // Measured inner width = the value the right drawer's handler reads for
        // `clamp(inner_width - client_x, …)` (see `pageInnerWidth`).
        const vw = await pageInnerWidth(page);
        // 300px is well under the 60%-of-vw combined-inset cap (vw*0.6 == 768 at
        // 1280), and only THIS drawer is pinned, so `inset_sum (300) < cap` =>
        // no down-scaling: the pinned render width equals the raw width signal
        // (300). The inline `width` we read back therefore equals the target
        // even though it is the carved-out render width while pinned.
        const targetWidth = 300;
        const inRangeX = side === "left" ? targetWidth : vw - targetWidth;
        await dragResizeHandleTo(page, spec, inRangeX);
        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(targetWidth, -1);

        // Source-of-truth check: localStorage holds the expected values under
        // the EXACT keys. This FAILS if the key string is wrong or the value
        // was not written on toggle/drag-end.
        const storedPinned = await page.evaluate((k) => localStorage.getItem(k), spec.pinnedKey);
        const storedWidth = await page.evaluate((k) => localStorage.getItem(k), spec.widthKey);
        expect(storedPinned).toBe("true");
        expect(storedWidth).not.toBeNull();
        expect(parseFloat(storedWidth as string)).toBeCloseTo(targetWidth, -1);

        // Reload: the drawer is closed after a reload, so re-open it. The pin
        // pref + width are read from localStorage on mount, so the restored
        // container must come back pinned at the restored width. This FAILS if
        // load_bool/load_f64 read the wrong key or the restore is dropped.
        await page.reload();
        await page.waitForTimeout(1500);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        await expect(container).toHaveClass(/\bpinned\b/, { timeout: 10_000 });
        await expect(container.locator("button.pin-button")).toHaveAttribute(
          "aria-pressed",
          "true",
        );
        await expect
          .poll(() => containerWidthPx(page, spec), { timeout: 10_000 })
          .toBeCloseTo(targetWidth, -1);
        // Pinned -> the grid is reflowed after restore too.
        await expect.poll(() => gridInsetPx(page, spec), { timeout: 10_000 }).toBeGreaterThan(0);
      } finally {
        await browser.close();
      }
    });

    test(`${side} drawer: mobile viewport ignores pinning (no pinned class, no grid inset)`, async ({
      baseURL,
    }) => {
      test.setTimeout(90_000);
      const uiURL = baseURL || DEFAULT_UI_URL;
      const meetingId = `e2e_drawer_mobile_${side}_${Date.now()}`;
      const browser = await chromium.launch({ args: BROWSER_ARGS });

      try {
        const ctx = await createAuthenticatedContext(
          browser,
          `mobile-${side}@videocall.rs`,
          `Mobile${side}`,
          uiURL,
        );
        const page = await ctx.newPage();
        await page.setViewportSize(MOBILE);

        await navigateToMeeting(page, meetingId, `Mobile${side}`);
        await joinMeetingFromPage(page);
        await openDrawer(page, spec);

        const container = page.locator(`#${spec.containerId}`);
        const pinBtn = container.locator("button.pin-button");

        // Click pin on a < 568px viewport. The toggle writes the pref, but the
        // `pinned` class is gated on `vw >= 568` so it must NOT be applied, and
        // the grid must NOT inset. These FAIL if the mobile gate regresses.
        await pinBtn.click();
        // The pref is persisted regardless (source of truth for the desktop
        // restore path), but the visual pin must be suppressed on mobile.
        await expect
          .poll(() => page.evaluate((k) => localStorage.getItem(k), spec.pinnedKey))
          .toBe("true");
        await expect(container).not.toHaveClass(/\bpinned\b/);
        // aria-pressed reflects the EFFECTIVE (gated) pinned state, which is
        // false on mobile.
        await expect(pinBtn).toHaveAttribute("aria-pressed", "false");
        // No reflow on mobile: grid inset stays 0 on both sides.
        expect(await gridInsetPx(page, spec)).toBe(0);
      } finally {
        await browser.close();
      }
    });
  }
});
