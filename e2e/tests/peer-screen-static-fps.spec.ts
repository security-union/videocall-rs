import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Per-peer screen-share `(static)` / `(no frames)` tooltip behaviour
 * (HCL issue #906).
 *
 * Real codecs emit zero encoded frames when the publisher's screen content
 * is genuinely static (no mouse motion, no UI changes). The tooltip then
 * reads `0fps | 0kbps`, which is visually indistinguishable from a broken
 * encoder. The fix in `signal_quality.rs`:
 *
 *   1. Records the most recent non-zero FPS / bitrate per (peer, sample).
 *   2. Classifies each zero sample as `Static` (held values + `(static)`
 *      annotation) when the prior live value is within 30s AND the peer's
 *      `peer_status` heartbeat is fresh (<5s), else `NoFrames` (raw zero
 *      + `(no frames)`).
 *   3. Renders the polyline at the held Y position during `Static`, so
 *      the chart line flatlines instead of dropping to zero.
 *
 * The unit tests in `dioxus-ui/src/components/signal_quality.rs` exhaust
 * the classifier and tooltip / chart helpers. This spec covers the
 * end-to-end happy path: a publisher whose screen-share track first
 * emits live frames and then stops emitting frames should surface the
 * `(static)` annotation in the tooltip (the held-value path).
 *
 * Mirrors `peer-screen-diagnostics.spec.ts` for auth + meeting setup
 * and the `MOCK_GET_DISPLAY_MEDIA_SCRIPT` pattern.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

// Mock `getDisplayMedia` with a canvas-backed stream whose frame emission
// can be paused on demand via a window-level toggle. This is the lever the
// test pulls to simulate a publisher whose screen has gone static after
// some initial live frames have flowed.
const MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    // Default to "emitting" so the share starts producing live samples.
    // The test flips this to false after the receiver has recorded a few
    // non-zero samples to drive the held-value path.
    window.__e2e906_emit_frames = true;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = 1280; canvas.height = 720;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 1280, 720);
      ctx.fillStyle = '#fff'; ctx.font = '32px sans-serif';
      ctx.fillText('Mock Screen Share (e2e-906)', 320, 360);
      // captureStream(0) means: only emit frames when requestFrame() is
      // explicitly called. That gives the test precise control over the
      // emit cadence — when the toggle is true we tick a rAF loop, when
      // false the encoder stops receiving frames and the receiver's
      // diagnostics see zero fps / kbps.
      const stream = canvas.captureStream(0);
      const track = stream.getVideoTracks()[0];
      let frame = 0;
      const tick = () => {
        if (window.__e2e906_emit_frames) {
          // Animate something so the encoder produces deltas — solid-color
          // still frames would still encode to ~zero. Move a small dot.
          frame++;
          ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 1280, 720);
          ctx.fillStyle = '#fff'; ctx.font = '32px sans-serif';
          ctx.fillText('Mock Screen Share (e2e-906)', 320, 360);
          ctx.fillStyle = '#ff0';
          const x = 100 + (frame * 10) % 1000;
          ctx.fillRect(x, 600, 20, 20);
          if (typeof track.requestFrame === 'function') {
            try { track.requestFrame(); } catch (_) { /* ignore */ }
          }
        }
        setTimeout(tick, 100); // 10fps when emitting
      };
      tick();
      return stream;
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

async function openSignalPopup(page: Page) {
  // In split-layout mode the signal button lives inside .split-peer-tile.
  // Hover the tile first to ensure any lazy-rendered overlay elements are
  // materialised (mirrors the crop/pin hover-gate pattern).
  const splitPeerTile = page.locator(".split-peer-tile").first();
  const hasSplitTile = await splitPeerTile.isVisible().catch(() => false);
  if (hasSplitTile) {
    await splitPeerTile.hover();
    await page.waitForTimeout(300);
  }

  const signalButton = page.locator('button[aria-label="Show signal quality"]').first();
  await expect(signalButton).toBeVisible({ timeout: 15_000 });
  await signalButton.click();
  const popup = page.locator(".signal-quality-popup");
  await expect(popup).toBeVisible({ timeout: 10_000 });
  return popup;
}

async function hoverChartOverlay(page: Page, popup: ReturnType<Page["locator"]>) {
  const overlay = popup.locator("div[style*='cursor: crosshair']").first();
  await expect(overlay).toBeVisible({ timeout: 5_000 });
  // Dispatch synthetic mousemove events — robust to viewport clamping
  // when the popup floats partly off-screen. Two events because some
  // implementations only register a tooltip on transition.
  await overlay.evaluate((el) => {
    const rect = (el as HTMLElement).getBoundingClientRect();
    const fire = (clientX: number) => {
      el.dispatchEvent(
        new MouseEvent("mousemove", {
          bubbles: true,
          cancelable: true,
          clientX,
          clientY: rect.top + rect.height / 2,
          buttons: 0,
        }),
      );
    };
    // Hover near the right edge so we land on the *latest* sample, which
    // is the one that will carry the (static) classification once frames
    // have stopped flowing for a second or two.
    fire(rect.right - 10);
    fire(rect.right - 5);
  });
  await page.waitForTimeout(300);
}

test.describe("Peer screen-share static-FPS tooltip", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("static screen-share holds the last-known FPS / kbps with a (static) annotation", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_static_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-906@videocall.rs", name: "Static906Host" },
        { email: "guest-906@videocall.rs", name: "Static906Guest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        await ctx.addInitScript(MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT);
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

      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      // ----- Phase 1: live screen-share. Wait for several signal samples
      // to be recorded so the receiver's history has at least one non-zero
      // entry to hold. The PeerTile samples ~1Hz via the diagnostics event
      // stream, so 6s gives ~5-6 live samples worth of history.
      await hostPage.waitForTimeout(6000);

      // ----- Phase 2: cut off frame emission on the guest. With
      // captureStream(0) the encoder stops receiving frames as soon as
      // requestFrame() stops being called, so the on-the-wire FPS / kbps
      // for the screen-share track drops to zero within a sample window.
      await guestPage.evaluate(() => {
        (window as unknown as { __e2e906_emit_frames: boolean }).__e2e906_emit_frames = false;
      });

      // Wait past the 1s sampling boundary plus a margin so the next
      // signal sample carries `screen_fps == 0` and the classifier kicks
      // in. We stay well within the 30s hold window AND well within the
      // 5s heartbeat-freshness window, so the sample should land in the
      // `Static` branch (held values + `(static)` annotation).
      await hostPage.waitForTimeout(3000);

      // Open the signal-quality popup and hover the chart to surface the
      // tooltip for the latest (zero) sample.
      const popup = await openSignalPopup(hostPage);
      await hoverChartOverlay(hostPage, popup);

      const tooltip = hostPage.locator("#signal-chart-tooltip-global");
      await expect(tooltip).toBeVisible({ timeout: 10_000 });

      // ----- Assert: the Screen line carries a held FPS / kbps value with
      // the `(static)` annotation. We don't pin a specific FPS / kbps
      // number because that depends on what the encoder shipped during the
      // live phase — only that it isn't zero and it carries the annotation.
      const tooltipHtml = await tooltip.innerHTML();
      const screenLine = tooltipHtml.split(/<br\s*\/?>|\n/i).find((l) => /Screen /.test(l));
      if (!screenLine) {
        throw new Error(`Tooltip did not contain a 'Screen ' line:\n${tooltipHtml}`);
      }
      // Static annotation must appear on either the FPS or the kbps part.
      // Use a regex over the metrics-suffix tail so the test stays
      // robust to small formatting tweaks (decimal places, separator).
      expect(screenLine).toMatch(/fps \(static\)/);
      expect(screenLine).toMatch(/kbps \(static\)/);
      // The held FPS value must NOT be "0.0fps (static)" — that would
      // mean the held lookup misfired and we're holding the zero we
      // were trying to mask. The held value should be the prior live
      // FPS, which in our mock is ~10fps.
      expect(screenLine).not.toMatch(/(?<!\d)0\.0fps \(static\)/);
      // And the no-frames annotation must NOT appear — heartbeat is
      // fresh (peer_status is still firing every ~1s on the live
      // session) so we should land squarely in the `Static` branch.
      expect(screenLine).not.toMatch(/\(no frames\)/);
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });
});
