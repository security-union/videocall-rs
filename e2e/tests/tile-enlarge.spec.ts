import { test, expect, chromium, Browser, ElementHandle, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { enableCamera, openPeerList, setMockPeers, wakeControls } from "../helpers/controls";
import {
  ENLARGED_ORDER_BASE,
  ENLARGE_MIN_TILE_W_FINE_POINTER,
  PIN_ORDER_BASE,
} from "../helpers/rust-mirrored-constants";
import {
  MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT,
  seedShareViewMode,
  startScreenShare,
  stopScreenShare,
} from "../helpers/screen-share-meeting";
import { enterMeetingAsHost, guestJoinsMeeting } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 3367: a remote camera tile can span 2x2 grid cells at the front of the
 * grid and be restored. The DOM contract is the camera tile in canvas_generator.rs
 * (`tile-enlarge-button`, `data-enlarged`, `.tile-enlarged`) and `.tile-slot` in peer_tile.rs.
 */

const DEFAULT_UI_URL = "http://localhost:3001";
const GRID = "#grid-container";
const PEER_TILES = `${GRID} > .ss-peer-panel > .tile-slot > [data-tile-root="true"]`;
const MOCK_TILES = `${PEER_TILES}[id^="peer-video-mock-"]`;
const SIDE_PANEL_TILES = `${GRID} > .ss-peer-panel > .tile-slot > .split-peer-tile`;
const SELF_CELL = "host-controls-nav";
const ENLARGE = '[data-testid="tile-enlarge-button"]';
const PIN = '[data-testid="tile-pin-button"]';
const ANNOUNCER = '[data-testid="ss-detach-announce"]';
const CAMERA_CANVAS = ".canvas-container > canvas";
const PEER_LIST = "#peer-list-container";
const RECEIVED_SHARE = '[data-testid="received-share-tile"]';
const BADGE_ID = "grid-overflow-badge";
const OVERFLOW_BADGE = `${GRID} > .ss-peer-panel > button#${BADGE_ID}.grid-overflow-badge`;
const OVERFLOW_LIST = '#grid-overflow-list[role="dialog"]';
const OVERFLOW_ROWS = `${OVERFLOW_LIST} > ul.grid-overflow-list-rows > li`;
const DESKTOP = { width: 1280, height: 720 };
const TOO_SHORT_TO_ENLARGE = { width: 1280, height: 320 };
const ENLARGE_GAINS_TOO_LITTLE = { width: 1366, height: 650 };
// Clears `PIN_DOUBLE_ACTIVATION_MS` (350, pin_order.rs).
const GUARD_WAIT_MS = 400;
const SIZE_TOLERANCE_PX = 1;
const FIT_SLACK_PX = 0.5;

const HOST = "EnlargeHost";
const ALPHA = "EnlargeAlpha";
const BRAVO = "EnlargeBravo";
const CHARLIE = "EnlargeCharlie";

function seedGridSelfView(): void {
  try {
    window.localStorage.setItem("vc_self_view_placement", "grid");
  } catch {
    /* no storage on this document */
  }
}

const tileOf = (page: Page, id: string): Locator => page.locator(`#${id}`);
const enlargeOf = (page: Page, id: string): Locator => tileOf(page, id).locator(ENLARGE);
const pinOf = (page: Page, id: string): Locator => tileOf(page, id).locator(PIN);

async function tileIdByName(page: Page, name: string): Promise<string> {
  const tile = page.locator(PEER_TILES).filter({
    has: page.locator(".floating-name-text", { hasText: new RegExp(`^${name}$`) }),
  });
  await expect(tile, `${name}'s tile`).toHaveCount(1, { timeout: 45_000 });
  const id = await tile.getAttribute("id");
  expect(id, `${name}'s tile id`).toMatch(/^peer-video-.+-div$/);
  return id!;
}

function domOrder(page: Page): Promise<string[]> {
  return page.evaluate(
    (sel) => Array.from(document.querySelectorAll(sel), (el) => el.id),
    PEER_TILES,
  );
}

// Row-major reading order of the given elements' boxes.
function visualOrder(page: Page, ids: string[]): Promise<string[]> {
  return page.evaluate((list) => {
    const items = list.map((id) => {
      const el = document.getElementById(id);
      if (!el) {
        throw new Error(`#${id} is not in the DOM`);
      }
      const r = el.getBoundingClientRect();
      if (r.width === 0 || r.height === 0) {
        throw new Error(`#${id} has no layout box`);
      }
      return { id, top: r.top, left: r.left };
    });
    items.sort((a, b) => a.top - b.top);
    const rows: (typeof items)[] = [];
    for (const item of items) {
      const row = rows[rows.length - 1];
      if (row && Math.abs(row[0].top - item.top) <= 4) {
        row.push(item);
      } else {
        rows.push([item]);
      }
    }
    return rows.flatMap((row) => row.sort((a, b) => a.left - b.left).map((i) => i.id));
  }, ids);
}

async function expectOrder(page: Page, visual: string[], what: string): Promise<void> {
  await expect
    .poll(() => domOrder(page), { timeout: 10_000, message: `${what}: camera tile DOM order` })
    .toEqual(visual.filter((id) => id !== SELF_CELL));
  await expect
    .poll(() => visualOrder(page, visual), { timeout: 10_000, message: `${what}: visual order` })
    .toEqual(visual);
}

interface Size {
  width: number;
  height: number;
}

async function sizeOf(page: Page, id: string): Promise<Size> {
  const el = tileOf(page, id);
  await expect(el, `#${id} must be mounted`).toHaveCount(1);
  const box = await el.boundingBox();
  expect(box, `#${id} must have a layout box`).not.toBeNull();
  return { width: box!.width, height: box!.height };
}

const sizeError = (a: Size, b: Size): number =>
  Math.max(Math.abs(a.width - b.width), Math.abs(a.height - b.height));

async function expectSize(page: Page, id: string, want: Size, what: string): Promise<void> {
  await expect
    .poll(async () => sizeError(await sizeOf(page, id), want), { timeout: 10_000, message: what })
    .toBeLessThanOrEqual(SIZE_TOLERANCE_PX);
}

async function expectSpansTwoCells(page: Page, id: string, standardId: string): Promise<void> {
  const grid = page.locator(GRID);
  await expect(grid).toHaveCount(1);
  const gap = await grid.evaluate((el) => {
    const cs = getComputedStyle(el);
    return { width: parseFloat(cs.columnGap), height: parseFloat(cs.rowGap) };
  });
  expect(gap.width, "premise: the grid has a column gap").toBeGreaterThan(0);
  expect(gap.height, "premise: the grid has a row gap").toBeGreaterThan(0);
  await expect
    .poll(
      async () => {
        const standard = await sizeOf(page, standardId);
        return sizeError(await sizeOf(page, id), {
          width: 2 * standard.width + gap.width,
          height: 2 * standard.height + gap.height,
        });
      },
      { timeout: 10_000, message: `#${id} must be two of #${standardId}'s cells and a gap` },
    )
    .toBeLessThanOrEqual(SIZE_TOLERANCE_PX);
  expect((await sizeOf(page, standardId)).width, "premise: a real cell").toBeGreaterThan(100);
}

interface GridFit {
  clipped: string[];
  overlapping: string[];
}

function gridFit(page: Page, ids: string[]): Promise<GridFit> {
  return page.evaluate(
    ({ grid, ids, slack }) => {
      const container = document.querySelector(grid);
      if (!container) {
        throw new Error(`${grid} is not in the DOM`);
      }
      const outer = container.getBoundingClientRect();
      const cs = getComputedStyle(container);
      const content = {
        left: outer.left + parseFloat(cs.paddingLeft),
        top: outer.top + parseFloat(cs.paddingTop),
        right: outer.right - parseFloat(cs.paddingRight),
        bottom: outer.bottom - parseFloat(cs.paddingBottom),
      };
      const rects = ids.map((id) => {
        const el = document.getElementById(id);
        if (!el) {
          throw new Error(`#${id} is not in the DOM`);
        }
        const r = el.getBoundingClientRect();
        if (r.width === 0 || r.height === 0) {
          throw new Error(`#${id} has no layout box`);
        }
        return { id, r };
      });
      const clipped = rects
        .filter(
          ({ r }) =>
            r.left < content.left - slack ||
            r.top < content.top - slack ||
            r.right > content.right + slack ||
            r.bottom > content.bottom + slack,
        )
        .map(({ id }) => id);
      const overlapping: string[] = [];
      rects.forEach(({ id, r }, i) => {
        for (const { id: other, r: o } of rects.slice(i + 1)) {
          const apart =
            r.right <= o.left + slack ||
            o.right <= r.left + slack ||
            r.bottom <= o.top + slack ||
            o.bottom <= r.top + slack;
          if (!apart) {
            overlapping.push(`${id} / ${other}`);
          }
        }
      });
      return { clipped, overlapping };
    },
    { grid: GRID, ids, slack: FIT_SLACK_PX },
  );
}

async function expectGridFits(page: Page, ids: string[]): Promise<void> {
  await expect
    .poll(() => gridFit(page, ids), {
      timeout: 10_000,
      message: "every tile stays in the grid's content box, overlapping none",
    })
    .toEqual({ clipped: [], overlapping: [] });
}

async function expectEnlargeState(
  page: Page,
  id: string,
  name: string,
  enlarged: boolean,
): Promise<void> {
  const tile = tileOf(page, id);
  const button = enlargeOf(page, id);
  await expect(button).toHaveCount(1, { timeout: 10_000 });
  await expect(button).toHaveAttribute("id", `${id}-enlarge-btn`);
  await expect(button).toHaveAttribute("aria-label", `Enlarge ${name}`);
  await expect(button).toHaveAttribute("aria-pressed", enlarged ? "true" : "false", {
    timeout: 10_000,
  });
  await expect(button).toHaveAttribute("title", enlarged ? "Restore size" : "Enlarge");
  if (enlarged) {
    await expect(tile).toHaveAttribute("data-enlarged", "true");
    await expect(tile).toHaveClass(/\btile-enlarged\b/);
  } else {
    await expect(tile).not.toHaveAttribute("data-enlarged");
    await expect(tile).not.toHaveClass(/\btile-enlarged\b/);
  }
}

// Spaces toggles on different tiles past the double-activation guard, which pin and enlarge share.
function clickPacer(page: Page): (id: string, control: Locator) => Promise<void> {
  let last: { at: number; id: string } | null = null;
  return async (id, control) => {
    if (last && last.id !== id) {
      const wait = last.at + GUARD_WAIT_MS - Date.now();
      if (wait > 0) {
        await page.waitForTimeout(wait);
      }
    }
    await control.click({ timeout: 10_000 });
    last = { at: Date.now(), id };
  };
}

async function handleOf(
  locator: Locator,
  what: string,
  timeout: number,
): Promise<ElementHandle<Element>> {
  const handle = await locator.elementHandle({ timeout });
  expect(handle, `${what} must be mounted`).not.toBeNull();
  return handle!;
}

interface MountedTile {
  id: string;
  node: ElementHandle<Element>;
}

// A remount would rebuild the decoder canvas and request a keyframe.
function remountedNodes(
  page: Page,
  tiles: MountedTile[],
  canvasTile: string,
  canvas: ElementHandle<Element>,
): Promise<string[]> {
  return page.evaluate(
    ({ tiles, canvasTile, canvas, canvasSelector }) => {
      const remounted = tiles
        .filter(({ id, node }) => document.getElementById(id) !== node || !node.isConnected)
        .map(({ id }) => id);
      const current = document.getElementById(canvasTile)?.querySelector(canvasSelector);
      if (current !== canvas || !canvas.isConnected) {
        remounted.push(`${canvasTile} canvas`);
      }
      return remounted;
    },
    { tiles, canvasTile, canvas, canvasSelector: CAMERA_CANVAS },
  );
}

async function closePeerList(page: Page): Promise<void> {
  await page.locator(`${PEER_LIST} button.close-button`).click({ timeout: 10_000 });
  await expect(page.locator(PEER_LIST)).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
}

async function launchAll(count: number): Promise<Browser[]> {
  const browsers: Browser[] = [];
  for (let i = 0; i < count; i++) {
    browsers.push(await chromium.launch({ args: BROWSER_ARGS }));
  }
  return browsers;
}

test.describe("Issue 3367: enlarging a camera tile", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("an enlarged tile leads at two cells without a remount, and restoring undoes it", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_tile_enlarge_${Date.now()}`;
    let browsers: Browser[] = [];
    try {
      browsers = await launchAll(4);
      const hostCtx = await createAuthenticatedContext(
        browsers[0],
        "enlarge-host@videocall.rs",
        HOST,
        uiURL,
      );
      await hostCtx.addInitScript(seedGridSelfView);
      const host = await hostCtx.newPage();
      await enterMeetingAsHost(host, meetingId, HOST);

      // Each guest's tile lands on the host before the next joins, so join order is A, B, C.
      const guests: Page[] = [];
      const ids: string[] = [];
      for (const [i, name] of [ALPHA, BRAVO, CHARLIE].entries()) {
        const ctx = await createAuthenticatedContext(
          browsers[i + 1],
          `${name.toLowerCase()}@videocall.rs`,
          name,
          uiURL,
        );
        const guest = await ctx.newPage();
        await guestJoinsMeeting(host, guest, meetingId, name);
        guests.push(guest);
        ids.push(await tileIdByName(host, name));
      }
      const [a, b, c] = ids;
      await enableCamera(guests[2]);

      await expect(host.locator(`${GRID} > #${SELF_CELL}`)).toHaveAttribute(
        "data-self-placement",
        "grid",
      );
      const grid = host.locator(GRID);
      const announcer = host.locator(ANNOUNCER);
      const cells = [SELF_CELL, a, b, c];
      const click = clickPacer(host);
      const clickEnlarge = (id: string) => click(id, enlargeOf(host, id));
      const clickPin = (id: string) => click(id, pinOf(host, id));

      await expectOrder(host, cells, "join order before any enlarge");
      await expect(grid).toHaveCSS("display", "flex");
      for (const [id, name] of [
        [a, ALPHA],
        [b, BRAVO],
        [c, CHARLIE],
      ]) {
        await expectEnlargeState(host, id, name, false);
      }
      const standard = await sizeOf(host, a);

      const mountedTiles = async (): Promise<MountedTile[]> => {
        const nodes: MountedTile[] = [];
        for (const id of [a, b, c]) {
          nodes.push({ id, node: await handleOf(tileOf(host, id), `#${id}`, 10_000) });
        }
        return nodes;
      };
      let tileNodes = await mountedTiles();
      const canvas = await handleOf(
        tileOf(host, c).locator(CAMERA_CANVAS),
        "C's camera canvas",
        45_000,
      );
      const expectNoRemount = async (what: string) => {
        expect(await remountedNodes(host, tileNodes, c, canvas), `${what} remounted`).toEqual([]);
      };

      await test.step("enlarge C: it leads at two cells and a gap, and nothing is overlapped or clipped", async () => {
        await clickEnlarge(c);
        await expectEnlargeState(host, c, CHARLIE, true);
        await expect(announcer).toHaveText(`${CHARLIE} enlarged`);
        await expect(enlargeOf(host, c), "the keyed reorder must not lose focus").toBeFocused();
        await expect(grid).toHaveCSS("display", "grid");
        await expect(grid).toHaveCSS("grid-auto-flow", "dense");
        await expect(tileOf(host, c)).toHaveCSS("order", String(ENLARGED_ORDER_BASE));
        await expectOrder(host, [c, SELF_CELL, a, b], "after enlarging C");
        await expectSpansTwoCells(host, c, a);
        const shrunk = await sizeOf(host, a);
        expect(shrunk.width, "the standard tiles shrink to make room").toBeLessThan(standard.width);
        await expectSize(host, b, shrunk, "every standard tile keeps one size");
        await expectSize(host, SELF_CELL, shrunk, "the self tile takes one standard cell");
        await expectGridFits(host, cells);
        await expectNoRemount("enlarging");
      });

      await test.step("the way back stays visible without hover or focus", async () => {
        await host.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
        await host.mouse.move(0, 0);
        await expect(enlargeOf(host, c)).toHaveCSS("opacity", "1");
        await expect(enlargeOf(host, a), "a resting control waits for hover").toHaveCSS(
          "opacity",
          "0",
        );
      });

      await test.step("C stays enlarged beside an open drawer, drops when nothing can paint, then comes back", async () => {
        const beforeDrawer = await sizeOf(host, a);
        await openPeerList(host);
        await expect
          .poll(async () => (await sizeOf(host, a)).width, {
            timeout: 10_000,
            message: "the open drawer re-plans the grid",
          })
          .toBeLessThan(beforeDrawer.width);
        await expectEnlargeState(host, c, CHARLIE, true);
        await expect(grid).toHaveCSS("display", "grid");
        await expectOrder(host, [c, SELF_CELL, a, b], "beside the open peer list");
        await expectSpansTwoCells(host, c, a);
        await expectGridFits(host, cells);
        await closePeerList(host);

        await host.setViewportSize(TOO_SHORT_TO_ENLARGE);
        await expect(host.locator(OVERFLOW_BADGE)).toBeVisible({ timeout: 10_000 });
        await expect
          .poll(() => domOrder(host), {
            timeout: 10_000,
            message: "the requested tile keeps its slot while the others fold into the badge",
          })
          .toEqual([c]);
        await expect(tileOf(host, c)).not.toHaveAttribute("data-enlarged");
        await expect(tileOf(host, c)).not.toHaveClass(/\btile-enlarged\b/);
        await expect(host.locator(ENLARGE)).toHaveCount(0);
        await expect(pinOf(host, c)).toHaveCount(1);
        await expect(tileOf(host, c)).toHaveCSS("order", "0");
        await expect(grid).toHaveCSS("display", "flex");
        await expect
          .poll(() => visualOrder(host, [SELF_CELL, c]), {
            timeout: 10_000,
            message: "an unpainted tile sits behind the self tile",
          })
          .toEqual([SELF_CELL, c]);
        await expectGridFits(host, [SELF_CELL, c, BADGE_ID]);

        await host.setViewportSize(DESKTOP);
        await expectEnlargeState(host, c, CHARLIE, true);
        await expect(tileOf(host, c)).toHaveCSS("order", String(ENLARGED_ORDER_BASE));
        await expectOrder(host, [c, SELF_CELL, a, b], "grown back");
        await expectSpansTwoCells(host, c, a);
        await expectGridFits(host, cells);
        expect(
          await remountedNodes(host, tileNodes, c, canvas),
          "only the tiles that folded into the badge are rebuilt",
        ).toEqual([a, b]);
        tileNodes = await mountedTiles();
      });

      await test.step("restore C: the grid is flex again and C is back at the standard size and slot", async () => {
        await clickEnlarge(c);
        await expectEnlargeState(host, c, CHARLIE, false);
        await expect(announcer).toHaveText(`${CHARLIE} restored to standard size`);
        await expect(enlargeOf(host, c)).toBeFocused();
        await expect(grid).toHaveCSS("display", "flex");
        await expect(grid).toHaveCSS("grid-auto-flow", "row");
        await expect(host.locator(`${GRID} .tile-enlarged`)).toHaveCount(0);
        await expect(host.locator(`${GRID} [data-enlarged]`)).toHaveCount(0);
        await expect(tileOf(host, c)).toHaveCSS("order", "0");
        await expectOrder(host, cells, "after restoring C");
        await expectSize(host, c, standard, "C returns to the standard size");
        await expectSize(host, a, standard, "the other tiles grow back");
        await expectNoRemount("restoring");
      });

      await test.step("a second enlarge goes behind the first, and restoring the first lets it lead", async () => {
        await clickEnlarge(c);
        await expectEnlargeState(host, c, CHARLIE, true);
        await clickEnlarge(a);
        await expectEnlargeState(host, a, ALPHA, true);
        await expectEnlargeState(host, c, CHARLIE, true);
        await expect(tileOf(host, c)).toHaveCSS("order", String(ENLARGED_ORDER_BASE));
        await expect(tileOf(host, a)).toHaveCSS("order", String(ENLARGED_ORDER_BASE + 1));
        await expectOrder(host, [c, a, SELF_CELL, b], "C enlarged, then A");
        await expectSpansTwoCells(host, c, b);
        await expectSpansTwoCells(host, a, b);
        await expectGridFits(host, cells);
        await expectNoRemount("a second enlarge");

        await clickEnlarge(c);
        await expectEnlargeState(host, c, CHARLIE, false);
        await expectEnlargeState(host, a, ALPHA, true);
        await expectOrder(host, [a, SELF_CELL, b, c], "C restored while A stays enlarged");
        await expectSpansTwoCells(host, a, b);

        await clickEnlarge(a);
        await expectEnlargeState(host, a, ALPHA, false);
        await expect(grid).toHaveCSS("display", "flex");
        await expectOrder(host, cells, "nothing enlarged");
      });

      await test.step("a tile both pinned and enlarged leads with both controls pressed, then returns to its pin slot", async () => {
        await clickPin(b);
        await clickPin(a);
        await expectOrder(host, [a, b, SELF_CELL, c], "A and B pinned, most recent first");
        await expect(tileOf(host, b)).toHaveAttribute("data-pin-rank", "1");
        await expect(tileOf(host, a)).toHaveAttribute("data-pin-rank", "0");

        await clickEnlarge(b);
        await expectEnlargeState(host, b, BRAVO, true);
        await expect(pinOf(host, b)).toHaveAttribute("aria-pressed", "true");
        await expect(tileOf(host, b)).toHaveAttribute("data-pinned", "true");
        await expect(tileOf(host, b)).toHaveAttribute("data-pin-rank", "1");
        await expect(tileOf(host, b)).toHaveCSS("order", String(ENLARGED_ORDER_BASE));
        await expectOrder(host, [b, a, SELF_CELL, c], "enlarged B ahead of the newer pin A");
        await expectSpansTwoCells(host, b, c);
        await expectGridFits(host, cells);

        await clickEnlarge(b);
        await expectEnlargeState(host, b, BRAVO, false);
        await expect(pinOf(host, b)).toHaveAttribute("aria-pressed", "true");
        await expect(tileOf(host, b)).toHaveAttribute("data-pin-rank", "1");
        await expect(tileOf(host, b)).toHaveCSS("order", String(PIN_ORDER_BASE + 1));
        await expect(grid).toHaveCSS("display", "flex");
        await expectOrder(host, [a, b, SELF_CELL, c], "B back in its pin slot");
        await expectNoRemount("toggling a pinned tile");
      });
    } finally {
      await Promise.all(browsers.map((br) => br.close().catch(() => undefined)));
    }
  });

  test("no control where enlarging is not offered, and an enlarged tile comes back after a share or a peer leaving", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_tile_enlarge_gate_${Date.now()}`;
    let browsers: Browser[] = [];
    try {
      browsers = await launchAll(3);
      const hostCtx = await createAuthenticatedContext(
        browsers[0],
        "enlarge-gate-host@videocall.rs",
        HOST,
        uiURL,
      );
      await seedShareViewMode(hostCtx, "enlarged");
      const host = await hostCtx.newPage();
      await enterMeetingAsHost(host, meetingId, HOST);

      const aCtx = await createAuthenticatedContext(
        browsers[1],
        "enlarge-gate-alpha@videocall.rs",
        ALPHA,
        uiURL,
      );
      await guestJoinsMeeting(host, await aCtx.newPage(), meetingId, ALPHA);
      const a = await tileIdByName(host, ALPHA);
      const grid = host.locator(GRID);
      const allControls = host.locator(ENLARGE);

      await test.step("the lone remote is full-bleed: it has a pin and no enlarge control", async () => {
        await expect(host.locator(`#${SELF_CELL}`)).toHaveAttribute(
          "data-self-placement",
          "corner",
        );
        await expect(tileOf(host, a)).toHaveClass(/\bfull-bleed\b/);
        await expect(pinOf(host, a)).toHaveCount(1);
        await expect(allControls).toHaveCount(0);
      });

      const sharerCtx = await createAuthenticatedContext(
        browsers[2],
        "enlarge-gate-bravo@videocall.rs",
        BRAVO,
        uiURL,
      );
      await sharerCtx.addInitScript(MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT);
      const sharer = await sharerCtx.newPage();
      await guestJoinsMeeting(host, sharer, meetingId, BRAVO);
      const b = await tileIdByName(host, BRAVO);

      await test.step("a second remote ends the full bleed, so both tiles offer it", async () => {
        await expect(tileOf(host, a)).not.toHaveClass(/\bfull-bleed\b/);
        await expectEnlargeState(host, a, ALPHA, false);
        await expectEnlargeState(host, b, BRAVO, false);
        await expect(allControls).toHaveCount(2);
      });

      await test.step("where enlarging would gain too little the control is withheld", async () => {
        await host.setViewportSize(ENLARGE_GAINS_TOO_LITTLE);
        await expect(allControls).toHaveCount(0, { timeout: 10_000 });
        await expect(pinOf(host, a)).toHaveCount(1);
        expect(
          (await sizeOf(host, a)).width,
          "premise: the tile is over the width floor, so the gain is what withholds it",
        ).toBeGreaterThanOrEqual(ENLARGE_MIN_TILE_W_FINE_POINTER);
        await host.setViewportSize(DESKTOP);
        await expectEnlargeState(host, a, ALPHA, false);
      });

      await enlargeOf(host, a).click({ timeout: 10_000 });
      await expectEnlargeState(host, a, ALPHA, true);
      await expectSpansTwoCells(host, a, b);
      await expectGridFits(host, [a, b]);

      await test.step("in the share's side panel the tile is standard and has no control", async () => {
        const shared = await startScreenShare(sharer, host);
        expect(shared, "the host never received the mocked screen share").toBe(true);
        await expect(grid).toHaveAttribute("data-share-layout", "enlarged", { timeout: 10_000 });
        await expect(grid).toHaveClass(/\bhas-screen-share\b/);
        const sideTile = host.locator(`${SIDE_PANEL_TILES}#${a}`);
        await expect(sideTile).toHaveCount(1, { timeout: 15_000 });
        await expect(sideTile.locator(PIN)).toHaveCount(1);
        await expect(allControls).toHaveCount(0);
        await expect(sideTile).not.toHaveClass(/\btile-enlarged\b/);
        await expect(sideTile).not.toHaveAttribute("data-enlarged");
        await expect(sideTile).toHaveCSS("order", "0");
      });

      await test.step("with the share back in a grid tile, the enlarged tile leads it and everything fits", async () => {
        const shareTile = host.locator(RECEIVED_SHARE);
        await shareTile.hover({ timeout: 10_000 });
        await shareTile.locator('[data-testid="ss-enlarge"]').click({ timeout: 10_000 });
        await expect(shareTile).toHaveAttribute("data-share-mode", "tile", { timeout: 10_000 });
        await expect(grid).toHaveAttribute("data-share-layout", "tile");
        await expect(grid).not.toHaveClass(/\bhas-screen-share\b/);
        await expectEnlargeState(host, a, ALPHA, true);
        await expect(tileOf(host, a)).toHaveCSS("order", String(ENLARGED_ORDER_BASE));
        const shareId = await shareTile.getAttribute("id");
        expect(shareId, "the share tile root has an id").toBeTruthy();
        await expect
          .poll(() => visualOrder(host, [a, shareId!, b]), {
            timeout: 10_000,
            message: "the enlarged tile leads the share tile",
          })
          .toEqual([a, shareId!, b]);
        await expectSpansTwoCells(host, a, b);
        await expectGridFits(host, [a, b, shareId!]);
      });

      await test.step("when the share stops the tile is still enlarged", async () => {
        await stopScreenShare(sharer);
        await expect(grid).toHaveAttribute("data-share-layout", "none", { timeout: 20_000 });
        await expect(grid).not.toHaveClass(/\bhas-screen-share\b/);
        await expectEnlargeState(host, a, ALPHA, true);
        await expect(grid).toHaveCSS("display", "grid");
        await expect(tileOf(host, a)).toHaveCSS("order", String(ENLARGED_ORDER_BASE));
        await expectOrder(host, [a, b], "the enlarged tile leads again");
        await expectSpansTwoCells(host, a, b);
        await expectGridFits(host, [a, b]);
      });

      await test.step("the other guest leaving makes it a lone full-bleed tile; a rejoin enlarges it again", async () => {
        await wakeControls(sharer);
        await sharer.locator("button.video-control-button.danger").first().click({
          timeout: 10_000,
        });
        await expect(sharer).toHaveURL("/", { timeout: 15_000 });
        await expect(tileOf(host, b)).toHaveCount(0, { timeout: 30_000 });
        await expect(tileOf(host, a)).toHaveClass(/\bfull-bleed\b/);
        await expect(tileOf(host, a)).not.toHaveAttribute("data-enlarged");
        await expect(allControls).toHaveCount(0);
        await expect(pinOf(host, a)).toHaveCount(1);

        await guestJoinsMeeting(host, sharer, meetingId, BRAVO);
        const rejoined = await tileIdByName(host, BRAVO);
        await expectEnlargeState(host, a, ALPHA, true);
        await expectSpansTwoCells(host, a, rejoined);
        await expectGridFits(host, [a, rejoined]);

        await enlargeOf(host, a).click({ timeout: 10_000 });
        await expectEnlargeState(host, a, ALPHA, false);
        await expect(grid).toHaveCSS("display", "flex");
      });
    } finally {
      await Promise.all(browsers.map((br) => br.close().catch(() => undefined)));
    }
  });

  test("an enlarged guest holds its two cells ahead of a +N badge, and folds into it when restored", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_tile_enlarge_overflow_${Date.now()}`;
    let browsers: Browser[] = [];
    try {
      browsers = await launchAll(2);
      const hostCtx = await createAuthenticatedContext(
        browsers[0],
        "enlarge-overflow-host@videocall.rs",
        HOST,
        uiURL,
      );
      await hostCtx.addInitScript(seedGridSelfView);
      const host = await hostCtx.newPage();
      await enterMeetingAsHost(host, meetingId, HOST);
      const guestCtx = await createAuthenticatedContext(
        browsers[1],
        "enlarge-overflow-guest@videocall.rs",
        ALPHA,
        uiURL,
      );
      await guestJoinsMeeting(host, await guestCtx.newPage(), meetingId, ALPHA);
      const g = await tileIdByName(host, ALPHA);
      const badge = host.locator(OVERFLOW_BADGE);
      const list = host.locator(OVERFLOW_LIST);
      const guestRow = list.locator(
        `#grid-overflow-row-${g.replace(/^peer-video-(.+)-div$/, "$1")}`,
      );

      await enlargeOf(host, g).click({ timeout: 10_000 });
      await expectEnlargeState(host, g, ALPHA, true);

      await test.step("40 mock peers overflow the grid: the guest leads, enlarged, and everything fits", async () => {
        expect(await setMockPeers(host, 40), "Mock Peers must be enabled").toBe(true);
        await expect(badge).toBeVisible({ timeout: 15_000 });
        await expect(host.locator(MOCK_TILES).first()).toBeVisible();
        await expectEnlargeState(host, g, ALPHA, true);
        await expect.poll(async () => (await domOrder(host))[0], { timeout: 10_000 }).toBe(g);
        const shown = await domOrder(host);
        expect(shown.length, "premise: mock tiles beside the guest").toBeGreaterThan(1);
        const shownCells = [SELF_CELL, ...shown, BADGE_ID];
        await expect
          .poll(async () => (await visualOrder(host, shownCells))[0], { timeout: 10_000 })
          .toBe(g);
        await expectSpansTwoCells(host, g, shown[1]);
        await expectGridFits(host, shownCells);
      });

      await test.step("the overflow list holds only the hidden participants, not the guest", async () => {
        await badge.click({ timeout: 10_000 });
        await expect(list).toBeVisible();
        const hidden = Number(await badge.getAttribute("data-overflow-count"));
        expect(hidden, "premise: the badge hides someone").toBeGreaterThan(0);
        await expect(host.locator(OVERFLOW_ROWS)).toHaveCount(hidden);
        await expect(guestRow).toHaveCount(0);
        await expect(list).not.toContainText(ALPHA);
        await host.keyboard.press("Escape");
        await expect(list).toHaveCount(0);
      });

      await test.step("restored, the camera-off guest folds into the badge and the list", async () => {
        await enlargeOf(host, g).click({ timeout: 10_000 });
        await expect(tileOf(host, g)).toHaveCount(0, { timeout: 15_000 });
        await badge.click({ timeout: 10_000 });
        await expect(list).toBeVisible();
        await expect(guestRow).toHaveCount(1);
      });
    } finally {
      await Promise.all(browsers.map((br) => br.close().catch(() => undefined)));
    }
  });
});
