import { test, expect, chromium, Browser, ElementHandle, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { wakeActionBar } from "../helpers/controls";
import { waitForServices } from "../helpers/wait-for-services";
import {
  admitGuestIfNeeded,
  clickJoinAndEnterGrid,
  joinMeetingAs,
  openShareMenu,
  startScreenShare,
  stopScreenShare,
} from "../helpers/screen-share-meeting";

/**
 * Issue 3173: while a share is live the share button opens a menu ("Share
 * something else", "Stop sharing") instead of stopping the share. The DOM
 * contract is `ScreenShareControl` / `ScreenShareMenu` in
 * video_control_buttons.rs; the swap is share_actions.rs `replace`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";
const TRIGGER = "#screen-share-trigger";
const MENU = '[data-testid="screen-share-menu"]';
const REPLACE = '[data-testid="screen-share-menu-replace"]';
const STOP = '[data-testid="screen-share-menu-stop"]';
const RECEIVED = '[data-testid="received-share-tile"]';
const REPLACED_TOAST = '[data-testid="screen-share-replaced-toast"]';
const OVERFLOW_TRIGGER = "#overflow-menu-trigger";
const OVERFLOW_POPOVER = ".action-bar-overflow-popover";
const OVERFLOW_GROUP = '[role="group"][aria-label="Screen share"]';
const OVERFLOW_REPLACE = '[data-testid="overflow-share-replace"]';
const OVERFLOW_STOP = '[data-testid="overflow-share-stop"]';
const OVERFLOW_START = '[data-testid="overflow-share-start"]';
const PHONE = { width: 390, height: 844 };
const PRESENTER = "MenuPresenter";

// Each pick returns a new capture: blue first, red after. It logs the trusted event it was
// called inside, or null. window.__holdNextPick keeps the next pick open (the picker is
// showing) until window.__releasePick().
const TWO_PICK_DISPLAY_MEDIA_SCRIPT = `
(() => {
  const md = navigator.mediaDevices;
  if (!md) return;
  window.__e2eSharePicks = 0;
  window.__e2ePickEvents = [];
  const colors = ['#1f4bd8', '#d81f1f'];
  Object.defineProperty(md, 'getDisplayMedia', {
    configurable: true,
    value: async () => {
      const e = window.event;
      window.__e2ePickEvents.push(e && e.isTrusted ? e.type : null);
      const color = colors[Math.min(window.__e2eSharePicks, colors.length - 1)];
      window.__e2eSharePicks += 1;
      if (window.__holdNextPick) {
        window.__holdNextPick = false;
        await new Promise((resolve) => { window.__releasePick = resolve; });
        window.__releasePick = undefined;
      }
      const c = document.createElement('canvas');
      c.width = 640; c.height = 360;
      const x = c.getContext('2d');
      const stream = c.captureStream(0);
      const track = stream.getVideoTracks()[0];
      let n = 0;
      const tick = () => {
        if (track.readyState === 'ended') return;
        n++;
        x.fillStyle = color; x.fillRect(0, 0, 640, 360);
        x.fillStyle = '#ffffff'; x.fillRect((n * 12) % 600, 300, 30, 30);
        try { track.requestFrame(); } catch (_) {}
        setTimeout(tick, 80);
      };
      tick();
      return stream;
    },
  });
})();
`;

interface MenuMeeting {
  presenter: Page;
  receiver: Page;
  close: () => Promise<void>;
}

async function openMenuMeeting(
  baseURL: string | undefined,
  label: string,
  share = true,
): Promise<MenuMeeting> {
  const uiURL = baseURL || DEFAULT_UI_URL;
  const browsers: Browser[] = [];
  const close = async (): Promise<void> => {
    await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
  };
  try {
    browsers.push(await chromium.launch({ args: BROWSER_ARGS }));
    browsers.push(await chromium.launch({ args: BROWSER_ARGS }));
    const receiverCtx = await createAuthenticatedContext(
      browsers[0],
      `menu-${label}-receiver@videocall.rs`,
      "MenuReceiver",
      uiURL,
    );
    const presenterCtx = await createAuthenticatedContext(
      browsers[1],
      `menu-${label}-presenter@videocall.rs`,
      PRESENTER,
      uiURL,
    );
    await presenterCtx.addInitScript(TWO_PICK_DISPLAY_MEDIA_SCRIPT);
    const meetingId = `e2e_share_menu_${label}_${Date.now()}`;
    const receiver = await joinMeetingAs(receiverCtx, meetingId, "MenuReceiver");
    await clickJoinAndEnterGrid(receiver);
    const presenter = await joinMeetingAs(presenterCtx, meetingId, PRESENTER);
    await admitGuestIfNeeded(receiver, presenter);
    await expect(receiver.locator('#grid-container [id^="peer-video-"]').first()).toBeVisible({
      timeout: 30_000,
    });
    if (share) {
      const shared = await startScreenShare(presenter, receiver);
      expect(shared, "the receiver never got the share").toBe(true);
      await expect(presenter.locator(TRIGGER)).toHaveAttribute("data-state", "active", {
        timeout: 10_000,
      });
    }
    return { presenter, receiver, close };
  } catch (err) {
    await close();
    throw err;
  }
}

// "blue" / "red" for the received share's decoded pixels, null until painted.
function receivedColor(page: Page): Promise<string | null> {
  return page.evaluate((sel) => {
    const c = document.querySelector(`${sel} canvas[id^="screen-share-"]`) as HTMLCanvasElement;
    if (!c || c.width === 0) return null;
    const t = document.createElement("canvas");
    t.width = 16;
    t.height = 9;
    const x = t.getContext("2d");
    if (!x) return null;
    x.drawImage(c, 0, 0, 16, 9);
    const d = x.getImageData(0, 0, 16, 9).data;
    let r = 0;
    let b = 0;
    for (let i = 0; i < d.length; i += 4) {
      r += d[i];
      b += d[i + 2];
    }
    const n = d.length / 4;
    if (r / n > b / n + 60) return "red";
    if (b / n > r / n + 60) return "blue";
    return null;
  }, RECEIVED);
}

const pickEvents = (p: Page): Promise<(string | null)[]> =>
  p.evaluate(() => (window as unknown as { __e2ePickEvents: (string | null)[] }).__e2ePickEvents);

type PickHold = { __holdNextPick?: boolean; __releasePick?: () => void };

function holdNextPick(p: Page): Promise<void> {
  return p.evaluate(() => {
    (window as unknown as PickHold).__holdNextPick = true;
  });
}

async function releasePick(p: Page): Promise<void> {
  await expect
    .poll(() => p.evaluate(() => typeof (window as unknown as PickHold).__releasePick))
    .toBe("function");
  await p.evaluate(() => (window as unknown as PickHold).__releasePick!());
}

// Busy share items in the overflow: aria-busy, aria-disabled, labels at 0.7.
async function expectBusy(item: Locator): Promise<void> {
  await expect(item).toHaveAttribute("aria-busy", "true");
  await expect(item).toHaveAttribute("aria-disabled", "true");
  expect(await item.locator(":scope > span").evaluate((el) => getComputedStyle(el).opacity)).toBe(
    "0.7",
  );
}

// Records the receiver's share-tile count and anything it says.
async function watchReceiverDuringSwap(r: Page): Promise<ElementHandle> {
  const tile = await r.locator(RECEIVED).elementHandle();
  expect(tile, "the received share tile must be mounted").not.toBeNull();
  await r.evaluate((sel) => {
    const log = { min: Number.POSITIVE_INFINITY, max: 0, said: [] as string[] };
    const announced = (): string =>
      document.querySelector('[data-testid="ss-detach-announce"]')?.textContent ?? "";
    const toasts = (): string[] =>
      Array.from(document.querySelectorAll(".peer-toasts .peer-toast")).map(
        (t) => t.textContent ?? "",
      );
    const firstAnnounced = announced();
    const firstToasts = new Set(toasts());
    const note = (text: string): void => {
      if (!log.said.includes(text)) log.said.push(text);
    };
    (window as unknown as { __swapLog: typeof log }).__swapLog = log;
    setInterval(() => {
      const n = document.querySelectorAll(sel).length;
      log.min = Math.min(log.min, n);
      log.max = Math.max(log.max, n);
      const now = announced();
      if (now !== firstAnnounced) note(now);
      toasts()
        .filter((t) => !firstToasts.has(t))
        .forEach(note);
    }, 100);
  }, RECEIVED);
  return tile!;
}

async function expectSwapUnnoticed(r: Page, tile: ElementHandle): Promise<void> {
  await expect
    .poll(() => receivedColor(r), {
      timeout: 20_000,
      message: "the receiver never showed the new capture",
    })
    .toBe("red");
  await r.waitForTimeout(3_000);
  const log = await r.evaluate(
    () =>
      (window as unknown as { __swapLog: { min: number; max: number; said: string[] } }).__swapLog,
  );
  expect(log.min, "the receiver's share tile went away during the swap").toBe(1);
  expect(log.max, "a second share tile appeared during the swap").toBe(1);
  expect(
    log.said.filter((s) => /stopped sharing|started sharing/.test(s)),
    "the receiver must not hear the presenter stop or start",
  ).toEqual([]);
  expect(
    await tile.evaluate((el) => el.isConnected),
    "the swap remounted the receiver's share tile",
  ).toBe(true);
}

async function openOverflow(page: Page): Promise<Locator> {
  await wakeActionBar(page);
  const trigger = page.locator(OVERFLOW_TRIGGER);
  await expect(trigger).toBeVisible({ timeout: 10_000 });
  await trigger.click();
  const popover = page.locator(OVERFLOW_POPOVER);
  await expect(popover).toBeVisible({ timeout: 5_000 });
  return popover;
}

test.describe("Issue 3173: the live share's menu", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("the share button opens a menu by click and keyboard, closes back to the button, and Stop sharing ends the share", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const m = await openMenuMeeting(baseURL, "keys");
    try {
      const p = m.presenter;
      const trigger = p.locator(TRIGGER);
      const menu = p.locator(MENU);
      await expect(trigger).toHaveAttribute("aria-label", "Screen share — Sharing options");
      await expect(trigger).toHaveAttribute("aria-haspopup", "menu");
      await expect(trigger).toHaveAttribute("aria-expanded", "false");
      await expect(trigger).not.toHaveAttribute("aria-controls");
      await expect(trigger).toHaveAccessibleDescription(/Share something else or stop sharing\./);

      await openShareMenu(p);
      await expect(trigger).toHaveAttribute("aria-expanded", "true");
      await expect(trigger).toHaveAttribute("aria-controls", "screen-share-menu");
      await expect(menu.locator(REPLACE)).toHaveText("Share something else");
      await expect(menu.locator(STOP)).toHaveText("Stop sharing");
      await expect(menu.locator(REPLACE)).toBeFocused();
      await expect(trigger, "opening the menu must not stop the share").toHaveAttribute(
        "data-state",
        "active",
      );
      await trigger.click();
      await expect(menu).toHaveCount(0);
      await expect(trigger).toHaveAttribute("aria-expanded", "false");
      await expect(trigger).toBeFocused();
      await expect(m.receiver.locator(RECEIVED)).toHaveCount(1);

      await trigger.focus();
      await p.keyboard.press("Enter");
      await expect(menu.locator(REPLACE)).toBeFocused();
      await p.keyboard.press("ArrowDown");
      await expect(menu.locator(STOP)).toBeFocused();
      await p.keyboard.press("ArrowDown");
      await expect(menu.locator(REPLACE), "ArrowDown wraps to the first item").toBeFocused();
      await p.keyboard.press("Escape");
      await expect(menu).toHaveCount(0);
      await expect(trigger).toBeFocused();

      await p.keyboard.press("ArrowDown");
      await expect(menu.locator(REPLACE)).toBeFocused();
      await p.keyboard.press("Tab");
      await expect(menu).toHaveCount(0);
      await expect(trigger).toBeFocused();

      await p.keyboard.press("ArrowUp");
      await expect(menu.locator(STOP), "ArrowUp opens on the last item").toBeFocused();
      await p.keyboard.press("Enter");
      await expect(trigger).toHaveAttribute("data-state", "idle", { timeout: 10_000 });
      await expect(trigger).toHaveAttribute("aria-label", "Screen share — Share Screen");
      await expect(trigger).toBeFocused();
      await expect(menu).toHaveCount(0);
      await expect(m.receiver.locator(RECEIVED)).toHaveCount(0, { timeout: 20_000 });
    } finally {
      await m.close();
    }
  });

  test("Share something else swaps the capture in place: the receiver keeps one share tile and hears no stop or start", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const m = await openMenuMeeting(baseURL, "swap");
    try {
      const p = m.presenter;
      const r = m.receiver;
      await expect.poll(() => receivedColor(r), { timeout: 20_000 }).toBe("blue");
      await expect(p.locator(".peer-toast.toast-loading.screen-share-toast")).toHaveCount(0, {
        timeout: 20_000,
      });
      const tile = await watchReceiverDuringSwap(r);

      const menu = await openShareMenu(p);
      await menu.locator(REPLACE).click();
      const toast = p.locator(REPLACED_TOAST);
      await expect(toast).toBeVisible({ timeout: 10_000 });
      await expect(toast).toHaveText(/^Switched to sharing /);
      await expect(toast).toHaveAttribute("aria-hidden", "true");
      await expect(toast).not.toHaveAttribute("role");
      expect(
        await p.evaluate(() => (window as unknown as { __e2eSharePicks: number }).__e2eSharePicks),
      ).toBe(2);
      expect(await pickEvents(p), "each pick must run inside its click").toEqual([
        "click",
        "click",
      ]);
      await expect(p.locator(TRIGGER)).toHaveAttribute("data-state", "active");
      await expectSwapUnnoticed(r, tile);

      await openShareMenu(p);
      await stopScreenShare(p);
      await expect(r.locator(RECEIVED), "the swapped share stops from an open menu").toHaveCount(
        0,
        { timeout: 20_000 },
      );
      await p.evaluate(() => navigator.mediaDevices.getDisplayMedia().then(() => undefined));
      expect(await pickEvents(p), "a pick outside any event must log null").toEqual([
        "click",
        "click",
        null,
      ]);
    } finally {
      await m.close();
    }
  });

  test("in a narrow bar the overflow popover starts, swaps and stops the share, handing focus back to its trigger each time", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const m = await openMenuMeeting(baseURL, "overflow", false);
    try {
      const p = m.presenter;
      const r = m.receiver;
      const overflowTrigger = p.locator(OVERFLOW_TRIGGER);
      const shareItem = (popover: Locator): Locator =>
        popover.locator("button.overflow-item", { has: p.locator('span:text-is("Screen share")') });
      await p.setViewportSize(PHONE);
      await expect(p.locator(TRIGGER), "precondition: the share button overflowed").toBeHidden({
        timeout: 10_000,
      });

      let popover = await openOverflow(p);
      await expect(popover.locator(OVERFLOW_GROUP)).toHaveCount(0);
      await holdNextPick(p);
      await shareItem(popover).click();
      await expect(overflowTrigger).toBeFocused();
      await expect(p.locator(TRIGGER)).toHaveAttribute("data-state", "requesting");
      popover = await openOverflow(p);
      const start = popover.locator(OVERFLOW_START);
      await expectBusy(start);
      await expect(start).toHaveAttribute(
        "aria-description",
        "Choose what to share in the browser dialog",
      );
      await start.focus();
      await expect(start).toBeFocused();
      await releasePick(p);
      await expect(p.locator(TRIGGER)).toHaveAttribute("data-state", "active", { timeout: 15_000 });
      await expect(
        overflowTrigger,
        "a share started from the item hands focus to the trigger",
      ).toBeFocused();
      await p.keyboard.press("Escape");
      await expect(p.locator(OVERFLOW_POPOVER)).toHaveCount(0);
      await expect(r.locator(RECEIVED)).toHaveCount(1, { timeout: 15_000 });
      await expect.poll(() => receivedColor(r), { timeout: 20_000 }).toBe("blue");
      await expect(p.locator(".peer-toast.toast-loading.screen-share-toast")).toHaveCount(0, {
        timeout: 20_000,
      });

      popover = await openOverflow(p);
      const group = popover.locator(OVERFLOW_GROUP);
      await expect(group.locator(OVERFLOW_REPLACE)).toHaveText("Share something else");
      await expect(group.locator(OVERFLOW_STOP)).toHaveText("Stop sharing");
      await expect(shareItem(popover)).toHaveCount(0);
      const stopColor = await group
        .locator(OVERFLOW_STOP)
        .locator("span")
        .evaluate((el) => getComputedStyle(el).color);
      const [errorColor, plainColor] = await popover.evaluate((el) =>
        ["var(--on-dark-error-text)", ""].map((color) => {
          const probe = document.createElement("span");
          probe.style.color = color;
          el.appendChild(probe);
          const computed = getComputedStyle(probe).color;
          probe.remove();
          return computed;
        }),
      );
      expect(errorColor, "precondition: the error token is not the popover's text colour").not.toBe(
        plainColor,
      );
      expect(stopColor, "the Stop sharing item uses the error colour").toBe(errorColor);
      const tile = await watchReceiverDuringSwap(r);
      await holdNextPick(p);
      await group.locator(OVERFLOW_REPLACE).click();
      await expect(p.locator(OVERFLOW_POPOVER)).toHaveCount(0);
      await expect(overflowTrigger).toBeFocused();
      await expect(p.locator(TRIGGER)).toHaveAttribute("data-state", "replacing");
      popover = await openOverflow(p);
      await expectBusy(popover.locator(OVERFLOW_REPLACE));
      await expectBusy(popover.locator(OVERFLOW_STOP));
      await popover.locator(OVERFLOW_STOP).click({ force: true });
      await p.waitForTimeout(1_000);
      await expect(p.locator(TRIGGER), "a busy Stop must not end the share").toHaveAttribute(
        "data-state",
        "replacing",
      );
      await expect(r.locator(RECEIVED)).toHaveCount(1);
      await p.keyboard.press("Escape");
      await releasePick(p);
      await expect(p.locator(REPLACED_TOAST)).toBeVisible({ timeout: 10_000 });
      expect(await pickEvents(p), "each overflow pick must run inside its click").toEqual([
        "click",
        "click",
      ]);
      await expect(p.locator(TRIGGER)).toHaveAttribute("data-state", "active");
      await expectSwapUnnoticed(r, tile);

      popover = await openOverflow(p);
      await popover.locator(OVERFLOW_STOP).click();
      await expect(overflowTrigger).toBeFocused();
      await expect(p.locator(TRIGGER)).toHaveAttribute("data-state", "idle", { timeout: 10_000 });
      await expect(r.locator(RECEIVED)).toHaveCount(0, { timeout: 20_000 });
      popover = await openOverflow(p);
      await expect(shareItem(popover)).toBeVisible();
      await expect(popover.locator(OVERFLOW_GROUP)).toHaveCount(0);
    } finally {
      await m.close();
    }
  });
});
