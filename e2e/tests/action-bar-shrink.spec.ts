import { test, expect, Locator, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

// Issue 2781. A clipped control keeps a layout box and its inner button keeps its
// full size, so Playwright calls it visible: every check here reads the width of
// the nav's direct child, which is what the clip rule shrinks to 1px.

const NAV = "nav.video-controls-container";
const AUTOSHRINK_KEY = "vc_dock_autoshrink";
const AUTOHIDE_KEY = "vc_dock_autohide";
const KEPT_KEY = "vc_action_bar_shrunk";
const DOCK_KEY = "vc_dock_position";
const HANDLE = "action-bar-expand-handle";
const MENU = ".dock-position-wrapper .glass-select-menu";
const AUTOSHRINK_ITEM = `${MENU} .glass-select-option[aria-label="Auto-shrink action bar"]`;
const AUTOHIDE_ITEM = `${MENU} .glass-select-option[aria-label="Auto-hide action bar"]`;
const DOCK_TRIGGER = "#dock-menu-trigger";
const REACTIONS_TRIGGER = "#reactions-trigger";
const REACTIONS_PALETTE = '[data-testid="reactions-palette"]';
const NOTHING_KEPT = JSON.stringify({ slots: [], hangup: false });
const GRID_CORNER = { position: { x: 10, y: 10 }, timeout: 10_000 };

// Must match HOVER_OPEN_DELAY_MS, HOVER_CLOSE_DELAY_MS and HOVER_SNAP_CLICK_GUARD_MS in
// action_bar_shrink.rs, and HOLD_TO_EDIT_MS in action_bar_layout.rs.
const HOVER_OPEN_MS = 300;
const HOVER_CLOSE_MS = 400;
const CLICK_GUARD_MS = 400;
const HOLD_MS = 2_000;
const SETTLE_MS = 200;

const TALL = { width: 1280, height: 1300 };
const PHONE = { width: 390, height: 844 };
const DEFAULT_KEPT = ["camera", "hangup-wrapper", "mic", "screen"];
const NO_SHARE = ["camera", "hangup-wrapper", "mic"];
const OTHER_CONTROLS = [
  "reactions",
  "raise_hand",
  "participants",
  "density",
  "diagnostics",
  "settings",
  "dock-position-wrapper",
];
// Each has its own reason to have no box. Any other control with none has gone where
// a clip belongs, unless More actions took it.
const MAY_HAVE_NO_BOX = [HANDLE, "action-bar-overflow-wrapper", "dock-position-wrapper"];

const POPOVERS = [
  { name: "dock menu", trigger: DOCK_TRIGGER, surface: MENU, escapes: true },
  {
    name: "reactions palette",
    trigger: REACTIONS_TRIGGER,
    surface: REACTIONS_PALETTE,
    outsideNav: true,
    escapes: true,
  },
  {
    name: "density popover",
    trigger: "#density-mode-trigger",
    surface: ".density-popover",
    outsideNav: true,
  },
  {
    name: "host's timer popover",
    trigger: "#meeting-timer-trigger",
    surface: '[data-testid="meeting-timer-popover"]',
    closedByTrigger: true,
  },
  {
    name: "More actions menu",
    trigger: "#overflow-menu-trigger",
    surface: ".action-bar-overflow-popover",
    viewport: PHONE,
    escapes: true,
  },
  {
    name: "Recent actions menu",
    trigger: "#recent-actions-trigger",
    surface: "#recent-actions-popover",
    outsideNav: true,
    escapes: true,
  },
];

const nav = (page: Page) => page.locator(NAV);
const handle = (page: Page) => page.getByTestId(HANDLE);
const slotSelector = (slug: string) =>
  `${NAV} > .action-bar-slot-wrapper[data-slot="${slug}"] > button.video-control-button`;
const slotButton = (page: Page, slug: string) => page.locator(slotSelector(slug));
const keptBox = (page: Page, slug: string) => page.getByTestId(`action-bar-kept-${slug}`);
const shrinkSwitch = (page: Page) => page.getByTestId("action-bar-autoshrink-toggle");
const stored = (page: Page, key: string) => page.evaluate((k) => localStorage.getItem(k), key);

const parkPointer = (page: Page) => page.mouse.move(1, 1);
const waitOutClickGuard = (page: Page) => page.waitForTimeout(CLICK_GUARD_MS + SETTLE_MS);

// Only an unset key: a reload must keep what the page changed.
async function seedStorage(page: Page, entries: Record<string, string>): Promise<void> {
  await page.context().addInitScript((seed) => {
    for (const [key, value] of Object.entries(seed)) {
      if (localStorage.getItem(key) === null) localStorage.setItem(key, value);
    }
  }, entries);
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
    await joinButton.click({ timeout: 10_000 }).catch(() => {});
  }
  await expect(grid).toBeVisible({ timeout: 20_000 });
  await expect(nav(page)).toHaveCount(1, { timeout: 15_000 });
}

async function joinWith(page: Page, label: string, seed: Record<string, string> = {}) {
  await seedStorage(page, seed);
  await fillAndSubmitJoinForm(page, `shrink_${label}_${Date.now()}`, "shrink-user");
  await enterGrid(page);
  await parkPointer(page);
}

// The join form's clicks can leave the mouse where the bar mounts, and the collapse
// off it is a snap, so this waits out the click guard before a test presses anything.
async function joinShrunk(
  page: Page,
  label: string,
  opts: {
    kept?: string[];
    seed?: Record<string, string>;
    viewport?: { width: number; height: number };
  } = {},
): Promise<void> {
  if (opts.viewport) await page.setViewportSize(opts.viewport);
  await joinWith(page, label, { [AUTOSHRINK_KEY]: "true", ...opts.seed });
  await expectFullWidth(page, opts.kept ?? DEFAULT_KEPT);
  await waitOutClickGuard(page);
}

const joinBare = (page: Page, label: string, seed: Record<string, string> = {}) =>
  joinShrunk(page, label, { kept: [HANDLE], seed: { [KEPT_KEY]: NOTHING_KEPT, ...seed } });

async function barWidths(page: Page) {
  const rows = await page.locator(`${NAV} > *`).evaluateAll((els) =>
    els.map((el) => ({
      name: el.getAttribute("data-slot") ?? el.classList[0],
      hasBox: getComputedStyle(el).display !== "none",
      overflowed: el.classList.contains("is-overflow-hidden"),
      width: el.getBoundingClientRect().width,
    })),
  );
  expect(rows.length, "the bar rendered no controls").toBeGreaterThan(0);
  const names = (keep: (row: (typeof rows)[number]) => boolean) =>
    rows
      .filter(keep)
      .map((row) => row.name)
      .sort();
  expect(
    names((row) => !row.hasBox && !row.overflowed && !MAY_HAVE_NO_BOX.includes(row.name)),
    "a control was hidden with display:none",
  ).toEqual([]);
  return {
    full: names((row) => row.hasBox && row.width > 1),
    clipped: names((row) => row.hasBox && row.width <= 1),
    overflowed: names((row) => row.overflowed),
  };
}

async function expectFullWidth(page: Page, controls: string[]): Promise<void> {
  await expect
    .poll(async () => (await barWidths(page)).full, {
      message: "only the kept controls keep their width",
    })
    .toEqual([...controls].sort());
}

// A resize reaches the overflow a frame or more later, so the first read still shows the
// last width. Every step must differ from the one before, or a stale read passes it.
async function expectOverflowAtWidth(
  page: Page,
  width: number,
  want: { full: string[]; gone: string[]; staying: string[] },
): Promise<void> {
  await page.setViewportSize({ width, height: 800 });
  await expect
    .poll(
      async () => {
        const { full, overflowed } = await barWidths(page);
        return {
          full,
          gone: want.gone.filter((name) => overflowed.includes(name)),
          staying: want.staying.filter((name) => !overflowed.includes(name)),
        };
      },
      { message: `the bar at ${width} px` },
    )
    .toEqual({ full: [...want.full].sort(), gone: want.gone, staying: want.staying });
}

async function expectShrunkTo(page: Page, kept: string[]): Promise<void> {
  await expectFullWidth(page, kept);
  const { clipped } = await barWidths(page);
  for (const name of OTHER_CONTROLS.filter((n) => !kept.includes(n))) {
    expect(clipped, `${name} must be clipped`).toContain(name);
  }
}

async function expectNothingClipped(page: Page): Promise<void> {
  await expect
    .poll(async () => (await barWidths(page)).clipped, {
      message: "an expanded bar clips nothing",
    })
    .toEqual([]);
}

async function expectExpanded(page: Page): Promise<void> {
  await expectNothingClipped(page);
  const { full } = await barWidths(page);
  for (const name of [...DEFAULT_KEPT, ...OTHER_CONTROLS]) {
    expect(full, `${name} must be at full width`).toContain(name);
  }
}

async function boxOf(locator: Locator) {
  await expect(locator).toHaveCount(1);
  const box = await locator.boundingBox();
  if (!box) throw new Error("a mounted control has no layout box");
  return box;
}

const middleOf = (box: { x: number; y: number; width: number; height: number }) => ({
  x: Math.round(box.x + box.width / 2),
  y: Math.round(box.y + box.height / 2),
});

async function centerOf(locator: Locator) {
  return middleOf(await boxOf(locator));
}

async function hoverOpenAttr(page: Page): Promise<boolean> {
  return (await nav(page).getAttribute("data-hover-open")) !== null;
}

// The second hover lands on the settled box of a bar the first one woke from a fade.
async function openBarByHover(page: Page): Promise<void> {
  await nav(page).hover();
  await nav(page).hover();
  await expect.poll(() => hoverOpenAttr(page)).toBe(true);
  await waitOutClickGuard(page);
}

async function openDockMenu(page: Page): Promise<void> {
  await openBarByHover(page);
  await page.locator(DOCK_TRIGGER).click();
  await expect(page.locator(MENU)).toBeVisible({ timeout: 5_000 });
}

async function openPreferences(page: Page): Promise<void> {
  await openDockMenu(page);
  await page
    .locator(".glass-select-option")
    .filter({ hasText: /Action Bar/ })
    .click();
  await expect(page.locator("#settings-panel-preferences")).toBeVisible({ timeout: 10_000 });
}

async function closePreferences(page: Page): Promise<void> {
  await page.locator('.device-settings-modal button[aria-label="Close settings"]').click();
  await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });
}

// The input is clipped inside its switch, so the label takes the click.
async function clickShrinkSwitch(page: Page): Promise<void> {
  await page.locator('label.glow-switch:has([data-testid="action-bar-autoshrink-toggle"])').click();
}

type BarEvent = "enter" | "leave" | "down" | "key" | "open" | "close" | "unpinned";

// Stamped on the renderer's clock, so a slow CDP hop can neither fail a timing premise
// nor let a press that missed the window pass. The first of each event wins.
async function stampBarEvents(page: Page): Promise<void> {
  await page.evaluate((selector) => {
    const bar = document.querySelector(selector) as Element;
    const stamps: Record<string, number> = {};
    const stamp = (name: string) => {
      stamps[name] ??= performance.now();
    };
    (window as unknown as { barStamps: Record<string, number> }).barStamps = stamps;
    bar.addEventListener("pointerenter", () => stamp("enter"));
    bar.addEventListener("pointerleave", () => stamp("leave"));
    window.addEventListener("pointerdown", () => stamp("down"), true);
    window.addEventListener("keydown", () => stamp("key"), true);
    new MutationObserver(() => {
      stamp(bar.hasAttribute("data-hover-open") ? "open" : "close");
    }).observe(bar, { attributes: true, attributeFilter: ["data-hover-open"] });
    new MutationObserver(() => {
      if (!bar.classList.contains("controls-expanded")) stamp("unpinned");
    }).observe(bar, { attributes: true, attributeFilter: ["class"] });
  }, NAV);
}

async function stampGap(page: Page, from: BarEvent, to: BarEvent): Promise<number> {
  const stamps = await page.evaluate(
    () => (window as unknown as { barStamps: Record<string, number> }).barStamps,
  );
  expect(Object.keys(stamps), `premise: both ${from} and ${to} happened`).toEqual(
    expect.arrayContaining([from, to]),
  );
  return stamps[to] - stamps[from];
}

// Waits in the page and reads `selector`'s box in the task the bar changes, so the only
// CDP hop before the press is this reply. `join-meeting.ts` documents CI main-thread
// stalls of seconds, and a press has to land inside a 400 ms window.
function boxAtBarChange(page: Page, change: "opened" | "closed" | "unpinned", selector: string) {
  return page.evaluate(
    ({ bar, change, selector }) =>
      new Promise<{ x: number; y: number; width: number; height: number }>((resolve) => {
        const el = document.querySelector(bar) as Element;
        const reached = () =>
          change === "unpinned"
            ? !el.classList.contains("controls-expanded")
            : el.hasAttribute("data-hover-open") === (change === "opened");
        const read = () => {
          const { x, y, width, height } = (
            document.querySelector(selector) as Element
          ).getBoundingClientRect();
          resolve({ x, y, width, height });
        };
        if (reached()) {
          read();
          return;
        }
        new MutationObserver((_, watcher) => {
          if (!reached()) return;
          watcher.disconnect();
          read();
        }).observe(el, { attributes: true, attributeFilter: ["class", "data-hover-open"] });
      }),
    { bar: NAV, change, selector },
  );
}

// The two inputs go out together: awaiting the move first would put a CDP round trip
// inside the 300 ms the press has to land in.
async function pressInsideIntent(page: Page, target: Locator): Promise<void> {
  await stampBarEvents(page);
  const { x, y } = await centerOf(target);
  await Promise.all([page.mouse.move(x, y), page.mouse.down()]);
  expect(await stampGap(page, "enter", "down"), "premise: pressed inside the intent").toBeLessThan(
    HOVER_OPEN_MS,
  );
}

async function isMicOn(page: Page): Promise<boolean> {
  return slotButton(page, "mic").evaluate((el) => el.classList.contains("active"));
}

async function expectMicOn(page: Page, on: boolean): Promise<void> {
  const mic = slotButton(page, "mic");
  if (on) await expect(mic).toHaveClass(/\bactive\b/);
  else await expect(mic).not.toHaveClass(/\bactive\b/);
}

async function tabUntil(page: Page, target: Locator, what: string): Promise<void> {
  for (let presses = 0; presses < 40; presses++) {
    if ((await target.count()) > 0) return;
    await page.keyboard.press("Tab");
  }
  throw new Error(`Tab never reached ${what}`);
}

async function arrowDownTo(page: Page, item: Locator): Promise<void> {
  for (let presses = 0; presses < 9; presses++) {
    await page.keyboard.press("ArrowDown");
    await page.waitForTimeout(50);
    if (await item.evaluate((el) => el === document.activeElement)) return;
  }
  throw new Error("ArrowDown never reached the menu item");
}

test.describe("Action bar auto-shrink", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("is off by default, and the first load records that", async ({ page }) => {
    await joinWith(page, "default");

    // load_dock_autoshrink persists the resolved value on the first load, so
    // inheriting from auto-hide can only ever happen once.
    await expect.poll(() => stored(page, AUTOSHRINK_KEY)).toBe("false");
    expect(await stored(page, KEPT_KEY)).toBeNull();

    await expect(nav(page)).not.toHaveClass(/\bauto-shrink\b/);
    await expect(nav(page)).toHaveClass(/\bcontrols-expanded\b/);
    await expect(handle(page)).toHaveCount(0);
    await expectExpanded(page);
  });

  test("turning it on shrinks the bar to the four kept buttons, hover expands it, mouse off shrinks it @bvt1", async ({
    page,
  }) => {
    await joinWith(page, "prefs_on");
    await expectExpanded(page);
    const expanded = await boxOf(nav(page));

    await openPreferences(page);
    await expect(shrinkSwitch(page)).not.toBeChecked();
    await clickShrinkSwitch(page);
    await expect(shrinkSwitch(page)).toBeChecked();
    expect(await stored(page, AUTOSHRINK_KEY)).toBe("true");
    await closePreferences(page);

    await parkPointer(page);
    await expect(nav(page)).toHaveClass(/\bauto-shrink\b/);
    await expect(nav(page)).not.toHaveClass(/\bcontrols-expanded\b/);
    await expectShrunkTo(page, DEFAULT_KEPT);
    expect((await boxOf(page.locator(`${NAV} > .dock-position-wrapper`))).width).toBe(1);
    expect((await boxOf(nav(page))).width).toBeLessThan(expanded.width);

    await nav(page).hover();
    await expect.poll(() => hoverOpenAttr(page)).toBe(true);
    await expectExpanded(page);

    await parkPointer(page);
    await expectShrunkTo(page, DEFAULT_KEPT);
  });

  test("hovering expands only after the intent delay, and leaving shrinks only after the grace", async ({
    page,
  }) => {
    await joinShrunk(page, "hover_timing");
    await stampBarEvents(page);
    const { x, y } = await centerOf(nav(page));

    await page.mouse.move(x, y);
    await expect.poll(() => hoverOpenAttr(page)).toBe(true);
    const openedAfter = await stampGap(page, "enter", "open");
    expect(openedAfter, "opened inside the intent delay").toBeGreaterThanOrEqual(HOVER_OPEN_MS - 5);
    expect(openedAfter, "opened late").toBeLessThanOrEqual(HOVER_OPEN_MS + 1_500);
    await expectExpanded(page);

    await parkPointer(page);
    await expect.poll(() => hoverOpenAttr(page)).toBe(false);
    const closedAfter = await stampGap(page, "leave", "close");
    expect(closedAfter, "closed inside the grace").toBeGreaterThanOrEqual(HOVER_CLOSE_MS - 5);
    expect(closedAfter, "closed late").toBeLessThanOrEqual(HOVER_CLOSE_MS + 1_500);
    await expectShrunkTo(page, DEFAULT_KEPT);
  });

  test("changing the kept buttons changes the shrunk bar, and survives a reload", async ({
    page,
  }) => {
    await joinWith(page, "kept_choice");

    await openPreferences(page);
    await clickShrinkSwitch(page);
    await expect(keptBox(page, "mic")).toBeChecked();
    await expect(keptBox(page, "raise_hand")).not.toBeChecked();
    await keptBox(page, "mic").uncheck();
    await keptBox(page, "raise_hand").check();
    await keptBox(page, "hangup").uncheck();
    await closePreferences(page);

    const kept = ["camera", "raise_hand", "screen"];
    await parkPointer(page);
    await expectShrunkTo(page, kept);

    await page.reload();
    await enterGrid(page);
    await parkPointer(page);
    await expect(nav(page)).toHaveClass(/\bauto-shrink\b/);
    await expectShrunkTo(page, kept);
    expect(await stored(page, AUTOSHRINK_KEY)).toBe("true");
    expect(JSON.parse((await stored(page, KEPT_KEY)) as string)).toEqual({
      slots: ["camera", "screen", "raise_hand"],
      hangup: false,
    });

    await openPreferences(page);
    await expect(shrinkSwitch(page)).toBeChecked();
    await expect(keptBox(page, "mic")).not.toBeChecked();
    await expect(keptBox(page, "raise_hand")).toBeChecked();
    await expect(keptBox(page, "hangup")).not.toBeChecked();
  });

  test("with nothing kept the bar is a handle with a real hit area, and a mouse enter opens it at once", async ({
    page,
  }) => {
    await joinBare(page, "bare");
    await expect(nav(page)).toHaveClass(/\bshrink-empty\b/);
    await expectShrunkTo(page, [HANDLE]);
    const hit = await boxOf(handle(page));
    expect(hit.width, "handle hit area width").toBeGreaterThanOrEqual(24);
    expect(hit.height, "handle hit area height").toBeGreaterThanOrEqual(24);

    // Not locator.hover(): the handle hides as soon as the pointer reaches it.
    await stampBarEvents(page);
    const { x, y } = middleOf(hit);
    await page.mouse.move(x, y);
    await expect.poll(() => hoverOpenAttr(page), { intervals: [25] }).toBe(true);
    expect(await stampGap(page, "enter", "open"), "no intent delay on a bare bar").toBeLessThan(
      HOVER_OPEN_MS / 2,
    );
    await expectExpanded(page);

    await parkPointer(page);
    await expectShrunkTo(page, [HANDLE]);
  });

  test("both resets restore the default four and leave shrinking on", async ({ page }) => {
    await joinShrunk(page, "resets");

    await openPreferences(page);
    await keptBox(page, "mic").uncheck();
    await keptBox(page, "raise_hand").check();
    await keptBox(page, "hangup").uncheck();
    await expect.poll(() => stored(page, KEPT_KEY)).not.toBeNull();

    await page.getByTestId("action-bar-kept-reset").click();
    await expect(keptBox(page, "mic")).toBeChecked();
    await expect(keptBox(page, "hangup")).toBeChecked();
    await expect(keptBox(page, "raise_hand")).not.toBeChecked();
    await expect(page.getByTestId("action-bar-kept-status")).toContainText(
      "Kept buttons reset to default.",
    );
    expect(await stored(page, KEPT_KEY)).toBeNull();
    await expect(shrinkSwitch(page)).toBeChecked();
    await closePreferences(page);

    await parkPointer(page);
    await expect(nav(page)).toHaveClass(/\bauto-shrink\b/);
    await expectShrunkTo(page, DEFAULT_KEPT);

    await openPreferences(page);
    await keptBox(page, "camera").uncheck();
    await keptBox(page, "settings").check();
    await expect.poll(() => stored(page, KEPT_KEY)).not.toBeNull();
    await closePreferences(page);
    await parkPointer(page);
    await expectShrunkTo(page, ["hangup-wrapper", "mic", "screen", "settings"]);

    await openDockMenu(page);
    await page.locator(".glass-select-option").filter({ hasText: "Reset Buttons" }).click();
    await parkPointer(page);
    await expect.poll(() => stored(page, KEPT_KEY)).toBeNull();
    expect(await stored(page, AUTOSHRINK_KEY)).toBe("true");
    await expect(nav(page)).toHaveClass(/\bauto-shrink\b/);
    await expectShrunkTo(page, DEFAULT_KEPT);
  });

  test("the dock menu item toggles shrinking and stays in step with Preferences", async ({
    page,
  }) => {
    await joinWith(page, "menu_item");
    const item = page.locator(AUTOSHRINK_ITEM);

    await openDockMenu(page);
    await expect(item).toHaveAttribute("role", "menuitemcheckbox");
    await expect(item).toHaveAttribute("aria-checked", "false");
    await expect(item).not.toHaveClass(/\bselected\b/);

    await item.click();
    await expect(item).toHaveCount(0);
    await expect(nav(page)).toHaveClass(/\bauto-shrink\b/);
    expect(await stored(page, AUTOSHRINK_KEY)).toBe("true");

    await openDockMenu(page);
    await expect(item).toHaveAttribute("aria-checked", "true");
    await expect(item).toHaveClass(/\bselected\b/);

    await page
      .locator(".glass-select-option")
      .filter({ hasText: /Action Bar/ })
      .click();
    await expect(page.locator("#settings-panel-preferences")).toBeVisible({ timeout: 10_000 });
    await expect(shrinkSwitch(page)).toBeChecked();
    await clickShrinkSwitch(page);
    await expect(shrinkSwitch(page)).not.toBeChecked();
    await closePreferences(page);
    await expect(nav(page)).not.toHaveClass(/\bauto-shrink\b/);
    expect(await stored(page, AUTOSHRINK_KEY)).toBe("false");

    await openDockMenu(page);
    await expect(item).toHaveAttribute("aria-checked", "false");
    await expect(item).not.toHaveClass(/\bselected\b/);
  });

  test("keyboard: Enter and Space toggle the menu item, and focus returns to the trigger", async ({
    page,
  }) => {
    await joinShrunk(page, "menu_keys");
    const trigger = page.locator(DOCK_TRIGGER);
    const item = page.locator(AUTOSHRINK_ITEM);
    await tabUntil(page, page.locator(`${DOCK_TRIGGER}:focus`), "the dock menu trigger");

    for (const [key, before, after] of [
      ["Enter", "true", "false"],
      ["Space", "false", "true"],
    ]) {
      await page.keyboard.press("Enter");
      await expect(item).toHaveAttribute("aria-checked", before);
      await arrowDownTo(page, item);
      await page.keyboard.press(key);
      await expect(item).toHaveCount(0);
      await expect(trigger, `${key} returns focus to the trigger`).toBeFocused();
      expect(await stored(page, AUTOSHRINK_KEY)).toBe(after);
    }

    await page.keyboard.press("Enter");
    await expect(item).toHaveAttribute("aria-checked", "true");
    await expect(item).toHaveClass(/\bselected\b/);
  });

  test("keyboard: Reset Buttons resets the kept set", async ({ page }) => {
    await joinShrunk(page, "reset_keys", {
      kept: ["settings"],
      seed: { [KEPT_KEY]: JSON.stringify({ slots: ["settings"], hangup: false }) },
    });
    await tabUntil(page, page.locator(`${DOCK_TRIGGER}:focus`), "the dock menu trigger");

    await page.keyboard.press("Enter");
    await expect(page.locator(MENU)).toBeVisible();
    await arrowDownTo(page, page.locator(`${MENU} .glass-select-option`, { hasText: "Reset" }));
    await page.keyboard.press("Enter");

    await expect.poll(() => stored(page, KEPT_KEY)).toBeNull();
    await expect(page.locator(MENU)).toHaveCount(0);
    await tabUntil(page, page.locator(`${NAV}:not(:focus-within)`), "something past the bar");
    await expectShrunkTo(page, DEFAULT_KEPT);
  });

  test("keyboard: Tab opens the bar around a control, a clipped control takes focus, Tab out shrinks", async ({
    page,
  }) => {
    await joinShrunk(page, "keyboard");

    await tabUntil(page, page.locator(`${NAV}:focus-within`), "the action bar");
    expect(await hoverOpenAttr(page), "no pointer took part").toBe(false);
    await expectExpanded(page);

    const reactions = slotButton(page, "reactions");
    await tabUntil(
      page,
      page.locator(`${NAV} > [data-slot="reactions"]:focus-within`),
      "Reactions",
    );
    await expect(reactions, "focus stays on the control, not <body>").toBeFocused();
    await expectExpanded(page);

    // Past the hover grace, so nothing but focus is holding it open.
    await page.waitForTimeout(HOVER_CLOSE_MS + SETTLE_MS);
    await expect(reactions).toBeFocused();
    await expectExpanded(page);
    expect(await hoverOpenAttr(page), "no pointer took part").toBe(false);

    await tabUntil(page, page.locator(`${NAV}:not(:focus-within)`), "something past the bar");
    await expectShrunkTo(page, DEFAULT_KEPT);
  });

  test("Escape shrinks a hover-opened bar, and it stays shrunk until the next enter", async ({
    page,
  }) => {
    await joinShrunk(page, "escape");
    await openBarByHover(page);
    await expectExpanded(page);

    await page.keyboard.press("Escape");
    await expect.poll(() => hoverOpenAttr(page), { message: "Escape dismissed it" }).toBe(false);
    await expectShrunkTo(page, DEFAULT_KEPT);

    const { x, y } = await centerOf(nav(page));
    await page.mouse.move(x + 1, y);
    await page.waitForTimeout(HOVER_OPEN_MS + SETTLE_MS);
    expect(await hoverOpenAttr(page), "a move inside is not an enter").toBe(false);
    await expectShrunkTo(page, DEFAULT_KEPT);

    await parkPointer(page);
    await nav(page).hover();
    await expect.poll(() => hoverOpenAttr(page)).toBe(true);
    await expectExpanded(page);
  });

  for (const { name, trigger, surface, viewport } of POPOVERS.filter((p) => p.escapes)) {
    test(`Escape closes the ${name} and does not collapse the bar`, async ({ page }) => {
      await joinShrunk(page, "escape_popover", {
        viewport,
        kept: viewport ? NO_SHARE : DEFAULT_KEPT,
      });
      await openBarByHover(page);
      await page.locator(trigger).click();
      await expect(page.locator(surface)).toBeVisible();

      await page.keyboard.press("Escape");
      await expect(page.locator(surface)).toHaveCount(0);
      await page.waitForTimeout(SETTLE_MS);
      expect(await hoverOpenAttr(page), "the popover took that Escape").toBe(true);
      await expectNothingClipped(page);
    });
  }

  for (const { name, trigger, surface, viewport, outsideNav, closedByTrigger } of POPOVERS) {
    test(`the ${name} holds the bar open past the grace, and closing it shrinks the bar`, async ({
      page,
    }) => {
      const kept = viewport ? NO_SHARE : DEFAULT_KEPT;
      await joinShrunk(page, "popover_pin", { viewport, kept });
      await openBarByHover(page);
      await page.locator(trigger).click();
      await expect(page.locator(surface)).toBeVisible({ timeout: 10_000 });
      if (outsideNav) await expect(nav(page).locator(surface), "premise").toHaveCount(0);

      await parkPointer(page);
      await expect
        .poll(() => hoverOpenAttr(page), { message: "premise: the hover state has ended" })
        .toBe(false);
      await expect(nav(page)).toHaveClass(/\bcontrols-expanded\b/);
      await expect(page.locator(surface)).toBeVisible();
      await expectNothingClipped(page);

      if (closedByTrigger) await page.locator(trigger).click();
      else await page.locator("#grid-container").click(GRID_CORNER);
      await expect(page.locator(surface)).toHaveCount(0);
      await parkPointer(page);
      await expect(nav(page)).not.toHaveClass(/\bcontrols-expanded\b/);
      await expectFullWidth(page, kept);
    });
  }

  test("a popover closing with the pointer off the bar drops mouse focus; keyboard focus stays", async ({
    page,
  }) => {
    await joinShrunk(page, "class_collapse");
    const trigger = page.locator(REACTIONS_TRIGGER);
    const palette = page.locator(REACTIONS_PALETTE);

    await openBarByHover(page);
    await trigger.click();
    await expect(palette).toBeVisible();
    // A reaction clicked with the mouse leaves the pointer on the palette, outside the
    // nav, and the palette's auto-hide then restores focus to the trigger.
    await page.locator('[data-testid="reaction-option-thumbs_up"]').click();
    await expect
      .poll(() => hoverOpenAttr(page), { message: "premise: only the palette holds the bar" })
      .toBe(false);
    await expect(palette).toHaveCount(0, { timeout: 9_000 });

    await expectShrunkTo(page, DEFAULT_KEPT);
    await expect(trigger, "a clipped control must not hold mouse focus").not.toBeFocused();
    await page.keyboard.press("Space");
    await waitOutClickGuard(page);
    await expect(palette, "Space must not reopen what nobody can see").toHaveCount(0);

    await tabUntil(page, page.locator(`${REACTIONS_TRIGGER}:focus`), "the reactions trigger");
    await page.keyboard.press("Enter");
    await expect(palette).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(palette).toHaveCount(0);
    await waitOutClickGuard(page);
    await expect(trigger, "keyboard focus survives the same collapse").toBeFocused();
    await expectExpanded(page);
  });

  test("a popover closing starts the click guard", async ({ page }) => {
    await joinShrunk(page, "guard_class");
    const micWasOn = await isMicOn(page);
    await openBarByHover(page);
    await page.locator(REACTIONS_TRIGGER).click();
    await expect(page.locator(REACTIONS_PALETTE)).toBeVisible();
    await parkPointer(page);
    await expect
      .poll(() => hoverOpenAttr(page), { message: "premise: only the palette holds the bar" })
      .toBe(false);

    await stampBarEvents(page);
    const unpinned = boxAtBarChange(page, "unpinned", slotSelector("mic"));
    await page.locator("#grid-container").click(GRID_CORNER);
    const { x, y } = middleOf(await unpinned);
    await Promise.all([page.mouse.move(x, y), page.mouse.down(), page.mouse.up()]);
    expect(await stampGap(page, "unpinned", "down"), "premise: inside the guard").toBeLessThan(
      CLICK_GUARD_MS,
    );

    await waitOutClickGuard(page);
    await expectMicOn(page, micWasOn);
    await slotButton(page, "mic").click();
    await expectMicOn(page, !micWasOn);
  });

  test("a press inside the intent delay does not expand the bar, and the button still acts", async ({
    page,
  }) => {
    await joinShrunk(page, "press_intent");
    const micWasOn = await isMicOn(page);

    await pressInsideIntent(page, slotButton(page, "mic"));
    await page.mouse.up();

    await expectMicOn(page, !micWasOn);
    await page.waitForTimeout(HOVER_OPEN_MS + SETTLE_MS);
    expect(await hoverOpenAttr(page), "the press cancelled the pending open").toBe(false);
    await expectShrunkTo(page, DEFAULT_KEPT);
  });

  test("the click guard goes by the press: a long press inside 400 ms does nothing, one just after acts", async ({
    page,
  }) => {
    await joinShrunk(page, "guard_press");
    const raise = slotButton(page, "raise_hand");
    await expect(raise).toHaveAttribute("data-raised", "false");

    await stampBarEvents(page);
    const opened = boxAtBarChange(page, "opened", slotSelector("raise_hand"));
    await nav(page).hover();
    const inside = middleOf(await opened);
    await Promise.all([page.mouse.move(inside.x, inside.y), page.mouse.down()]);
    expect(await stampGap(page, "open", "down"), "premise: pressed inside the guard").toBeLessThan(
      CLICK_GUARD_MS,
    );
    await waitOutClickGuard(page);
    await page.mouse.up();
    await waitOutClickGuard(page);
    await expect(raise, "the guard swallowed that press").toHaveAttribute("data-raised", "false");

    await parkPointer(page);
    await expectShrunkTo(page, DEFAULT_KEPT);
    await waitOutClickGuard(page);

    await stampBarEvents(page);
    const reopened = boxAtBarChange(page, "opened", slotSelector("raise_hand"));
    await nav(page).hover();
    const after = middleOf(await reopened);
    await page.waitForTimeout(CLICK_GUARD_MS + 20);
    await Promise.all([page.mouse.move(after.x, after.y), page.mouse.down(), page.mouse.up()]);
    expect(await stampGap(page, "open", "down"), "premise: past the guard").toBeGreaterThanOrEqual(
      CLICK_GUARD_MS,
    );
    await expect(raise, "a press after the guard acts").toHaveAttribute("data-raised", "true");
  });

  test("keyboard activation inside the click guard still acts", async ({ page }) => {
    await joinShrunk(page, "guard_keyboard");
    const micWasOn = await isMicOn(page);

    await pressInsideIntent(page, slotButton(page, "mic"));
    await page.mouse.up();
    await expectMicOn(page, !micWasOn);
    await parkPointer(page);
    await waitOutClickGuard(page);
    await expect(slotButton(page, "mic"), "premise: a kept control keeps its focus").toBeFocused();

    await stampBarEvents(page);
    const opened = boxAtBarChange(page, "opened", slotSelector("mic"));
    await nav(page).hover();
    await opened;
    await page.keyboard.press("Enter");
    expect(await stampGap(page, "open", "key"), "premise: inside the guard").toBeLessThan(
      CLICK_GUARD_MS,
    );
    await expectMicOn(page, micWasOn);
  });

  test("Escape that drops Hang up under a resting pointer: a click inside 400 ms stays in the call", async ({
    page,
  }) => {
    await joinShrunk(page, "guard_hangup");
    const hangupSelector = `${NAV} > .hangup-wrapper`;
    const landing = await boxOf(page.locator(hangupSelector));
    const y = Math.round(landing.y + landing.height / 2);

    await page.mouse.move(Math.round(landing.x + landing.width / 2), y);
    await expect.poll(() => hoverOpenAttr(page)).toBe(true);
    await waitOutClickGuard(page);

    // A point Hang up will cover once it lands, on a non-kept control for now.
    const x = await page.evaluate(
      ({ from, to, atY }) => {
        for (let at = from; at <= to; at += 2) {
          if (document.elementFromPoint(at, atY)?.closest(".slot-shrinkable")) return at;
        }
        return null;
      },
      { from: Math.ceil(landing.x), to: Math.floor(landing.x + landing.width), atY: y },
    );
    if (x === null) throw new Error("no non-kept control overlaps where Hang up lands");
    await page.mouse.move(x, y);

    await stampBarEvents(page);
    const closed = boxAtBarChange(page, "closed", hangupSelector);
    await page.keyboard.press("Escape");
    const landed = await closed;
    expect(x, "premise: Hang up landed under the pointer").toBeGreaterThanOrEqual(landed.x);
    expect(x, "premise: Hang up landed under the pointer").toBeLessThanOrEqual(
      landed.x + landed.width,
    );

    await Promise.all([page.mouse.down(), page.mouse.up()]);
    expect(await stampGap(page, "close", "down"), "premise: inside the guard").toBeLessThan(
      CLICK_GUARD_MS,
    );
    await waitOutClickGuard(page);
    await expect(nav(page), "the guard kept this user in the call").toHaveCount(1);
    await expect(page.locator("#grid-container")).toBeVisible();

    await page.mouse.click(x, y);
    await expect(nav(page), "the same click after the guard hangs up").toHaveCount(0, {
      timeout: 10_000,
    });
  });

  test("a mouse-clicked control loses focus when the bar shrinks, and a keyboard path keeps it", async ({
    page,
  }) => {
    await joinShrunk(page, "hidden_focus");
    const raise = slotButton(page, "raise_hand");

    await openBarByHover(page);
    await raise.click();
    await expect(raise).toHaveAttribute("data-raised", "true");
    await expect(raise, "premise: the click focused it").toBeFocused();

    await parkPointer(page);
    await expectShrunkTo(page, DEFAULT_KEPT);
    await expect(raise, "a clipped control must not hold mouse focus").not.toBeFocused();
    await page.keyboard.press("Space");
    await page.waitForTimeout(SETTLE_MS);
    await expect(raise, "Space must not fire a control nobody can see").toHaveAttribute(
      "data-raised",
      "true",
    );

    await tabUntil(page, page.locator(`${NAV} > [data-slot="raise_hand"]:focus-within`), "Raise");
    await expect(raise).toBeFocused();
    await page.waitForTimeout(HOVER_CLOSE_MS + SETTLE_MS);
    await expect(raise, "keyboard focus stays").toBeFocused();
    await expectExpanded(page);
    await page.keyboard.press("Space");
    await expect(raise, "and acts where it can be seen").toHaveAttribute("data-raised", "false");
  });

  test("Diagnostics' close button on a collapsed bar keeps mouse focus off its clipped trigger, and keyboard focus lands on it", async ({
    page,
  }) => {
    await joinShrunk(page, "diagnostics_close");
    const trigger = slotButton(page, "diagnostics");
    const drawer = page.locator("#diagnostics-sidebar");
    const closeButton = drawer.locator(
      '.sidebar-header > button.close-button[aria-label="Close panel"]',
    );
    const openOverCollapsedBar = async () => {
      await openBarByHover(page);
      await trigger.click();
      await expect(drawer).toHaveClass(/\bvisible\b/);
      await parkPointer(page);
      await expect
        .poll(() => hoverOpenAttr(page), { message: "premise: the hover state has ended" })
        .toBe(false);
      await expect(nav(page), "premise: nothing holds the bar").not.toHaveClass(
        /\bcontrols-expanded\b/,
      );
      await waitOutClickGuard(page);
    };

    await openOverCollapsedBar();
    await closeButton.click();
    await expect(drawer).not.toHaveClass(/\bvisible\b/);
    await expect
      .poll(async () => (await barWidths(page)).clipped, { message: "premise: still clipped" })
      .toContain("diagnostics");
    await page.waitForTimeout(SETTLE_MS);
    await expect(trigger, "a clipped control must not hold mouse focus").not.toBeFocused();
    await page.keyboard.press("Space");
    await waitOutClickGuard(page);
    await expect(drawer, "Space must not reopen what nobody can see").not.toHaveClass(
      /\bvisible\b/,
    );

    await openOverCollapsedBar();
    await closeButton.focus();
    await page.keyboard.press("Enter");
    await expect(drawer).not.toHaveClass(/\bvisible\b/);
    await expect(trigger, "keyboard focus lands on the trigger").toBeFocused();
    await expectExpanded(page);
  });

  test("auto-hide alone fades the bar and never shrinks it, though the same bar can", async ({
    page,
  }) => {
    await joinWith(page, "autohide_alone", { [AUTOHIDE_KEY]: "true", [AUTOSHRINK_KEY]: "false" });

    await expect(nav(page)).toHaveClass(/\bcontrols-hidden\b/, { timeout: 10_000 });
    await expect(nav(page)).not.toHaveClass(/\bauto-shrink\b/);
    await expect(nav(page)).toHaveClass(/\bcontrols-expanded\b/);
    await expect(handle(page)).toHaveCount(0);
    await expect(
      page.locator(`${NAV} > [data-slot="settings"].slot-shrinkable`),
      "premise: non-kept controls can shrink, and only the setting stops them",
    ).toHaveCount(1);
    await expectExpanded(page);

    await openDockMenu(page);
    await page.locator(AUTOSHRINK_ITEM).click();
    await parkPointer(page);
    await expectShrunkTo(page, DEFAULT_KEPT);
  });

  test("with auto-hide and auto-shrink on, hover wakes and expands, and leaving shrinks then fades", async ({
    page,
  }) => {
    await joinWith(page, "hide_and_shrink", { [AUTOHIDE_KEY]: "true", [AUTOSHRINK_KEY]: "true" });
    await expect(nav(page)).toHaveClass(/\bcontrols-hidden\b/, { timeout: 10_000 });
    await expectShrunkTo(page, DEFAULT_KEPT);

    await openBarByHover(page);
    await expect(nav(page)).not.toHaveClass(/\bcontrols-hidden\b/);
    await expectExpanded(page);

    await parkPointer(page);
    await expectShrunkTo(page, DEFAULT_KEPT);
    await expect(nav(page), "shrinks before it fades").not.toHaveClass(/\bcontrols-hidden\b/);
    await expect(nav(page)).toHaveClass(/\bcontrols-hidden\b/, { timeout: 10_000 });
    await expectShrunkTo(page, DEFAULT_KEPT);
  });

  test("a stored auto-hide of true turns shrinking on once: turning auto-hide off and reloading keeps it", async ({
    page,
  }) => {
    await joinWith(page, "inherits", { [AUTOHIDE_KEY]: "true" });
    await expect(nav(page)).toHaveClass(/\bauto-shrink\b/);
    expect(await stored(page, AUTOSHRINK_KEY)).toBe("true");
    await expectShrunkTo(page, DEFAULT_KEPT);

    await openDockMenu(page);
    await page.locator(AUTOHIDE_ITEM).click();
    await expect.poll(() => stored(page, AUTOHIDE_KEY)).toBe("false");

    await page.reload();
    await enterGrid(page);
    await parkPointer(page);
    await expect(nav(page), "auto-hide is off, and shrinking stays on").toHaveClass(
      /\bauto-shrink\b/,
    );
    expect(await stored(page, AUTOSHRINK_KEY)).toBe("true");
    await expectShrunkTo(page, DEFAULT_KEPT);
  });

  test("turning auto-hide on later does not turn shrinking on", async ({ page }) => {
    await joinWith(page, "not_inherited");
    await expect.poll(() => stored(page, AUTOSHRINK_KEY)).toBe("false");

    await openDockMenu(page);
    await page.locator(AUTOHIDE_ITEM).click();
    await expect.poll(() => stored(page, AUTOHIDE_KEY)).toBe("true");

    await page.reload();
    await enterGrid(page);
    await parkPointer(page);
    await expect(nav(page)).not.toHaveClass(/\bauto-shrink\b/);
    expect(await stored(page, AUTOSHRINK_KEY)).toBe("false");
    await expectExpanded(page);
  });

  test("holding a kept button on a shrunk bar enters edit mode, which shows every button", async ({
    page,
  }) => {
    await joinShrunk(page, "hold");
    const micWasOn = await isMicOn(page);

    await pressInsideIntent(page, slotButton(page, "mic"));
    await expect(nav(page)).toHaveClass(/\bcustomize-mode\b/, { timeout: HOLD_MS + 3_000 });
    await page.mouse.up();

    await expect(handle(page)).toHaveCount(0);
    await expectExpanded(page);
    await expectMicOn(page, micWasOn);
  });

  test("Recent actions works from a shrunk bar: Tab reveals its trigger, and an entry acts once hovered open", async ({
    page,
  }) => {
    await joinShrunk(page, "recents", {
      seed: { vc_recent_actions: JSON.stringify(["slot:participants"]) },
    });
    const trigger = page.locator("#recent-actions-trigger");
    const menu = page.locator("#recent-actions-popover");
    const entry = menu.locator(
      '[role="menu"] > .recent-actions-row > button[data-recent-action="slot:participants"]',
    );
    expect((await barWidths(page)).clipped, "premise: its trigger is clipped").toContain(
      "recent_actions",
    );

    await tabUntil(page, page.locator("#recent-actions-trigger:focus"), "Recent actions");
    await expectExpanded(page);
    await page.keyboard.press("Enter");
    await expect(menu).toBeVisible();
    await expect(entry).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(menu).toHaveCount(0);
    await expect(trigger, "Escape returns focus to the trigger").toBeFocused();
    await expectExpanded(page);

    await tabUntil(page, page.locator(`${NAV}:not(:focus-within)`), "something past the bar");
    await expectShrunkTo(page, DEFAULT_KEPT);
    await openBarByHover(page);
    await trigger.click();
    await expect(entry).toBeVisible();
    await entry.click();
    await expect(page.locator("#peer-list-container")).toHaveClass(/\bvisible\b/);
    await expect(menu).toHaveCount(0);
  });

  test("in a narrow window Share outlasts the non-kept buttons, and goes below 484 px", async ({
    page,
  }) => {
    await joinShrunk(page, "narrow", { viewport: { width: 768, height: 800 } });

    const shareStays = {
      full: DEFAULT_KEPT,
      gone: ["reactions", "raise_hand", "participants", "meeting_timer"],
      staying: ["screen"],
    };
    const shareGone = { full: NO_SHARE, gone: ["screen", "reactions"], staying: [] };
    await expectOverflowAtWidth(page, 768, {
      full: DEFAULT_KEPT,
      gone: ["diagnostics", "meeting_timer"],
      staying: ["screen", "reactions"],
    });
    await expectOverflowAtWidth(page, 484, shareStays);
    await expectOverflowAtWidth(page, 390, shareGone);
    await expectOverflowAtWidth(page, 484, shareStays);
    await expectOverflowAtWidth(page, 320, shareGone);
    await expectOverflowAtWidth(page, 484, shareStays);
    await expectOverflowAtWidth(page, 483, shareGone);
  });

  for (const dock of ["left", "right"] as const) {
    test(`on the ${dock} dock the bar shrinks, expands on hover, and shrinks when the mouse leaves`, async ({
      page,
    }) => {
      await joinShrunk(page, `dock_${dock}`, { viewport: TALL, seed: { [DOCK_KEY]: dock } });
      await expect(nav(page)).toHaveClass(new RegExp(`\\bdock-${dock}\\b`));
      const shrunk = await boxOf(nav(page));

      await nav(page).hover();
      await expect.poll(() => hoverOpenAttr(page)).toBe(true);
      await expectExpanded(page);
      expect((await boxOf(nav(page))).height).toBeGreaterThan(shrunk.height);

      await parkPointer(page);
      await expectShrunkTo(page, DEFAULT_KEPT);
    });

    for (const height of [600, 950]) {
      test(`the ${dock} dock's menu stays on screen at ${height} px high, and Action Bar… clicks`, async ({
        page,
      }) => {
        await page.setViewportSize({ width: 1280, height });
        await joinWith(page, `menu_${dock}_${height}`, { [DOCK_KEY]: dock });
        await openDockMenu(page);

        const box = await boxOf(page.locator(MENU));
        expect(box.y, "the menu's top").toBeGreaterThanOrEqual(0);
        expect(box.y + box.height, "the menu's bottom").toBeLessThanOrEqual(height);
        expect(box.x, "the menu's left").toBeGreaterThanOrEqual(0);
        expect(box.x + box.width, "the menu's right").toBeLessThanOrEqual(1280);

        await page
          .locator(".glass-select-option")
          .filter({ hasText: /Action Bar/ })
          .click({ timeout: 5_000 });
        await expect(page.locator("#settings-panel-preferences")).toBeVisible({ timeout: 10_000 });
      });
    }
  }

  test("with nothing kept the left dock's handle is vertical, 24 by 72", async ({ page }) => {
    await joinBare(page, "dock_handle", { [DOCK_KEY]: "left" });
    const hit = await boxOf(handle(page));
    expect([hit.width, hit.height]).toEqual([24, 72]);
  });

  for (const dock of ["bottom", "left", "right"] as const) {
    test(`Tab into a faded bar on the ${dock} dock restores its opacity and position`, async ({
      page,
    }) => {
      await page.setViewportSize(TALL);
      await joinWith(page, `fade_${dock}`, {
        [AUTOHIDE_KEY]: "true",
        [AUTOSHRINK_KEY]: "true",
        [DOCK_KEY]: dock,
      });
      await expectShrunkTo(page, DEFAULT_KEPT);

      const opacity = () => nav(page).evaluate((el) => getComputedStyle(el).opacity);
      const edge = async () => {
        const box = await boxOf(nav(page));
        if (dock === "left") return Math.round(box.x);
        if (dock === "right") return Math.round(box.x + box.width);
        return Math.round(box.y + box.height);
      };
      const resting = await edge();

      await expect(nav(page)).toHaveClass(/\bcontrols-hidden\b/, { timeout: 10_000 });
      await expect.poll(opacity).toBe("0.1");
      await expect.poll(edge, { message: "premise: the fade moved it" }).not.toBe(resting);

      await tabUntil(page, page.locator(`${NAV}:focus-within`), "the action bar");
      await expect(nav(page), "only focus changed").toHaveClass(/\bcontrols-hidden\b/);
      await expect.poll(opacity).toBe("1");
      await expect.poll(edge).toBe(resting);
      await expectExpanded(page);
    });
  }

  test.describe("with a coarse pointer", () => {
    test.use({ hasTouch: true });

    for (const { name, viewport, kept } of [
      { name: "a touch screen", viewport: undefined, kept: DEFAULT_KEPT },
      { name: "a phone", viewport: PHONE, kept: NO_SHARE },
    ]) {
      test(`on ${name} the handle expands, a tap outside shrinks, and a kept button acts without expanding`, async ({
        page,
      }) => {
        const shrunk = [...kept, HANDLE];
        await joinShrunk(page, "touch", { viewport, kept: shrunk });
        expect(
          await page.evaluate(() => matchMedia("(any-pointer: coarse)").matches),
          "premise: hasTouch reports a coarse pointer",
        ).toBe(true);

        const hit = await boxOf(handle(page));
        expect(hit.width, "handle hit area width").toBeGreaterThanOrEqual(44);
        expect(hit.height, "handle hit area height").toBeGreaterThanOrEqual(44);
        await expect(handle(page)).toHaveAttribute("aria-expanded", "false");

        await handle(page).tap();
        await expect(handle(page)).toHaveAttribute("aria-expanded", "true");
        await expect(nav(page)).toHaveClass(/\bcontrols-expanded\b/);
        await expectNothingClipped(page);

        await page.touchscreen.tap(20, 300);
        await expect(handle(page)).toHaveAttribute("aria-expanded", "false");
        await expect(nav(page)).not.toHaveClass(/\bcontrols-expanded\b/);
        await expectFullWidth(page, shrunk);

        // That collapse starts the click guard, which would swallow this tap.
        await waitOutClickGuard(page);
        const micWasOn = await isMicOn(page);
        await slotButton(page, "mic").tap();
        await expectMicOn(page, !micWasOn);
        await page.waitForTimeout(HOVER_OPEN_MS + SETTLE_MS);
        expect(await hoverOpenAttr(page), "a touch is not a hover").toBe(false);
        await expect(nav(page)).not.toHaveClass(/\bcontrols-expanded\b/);
        await expectFullWidth(page, shrunk);
      });
    }

    test("a mouse click on a bare bar's handle does not latch, so mouse off still shrinks", async ({
      page,
    }) => {
      await joinBare(page, "hybrid_bare");

      await handle(page).hover();
      await expect.poll(() => hoverOpenAttr(page)).toBe(true);
      await waitOutClickGuard(page);
      await handle(page).click();
      await page.waitForTimeout(SETTLE_MS);
      await expect(handle(page), "a mouse has hover, so no latch").toHaveAttribute(
        "aria-expanded",
        "false",
      );

      await parkPointer(page);
      await expect(nav(page)).not.toHaveClass(/\bcontrols-expanded\b/);
      await expectShrunkTo(page, [HANDLE]);
    });

    test("a mouse press on the handle inside the intent opens a non-bare bar, and mouse off shrinks it", async ({
      page,
    }) => {
      const shrunk = [...DEFAULT_KEPT, HANDLE];
      await joinShrunk(page, "hybrid_open", { kept: shrunk });

      await pressInsideIntent(page, handle(page));
      await page.mouse.up();
      await expect
        .poll(() => hoverOpenAttr(page), { message: "the click opens the bar at once" })
        .toBe(true);
      await expect(handle(page), "a mouse has hover, so no latch").toHaveAttribute(
        "aria-expanded",
        "false",
      );
      await expectExpanded(page);

      await parkPointer(page);
      await expect(nav(page)).not.toHaveClass(/\bcontrols-expanded\b/);
      await expectShrunkTo(page, shrunk);
    });

    test("with nothing kept the right dock's handle is vertical, 44 by 96", async ({ page }) => {
      await joinBare(page, "dock_handle_coarse", { [DOCK_KEY]: "right" });
      const hit = await boxOf(handle(page));
      expect([hit.width, hit.height]).toEqual([44, 96]);
    });

    test("in a narrow window Share outlasts the non-kept buttons, and goes below 599 px", async ({
      page,
    }) => {
      const stays = [...DEFAULT_KEPT, HANDLE];
      await joinShrunk(page, "narrow_coarse", {
        viewport: { width: 768, height: 800 },
        kept: stays,
      });

      const shareStays = { full: stays, gone: ["reactions", "meeting_timer"], staying: ["screen"] };
      const shareGone = { full: [...NO_SHARE, HANDLE], gone: ["screen"], staying: [] };
      await expectOverflowAtWidth(page, 768, {
        full: stays,
        gone: ["participants", "meeting_timer"],
        staying: ["screen", "reactions"],
      });
      await expectOverflowAtWidth(page, 599, shareStays);
      await expectOverflowAtWidth(page, 598, shareGone);
      await expectOverflowAtWidth(page, 599, shareStays);
      await expectOverflowAtWidth(page, 390, shareGone);
    });
  });
});
