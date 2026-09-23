import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { joinMeetingFromPage } from "../helpers/two-user-meeting";
import { MEETING_FOOTER } from "../helpers/rust-mirrored-constants";

/**
 * The tile group is centred horizontally in the free band (#2700). The 2+ tile
 * arm is `flex-wrap: wrap; justify-content: center`, so every line centres,
 * including a partial last row; the 1-tile and screen-share arms are untouched.
 * Numbers are acceptance-checklist literals for a 1240x580 meeting area (gap
 * 16, padding 20/20/152/20). The viewport is 1280x720 plus the meeting footer's
 * reserve (issue 2791), which the bottom padding includes.
 *
 * Observed on 3bdabd49, where the arm was start-packed CSS grid: 3 failed,
 * 1 passed — every row started at 20 (340 with the peer list open) instead of
 * 209 (369), and a lone last-row tile sat at 20 instead of centred.
 */

const DEFAULT_UI_URL = "http://localhost:3001";
const DESKTOP = { width: 1280, height: 720 + MEETING_FOOTER.MEETING_FOOTER_RESERVE };

const EPS = 1;

const GRID = "#grid-container";
const PEER_LIST = "#peer-list-container";
/** Both in-flow tile kinds: remote/mock tiles and the self view when in grid. */
const IN_FLOW_TILES = "#grid-container > .grid-item, #grid-container > .host";

interface Rect {
  left: number;
  right: number;
  top: number;
  bottom: number;
  width: number;
  height: number;
}

async function inFlowTiles(page: Page): Promise<Rect[]> {
  return page.evaluate((sel: string) => {
    return Array.from(document.querySelectorAll(sel))
      .map((el): Rect => {
        const r = el.getBoundingClientRect();
        return {
          left: r.left,
          right: r.right,
          top: r.top,
          bottom: r.bottom,
          width: r.width,
          height: r.height,
        };
      })
      .filter((r) => r.width > 0 && r.height > 0)
      .sort((a, b) => a.top - b.top || a.left - b.left);
  }, IN_FLOW_TILES);
}

function rowsOf(tiles: Rect[]): Rect[][] {
  const rows: Rect[][] = [];
  for (const tile of tiles) {
    const row = rows.find((r) => Math.abs(r[0].top - tile.top) <= EPS);
    if (row) row.push(tile);
    else rows.push([tile]);
  }
  return rows;
}

function rowCentre(row: Rect[]): number {
  return (row[0].left + row[row.length - 1].right) / 2;
}

function expectClose(actual: number, expected: number, what: string): void {
  expect(
    Math.abs(actual - expected),
    `${what}: got ${actual}, want ${expected}`,
  ).toBeLessThanOrEqual(EPS);
}

async function joinSoloMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await fillAndSubmitJoinForm(page, meetingId, username);
  await page.waitForTimeout(1000);
  expect(await joinMeetingFromPage(page)).toBe("in-meeting");
  await expect(page.locator(GRID)).toBeVisible({ timeout: 15_000 });
}

async function wakeBar(page: Page): Promise<void> {
  const vp = page.viewportSize() ?? DESKTOP;
  await page.mouse.move(Math.floor(vp.width / 2), Math.floor(vp.height / 2));
  await page.waitForTimeout(300);
}

async function activateSlot(page: Page, inline: string, overflowLabel: string): Promise<void> {
  await wakeBar(page);
  const direct = page.locator(inline).first();
  if ((await direct.count()) > 0 && (await direct.isVisible())) {
    await direct.click({ timeout: 10_000 });
    return;
  }
  const trigger = page.locator("#overflow-menu-trigger");
  await expect(trigger).toBeVisible({ timeout: 10_000 });
  await trigger.click({ timeout: 10_000 });
  const item = page.locator(".action-bar-overflow-popover button.overflow-item", {
    has: page.locator(`span:text-is("${overflowLabel}")`),
  });
  await expect(item).toBeVisible({ timeout: 10_000 });
  await item.click({ timeout: 10_000 });
}

async function openPeerList(page: Page): Promise<void> {
  await activateSlot(page, '[data-testid="peer-list-button"]', "Participants");
  await expect(page.locator(PEER_LIST)).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
}

async function addMockPeers(page: Page, count: number): Promise<void> {
  await wakeBar(page);
  const mockButton = page.locator("button.video-control-button", {
    has: page.locator(".tooltip", { hasText: /Mock Peers/i }),
  });
  await expect(
    mockButton,
    "Mock Peers must be present (MOCK_PEERS_ENABLED=true in docker/docker-compose.e2e.yaml)",
  ).toBeVisible({ timeout: 10_000 });
  await mockButton.click();
  const countInput = page.locator(".mock-peers-popover input[type='number']");
  await expect(countInput).toBeVisible({ timeout: 5_000 });
  await countInput.fill(String(count));
  await page.waitForTimeout(400);
  await page.locator(".mock-peers-popover-close").click();
  await expect(page.locator(".mock-peers-popover")).not.toBeVisible({ timeout: 5_000 });
  await expect(page.locator(`${GRID} .grid-item`)).toHaveCount(count, { timeout: 15_000 });
}

/**
 * A page whose self view is already IN the grid. The acceptance table counts it
 * as one of the n tiles; the shipped default parks it in the corner, out of
 * flow, where it would not participate in the flex line at all.
 */
async function newGridSelfPage(browser: Browser, uiURL: string, who: string): Promise<Page> {
  const ctx = await createAuthenticatedContext(browser, `${who}@videocall.rs`, who, uiURL);
  await ctx.addInitScript(() => {
    try {
      localStorage.setItem("vc_self_view_placement", "grid");
    } catch {
      /* storage unavailable — the placement assertion below reports it */
    }
  });
  const page = await ctx.newPage();
  await page.setViewportSize(DESKTOP);
  return page;
}

/** Asserts the seeded placement took, so `n` really is tiles + self. */
async function expectTileCount(page: Page, n: number): Promise<void> {
  await expect(page.locator(`${GRID} > .host[data-self-placement="grid"]`)).toHaveCount(1, {
    timeout: 10_000,
  });
  await expect.poll(async () => (await inFlowTiles(page)).length, { timeout: 10_000 }).toBe(n);
}

test.describe("The tile group is centred in the free band (#2700)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("four tiles wrap 2x2 and both rows centre on the band", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newGridSelfPage(browser, uiURL, "Centre4");
      await joinSoloMeeting(page, `e2e_centre_n4_${Date.now()}`, "Centre4");
      await addMockPeers(page, 3);
      await expectTileCount(page, 4);

      const rows = rowsOf(await inFlowTiles(page));
      expect(rows.length, "four 423px tiles must wrap into two rows").toBe(2);
      expect(rows.map((r) => r.length)).toEqual([2, 2]);

      for (const [i, row] of rows.entries()) {
        expectClose(row[0].left, 209, `row ${i + 1} first tile left`);
        expectClose(row[1].left, 648, `row ${i + 1} second tile left`);
        expectClose(row[0].width, 423, `row ${i + 1} tile width`);
        expectClose(row[0].height, 282, `row ${i + 1} tile height`);
        expectClose(rowCentre(row), 640, `row ${i + 1} centre`);
      }
      expectClose(rows[0][0].top, 20, "row 1 top");
      expectClose(rows[1][0].top, 318, "row 2 top");
    } finally {
      await browser.close();
    }
  });

  test("a partial last row is centred under the full row @bvt1", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newGridSelfPage(browser, uiURL, "Centre3");
      await joinSoloMeeting(page, `e2e_centre_n3_${Date.now()}`, "Centre3");
      await addMockPeers(page, 2);
      await expectTileCount(page, 3);

      const rows = rowsOf(await inFlowTiles(page));
      expect(
        rows.map((r) => r.length),
        "three tiles must lay out 2 then 1",
      ).toEqual([2, 1]);

      expectClose(rows[0][0].left, 209, "row 1 first tile left");
      expectClose(rows[0][1].left, 648, "row 1 second tile left");
      expectClose(rows[1][0].left, 428.5, "lone row 2 tile left");
      expectClose(rows[1][0].left - rows[0][0].left, 219.5, "half a (tile + gap)");

      expectClose(rowCentre(rows[0]), 640, "row 1 centre");
      expectClose(rowCentre(rows[1]), 640, "row 2 centre");
    } finally {
      await browser.close();
    }
  });

  test("the group re-centres on the band the peer list leaves behind", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newGridSelfPage(browser, uiURL, "CentreBand");
      await joinSoloMeeting(page, `e2e_centre_band_${Date.now()}`, "CentreBand");
      await addMockPeers(page, 3);
      await expectTileCount(page, 4);
      await openPeerList(page);

      // Band 320..1280, so every centre shifts by half the 320px reserve.
      await expect
        .poll(async () => Math.round(rowsOf(await inFlowTiles(page))[0][0].left), {
          timeout: 10_000,
          message: "row 1 must re-pack against the reserved band",
        })
        .toBe(369);

      let rows = rowsOf(await inFlowTiles(page));
      expect(rows.map((r) => r.length)).toEqual([2, 2]);
      for (const [i, row] of rows.entries()) {
        expectClose(row[0].left, 369, `row ${i + 1} first tile left`);
        expectClose(row[1].left, 808, `row ${i + 1} second tile left`);
        expectClose(rowCentre(row), 800, `row ${i + 1} centre`);
      }
      // #2701's bound still holds: nothing crosses into the drawer.
      for (const tile of rows.flat()) {
        expect(tile.left).toBeGreaterThanOrEqual(340 - EPS);
      }

      await addMockPeers(page, 2);
      await expectTileCount(page, 3);
      rows = rowsOf(await inFlowTiles(page));
      expect(rows.map((r) => r.length)).toEqual([2, 1]);
      expectClose(rows[1][0].left, 588.5, "lone row 2 tile left in the band");
      expectClose(rowCentre(rows[0]), 800, "row 1 centre in the band");
      expectClose(rowCentre(rows[1]), 800, "row 2 centre in the band");
    } finally {
      await browser.close();
    }
  });

  // Hiding the self view drops `self_cell`, so three tiles become two: tw 612,
  // lefts 20 and 648. On fe2b470d the hidden host stayed out of flow but
  // inflated to the 612px tile width, overlapping tile one at its static spot.
  test("a hidden self view leaves the flex line entirely", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newGridSelfPage(browser, uiURL, "CentreHidden");
      await joinSoloMeeting(page, `e2e_centre_hidden_${Date.now()}`, "CentreHidden");
      await addMockPeers(page, 2);
      await expectTileCount(page, 3);

      const nav = page.locator("#host-controls-nav");
      await expect(nav).toHaveAttribute("data-self-hidden", "false");
      await nav.hover();
      await nav.locator('.host-tile-chrome [data-testid="self-view-hide-button"]').click();
      await expect(nav).toHaveAttribute("data-self-hidden", "true", { timeout: 10_000 });

      await expect
        .poll(async () => Math.round((await nav.boundingBox())?.width ?? 0), { timeout: 10_000 })
        .toBe(0);

      const tiles = await inFlowTiles(page);
      expect(tiles.length, "only the two mock tiles remain in flow").toBe(2);
      const rows = rowsOf(tiles);
      expect(rows.map((r) => r.length)).toEqual([2]);
      expectClose(rows[0][0].left, 20, "first tile left");
      expectClose(rows[0][0].width, 612, "tile width with zero slack");
      expectClose(rows[0][1].left, 648, "second tile left");
      expectClose(rowCentre(rows[0]), 640, "row centre");
    } finally {
      await browser.close();
    }
  });

  // The zero-slack bookend: two 612px tiles plus the gap consume the whole
  // 1240px band, so centring has nothing to distribute and the first tile stays
  // hard against the padding. Passes before AND after the change by design —
  // it guards the no-op, it does not demonstrate the fix.
  test("two tiles fill the band, so centring moves nothing", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newGridSelfPage(browser, uiURL, "Centre2");
      await joinSoloMeeting(page, `e2e_centre_n2_${Date.now()}`, "Centre2");
      await addMockPeers(page, 1);
      await expectTileCount(page, 2);

      const rows = rowsOf(await inFlowTiles(page));
      expect(
        rows.map((r) => r.length),
        "two tiles share one row",
      ).toEqual([2]);
      expectClose(rows[0][0].left, 20, "first tile sits on the padding");
      expectClose(rows[0][0].width, 612, "tile width with zero slack");
      expectClose(rows[0][1].left, 648, "second tile left");
      expectClose(rowCentre(rows[0]), 640, "row centre");
    } finally {
      await browser.close();
    }
  });
});
