import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import {
  MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT,
  startScreenShare,
} from "../helpers/screen-share-meeting";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 66 — where the local user's own tile is drawn, and whether that choice
 * survives a reload.
 */

const SELF_NAV = "#host-controls-nav";
const HIDE_BTN = "self-view-hide-button";
const PLACEMENT_BTN = "self-view-placement-toggle";
const PLACEMENT_KEY = "vc_self_view_placement";
const VISIBLE_KEY = "vc_self_view_visible";
/** Issue 2693: the stand-in the hidden self view leaves behind, and its one control. */
const SHOW_BTN = "self-view-show-button";
const HIDDEN_HINT_ID = "self-view-hidden-hint";
const SHOW_ICON = `button.video-control-button[data-testid="${SHOW_BTN}"]`;
const TOAST = '.peer-toast.self-view-hidden-toast[role="status"]';
const TOAST_CLOSE = 'button.toast-close-btn[data-testid="self-view-toast-close"]';
const SCOPE_TEXT = "Only affects your view — others still see you.";
/** The action bar. `position: fixed` at the viewport bottom, so its box is comparable. */
const DOCK = ".video-controls-container";

/** 3:2 — `TILE_AR` in attendants_layout.rs, the cap `.grid-item` and the grid self tile share. */
const TILE_ASPECT = 1.5;

function seedScript(key: string, value: string): string {
  return `(() => { try { localStorage.setItem(${JSON.stringify(key)}, ${JSON.stringify(
    value,
  )}); } catch (_) {} })();`;
}

function readStored(page: Page, key: string): Promise<string | null> {
  return page.evaluate((k) => localStorage.getItem(k), key);
}

async function enterGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");
  const which = await waitForVisibleState(
    [
      { name: "join", locator: joinButton },
      { name: "grid", locator: grid },
    ],
    30_000,
  );
  if (which === "join") {
    await page.waitForTimeout(800);
    await joinButton.click().catch(() => {});
  }
  await expect(grid).toBeVisible({ timeout: 20_000 });
  await expect(page.locator(SELF_NAV)).toHaveCount(1, { timeout: 20_000 });
}

async function joinSoloMeeting(page: Page, label: string): Promise<void> {
  await fillAndSubmitJoinForm(page, `self_view_${label}_${Date.now()}`, "SelfViewUser");
  await enterGrid(page);
}

/** Reveal the hover-gated chrome cluster, then press one of its self-view buttons. */
async function pressSelfTileControl(
  page: Page,
  testId: string,
  accessibleName: string,
): Promise<void> {
  const nav = page.locator(SELF_NAV);
  await expect(nav).toHaveCount(1);
  await nav.hover();
  const button = nav.locator(`.host-tile-chrome [data-testid="${testId}"]`);
  await expect(button).toHaveCount(1);
  await expect(button).toHaveClass(/\bself-tile-action\b/);
  await expect(button).toHaveAttribute("aria-label", accessibleName);
  await expect(button).toBeVisible({ timeout: 10_000 });
  await button.click();
}

/** The dedicated self-view live region; the announcer is silent on hide. */
function announcer(page: Page) {
  return page.locator('[data-testid="self-view-announcer"]');
}

async function expectAnnouncement(page: Page, text: string): Promise<void> {
  const region = announcer(page);
  await expect(region).toHaveCount(1);
  await expect(region).toHaveClass(/\bvisually-hidden\b/);
  await expect(region).toHaveAttribute("role", "status");
  await expect(region).toHaveAttribute("aria-live", "polite");
  await expect
    .poll(() => region.textContent().then((t) => t ?? ""), { timeout: 10_000 })
    .toContain(text);
}

/** Focus is moved from a Rust `spawn` after a 0ms tick, so this polls rather than reads once. */
async function expectFocusedLabel(page: Page, label: string): Promise<void> {
  await expect
    .poll(() => page.evaluate(() => document.activeElement?.getAttribute("aria-label") ?? null), {
      timeout: 10_000,
    })
    .toBe(label);
}

/** Same 0ms-tick `spawn` as above, keyed on the testid rather than the label. */
async function expectFocusedTestId(page: Page, testId: string): Promise<void> {
  await expect
    .poll(() => page.evaluate(() => document.activeElement?.getAttribute("data-testid") ?? null), {
      timeout: 10_000,
    })
    .toBe(testId);
}

/** `.video-control-button:hover .tooltip` sets visibility AND opacity; `toBeVisible` reads only the first. */
async function expectTooltipRevealedOnHover(page: Page, buttonSelector: string): Promise<void> {
  const tooltip = page.locator(`${buttonSelector} span.tooltip`);
  await expect(tooltip).toHaveCount(1);
  await page.mouse.move(0, 0);
  await expect(tooltip).toBeHidden();
  await page.locator(buttonSelector).hover();
  await expect(tooltip).toBeVisible({ timeout: 10_000 });
  await expect
    .poll(() => tooltip.evaluate((el) => window.getComputedStyle(el).opacity), { timeout: 10_000 })
    .toBe("1");
}

async function openPreferences(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.locator('[data-testid="open-settings"]').click();
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  await page.locator(".settings-nav-button").filter({ hasText: "Preferences" }).click();
  await expect(page.locator("#settings-panel-preferences")).toBeVisible({ timeout: 10_000 });
}

async function closeSettings(page: Page): Promise<void> {
  await page.locator('.device-settings-modal button[aria-label="Close settings"]').click();
  await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 10_000 });
}

interface TwoUserMeeting {
  hostPage: Page;
  guestPage: Page;
  close: () => Promise<void>;
}

async function openTwoUserMeeting(
  uiURL: string,
  label: string,
  init: { host?: string; guest?: string } = {},
): Promise<TwoUserMeeting> {
  const browsers: Browser[] = [];
  const close = async (): Promise<void> => {
    await Promise.all(browsers.map((b) => b.close()));
  };
  try {
    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    browsers.push(hostBrowser);
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    browsers.push(guestBrowser);

    const hostCtx = await createAuthenticatedContext(
      hostBrowser,
      `${label}-host@videocall.rs`,
      "SelfViewHost",
      uiURL,
    );
    const guestCtx = await createAuthenticatedContext(
      guestBrowser,
      `${label}-guest@videocall.rs`,
      "SelfViewGuest",
      uiURL,
    );
    if (init.host) await hostCtx.addInitScript(init.host);
    if (init.guest) await guestCtx.addInitScript(init.guest);

    const hostPage = await hostCtx.newPage();
    const guestPage = await guestCtx.newPage();
    await enterTwoUserMeeting(hostPage, guestPage, `self_view_${label}_${Date.now()}`);
    return { hostPage, guestPage, close };
  } catch (err) {
    await close();
    throw err;
  }
}

interface Box {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** Half-open overlap on both axes, so boxes that merely touch do not count. */
function boxesOverlap(a: Box, b: Box): boolean {
  return a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;
}

async function boxOf(page: Page, selector: string): Promise<Box> {
  const box = await page.locator(selector).boundingBox();
  expect(box, `${selector} must have a layout box`).not.toBeNull();
  expect(box!.width, `${selector} must have a non-zero width`).toBeGreaterThan(0);
  expect(box!.height, `${selector} must have a non-zero height`).toBeGreaterThan(0);
  return box!;
}

function expectBoxWithinViewport(page: Page, box: Box, label: string): void {
  const viewport = page.viewportSize();
  expect(viewport, `a viewport size is required to bound the ${label}`).not.toBeNull();
  expect(box.width, `${label} must have a non-zero width`).toBeGreaterThan(0);
  expect(box.height, `${label} must have a non-zero height`).toBeGreaterThan(0);
  expect(box.x, `${label} must not start off the left edge`).toBeGreaterThanOrEqual(0);
  expect(box.y, `${label} must not start above the top edge`).toBeGreaterThanOrEqual(0);
  expect(box.x + box.width, `${label} must not run past the right edge`).toBeLessThanOrEqual(
    viewport!.width,
  );
  expect(box.y + box.height, `${label} must not run past the bottom edge`).toBeLessThanOrEqual(
    viewport!.height,
  );
}

async function expectBoxInsideViewport(page: Page, selector: string, label: string): Promise<Box> {
  const box = await boxOf(page, selector);
  expectBoxWithinViewport(page, box, label);
  return box;
}

async function expectClearOfDock(page: Page, box: Box, label: string): Promise<void> {
  await expect(page.locator(DOCK)).toHaveCount(1);
  const dockBox = await boxOf(page, DOCK);
  expect(
    boxesOverlap(box, dockBox),
    `${label} at (${box.x},${box.y},${box.width}x${box.height}) must clear the ` +
      `dock at (${dockBox.x},${dockBox.y},${dockBox.width}x${dockBox.height})`,
  ).toBe(false);
}

interface RevealSnapshot {
  open: string | null;
  display: string;
  visibility: string;
  opacity: string;
  box: Box | null;
}

/** Attribute, computed style and box in ONE round-trip: the 4s reveal outruns
 *  a sequence of reads. */
function readReveal(page: Page, buttonSelector: string): Promise<RevealSnapshot> {
  return page.evaluate((sel) => {
    const button = document.querySelector(sel);
    const tip = button ? button.querySelector("span.tooltip") : null;
    if (!button || !tip) {
      return { open: null, display: "", visibility: "", opacity: "", box: null };
    }
    const style = window.getComputedStyle(tip);
    const rect = tip.getBoundingClientRect();
    return {
      open: button.getAttribute("data-tooltip-open"),
      display: style.display,
      visibility: style.visibility,
      opacity: style.opacity,
      box: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
    };
  }, buttonSelector);
}

async function selfNavAspect(page: Page): Promise<number> {
  const box = await page.locator(SELF_NAV).boundingBox();
  expect(box, "self-view nav must have a layout box").not.toBeNull();
  expect(box!.width).toBeGreaterThan(0);
  expect(box!.height).toBeGreaterThan(0);
  return box!.width / box!.height;
}

test.describe("Self-view placement (issue 66)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // Fails un-fixed: `data-self-placement` is not emitted, so toHaveAttribute finds no attribute.
  test("self view defaults to the corner and leaves the lone remote full-bleed @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const meeting = await openTwoUserMeeting(baseURL || "http://localhost:3001", "default");
    try {
      const page = meeting.hostPage;
      const nav = page.locator(SELF_NAV);
      await expect(nav).toHaveCount(1);
      await expect(nav).toHaveAttribute("data-self-placement", "corner");
      await expect(nav).toHaveAttribute("data-self-hidden", "false");

      expect(await nav.evaluate((el) => window.getComputedStyle(el).position)).toBe("absolute");

      expect(await readStored(page, PLACEMENT_KEY)).toBeNull();

      await expect(page.locator("#grid-container.participants-1")).toBeVisible({ timeout: 20_000 });
      await expect(page.locator("#grid-container .grid-item.full-bleed")).toHaveCount(1);
    } finally {
      await meeting.close();
    }
  });

  // Fails un-fixed: there is no on-tile placement button, so its presence assertion finds zero.
  test("the on-tile toggle moves the self view into the grid and back @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const meeting = await openTwoUserMeeting(baseURL || "http://localhost:3001", "toggle");
    try {
      const page = meeting.hostPage;
      const nav = page.locator(SELF_NAV);
      await expect(nav).toHaveAttribute("data-self-placement", "corner");

      await pressSelfTileControl(page, PLACEMENT_BTN, "Move self view to grid");

      await expect(nav).toHaveAttribute("data-self-placement", "grid");
      await expectAnnouncement(page, "Self view moved to the grid");
      await expectFocusedLabel(page, "Move self view to corner");

      await expect(page.locator("#grid-container.participants-2")).toBeVisible({ timeout: 15_000 });
      const remote = page.locator("#grid-container .grid-item");
      await expect(remote).toHaveCount(1);
      await expect(page.locator("#grid-container .grid-item.full-bleed")).toHaveCount(0);
      await expect(nav).toHaveAttribute("data-self-full-bleed", "false");

      const order = await page.locator("#grid-container").evaluate((grid) => {
        const selfNav = grid.querySelector("nav#host-controls-nav");
        const tiles = grid.querySelectorAll(".grid-item");
        if (!selfNav || tiles.length === 0) return null;
        const lastTile = tiles[tiles.length - 1];
        return {
          tiles: tiles.length,
          navIsDirectChild: selfNav.parentElement === grid,
          navFollowsLastTile: Boolean(
            lastTile.compareDocumentPosition(selfNav) & Node.DOCUMENT_POSITION_FOLLOWING,
          ),
        };
      });
      expect(order, "grid must hold both the self nav and a remote tile").not.toBeNull();
      expect(order!.tiles).toBe(1);
      expect(order!.navIsDirectChild).toBe(true);
      expect(order!.navFollowsLastTile).toBe(true);

      const selfBox = await nav.boundingBox();
      const remoteBox = await remote.first().boundingBox();
      expect(selfBox).not.toBeNull();
      expect(remoteBox).not.toBeNull();
      const leftOf = selfBox!.x + selfBox!.width <= remoteBox!.x + 2;
      const above = selfBox!.y + selfBox!.height <= remoteBox!.y + 2;
      expect(
        leftOf || above,
        `self tile at (${selfBox!.x},${selfBox!.y}) must precede the remote at ` +
          `(${remoteBox!.x},${remoteBox!.y})`,
      ).toBe(true);

      const chip = nav.locator("h4.floating-name.self-tile-name");
      await expect(chip).toHaveCount(1);
      await expect(chip.locator("span.self-indicator")).toHaveText("You");
      await expect(chip.locator("span.floating-name-text")).toHaveText(/\S/);
      await expect(chip.locator("span.floating-name-text")).not.toContainText("(You)");

      await pressSelfTileControl(page, PLACEMENT_BTN, "Move self view to corner");
      await expect(nav).toHaveAttribute("data-self-placement", "corner");
      await expectAnnouncement(page, "Self view moved to the corner");
      await expectFocusedLabel(page, "Move self view to grid");
      expect(await readStored(page, PLACEMENT_KEY)).toBe("corner");
      await expect(page.locator("#grid-container.participants-1")).toBeVisible({ timeout: 15_000 });
      await expect(page.locator("#grid-container .grid-item.full-bleed")).toHaveCount(1);
      await expect(nav.locator("h4.floating-name.self-tile-name")).toHaveCount(0);
    } finally {
      await meeting.close();
    }
  });

  // Fails un-fixed: nothing writes `vc_self_view_placement`, so the post-toggle read is null.
  test("grid placement persists across a reload and into a fresh session @bvt1", async ({
    page,
    browser,
    context,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    await injectSessionCookie(context, { baseURL });
    await joinSoloMeeting(page, "persist");

    const nav = page.locator(SELF_NAV);
    await expect(nav).toHaveAttribute("data-self-placement", "corner");
    await pressSelfTileControl(page, PLACEMENT_BTN, "Move self view to grid");
    await expect(nav).toHaveAttribute("data-self-placement", "grid");
    expect(await readStored(page, PLACEMENT_KEY)).toBe("grid");

    await page.reload();
    await enterGrid(page);
    await expect(nav).toHaveAttribute("data-self-placement", "grid", { timeout: 20_000 });

    const freshCtx = await browser.newContext({
      baseURL: baseURL || "http://localhost:3001",
      ignoreHTTPSErrors: true,
    });
    try {
      await injectSessionCookie(freshCtx, { baseURL, email: "self-view-fresh@videocall.rs" });
      await freshCtx.addInitScript(seedScript(PLACEMENT_KEY, "grid"));
      const freshPage = await freshCtx.newPage();
      await joinSoloMeeting(freshPage, "seeded");
      const freshNav = freshPage.locator(SELF_NAV);
      await expect(freshNav).toHaveAttribute("data-self-placement", "grid");
      await expect(freshPage.locator("#grid-container.participants-1")).toBeVisible({
        timeout: 15_000,
      });
      await expect(freshNav).toHaveAttribute("data-self-full-bleed", "true");
    } finally {
      await freshCtx.close();
    }
  });

  // Fails un-fixed: no hide button, no `data-self-hidden`, no toast for the locators to find.
  test("hiding the self view offers Undo, then Preferences brings it back @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    await injectSessionCookie(context, { baseURL });
    await joinSoloMeeting(page, "hide");

    const nav = page.locator(SELF_NAV);
    await expect(nav).toHaveAttribute("data-self-hidden", "false");

    await pressSelfTileControl(page, HIDE_BTN, "Hide self view");
    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    expect(await readStored(page, VISIBLE_KEY)).toBe("false");

    const toast = page.locator('.self-view-hidden-toast[role="status"]');
    await expect(toast).toBeVisible({ timeout: 10_000 });
    await expect(toast).toContainText("Self view hidden");
    await expect(toast).toContainText("others still see you");
    const undo = toast.locator('[data-testid="self-view-undo-hide"]');
    await expect(undo).toHaveText("Undo");
    await expectFocusedLabel(page, "Undo hiding the self view");

    await undo.click();
    await expect(nav).toHaveAttribute("data-self-hidden", "false");
    expect(await readStored(page, VISIBLE_KEY)).toBe("true");
    await expect(toast).toHaveCount(0, { timeout: 10_000 });
    await expectAnnouncement(page, "Self view shown.");

    await pressSelfTileControl(page, HIDE_BTN, "Hide self view");
    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    await expect
      .poll(
        () =>
          announcer(page)
            .textContent()
            .then((t) => t ?? ""),
        { timeout: 10_000 },
      )
      .toBe("");
    // WCAG 2.2.1: the countdown only runs once focus has left the toast.
    await expectFocusedLabel(page, "Undo hiding the self view");
    await page.locator("#grid-container").focus();
    await expect(page.locator(".self-view-hidden-toast")).toHaveCount(0, { timeout: 30_000 });

    await openPreferences(page);
    const visibleSwitch = page.locator('[data-testid="self-view-visible-checkbox"]');
    await expect(visibleSwitch).toHaveCount(1);
    await expect(visibleSwitch).not.toBeChecked();
    await page.locator("label.glow-switch").filter({ has: visibleSwitch }).click();
    await expect(visibleSwitch).toBeChecked();
    await closeSettings(page);

    await expect(nav).toHaveAttribute("data-self-hidden", "false");
    expect(await readStored(page, VISIBLE_KEY)).toBe("true");
  });

  // Fails un-fixed: the toast carries no close control, so the locator finds zero nodes.
  test("the hide toast can be dismissed without unhiding the self view @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    await injectSessionCookie(context, { baseURL });
    await joinSoloMeeting(page, "toastclose");

    const nav = page.locator(SELF_NAV);
    await expect(nav).toHaveAttribute("data-self-hidden", "false");

    await pressSelfTileControl(page, HIDE_BTN, "Hide self view");
    await expect(nav).toHaveAttribute("data-self-hidden", "true");

    const toast = page.locator(TOAST);
    await expect(toast).toBeVisible({ timeout: 10_000 });
    await expect(toast).toContainText(SCOPE_TEXT);
    const close = toast.locator(TOAST_CLOSE);
    await expect(close).toBeVisible();
    await expect(close).toHaveAttribute("type", "button");
    await expect(close).toHaveAttribute("aria-label", "Dismiss message");
    await expect(toast.locator('[data-testid="self-view-undo-hide"]')).toHaveCount(1);

    await close.click();

    // 2s is far inside the 12s auto-dismiss, suspended while the toast holds focus.
    await expect(page.locator(".self-view-hidden-toast")).toHaveCount(0, { timeout: 2_000 });

    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    expect(await readStored(page, VISIBLE_KEY)).toBe("false");

    const show = page.locator(SHOW_ICON);
    await expect(show).toBeVisible({ timeout: 10_000 });
    await expectFocusedTestId(page, SHOW_BTN);

    await show.click();
    await expect(nav).toHaveAttribute("data-self-hidden", "false");
    expect(await readStored(page, VISIBLE_KEY)).toBe("true");
    await expectAnnouncement(page, "Self view shown.");
    await expect(page.locator(SHOW_ICON)).toHaveCount(0, { timeout: 10_000 });
  });

  // Fails un-fixed: the Escape ladder has no toast branch, so the toast outlives the key.
  test("Escape dismisses the hide toast without unhiding the self view @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    await injectSessionCookie(context, { baseURL });
    await joinSoloMeeting(page, "toastesc");

    const nav = page.locator(SELF_NAV);
    const toast = page.locator(TOAST);

    await pressSelfTileControl(page, HIDE_BTN, "Hide self view");
    await expect(toast).toBeVisible({ timeout: 10_000 });
    await expectFocusedLabel(page, "Undo hiding the self view");

    // No drawer or popover is open, so the ladder falls through to the toast.
    await page.keyboard.press("Escape");

    // 2s is far inside the 12s auto-dismiss, suspended while the toast holds focus.
    await expect(page.locator(".self-view-hidden-toast")).toHaveCount(0, { timeout: 2_000 });
    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    expect(await readStored(page, VISIBLE_KEY)).toBe("false");
    await expect(page.locator(SHOW_ICON)).toBeVisible({ timeout: 10_000 });
    await expectFocusedTestId(page, SHOW_BTN);

    await page.locator(SHOW_ICON).click();
    await expect(nav).toHaveAttribute("data-self-hidden", "false");

    await pressSelfTileControl(page, HIDE_BTN, "Hide self view");
    await expect(toast).toBeVisible({ timeout: 10_000 });
    await expectFocusedLabel(page, "Undo hiding the self view");
    await page.locator("#grid-container").focus();

    await page.keyboard.press("Escape");

    // Leaving the toast arms the 12s timer, so 2s still tells the two apart.
    await expect(page.locator(".self-view-hidden-toast")).toHaveCount(0, { timeout: 2_000 });
    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    expect(await readStored(page, VISIBLE_KEY)).toBe("false");
    // A focus handoff is scheduled on a 0ms timer, and same-delay timers fire in
    // scheduling order, so draining one turn here proves it had its chance.
    await page.evaluate(() => new Promise<void>((resolve) => setTimeout(resolve, 0)));
    expect(await page.evaluate(() => document.activeElement?.id ?? null)).toBe("grid-container");
  });

  // Fails un-fixed: no grid self tile, so `.self-tile-camera-off` is absent and the box is 16:9.
  test("a camera-off self tile in the grid keeps its 3:2 cell at any width @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const meeting = await openTwoUserMeeting(baseURL || "http://localhost:3001", "cameraoff", {
      host: seedScript(PLACEMENT_KEY, "grid"),
    });
    try {
      const page = meeting.hostPage;
      const nav = page.locator(SELF_NAV);
      await expect(nav).toHaveAttribute("data-self-placement", "grid");
      await expect(page.locator("#grid-container.participants-2")).toBeVisible({ timeout: 20_000 });

      await expect(nav.locator(".self-tile-camera-off")).toHaveCount(1);
      await expect(nav).not.toContainText("Camera Off");

      const wideAspect = await selfNavAspect(page);
      expect(wideAspect).toBeGreaterThan(TILE_ASPECT * 0.9);
      expect(wideAspect).toBeLessThan(TILE_ASPECT * 1.1);

      await page.setViewportSize({ width: 390, height: 780 });
      await page.waitForTimeout(1500);
      const narrowAspect = await selfNavAspect(page);
      expect(narrowAspect).toBeGreaterThan(TILE_ASPECT * 0.9);
      expect(narrowAspect).toBeLessThan(TILE_ASPECT * 1.1);

      await page.setViewportSize({ width: 1280, height: 720 });
      await page.waitForTimeout(1500);

      const cameraToggle = page.locator('[data-testid="camera-toggle-button"]');
      await page.locator(".video-controls-container").hover();
      await expect(cameraToggle).toBeVisible({ timeout: 15_000 });
      await cameraToggle.click();
      await expect(nav.locator(".self-tile-camera-off")).toHaveCount(0, { timeout: 20_000 });

      const cameraOnAspect = await selfNavAspect(page);
      expect(cameraOnAspect).toBeGreaterThan(TILE_ASPECT * 0.9);
      expect(cameraOnAspect).toBeLessThan(TILE_ASPECT * 1.1);
    } finally {
      await meeting.close();
    }
  });

  // Fails un-fixed: the Preferences "Self view" radiogroup does not exist; both testids find zero.
  test("the Preferences radiogroup returns the self view to the corner @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    await injectSessionCookie(context, { baseURL });
    await context.addInitScript(seedScript(PLACEMENT_KEY, "grid"));
    await joinSoloMeeting(page, "prefs");

    const nav = page.locator(SELF_NAV);
    await expect(nav).toHaveAttribute("data-self-placement", "grid");

    await openPreferences(page);
    const group = page.locator('[role="radiogroup"][aria-label="Self view placement"]');
    await expect(group).toHaveCount(1);
    const corner = page.locator('[data-testid="self-view-placement-corner"]');
    const grid = page.locator('[data-testid="self-view-placement-grid"]');
    await expect(grid).toHaveAttribute("aria-checked", "true");
    await expect(corner).toHaveAttribute("aria-checked", "false");

    await corner.click();
    await expect(corner).toHaveAttribute("aria-checked", "true");
    await expect(grid).toHaveAttribute("aria-checked", "false");
    await closeSettings(page);

    await expect(nav).toHaveAttribute("data-self-placement", "corner");
    expect(await readStored(page, PLACEMENT_KEY)).toBe("corner");
  });

  // Fails un-fixed: `effective_self_placement` does not exist, so nothing reads corner-over-grid.
  test("a remote screen share pins a grid self view to the corner, preference intact @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const meeting = await openTwoUserMeeting(baseURL || "http://localhost:3001", "share", {
      host: MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT,
      guest: seedScript(PLACEMENT_KEY, "grid"),
    });
    try {
      const { hostPage, guestPage } = meeting;
      const nav = guestPage.locator(SELF_NAV);
      await expect(nav).toHaveAttribute("data-self-placement", "grid");

      const shared = await startScreenShare(hostPage, guestPage);
      expect(shared, "the viewer must receive the mocked screen share").toBe(true);

      await expect(nav).toHaveAttribute("data-self-placement", "corner", { timeout: 20_000 });
      expect(await readStored(guestPage, PLACEMENT_KEY)).toBe("grid");
      await expect(nav.locator(`[data-testid="${PLACEMENT_BTN}"]`)).toHaveCount(0);
      await expect(nav.locator(`[data-testid="${HIDE_BTN}"]`)).toHaveCount(1);

      const stopButton = hostPage.locator("button.video-control-button", {
        has: hostPage.locator(".tooltip", { hasText: "Stop Screen Share" }),
      });
      await hostPage.locator(".video-controls-container").hover();
      await expect(stopButton).toBeVisible({ timeout: 15_000 });
      await stopButton.click();

      await expect(nav).toHaveAttribute("data-self-placement", "grid", { timeout: 30_000 });
    } finally {
      await meeting.close();
    }
  });
});

/** Issue 2693 — the persistent way back from a hidden self view, once the toast is gone. */
test.describe("Self-view hidden indicator (issue 2693)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // Fails un-fixed: nothing renders `button.self-view-hidden-icon`, so the control finds zero.
  // Also fails un-fixed: a Preferences hide leaves "Self view shown." latched, not "".
  test("a self view hidden before the join offers a persistent Show icon @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    await injectSessionCookie(context, { baseURL });
    await context.addInitScript(seedScript(VISIBLE_KEY, "false"));
    await joinSoloMeeting(page, "hiddenicon");

    const nav = page.locator(SELF_NAV);
    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    expect(await readStored(page, VISIBLE_KEY)).toBe("false");

    // Direct child of the grid container, per its mount site in attendants.rs.
    await expect(page.locator(`#grid-container > ${SHOW_ICON}`)).toHaveCount(1);
    const show = page.locator(SHOW_ICON);
    await expect(show).toBeVisible({ timeout: 10_000 });
    await expect(show).toHaveClass(/\bself-view-hidden-icon\b/);
    // The dock defaults to Bottom, which leaves the bottom-right lane alone.
    await expect(show).not.toHaveClass(/--dock-right\b/);
    await expect(show).toHaveAttribute("type", "button");
    await expect(show).toHaveAttribute("aria-label", "Show self view");

    await expect(show).not.toContainText("Self view hidden");
    await expect(show).not.toHaveAttribute("role", "status");
    await expect(show.locator('svg[aria-hidden="true"]')).toHaveCount(1);

    // A `visibility: hidden` tooltip is out of the a11y tree, so the hint is its own node.
    await expect(show).toHaveAttribute("aria-describedby", HIDDEN_HINT_ID);
    const hint = page.locator(`#${HIDDEN_HINT_ID}`);
    await expect(hint).toHaveCount(1);
    await expect(hint).toHaveClass(/\bvisually-hidden\b/);
    await expect(hint).toHaveText(SCOPE_TEXT);

    const tooltip = show.locator("span.tooltip");
    await expect(tooltip).toHaveAttribute("aria-hidden", "true");
    await expect(tooltip.locator("span.tooltip-title")).toHaveText("Show self view");
    await expect(tooltip.locator("span.tooltip-desc")).toHaveText(SCOPE_TEXT);

    const iconBox = await expectBoxInsideViewport(page, SHOW_ICON, "the Show icon");
    await expectClearOfDock(page, iconBox, "the Show icon");

    // Rust owns the arrival reveal at every width; wait it out before reading rest.
    await expect(show).toHaveAttribute("data-tooltip-open", "false", { timeout: 8_000 });

    // A 240px tooltip centred on a button ~24px from the right edge hangs off screen.
    await expectTooltipRevealedOnHover(page, SHOW_ICON);
    await expectBoxInsideViewport(page, `${SHOW_ICON} span.tooltip`, "the Show tooltip");

    await show.click();

    await expect(nav).toHaveAttribute("data-self-hidden", "false");
    expect(await readStored(page, VISIBLE_KEY)).toBe("true");
    await expectAnnouncement(page, "Self view shown.");
    await expect(page.locator(SHOW_ICON)).toHaveCount(0, { timeout: 10_000 });

    // A Rust `spawn` walks hide button, camera toggle, grid, taking the first
    // that accepts focus (`.self-tile-action` is hidden off-hover). Not `body`.
    await expect
      .poll(
        () =>
          page.evaluate(() => {
            const el = document.activeElement;
            if (!el) return "none";
            // `id` is "" rather than null when absent, so fall through on falsy.
            return el.getAttribute("data-testid") ?? (el.id || el.tagName.toLowerCase());
          }),
        { timeout: 10_000 },
      )
      .toMatch(/^(self-view-hide-button|camera-toggle-button|grid-container)$/);

    // A toastless hide must clear the announcement: an unchanged live region says nothing.
    await openPreferences(page);
    const visibleSwitch = page.locator('[data-testid="self-view-visible-checkbox"]');
    await expect(visibleSwitch).toBeChecked();
    await page.locator("label.glow-switch").filter({ has: visibleSwitch }).click();
    await expect(visibleSwitch).not.toBeChecked();
    await closeSettings(page);

    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    await expect(show).toBeVisible({ timeout: 10_000 });
    await expect
      .poll(
        () =>
          announcer(page)
            .textContent()
            .then((t) => t ?? ""),
        { timeout: 10_000 },
      )
      .toBe("");

    await show.click();
    await expect(nav).toHaveAttribute("data-self-hidden", "false");
    await expectAnnouncement(page, "Self view shown.");
  });

  // Fails un-fixed: the icon never exists, so it cannot be visible beside the toast.
  test("the Show icon appears with the Undo toast and outlives it @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    await injectSessionCookie(context, { baseURL });
    await joinSoloMeeting(page, "icontoast");

    const nav = page.locator(SELF_NAV);
    await expect(nav).toHaveAttribute("data-self-hidden", "false");
    const show = page.locator(SHOW_ICON);
    await expect(show).toHaveCount(0);

    await pressSelfTileControl(page, HIDE_BTN, "Hide self view");
    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    expect(await readStored(page, VISIBLE_KEY)).toBe("false");

    const toast = page.locator(TOAST);
    await expect(toast).toBeVisible({ timeout: 10_000 });
    await expect(show).toBeVisible({ timeout: 10_000 });
    // Read once: a retrying matcher would outlast the 4s reveal and pass regardless.
    expect(await show.getAttribute("data-tooltip-open")).toBe("false");

    // Showing while the toast still stands must retire the toast with it.
    await show.click();
    await expect(nav).toHaveAttribute("data-self-hidden", "false");
    await expect(page.locator(".self-view-hidden-toast")).toHaveCount(0, { timeout: 10_000 });
    await expect(show).toHaveCount(0, { timeout: 10_000 });

    await pressSelfTileControl(page, HIDE_BTN, "Hide self view");
    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    await expect(toast).toBeVisible({ timeout: 10_000 });
    await expect(show).toBeVisible({ timeout: 10_000 });
    expect(await show.getAttribute("data-tooltip-open")).toBe("false");

    // WCAG 2.2.1: the 12s countdown only runs once focus has left the toast.
    await expectFocusedLabel(page, "Undo hiding the self view");
    await page.locator("#grid-container").focus();
    await expect(page.locator(".self-view-hidden-toast")).toHaveCount(0, { timeout: 30_000 });

    await expect(show).toBeVisible();
    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    // The toast outlived the 4s reveal window, so a late flip would show here.
    expect(await show.getAttribute("data-tooltip-open")).toBe("false");

    await show.click();
    await expect(nav).toHaveAttribute("data-self-hidden", "false");
    expect(await readStored(page, VISIBLE_KEY)).toBe("true");
    await expect(show).toHaveCount(0, { timeout: 10_000 });
  });

  // Fails un-fixed: the reveal burns behind the settings overlay, so the attribute
  // is already back to "false" by the time the modal closes.
  test("a hide from Preferences defers the reveal until settings closes @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    await injectSessionCookie(context, { baseURL });
    await joinSoloMeeting(page, "iconprefs");

    const nav = page.locator(SELF_NAV);
    await expect(nav).toHaveAttribute("data-self-hidden", "false");

    await openPreferences(page);
    const visibleSwitch = page.locator('[data-testid="self-view-visible-checkbox"]');
    await expect(visibleSwitch).toBeChecked();
    await page.locator("label.glow-switch").filter({ has: visibleSwitch }).click();
    await expect(visibleSwitch).not.toBeChecked();

    const show = page.locator(SHOW_ICON);
    await expect(show).toHaveCount(1, { timeout: 10_000 });
    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    // Read once: the overlay must DEFER the reveal, not spend it out of sight.
    expect(await show.getAttribute("data-tooltip-open")).toBe("false");

    await closeSettings(page);

    let revealed: RevealSnapshot | null = null;
    await expect
      .poll(
        async () => {
          const snap = await readReveal(page, SHOW_ICON);
          revealed = snap;
          return `${snap.open}|${snap.visibility}|${snap.opacity}`;
        },
        { timeout: 2_000, intervals: [50, 50, 100, 100, 200, 200, 300, 500] },
      )
      .toBe("true|visible|1");

    const shown = revealed as RevealSnapshot | null;
    expect(shown, "the deferred reveal must have been sampled while open").not.toBeNull();
    expect(shown!.box, "an open tooltip must have a box").not.toBeNull();
    expectBoxWithinViewport(page, shown!.box!, "the Show tooltip");

    await expect(show).toHaveAttribute("data-tooltip-open", "false", { timeout: 6_000 });
    await expect(show.locator("span.tooltip")).toBeHidden();
  });

  // Fails un-fixed: no icon exists at any width, so the mobile presence assertion finds zero.
  test("the Show icon stays inside a 390px viewport and clear of the dock @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    await injectSessionCookie(context, { baseURL });
    await context.addInitScript(seedScript(VISIBLE_KEY, "false"));
    // Sized BEFORE the join: the phone reveal is scoped to the icon's mount.
    await page.setViewportSize({ width: 390, height: 844 });
    await joinSoloMeeting(page, "iconmobile");

    const show = page.locator(SHOW_ICON);
    const tooltip = show.locator("span.tooltip");
    let revealed: RevealSnapshot | null = null;
    await expect
      .poll(
        async () => {
          const snap = await readReveal(page, SHOW_ICON);
          revealed = snap;
          return `${snap.open}|${snap.visibility}|${snap.opacity}`;
        },
        { timeout: 2_000, intervals: [50, 50, 100, 100, 200, 200, 300, 500] },
      )
      .toBe("true|visible|1");

    const shown = revealed as RevealSnapshot | null;
    expect(shown, "the reveal must have been sampled while open").not.toBeNull();
    // `.video-control-button .tooltip` is `display: none` below 641px; the opt-out must beat it.
    expect(shown!.display, "the phone opt-out must restore display").not.toBe("none");
    expect(shown!.box, "an open tooltip must have a box").not.toBeNull();
    expectBoxWithinViewport(page, shown!.box!, "the Show tooltip");

    await expect(show).toHaveAttribute("data-tooltip-open", "false", { timeout: 6_000 });
    await expect(tooltip).toBeHidden();

    const nav = page.locator(SELF_NAV);
    await expect(nav).toHaveAttribute("data-self-hidden", "true");
    await expect(show).toBeVisible();
    const viewport = page.viewportSize();
    expect(viewport).not.toBeNull();
    expect(viewport!.width).toBe(390);

    const iconBox = await expectBoxInsideViewport(page, SHOW_ICON, "the Show icon");
    await expectClearOfDock(page, iconBox, "the Show icon");

    // `right: clamp(0.5rem, 2vw, 1rem)` resolves to 8px at 390px, off the grid.
    const gridBox = await boxOf(page, "#grid-container");
    const rightGap = gridBox.x + gridBox.width - (iconBox.x + iconBox.width);
    expect(rightGap).toBeGreaterThanOrEqual(0);
    expect(rightGap).toBeLessThanOrEqual(24);

    // An obscured control fails the click, so this also proves nothing covers it.
    await show.click();
    await expect(nav).toHaveAttribute("data-self-hidden", "false");
    expect(await readStored(page, VISIBLE_KEY)).toBe("true");
    await expect(show).toHaveCount(0, { timeout: 10_000 });
  });
});
