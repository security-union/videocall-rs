import { test, expect, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { chromium } from "@playwright/test";

/**
 * Signal-quality popup — portal positioning and multi-popup persistence.
 *
 * Regression coverage for the popup-clipping bug where the
 * `SignalQualityPopup` was rendered as a child of the tile's
 * `.canvas-container` and got clipped by that container's
 * `overflow: hidden` border-radius (added in PR #923) on small tiles in
 * dense grids, plus the follow-up requirement that the popup remain open
 * until the user explicitly closes it via the "X" button (no auto-close
 * on Esc / click-outside), so multiple per-peer popups can be open
 * simultaneously.
 *
 * The fix renders the popup as a sibling of `.canvas-container` and
 * applies the new `.signal-quality-popup-portal` class, which uses
 * `position: fixed` (z-index 9400, above peer list / below modal
 * dialogs). A JS-driven anchor effect reads the source tile's
 * `getBoundingClientRect()` and writes `top` / `left` on the popup so it
 * stays glued to the tile across resize / scroll / grid reflow events.
 *
 * The relevant code:
 *   - `dioxus-ui/src/components/signal_quality.rs`
 *     - `compute_popup_position()` (pure position math, unit-tested)
 *     - `install_popup_anchor()` (ResizeObserver + window listeners)
 *   - `dioxus-ui/src/components/canvas_generator.rs` (3 call sites moved
 *     out of `.canvas-container`)
 *   - `dioxus-ui/static/style.css` (.signal-quality-popup-portal class)
 *
 * What this spec asserts:
 *
 *   1. After clicking the signal-bars icon, the popup is rendered as a
 *      sibling of `.canvas-container` (NOT a descendant), proving the
 *      portal-mode DOM hoist is in effect.
 *   2. The popup has `position: fixed` and z-index >= 9400, proving the
 *      stacking-context escape from the tile's `overflow: hidden`.
 *   3. Resizing the viewport keeps the popup inside the viewport
 *      (clamp / flip math from `compute_popup_position`).
 *   4. Pressing `Escape` does NOT dismiss the popup.
 *   5. Clicking outside the popup does NOT dismiss the popup.
 *   6. With multiple peers in the meeting, opening two signal-quality
 *      popups keeps BOTH visible at the same time; closing one via its
 *      "X" button leaves the other untouched.
 *
 * Coverage of the unchanged popup content (transport badge, chart, etc.)
 * lives in `signal-quality-peer-transport.spec.ts`; this spec
 * intentionally focuses on the positioning/dismissal behavior.
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

/**
 * Click the "Start Meeting" / "Join Meeting" button and wait for the meeting
 * grid to appear. Mirrors the same helper in
 * `signal-quality-peer-transport.spec.ts`.
 */
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

test.describe("Signal-quality popup — portal positioning", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("popup escapes tile clip, position is fixed, Esc and click-outside do NOT dismiss", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_sigq_portal_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sigqp@videocall.rs", name: "SigQPHost" },
        { email: "guest-sigqp@videocall.rs", name: "SigQPGuest" },
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

      const joinButton = members[1].page.getByRole("button", {
        name: /Start Meeting|Join Meeting/,
      });
      const waitingRoom = members[1].page.getByText("Waiting to be admitted");
      const guestGrid = members[1].page.locator("#grid-container");

      const result = await Promise.race([
        joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
        waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
        guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
      ]);

      if (result === "waiting") {
        const admitButton = members[0].page.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await members[0].page.waitForTimeout(1000);
        await admitButton.dispatchEvent("click");
        await members[0].page.waitForTimeout(3000);
      }

      if (result !== "auto-joined") {
        await clickJoinAndEnterGrid(members[1].page);
      } else {
        await expect(guestGrid).toBeVisible({ timeout: 15_000 });
      }

      // Settle the mesh so the peer tile (and its signal-bars button) is
      // rendered on the host's side.
      await members[0].page.waitForTimeout(10_000);

      const hostPage = members[0].page;
      const tileCanvas = hostPage.locator("#grid-container .canvas-container");
      await expect(tileCanvas).toHaveCount(1, { timeout: 30_000 });

      // ── 1. Open the popup ────────────────────────────────────────────
      const signalButton = hostPage.locator(
        '#grid-container button[aria-label="Show signal quality"]',
      );
      await expect(signalButton).toBeVisible({ timeout: 15_000 });
      await signalButton.click();

      const popup = hostPage.locator(".signal-quality-popup");
      await expect(popup).toBeVisible({ timeout: 10_000 });

      // ── 2. Portal hoist: popup must NOT be a descendant of any tile's
      //    `.canvas-container`. If the fix regresses, the popup would
      //    be back inside `.canvas-container` and this count would be
      //    1 instead of 0.
      const popupInsideCanvas = hostPage.locator(
        "#grid-container .canvas-container .signal-quality-popup",
      );
      await expect(popupInsideCanvas).toHaveCount(0);

      // ── 3. position: fixed + z-index >= 9400 ─────────────────────────
      const computed = await popup.evaluate((el) => {
        const cs = window.getComputedStyle(el);
        return {
          position: cs.position,
          zIndex: cs.zIndex,
        };
      });
      expect(computed.position).toBe("fixed");
      // Browsers serialize z-index as a number string ("9400") or
      // "auto" if unset. We compare numerically with tolerance for
      // any future increase.
      expect(Number(computed.zIndex)).toBeGreaterThanOrEqual(9400);

      // ── 4. Popup is inside the viewport ──────────────────────────────
      const viewportBox = hostPage.viewportSize();
      expect(viewportBox).not.toBeNull();
      if (viewportBox) {
        const popupBox = await popup.boundingBox();
        expect(popupBox).not.toBeNull();
        if (popupBox) {
          // The popup must sit fully inside the viewport, with no
          // overflow on either axis. This is the asseration that
          // would fail today (pre-fix) on a small tile because the
          // popup was clipped by the tile's `overflow: hidden`.
          expect(popupBox.x).toBeGreaterThanOrEqual(0);
          expect(popupBox.y).toBeGreaterThanOrEqual(0);
          expect(popupBox.x + popupBox.width).toBeLessThanOrEqual(viewportBox.width + 1);
          expect(popupBox.y + popupBox.height).toBeLessThanOrEqual(viewportBox.height + 1);
        }
      }

      // ── 5. Resize the viewport; popup repositions to stay on-screen ─
      const narrowedSize = { width: 800, height: 600 };
      await hostPage.setViewportSize(narrowedSize);
      // Give the ResizeObserver + the rAF inside install_popup_anchor()
      // time to re-run.
      await hostPage.waitForTimeout(500);
      await expect(popup).toBeVisible();
      const popupBoxAfterResize = await popup.boundingBox();
      expect(popupBoxAfterResize).not.toBeNull();
      if (popupBoxAfterResize) {
        expect(popupBoxAfterResize.x).toBeGreaterThanOrEqual(0);
        expect(popupBoxAfterResize.x + popupBoxAfterResize.width).toBeLessThanOrEqual(
          narrowedSize.width + 1,
        );
        expect(popupBoxAfterResize.y + popupBoxAfterResize.height).toBeLessThanOrEqual(
          narrowedSize.height + 1,
        );
      }

      // ── 6. Esc does NOT dismiss ─────────────────────────────────────
      // Multi-popup UX: keystrokes should never collapse open popups.
      // The only sanctioned dismiss path is the "X" close button.
      await hostPage.keyboard.press("Escape");
      await hostPage.waitForTimeout(300);
      await expect(popup).toBeVisible();

      // ── 7. Click-outside does NOT dismiss ───────────────────────────
      // The previous transparent backdrop (z-index 9399) has been
      // removed — clicks outside the popup must pass through to whatever
      // they hit (other tiles, controls, empty space) without closing
      // anything.  Click on the page <body> at a coordinate that the
      // popup's bounding box does not cover.
      const popupBoxForOutside = await popup.boundingBox();
      expect(popupBoxForOutside).not.toBeNull();
      if (popupBoxForOutside) {
        // Pick a click target far from the popup; (10, 10) is the
        // top-left of the viewport, which is well outside the popup
        // both before and after the narrowed-viewport resize above.
        await hostPage.mouse.click(10, 10);
        await hostPage.waitForTimeout(300);
        await expect(popup).toBeVisible();
      }

      // The deprecated backdrop element must no longer be rendered.
      await expect(hostPage.locator(".signal-quality-popup-backdrop")).toHaveCount(0);

      // ── 8. Explicit "X" close still works ───────────────────────────
      // This is the only dismiss path that should ever fire.
      await popup.locator("button.popup-close").click();
      await expect(popup).toBeHidden({ timeout: 5_000 });
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

  test("multiple per-peer popups stay open simultaneously; X on one leaves the other open", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_sigq_multi_${Date.now()}`;

    // Three members: host + two guests. The host's grid will render two
    // peer tiles, each with its own signal-quality button. We open both
    // popups and assert they coexist.
    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sigqm@videocall.rs", name: "SigQMHost" },
        { email: "guest1-sigqm@videocall.rs", name: "SigQMGuest1" },
        { email: "guest2-sigqm@videocall.rs", name: "SigQMGuest2" },
      ];

      for (let i = 0; i < 3; i++) {
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

      // Host joins first, then admits each guest.
      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);

      for (let i = 1; i < 3; i++) {
        members[i].page = await joinMeetingAs(members[i].context, meetingId, profiles[i].name);

        const joinButton = members[i].page.getByRole("button", {
          name: /Start Meeting|Join Meeting/,
        });
        const waitingRoom = members[i].page.getByText("Waiting to be admitted");
        const guestGrid = members[i].page.locator("#grid-container");

        const result = await Promise.race([
          joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
          waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
          guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
        ]);

        if (result === "waiting") {
          // Each guest produces its own "Admit" button on the host page;
          // .first() picks the topmost pending guest.
          const admitButton = members[0].page.getByTitle("Admit").first();
          await expect(admitButton).toBeVisible({ timeout: 20_000 });
          await members[0].page.waitForTimeout(1000);
          await admitButton.dispatchEvent("click");
          await members[0].page.waitForTimeout(3000);
        }

        if (result !== "auto-joined") {
          await clickJoinAndEnterGrid(members[i].page);
        } else {
          await expect(guestGrid).toBeVisible({ timeout: 15_000 });
        }
      }

      // Let the mesh settle so both guest tiles + their signal-meter
      // buttons are mounted on the host side.
      await members[0].page.waitForTimeout(12_000);

      const hostPage = members[0].page;
      const signalButtons = hostPage.locator(
        '#grid-container button[aria-label="Show signal quality"]',
      );
      // Two guest tiles ⇒ two signal-quality buttons on the host side.
      await expect(signalButtons).toHaveCount(2, { timeout: 30_000 });

      // ── Open popup A ────────────────────────────────────────────────
      await signalButtons.nth(0).click();
      const popups = hostPage.locator(".signal-quality-popup");
      await expect(popups).toHaveCount(1, { timeout: 10_000 });

      // ── Open popup B without closing A ──────────────────────────────
      // Pre-fix, the transparent backdrop installed by popup A swallowed
      // the click on tile B's signal-meter button and the count would
      // toggle from 1 → 1 (A closed, B opened). With the backdrop gone
      // the click reaches the second button and we end up with 2 popups.
      await signalButtons.nth(1).click();
      await expect(popups).toHaveCount(2, { timeout: 10_000 });

      // Both popups are visually present.
      await expect(popups.nth(0)).toBeVisible();
      await expect(popups.nth(1)).toBeVisible();

      // ── Close popup A via its X; B must remain open ─────────────────
      // Each popup carries its own peer_id in its DOM id
      // (`signal-quality-popup-<peer_id>`) so we identify popup A by the
      // first `.signal-quality-popup` in document order.
      const popupA = popups.nth(0);
      const popupAId = await popupA.getAttribute("id");
      expect(popupAId).toMatch(/^signal-quality-popup-/);
      await popupA.locator("button.popup-close").click();
      await expect(popups).toHaveCount(1, { timeout: 5_000 });

      // The surviving popup must NOT be popup A.
      const survivor = popups.first();
      const survivorId = await survivor.getAttribute("id");
      expect(survivorId).toMatch(/^signal-quality-popup-/);
      expect(survivorId).not.toBe(popupAId);
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
});
