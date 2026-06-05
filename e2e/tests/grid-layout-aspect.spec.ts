import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Grid layout aspect-ratio regressions (HCL bugs #6 and #7).
 *
 * Bug #7 — 2-peer meeting (host + 1 remote, no screen-share): the lone
 * remote tile must FILL the viewport, not be a 3:2-capped tile centered
 * with surplus padding. The fix is `.participants-1 .grid-item.full-bleed`
 * dropping the aspect-ratio cap and using `place-self: stretch`.
 *
 * Bug #6 — 3-peer meeting (host + 2 remote, no screen-share): both tiles
 * MUST hold the 3:2 aspect ratio regardless of viewport shape. The fix is
 * the `tile_count == 1` branch in `attendants.rs::container_style` which
 * forces 2+ tiles to use the natural-tile-size track path. Before the fix
 * `tile_count <= 2` used `1fr` cells and on narrow viewports stretched
 * the tiles vertically because `.grid-item { height: 100%, max-width:
 * 100% }` clamps width but keeps full height.
 *
 * Both tests run without screen-share (which can't be driven from
 * headless Chromium without the getDisplayMedia mock).
 */

async function navigateAndJoin(page: Page, meetingId: string, username: string): Promise<void> {
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

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waiting = page.getByText("Waiting to be admitted");
  const grid = page.locator("#grid-container");

  const r = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waiting.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
  ]);

  if (r === "join") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
    await expect(grid).toBeVisible({ timeout: 15_000 });
  } else if (r === "waiting") {
    // host needs to admit — caller handles
    return;
  }
}

async function admitFromHost(hostPage: Page, guestPage: Page): Promise<void> {
  const admitButton = hostPage.getByTitle("Admit").first();
  if (await admitButton.isVisible({ timeout: 20_000 }).catch(() => false)) {
    await hostPage.waitForTimeout(800);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

    const guestJoin = guestPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    const guestGrid = guestPage.locator("#grid-container");
    const r = await Promise.race([
      guestJoin.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (r === "join") {
      await guestPage.waitForTimeout(800);
      await guestJoin.click();
      await guestPage.waitForTimeout(2500);
      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    }
  }
}

test.describe("Grid layout aspect ratio", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ──────────────────────────────────────────────────────────────────────
  // Bug #7: 2-peer meeting → lone remote tile fills the viewport.
  //
  // Before the fix the `.grid-item.full-bleed` carried `aspect-ratio: 3/2`
  // inherited from `.grid-item`. The viewport-area-minus-paddings was wider
  // than 3:2 of its height on most desktops, so the tile sat as a height-
  // bound 3:2 rectangle centered with surplus on left/right (or top/bottom
  // on portrait). The user's report: "tile doesn't fill the area."
  // ──────────────────────────────────────────────────────────────────────
  test("2-peer meeting renders the remote tile as participants-1 full-bleed without an aspect ratio cap", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_2peer_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "2peer-host@videocall.rs",
        "TwoPeerHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "2peer-guest@videocall.rs",
        "TwoPeerGuest",
        uiURL,
      );
      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Force a fixed viewport so the assertion has a deterministic frame.
      await hostPage.setViewportSize({ width: 1600, height: 900 });

      await navigateAndJoin(hostPage, meetingId, "TwoPeerHost");
      await navigateAndJoin(guestPage, meetingId, "TwoPeerGuest");
      await admitFromHost(hostPage, guestPage);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
      await hostPage.waitForTimeout(5000);

      // The host sees ONE remote tile. The grid container must carry
      // `participants-1` and the tile must be `.grid-item.full-bleed`.
      const grid = hostPage.locator("#grid-container.participants-1");
      await expect(grid).toBeVisible({ timeout: 15_000 });

      const tile = grid.locator(".grid-item.full-bleed");
      await expect(tile).toHaveCount(1, { timeout: 10_000 });

      // The load-bearing assertion for bug #7: the tile's aspect-ratio
      // CSS must be `auto` (the `.participants-1` override). A regression
      // that drops the override would leave `aspect-ratio: 3 / 2`
      // inherited from `.grid-item` and fail this assertion.
      const aspectRatio = await tile.evaluate((el) => window.getComputedStyle(el).aspectRatio);
      expect(aspectRatio).toBe("auto");

      // The tile should also have `place-self: stretch` (or equivalent)
      // so it actually expands to its grid cell. Browsers split
      // `place-self: stretch` into `align-self: stretch` and
      // `justify-self: stretch` on computed style.
      const alignSelf = await tile.evaluate((el) => window.getComputedStyle(el).alignSelf);
      const justifySelf = await tile.evaluate((el) => window.getComputedStyle(el).justifySelf);
      expect(alignSelf).toBe("stretch");
      expect(justifySelf).toBe("stretch");

      // Sanity: tile should be a significant fraction of the grid area.
      // Pre-fix it would be capped at 900 * 1.5 = 1350px wide of an
      // ~1560px available area (≤87%). Post-fix it fills the area.
      const tileWidth = await tile.evaluate((el) => el.getBoundingClientRect().width);
      const gridWidth = await grid.evaluate((el) => el.getBoundingClientRect().width);
      const ratio = tileWidth / gridWidth;
      expect(ratio).toBeGreaterThan(0.95);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // Bug #6: 3-peer meeting → both tiles maintain the 3:2 aspect ratio
  // even on viewports where the cell would otherwise stretch.
  //
  // The pre-fix `tile_count <= 2` branch used `1fr` columns + stretch
  // packing. On narrower viewports the cell became taller than
  // `cell_w * 2/3`, and `.grid-item { height: 100%; max-width: 100% }`
  // pinned height while clamping width — resulting in tiles taller than
  // 3:2.
  //
  // We deliberately use a TALL viewport (900x1200) to force the regression
  // scenario: the natural cell will be width-bound, and the post-fix code
  // must use `var(--tile-w) / var(--tile-h)` tracks so the cell is the
  // tile's 3:2 footprint, not a stretched 1fr.
  // ──────────────────────────────────────────────────────────────────────
  test("3-peer meeting holds 3:2 aspect on tall viewport (no 1fr stretch)", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3peer_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    const browser3 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "3peer-host@videocall.rs",
        "ThreePeerHost",
        uiURL,
      );
      const g1Ctx = await createAuthenticatedContext(
        browser2,
        "3peer-g1@videocall.rs",
        "ThreePeerGuest1",
        uiURL,
      );
      const g2Ctx = await createAuthenticatedContext(
        browser3,
        "3peer-g2@videocall.rs",
        "ThreePeerGuest2",
        uiURL,
      );
      const hostPage = await hostCtx.newPage();
      const g1Page = await g1Ctx.newPage();
      const g2Page = await g2Ctx.newPage();

      // Tall viewport to force the regression scenario (cell would
      // stretch vertically under the pre-fix code).
      await hostPage.setViewportSize({ width: 900, height: 1200 });

      await navigateAndJoin(hostPage, meetingId, "ThreePeerHost");
      await navigateAndJoin(g1Page, meetingId, "ThreePeerGuest1");
      await admitFromHost(hostPage, g1Page);
      await navigateAndJoin(g2Page, meetingId, "ThreePeerGuest2");
      await admitFromHost(hostPage, g2Page);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
      await hostPage.waitForTimeout(7000);

      // The host sees TWO remote tiles. Container has `participants-2`.
      const grid = hostPage.locator("#grid-container.participants-2");
      await expect(grid).toBeVisible({ timeout: 15_000 });
      const tiles = grid.locator(".grid-item");
      await expect(tiles).toHaveCount(2, { timeout: 15_000 });

      // The load-bearing assertion for bug #6: each tile's actual
      // rendered (width / height) must approximate the 3:2 ratio.
      // Pre-fix on a tall viewport the tile would be width-bound but
      // height = full row (taller than width). Post-fix the tile sits
      // in a `var(--tile-w) / var(--tile-h)` cell that's exactly 3:2.
      for (let i = 0; i < 2; i++) {
        const dims = await tiles.nth(i).evaluate((el) => {
          const r = el.getBoundingClientRect();
          return { w: r.width, h: r.height };
        });
        expect(dims.w).toBeGreaterThan(0);
        expect(dims.h).toBeGreaterThan(0);
        const aspect = dims.w / dims.h;
        // 3:2 = 1.5. Allow 5% tolerance for the 2px border and any
        // sub-pixel rounding by the browser.
        expect(aspect).toBeGreaterThan(1.5 * 0.95);
        expect(aspect).toBeLessThan(1.5 * 1.05);
      }

      // The container's grid-template-columns must reference the
      // computed tile width (`var(--tile-w)`), NOT `1fr`. This is
      // the structural assertion that catches a regression which
      // reverts the `tile_count == 1` branch back to `tile_count <= 2`.
      const containerStyle = await grid.evaluate((el) => el.getAttribute("style") || "");
      expect(containerStyle).toContain("--tile-w");
      expect(containerStyle).toContain("var(--tile-w)");
      // The 1fr stretch path is the bug path; it must not be in the style.
      expect(containerStyle).not.toContain("grid-template-columns: repeat(2, 1fr)");
    } finally {
      await browser1.close();
      await browser2.close();
      await browser3.close();
    }
  });
});
