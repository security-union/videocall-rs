import { test, expect, chromium, Browser, BrowserContext, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { openPeerList } from "../helpers/controls";
import {
  MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT,
  OWN_SHARE_VIEW_KEY,
  RECEIVED_SHARE_VIEW_KEY,
  admitGuestIfNeeded,
  clickJoinAndEnterGrid,
  joinMeetingAs,
  seedShareViewMode,
  startScreenShare,
  stopScreenShare,
} from "../helpers/screen-share-meeting";

/**
 * Issue 2792: a shared-content tile's bar offers Enlarge (the split), Pin
 * (front of the grid, issue 2866) and Detach (separate window), and the view
 * chosen for one share is the view the next share opens in.
 *
 * The HOST views a mocked share published by the GUEST. The DOM contract lives
 * in canvas_generator.rs (`ShareTileRoot`, `ScreenShareZoomControls`,
 * `ShareDetachCta`, `OwnShareTile`) and the share block of attendants.rs.
 */

const DEFAULT_UI_URL = "http://localhost:3001";
const GRID = "#grid-container";
const CAMERA_TILE = `${GRID} [id^="peer-video-"][id$="-div"]`;
const ANNOUNCER = '[data-testid="ss-detach-announce"]';
const WINDOW_TOAST = '.peer-toasts [data-testid="ss-window-toast"]';
const SHARER_NAME = "VmSharer";
const SIZE_TOLERANCE_PX = 4;
const ENLARGED_DEFAULT_MIN_WIDTH_PX = 720;
const PHONE = { width: 390, height: 844 };
const LAPTOP = { width: 1024, height: 768 };
const TABLET_PORTRAIT = { width: 768, height: 1024 };
const PHONE_LANDSCAPE = { width: 844, height: 390 };

interface Box {
  x: number;
  y: number;
  width: number;
  height: number;
}

interface ShareMeeting {
  viewer: Page;
  sharer: Page;
  close: () => Promise<void>;
  addSharer: (name: string, prepare?: (ctx: BrowserContext) => Promise<void>) => Promise<Page>;
}

const receivedTile = (page: Page): Locator => page.locator('[data-testid="received-share-tile"]');
const ownTile = (page: Page): Locator => page.locator('[data-testid="own-share-tile"]');
const cameraTile = (page: Page): Locator => page.locator(CAMERA_TILE).first();
const control = (tile: Locator, testId: string): Locator =>
  tile.locator(`[data-testid="${testId}"]`);
const cameraPin = (page: Page): Locator =>
  cameraTile(page).locator('[data-testid="tile-pin-button"]');
const reattachButton = (popup: Page): Locator =>
  popup.getByRole("button", { name: "Reattach", exact: true });

const stored = (page: Page, key: string): Promise<string | null> =>
  page.evaluate((k) => window.localStorage.getItem(k), key);

const tileView = (ctx: BrowserContext): Promise<void> => seedShareViewMode(ctx, "tile");

async function gridWidth(page: Page): Promise<number> {
  const box = await page.locator(GRID).boundingBox();
  return box ? box.width : Number.NaN;
}

async function openShareMeeting(
  baseURL: string | undefined,
  label: string,
  prepareViewer?: (ctx: BrowserContext) => Promise<void>,
): Promise<ShareMeeting> {
  const uiURL = baseURL || DEFAULT_UI_URL;
  const browsers: Browser[] = [];
  const close = async (): Promise<void> => {
    await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
  };
  try {
    browsers.push(await chromium.launch({ args: BROWSER_ARGS }));
    browsers.push(await chromium.launch({ args: BROWSER_ARGS }));
    const viewerCtx = await createAuthenticatedContext(
      browsers[0],
      `vm-${label}-viewer@videocall.rs`,
      "VmViewer",
      uiURL,
    );
    const sharerCtx = await createAuthenticatedContext(
      browsers[1],
      `vm-${label}-sharer@videocall.rs`,
      SHARER_NAME,
      uiURL,
    );
    await sharerCtx.addInitScript(MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT);
    if (prepareViewer) {
      await prepareViewer(viewerCtx);
    }

    const meetingId = `e2e_share_view_${label}_${Date.now()}`;
    const viewer = await joinMeetingAs(viewerCtx, meetingId, "VmViewer");
    await clickJoinAndEnterGrid(viewer);
    const sharer = await joinMeetingAs(sharerCtx, meetingId, SHARER_NAME);
    await admitGuestIfNeeded(viewer, sharer);
    await expect(cameraTile(viewer)).toBeVisible({ timeout: 30_000 });
    const addSharer: ShareMeeting["addSharer"] = async (name, prepare) => {
      const browser = await chromium.launch({ args: BROWSER_ARGS });
      browsers.push(browser);
      const ctx = await createAuthenticatedContext(
        browser,
        `vm-${label}-${name.toLowerCase()}@videocall.rs`,
        name,
        uiURL,
      );
      await ctx.addInitScript(MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT);
      await prepare?.(ctx);
      const page = await joinMeetingAs(ctx, meetingId, name);
      await admitGuestIfNeeded(viewer, page);
      return page;
    };
    return { viewer, sharer, close, addSharer };
  } catch (err) {
    await close();
    throw err;
  }
}

async function share(m: ShareMeeting): Promise<Locator> {
  const shared = await startScreenShare(m.sharer, m.viewer);
  expect(shared, "the viewer never received the mocked screen share").toBe(true);
  const tile = receivedTile(m.viewer);
  await expect(tile).toBeVisible({ timeout: 10_000 });
  return tile;
}

async function stopShare(m: ShareMeeting): Promise<void> {
  await stopScreenShare(m.sharer);
  await expect(receivedTile(m.viewer)).toHaveCount(0, { timeout: 20_000 });
  await expect(ownTile(m.sharer)).toHaveCount(0, { timeout: 10_000 });
}

// The bar is revealed by hover / focus-within (opacity + pointer-events).
async function press(tile: Locator, testId: string): Promise<void> {
  const button = control(tile, testId);
  await expect(button).toHaveCount(1, { timeout: 10_000 });
  await tile.hover({ timeout: 10_000 });
  await button.click({ timeout: 10_000 });
}

// Resolves to null when no window opened within 12 s.
async function openDetached(viewer: Page, tile: Locator, trigger: Locator): Promise<Page | null> {
  const popup = viewer
    .context()
    .waitForEvent("page", { timeout: 12_000 })
    .catch(() => null);
  await tile.hover({ timeout: 10_000 });
  await trigger.click({ timeout: 10_000 });
  return popup;
}

// "<share origin of the focused element, or outside>:<its testid or id>".
async function focusedControl(page: Page): Promise<string> {
  return page.evaluate(() => {
    const el = document.activeElement;
    if (!el) {
      return "none";
    }
    const tile = el.closest("[data-share-origin]");
    const origin = tile ? tile.getAttribute("data-share-origin") : "outside";
    return `${origin}:${el.getAttribute("data-testid") ?? el.id}`;
  });
}

async function expectFocus(page: Page, expected: string): Promise<void> {
  await expect.poll(() => focusedControl(page), { timeout: 5_000 }).toBe(expected);
}

// Grid reading order: `a` sits left of `b` in one row, or in an earlier row.
function precedes(a: Box, b: Box): boolean {
  return Math.abs(a.y - b.y) <= SIZE_TOLERANCE_PX ? a.x < b.x : a.y < b.y;
}

async function expectPrecedes(first: Locator, second: Locator, what: string): Promise<void> {
  await expect(first).toBeVisible({ timeout: 10_000 });
  await expect(second).toBeVisible({ timeout: 10_000 });
  await expect
    .poll(
      async () => {
        const a = await first.boundingBox();
        const b = await second.boundingBox();
        return a !== null && b !== null && precedes(a, b);
      },
      { timeout: 10_000, message: what },
    )
    .toBe(true);
}

// Samples for `ms`, failing on the first read that is not `want`.
async function expectHeld(
  page: Page,
  read: () => Promise<string | null>,
  want: string,
  ms: number,
  what: string,
): Promise<void> {
  const until = Date.now() + ms;
  while (Date.now() < until) {
    expect(await read(), what).toBe(want);
    await page.waitForTimeout(100);
  }
}

function maxDelta(a: Box, b: Box): number {
  return Math.max(
    Math.abs(a.x - b.x),
    Math.abs(a.y - b.y),
    Math.abs(a.width - b.width),
    Math.abs(a.height - b.height),
  );
}

// Two reads 400ms apart must agree, so a box is never taken mid-reflow.
async function settledBox(locator: Locator, what: string): Promise<Box> {
  await expect(locator, `${what} must be visible before it is measured`).toBeVisible({
    timeout: 10_000,
  });
  const deadline = Date.now() + 15_000;
  let previous = await locator.boundingBox();
  while (Date.now() < deadline) {
    await locator.page().waitForTimeout(400);
    const current = await locator.boundingBox();
    if (previous && current && maxDelta(previous, current) <= 1) {
      return current;
    }
    previous = current;
  }
  throw new Error(`${what} never settled`);
}

// Frames the separate window's mirror has presented; null until it plays.
function mirrorFrames(popup: Page): Promise<number | null> {
  return popup.evaluate(() => {
    const v = document.getElementById("ss-detached-video") as HTMLVideoElement | null;
    if (!v || v.paused || v.readyState < 2 || v.videoWidth === 0) {
      return null;
    }
    return v.getVideoPlaybackQuality().totalVideoFrames;
  });
}

async function expectWindowWaiting(popup: Page): Promise<void> {
  await expect(popup.locator("body")).toHaveAttribute("data-ss-state", "waiting", {
    timeout: 10_000,
  });
  await expect(
    popup
      .locator("#ss-detached-waiting")
      .getByRole("heading", { name: "Shared content will be displayed here" }),
  ).toBeVisible();
  for (const id of ["ss-detached-zoom-in", "ss-detached-maximize", "ss-detached-viewport"]) {
    await expect(popup.locator(`#${id}`)).toHaveCount(1);
    await expect(popup.locator(`#${id}`)).toBeHidden();
  }
  await expect(popup.locator(".ss-detached-name")).toHaveText("Waiting for shared content");
  await expect(popup).toHaveTitle("Waiting for shared content");
  const reattach = reattachButton(popup);
  await expect(reattach).toBeVisible();
  await expect(reattach).toHaveAttribute("aria-keyshortcuts", "Escape");
  await expect(reattach).toHaveAccessibleDescription(
    "New shared content appears here automatically. To view it in the meeting window instead, select Reattach.",
  );
}

async function expectWindowLive(popup: Page, name: string): Promise<void> {
  await expect(popup.locator("body")).toHaveAttribute("data-ss-state", "live", {
    timeout: 15_000,
  });
  await expect(popup.locator("#ss-detached-waiting")).toHaveCount(1);
  await expect(popup.locator("#ss-detached-waiting")).toBeHidden();
  await expect(popup.locator("#ss-detached-zoom-in")).toBeVisible();
  await expect(popup).toHaveTitle("Shared content");
  await expect(reattachButton(popup)).toHaveAccessibleDescription(
    "Return shared content to the meeting window",
  );
  await expectWindowPlays(popup, name);
}

async function expectWindowPlays(popup: Page, name: string): Promise<void> {
  await expect(popup.locator(".ss-detached-name")).toHaveText(name, { timeout: 15_000 });
  await expect(popup.locator("#ss-detached-video")).toBeVisible();
  await expect
    .poll(() => mirrorFrames(popup), {
      timeout: 15_000,
      message: `the window never played ${name}'s share`,
    })
    .not.toBeNull();
  const first = await mirrorFrames(popup);
  expect(first, "the window's mirror stopped playing").not.toBeNull();
  await expect
    .poll(async () => (await mirrorFrames(popup)) ?? -1, {
      timeout: 10_000,
      message: "the window's mirror is frozen",
    })
    .toBeGreaterThan(first!);
}

test.describe("Issue 2792: shared-content view modes", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a received share with the Tile view stored opens as a camera-sized grid tile and the sharer sees its own tile @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const m = await openShareMeeting(baseURL, "tile", async (ctx) => {
      await tileView(ctx);
      await ctx.addInitScript(() => {
        try {
          window.localStorage.setItem("vc_self_view_placement", "grid");
        } catch {
          /* no storage on this document */
        }
      });
    });
    try {
      const grid = m.viewer.locator(GRID);
      const selfInGrid = m.viewer.locator(`${GRID} > .host[data-self-placement="grid"]`);
      await expect(grid).toHaveAttribute("data-share-layout", "none", { timeout: 10_000 });
      await expect(selfInGrid, "precondition: the seeded Grid self view").toBeVisible({
        timeout: 10_000,
      });

      const tile = await share(m);
      await expect(tile).toHaveAttribute("data-share-origin", "received");
      await expect(tile).toHaveAttribute("data-share-mode", "tile");
      await expect(control(tile, "ss-enlarge")).toBeVisible({ timeout: 10_000 });
      await expect(tile.locator(".crop-icon"), "shared content has no Crop").toHaveCount(0);
      await expect(grid).toHaveAttribute("data-share-layout", "tile");
      await expect(grid).not.toHaveClass(/\bhas-screen-share\b/);
      await expect(m.viewer.locator(".screen-share-resize-handle")).toBeHidden({ timeout: 5_000 });
      await expect(selfInGrid, "a Tile share keeps a Grid self view in the grid").toBeVisible();
      for (const id of ["ss-enlarge", "ss-pin", "ss-detach"]) {
        await expect(control(tile, id)).toHaveAttribute("aria-pressed", "false");
      }

      const cam = cameraTile(m.viewer);
      await expect
        .poll(
          async () => {
            const s = await tile.boundingBox();
            const c = await cam.boundingBox();
            return s && c
              ? Math.max(Math.abs(s.width - c.width), Math.abs(s.height - c.height))
              : Number.POSITIVE_INFINITY;
          },
          { timeout: 15_000, message: "the share tile must be the size of a camera tile" },
        )
        .toBeLessThanOrEqual(SIZE_TOLERANCE_PX);
      const shareBox = await settledBox(tile, "share tile");
      const camBox = await settledBox(cam, "camera tile");
      const sameRow = Math.abs(shareBox.y - camBox.y) <= SIZE_TOLERANCE_PX;
      expect(
        sameRow ? shareBox.x < camBox.x : shareBox.y < camBox.y,
        "the share tile takes the first grid cell",
      ).toBe(true);

      const own = ownTile(m.sharer);
      await expect(own).toBeVisible({ timeout: 15_000 });
      await expect(own).toHaveAttribute("data-share-origin", "own");
      await expect(own).toHaveAttribute("data-share-mode", "tile");
      await expect(own.locator("h4.floating-name")).toHaveText("You are presenting");
      for (const id of ["ss-enlarge", "ss-pin", "ss-detach", "ss-hide-preview"]) {
        await expect(control(own, id)).toHaveCount(1);
      }
      await expect(control(own, "ss-zoom-actual")).toHaveCount(0);
      const preview = own.locator("video#own-screen-share-video");
      await expect(preview).toHaveCount(1);
      await expect
        .poll(() => preview.evaluate((v) => (v as HTMLVideoElement).srcObject !== null), {
          timeout: 10_000,
        })
        .toBe(true);
      await expect(m.sharer.locator("#screen-share-preview")).toHaveCount(0);
      await expect(m.sharer.locator('[data-share-origin="received"]')).toHaveCount(0);
      await expect(m.sharer.locator(GRID)).toHaveAttribute("data-share-layout", "tile");
    } finally {
      await m.close();
    }
  });

  test("with no view stored a received share opens Enlarged without storing it, and the sharer's own share opens as a grid tile @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const m = await openShareMeeting(baseURL, "default");
    try {
      const grid = m.viewer.locator(GRID);
      expect(
        await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY),
        "precondition: no stored view",
      ).toBeNull();
      await expect(grid).toBeVisible();
      expect(await gridWidth(m.viewer), "precondition: a wide meeting area").toBeGreaterThanOrEqual(
        ENLARGED_DEFAULT_MIN_WIDTH_PX,
      );

      const tile = await share(m);
      await expect(tile).toHaveAttribute("data-share-mode", "enlarged", { timeout: 10_000 });
      await expect(grid).toHaveAttribute("data-share-layout", "enlarged");
      await expect(grid).toHaveClass(/\bhas-screen-share\b/);
      await expect(
        m.viewer.locator(`${GRID} > .ss-left-pane > [data-testid="received-share-tile"]`),
      ).toBeVisible();
      await expect(m.viewer.locator(`${GRID} > .ss-left-pane`)).toHaveCSS("display", "flex");
      await expect(m.viewer.locator(`${GRID} > .screen-share-resize-handle`)).toBeVisible();
      const sideCam = m.viewer
        .locator(`${GRID} > .ss-peer-panel [id^="peer-video-"][id$="-div"]`)
        .first();
      await expect(sideCam).toBeVisible({ timeout: 10_000 });
      await expect(sideCam).toHaveClass(/\bsplit-peer-tile\b/);
      await expect(control(tile, "ss-enlarge")).toHaveAttribute("aria-pressed", "true");
      await expect(control(tile, "ss-enlarge")).toHaveAttribute("title", "Return to grid");
      const stage = await settledBox(tile, "enlarged share tile");
      const side = await settledBox(sideCam, "camera tile in the side panel");
      expect(stage.width, "the enlarged share is the stage").toBeGreaterThan(side.width * 2);
      expect(
        await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY),
        "the default view is not stored",
      ).toBeNull();

      const own = ownTile(m.sharer);
      await expect(own).toBeVisible({ timeout: 15_000 });
      await expect(own).toHaveAttribute("data-share-mode", "tile");
      await expect(m.sharer.locator(GRID)).toHaveAttribute("data-share-layout", "tile");
      await expect(m.sharer.locator(GRID)).not.toHaveClass(/\bhas-screen-share\b/);
      expect(await stored(m.sharer, OWN_SHARE_VIEW_KEY)).toBeNull();

      await press(tile, "ss-enlarge");
      await expect(tile).toHaveAttribute("data-share-mode", "tile", { timeout: 10_000 });
      await expect(grid).toHaveAttribute("data-share-layout", "tile");
      expect(await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY), "the first choice is stored").toBe(
        "tile",
      );
    } finally {
      await m.close();
    }
  });

  test("with no view stored a received share opens as a grid tile while the meeting area is under 720px wide, portrait or short", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const m = await openShareMeeting(baseURL, "narrow");
    try {
      const grid = m.viewer.locator(GRID);
      const peerList = m.viewer.locator("#peer-list-container");
      const expectMeetingArea = async (wide: boolean, what: string): Promise<void> => {
        await expect(grid).toBeVisible();
        const poll = expect.poll(() => gridWidth(m.viewer), { timeout: 10_000, message: what });
        await (wide
          ? poll.toBeGreaterThanOrEqual(ENLARGED_DEFAULT_MIN_WIDTH_PX)
          : poll.toBeLessThan(ENLARGED_DEFAULT_MIN_WIDTH_PX));
      };
      const shareArrivesAs = async (mode: "tile" | "enlarged", what: string): Promise<void> => {
        const tile = await share(m);
        await expect(tile, what).toHaveAttribute("data-share-mode", mode, { timeout: 10_000 });
        await expect(grid).toHaveAttribute("data-share-layout", mode);
        expect(
          await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY),
          "the default view is not stored",
        ).toBeNull();
      };

      await m.viewer.setViewportSize(PHONE);
      await expectMeetingArea(false, "precondition: a phone-wide meeting area");
      await shareArrivesAs("tile", "a phone opens the share as a tile");
      await expect(grid).not.toHaveClass(/\bhas-screen-share\b/);
      await expect(control(receivedTile(m.viewer), "ss-enlarge")).toHaveAttribute(
        "aria-pressed",
        "false",
      );
      await stopShare(m);

      await m.viewer.setViewportSize(TABLET_PORTRAIT);
      await expectMeetingArea(true, "precondition: a portrait area at least 720px wide");
      await shareArrivesAs("tile", "a portrait tablet opens the share as a tile");
      await stopShare(m);

      await m.viewer.setViewportSize(PHONE_LANDSCAPE);
      await expectMeetingArea(true, "precondition: a landscape phone at least 720px wide");
      await shareArrivesAs("tile", "a landscape phone opens the share as a tile");
      await stopShare(m);

      await m.viewer.setViewportSize(LAPTOP);
      await openPeerList(m.viewer);
      await expectMeetingArea(false, "precondition: the peer list narrows a 1024px meeting area");
      await shareArrivesAs("tile", "an open peer list leaves too little room to enlarge");
      await stopShare(m);

      await peerList.locator("button.close-button").click({ timeout: 10_000 });
      await expect(peerList).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expectMeetingArea(true, "precondition: the same window with the peer list closed");
      await shareArrivesAs("enlarged", "the same window without the drawer enlarges");
    } finally {
      await m.close();
    }
  });

  test("a share arriving Enlarged closes an open density menu and moves focus from the menu or its trigger to the grid", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const m = await openShareMeeting(baseURL, "density");
    try {
      const grid = m.viewer.locator(GRID);
      const menu = m.viewer.locator(".density-popover");
      const trigger = m.viewer.locator("#density-mode-trigger");
      await m.viewer.locator(".video-controls-container").hover({ timeout: 10_000 });
      await trigger.click({ timeout: 10_000 });
      await expect(menu).toBeVisible({ timeout: 5_000 });
      await expect
        .poll(() => focusedControl(m.viewer), { timeout: 5_000 })
        .toMatch(/^outside:density-opt-/);

      await share(m);
      await expect(grid).toHaveAttribute("data-share-layout", "enlarged", { timeout: 10_000 });
      await expect(menu).toHaveCount(0);
      await expectFocus(m.viewer, "outside:grid-container");

      await stopShare(m);
      await expect(grid).toHaveAttribute("data-share-layout", "none", { timeout: 10_000 });
      await expect(trigger).toHaveAttribute("aria-expanded", "false");
      await expect(menu, "the menu must not reopen when the split ends").toHaveCount(0);

      await m.viewer.locator(".video-controls-container").hover({ timeout: 10_000 });
      await trigger.focus();
      await expectFocus(m.viewer, "outside:density-mode-trigger");
      await share(m);
      await expectFocus(m.viewer, "outside:grid-container");
    } finally {
      await m.close();
    }
  });

  test("Enlarge opens the split and returns the share to the grid, focus staying on Enlarge @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const m = await openShareMeeting(baseURL, "enlarge", tileView);
    try {
      const grid = m.viewer.locator(GRID);
      const tile = await share(m);
      const enlarge = control(tile, "ss-enlarge");
      await expect(tile).toHaveAttribute("data-share-mode", "tile");

      await press(tile, "ss-enlarge");
      await expect(tile).toHaveAttribute("data-share-mode", "enlarged", { timeout: 10_000 });
      await expect(grid).toHaveAttribute("data-share-layout", "enlarged");
      await expect(grid).toHaveClass(/\bhas-screen-share\b/);
      await expect(enlarge).toHaveAttribute("aria-pressed", "true");
      await expect(enlarge).toHaveAttribute("title", "Return to grid");
      await expectFocus(m.viewer, "received:ss-enlarge");
      expect(await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY)).toBe("enlarged");
      const stage = await settledBox(tile, "enlarged share tile");
      const side = await settledBox(cameraTile(m.viewer), "camera tile in the side panel");
      expect(stage.width, "the enlarged share is the stage").toBeGreaterThan(side.width * 2);

      await press(tile, "ss-enlarge");
      await expect(tile).toHaveAttribute("data-share-mode", "tile", { timeout: 10_000 });
      await expect(grid).toHaveAttribute("data-share-layout", "tile");
      await expect(grid).not.toHaveClass(/\bhas-screen-share\b/);
      await expect(enlarge).toHaveAttribute("aria-pressed", "false");
      await expect(enlarge).toHaveAttribute("title", "Enlarge");
      await expectFocus(m.viewer, "received:ss-enlarge");
      expect(await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY)).toBe("tile");
    } finally {
      await m.close();
    }
  });

  test("Pin keeps the share a camera-sized grid tile, from Tile and from Enlarged, and Escape leaves it pinned @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const m = await openShareMeeting(baseURL, "pin", tileView);
    try {
      const grid = m.viewer.locator(GRID);
      const announce = m.viewer.locator(ANNOUNCER);
      const tile = await share(m);
      const pin = control(tile, "ss-pin");
      const badge = tile.locator('[data-testid="tile-pin-badge"]');
      const cam = cameraTile(m.viewer);
      await expect(badge).toHaveCount(0);

      const expectPinnedInGrid = async (): Promise<void> => {
        await expect(tile).toHaveAttribute("data-share-mode", "pinned", { timeout: 10_000 });
        await expect(tile).toHaveClass(/\btile-pinned\b/);
        await expect(tile).toHaveAttribute("data-pinned", "true");
        await expect(tile).toHaveAttribute("data-pin-rank", "0");
        await expect(badge).toBeVisible();
        await expect(pin).toHaveAttribute("aria-pressed", "true");
        await expect(announce).toHaveText("Shared content pinned", { timeout: 10_000 });
        await expectFocus(m.viewer, "received:ss-pin");
        expect(await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY)).toBe("pinned");
        await expect(grid).toHaveAttribute("data-share-layout", "tile");
        await expect(grid).not.toHaveClass(/\bhas-screen-share\b/);
        expect(await tile.evaluate((el) => getComputedStyle(el).position)).not.toBe("fixed");
        const shareBox = await settledBox(tile, "pinned share tile");
        const camBox = await settledBox(cam, "camera tile");
        expect(
          Math.max(
            Math.abs(shareBox.width - camBox.width),
            Math.abs(shareBox.height - camBox.height),
          ),
          "a pinned share is the size of a camera tile",
        ).toBeLessThanOrEqual(SIZE_TOLERANCE_PX);
        expect(precedes(shareBox, camBox), "the pinned share takes the first cell").toBe(true);
      };

      await press(tile, "ss-pin");
      await expectPinnedInGrid();

      await control(tile, "ss-zoom-viewport").focus({ timeout: 5_000 });
      await expectFocus(m.viewer, "received:ss-zoom-viewport");
      await m.viewer.keyboard.press("Escape");
      await expectHeld(
        m.viewer,
        () => tile.getAttribute("data-share-mode"),
        "pinned",
        1_000,
        "Escape must leave the share pinned",
      );

      await press(tile, "ss-pin");
      await expect(tile).toHaveAttribute("data-share-mode", "tile", { timeout: 10_000 });
      await expect(tile).not.toHaveClass(/\btile-pinned\b/);
      await expect(tile).not.toHaveAttribute("data-pinned");
      await expect(badge).toHaveCount(0);
      await expect(pin).toHaveAttribute("aria-pressed", "false");
      await expect(announce).toHaveText("Shared content unpinned", { timeout: 10_000 });
      await expectFocus(m.viewer, "received:ss-pin");
      expect(await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY)).toBe("tile");

      await press(tile, "ss-enlarge");
      await expect(tile).toHaveAttribute("data-share-mode", "enlarged", { timeout: 10_000 });
      await expect(grid).toHaveClass(/\bhas-screen-share\b/);
      await press(tile, "ss-pin");
      await expectPinnedInGrid();
      await expect(control(tile, "ss-enlarge")).toHaveAttribute("aria-pressed", "false");
    } finally {
      await m.close();
    }
  });

  test("camera and share pins coexist, the most recent pin first", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const m = await openShareMeeting(baseURL, "camerapin", tileView);
    try {
      const tile = await share(m);
      const cam = cameraTile(m.viewer);
      const camPin = cameraPin(m.viewer);
      await expect(camPin).toHaveCount(1, { timeout: 10_000 });
      await expectPrecedes(tile, cam, "an unpinned share leads an unpinned camera tile");

      await camPin.click({ timeout: 10_000 });
      await expect(camPin).toHaveAttribute("aria-pressed", "true", { timeout: 10_000 });
      await expect(cam).toHaveAttribute("data-pin-rank", "0");
      await expect(tile).toHaveAttribute("data-share-mode", "tile");
      await expectPrecedes(cam, tile, "a pinned camera tile leads an unpinned share");

      await press(tile, "ss-pin");
      await expect(tile).toHaveAttribute("data-share-mode", "pinned", { timeout: 10_000 });
      await expect(tile).toHaveAttribute("data-pin-rank", "0");
      await expect(cam).toHaveAttribute("data-pin-rank", "1");
      await expect(camPin, "a share pin leaves the camera pin").toHaveAttribute(
        "aria-pressed",
        "true",
      );
      await expectPrecedes(tile, cam, "the more recent share pin leads");

      await camPin.click({ timeout: 10_000 });
      await expect(camPin).toHaveAttribute("aria-pressed", "false", { timeout: 10_000 });
      await expect(cam).not.toHaveAttribute("data-pinned");
      await expect(tile, "a camera unpin leaves the share pin").toHaveAttribute(
        "data-share-mode",
        "pinned",
      );
      await expect(tile).toHaveAttribute("data-pin-rank", "0");
      expect(
        await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY),
        "a camera pin is not a share-view choice",
      ).toBe("pinned");
    } finally {
      await m.close();
    }
  });

  test("a peer tile keeps its DOM node when the viewer shares and when a received share is enlarged", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const m = await openShareMeeting(baseURL, "nodes", async (ctx) => {
      await tileView(ctx);
      await ctx.addInitScript(MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT);
    });
    try {
      const grid = m.viewer.locator(GRID);
      const cam = cameraTile(m.viewer);
      const tileId = await cam.getAttribute("id", { timeout: 10_000 });
      expect(tileId).toMatch(/^peer-video-\d+-div$/);
      const held = await cam.elementHandle({ timeout: 10_000 });
      expect(held, "the camera tile must be mounted before it is held").not.toBeNull();
      const isHeldNode = (): Promise<boolean> =>
        m.viewer.evaluate(({ el, id }) => el.isConnected && document.getElementById(id) === el, {
          el: held!,
          id: tileId!,
        });
      await expect(grid).toHaveAttribute("data-share-layout", "none", { timeout: 10_000 });

      const ownShared = await startScreenShare(m.viewer, m.sharer);
      expect(ownShared, "the viewer's own share must reach the other side").toBe(true);
      await expect(ownTile(m.viewer)).toBeVisible({ timeout: 15_000 });
      await expect(grid).toHaveAttribute("data-share-layout", "tile");
      expect(await isHeldNode(), "own share start remounted the peer tile").toBe(true);

      await stopScreenShare(m.viewer);
      await expect(ownTile(m.viewer)).toHaveCount(0, { timeout: 15_000 });
      await expect(grid).toHaveAttribute("data-share-layout", "none", { timeout: 10_000 });
      expect(await isHeldNode(), "own share stop remounted the peer tile").toBe(true);

      const tile = await share(m);
      await expect(grid).toHaveAttribute("data-share-layout", "tile");
      expect(await isHeldNode(), "a received share remounted the peer tile").toBe(true);

      await press(tile, "ss-enlarge");
      await expect(tile).toHaveAttribute("data-share-mode", "enlarged", { timeout: 10_000 });
      await expect(cam).toHaveClass(/\bsplit-peer-tile\b/, { timeout: 10_000 });
      expect(await isHeldNode(), "Enlarge remounted the peer tile").toBe(true);

      await press(tile, "ss-enlarge");
      await expect(tile).toHaveAttribute("data-share-mode", "tile", { timeout: 10_000 });
      await expect(cam).toHaveClass(/\bgrid-item\b/, { timeout: 10_000 });
      expect(await isHeldNode(), "Exit from Enlarged remounted the peer tile").toBe(true);
    } finally {
      await m.close();
    }
  });

  test("the chosen view is the view of the next share, per origin", async ({ baseURL }) => {
    test.setTimeout(300_000);
    const m = await openShareMeeting(baseURL, "sticky", tileView);
    try {
      let tile = await share(m);
      const own = ownTile(m.sharer);
      await expect(tile).toHaveAttribute("data-share-mode", "tile");
      await press(tile, "ss-enlarge");
      await expect(tile).toHaveAttribute("data-share-mode", "enlarged", { timeout: 10_000 });
      await expect(own).toHaveAttribute("data-share-mode", "tile", { timeout: 15_000 });
      await press(own, "ss-enlarge");
      await expect(own).toHaveAttribute("data-share-mode", "enlarged", { timeout: 10_000 });
      await expect(m.sharer.locator(GRID)).toHaveClass(/\bhas-screen-share\b/);
      expect(await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY)).toBe("enlarged");
      expect(await stored(m.sharer, OWN_SHARE_VIEW_KEY)).toBe("enlarged");
      expect(
        await stored(m.sharer, RECEIVED_SHARE_VIEW_KEY),
        "the own share keeps its own key",
      ).toBeNull();

      await stopShare(m);
      tile = await share(m);
      await expect(tile).toHaveAttribute("data-share-mode", "enlarged", { timeout: 10_000 });
      await expect(m.viewer.locator(GRID)).toHaveClass(/\bhas-screen-share\b/);
      await expect(own).toHaveAttribute("data-share-mode", "enlarged", { timeout: 15_000 });

      await press(tile, "ss-pin");
      await expect(tile).toHaveAttribute("data-share-mode", "pinned", { timeout: 10_000 });
      expect(await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY)).toBe("pinned");
      await stopShare(m);

      // A share arriving with the Pinned view goes ahead of a held camera pin.
      const cam = cameraTile(m.viewer);
      await cameraPin(m.viewer).click({ timeout: 10_000 });
      await expect(cam).toHaveAttribute("data-pin-rank", "0", { timeout: 10_000 });

      tile = await share(m);
      await expect(tile).toHaveAttribute("data-share-mode", "pinned", { timeout: 10_000 });
      await expect(tile).toHaveClass(/\btile-pinned\b/);
      await expect(tile).toHaveAttribute("data-pin-rank", "0");
      await expect(m.viewer.locator(GRID)).toHaveAttribute("data-share-layout", "tile");
      await expect(cam).toHaveAttribute("data-pin-rank", "1", { timeout: 10_000 });
      await expect(cameraPin(m.viewer)).toHaveAttribute("aria-pressed", "true");
      await expectPrecedes(tile, cam, "the arriving pinned share leads the camera pin");
    } finally {
      await m.close();
    }
  });

  test("Detach returns peers to normal-grid size, reattach focuses Detach, and closing a window left waiting returns the next share to the pre-detach view", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const m = await openShareMeeting(baseURL, "detach", tileView);
    try {
      const grid = m.viewer.locator(GRID);
      const announce = m.viewer.locator(ANNOUNCER);
      const cam = cameraTile(m.viewer);
      const normal = await settledBox(cam, "camera tile before any share");

      const tile = await share(m);
      await expect(tile).toHaveAttribute("data-share-mode", "tile");
      const beside = await settledBox(cam, "camera tile beside the share tile");
      expect(beside.width, "a share cell shrinks the camera tile").toBeLessThan(normal.width - 50);

      const opened = await openDetached(m.viewer, tile, control(tile, "ss-detach"));
      expect(opened, "Detach must open a separate window").not.toBeNull();
      const popup = opened!;

      await expect(tile).toHaveAttribute("data-share-mode", "detached", { timeout: 10_000 });
      await expect(tile).toHaveAttribute("inert", "true");
      await expect(grid).toHaveAttribute("data-share-layout", "tile");
      await expect(grid).not.toHaveClass(/\bhas-screen-share\b/);
      const offscreen = await tile.boundingBox();
      expect(offscreen, "the detached tile stays laid out, off-screen").not.toBeNull();
      expect(offscreen!.x + offscreen!.width).toBeLessThanOrEqual(0);
      await expect(announce).toHaveText("Shared content opened in a separate window", {
        timeout: 10_000,
      });
      await expectFocus(m.viewer, "outside:grid-container");
      await expect
        .poll(() => stored(m.viewer, RECEIVED_SHARE_VIEW_KEY), { timeout: 5_000 })
        .toBe("detached");
      const detachedCam = await settledBox(cam, "camera tile while the share is detached");
      expect(Math.abs(detachedCam.width - normal.width)).toBeLessThanOrEqual(SIZE_TOLERANCE_PX);
      expect(Math.abs(detachedCam.height - normal.height)).toBeLessThanOrEqual(SIZE_TOLERANCE_PX);
      expect(detachedCam.width).toBeGreaterThan(beside.width + 50);

      const closed = popup.waitForEvent("close", { timeout: 10_000 });
      await popup.locator("#ss-detached-reattach").click({ timeout: 10_000 });
      await closed;
      await expect(tile).toHaveAttribute("data-share-mode", "tile", { timeout: 10_000 });
      await expect(tile).not.toHaveAttribute("inert");
      await expect(announce).toHaveText("Shared content returned to the meeting", {
        timeout: 10_000,
      });
      await expectFocus(m.viewer, "received:ss-detach");
      expect(await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY)).toBe("tile");

      const second = await openDetached(m.viewer, tile, control(tile, "ss-detach"));
      expect(second, "a window that opened once must open again").not.toBeNull();
      await expect(tile).toHaveAttribute("data-share-mode", "detached", { timeout: 10_000 });
      await expect
        .poll(() => stored(m.viewer, RECEIVED_SHARE_VIEW_KEY), { timeout: 5_000 })
        .toBe("detached");
      await stopShare(m);
      await expectWindowWaiting(second!);
      await cameraPin(m.viewer).focus();
      await expectFocus(m.viewer, "outside:tile-pin-button");
      const secondClosed = second!.waitForEvent("close", { timeout: 10_000 });
      expect(second!.isClosed(), "the waiting window closed before Escape").toBe(false);
      // The window closes on keydown, so the keyup of press() finds the page gone.
      await second!.keyboard.press("Escape").catch((e) => {
        if (!second!.isClosed()) throw e;
      });
      await secondClosed;
      await expect(announce).toHaveText("Separate window closed", { timeout: 10_000 });
      await expectFocus(m.viewer, "outside:grid-container");
      expect(await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY)).toBe("tile");

      const next = await share(m);
      await expect(next).toHaveAttribute("data-share-mode", "tile");
      await expect(control(next, "ss-detach-cta")).toHaveCount(0);
      await expect(announce).toHaveText(`${SHARER_NAME} started sharing`, { timeout: 5_000 });
    } finally {
      await m.close();
    }
  });

  test("a stored Detached view offers the window on the tile, folds into a suggested Detach, and opens the window from the next offer", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const m = await openShareMeeting(baseURL, "cta", (ctx) => seedShareViewMode(ctx, "detached"));
    try {
      const tile = await share(m);
      const cta = control(tile, "ss-detach-cta");
      const detach = control(tile, "ss-detach");
      await expect(tile).toHaveAttribute("data-share-mode", "enlarged");
      await expect(cta).toBeVisible({ timeout: 5_000 });
      await expect(cta).toHaveAttribute("aria-label", "Open in separate window");
      await expect(detach).not.toHaveAttribute("data-suggested", "true");
      await expect(m.viewer.locator(ANNOUNCER)).toHaveText(
        new RegExp(`${SHARER_NAME} started sharing\\. You can open it in a separate window\\.`),
        { timeout: 5_000 },
      );

      // CTA_TIMEOUT_MS is 12 s while the button does not hold focus.
      await expect(cta).toHaveCount(0, { timeout: 20_000 });
      await expect(detach).toHaveAttribute("data-suggested", "true");
      await expect(tile).toHaveAttribute("data-share-mode", "enlarged");
      expect(await stored(m.viewer, RECEIVED_SHARE_VIEW_KEY)).toBe("detached");

      await stopShare(m);
      const next = await share(m);
      const opened = await openDetached(m.viewer, next, control(next, "ss-detach-cta"));
      expect(opened, "the one-click route must open the window").not.toBeNull();
      await expect(next).toHaveAttribute("data-share-mode", "detached", { timeout: 10_000 });
    } finally {
      await m.close();
    }
  });
});

test.describe("Issue 3173: the received separate window waits between shares", () => {
  const SECOND_SHARER = "VmSharerTwo";
  const HOLDER = "VmHolder";

  const openWithWindow = (baseURL: string | undefined, label: string): Promise<ShareMeeting> =>
    openShareMeeting(baseURL, label, tileView);

  async function detachShare(m: ShareMeeting, holder = m.viewer): Promise<Page> {
    await share(m);
    const tile = receivedTile(holder);
    await expect(tile).toHaveAttribute("data-share-mode", "tile", { timeout: 10_000 });
    const opened = await openDetached(holder, tile, control(tile, "ss-detach"));
    expect(opened, "Detach must open a separate window").not.toBeNull();
    await expect(tile).toHaveAttribute("data-share-mode", "detached", { timeout: 10_000 });
    await expectWindowPlays(opened!, SHARER_NAME);
    return opened!;
  }

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("the window waits when the sharer stops, speaks only in the focused document, plays the next share, and closes on Reattach and when the meeting view is left", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const m = await openWithWindow(baseURL, "waits");
    try {
      const grid = m.viewer.locator(GRID);
      const announce = m.viewer.locator(ANNOUNCER);
      const toast = m.viewer.locator(WINDOW_TOAST);
      const popup = await detachShare(m);
      const note = popup.locator("#ss-detached-announce");
      const pages = m.viewer.context().pages().length;

      await stopShare(m);
      await expectWindowWaiting(popup);
      await expect(note).toHaveText(
        `${SHARER_NAME} stopped sharing. Shared content will be displayed here.`,
      );
      await expect(grid).toHaveAttribute("data-share-layout", "none");
      // Playwright reports every page focused, so the window holds focus here.
      await expectHeld(
        m.viewer,
        async () => (await announce.textContent())?.trim() ?? null,
        "Shared content opened in a separate window",
        2_000,
        "the meeting window must stay silent while the window has focus",
      );

      await popup.evaluate(() => {
        document.hasFocus = () => false;
      });
      const reshared = await startScreenShare(m.sharer, m.viewer);
      expect(reshared, "the viewer never received the next share").toBe(true);
      await expect(toast).toHaveText(`${SHARER_NAME} is sharing in the separate window`);
      const tile = receivedTile(m.viewer);
      await expect(tile).toHaveAttribute("data-share-mode", "detached", { timeout: 10_000 });
      await expect(tile).toHaveAttribute("inert", "true");
      await expectWindowLive(popup, SHARER_NAME);
      await expect(announce).toHaveText(
        `${SHARER_NAME} started sharing. Showing it in the separate window.`,
        { timeout: 10_000 },
      );
      await expect(note).toBeEmpty();
      expect(m.viewer.context().pages(), "the next share reuses the open window").toHaveLength(
        pages,
      );

      await stopShare(m);
      await expectWindowWaiting(popup);
      const closed = popup.waitForEvent("close", { timeout: 10_000 });
      await reattachButton(popup).click({ timeout: 10_000 });
      await closed;
      await expect(announce).toHaveText("Separate window closed", { timeout: 10_000 });
      await expect(grid).toHaveAttribute("data-share-layout", "none");

      const next = await share(m);
      await expect(next).toHaveAttribute("data-share-mode", "tile");
      await expect(next).not.toHaveAttribute("inert");
      expect(m.viewer.context().pages(), "a closed window stays closed").toHaveLength(pages - 1);
      const last = await openDetached(m.viewer, next, control(next, "ss-detach"));
      expect(last, "Detach must open again after a waiting window closed").not.toBeNull();
      await m.viewer.goBack();
      await expect.poll(() => last!.isClosed(), { timeout: 10_000 }).toBe(true);
    } finally {
      await m.close();
    }
  });

  test("a window left waiting by a sharer who leaves plays another participant's share", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const m = await openWithWindow(baseURL, "handover");
    try {
      const second = await m.addSharer(SECOND_SHARER);
      await expect(m.viewer.locator(CAMERA_TILE)).toHaveCount(2, { timeout: 30_000 });
      const popup = await detachShare(m);
      const pages = m.viewer.context().pages().length;

      await m.sharer.close();
      await expect(receivedTile(m.viewer)).toHaveCount(0, { timeout: 30_000 });
      await expectWindowWaiting(popup);

      const shared = await startScreenShare(second, m.viewer);
      expect(shared, "the viewer never received the second participant's share").toBe(true);
      const toast = m.viewer.locator(WINDOW_TOAST);
      await expect(toast).toHaveText(`${SECOND_SHARER} is sharing in the separate window`);
      await expect(receivedTile(m.viewer)).toHaveAttribute("data-share-mode", "detached", {
        timeout: 10_000,
      });
      await expectWindowLive(popup, SECOND_SHARER);
      await expect(popup.locator("#ss-detached-announce")).toHaveText(
        `${SECOND_SHARER} started sharing`,
      );
      expect(m.viewer.context().pages(), "the second share reuses the open window").toHaveLength(
        pages,
      );
    } finally {
      await m.close();
    }
  });

  test("the window closes when its holder is removed from the meeting", async ({ baseURL }) => {
    test.setTimeout(300_000);
    const m = await openWithWindow(baseURL, "kicked");
    try {
      const holder = await m.addSharer(HOLDER, tileView);
      await expect(m.viewer.locator(CAMERA_TILE)).toHaveCount(2, { timeout: 30_000 });
      const popup = await detachShare(m, holder);
      const holderTile = m.viewer.locator(".grid-item:has(.tile-mute-btn)", { hasText: HOLDER });
      await holderTile.hover({ timeout: 15_000 });
      await holderTile.getByTitle("Host actions").click({ timeout: 10_000 });
      await holderTile
        .locator(".tile-context-menu-item", { hasText: "Remove from meeting" })
        .click({ timeout: 10_000 });
      await expect(holder.locator(".meeting-ended-overlay")).toBeVisible({ timeout: 20_000 });
      await expect.poll(() => popup.isClosed(), { timeout: 10_000 }).toBe(true);
    } finally {
      await m.close();
    }
  });

  test("a share that takes the window leaves the one it replaced as a tile with zoom and Swap, Swap trades places, and the window's share stopping brings the tile back", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const m = await openWithWindow(baseURL, "demote");
    try {
      const second = await m.addSharer(SECOND_SHARER);
      await expect(m.viewer.locator(CAMERA_TILE)).toHaveCount(2, { timeout: 30_000 });
      const popup = await detachShare(m);
      const demoted = m.viewer.locator(
        '[data-testid="received-share-tile"][data-share-role="secondary"]',
      );
      const expectDemoted = async (name: string): Promise<void> => {
        await expect(demoted).toHaveCount(1, { timeout: 15_000 });
        await expect(demoted).toHaveAttribute("aria-label", `Shared content from ${name}`);
        await expect(demoted).toHaveAttribute("data-share-mode", "tile");
        await expect(control(demoted, "ss-zoom-in")).toHaveCount(1);
        await expect(control(demoted, "ss-enlarge")).toHaveCount(0);
        await expect(control(demoted, "ss-pin")).toHaveCount(0);
        const swap = control(demoted, "ss-detach");
        await expect(swap).toHaveAttribute("data-swap", "true");
        await expect(swap).toHaveAttribute("title", "Swap with the separate window");
      };

      await startScreenShare(second, m.viewer);
      await expectDemoted(SHARER_NAME);
      await expectWindowPlays(popup, SECOND_SHARER);

      await press(demoted, "ss-detach");
      await expectDemoted(SECOND_SHARER);
      await expectWindowPlays(popup, SHARER_NAME);
      await expectFocus(m.viewer, "received:ss-detach");

      await stopScreenShare(second);
      await expect(demoted).toHaveCount(0, { timeout: 20_000 });
      await expect(receivedTile(m.viewer)).toHaveAttribute("data-share-mode", "detached");
      await expectWindowPlays(popup, SHARER_NAME);

      const toast = m.viewer.locator(WINDOW_TOAST);
      await startScreenShare(second, m.viewer);
      await expectDemoted(SHARER_NAME);
      await expect(toast).toHaveText(`${SECOND_SHARER} is sharing in the separate window`);
      await stopScreenShare(second);
      await expect(toast).toHaveText(`${SHARER_NAME} is sharing in the separate window`);
      await expect(demoted).toHaveCount(0, { timeout: 20_000 });
      await expectWindowPlays(popup, SHARER_NAME);
    } finally {
      await m.close();
    }
  });
});

test.describe("Issue 3308: the separate window is a regular window", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("Detach opens a popup window, never Picture-in-Picture, and Reattach closes it @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const m = await openShareMeeting(baseURL, "popup", async (ctx) => {
      await tileView(ctx);
      await ctx.addInitScript(() => {
        if (window.location.href === "about:blank") return;
        const w = window as unknown as { __pipCalls?: number };
        Object.defineProperty(window, "documentPictureInPicture", {
          configurable: true,
          value: {
            window: null,
            requestWindow: () => {
              w.__pipCalls = (w.__pipCalls ?? 0) + 1;
              return new Promise(() => {});
            },
          },
        });
        w.__pipCalls = 0;
      });
    });
    const pipCalls = (): Promise<number | null> =>
      m.viewer.evaluate(() => (window as unknown as { __pipCalls?: number }).__pipCalls ?? null);
    try {
      expect(await pipCalls(), "the counting Picture-in-Picture stub is installed").toBe(0);
      const tile = await share(m);
      await expect(tile).toHaveAttribute("data-share-mode", "tile");

      const popup = await openDetached(m.viewer, tile, control(tile, "ss-detach"));
      expect(popup, "Detach must open a popup window").not.toBeNull();
      expect(await pipCalls(), "Detach must not request a Picture-in-Picture window").toBe(0);
      const reattach = popup!.locator("#ss-detached-reattach");
      await expect(reattach).toBeVisible({ timeout: 10_000 });
      await expect(popup!.locator("#ss-detached-maximize")).toHaveAttribute(
        "aria-pressed",
        "false",
      );
      await expect(tile).toHaveAttribute("data-share-mode", "detached", { timeout: 10_000 });

      const closed = popup!.waitForEvent("close", { timeout: 10_000 });
      await reattach.click({ timeout: 10_000 });
      await closed;
      await expect(tile).toHaveAttribute("data-share-mode", "tile", { timeout: 10_000 });
    } finally {
      await m.close();
    }
  });
});
