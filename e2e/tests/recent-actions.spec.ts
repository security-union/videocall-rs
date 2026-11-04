import { test, expect, chromium, Locator, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import {
  BROWSER_ARGS,
  PIN_WEBSOCKET_TRANSPORT_INIT_SCRIPT,
  createAuthenticatedContext,
} from "../helpers/auth-context";
import { createMeeting } from "../helpers/meeting-api";
import { MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT } from "../helpers/screen-share-meeting";
import {
  enterMeetingAsHost,
  guestJoinsMeeting,
  joinMeetingFromPage,
} from "../helpers/two-user-meeting";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { openDiagnosticsDrawer } from "../helpers/diagnostics-drawer";
import { waitForServices } from "../helpers/wait-for-services";

const LIST_KEY = "vc_recent_actions";
const LIMIT_KEY = "vc_recent_actions_limit";
const TRIGGER =
  '.action-bar-slot-wrapper[data-slot="recent_actions"] > button#recent-actions-trigger.video-control-button';
const POPOVER = "#recent-actions-popover";
const MENU = `${POPOVER} > [role="menu"]`;
const ENTRIES = `${MENU} > .recent-actions-row > button[data-recent-action]`;
const DIALOG = "#device-settings-dialog";
const PEER_LIST = "#peer-list-container";

function meetingId(label: string): string {
  return `recents_${label}_${Date.now()}`;
}

async function seedStorage(page: Page, list: string[], limit?: number): Promise<void> {
  await page.goto("/");
  await page.evaluate(
    ([listKey, listValue, limitKey, limitValue]) => {
      localStorage.setItem(listKey, listValue);
      if (limitValue) localStorage.setItem(limitKey, limitValue);
    },
    [LIST_KEY, JSON.stringify(list), LIMIT_KEY, limit === undefined ? "" : JSON.stringify(limit)],
  );
}

async function join(page: Page, label: string, seed?: string[], limit?: number): Promise<void> {
  if (seed) await seedStorage(page, seed, limit);
  await enterMeetingAsHost(page, meetingId(label), "RecentUser");
  await expect(page.locator(TRIGGER)).toBeVisible({ timeout: 10_000 });
}

async function openMenu(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.locator(TRIGGER).click();
  await expect(page.locator(POPOVER)).toBeVisible();
}

async function openMenuFromOverflow(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.locator("#overflow-menu-trigger").click();
  const item = page
    .locator(".action-bar-overflow-popover > button.overflow-item")
    .filter({ hasText: /^Recent actions$/ });
  await expect(item).toHaveCount(1);
  await item.click();
  await expect(page.locator(POPOVER)).toBeVisible();
}

async function expectPopoverOnScreen(
  page: Page,
  size: { width: number; height: number },
): Promise<void> {
  const box = await page.locator(POPOVER).boundingBox();
  expect(box).not.toBeNull();
  if (box) {
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.y).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(size.width);
    expect(box.y + box.height).toBeLessThanOrEqual(size.height);
  }
}

async function expectPopoverClearOf(page: Page, drawer: Locator): Promise<void> {
  const popover = page.locator(POPOVER);
  const [drawerBox, popoverBox] = await Promise.all([drawer.boundingBox(), popover.boundingBox()]);
  expect(drawerBox).not.toBeNull();
  expect(popoverBox).not.toBeNull();
  if (drawerBox && popoverBox) {
    const overlapX =
      Math.min(drawerBox.x + drawerBox.width, popoverBox.x + popoverBox.width) -
      Math.max(drawerBox.x, popoverBox.x);
    const overlapY =
      Math.min(drawerBox.y + drawerBox.height, popoverBox.y + popoverBox.height) -
      Math.max(drawerBox.y, popoverBox.y);
    expect
      .soft(
        overlapX > 0 && overlapY > 0,
        `popover ${JSON.stringify(popoverBox)} overlaps drawer ${JSON.stringify(drawerBox)}`,
      )
      .toBe(false);
  }
  const popoverIsOnTop = await popover.evaluate((el) => {
    const r = el.getBoundingClientRect();
    return el.contains(document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2));
  });
  expect.soft(popoverIsOnTop, "elementFromPoint at the popover's centre").toBe(true);
}

async function closeMenu(page: Page): Promise<void> {
  await page.keyboard.press("Escape");
  await expect(page.locator(POPOVER)).toHaveCount(0);
}

function entry(page: Page, slug: string): Locator {
  return page.locator(`${ENTRIES}[data-recent-action="${slug}"]`);
}

async function expectListed(page: Page, slugs: string[]): Promise<void> {
  await expect
    .poll(() =>
      page
        .locator(ENTRIES)
        .evaluateAll((els) => els.map((el) => el.getAttribute("data-recent-action"))),
    )
    .toEqual(slugs);
}

function stored(page: Page): Promise<unknown> {
  return page.evaluate((key) => JSON.parse(localStorage.getItem(key) ?? "null"), LIST_KEY);
}

function hint(item: Locator): Locator {
  return item.locator(":scope > .recent-actions-item-text > .recent-actions-item-hint");
}

async function focusedTag(page: Page): Promise<string | undefined> {
  return page.evaluate(() => document.activeElement?.tagName);
}

async function expectFocusInside(page: Page, selector: string): Promise<void> {
  await expect
    .poll(() => page.evaluate((s) => Boolean(document.activeElement?.closest(s)), selector))
    .toBe(true);
}

async function openSettingsTab(page: Page, section: string): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.getByTestId("open-settings").click();
  await expect(page.locator(DIALOG)).toBeVisible({ timeout: 10_000 });
  await page.locator(`#settings-tab-${section}`).click();
  await expect(page.locator(`#settings-panel-${section}`)).toBeVisible();
}

const openPreferences = (page: Page) => openSettingsTab(page, "preferences");

async function expectRingAfterTabbingBackTo(page: Page, control: Locator): Promise<void> {
  await control.focus();
  await page.keyboard.press("Tab");
  await page.keyboard.press("Shift+Tab");
  await expect(control).toBeFocused();
  await expect(control).toHaveCSS("outline-style", "solid");
}

async function closeSettings(page: Page): Promise<void> {
  await page.locator(`${DIALOG} button[aria-label="Close settings"]`).click();
  await expect(page.locator(DIALOG)).toHaveCount(0);
}

function tab(page: Page, section: string): Locator {
  return page.locator(`${DIALOG} [role="tab"]#settings-tab-${section}`);
}

function settingGroup(page: Page, slug: string): Locator {
  return page.locator(`${DIALOG} [data-recent-action="${slug}"]`);
}

test.describe("Recent actions", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.describe("signed in", () => {
    // An open drawer takes its width from the bar's band, and Recent actions, last in the bar, overflows first.
    test.use({ viewport: { width: 1700, height: 900 } });

    test.beforeEach(async ({ context, baseURL }) => {
      await injectSessionCookie(context, { baseURL });
    });

    test("docked left with Participants open, the popover is not under the drawer", async ({
      page,
    }) => {
      await page.goto("/");
      await page.evaluate(() => localStorage.setItem("vc_dock_position", "left"));
      await seedStorage(page, ["slot:participants"]);
      await enterMeetingAsHost(page, meetingId("dock_left"), "RecentUser");
      await expect(page.locator('[data-dock="dock-left"]')).toHaveCount(1);

      await page.getByTestId("peer-list-button").click();
      const drawer = page.locator("#peer-list-container.visible");
      await expect(drawer).toBeInViewport({ ratio: 1 });
      await openMenuFromOverflow(page);
      await expectPopoverClearOf(page, drawer);
    });

    test("docked right with Diagnostics open, the popover is not under the drawer", async ({
      page,
    }) => {
      await page.goto("/");
      await page.evaluate(() => localStorage.setItem("vc_dock_position", "right"));
      await seedStorage(page, ["slot:participants"]);
      await enterMeetingAsHost(page, meetingId("dock_right"), "RecentUser");
      await expect(page.locator('[data-dock="dock-right"]')).toHaveCount(1);

      const drawer = await openDiagnosticsDrawer(page);
      await expect(drawer).toBeInViewport({ ratio: 1 });
      await openMenuFromOverflow(page);
      await expectPopoverClearOf(page, drawer);
    });

    for (const dock of ["left", "right"]) {
      test(`at 320px docked ${dock}, the popover is fully on-screen`, async ({ page }) => {
        const size = { width: 320, height: 568 };
        const seeded = [
          "setting:preferred_protocol",
          "setting:action_bar_position",
          "setting:speaker_highlight",
          "slot:participants",
        ];
        await page.setViewportSize(size);
        await page.goto("/");
        await page.evaluate((side) => localStorage.setItem("vc_dock_position", side), dock);
        await seedStorage(page, seeded);
        await enterMeetingAsHost(page, meetingId(`narrow_dock_${dock}`), "RecentUser");
        await expect(page.locator(`[data-dock="dock-${dock}"]`)).toHaveCount(1);

        await openMenuFromOverflow(page);
        await expect(page.locator(ENTRIES)).toHaveCount(seeded.length);
        await expectPopoverOnScreen(page, size);
        await page.keyboard.press("End");
        await expect(page.locator(ENTRIES).last()).toBeFocused();
        await expect(page.locator(ENTRIES).last()).toBeInViewport({ ratio: 0.95 });
      });
    }

    test("ArrowDown on the trigger with Density open from an entry moves into Density", async ({
      page,
    }) => {
      await join(page, "density_arrow", ["slot:density"]);
      await openMenu(page);
      await entry(page, "slot:density").click();
      await expect(page.locator(".density-popover")).toBeVisible();
      await expect(page.locator(POPOVER)).toHaveCount(0);
      await expectFocusInside(page, ".density-popover");

      await page.locator(TRIGGER).focus();
      await expect(page.locator(TRIGGER)).toBeFocused();
      await page.keyboard.press("ArrowDown");

      await expectFocusInside(page, ".density-popover");
      await expect(page.locator(POPOVER)).toHaveCount(0);
      await expect(page.locator(".density-popover")).toBeVisible();
    });

    test("the Settings entry moves focus into the dialog, Escape closes it and focus returns", async ({
      page,
    }) => {
      await join(page, "settings_entry", ["slot:settings"]);
      await openMenu(page);
      await entry(page, "slot:settings").click();
      await expect(page.locator(DIALOG)).toBeVisible();
      await expectFocusInside(page, DIALOG);

      await page.keyboard.press("Escape");
      await expect(page.locator(DIALOG)).toHaveCount(0);
      await expect(page.locator(TRIGGER)).toBeFocused();
    });

    test("under forced colors a checked entry keeps a visible outline", async ({ page }) => {
      await join(page, "forced_colors", ["slot:diagnostics", "slot:participants"]);
      const hand = page.getByTestId("raise-hand-button");
      await hand.click();
      await expect(hand).toHaveAttribute("aria-pressed", "true");
      await page.emulateMedia({ forcedColors: "active" });
      expect(await page.evaluate(() => matchMedia("(forced-colors: active)").matches)).toBe(true);

      await openMenu(page);
      await expectListed(page, ["slot:raise_hand", "slot:diagnostics", "slot:participants"]);
      const checked = entry(page, "slot:raise_hand");
      await expect(checked).toHaveAttribute("aria-checked", "true");
      await page.keyboard.press("End");
      await expect(entry(page, "slot:participants")).toBeFocused();

      await expect(checked).toHaveCSS("outline-style", "solid");
      await expect(checked).toHaveCSS("outline-width", "2px");
      await expect(entry(page, "slot:diagnostics")).toHaveCSS("outline-style", "none");
    });

    test("a fresh context shows the empty state", async ({ page }) => {
      await join(page, "empty");
      expect(await stored(page)).toBeNull();

      await openMenu(page);
      await expect(page.locator(`${POPOVER} > .recent-actions-heading`)).toHaveText(
        "Recent actions",
      );
      const empty = page.locator(`${MENU} > button[data-testid="recent-actions-empty"]`);
      await expect(empty).toBeVisible();
      await expect(empty).toHaveAttribute("role", "menuitem");
      await expect(empty).toHaveAttribute("aria-disabled", "true");
      await expect(empty).toContainText("No recent actions yet");
      await expect(empty).toBeFocused();
      await expect(page.locator(ENTRIES)).toHaveCount(0);
    });

    test("used features list most recent first and de-duplicated, without Mic or Camera", async ({
      page,
    }) => {
      await join(page, "mru");
      const peers = page.getByTestId("peer-list-button");
      const drawer = page.locator(PEER_LIST);

      await peers.click();
      await expect(drawer).toHaveClass(/\bvisible\b/);
      const hand = page.getByTestId("raise-hand-button");
      await hand.click();
      await expect(hand).toHaveAttribute("aria-pressed", "true");

      for (const testId of ["mic-toggle-button", "camera-toggle-button"]) {
        const control = page.getByTestId(testId);
        const before = await control.getAttribute("aria-label");
        expect(before).toBeTruthy();
        await control.click();
        await expect(control).not.toHaveAttribute("aria-label", before as string);
      }

      await peers.click();
      await expect(drawer).not.toHaveClass(/\bvisible\b/);

      await openMenu(page);
      await expectListed(page, ["slot:participants", "slot:raise_hand"]);
      await expect(entry(page, "slot:participants")).toHaveText("Participants");
      await expect(entry(page, "slot:raise_hand")).toHaveText("Raise hand");
      expect(await stored(page)).toEqual(["slot:participants", "slot:raise_hand"]);
    });

    test("activating an entry opens the drawer or toggles what its primary control does", async ({
      page,
    }) => {
      await join(page, "activate", ["slot:participants", "slot:raise_hand"]);

      await openMenu(page);
      await expectListed(page, ["slot:participants", "slot:raise_hand"]);
      await entry(page, "slot:participants").click();
      await expect(page.locator(POPOVER)).toHaveCount(0);
      await expect(page.locator(PEER_LIST)).toHaveClass(/\bvisible\b/);

      await openMenu(page);
      await entry(page, "slot:participants").click();
      await expect(page.locator(PEER_LIST)).not.toHaveClass(/\bvisible\b/);

      const hand = page.getByTestId("raise-hand-button");
      await expect(hand).toHaveAttribute("aria-pressed", "false");
      await openMenu(page);
      const handEntry = entry(page, "slot:raise_hand");
      await expect(handEntry).toHaveAttribute("role", "menuitemcheckbox");
      await expect(handEntry).toHaveAttribute("aria-checked", "false");
      await handEntry.click();
      await expect(hand).toHaveAttribute("aria-pressed", "true");

      await openMenu(page);
      await expect(handEntry).toHaveAttribute("aria-checked", "true");
      await handEntry.click();
      await expect(hand).toHaveAttribute("aria-pressed", "false");

      await openMenu(page);
      await expect(handEntry).toHaveAttribute("aria-checked", "false");
    });

    test("an entry that opens a popover or panel leaves it open", async ({ page }) => {
      await join(page, "surfaces", [
        "slot:density",
        "slot:reactions",
        "slot:participants",
        "slot:settings",
      ]);

      await openMenu(page);
      await entry(page, "slot:density").click();
      await expect(page.locator(".density-popover")).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(page.locator(".density-popover")).toHaveCount(0);

      await openMenu(page);
      await entry(page, "slot:reactions").click();
      await expect(page.getByTestId("reactions-palette")).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(page.getByTestId("reactions-palette")).toHaveCount(0);

      await openMenu(page);
      await entry(page, "slot:participants").click();
      await expect(page.locator("#peer-list-container.visible")).toBeVisible();
      await page.getByTestId("peer-list-button").click();
      await expect(page.locator("#peer-list-container.visible")).toHaveCount(0);

      await openMenu(page);
      await entry(page, "slot:settings").click();
      await expect(page.locator(DIALOG)).toBeVisible();
    });

    test("the Record entry starts a recording and then reads Stop recording", async ({
      page,
      context,
    }) => {
      test.setTimeout(120_000);
      await context.addInitScript("window.showSaveFilePicker = undefined;");
      await join(page, "record", ["slot:recording"]);
      const bar = page.getByTestId("record-button");
      await expect(bar).toBeVisible();

      await openMenu(page);
      await expect(entry(page, "slot:recording")).toHaveText("Record");
      await entry(page, "slot:recording").click();
      await expect(bar).toHaveClass(/\brecord-active\b/, { timeout: 15_000 });

      const download = page.waitForEvent("download", { timeout: 30_000 });
      await openMenu(page);
      await expect(entry(page, "slot:recording")).toHaveText("Stop recording");
      await entry(page, "slot:recording").click();
      await download;
      await expect(bar).not.toHaveClass(/\brecord-active\b/);
    });

    test("the Screen share entry starts a share and then offers stopping it", async ({
      page,
      context,
    }) => {
      await context.addInitScript(MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT);
      await join(page, "share", ["slot:screen"]);
      const barShare = page.locator('#screen-share-trigger[data-state="active"]');

      await openMenu(page);
      await entry(page, "slot:screen").click();
      await expect(barShare).toBeVisible({ timeout: 15_000 });

      await openMenu(page);
      const stop = page.locator(
        `${MENU} > .recent-actions-row > .overflow-share-group > [data-testid="overflow-share-stop"]`,
      );
      await expect(stop).toHaveAttribute("role", "menuitem");
      await stop.click();
      await expect(barShare).toHaveCount(0, { timeout: 15_000 });
      await expect(page.locator(POPOVER)).toHaveCount(0);
    });

    test("the list persists across a reload and into a different meeting @bvt1", async ({
      page,
    }) => {
      test.setTimeout(120_000);
      await join(page, "persist");
      await page.getByTestId("peer-list-button").click();
      await page.getByTestId("raise-hand-button").click();
      await openMenu(page);
      const used = ["slot:raise_hand", "slot:participants"];
      await expectListed(page, used);
      expect(await stored(page)).toEqual(used);

      await page.reload();
      expect(await joinMeetingFromPage(page)).toBe("in-meeting");
      await openMenu(page);
      await expectListed(page, used);

      await fillAndSubmitJoinForm(page, meetingId("persist_other"), "RecentUser");
      expect(await joinMeetingFromPage(page)).toBe("in-meeting");
      await openMenu(page);
      await expectListed(page, used);
    });

    test("the Preferences limit truncates the list and persists", async ({ page }) => {
      const seeded = [
        "slot:participants",
        "slot:raise_hand",
        "slot:reactions",
        "slot:diagnostics",
        "slot:density",
      ];
      await join(page, "limit", seeded);

      await openPreferences(page);
      const limit = (n: number) => page.getByTestId(`recent-actions-limit-${n}`);
      for (const n of [3, 5, 8, 10]) await expect(limit(n)).toHaveAttribute("role", "radio");
      await expect(limit(5)).toHaveAttribute("aria-checked", "true");
      const kept = ["slot:settings", "slot:participants", "slot:raise_hand"];
      await expect.poll(() => stored(page)).toEqual(["slot:settings", ...seeded.slice(0, 4)]);

      await limit(3).click();
      await expect(limit(3)).toHaveAttribute("aria-checked", "true");
      await expect(limit(5)).toHaveAttribute("aria-checked", "false");
      await expect.poll(() => stored(page)).toEqual(kept);
      expect(await page.evaluate((key) => localStorage.getItem(key), LIMIT_KEY)).toBe("3");
      await closeSettings(page);
      await openMenu(page);
      await expectListed(page, kept);

      await page.reload();
      expect(await joinMeetingFromPage(page)).toBe("in-meeting");
      await openMenu(page);
      await expectListed(page, kept);
      await closeMenu(page);

      await page.getByTestId("peer-list-button").click();
      await openMenu(page);
      await expectListed(page, ["slot:participants", "slot:settings", "slot:raise_hand"]);
      await closeMenu(page);

      await openPreferences(page);
      await expect(limit(3)).toHaveAttribute("aria-checked", "true");
      await limit(10).click();
      await expect(limit(10)).toHaveAttribute("aria-checked", "true");
      await closeSettings(page);
      await openMenu(page);
      await expectListed(page, ["slot:settings", "slot:participants", "slot:raise_hand"]);
    });

    test("a setting used in Settings lists with its hint and deep links back to its control", async ({
      page,
    }) => {
      await join(page, "setting");

      await openPreferences(page);
      const radios = (checked: boolean) =>
        settingGroup(page, "setting:density").locator(
          `:scope > [role="radiogroup"] > button[role="radio"][aria-checked="${checked}"]`,
        );
      await expect(radios(false).first()).toBeVisible();
      const chosenLabel = (await radios(false).first().textContent()) as string;
      await radios(false).first().click();
      const chosen = settingGroup(page, "setting:density")
        .locator(':scope > [role="radiogroup"] > button[role="radio"]')
        .filter({ hasText: new RegExp(`^${chosenLabel}$`) });
      await expect(chosen).toHaveAttribute("aria-checked", "true");
      await expect.poll(() => stored(page)).toEqual(["setting:density", "slot:settings"]);
      await closeSettings(page);

      await openMenu(page);
      await expectListed(page, ["setting:density", "slot:settings"]);
      const item = entry(page, "setting:density");
      await expect(item.locator(":scope > .recent-actions-item-text > span").first()).toHaveText(
        "Density",
      );
      await expect(hint(item)).toHaveText("Settings · Preferences");

      await item.click();
      await expect(page.locator(DIALOG)).toBeVisible();
      await expect(tab(page, "preferences")).toHaveAttribute("aria-selected", "true");
      const group = settingGroup(page, "setting:density");
      await expect(group).toBeInViewport();
      await expect(group).toHaveClass(/\brecent-target-flash\b/);
      await expect(chosen).toHaveAttribute("aria-checked", "true");
      await expect(chosen).toBeFocused();
      await expect(group).not.toHaveClass(/\brecent-target-flash\b/, { timeout: 5_000 });

      for (const key of ["ArrowRight", "ArrowLeft", "ArrowDown", "ArrowUp"]) {
        await page.keyboard.press(key);
      }
      await page.waitForTimeout(200);
      await expect(tab(page, "preferences")).toHaveAttribute("aria-selected", "true");
      await expect(chosen).toHaveAttribute("aria-checked", "true");
      await expect(chosen).toBeFocused();

      await tab(page, "preferences").focus();
      await page.keyboard.press("ArrowRight");
      await expect(tab(page, "preferences")).toHaveAttribute("aria-selected", "false");

      await page.keyboard.press("Escape");
      await expect(page.locator(DIALOG)).toHaveCount(0);
      await expect(page.locator(TRIGGER)).toBeFocused();
    });

    test("the Theme entry lands on the current theme's button, not the first one", async ({
      page,
    }) => {
      await join(page, "theme_current");
      await openSettingsTab(page, "appearance");
      const light = settingGroup(page, "setting:theme").locator(
        'button.theme-icon-button[title="Light"]',
      );
      await expect(light).toBeVisible();
      await light.click();
      await expect(light).toHaveAttribute("aria-pressed", "true");
      await expect.poll(() => stored(page)).toEqual(["setting:theme", "slot:settings"]);
      await closeSettings(page);

      await openMenu(page);
      await entry(page, "setting:theme").click();
      await expect(page.locator(DIALOG)).toBeVisible();
      await expect(light).toBeFocused();
      await expect(light).toHaveAttribute("aria-pressed", "true");
    });

    test("using the Auto-shrink switch lists its setting", async ({ page }) => {
      await join(page, "auto_shrink_switch");
      await openPreferences(page);
      const toggle = page.getByTestId("action-bar-autoshrink-toggle");
      const label = page.locator(
        'label.glow-switch:has([data-testid="action-bar-autoshrink-toggle"])',
      );
      await expect(toggle).not.toBeChecked();
      await label.click();
      await expect(toggle).toBeChecked();
      await expect.poll(() => stored(page)).toEqual(["setting:auto_shrink", "slot:settings"]);
      await label.click();
      await expect(toggle).not.toBeChecked();
      await closeSettings(page);

      await openMenu(page);
      await expectListed(page, ["setting:auto_shrink", "slot:settings"]);
      await expect(hint(entry(page, "setting:auto_shrink"))).toHaveText("Settings · Preferences");
    });

    test("clicking the open trigger closes the menu and does not reopen it", async ({ page }) => {
      await join(page, "trigger_closes", ["slot:participants"]);
      await openMenu(page);
      await page.locator(TRIGGER).click();
      await page.waitForTimeout(300);
      await expect(page.locator(POPOVER)).toHaveCount(0);
      await expect(page.locator(TRIGGER)).toHaveAttribute("aria-expanded", "false");
    });

    test("a Meeting timer opened from the overflow returns focus to More actions, not Recent actions", async ({
      page,
      context,
    }) => {
      await context.addInitScript(
        `localStorage.setItem("vc_action_bar_layout", JSON.stringify({ v: 2, slots: ["mic", "camera", "recent_actions"], hidden: [] }));`,
      );
      await page.setViewportSize({ width: 1150, height: 720 });
      await join(page, "timer_overflow");
      const more = page.locator("#overflow-menu-trigger");
      await expect(page.locator("#meeting-timer-trigger")).toBeHidden();

      await page.locator(".video-controls-container").hover();
      await more.click();
      const item = page
        .locator(".action-bar-overflow-popover > button.overflow-item")
        .filter({ hasText: /^Meeting timer$/ });
      await expect(item).toHaveCount(1);
      await item.click();
      const popover = page.getByTestId("meeting-timer-popover");
      await expect(popover).toBeVisible();
      await popover.locator("button").first().focus();

      await page.keyboard.press("Escape");
      await expect(popover).toHaveCount(0);
      await expect(more).toBeFocused();
    });

    test("a selected option shows a focus ring when tabbed to, also after a deep link's flash", async ({
      page,
    }) => {
      await seedStorage(page, ["setting:density"]);
      await enterMeetingAsHost(page, meetingId("focus_ring"), "RecentUser");
      await openPreferences(page);
      const selected = page.locator(
        '#settings-panel-preferences [role="radiogroup"][aria-label="Tile density mode"] > button[aria-checked="true"]',
      );
      await expectRingAfterTabbingBackTo(page, selected);

      await page.locator("#settings-tab-appearance").click();
      await expectRingAfterTabbingBackTo(
        page,
        page.locator("#settings-panel-appearance .theme-icon-button--active"),
      );
      await closeSettings(page);

      await openMenu(page);
      await entry(page, "setting:density").click();
      await expect(selected).toBeFocused();
      await expect(settingGroup(page, "setting:density")).not.toHaveClass(
        /\brecent-target-flash\b/,
        { timeout: 5_000 },
      );
      await expectRingAfterTabbingBackTo(page, selected);
    });

    test("settings entries open the tab that holds their control", async ({ page }) => {
      await join(
        page,
        "setting_tabs",
        [
          "setting:preferred_protocol",
          "setting:camera",
          "setting:theme",
          "setting:microphone",
          "setting:auto_shrink",
          "setting:avatar",
        ],
        8,
      );

      for (const [slug, section, title] of [
        ["setting:preferred_protocol", "network", "Network"],
        ["setting:camera", "video", "Video"],
        ["setting:theme", "appearance", "Appearance"],
        ["setting:microphone", "audio", "Audio"],
        ["setting:auto_shrink", "preferences", "Preferences"],
        ["setting:avatar", "profile", "Profile"],
      ]) {
        await openMenu(page);
        await expect(hint(entry(page, slug))).toHaveText(`Settings · ${title}`);
        await entry(page, slug).click();
        await expect(page.locator(DIALOG)).toBeVisible();
        await expect(tab(page, section)).toHaveAttribute("aria-selected", "true");
        await expect(settingGroup(page, slug)).toBeInViewport();
        await expectFocusInside(page, `${DIALOG} [data-recent-action="${slug}"]`);
        await page.keyboard.press("Escape");
        await expect(page.locator(DIALOG)).toHaveCount(0);
        await expect(page.locator(TRIGGER)).toBeFocused();
      }
    });

    test("keyboard: arrows wrap, Home and End jump, Escape and Tab close and return focus", async ({
      page,
    }) => {
      const seeded = ["slot:diagnostics", "slot:raise_hand", "slot:participants"];
      await join(page, "keyboard", seeded);
      const [first, second, third] = seeded.map((slug) => entry(page, slug));

      await openMenu(page);
      await expectListed(page, seeded);
      await expect(first).toBeFocused();
      await page.keyboard.press("ArrowDown");
      await expect(second).toBeFocused();
      await page.keyboard.press("ArrowDown");
      await expect(third).toBeFocused();
      await page.keyboard.press("ArrowDown");
      await expect(first).toBeFocused();
      await page.keyboard.press("ArrowUp");
      await expect(third).toBeFocused();
      await page.keyboard.press("Home");
      await expect(first).toBeFocused();
      await page.keyboard.press("End");
      await expect(third).toBeFocused();
      await closeMenu(page);
      await expect(page.locator(TRIGGER)).toBeFocused();

      await page.keyboard.press("ArrowUp");
      await expect(page.locator(POPOVER)).toBeVisible();
      await expect(third).toBeFocused();
      await closeMenu(page);

      await page.keyboard.press("ArrowDown");
      await expect(page.locator(POPOVER)).toBeVisible();
      await expect(first).toBeFocused();
      await page.keyboard.press("Tab");
      await expect(page.locator(POPOVER)).toHaveCount(0);
      await expect(page.locator(TRIGGER)).toBeFocused();
    });

    for (const key of ["Enter", "Space"]) {
      test(`${key} on the third entry runs it, closes the menu and keeps focus off body`, async ({
        page,
      }) => {
        await join(page, `key_${key}`, ["slot:raise_hand", "slot:reactions", "slot:participants"]);
        await openMenu(page);
        await page.keyboard.press("ArrowDown");
        await page.keyboard.press("ArrowDown");
        await expect(entry(page, "slot:participants")).toBeFocused();

        await page.keyboard.press(key);

        await expect.soft(page.locator(PEER_LIST)).toHaveClass(/\bvisible\b/, { timeout: 5_000 });
        await expect.soft(page.locator(POPOVER)).toHaveCount(0, { timeout: 5_000 });
        await page.waitForTimeout(300);
        expect.soft(await focusedTag(page)).not.toBe("BODY");
      });

      test(`${key} on a third-entry setting deep links to its control`, async ({ page }) => {
        await join(page, `key_setting_${key}`, [
          "slot:raise_hand",
          "slot:participants",
          "setting:density",
        ]);
        await page.locator(TRIGGER).focus();
        await page.keyboard.press("ArrowUp");
        await expect(entry(page, "setting:density")).toBeFocused();

        await page.keyboard.press(key);

        await expect.soft(page.locator(DIALOG)).toBeVisible({ timeout: 5_000 });
        await expect.soft(page.locator(POPOVER)).toHaveCount(0, { timeout: 5_000 });
        await page.waitForTimeout(300);
        expect.soft(await focusedTag(page)).not.toBe("BODY");
        await expect
          .soft(tab(page, "preferences"))
          .toHaveAttribute("aria-selected", "true", { timeout: 5_000 });
        await expect
          .soft(
            settingGroup(page, "setting:density").locator(
              ':scope > [role="radiogroup"] > button[role="radio"][aria-checked="true"]',
            ),
          )
          .toBeFocused({ timeout: 5_000 });
      });
    }

    test("real keys: Enter on the first entry deep links, arrows stay on the control, Escape returns", async ({
      page,
    }) => {
      await join(page, "real_keys", ["setting:density", "slot:participants"]);
      const barDensity = page.locator("#density-mode-trigger");
      await expect(barDensity).toHaveAttribute("title", "Layout density: Auto");
      await page.locator(TRIGGER).focus();
      await page.keyboard.press("ArrowDown");
      await expect(entry(page, "setting:density")).toBeFocused();

      await page.keyboard.press("Enter");

      await expect(page.locator(DIALOG)).toBeVisible();
      await expect(tab(page, "preferences")).toHaveAttribute("aria-selected", "true");
      const auto = settingGroup(page, "setting:density")
        .locator(':scope > [role="radiogroup"] > button[role="radio"]')
        .filter({ hasText: /^Auto$/ });
      await expect(auto).toHaveAttribute("aria-checked", "true");
      await expect(auto).toBeFocused();
      await page.keyboard.press("ArrowRight");
      await page.waitForTimeout(200);
      await expect(tab(page, "preferences")).toHaveAttribute("aria-selected", "true");
      await expect(auto).toHaveAttribute("aria-checked", "true");
      await expect(auto).toBeFocused();

      await page.keyboard.press("Escape");
      await expect(page.locator(DIALOG)).toHaveCount(0);
      await expect(page.locator(TRIGGER)).toBeFocused();
      await expect(barDensity).toHaveAttribute("title", "Layout density: Auto");
    });

    test("overflowed on a narrow viewport, it is reachable and the popover is on-screen", async ({
      page,
    }) => {
      const seeded = [
        "setting:preferred_protocol",
        "setting:action_bar_position",
        "setting:speaker_highlight",
        "setting:notifications",
        "setting:video_tiles",
        "setting:self_view",
        "setting:auto_hide",
        "setting:density",
        "setting:theme",
        "slot:participants",
      ];
      await seedStorage(page, seeded, 10);
      await enterMeetingAsHost(page, meetingId("narrow"), "RecentUser");
      await expect(page.locator(TRIGGER)).toBeVisible({ timeout: 10_000 });

      for (const size of [
        { width: 400, height: 720 },
        { width: 320, height: 568 },
      ]) {
        await page.setViewportSize(size);
        await expect(page.locator(TRIGGER)).toBeHidden();
        await openMenuFromOverflow(page);

        await expect(page.locator(ENTRIES)).toHaveCount(seeded.length);
        await expectPopoverOnScreen(page, size);
        await page.keyboard.press("End");
        await expect(page.locator(ENTRIES).last()).toBeFocused();
        // The scroller's integer scrollTop can clip a fraction of a pixel off the last row.
        await expect(page.locator(ENTRIES).last()).toBeInViewport({ ratio: 0.95 });

        await closeMenu(page);
        await expect(page.locator("#overflow-menu-trigger")).toBeFocused();
      }
    });
  });

  test("a stored avatar setting is not listed for a guest and stays stored", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const seeded = ["setting:avatar", "setting:theme"];
    const id = meetingId("guest_avatar");
    const host = { email: "recents-avatar-host@videocall.rs", name: "RecentsAvatarHost" };
    await createMeeting(host.email, host.name, {
      meetingId: id,
      waitingRoomEnabled: false,
      allowGuests: true,
    });
    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await createAuthenticatedContext(hostBrowser, host.email, host.name, uiURL);
      await hostCtx.addInitScript(PIN_WEBSOCKET_TRANSPORT_INIT_SCRIPT);
      const guestCtx = await guestBrowser.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
      await guestCtx.addInitScript(PIN_WEBSOCKET_TRANSPORT_INIT_SCRIPT);
      await guestCtx.addInitScript(
        ([key, value]) => localStorage.setItem(key, value),
        [LIST_KEY, JSON.stringify(seeded)],
      );
      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      await enterMeetingAsHost(hostPage, id, host.name);

      await guestPage.goto(`/meeting/${id}/guest`);
      const nameField = guestPage.locator("#guest-name");
      await expect(nameField).toBeVisible({ timeout: 20_000 });
      await nameField.fill("RecentsGuest");
      await nameField.press("Enter");
      expect(await joinMeetingFromPage(guestPage)).toBe("in-meeting");
      await expect(guestPage.locator(TRIGGER)).toBeVisible({ timeout: 10_000 });

      await openMenu(guestPage);
      await expectListed(guestPage, ["setting:theme"]);
      expect(await stored(guestPage)).toEqual(seeded);
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  test("a stored host-only action is not listed for a non-host and stays stored", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const seeded = [
      "slot:meeting_options",
      "slot:participants",
      "slot:meeting_timer",
      "slot:recording",
    ];
    const id = meetingId("nonhost");
    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "recents-host@videocall.rs",
        "RecentsHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        guestBrowser,
        "recents-guest@videocall.rs",
        "RecentsGuest",
        uiURL,
      );
      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      await seedStorage(hostPage, seeded);
      await seedStorage(guestPage, seeded);

      await enterMeetingAsHost(hostPage, id, "RecentsHost");
      await expect(hostPage.getByTestId("open-meeting-options")).toBeVisible({ timeout: 10_000 });
      await openMenu(hostPage);
      await expectListed(hostPage, seeded);

      await guestJoinsMeeting(hostPage, guestPage, id, "RecentsGuest");
      await expect(guestPage.locator(TRIGGER)).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.getByTestId("open-meeting-options")).toHaveCount(0);
      await expect(guestPage.getByTestId("record-button")).toHaveCount(0);
      await openMenu(guestPage);
      await expectListed(guestPage, ["slot:participants"]);
      expect(await stored(guestPage)).toEqual(seeded);

      await entry(guestPage, "slot:participants").click();
      await expect(guestPage.locator(PEER_LIST)).toHaveClass(/\bvisible\b/);
      expect(await stored(guestPage)).toEqual([
        "slot:participants",
        "slot:meeting_options",
        "slot:meeting_timer",
        "slot:recording",
      ]);
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});
