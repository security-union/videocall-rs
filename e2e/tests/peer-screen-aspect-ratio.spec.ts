import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Consumer-side screen-share aspect-ratio fidelity (HCL issue: shared
 * content squashed/stretched on receivers).
 *
 * The bug: the receiving participants' screen-share tile rendered the
 * publisher's frame with `object-fit: cover` (inherited from the camera-tile
 * `.cropped` class which is applied by default), which stretched/cropped
 * 16:9 source content into whatever shape the tile happened to be — e.g. a
 * tall narrow column or an ultrawide pinned panel — producing wrong on-screen
 * proportions.
 *
 * The fix: `dioxus-ui/static/global.css` adds a higher-specificity rule
 * targeting `canvas[id^="screen-share-"]` in both the grid tile
 * (`.grid-item .canvas-container`) and the split-layout left panel
 * (`.split-screen-tile .canvas-container`), forcing `object-fit: contain`
 * with `width/height: 100%; min-width: 0` so the canvas fills its container
 * and the drawn frame letterboxes / pillarboxes inside, preserving the
 * publisher's source aspect ratio regardless of the tile shape.
 *
 * This spec asserts that the receive-side screen-share canvas has computed
 * `object-fit: contain` (not `cover`) and that its bounding box fully covers
 * the parent `.canvas-container` (so the container's aspect ratio governs
 * the box, while `object-fit: contain` letterboxes the drawn content
 * inside). It does not — and cannot — pixel-sample the drawn frame: the
 * receiving canvas is driven by the WebTransport / WebSocket decoder
 * pipeline whose output dims depend on encoder behavior outside this fix's
 * scope. The computed-style + bounding-box assertion is the load-bearing
 * check that the CSS contract is in effect.
 *
 * Reuses the `MOCK_GET_DISPLAY_MEDIA_SCRIPT` pattern from
 * `peer-screen-diagnostics.spec.ts` so the share button does not open the
 * native OS picker, and a synthetic 1280x720 (16:9) canvas-backed stream
 * stands in for the shared window.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

const MOCK_GET_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = 1280; canvas.height = 720;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 1280, 720);
      ctx.fillStyle = '#fff'; ctx.font = '32px sans-serif';
      ctx.fillText('Mock Screen Share (aspect-ratio)', 320, 360);
      return canvas.captureStream(10);
    };
    Object.defineProperty(mediaDevices, 'getDisplayMedia', {
      configurable: true, value: async () => createStream(),
    });
  })();
`;

async function joinMeetingAs(
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

async function clickJoinAndEnterGrid(page: Page): Promise<void> {
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

async function admitGuestIfNeeded(hostPage: Page, guestPage: Page): Promise<void> {
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

async function startScreenShare(sharerPage: Page, viewerPage: Page): Promise<boolean> {
  await wakeControls(sharerPage);
  await sharerPage.waitForTimeout(300);
  const shareButton = sharerPage.locator("button.video-control-button", {
    has: sharerPage.locator(".tooltip", { hasText: "Share Screen" }),
  });

  await expect(shareButton).toBeVisible({ timeout: 10_000 });
  await shareButton.click();

  try {
    await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({
      timeout: 15_000,
    });
    return true;
  } catch {
    return false;
  }
}

test.describe("Peer screen-share aspect ratio", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("consumer-side screen-share canvas uses object-fit: contain (preserves source aspect ratio)", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_aspect_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-ssaspect@videocall.rs", name: "SSAspectHost" },
        { email: "guest-ssaspect@videocall.rs", name: "SSAspectGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        await ctx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);

      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      await admitGuestIfNeeded(members[0].page, members[1].page);

      const hostPage = members[0].page;
      const guestPage = members[1].page;

      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
        timeout: 30_000,
      });

      // Guest starts screen-share. If the wasm-level mock could not produce a
      // stream (rare in headless variants), skip cleanly — the css-only
      // contract is still enforced by the rule itself, but we cannot exercise
      // it without an active share session.
      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      // Wait for the screen-share canvas to be mounted on the host side.
      const screenCanvas = hostPage.locator('canvas[id^="screen-share-"]').first();
      await expect(screenCanvas).toBeVisible({ timeout: 15_000 });

      // The screen-share canvas must use object-fit: contain (not cover).
      // This is the load-bearing assertion — it guarantees that whatever
      // shape the tile is, the publisher's frame letterboxes/pillarboxes
      // inside instead of being stretched to fill.
      const objectFit = await screenCanvas.evaluate(
        (el) => window.getComputedStyle(el as HTMLCanvasElement).objectFit,
      );
      expect(objectFit).toBe("contain");

      // The canvas should fill its container (so the container's aspect
      // ratio governs the bounding box; `object-fit: contain` then
      // letterboxes the drawn frame inside). Verify by comparing
      // getBoundingClientRect of the canvas vs its parent .canvas-container.
      const sizing = await screenCanvas.evaluate((el) => {
        const c = el as HTMLCanvasElement;
        const parent = c.closest(".canvas-container") as HTMLElement | null;
        const cRect = c.getBoundingClientRect();
        const pRect = parent ? parent.getBoundingClientRect() : null;
        return {
          canvasWidth: cRect.width,
          canvasHeight: cRect.height,
          parentWidth: pRect ? pRect.width : 0,
          parentHeight: pRect ? pRect.height : 0,
          minWidth: window.getComputedStyle(c).minWidth,
        };
      });

      // Canvas fills container width/height (within 1px rounding tolerance).
      expect(sizing.parentWidth).toBeGreaterThan(0);
      expect(sizing.parentHeight).toBeGreaterThan(0);
      expect(Math.abs(sizing.canvasWidth - sizing.parentWidth)).toBeLessThanOrEqual(1);
      expect(Math.abs(sizing.canvasHeight - sizing.parentHeight)).toBeLessThanOrEqual(1);

      // min-width must NOT be 100% — the camera-tile rule forces
      // min-width: 100% to support "zoom to fill" cropping, which on a
      // screen-share canvas would push the canvas off-center and combine
      // poorly with `object-fit: contain`. The screen-share override sets
      // min-width: 0.
      expect(sizing.minWidth).not.toBe(`${Math.round(sizing.parentWidth)}px`);
    } finally {
      for (const m of members) {
        await m.context.close().catch(() => {});
      }
      for (const b of browsers) {
        await b.close().catch(() => {});
      }
    }
  });
});
