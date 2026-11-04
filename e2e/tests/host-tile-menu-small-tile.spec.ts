import { test, expect, chromium, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { enableDiagnosticsTileIndicators } from "../helpers/diagnostics-tile-indicators";
import { waitForServices } from "../helpers/wait-for-services";
import { enterMeetingAsHost, guestJoinsMeeting } from "../helpers/two-user-meeting";

// Issue 3205. The self view takes a grid cell so the lone attendee is not full-bleed.

const HOST = { email: "host-tilemenu-small@videocall.rs", name: "TileMenuHost" };
const GUEST = { email: "guest-tilemenu-small@videocall.rs", name: "TileMenuGuest" };
const SHORT_DESKTOP = { width: 1280, height: 320 };

interface Box {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

interface MenuProbe {
  menu: Box;
  clip: Box;
  rows: number;
  lastRowCentreY: number;
  belowTrigger: boolean;
  besideTrigger: boolean;
  hits: Record<"left" | "right" | "bottom" | "lastRow", string>;
}

function probeTileMenu(tile: Element): MenuProbe {
  const wrapper = tile.querySelector(".tile-mute-menu-wrapper");
  const trigger = wrapper?.querySelector(".tile-mute-btn");
  const menu = wrapper?.querySelector(".tile-context-menu");
  const clip = tile.querySelector(".canvas-container");
  const rows = menu ? Array.from(menu.querySelectorAll(".tile-context-menu-item")) : [];
  if (!wrapper || !trigger || !menu || !clip || rows.length === 0) {
    throw new Error("open tile menu, its trigger, rows or the tile's canvas container missing");
  }
  const box = (el: Element): Box => {
    const r = el.getBoundingClientRect();
    return { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
  };
  const m = box(menu);
  const t = box(trigger);
  const last = box(rows[rows.length - 1]);
  const lastY = (last.top + last.bottom) / 2;
  const midY = (m.top + m.bottom) / 2;
  const hit = (x: number, y: number): string => {
    const el = document.elementFromPoint(x, y);
    if (el && menu.contains(el)) return "menu";
    return el ? `${el.tagName.toLowerCase()}.${el.getAttribute("class") ?? ""}` : "nothing";
  };
  return {
    menu: m,
    clip: box(clip),
    rows: rows.length,
    lastRowCentreY: lastY,
    belowTrigger: m.top >= t.bottom && m.top - t.bottom <= 16,
    besideTrigger: m.left <= t.right && m.right >= t.left,
    hits: {
      left: hit(m.left + 3, midY),
      right: hit(m.right - 3, midY),
      bottom: hit((m.left + m.right) / 2, m.bottom - 3),
      lastRow: hit((last.left + last.right) / 2, lastY),
    },
  };
}

function pinWinsItsOwnDisc(pin: Element): boolean {
  const r = pin.getBoundingClientRect();
  const y = r.top + r.height / 2;
  return [r.left + 1, r.left + r.width / 2].every((x) => {
    const hit = document.elementFromPoint(x, y);
    return hit !== null && pin.contains(hit);
  });
}

async function boxOf(el: Locator): Promise<{ width: number; height: number }> {
  const box = await el.boundingBox();
  if (!box) {
    throw new Error(`${el} has no layout box`);
  }
  return box;
}

async function expectSpansTwoCells(tile: Locator, cell: Locator, grid: Locator): Promise<void> {
  const gap = await grid.evaluate((el) => {
    const cs = getComputedStyle(el);
    return { column: parseFloat(cs.columnGap), row: parseFloat(cs.rowGap) };
  });
  expect(gap.column, "premise: the grid has a column gap").toBeGreaterThan(0);
  expect(gap.row, "premise: the grid has a row gap").toBeGreaterThan(0);
  await expect
    .poll(
      async () => {
        const big = await boxOf(tile);
        const one = await boxOf(cell);
        return Math.max(
          Math.abs(big.width - (2 * one.width + gap.column)),
          Math.abs(big.height - (2 * one.height + gap.row)),
        );
      },
      { timeout: 10_000, message: "the tile must span two cells and a gap" },
    )
    .toBeLessThanOrEqual(1);
}

function peerTile(page: Page, name: string): Locator {
  return page
    .locator('.grid-item[data-tile-root="true"]', {
      has: page.locator("h4.floating-name", { hasText: name }),
    })
    .first();
}

function tileMenu(tile: Locator): Locator {
  return tile.locator(".tile-mute-menu-wrapper > .tile-context-menu");
}

async function openTileMenu(tile: Locator): Promise<Locator> {
  const trigger = tile.locator("button.tile-mute-btn");
  await tile.hover();
  await trigger.click({ timeout: 10_000 });
  await expect(trigger).toHaveAttribute("aria-expanded", "true");
  const menu = tileMenu(tile);
  await expect(menu).toBeVisible({ timeout: 5_000 });
  return menu;
}

async function expectTileMenuClosed(tile: Locator): Promise<void> {
  const trigger = tile.locator("button.tile-mute-btn");
  await expect(tileMenu(tile)).toHaveCount(0, { timeout: 5_000 });
  await expect(trigger).toHaveAttribute("aria-expanded", "false");
  await expect(trigger).not.toHaveAttribute("aria-controls");
}

test.describe("Host tile menu on a small tile", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("issue 3205 small tile host menu: opens whole and on screen, and every close path and Mute still work @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_tilemenu_small_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await createAuthenticatedContext(hostBrowser, HOST.email, HOST.name, uiURL);
      await hostCtx.addInitScript(() => {
        try {
          localStorage.setItem("vc_self_view_placement", "grid");
        } catch {
          /* the self-tile placement assertion below reports it */
        }
      });
      await enableDiagnosticsTileIndicators(hostCtx);
      const guestCtx = await createAuthenticatedContext(
        guestBrowser,
        GUEST.email,
        GUEST.name,
        uiURL,
      );
      await guestCtx.addInitScript(() => {
        localStorage.setItem("vc_prejoin_mic_on", "true");
        localStorage.setItem("vc_prejoin_camera_on", "true");
      });
      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      await enterMeetingAsHost(hostPage, meetingId, HOST.name);
      await guestJoinsMeeting(hostPage, guestPage, meetingId, GUEST.name);

      const tile = peerTile(hostPage, GUEST.name);
      const trigger = tile.locator("button.tile-mute-btn");
      const pin = tile.getByTestId("tile-pin-button");
      const crop = tile.locator("button.crop-icon");
      const signal = tile.getByTestId("peer-signal-indicator");
      await expect(tile).toBeVisible({ timeout: 30_000 });
      await expect(tile.locator("canvas").first()).toBeVisible({ timeout: 45_000 });
      await expect(tile.locator('.audio-indicator[data-mic-muted="false"]')).toHaveCount(1, {
        timeout: 45_000,
      });

      const grid = hostPage.locator("#grid-container");
      const selfTile = hostPage.locator('#grid-container > .host[data-self-placement="grid"]');
      const enlarge = tile.getByTestId("tile-enlarge-button");
      await test.step("issue 3367: on a tile wide enough to offer it, enlarge shows on hover and focus, leaves the pin clear, follows it in Tab order and really grows the tile", async () => {
        await expect(enlarge).toHaveCount(1, { timeout: 10_000 });
        await expect(pin).toHaveCount(1);
        await expect(selfTile).toHaveCount(1, { timeout: 10_000 });
        await tile.hover();
        await expect(enlarge, "hover reveals it").toHaveCSS("opacity", "1");
        const enlargeBox = await enlarge.boundingBox();
        const pinBox = await pin.boundingBox();
        expect(enlargeBox, "the enlarge button must be laid out").not.toBeNull();
        expect(pinBox, "the pin must be laid out").not.toBeNull();
        expect(enlargeBox!.x + enlargeBox!.width).toBeLessThanOrEqual(pinBox!.x);
        expect(await pin.evaluate(pinWinsItsOwnDisc), "enlarge must not cover the pin").toBe(true);

        const menu = await openTileMenu(tile);
        await menu.locator(".tile-context-menu-item").last().focus();
        await hostPage.keyboard.press("Tab");
        await expect(pin).toBeFocused();
        await expectTileMenuClosed(tile);
        await hostPage.mouse.move(0, 0);
        await expect(enlarge, "no hover and no focus hides it").toHaveCSS("opacity", "0");
        await hostPage.keyboard.press("Tab");
        await expect(enlarge).toBeFocused();
        await expect(enlarge, "keyboard focus reveals it").toHaveCSS("opacity", "1");

        const standard = await boxOf(tile);
        await hostPage.keyboard.press("Enter");
        await expect(enlarge).toHaveAttribute("aria-pressed", "true", { timeout: 10_000 });
        await expect(enlarge).toBeFocused();
        await expect(hostPage.getByTestId("ss-detach-announce")).toHaveText(
          `${GUEST.name} enlarged`,
        );
        await expect(grid).toHaveCSS("display", "grid");
        await expectSpansTwoCells(tile, selfTile, grid);
        expect((await boxOf(tile)).width, "the tile really grew").toBeGreaterThan(standard.width);

        await hostPage.keyboard.press("Enter");
        await expect(enlarge).toHaveAttribute("aria-pressed", "false", { timeout: 10_000 });
        await expect(enlarge).toBeFocused();
        await expect(grid).toHaveCSS("display", "flex");
        await expect
          .poll(async () => Math.abs((await boxOf(tile)).width - standard.width), {
            timeout: 10_000,
            message: "restoring returns the tile to its width",
          })
          .toBeLessThanOrEqual(1);
      });

      await hostPage.setViewportSize(SHORT_DESKTOP);
      await expect(selfTile).toHaveCount(1, { timeout: 10_000 });
      await expect(grid).toHaveAttribute("data-tile-flow", "wrap");
      await expect(tile).not.toHaveClass(/\bfull-bleed\b/);
      await expect
        .poll(async () => (await tile.boundingBox())?.height ?? Infinity, { timeout: 10_000 })
        .toBeLessThan(160);
      await expect(pin).toHaveCount(1);
      await expect(enlarge, "a tile this small is not offered enlarge").toHaveCount(0);
      const triggerId = await trigger.getAttribute("id");
      expect(triggerId).toMatch(/^tile-host-actions-.+/);

      await test.step("the menu opens whole, on screen and beside its trigger", async () => {
        const menu = await openTileMenu(tile);
        await expect(menu.locator(".tile-context-menu-item")).not.toHaveCount(0);
        await expect(menu.getByRole("button", { name: "Mute", exact: true })).toBeVisible();
        await expect(menu).toHaveAttribute("id", `${triggerId}-menu`);
        await expect(trigger).toHaveAttribute("aria-controls", `${triggerId}-menu`);

        const opened = await tile.evaluate(probeTileMenu);
        expect(
          opened.lastRowCentreY,
          `the last of ${opened.rows} rows must lie below the tile, or the tile is not small`,
        ).toBeGreaterThan(opened.clip.bottom);

        await expect
          .poll(
            async () => {
              const p = await tile.evaluate(probeTileMenu);
              return { below: p.belowTrigger, beside: p.besideTrigger, hits: p.hits };
            },
            { timeout: 5_000, message: "menu must sit under its trigger with every edge on top" },
          )
          .toEqual({
            below: true,
            beside: true,
            hits: { left: "menu", right: "menu", bottom: "menu", lastRow: "menu" },
          });

        const box = await menu.boundingBox();
        expect(box, "the open menu must be laid out").not.toBeNull();
        expect(box!.x).toBeGreaterThanOrEqual(0);
        expect(box!.y).toBeGreaterThanOrEqual(0);
        expect(box!.x + box!.width).toBeLessThanOrEqual(SHORT_DESKTOP.width);
        expect(box!.y + box!.height).toBeLessThanOrEqual(SHORT_DESKTOP.height);
      });

      await test.step("a click on the menu's own padding keeps it open", async () => {
        const menu = tileMenu(tile);
        const box = await menu.boundingBox();
        expect(box, "the open menu must be laid out").not.toBeNull();
        await menu.click({ position: { x: box!.width / 2, y: 2 } });
        await hostPage.evaluate(
          () =>
            new Promise<void>((r) => requestAnimationFrame(() => requestAnimationFrame(() => r()))),
        );
        await expect(menu).toHaveCount(1);
        await expect(trigger).toHaveAttribute("aria-expanded", "true");
      });

      await test.step("off the tile the trigger stays shown, and an outside click closes it", async () => {
        const footer = await hostPage.getByTestId("meeting-footer").boundingBox();
        expect(footer, "the meeting footer must be laid out").not.toBeNull();
        const off = { x: footer!.x + footer!.width / 2, y: footer!.y + footer!.height / 2 };
        const offTile = await hostPage.evaluate(({ x, y }) => {
          const hit = document.elementFromPoint(x, y);
          if (!hit) throw new Error(`nothing at ${x},${y}`);
          return hit.closest(".meeting-footer") !== null && hit.closest(".grid-item") === null;
        }, off);
        expect(offTile, "the pointer must rest on the footer, outside every tile").toBe(true);
        await hostPage.mouse.move(off.x, off.y);
        await expect(trigger).toBeVisible();

        const tileBox = await tile.boundingBox();
        const selfBox = await selfTile.boundingBox();
        expect(tileBox, "the attendee tile must be laid out").not.toBeNull();
        expect(selfBox, "the self tile must be laid out").not.toBeNull();
        const outside = {
          x: Math.min(tileBox!.x, selfBox!.x) / 2,
          y: tileBox!.y + tileBox!.height / 2,
        };
        await hostPage.mouse.click(outside.x, outside.y);
        await expectTileMenuClosed(tile);
      });

      await test.step("Escape from a keyboard-focused row closes it and refocuses the trigger", async () => {
        await tile.hover();
        await trigger.focus();
        await hostPage.keyboard.press("Enter");
        const menu = tileMenu(tile);
        await expect(menu).toBeVisible({ timeout: 5_000 });
        await hostPage.keyboard.press("Tab");
        await expect(menu.locator(".tile-context-menu-item").first()).toBeFocused();
        await hostPage.keyboard.press("Escape");
        await expectTileMenuClosed(tile);
        await expect(trigger).toBeFocused();
      });

      await test.step("tabbing out of the last row closes it", async () => {
        const menu = await openTileMenu(tile);
        await menu.locator(".tile-context-menu-item").last().focus();
        await expect(menu).toBeVisible();
        await hostPage.keyboard.press("Tab");
        await expect(pin).toBeFocused();
        await expectTileMenuClosed(tile);
      });

      await test.step("the tile's pin button closes it and pins", async () => {
        await expect(pin).toHaveAttribute("aria-pressed", "false");
        await openTileMenu(tile);
        await pin.click({ timeout: 10_000 });
        await expectTileMenuClosed(tile);
        await expect(pin).toHaveAttribute("aria-pressed", "true");
        await pin.click({ timeout: 10_000 });
        await expect(pin).toHaveAttribute("aria-pressed", "false");
      });

      await test.step("the tile's crop button closes it and toggles the crop", async () => {
        await tile.hover();
        const wasActive = await crop.evaluate((el) => el.classList.contains("active"));
        await openTileMenu(tile);
        await crop.click({ timeout: 10_000 });
        await expectTileMenuClosed(tile);
        if (wasActive) {
          await expect(crop).not.toHaveClass(/\bactive\b/);
        } else {
          await expect(crop).toHaveClass(/\bactive\b/);
        }
      });

      await test.step("Mute closes it and mutes the attendee", async () => {
        const menu = await openTileMenu(tile);
        await menu.getByRole("button", { name: "Mute", exact: true }).click({ timeout: 10_000 });
        await expectTileMenuClosed(tile);
        await expect(tile.locator('.audio-indicator[data-mic-muted="true"]')).toHaveCount(1, {
          timeout: 10_000,
        });
        await expect(guestPage.getByTestId("host-mute-notice")).toBeVisible({ timeout: 15_000 });
      });

      await test.step("the tile's signal button closes it and opens the signal popup", async () => {
        const popup = hostPage.locator(".signal-quality-popup");
        await expect(popup).toHaveCount(0);
        await openTileMenu(tile);
        await signal.click({ timeout: 10_000 });
        await expectTileMenuClosed(tile);
        await expect(popup).toBeVisible({ timeout: 10_000 });
      });
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});
