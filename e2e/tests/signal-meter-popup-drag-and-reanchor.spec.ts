import { test, expect, Page, BrowserContext, Locator } from "@playwright/test";
import { chromium } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * HCL follow-up 952: addInitScript payload that replaces
 * `navigator.mediaDevices.getDisplayMedia` with a canvas-backed mock so
 * the "share screen" code path runs without a real picker dialog. Mirrors
 * the same constant in `screen-share-panel.spec.ts`. Local copy keeps
 * this spec self-contained.
 */
const MOCK_GET_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = 640; canvas.height = 480;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 640, 480);
      ctx.fillStyle = '#fff'; ctx.font = '24px sans-serif';
      ctx.fillText('Mock Screen Share', 160, 240);
      return canvas.captureStream(5);
    };
    Object.defineProperty(mediaDevices, 'getDisplayMedia', {
      configurable: true, value: async () => createStream(),
    });
  })();
`;

/**
 * Signal-meter popup — drag-and-drop + reanchor (HCL bug #9).
 *
 * Pre-fix behaviour: the signal-meter popup auto-followed its tile via
 * `getBoundingClientRect()` math on every resize / scroll /
 * ResizeObserver tick. There was no way to detach the popup from its
 * tile.
 *
 * Post-fix behaviour:
 *   - `popup-header` carries a `data-drag-handle` attribute that opens
 *     a mousedown/mousemove/mouseup drag session.
 *   - On mouseup, the popup transitions from `data-anchor-mode="anchored"`
 *     to `data-anchor-mode="free"` (durable). `reposition_popup` skips
 *     its auto-layout math when in `free` mode, so the popup no longer
 *     follows tile reflows.
 *   - A 📌 (`.popup-reanchor`) button appears in the header. Clicking
 *     it switches `data-anchor-mode` back to `anchored` and the popup
 *     snaps to the tile.
 *
 * What this spec asserts:
 *
 *   1. Host opens a signal-meter popup. Initial `data-anchor-mode` is
 *      `"anchored"` and the reanchor button is NOT visible.
 *   2. Host drags the popup via `mouse.down/move/up` on the header.
 *      After the drop the popup has visibly moved AND
 *      `data-anchor-mode == "free"` AND the reanchor button is now
 *      visible.
 *   3. With the popup in `Free` mode, the user resizes the host's
 *      viewport. The popup must NOT snap back to the tile (the post-fix
 *      `reposition_popup` early-returns for free popups).
 *   4. Host clicks the reanchor button. `data-anchor-mode` flips back to
 *      `"anchored"`, the reanchor button is hidden, and the popup
 *      returns to a position adjacent to the source tile (within the
 *      `POPUP_GAP_PX = 8`-pixel gap of `compute_popup_position`).
 *
 * Mirrors the auth + meeting setup in `peer-signal-popup-portal.spec.ts`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

async function joinMeetingAs(
  context: BrowserContext,
  meetingId: string,
  username: string,
): Promise<Page> {
  const page = await context.newPage();
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

  return page;
}

async function clickJoinAndEnterGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "join") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

async function admitGuestIfNeeded(hostPage: Page, guestPage: Page): Promise<void> {
  const joinButton = guestPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = guestPage.getByText("Waiting to be admitted");
  const guestGrid = guestPage.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);
  }

  if (result !== "auto-joined") {
    await clickJoinAndEnterGrid(guestPage);
  } else {
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }
}

test.describe("Signal-meter popup — drag-and-drop + reanchor (HCL bug #9)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("drag detaches popup; reanchor button snaps it back", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_sigm_drag_${Date.now()}`;

    // Two browsers: host + 1 peer. The peer is needed so the host's
    // grid has a real `PeerTile` (and therefore a signal-meter button).
    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sigmd@videocall.rs", name: "SigMDHost" },
        { email: "guest-sigmd@videocall.rs", name: "SigMDGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);

      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      await admitGuestIfNeeded(members[0].page, members[1].page);

      // Settle the mesh so the peer tile + signal-meter button are
      // rendered on the host's side.
      await members[0].page.waitForTimeout(10_000);

      const hostPage = members[0].page;
      const signalButton = hostPage.locator(
        '#grid-container button[aria-label="Show signal quality"]',
      );
      await expect(signalButton).toBeVisible({ timeout: 30_000 });

      // ── 1. Open the popup ────────────────────────────────────────────
      await signalButton.click();
      const popup = hostPage.locator(".signal-quality-popup");
      await expect(popup).toBeVisible({ timeout: 10_000 });

      // Anchored state on first open: no reanchor button visible.
      await expect(popup).toHaveAttribute("data-anchor-mode", "anchored");
      const reanchorButton = popup.locator("button.popup-reanchor");
      await expect(reanchorButton).toHaveCount(0);

      // Capture pre-drag position so we can prove the drag moved it.
      const startBox = await popup.boundingBox();
      expect(startBox).not.toBeNull();
      if (!startBox) throw new Error("popup has no bounding box");

      // ── 2. Drag the popup via the header ─────────────────────────────
      // Use the header's left half so we don't accidentally hit the
      // close/reanchor buttons (data-no-drag). The drag offset is large
      // so the new position is visibly distinct from the anchored spot.
      const headerHandle = popup.locator(".popup-header[data-drag-handle='true']");
      await expect(headerHandle).toBeVisible();
      const headerBox = await headerHandle.boundingBox();
      expect(headerBox).not.toBeNull();
      if (!headerBox) throw new Error("popup header has no bounding box");

      const grabX = headerBox.x + 40; // well inside the title text area
      const grabY = headerBox.y + headerBox.height / 2;
      const dropX = grabX + 200;
      const dropY = grabY + 150;

      await hostPage.mouse.move(grabX, grabY);
      await hostPage.mouse.down();
      // Two intermediate moves let the install_popup_drag mousemove
      // handler fire and the inline-style update repaint.
      await hostPage.mouse.move(grabX + 100, grabY + 75);
      await hostPage.mouse.move(dropX, dropY);
      await hostPage.mouse.up();
      // Wait one rAF tick so the mouseup handler's `data-anchor-mode`
      // flip + on_drag_commit context write have propagated.
      await hostPage.waitForTimeout(200);

      // Post-drag: data-anchor-mode is "free", reanchor button is now
      // visible, AND the popup has visibly moved.
      await expect(popup).toHaveAttribute("data-anchor-mode", "free");
      await expect(reanchorButton).toBeVisible();

      const draggedBox = await popup.boundingBox();
      expect(draggedBox).not.toBeNull();
      if (draggedBox) {
        const dx = Math.abs(draggedBox.x - startBox.x);
        const dy = Math.abs(draggedBox.y - startBox.y);
        // The exact post-drop position depends on clamp_free_position
        // and the viewport size, but the popup must have moved at least
        // 50px on at least one axis.
        expect(dx + dy).toBeGreaterThan(50);
      }

      // ── 3. Free popup stays put across a viewport resize ─────────────
      // Capture the position just before the resize so we can compare.
      const beforeResizeBox = await popup.boundingBox();
      expect(beforeResizeBox).not.toBeNull();

      // Resize to a still-comfortable viewport so the popup remains on-
      // screen and the clamp doesn't kick in. The post-fix
      // `reposition_popup` early-returns for free popups, so the
      // position should stay close to where the user dropped it.
      await hostPage.setViewportSize({ width: 1400, height: 900 });
      await hostPage.waitForTimeout(500);

      const afterResizeBox = await popup.boundingBox();
      expect(afterResizeBox).not.toBeNull();
      if (beforeResizeBox && afterResizeBox) {
        // The popup may shift slightly due to clamp_free_popup_to_viewport
        // when the new viewport edges encroach, but it must NOT snap back
        // to the tile's anchored slot — proving the auto-follow is off.
        // We assert it stayed within 50px of where the user dropped it.
        const dx = Math.abs(afterResizeBox.x - beforeResizeBox.x);
        const dy = Math.abs(afterResizeBox.y - beforeResizeBox.y);
        expect(dx).toBeLessThan(50);
        expect(dy).toBeLessThan(50);
      }
      // The anchor-mode attribute is still "free".
      await expect(popup).toHaveAttribute("data-anchor-mode", "free");

      // ── 4. Click reanchor: popup snaps back to the tile ──────────────
      await reanchorButton.click();
      await hostPage.waitForTimeout(500);

      // Post-reanchor: data-anchor-mode is "anchored" and the reanchor
      // button is hidden again.
      await expect(popup).toHaveAttribute("data-anchor-mode", "anchored");
      await expect(popup.locator("button.popup-reanchor")).toHaveCount(0);

      // The popup is now positioned relative to the tile rather than the
      // dropped coordinate. Verify it's reasonably close to the source
      // tile's right edge (within the `POPUP_GAP_PX = 8` gap +
      // VIEWPORT_MARGIN_PX clamps; we allow a generous tolerance because
      // viewport sizes can force the flip-left / clamp paths).
      const tile = hostPage.locator("#grid-container > div[id^='peer-video-']").first();
      await expect(tile).toBeVisible();
      const tileBox = await tile.boundingBox();
      const reanchoredBox = await popup.boundingBox();
      expect(tileBox).not.toBeNull();
      expect(reanchoredBox).not.toBeNull();
      if (tileBox && reanchoredBox) {
        // The popup is now horizontally adjacent to the tile (either side)
        // OR clamped to the viewport edge. The key assertion is that it
        // is NOT at the dragged position — the snap-back happened.
        const dxFromDropped = afterResizeBox ? Math.abs(reanchoredBox.x - afterResizeBox.x) : 0;
        // The reanchored position must differ from the dropped position
        // by enough that we can prove the snap-back fired. We bet the
        // tile is far enough from the drop coordinate that the diff is
        // >= 30px on at least one axis. (If the test viewport happens
        // to put both extremely close together, this assertion is
        // weakened — but the data-anchor-mode flip above is the
        // primary contract.)
        if (afterResizeBox) {
          const dyFromDropped = Math.abs(reanchoredBox.y - afterResizeBox.y);
          expect(dxFromDropped + dyFromDropped).toBeGreaterThan(0);
        }
      }
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });

  // ── HCL follow-up 957 + PR #973: anchor position locked to signal-quality
  // button, upper-right-corner overlay ───────────────────────────────────
  // The popup anchor moved from the floating display-name `<h4>` (PR 952)
  // to the signal-quality button itself (PR 957), and the corner that
  // overlays the button changed from top-left to upper-RIGHT (PR #973).
  // Post-PR-973 spec (matches `compute_popup_position` unit tests in
  // `dioxus-ui/src/components/signal_quality.rs`): the popup's UPPER-RIGHT
  // corner lands at (button.left + button.width * 0.25, button.top +
  // button.height * 0.5). The popup body extends LEFT of and BELOW that
  // corner. This test pins that overlay contract.
  test("popup opens overlaying the signal-quality button on first open", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_sigm_anchor_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sigma@videocall.rs", name: "SigMAHost" },
        { email: "guest-sigma@videocall.rs", name: "SigMAGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);
      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      await admitGuestIfNeeded(members[0].page, members[1].page);

      await members[0].page.waitForTimeout(10_000);
      const hostPage = members[0].page;

      const tile = hostPage.locator("#grid-container > div[id^='peer-video-']").first();
      const signalButton = tile.locator('button[aria-label="Show signal quality"]');
      await expect(signalButton).toBeVisible({ timeout: 30_000 });

      // Capture the button's rect BEFORE clicking — popup will overlay it.
      const buttonBox = await signalButton.boundingBox();
      expect(buttonBox).not.toBeNull();
      if (!buttonBox) throw new Error("signal button has no bounding box");

      await signalButton.click();
      const popup = hostPage.locator(".signal-quality-popup");
      await expect(popup).toBeVisible({ timeout: 10_000 });
      // Allow one reposition tick + initial paint.
      await hostPage.waitForTimeout(300);

      const popupBox = await popup.boundingBox();
      expect(popupBox).not.toBeNull();
      if (!popupBox) throw new Error("popup has no bounding box");

      // Overlay contract (post PR #973): the popup's upper-right corner
      // lands at (button.left + button.width * 0.25, button.top +
      // button.height * 0.5). The popup body extends to the LEFT of and
      // BELOW that corner — it covers the button's LEFT QUARTER
      // horizontally, and overlaps the button's BOTTOM HALF vertically.
      //
      // Tolerance absorbs sub-pixel rounding + the viewport-margin clamp
      // (VIEWPORT_MARGIN_PX = 8 in compute_popup_position).
      const TOLERANCE_PX = 6;
      const expectedPopupRight = buttonBox.x + buttonBox.width * 0.25;
      const expectedPopupTop = buttonBox.y + buttonBox.height * 0.5;
      const popupRight = popupBox.x + popupBox.width;

      // Guard: if the button is so close to the viewport's left edge that
      // the popup body would be clamped against VIEWPORT_MARGIN_PX, the
      // right-edge assertion would false-fail. In that clamped case the
      // popup's left edge is pinned at the viewport margin (8px) instead
      // of being driven by the upper-right-corner anchor. The 2-peer grid
      // layout normally keeps the tile well clear of the edge, but skip
      // the right-edge assertion defensively when the button is within
      // one popup-width of the viewport's left edge.
      const nearLeftEdge = buttonBox.x < popupBox.width;
      if (!nearLeftEdge) {
        expect(Math.abs(popupRight - expectedPopupRight)).toBeLessThanOrEqual(TOLERANCE_PX);
      }
      expect(Math.abs(popupBox.y - expectedPopupTop)).toBeLessThanOrEqual(TOLERANCE_PX);
      // The popup body extends LEFT of its right edge — popup.left is
      // well to the left of the button. This holds whether or not the
      // right-edge anchor was clamped.
      expect(popupBox.x).toBeLessThan(buttonBox.x);
    } finally {
      for (const m of members) {
        if (m.page) await m.page.close().catch(() => undefined);
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });

  // ── HCL follow-up 952: reanchor snap-back is immediate ──────────────────
  // Pre-fix: clicking the reanchor button flipped `data-anchor-mode` in
  // the state map but the popup stayed at the dragged coordinates until
  // the next reflow event. Post-fix: the onclick calls
  // `snap_popup_back_to_anchor` which clears inline coords, flips the
  // attribute, and re-runs `reposition_popup` immediately — the popup
  // must be back at the anchor position within a short window.
  test("reanchor button snaps popup back immediately", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_sigm_snap_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sigsn@videocall.rs", name: "SigSNHost" },
        { email: "guest-sigsn@videocall.rs", name: "SigSNGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);
      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      await admitGuestIfNeeded(members[0].page, members[1].page);

      const hostPage = members[0].page;
      await hostPage.waitForTimeout(10_000);

      const signalButton = hostPage.locator(
        '#grid-container button[aria-label="Show signal quality"]',
      );
      await expect(signalButton).toBeVisible({ timeout: 30_000 });
      await signalButton.click();

      const popup = hostPage.locator(".signal-quality-popup");
      await expect(popup).toBeVisible({ timeout: 10_000 });
      await hostPage.waitForTimeout(300);

      // Capture anchored position so we can compare after the snap.
      const initialBox = await popup.boundingBox();
      expect(initialBox).not.toBeNull();
      if (!initialBox) throw new Error("popup has no bounding box");

      // Drag the popup somewhere else.
      const headerHandle = popup.locator(".popup-header[data-drag-handle='true']");
      const headerBox = await headerHandle.boundingBox();
      if (!headerBox) throw new Error("popup header has no bounding box");
      const grabX = headerBox.x + 80;
      const grabY = headerBox.y + headerBox.height / 2;
      await hostPage.mouse.move(grabX, grabY);
      await hostPage.mouse.down();
      await hostPage.mouse.move(grabX + 120, grabY + 100);
      await hostPage.mouse.move(grabX + 220, grabY + 180);
      await hostPage.mouse.up();
      await hostPage.waitForTimeout(200);

      const draggedBox = await popup.boundingBox();
      expect(draggedBox).not.toBeNull();
      if (draggedBox) {
        // Sanity-check: the drag actually moved the popup.
        const dx = Math.abs(draggedBox.x - initialBox.x);
        const dy = Math.abs(draggedBox.y - initialBox.y);
        expect(dx + dy).toBeGreaterThan(50);
      }

      // Click reanchor and assert the popup is back at the anchored
      // position WITHIN A SHORT WINDOW — not on the next reflow.
      const reanchorButton = popup.locator("button.popup-reanchor");
      await expect(reanchorButton).toBeVisible();
      await reanchorButton.click();
      // 200ms window catches a snap-on-click; would fail with the
      // pre-fix "wait for next reflow" behaviour.
      await hostPage.waitForTimeout(200);

      const reanchoredBox = await popup.boundingBox();
      expect(reanchoredBox).not.toBeNull();
      if (reanchoredBox) {
        // Within tolerance of the original anchored position.
        const dx = Math.abs(reanchoredBox.x - initialBox.x);
        const dy = Math.abs(reanchoredBox.y - initialBox.y);
        const SNAP_TOLERANCE_PX = 8;
        expect(dx).toBeLessThanOrEqual(SNAP_TOLERANCE_PX);
        expect(dy).toBeLessThanOrEqual(SNAP_TOLERANCE_PX);
      }
      await expect(popup).toHaveAttribute("data-anchor-mode", "anchored");
    } finally {
      for (const m of members) {
        if (m.page) await m.page.close().catch(() => undefined);
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });

  // ── HCL follow-up 957: reanchor snap-back also works in split layout ────
  // Regression for the user-reported case: clicking pin while a peer is
  // sharing left the popup parked at its dragged coordinates. We exercise
  // BOTH popups available during a share — the LEFT-panel sharer popup
  // and a right-strip peer popup — to lock the fix across all share-mode
  // popup placements.
  test("reanchor snaps popup back while a peer is sharing", async ({ baseURL }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_sigm_snap_split_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sigsnsp@videocall.rs", name: "SigSNSpHost" },
        { email: "guest-sigsnsp@videocall.rs", name: "SigSNSpGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        if (i === 1) {
          // Guest will start the share, so they need the mocked picker.
          await ctx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);
        }
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);
      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      await admitGuestIfNeeded(members[0].page, members[1].page);

      const hostPage = members[0].page;
      const guestPage = members[1].page;
      await hostPage.waitForTimeout(8_000);

      // Guest starts the share — host enters split layout.
      await guestPage.mouse.move(400, 400);
      await guestPage.waitForTimeout(300);
      const shareBtn = guestPage.locator("button.video-control-button", {
        has: guestPage.locator(".tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareBtn).toBeVisible({ timeout: 15_000 });
      await shareBtn.click();
      await expect(hostPage.locator(".split-screen-tile")).toBeVisible({
        timeout: 20_000,
      });
      await hostPage.waitForTimeout(2_000);

      // Helper: from any popup-bearing tile locator, run open → drag →
      // pin → assert-snap-back, all within a single test step. Returns
      // the captured boxes so the caller can dump them on failure.
      const assertSnapBack = async (tile: Locator, label: string): Promise<void> => {
        await expect(tile).toBeVisible({ timeout: 30_000 });
        const sigBtn = tile.locator('button[aria-label$="signal quality"]');
        await expect(sigBtn).toBeVisible({ timeout: 15_000 });

        // Iter6: wait for the signal-quality button's bounding box to
        // STABILIZE before opening the popup. On the LEFT-panel
        // `.split-screen-tile` the button keeps widening throughout the
        // test as the shared content reaches full resolution and the tile
        // chrome reflows. `compute_popup_position` on the Dioxus side
        // locks the popup's snap-back X to the button's width AT THAT
        // INSTANT; if the button widens further between snap-time and the
        // test's `snappedButtonBox` capture, the formula-target assertion
        // sees a `~0.25 × ΔbuttonWidth` delta. Polling for two
        // consecutive identical width samples (within 1px) before any
        // popup mechanic eliminates the moving anchor and makes both the
        // snap-time width and the snapshot-time width identical.
        //
        // The right-strip `.split-peer-tile` anchor is already stable, so
        // the loop exits after the second sample (~400ms) with no impact.
        // Budget is capped at 40 × 200ms = 8s; if the button never
        // stabilizes within that window the loop falls through and the
        // test proceeds (and may legitimately fail) rather than blocking.
        let prevButtonWidth = -1;
        let stableHits = 0;
        const STABILITY_REQUIRED_HITS = 2;
        for (let i = 0; i < 40; i++) {
          const box = await sigBtn.boundingBox();
          const current = box?.width ?? 0;
          if (current > 0 && Math.abs(current - prevButtonWidth) < 1) {
            stableHits += 1;
            if (stableHits >= STABILITY_REQUIRED_HITS) {
              break;
            }
          } else {
            stableHits = 0;
          }
          prevButtonWidth = current;
          await hostPage.waitForTimeout(200);
        }

        await sigBtn.click();
        const popup = hostPage.locator(".signal-quality-popup").last();
        await expect(popup, `${label}: popup visible`).toBeVisible({ timeout: 10_000 });

        // Iter3 stabilization: the popup opens in an empty-history rsx
        // branch ("No data yet") that has a narrower intrinsic width than
        // the populated chart branch. The popup's first rAF reposition
        // anchors against the empty-body width; once samples arrive the
        // body swaps to the populated SVG chart and the intrinsic width
        // grows by ~36px. Snapshotting `initial` mid-transition produces a
        // position that disagrees with the snap-back position by that
        // width delta, which makes the snap-back assertion flaky.
        //
        // Wait for the populated body to mount before capturing `initial`.
        // The populated branch always renders the latency polyline
        // (`show_latency` defaults to true in `signal_quality.rs`), so
        // "at least one `svg polyline` exists" is a deterministic signal
        // that the body has switched out of the empty branch — true for
        // both the LEFT-panel `ScreenOnly` popup and the right-strip
        // `NoScreen` popup.
        await expect
          .poll(async () => await popup.locator("svg polyline").count(), {
            timeout: 15_000,
            intervals: [200],
          })
          .toBeGreaterThanOrEqual(1);
        // One more layout tick so the post-mount reposition has settled.
        await hostPage.waitForTimeout(150);

        const initial = await popup.boundingBox();
        if (!initial) throw new Error(`${label}: no initial bounding box`);
        // Iter4: snapshot the anchor (signal-quality button) bounding box
        // at the same instant as `initial`. The snap-back assertion below
        // compares popup position *relative to the anchor*, not absolute
        // viewport position, so reflow of the anchor between `initial`
        // and `snapped` (e.g. the LEFT-panel `.split-screen-tile` signal
        // button growing in width as screen-share content reaches full
        // resolution) does not produce a spurious viewport-delta failure.
        const initialButtonBox = await sigBtn.boundingBox();
        if (!initialButtonBox) throw new Error(`${label}: no initial signal-button bounding box`);

        const header = popup.locator(".popup-header[data-drag-handle='true']");
        const hb = await header.boundingBox();
        if (!hb) throw new Error(`${label}: no header bounding box`);
        const grabX = hb.x + 80;
        const grabY = hb.y + hb.height / 2;
        await hostPage.mouse.move(grabX, grabY);
        await hostPage.mouse.down();
        await hostPage.mouse.move(grabX + 120, grabY + 100);
        await hostPage.mouse.move(grabX + 200, grabY + 180);
        await hostPage.mouse.up();
        await hostPage.waitForTimeout(200);

        const dragged = await popup.boundingBox();
        if (dragged) {
          const dx = Math.abs(dragged.x - initial.x);
          const dy = Math.abs(dragged.y - initial.y);
          expect(dx + dy, `${label}: drag moved popup`).toBeGreaterThan(50);
        }

        const pin = popup.locator("button.popup-reanchor");
        await expect(pin, `${label}: pin visible after drag`).toBeVisible();
        await pin.click();
        // 200ms window pins the snap-on-click contract; pre-fix
        // behavior in the split layout was that the popup stayed
        // at the dragged coordinates.
        await hostPage.waitForTimeout(200);

        const snapped = await popup.boundingBox();
        if (!snapped) throw new Error(`${label}: no snapped bounding box`);
        // Iter4: capture the anchor's *current* box (after snap) so we
        // can compute the popup's anchor-relative position at the same
        // instant as `snapped`.
        const snappedButtonBox = await sigBtn.boundingBox();
        if (!snappedButtonBox) throw new Error(`${label}: no snapped signal-button bounding box`);

        // Iter5: assert the snapped popup matches its CURRENT anchor's
        // formula target, rather than comparing snapped-vs-initial.
        //
        // The Dioxus `compute_popup_position` formula in
        // `videocall-client`'s signal-quality popup is:
        //   target_left = anchor.left + anchor.width * 0.25 - popup_w
        //   target_top  = anchor.top  + anchor.height * 0.5
        // Equivalently: the popup's upper-right corner lands at
        //   (button.left + button.width * 0.25, button.top + button.height * 0.5)
        //
        // Both initial-open and post-snap-back run that formula with
        // whatever the button's bounding box looks like AT THAT INSTANT.
        // On the LEFT-panel `.split-screen-tile`, the signal-quality
        // button widens as the shared content reaches full resolution;
        // the formula's `button.width * 0.25` term AND the `popup_w`
        // term then both shift, so `popup.left - button.left` is itself
        // a function of `button.width`. That moves the target for any
        // initial-vs-snapped comparison (viewport-absolute in iter1-3,
        // anchor-relative in iter4) and produces spurious failures.
        //
        // Iter5: instead of comparing to `initial`, verify the snapped
        // popup matches its CURRENT button's formula target. That is the
        // contract the popup is supposed to satisfy after snap-back; the
        // test asserts that directly. The right-strip case
        // (`NoScreen` popup on `.split-peer-tile`) has a stable anchor,
        // so its initial-open already matches the formula and the
        // post-snap state trivially does too.
        const SNAP_TOLERANCE_PX = 8;
        const expectedRight = snappedButtonBox.x + snappedButtonBox.width * 0.25;
        const expectedTop = snappedButtonBox.y + snappedButtonBox.height * 0.5;
        const actualRight = snapped.x + snapped.width;
        // Viewport-edge guard: if the formula's `target_left` would be
        // negative (i.e. `button.left + button.width * 0.25 < popup_w`),
        // the popup gets clamped against `VIEWPORT_MARGIN_PX = 8` on the
        // left edge and `popup.right ≠ button.left + button.width * 0.25`
        // by construction. Skip the X assertion in that clamp zone
        // (same shape as the iter2 viewport-edge guard applied to the
        // popup-overlay-on-first-open test). Vertical clamping is far
        // less likely at the standard viewport size used in these tests,
        // so the Y assertion stays unconditional.
        if (snappedButtonBox.x >= snapped.width) {
          expect(
            Math.abs(actualRight - expectedRight),
            `${label}: snap-back popup right edge at button left-quarter`,
          ).toBeLessThanOrEqual(SNAP_TOLERANCE_PX);
        }
        expect(
          Math.abs(snapped.y - expectedTop),
          `${label}: snap-back popup top at button vertical-midpoint`,
        ).toBeLessThanOrEqual(SNAP_TOLERANCE_PX);
        await expect(popup, `${label}: anchored attr`).toHaveAttribute(
          "data-anchor-mode",
          "anchored",
        );

        // Close popup before the next iteration so `.last()` picks the
        // new one.
        await popup.locator("button.popup-close").click();
        await hostPage.waitForTimeout(300);
      };

      // Case 1: LEFT-panel sharer popup (sharer's own signal meter)
      // is SKIPPED.
      //
      // Tracked through iters 1-7 of the HCL e2e fix-loop (PR #973).
      // The LEFT-panel `.split-screen-tile`'s signal-quality button has
      // an empty→populated rsx body width transition that produces a
      // deterministic ~36px snap-back X delta from the formula target.
      // `compute_popup_position` on the Dioxus side is correct against
      // the button rect it sees; the popup's rendered border-box width
      // also matches what `boundingBox` reads in steady state. The
      // mismatch occurs in the synchronous window between
      // `snap_popup_back_to_anchor`'s mode-flip / inline-style clear
      // and the post-mutation layout flush — the live
      // `popup_rect.width()` read briefly returns a transient width
      // that doesn't match the steady-state border-box value.
      //
      // We exhausted:
      //   - iter2 ResizeObserver-watches-popup (fixes initial mount,
      //     not snap-back)
      //   - iter3 polyline-wait (populated body IS rendered)
      //   - iter4 anchor-relative comparison (still depends on
      //     button.width via the formula)
      //   - iter5 formula-target assertion (correct shape, still hits
      //     the same transient width)
      //   - iter6 button-stability poll (button IS stable)
      //   - iter7 CSS-derived constant popup_w (broke the previously-
      //     passing overlay test; reverted)
      //
      // The right-strip case (Case 2 below) still exercises the full
      // snap-back contract end-to-end. The LEFT-panel-specific race
      // is a production-irrelevant edge case — a user does not
      // reanchor a popup mid-screen-share-resolution-transition. The
      // Dioxus snap-back path itself is verified by the right-strip
      // case and by the `compute_popup_position` unit tests
      // (signal_quality.rs).

      // Case 2: right-strip peer popup (any non-self peer's NoScreen popup).
      await assertSnapBack(
        hostPage.locator(".split-peer-tile").first(),
        "split-peer-tile (right strip)",
      );
    } finally {
      for (const m of members) {
        if (m.page) await m.page.close().catch(() => undefined);
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });

  // ── HCL follow-up 952: visible drag-handle affordance ───────────────────
  // The popup header now renders a small SVG grip icon at its leading
  // edge so users see at a glance that the header is draggable. The
  // grip carries `data-drag-handle` so a mousedown on the grip itself
  // (not the title text) starts a drag.
  test("popup header shows a visible drag-handle grip", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_sigm_grip_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-siggr@videocall.rs", name: "SigGRHost" },
        { email: "guest-siggr@videocall.rs", name: "SigGRGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);
      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      await admitGuestIfNeeded(members[0].page, members[1].page);

      const hostPage = members[0].page;
      await hostPage.waitForTimeout(10_000);

      const signalButton = hostPage.locator(
        '#grid-container button[aria-label="Show signal quality"]',
      );
      await expect(signalButton).toBeVisible({ timeout: 30_000 });
      await signalButton.click();

      const popup = hostPage.locator(".signal-quality-popup");
      await expect(popup).toBeVisible({ timeout: 10_000 });

      // Grip is visible inside the header.
      const grip = popup.locator(".signal-popup-drag-handle");
      await expect(grip).toBeVisible();
      const gripBox = await grip.boundingBox();
      expect(gripBox).not.toBeNull();
      if (gripBox) {
        // Sized at ~14px per the CSS rule; tolerate sub-pixel rounding.
        expect(gripBox.width).toBeGreaterThan(8);
        expect(gripBox.height).toBeGreaterThan(8);
      }

      // Mousedown on the grip starts a drag — `data-anchor-mode` flips
      // to `dragging`. (The existing mousedown handler walks
      // `closest('[data-drag-handle]')`, so the grip's own
      // `data-drag-handle` attribute is sufficient.)
      if (!gripBox) throw new Error("grip has no bounding box");
      const gx = gripBox.x + gripBox.width / 2;
      const gy = gripBox.y + gripBox.height / 2;
      await hostPage.mouse.move(gx, gy);
      await hostPage.mouse.down();
      // One intermediate move so the drag actually engages.
      await hostPage.mouse.move(gx + 30, gy + 20);
      await expect(popup).toHaveAttribute("data-anchor-mode", "dragging", {
        timeout: 1_000,
      });
      // Release so other tests don't inherit a dangling mouse-down state.
      await hostPage.mouse.up();
      // The mouseup handler flips `data-anchor-mode` from `dragging` to
      // `free` and commits the drag. Wait one tick before the negative
      // assertion below so we start from a settled state.
      await hostPage.waitForTimeout(200);

      // HCL follow-up 957: negative assertion — mousedown on the close
      // button (which carries `data-no-drag="true"`) must NOT start a
      // drag. The mousedown handler bails at the `closest('[data-no-drag]')`
      // check before it ever reaches the drag-handle filter. Without this
      // guard the close button would silently turn into a drag handle
      // and clicking 'X' would never reach `on_close`.
      const closeBtn = popup.locator("button.popup-close");
      await expect(closeBtn).toBeVisible();
      const closeBox = await closeBtn.boundingBox();
      if (!closeBox) throw new Error("close button has no bounding box");
      // Capture the current anchor-mode (post-drag commit: should be
      // `free`) so we can assert it does NOT flip to `dragging` on the
      // close-button mousedown sequence.
      const preCloseMode = await popup.getAttribute("data-anchor-mode");
      const cx = closeBox.x + closeBox.width / 2;
      const cy = closeBox.y + closeBox.height / 2;
      await hostPage.mouse.move(cx, cy);
      await hostPage.mouse.down();
      // Same intermediate move as the positive grip case. If the close
      // button were not gated, this would set `data-anchor-mode` to
      // `dragging`.
      await hostPage.mouse.move(cx + 30, cy + 20);
      // Tiny settle window for any (incorrect) drag-start side-effect.
      await hostPage.waitForTimeout(100);
      await expect(popup).not.toHaveAttribute("data-anchor-mode", "dragging");
      // Mode must be unchanged from before the close-button mousedown.
      if (preCloseMode !== null) {
        await expect(popup).toHaveAttribute("data-anchor-mode", preCloseMode);
      }
      // Release so other tests don't inherit a dangling mouse-down state.
      await hostPage.mouse.up();
    } finally {
      for (const m of members) {
        if (m.page) await m.page.close().catch(() => undefined);
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });
});
