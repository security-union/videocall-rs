/**
 * Two-peer screen-share harness: a HOST publishing a mocked `getDisplayMedia`
 * share and a GUEST rendering it. Extracted verbatim from
 * `tests/screen-share-static-keyframe-floor.spec.ts`, where it was file-local, so
 * a second sharer-side spec reuses one implementation rather than forking a copy.
 */

import { Page, BrowserContext, Locator, expect } from "@playwright/test";
import { wakeControls } from "./controls";

export interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

// `captureStream(0)` emits a frame ONLY on `requestFrame()`, so clearing
// `__e2e1903_emit_frames` makes the track go quiet and the encoder's read() park —
// the faithful model of a share whose content stopped changing.
export const MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    window.__e2e1903_emit_frames = true;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = 1280; canvas.height = 720;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 1280, 720);
      ctx.fillStyle = '#fff'; ctx.font = '32px sans-serif';
      ctx.fillText('Mock Screen Share (e2e-1903)', 320, 360);
      const stream = canvas.captureStream(0);
      const track = stream.getVideoTracks()[0];
      let frame = 0;
      const tick = () => {
        if (window.__e2e1903_emit_frames) {
          frame++;
          ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 1280, 720);
          ctx.fillStyle = '#fff'; ctx.font = '32px sans-serif';
          ctx.fillText('Mock Screen Share (e2e-1903)', 320, 360);
          ctx.fillStyle = '#ff0';
          const x = 100 + (frame * 10) % 1000;
          ctx.fillRect(x, 600, 20, 20);
          if (typeof track.requestFrame === 'function') {
            try { track.requestFrame(); } catch (_) { /* ignore */ }
          }
        }
        setTimeout(tick, 80); // ~12fps when emitting (< the 150ms static poll)
      };
      tick();
      return stream;
    };
    Object.defineProperty(mediaDevices, 'getDisplayMedia', {
      configurable: true, value: async () => createStream(),
    });
  })();
`;

export async function joinMeetingAs(
  context: BrowserContext,
  meetingId: string,
  username: string,
): Promise<Page> {
  const page = await context.newPage();
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);

  return page;
}

export async function clickJoinAndEnterGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "join") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

export async function admitGuestIfNeeded(hostPage: Page, guestPage: Page): Promise<void> {
  const joinButton = guestPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = guestPage.getByText("Waiting to be admitted");
  const guestGrid = guestPage.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);
  }

  if (result !== "auto-joined") {
    await clickJoinAndEnterGrid(guestPage);
  } else {
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }
}

// Issue 2792: the view a new share opens in is read from these keys when the
// share starts (share_view.rs `ShareOrigin::pref_key`).
export type ShareViewMode = "tile" | "enlarged" | "pinned" | "detached";
export const RECEIVED_SHARE_VIEW_KEY = "vc_share_view_mode";
export const OWN_SHARE_VIEW_KEY = "vc_own_share_view_mode";

// Re-applied on every navigation, so a spec asserting that an in-page choice
// persists must not reload after seeding.
export async function seedShareViewMode(
  target: BrowserContext | Page,
  mode: ShareViewMode,
  key: string = RECEIVED_SHARE_VIEW_KEY,
): Promise<void> {
  await target.addInitScript(
    ([k, v]) => {
      if (window.location.href === "about:blank") return;
      try {
        window.localStorage.setItem(k, v);
      } catch {
        /* no storage on this document */
      }
    },
    [key, mode],
  );
}

export function shareButton(page: Page) {
  return page.locator("button.video-control-button", {
    has: page.locator(".tooltip", { hasText: "Share Screen" }),
  });
}

// While a share is live the button opens a menu instead of stopping it.
export function shareMenuTrigger(page: Page) {
  return page.locator('#screen-share-trigger[data-state="active"]');
}

// Returns the menu as is when it is already open: clicking the trigger would close it.
export async function openShareMenu(sharerPage: Page): Promise<Locator> {
  const open = sharerPage.locator('[data-testid="screen-share-menu"]');
  if (await open.isVisible()) {
    return open;
  }
  await wakeControls(sharerPage);
  await sharerPage.waitForTimeout(300);
  await sharerPage.locator(".video-controls-container").hover();
  const trigger = shareMenuTrigger(sharerPage);
  await expect(trigger).toBeVisible({ timeout: 10_000 });
  await trigger.click();
  const menu = sharerPage.locator('[data-testid="screen-share-menu"]');
  await expect(menu).toBeVisible({ timeout: 5_000 });
  return menu;
}

// True once the VIEWER renders the received share tile, confirming encoded
// screen frames actually reached it.
export async function startScreenShare(sharerPage: Page, viewerPage: Page): Promise<boolean> {
  await wakeControls(sharerPage);
  await sharerPage.waitForTimeout(300);
  const button = shareButton(sharerPage);

  await expect(button).toBeVisible({ timeout: 10_000 });
  await button.click();

  try {
    await expect(viewerPage.locator('[data-share-origin="received"]')).toBeVisible({
      timeout: 15_000,
    });
    return true;
  } catch {
    return false;
  }
}

export async function stopScreenShare(sharerPage: Page): Promise<void> {
  const menu = await openShareMenu(sharerPage);
  await menu.locator('[data-testid="screen-share-menu-stop"]').click();
  await expect(sharerPage.locator("#screen-share-trigger")).toHaveAttribute("data-state", "idle", {
    timeout: 10_000,
  });
}
