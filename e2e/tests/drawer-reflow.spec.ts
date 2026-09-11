import { test, expect, chromium, Browser, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { joinMeetingFromPage } from "../helpers/two-user-meeting";
import {
  MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT,
  admitGuestIfNeeded,
  clickJoinAndEnterGrid,
  joinMeetingAs,
  startScreenShare,
} from "../helpers/screen-share-meeting";

/**
 * Side drawers carve horizontal space out of the tile grid (#2701, #2272): the
 * peer list (left), chat (right, inboard) and diagnostics (right, at the edge)
 * reserve their painted width from `#grid-container`, and reserve nothing below
 * 568px. The px here are acceptance-checklist literals, never re-derived from
 * `drawer_reserves`. On 095fc050 the grid measured 0..1280 in every state, chat
 * sat at 920..1280 inside diagnostics at 720..1280, diagnostics painted 560 not
 * the capped 408, and `.grid-item.full-bleed` was `fixed; height: 100vh`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

const DESKTOP = { width: 1280, height: 720 };
const MOBILE = { width: 375, height: 667 };
const NARROW = { width: 600, height: 700 };
const MEDIUM = { width: 900, height: 700 };

const EPS = 1;

interface Rect {
  left: number;
  right: number;
  top: number;
  bottom: number;
  width: number;
  height: number;
}

const GRID = "#grid-container";
const PEER_LIST = "#peer-list-container";
const DIAGNOSTICS = "#diagnostics-sidebar";
const CHAT = "#chat-sidebar";
const ACTION_BAR = ".video-controls-container";

async function rects(page: Page, selectors: string[]): Promise<Record<string, Rect | null>> {
  return page.evaluate((sels: string[]) => {
    const flat: Record<string, Rect | null> = {};
    for (const sel of sels) {
      const el = document.querySelector(sel);
      if (!el) {
        flat[sel] = null;
        continue;
      }
      const r = el.getBoundingClientRect();
      flat[sel] = {
        left: r.left,
        right: r.right,
        top: r.top,
        bottom: r.bottom,
        width: r.width,
        height: r.height,
      };
    }
    return flat;
  }, selectors);
}

async function rectOf(page: Page, selector: string): Promise<Rect> {
  const r = (await rects(page, [selector]))[selector];
  if (!r) throw new Error(`no element matched ${selector}`);
  return r;
}

async function allRects(page: Page, selector: string): Promise<Rect[]> {
  return page.evaluate((sel: string) => {
    return Array.from(document.querySelectorAll(sel)).map((el): Rect => {
      const r = el.getBoundingClientRect();
      return {
        left: r.left,
        right: r.right,
        top: r.top,
        bottom: r.bottom,
        width: r.width,
        height: r.height,
      };
    });
  }, selector);
}

/** Polls, so chat's 250ms transform slide settles instead of being raced. */
async function expectSpan(
  page: Page,
  selector: string,
  left: number,
  right: number,
  what: string,
): Promise<void> {
  await expect
    .poll(
      async () => {
        const r = (await rects(page, [selector]))[selector];
        return r ? `${Math.round(r.left)}..${Math.round(r.right)}` : "missing";
      },
      { timeout: 10_000, message: `${what} (${selector}) must span ${left}..${right}` },
    )
    .toBe(`${left}..${right}`);
}

function intersectionArea(a: Rect, b: Rect): number {
  const w = Math.min(a.right, b.right) - Math.max(a.left, b.left);
  const h = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
  return w > EPS && h > EPS ? w * h : 0;
}

/** The core #2701 assertion; the count is checked first so it cannot pass vacuously. */
async function expectTilesClearOf(page: Page, openDrawers: string[], what: string): Promise<void> {
  const tiles = [...(await allRects(page, ".grid-item")), ...(await allRects(page, ".host"))];
  expect(tiles.length, `${what}: expected at least one tile rect to check`).toBeGreaterThan(0);

  const drawerRects = await rects(page, openDrawers);
  for (const sel of openDrawers) {
    const drawer = drawerRects[sel];
    expect(drawer, `${what}: open drawer ${sel} must be present`).not.toBeNull();
    for (const [i, tile] of tiles.entries()) {
      expect(
        intersectionArea(tile, drawer as Rect),
        `${what}: tile #${i} ${JSON.stringify(tile)} overlaps ${sel} ${JSON.stringify(drawer)}`,
      ).toBe(0);
    }
  }
}

async function joinSoloMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await fillAndSubmitJoinForm(page, meetingId, username);
  await page.waitForTimeout(1000);
  const result = await joinMeetingFromPage(page);
  expect(result).toBe("in-meeting");
  await expect(page.locator(GRID)).toBeVisible({ timeout: 15_000 });
}

/** `wakeControls` nudges a fixed (400, 400), outside the 375px viewport. */
async function wakeBar(page: Page): Promise<void> {
  const vp = page.viewportSize() ?? DESKTOP;
  await page.mouse.move(Math.floor(vp.width / 2), Math.floor(vp.height / 2));
  await page.waitForTimeout(300);
}

/** Activate an action-bar slot, via "More actions" when the band is too narrow
 *  for it inline. `synthetic` dispatches the click at the element, needed only
 *  while a full-screen mobile drawer legitimately covers the bar. */
async function activateSlot(
  page: Page,
  directButton: Locator,
  overflowLabel: string,
  synthetic = false,
): Promise<void> {
  await wakeBar(page);

  const direct = directButton.first();
  if (!synthetic && (await direct.count()) > 0 && (await direct.isVisible())) {
    await direct.click({ timeout: 10_000 });
    return;
  }

  const trigger = page.locator("#overflow-menu-trigger");
  await expect(
    trigger,
    `${overflowLabel} is not inline, so the overflow trigger must be present`,
  ).toBeAttached({ timeout: 10_000 });
  if (synthetic) {
    await trigger.dispatchEvent("click");
  } else {
    await trigger.click({ timeout: 10_000 });
  }

  const item = page.locator(".action-bar-overflow-popover button.overflow-item", {
    has: page.locator(`span:text-is("${overflowLabel}")`),
  });
  await expect(item).toBeAttached({ timeout: 10_000 });
  if (synthetic) {
    await item.dispatchEvent("click");
  } else {
    await item.click({ timeout: 10_000 });
  }
}

async function openPeerList(page: Page, synthetic = false): Promise<void> {
  await activateSlot(
    page,
    page.locator('[data-testid="peer-list-button"]'),
    "Participants",
    synthetic,
  );
  await expect(page.locator(PEER_LIST)).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
}

async function closePeerList(page: Page): Promise<void> {
  await page.locator(`${PEER_LIST} button.close-button`).click({ timeout: 10_000 });
  await expect(page.locator(PEER_LIST)).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
}

function diagnosticsButton(page: Page): Locator {
  return page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
}

async function openDiagnostics(page: Page, synthetic = false): Promise<void> {
  await activateSlot(page, diagnosticsButton(page), "Diagnostics", synthetic);
  await expect(page.locator(DIAGNOSTICS)).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
}

/** Its trigger is painted over, and Escape needs focus in `#main-container`. */
async function closeDiagnostics(page: Page): Promise<void> {
  await page.locator(`${DIAGNOSTICS} button.close-button`).click({ timeout: 10_000 });
  await expect(page.locator(DIAGNOSTICS)).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
}

/** The public build strips the chat integration, so `#chat-sidebar` never
 *  mounts. Keyed on the sidebar, not its action-bar button: the button is also
 *  absent when a narrow band sheds it into the overflow menu. */
const NO_CHAT = "chat integration is not part of the public build";

function chatIsBuiltIn(page: Page): Promise<boolean> {
  return page
    .locator(CHAT)
    .count()
    .then((n) => n > 0);
}

function chatButton(page: Page): Locator {
  return page.locator(".video-controls-container").getByRole("button", {
    name: "Chat",
    exact: true,
  });
}

async function openChat(page: Page, synthetic = false): Promise<void> {
  await activateSlot(page, chatButton(page), "Chat", synthetic);
  await expect(page.locator(CHAT)).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
}

async function closeChat(page: Page): Promise<void> {
  await page.getByRole("button", { name: "Close chat" }).click({ timeout: 10_000 });
  await expect(page.locator(CHAT)).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
}

async function addMockPeers(page: Page, count: number): Promise<void> {
  await wakeBar(page);
  const mockButton = page.locator("button.video-control-button", {
    has: page.locator(".tooltip", { hasText: /Mock Peers/i }),
  });
  await expect(
    mockButton,
    "Mock Peers control must be present (MOCK_PEERS_ENABLED=true in docker/docker-compose.e2e.yaml)",
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

async function newSoloPage(
  browser: Browser,
  uiURL: string,
  email: string,
  name: string,
  viewport: { width: number; height: number },
): Promise<Page> {
  const ctx = await createAuthenticatedContext(browser, email, name, uiURL);
  const page = await ctx.newPage();
  await page.setViewportSize(viewport);
  return page;
}

test.describe("Drawers reflow the tile grid (#2701 / #2272)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("chat and diagnostics open together sit side by side and leave the grid 512px @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-2272@videocall.rs",
        "Reflow2272",
        DESKTOP,
      );
      await joinSoloMeeting(page, `e2e_reflow_2272_${Date.now()}`, "Reflow2272");
      test.skip(!(await chatIsBuiltIn(page)), NO_CHAT);
      await addMockPeers(page, 6);

      await openChat(page);
      await expectSpan(page, GRID, 0, 920, "grid with chat only");
      await expectSpan(page, CHAT, 920, 1280, "chat alone");

      // Diagnostics opened ON TOP of an already-open chat: #2272's exact order.
      await openDiagnostics(page);
      await expectSpan(page, DIAGNOSTICS, 872, 1280, "diagnostics beside chat");
      await expectSpan(page, CHAT, 512, 872, "chat inboard of diagnostics");
      await expectSpan(page, GRID, 0, 512, "grid with chat + diagnostics");

      const both = await rects(page, [CHAT, DIAGNOSTICS]);
      expect(intersectionArea(both[CHAT] as Rect, both[DIAGNOSTICS] as Rect)).toBe(0);

      await expectTilesClearOf(page, [CHAT, DIAGNOSTICS], "chat + diagnostics");
    } finally {
      await browser.close();
    }
  });

  test("each drawer carves exactly its own width out of the grid, on its own side", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-single@videocall.rs",
        "ReflowSingle",
        DESKTOP,
      );
      await joinSoloMeeting(page, `e2e_reflow_single_${Date.now()}`, "ReflowSingle");
      await addMockPeers(page, 6);

      await expectSpan(page, GRID, 0, 1280, "grid with no drawer open");

      await openPeerList(page);
      await expectSpan(page, PEER_LIST, 0, 320, "peer list");
      await expectSpan(page, GRID, 320, 1280, "grid with the peer list open");
      await expectTilesClearOf(page, [PEER_LIST], "peer list only");
      for (const tile of await allRects(page, `${GRID} .grid-item`)) {
        expect(tile.left).toBeGreaterThanOrEqual(340 - EPS);
      }
      await closePeerList(page);
      await expectSpan(page, GRID, 0, 1280, "grid after closing the peer list");

      // Alone, so the 768 budget does not bind and diagnostics paints its 560.
      await openDiagnostics(page);
      await expectSpan(page, DIAGNOSTICS, 720, 1280, "diagnostics alone");
      await expectSpan(page, GRID, 0, 720, "grid with diagnostics open");
      await expectTilesClearOf(page, [DIAGNOSTICS], "diagnostics only");
      for (const tile of await allRects(page, `${GRID} .grid-item`)) {
        expect(tile.right).toBeLessThanOrEqual(700 + EPS);
      }
      await closeDiagnostics(page);
      await expectSpan(page, GRID, 0, 1280, "grid after closing diagnostics");

      // Guarded rather than skipping the whole test: the peer-list and
      // diagnostics halves above are build-independent and worth keeping.
      if (await chatIsBuiltIn(page)) {
        await openChat(page);
        await expectSpan(page, CHAT, 920, 1280, "chat alone");
        await expectSpan(page, GRID, 0, 920, "grid with chat open");
        await expectTilesClearOf(page, [CHAT], "chat only");
        await closeChat(page);
        await expectSpan(page, GRID, 0, 1280, "grid after closing chat");
      }
    } finally {
      await browser.close();
    }
  });

  test("all three drawers open land on their floors and keep a tile band between them", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-all3@videocall.rs",
        "ReflowAll3",
        DESKTOP,
      );
      await joinSoloMeeting(page, `e2e_reflow_all3_${Date.now()}`, "ReflowAll3");
      test.skip(!(await chatIsBuiltIn(page)), NO_CHAT);
      await addMockPeers(page, 4);

      await openPeerList(page);
      await openChat(page);
      await openDiagnostics(page);

      // 320 + 560 + 360 against a 768 budget, so all three land on their floors.
      await expectSpan(page, PEER_LIST, 0, 300, "peer list at its floor");
      await expectSpan(page, GRID, 300, 620, "grid between all three drawers");
      await expectSpan(page, CHAT, 620, 980, "chat at its fixed width");
      await expectSpan(page, DIAGNOSTICS, 980, 1280, "diagnostics at its floor");

      const open = await rects(page, [PEER_LIST, CHAT, DIAGNOSTICS]);
      expect(intersectionArea(open[CHAT] as Rect, open[DIAGNOSTICS] as Rect)).toBe(0);
      expect(intersectionArea(open[CHAT] as Rect, open[PEER_LIST] as Rect)).toBe(0);
      await expectTilesClearOf(page, [PEER_LIST, CHAT, DIAGNOSTICS], "all three open");
    } finally {
      await browser.close();
    }
  });

  test("closing the drawers restores the grid, and the capped diagnostics width is never persisted", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "reflow-restore@videocall.rs",
        "ReflowRestore",
        uiURL,
      );
      // Seeded, so "the dragged 560 came back" is a real claim, not a null read.
      await ctx.addInitScript(() => {
        try {
          localStorage.setItem("vc_drawer_right_width", "560");
        } catch {
          /* storage unavailable — the assertion below reports it */
        }
      });
      const page = await ctx.newPage();
      await page.setViewportSize(DESKTOP);
      await joinSoloMeeting(page, `e2e_reflow_restore_${Date.now()}`, "ReflowRestore");
      test.skip(!(await chatIsBuiltIn(page)), NO_CHAT);

      await openChat(page);
      await openDiagnostics(page);
      await expectSpan(page, DIAGNOSTICS, 872, 1280, "diagnostics capped beside chat");
      expect(
        await page.evaluate(() => localStorage.getItem("vc_drawer_right_width")),
        "the 408px render width must never be written back over the dragged 560",
      ).toBe("560");

      await closeDiagnostics(page);
      await closeChat(page);
      await expectSpan(page, GRID, 0, 1280, "grid after closing every drawer");

      await openDiagnostics(page);
      await expectSpan(page, DIAGNOSTICS, 720, 1280, "diagnostics back at its dragged 560");
    } finally {
      await browser.close();
    }
  });

  test("the action bar re-centres in the free band between the drawers", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    const centreX = async (page: Page): Promise<number> => {
      const r = await rectOf(page, ACTION_BAR);
      return Math.round((r.left + r.right) / 2);
    };

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-bar@videocall.rs",
        "ReflowBar",
        DESKTOP,
      );
      await joinSoloMeeting(page, `e2e_reflow_bar_${Date.now()}`, "ReflowBar");

      await wakeBar(page);
      await expect.poll(() => centreX(page), { timeout: 10_000 }).toBe(640);

      await openPeerList(page);
      await expect.poll(() => centreX(page), { timeout: 10_000, message: "640 + 320/2" }).toBe(800);
      await closePeerList(page);

      // The 640 and 800 cases above need no chat, so they stay public.
      if (await chatIsBuiltIn(page)) {
        await openChat(page);
        await openDiagnostics(page);
        await expect
          .poll(() => centreX(page), { timeout: 10_000, message: "640 - 768/2" })
          .toBe(256);

        await openPeerList(page);
        await expect
          .poll(() => centreX(page), { timeout: 10_000, message: "640 + (300 - 660)/2" })
          .toBe(460);
      }
    } finally {
      await browser.close();
    }
  });

  test("narrow desktop (600px): chat is a full-width sheet the peer list evicts", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-narrow@videocall.rs",
        "ReflowNarrow",
        NARROW,
      );
      await joinSoloMeeting(page, `e2e_reflow_narrow_${Date.now()}`, "ReflowNarrow");
      test.skip(!(await chatIsBuiltIn(page)), NO_CHAT);

      // A sheet reserves nothing, so the grid keeps the whole viewport.
      await openChat(page);
      await expectSpan(page, CHAT, 0, 600, "chat as a full-width sheet");
      await expectSpan(page, GRID, 0, 600, "grid behind the chat sheet");

      // The sheet covers the action bar, so the trigger needs a dispatched click.
      await openPeerList(page, true);
      await expect(page.locator(CHAT)).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expectSpan(page, PEER_LIST, 0, 300, "peer list at its floor");
      await expectSpan(page, GRID, 300, 600, "grid beside the peer list");
    } finally {
      await browser.close();
    }
  });

  // At 900 any two drawers' floors (>= 600) leave under the 320 tile band, so
  // each opener evicts the one already there. On 095fc050 nothing ever closed
  // and all three stacked as overlays.
  test("medium desktop (900px): each opener evicts the drawer already open", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-medium@videocall.rs",
        "ReflowMedium",
        MEDIUM,
      );
      await joinSoloMeeting(page, `e2e_reflow_medium_${Date.now()}`, "ReflowMedium");
      test.skip(!(await chatIsBuiltIn(page)), NO_CHAT);

      // Alone, but still capped: 560 exceeds `max_total_reserve(900) = 500`.
      await openDiagnostics(page);
      await expectSpan(page, DIAGNOSTICS, 400, 900, "diagnostics capped at 900");
      await expectSpan(page, GRID, 0, 400, "grid with diagnostics only at 900");

      // Chat is NOT a sheet at 900: it reserves its 360 and evicts diagnostics,
      // whose floors together (660) leave only a 240px band.
      await openChat(page);
      await expect(page.locator(DIAGNOSTICS)).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expectSpan(page, CHAT, 540, 900, "chat beside the tiles at 900");
      await expectSpan(page, GRID, 0, 540, "grid with chat only at 900");

      // 320, not the 300 floor: the floors decide WHETHER chat closes, but once
      // it has, the peer list alone fits inside 500 and nothing shrinks it.
      await openPeerList(page);
      await expect(page.locator(CHAT)).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expectSpan(page, PEER_LIST, 0, 320, "peer list at its persisted width");
      await expectSpan(page, GRID, 320, 900, "grid with the peer list alone");
    } finally {
      await browser.close();
    }
  });

  // Shrinking the window sheds drawers in the order diagnostics -> peer list ->
  // chat, and says so once in the toast live region. On 095fc050 a resize closed
  // nothing and the notice did not exist.
  test("resizing into a narrow window sheds diagnostics and announces it", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-resize@videocall.rs",
        "ReflowResize",
        DESKTOP,
      );
      await joinSoloMeeting(page, `e2e_reflow_resize_${Date.now()}`, "ReflowResize");
      test.skip(!(await chatIsBuiltIn(page)), NO_CHAT);

      await openChat(page);
      await openDiagnostics(page);
      await expectSpan(page, GRID, 0, 512, "grid with chat + diagnostics at 1280");

      await page.setViewportSize({ width: 640, height: 720 });

      await expect(page.locator(DIAGNOSTICS)).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expect(page.locator(CHAT)).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expectSpan(page, CHAT, 0, 640, "chat as a sheet after the resize");
      await expectSpan(page, GRID, 0, 640, "grid after the resize");

      const notice = page.locator('[data-testid="drawer-resize-notice"]');
      await expect(notice).toBeVisible({ timeout: 10_000 });
      await expect(notice).toHaveText(/Diagnostics closed/, { timeout: 10_000 });
    } finally {
      await browser.close();
    }
  });

  test("mobile (375px): drawers stay full-screen overlays and never reflow the grid", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-mobile@videocall.rs",
        "ReflowMobile",
        MOBILE,
      );
      await joinSoloMeeting(page, `e2e_reflow_mobile_${Date.now()}`, "ReflowMobile");

      // Reserves are hard zero here, so tile disjointness is unsatisfiable by
      // design and is deliberately not asserted.
      await expectSpan(page, GRID, 0, 375, "grid with no drawer open (mobile)");

      await openPeerList(page);
      await expectSpan(page, PEER_LIST, 0, 375, "peer list covers the viewport");
      await expectSpan(page, GRID, 0, 375, "grid with the peer list open (mobile)");
      await closePeerList(page);

      if (await chatIsBuiltIn(page)) {
        await openChat(page);
        await expectSpan(page, CHAT, 0, 375, "chat covers the viewport");
        await expectSpan(page, GRID, 0, 375, "grid with chat open (mobile)");
        await closeChat(page);
      }

      await openDiagnostics(page);
      await expectSpan(page, GRID, 0, 375, "grid with diagnostics open (mobile)");
    } finally {
      await browser.close();
    }
  });

  test("mobile (375px): opening chat closes diagnostics and opening diagnostics closes chat", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-pair@videocall.rs",
        "ReflowPair",
        MOBILE,
      );
      await joinSoloMeeting(page, `e2e_reflow_pair_${Date.now()}`, "ReflowPair");
      test.skip(!(await chatIsBuiltIn(page)), NO_CHAT);

      const chat = page.locator(CHAT);
      const diagnostics = page.locator(DIAGNOSTICS);

      await openChat(page);
      await openDiagnostics(page, true);
      await expect(chat).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });

      await closeDiagnostics(page);

      await openDiagnostics(page);
      await openChat(page, true);
      await expect(diagnostics).not.toHaveClass(/\bvisible\b/, { timeout: 10_000 });
      await expect(chat).toHaveClass(/\bvisible\b/, { timeout: 10_000 });
    } finally {
      await browser.close();
    }
  });

  test("the lone remote tile in a 2-peer meeting fills the reserved band, and so does it when pinned", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser,
        "reflow-fb-host@videocall.rs",
        "ReflowFbHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser,
        "reflow-fb-guest@videocall.rs",
        "ReflowFbGuest",
        uiURL,
      );
      const meetingId = `e2e_reflow_fullbleed_${Date.now()}`;

      const hostPage = await joinMeetingAs(hostCtx, meetingId, "ReflowFbHost");
      await hostPage.setViewportSize(DESKTOP);
      await clickJoinAndEnterGrid(hostPage);

      const guestPage = await joinMeetingAs(guestCtx, meetingId, "ReflowFbGuest");
      await guestPage.setViewportSize(DESKTOP);
      await admitGuestIfNeeded(hostPage, guestPage);

      const fullBleed = hostPage.locator(`${GRID} .grid-item.full-bleed`);
      await expect(fullBleed).toHaveCount(1, { timeout: 30_000 });

      // Baseline, not the regression check: the old `fixed` rule satisfied it.
      let grid = await rectOf(hostPage, GRID);
      let tile = await rectOf(hostPage, `${GRID} .grid-item.full-bleed`);
      expect(Math.round(tile.left)).toBe(Math.round(grid.left));
      expect(Math.round(tile.right)).toBe(Math.round(grid.right));

      await openPeerList(hostPage);
      await expectSpan(hostPage, GRID, 320, 1280, "grid with the peer list open");
      await expect
        .poll(
          async () => Math.round((await rectOf(hostPage, `${GRID} .grid-item.full-bleed`)).left),
          { timeout: 10_000 },
        )
        .toBe(320);
      grid = await rectOf(hostPage, GRID);
      tile = await rectOf(hostPage, `${GRID} .grid-item.full-bleed`);
      expect(Math.round(tile.right)).toBe(Math.round(grid.right));
      const drawer = await rectOf(hostPage, PEER_LIST);
      expect(intersectionArea(tile, drawer)).toBe(0);

      // Pinning the LONE tile leaves it `grid-item full-bleed grid-item-pinned`,
      // and `.grid-item.full-bleed` (0,2,0) outranks `.grid-item-pinned` (0,1,0),
      // so what this re-asserts is that the full-bleed rule still governs once
      // the pin class lands. The `position: fixed` + reserve-var path that
      // `.grid-item-pinned` owns alone needs a 3+ tile meeting and is NOT
      // covered here. The pin control is `visibility: hidden` until hover, hence
      // `force`, and it needs a REAL peer: on a mock tile the class never appears.
      await hostPage.locator(`${GRID} .grid-item`).first().hover();
      await hostPage.waitForTimeout(400);
      await hostPage.locator(`${GRID} .grid-item button.pin-icon`).first().click({ force: true });
      await expect(hostPage.locator(`${GRID} .grid-item-pinned`)).toHaveCount(1, {
        timeout: 10_000,
      });
      await expect
        .poll(async () => Math.round((await rectOf(hostPage, `${GRID} .grid-item-pinned`)).left), {
          timeout: 10_000,
          message: "the pinned tile must start at the peer list's inner edge",
        })
        .toBeGreaterThanOrEqual(320);
      expect(intersectionArea(await rectOf(hostPage, `${GRID} .grid-item-pinned`), drawer)).toBe(0);
    } finally {
      await browser.close();
    }
  });

  test("screen-share split panes stay clear of an open drawer", async ({ baseURL }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser,
        "reflow-ss-host@videocall.rs",
        "ReflowSsHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser,
        "reflow-ss-guest@videocall.rs",
        "ReflowSsGuest",
        uiURL,
      );
      await guestCtx.addInitScript(MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT);
      const meetingId = `e2e_reflow_ss_${Date.now()}`;

      const hostPage = await joinMeetingAs(hostCtx, meetingId, "ReflowSsHost");
      await hostPage.setViewportSize(DESKTOP);
      await clickJoinAndEnterGrid(hostPage);

      const guestPage = await joinMeetingAs(guestCtx, meetingId, "ReflowSsGuest");
      await guestPage.setViewportSize(DESKTOP);
      await admitGuestIfNeeded(hostPage, guestPage);

      const shared = await startScreenShare(guestPage, hostPage);
      expect(shared, "the guest's mocked screen share must reach the host").toBe(true);

      await openDiagnostics(hostPage);
      await expectSpan(hostPage, GRID, 0, 720, "split container with diagnostics open");

      const panes = await allRects(hostPage, `${GRID} > div`);
      expect(panes.length, "the split layout must render its panes").toBeGreaterThanOrEqual(3);

      const drawer = await rectOf(hostPage, DIAGNOSTICS);
      for (const [i, pane] of panes.entries()) {
        expect(
          intersectionArea(pane, drawer),
          `split pane #${i} overlaps the diagnostics drawer`,
        ).toBe(0);
      }
    } finally {
      await browser.close();
    }
  });

  // The action-bar popovers are `position: fixed` siblings of the dock and must
  // track its band centre, not the viewport's. Reactions is the first secondary
  // slot, so it stays inline in this state while later slots are shed. The dock
  // centre is read live rather than hardcoded, so this holds whether the fix
  // anchors the popover to the band or to its own trigger. On bd7663d3
  // `.reactions-palette` is `left: 50%`, i.e. 640 against the dock's 800.
  test("an action-bar popover follows the dock into the free band", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-popover@videocall.rs",
        "ReflowPopover",
        DESKTOP,
      );
      await joinSoloMeeting(page, `e2e_reflow_popover_${Date.now()}`, "ReflowPopover");
      await openPeerList(page);

      await wakeBar(page);
      const trigger = page.locator("#reactions-trigger");
      await expect(
        trigger,
        "reactions is the first secondary slot, so it must stay inline at a 960px band",
      ).toBeVisible({ timeout: 10_000 });
      await trigger.click({ timeout: 10_000 });

      const palette = page.locator(".reactions-palette");
      await expect(palette).toBeVisible({ timeout: 10_000 });

      const measured = await rects(page, [".reactions-palette", ACTION_BAR, PEER_LIST]);
      const paletteRect = measured[".reactions-palette"] as Rect;
      const barRect = measured[ACTION_BAR] as Rect;
      const centre = (r: Rect) => (r.left + r.right) / 2;

      expect(Math.round(centre(barRect)), "the dock's band centre at 1280 + 320").toBe(800);
      expect(Math.abs(centre(paletteRect) - centre(barRect))).toBeLessThanOrEqual(8);
      expect(intersectionArea(paletteRect, measured[PEER_LIST] as Rect)).toBe(0);
    } finally {
      await browser.close();
    }
  });

  test("chat-sheet composer clears the action bar at 640px", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(browser, uiURL, "reflow-composer@videocall.rs", "ReflowComp", {
        width: 640,
        height: 720,
      });
      await joinSoloMeeting(page, `e2e_reflow_composer_${Date.now()}`, "ReflowComp");
      test.skip(!(await chatIsBuiltIn(page)), NO_CHAT);

      await openChat(page);
      await expectSpan(page, CHAT, 0, 640, "chat as a sheet at 640");

      await wakeBar(page);
      // The CONTROLS, not `.chat-input-area`'s box: the clearance is delivered
      // by padding-bottom on that box, which grows it downward to the viewport
      // edge rather than lifting its bottom. Its rect therefore still ends at
      // 720 with the fix in, while the input and send button are what a user
      // has to reach. Pre-fix both sat inside the dock's 633.76..720 band.
      const measured = await rects(page, [".chat-input", ".chat-send-button", ACTION_BAR]);
      const barTop = (measured[ACTION_BAR] as Rect).top;
      for (const sel of [".chat-input", ".chat-send-button"]) {
        const control = measured[sel];
        expect(control, `the sheet must render ${sel}`).not.toBeNull();
        expect((control as Rect).bottom, `${sel} must clear the dock`).toBeLessThanOrEqual(barTop);
      }
    } finally {
      await browser.close();
    }
  });

  // Two bugs, one each side of the fix: on 095fc050 the bar was centred at 640
  // and overlapped the peer list by 19,353 px^2; on a62abd58 the recentred bar
  // still budgeted the raw viewport and hung off-screen at 234..1366.
  test("the action bar stays inside the viewport and clear of every open drawer", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newSoloPage(
        browser,
        uiURL,
        "reflow-barfit@videocall.rs",
        "ReflowBarFit",
        DESKTOP,
      );
      await joinSoloMeeting(page, `e2e_reflow_barfit_${Date.now()}`, "ReflowBarFit");

      await openPeerList(page);
      let bar = await rectOf(page, ACTION_BAR);
      expect(bar.left, "action bar clipped off the left edge").toBeGreaterThanOrEqual(-EPS);
      expect(bar.right, "action bar clipped off the right edge").toBeLessThanOrEqual(
        DESKTOP.width + EPS,
      );
      expect(intersectionArea(bar, await rectOf(page, PEER_LIST))).toBe(0);
      await closePeerList(page);

      // The peer-list half above is build-independent and stays public.
      if (await chatIsBuiltIn(page)) {
        await openChat(page);
        await openDiagnostics(page);
        bar = await rectOf(page, ACTION_BAR);
        expect(bar.left, "action bar clipped off the left edge").toBeGreaterThanOrEqual(-EPS);
        expect(bar.right, "action bar clipped off the right edge").toBeLessThanOrEqual(
          DESKTOP.width + EPS,
        );
        const open = await rects(page, [CHAT, DIAGNOSTICS]);
        expect(intersectionArea(bar, open[CHAT] as Rect)).toBe(0);
        expect(intersectionArea(bar, open[DIAGNOSTICS] as Rect)).toBe(0);
      }
    } finally {
      await browser.close();
    }
  });
});
