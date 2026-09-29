import { test, expect, chromium, Browser, ElementHandle, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { continuousToneWavPath } from "../helpers/audio-fixtures";
import { setMockPeers, wakeControls } from "../helpers/controls";
import { classifyGlow } from "../helpers/speaking-glow";
import { enterMeetingAsHost, guestJoinsMeeting } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 2866: pinning moves a tile to the front of the grid at the size of every
 * other tile, most recent pin first. The DOM contract is the camera tile in
 * canvas_generator.rs (`tile-pin-button`, `data-pinned`, `data-pin-rank`) and
 * the peer loop in attendants.rs (`.ss-peer-panel`, `.grid-overflow-badge`).
 */

const DEFAULT_UI_URL = "http://localhost:3001";
const GRID = "#grid-container";
const PEER_TILES = `${GRID} > .ss-peer-panel > .tile-slot > [data-tile-root="true"]`;
const MOCK_TILES = `${PEER_TILES}[id^="peer-video-mock-"]`;
const SELF_CELL = "host-controls-nav";
const PIN = '[data-testid="tile-pin-button"]';
const ANNOUNCER = '[data-testid="ss-detach-announce"]';
const OVERFLOW_BADGE = `${GRID} .grid-overflow-badge`;
// Clears `PIN_DOUBLE_ACTIVATION_MS` (350, pin_order.rs), which drops a pin of a
// different tile that lands sooner than that after the last one.
const PIN_GUARD_WAIT_MS = 400;
const SIZE_TOLERANCE_PX = 1;
// A's signal and host-menu buttons may sit between B's pin and A's.
const TAB_STOPS_TO_A = 6;

const HOST = "PinOrderHost";
const ALPHA = "PinAlpha";
const BRAVO = "PinBravo";
const CHARLIE = "PinCharlie";

function seedGridSelfView(): void {
  try {
    window.localStorage.setItem("vc_self_view_placement", "grid");
  } catch {
    /* no storage on this document */
  }
}

const tileOf = (page: Page, id: string): Locator => page.locator(`#${id}`);
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

interface Geometry {
  width: number;
  height: number;
  position: string;
}

function geometryOf(page: Page, id: string): Promise<Geometry> {
  return tileOf(page, id).evaluate((el) => {
    const r = el.getBoundingClientRect();
    return { width: r.width, height: r.height, position: getComputedStyle(el).position };
  });
}

async function expectSiblingSize(page: Page, pinned: string, siblings: string[]): Promise<void> {
  await expect
    .poll(
      async () => {
        const p = await geometryOf(page, pinned);
        let worst = 0;
        for (const id of siblings) {
          const s = await geometryOf(page, id);
          worst = Math.max(worst, Math.abs(s.width - p.width), Math.abs(s.height - p.height));
        }
        return worst;
      },
      { timeout: 10_000, message: `#${pinned} must be the size of ${siblings.join(", ")}` },
    )
    .toBeLessThanOrEqual(SIZE_TOLERANCE_PX);
  const p = await geometryOf(page, pinned);
  const s = await geometryOf(page, siblings[0]);
  expect(p.width, "the pinned tile must have a real box").toBeGreaterThan(100);
  expect(p.position).not.toBe("fixed");
  expect(p.position, "a pinned tile is laid out like its siblings").toBe(s.position);
}

// Ids whose own centre point does not hit them.
function coveredCentres(page: Page, ids: string[]): Promise<string[]> {
  return page.evaluate(
    (list) =>
      list.filter((id) => {
        const el = document.getElementById(id);
        if (!el) {
          return true;
        }
        const r = el.getBoundingClientRect();
        const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        return !(hit && el.contains(hit));
      }),
    ids,
  );
}

async function expectPinState(
  page: Page,
  id: string,
  name: string,
  rank: number | null,
): Promise<void> {
  const tile = tileOf(page, id);
  const pin = pinOf(page, id);
  await expect(pin).toHaveAttribute("aria-label", `Pin ${name}`);
  await expect(pin).toHaveAttribute("aria-pressed", rank === null ? "false" : "true", {
    timeout: 10_000,
  });
  await expect(pin).toHaveAttribute("title", rank === null ? "Pin" : "Unpin");
  if (rank === null) {
    await expect(tile).not.toHaveAttribute("data-pinned");
    await expect(tile).not.toHaveAttribute("data-pin-rank");
    await expect(tile).not.toHaveClass(/\btile-pinned\b/);
  } else {
    await expect(tile).toHaveAttribute("data-pinned", "true");
    await expect(tile).toHaveAttribute("data-pin-rank", String(rank));
    await expect(tile).toHaveClass(/\btile-pinned\b/);
  }
}

// Spaces pin changes on different tiles past the double-activation guard.
function pinPacer(): (page: Page, id: string, activate: () => Promise<void>) => Promise<void> {
  let last: { at: number; id: string } | null = null;
  return async (page, id, activate) => {
    if (last && last.id !== id) {
      const wait = last.at + PIN_GUARD_WAIT_MS - Date.now();
      if (wait > 0) {
        await page.waitForTimeout(wait);
      }
    }
    await activate();
    last = { at: Date.now(), id };
  };
}

const opacityOf = (pin: Locator): Promise<string> =>
  pin.evaluate((el) => getComputedStyle(el).opacity);

const activeId = (page: Page): Promise<string> =>
  page.evaluate(() => document.activeElement?.id ?? "");

// No hover and no focus inside any tile.
async function leaveTiles(page: Page): Promise<void> {
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await page.mouse.move(0, 0);
}

// A remount would rebuild the decoder canvas and request a keyframe.
function keepsNodes(
  page: Page,
  id: string,
  tile: ElementHandle<Element>,
  canvas: ElementHandle<Element>,
): Promise<boolean> {
  return page.evaluate(
    ({ id, tile, canvas }) => {
      const root = document.getElementById(id);
      return (
        root === tile &&
        tile.isConnected &&
        root.querySelector(".canvas-container > canvas") === canvas &&
        canvas.isConnected
      );
    },
    { id, tile, canvas },
  );
}

interface GlowRead {
  style: string;
  speaking: boolean;
  otherPinned: string | null;
}

// One in-page read, so the glow and the other tile's pin are observed together.
function glowBeside(page: Page, speaker: string, pinned: string): Promise<GlowRead> {
  return page.evaluate(
    ({ speaker, pinned }) => {
      const s = document.getElementById(speaker);
      return {
        style: s?.getAttribute("style") ?? "",
        speaking: s?.classList.contains("speaking-tile") ?? false,
        otherPinned: document.getElementById(pinned)?.getAttribute("data-pinned") ?? null,
      };
    },
    { speaker, pinned },
  );
}

async function enableMic(page: Page): Promise<void> {
  await wakeControls(page);
  const toggle = page.locator('[data-testid="mic-toggle-button"]');
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  if (!((await toggle.getAttribute("class")) || "").includes("active")) {
    await toggle.click();
  }
  await expect(toggle).toHaveClass(/\bactive\b/, { timeout: 15_000 });
}

// The `vc_prejoin_camera_on` seed leaves this join flow's camera off, so use
// the in-meeting toggle.
async function startCamera(page: Page): Promise<void> {
  await wakeControls(page);
  const toggle = page.locator('[data-testid="camera-toggle-button"]');
  await expect(toggle).toHaveAttribute("aria-label", /Start Video/, { timeout: 15_000 });
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-label", /Stop Video/, { timeout: 15_000 });
}

async function launchAll(args: string[][]): Promise<Browser[]> {
  const browsers: Browser[] = [];
  for (const a of args) {
    browsers.push(await chromium.launch({ args: a }));
  }
  return browsers;
}

test.describe("Issue 2866: pinning reorders the grid", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a pin moves the tile to the front at grid size, most recent pin first, focus following it @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_pin_order_${Date.now()}`;
    let browsers: Browser[] = [];
    try {
      browsers = await launchAll([
        BROWSER_ARGS,
        [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${continuousToneWavPath()}`],
        BROWSER_ARGS,
        BROWSER_ARGS,
      ]);
      const [hostBrowser, speakerBrowser, quietBrowser, cameraBrowser] = browsers;
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "pin-order-host@videocall.rs",
        HOST,
        uiURL,
      );
      await hostCtx.addInitScript(seedGridSelfView);
      const host = await hostCtx.newPage();
      await enterMeetingAsHost(host, meetingId, HOST);

      // Each guest's tile lands on the host before the next one joins, so join
      // order is A, B, C. C turns its camera on, so its tile carries a decoder canvas.
      const guests: Record<string, Page> = {};
      const ids: Record<string, string> = {};
      for (const [name, browser] of [
        [ALPHA, speakerBrowser],
        [BRAVO, quietBrowser],
        [CHARLIE, cameraBrowser],
      ] as const) {
        const ctx = await createAuthenticatedContext(
          browser,
          `${name.toLowerCase()}@videocall.rs`,
          name,
          uiURL,
        );
        guests[name] = await ctx.newPage();
        await guestJoinsMeeting(host, guests[name], meetingId, name);
        ids[name] = await tileIdByName(host, name);
      }
      await startCamera(guests[CHARLIE]);

      await expect(host.locator(`#${SELF_CELL}`)).toHaveAttribute("data-self-placement", "grid");
      const [a, b, c] = [ids[ALPHA], ids[BRAVO], ids[CHARLIE]];
      const announcer = host.locator(ANNOUNCER);
      const pace = pinPacer();
      const clickPin = (id: string) =>
        pace(host, id, () => pinOf(host, id).click({ timeout: 10_000 }));
      const pressEnter = (id: string) => pace(host, id, () => host.keyboard.press("Enter"));

      await expectOrder(host, [SELF_CELL, a, b, c], "join order before any pin");
      for (const [id, name] of [
        [a, ALPHA],
        [b, BRAVO],
        [c, CHARLIE],
      ]) {
        await expectPinState(host, id, name, null);
      }
      const cTile = await tileOf(host, c).elementHandle({ timeout: 10_000 });
      const cCanvas = await tileOf(host, c)
        .locator(".canvas-container > canvas")
        .elementHandle({ timeout: 45_000 });
      expect(cTile && cCanvas, "C's tile and camera canvas must be mounted").toBeTruthy();
      const cKeepsNodes = () => keepsNodes(host, c, cTile!, cCanvas!);

      await test.step("pin C: C leads at the size of its siblings", async () => {
        await clickPin(c);
        await expectPinState(host, c, CHARLIE, 0);
        await expect(announcer).toHaveText(`${CHARLIE} pinned`);
        await expectOrder(host, [c, SELF_CELL, a, b], "after pinning C");
        expect(await cKeepsNodes(), "pinning remounted C's tile or canvas").toBe(true);
        await expectSiblingSize(host, c, [a, b]);
        await expect
          .poll(() => coveredCentres(host, [a, b, SELF_CELL]), {
            timeout: 10_000,
            message: "the pinned tile must not cover any other tile",
          })
          .toEqual([]);
      });

      await test.step("pin B, unpin C, re-pin C: the most recent pin leads", async () => {
        await clickPin(b);
        await expectPinState(host, b, BRAVO, 0);
        await expectPinState(host, c, CHARLIE, 1);
        await expectOrder(host, [b, c, SELF_CELL, a], "after pinning B");

        await clickPin(c);
        await expectPinState(host, c, CHARLIE, null);
        await expectPinState(host, b, BRAVO, 0);
        await expect(announcer).toHaveText(`${CHARLIE} unpinned`);
        await expectOrder(host, [b, SELF_CELL, a, c], "after unpinning C");
        expect(await cKeepsNodes(), "unpinning remounted C's tile or canvas").toBe(true);
        await expect.poll(() => activeId(host), { timeout: 5_000 }).toBe(`${c}-pin-btn`);
        await host.mouse.move(0, 0);
        await expect
          .poll(() => opacityOf(pinOf(host, c)), {
            timeout: 5_000,
            message: "a mouse-focused unpinned pin hides once the pointer leaves",
          })
          .toBe("0");

        await clickPin(c);
        await expectPinState(host, c, CHARLIE, 0);
        await expectPinState(host, b, BRAVO, 1);
        await expectOrder(host, [c, b, SELF_CELL, a], "after re-pinning C");
        await expectSiblingSize(host, c, [a, b]);
        expect(await cKeepsNodes(), "re-pinning remounted C's tile or canvas").toBe(true);
      });

      await test.step("Tab reaches A's hidden pin; Enter pins and unpins A, focus staying on it", async () => {
        const pinA = pinOf(host, a);
        await leaveTiles(host);
        await expect.poll(() => opacityOf(pinA), { timeout: 5_000 }).toBe("0");
        // B is the tile before A in DOM order, so Tab from B's pin enters A's tile.
        await pinOf(host, b).focus();
        const visited: string[] = [];
        while (visited.length < TAB_STOPS_TO_A && visited.at(-1) !== `${a}-pin-btn`) {
          await host.keyboard.press("Tab");
          visited.push(await activeId(host));
        }
        expect(visited.at(-1), `Tab from B's pin visited ${visited.join(", ")}`).toBe(
          `${a}-pin-btn`,
        );
        await expect
          .poll(() => opacityOf(pinA), {
            timeout: 5_000,
            message: "a keyboard-focused pin shows",
          })
          .toBe("1");

        await pressEnter(a);
        await expectPinState(host, a, ALPHA, 0);
        await expect(announcer).toHaveText(`${ALPHA} pinned`);
        await expectOrder(host, [a, c, b, SELF_CELL], "after Enter pinned A");
        await expect.poll(() => activeId(host), { timeout: 5_000 }).toBe(`${a}-pin-btn`);

        await pressEnter(a);
        await expectPinState(host, a, ALPHA, null);
        await expect(announcer).toHaveText(`${ALPHA} unpinned`);
        await expectOrder(host, [c, b, SELF_CELL, a], "after Enter unpinned A");
        await expect.poll(() => activeId(host), { timeout: 5_000 }).toBe(`${a}-pin-btn`);
      });

      await test.step("a pressed pin shows without hover or focus", async () => {
        await leaveTiles(host);
        await expect.poll(() => activeId(host)).not.toBe(`${a}-pin-btn`);
        await expect.poll(() => opacityOf(pinOf(host, c)), { timeout: 5_000 }).toBe("1");
        await expect.poll(() => opacityOf(pinOf(host, b)), { timeout: 5_000 }).toBe("1");
        await expect.poll(() => opacityOf(pinOf(host, a)), { timeout: 5_000 }).toBe("0");
      });

      await test.step("an unpinned speaker still glows while other tiles are pinned", async () => {
        await expectPinState(host, a, ALPHA, null);
        await enableMic(guests[ALPHA]);
        await expect
          .poll(
            async () => {
              const g = await glowBeside(host, a, c);
              return `${classifyGlow(g.style)} speaking-tile=${g.speaking} other-pinned=${g.otherPinned}`;
            },
            {
              timeout: 45_000,
              message: `${ALPHA} is speaking, unpinned, while ${CHARLIE} is pinned`,
            },
          )
          .toBe("lit speaking-tile=true other-pinned=true");
        await expectPinState(host, c, CHARLIE, 0);
        expect(classifyGlow((await tileOf(host, c).getAttribute("style")) ?? "")).toBe("silent");
      });
    } finally {
      await Promise.all(browsers.map((br) => br.close().catch(() => undefined)));
    }
  });

  test("a pinned late joiner keeps the first cell when mock peers overflow the grid @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_pin_overflow_${Date.now()}`;
    const late = "PinLateJoiner";
    let browsers: Browser[] = [];
    try {
      browsers = await launchAll([BROWSER_ARGS, BROWSER_ARGS]);
      const hostCtx = await createAuthenticatedContext(
        browsers[0],
        "pin-overflow-host@videocall.rs",
        HOST,
        uiURL,
      );
      await hostCtx.addInitScript(seedGridSelfView);
      const host = await hostCtx.newPage();
      await enterMeetingAsHost(host, meetingId, HOST);
      expect(await setMockPeers(host, 2), "Mock Peers must be enabled").toBe(true);
      await expect(host.locator(MOCK_TILES)).toHaveCount(2, { timeout: 15_000 });

      const lateCtx = await createAuthenticatedContext(
        browsers[1],
        "pin-late-joiner@videocall.rs",
        late,
        uiURL,
      );
      await guestJoinsMeeting(host, await lateCtx.newPage(), meetingId, late);
      const l = await tileIdByName(host, late);
      await expect(tileOf(host, l)).not.toHaveAttribute("data-pin-rank");
      const mock = (await domOrder(host)).filter((id) => id !== l);
      expect(mock, "two mock tiles beside the late joiner").toHaveLength(2);
      await expect
        .poll(() => domOrder(host), { timeout: 10_000, message: "the late joiner joins last" })
        .toEqual([...mock, l]);
      await expect
        .poll(async () => (await visualOrder(host, [SELF_CELL, ...mock, l])).at(-1))
        .toBe(l);

      await pinOf(host, l).click({ timeout: 10_000 });
      await expectPinState(host, l, late, 0);
      await expect.poll(async () => (await visualOrder(host, [SELF_CELL, ...mock, l]))[0]).toBe(l);
      await expectSiblingSize(host, l, mock);

      expect(await setMockPeers(host, 40), "Mock Peers must be enabled").toBe(true);
      await expect(host.locator(OVERFLOW_BADGE)).toBeVisible({ timeout: 15_000 });
      await expect(host.locator(MOCK_TILES).first()).toBeVisible();
      await expectPinState(host, l, late, 0);
      await expect.poll(async () => (await domOrder(host))[0], { timeout: 10_000 }).toBe(l);
      const overflowMocks = (await domOrder(host)).filter((id) => id !== l).slice(0, 2);
      await expect
        .poll(async () => (await visualOrder(host, [SELF_CELL, ...overflowMocks, l]))[0])
        .toBe(l);
      await expectSiblingSize(host, l, overflowMocks);

      await pinOf(host, l).click({ timeout: 10_000 });
      await expect(
        tileOf(host, l),
        "unpinned, the camera-off late joiner folds into the +N badge",
      ).toHaveCount(0, { timeout: 15_000 });
      await expect(host.locator(OVERFLOW_BADGE)).toBeVisible();
    } finally {
      await Promise.all(browsers.map((br) => br.close().catch(() => undefined)));
    }
  });
});
