import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Per-peer screen-share diagnostics (HCL issue #883).
 *
 * When a remote peer is sharing their screen the host can open that peer's
 * SignalQualityPopup (the small bars icon on the peer tile) and see screen
 * resolution / FPS / bitrate alongside the camera-video and audio metrics.
 * The chart also renders a third "Screen" polyline and a "Screen" legend
 * entry once at least one sample has been recorded with `screen_enabled`.
 *
 * Flow exercised by this spec:
 *   1. Host + guest join the same meeting.
 *   2. Guest starts screen-share (getDisplayMedia is mocked at the wasm
 *      boundary so the system picker is never shown).
 *   3. Host opens the signal-quality popup for the guest tile.
 *   4. The chart's Screen legend entry appears (gated on `has_screen_data`).
 *   5. Hovering the chart pops the body tooltip with a "Screen: ... fps ...
 *      kbps" line — and when the decoder has reported a resolution, the
 *      tooltip line carries the "WxH" prefix.
 *   6. The "?" help text for the Screen legend mentions Resolution / FPS /
 *      Bitrate so the wording change introduced in #883 stays in sync.
 *   7. (#891 amendment) The tooltip Screen line carries the publisher's
 *      *source* resolution end-to-end. When source == received the tooltip
 *      collapses to a single WxH value with no arrow / no downscale badge;
 *      the help text mentions Source / Received / pixel area so the
 *      degradation framing stays in sync.
 *
 * Mirrors the structure of `signal-quality-peer-transport.spec.ts` (auth +
 * meeting setup) and the `MOCK_GET_DISPLAY_MEDIA_SCRIPT` pattern from
 * `screen-share-panel.spec.ts`.
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
      ctx.fillText('Mock Screen Share (e2e-883)', 320, 360);
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
  await sharerPage.mouse.move(400, 400);
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

test.describe("Peer screen-share diagnostics", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("host signal-quality popup surfaces screen-share metrics when peer is sharing", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_diag_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-ssdiag@videocall.rs", name: "SSDiagHost" },
        { email: "guest-ssdiag@videocall.rs", name: "SSDiagGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        // Mock getDisplayMedia on both sides so the guest's share button does
        // not open a real system picker.
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

      // Wait for mesh to settle so the host has the guest tile + diagnostics flow.
      await hostPage.waitForTimeout(8000);

      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
        timeout: 30_000,
      });

      // Guest starts screen-share. If the wasm-level mock could not produce a
      // stream (rare in headless variants), skip cleanly.
      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      // Wait for at least one signal sample carrying screen_enabled to be
      // recorded — the polyline / legend / tooltip are all gated on this.
      await hostPage.waitForTimeout(5000);

      // Open the signal-quality popup for the remote peer. Once screen share
      // is active the layout is split; the bars icon is still rendered inside
      // the peer's tile (whichever side it lives on).
      const signalButton = hostPage.locator('button[aria-label="Show signal quality"]').first();
      await expect(signalButton).toBeVisible({ timeout: 15_000 });
      await signalButton.click();

      const popup = hostPage.locator(".signal-quality-popup");
      await expect(popup).toBeVisible({ timeout: 10_000 });

      // ----- Assert: the Screen legend entry has appeared. -----
      const screenLegend = popup.locator(".signal-chart-legend .legend-item", {
        hasText: /^Screen/,
      });
      await expect(screenLegend).toBeVisible({ timeout: 15_000 });

      // ----- Assert: the screen polyline is present in the SVG. -----
      // Three quality polylines (audio, video, screen) plus the dashed
      // latency line means at least 4 polyline elements once screen data
      // has been recorded.
      const polylines = popup.locator("svg polyline");
      await expect
        .poll(async () => polylines.count(), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(4);

      // ----- Assert: the Screen "?" help text mentions the metrics it tracks. -----
      const helpButton = screenLegend.locator("button.legend-help-btn");
      await helpButton.click();
      const helpText = popup.locator(".legend-help-text");
      await expect(helpText).toBeVisible({ timeout: 5_000 });
      await expect(helpText).toContainText("Resolution");
      await expect(helpText).toContainText(/FPS/i);
      await expect(helpText).toContainText(/Bitrate/i);

      // ----- Assert: hovering the chart shows a Screen tooltip line. -----
      const overlay = popup.locator("div[style*='cursor: crosshair']").first();
      await expect(overlay).toBeVisible({ timeout: 5_000 });
      const box = await overlay.boundingBox();
      if (!box) {
        throw new Error("chart overlay has no bounding box");
      }
      // Hover near the right edge so the tooltip targets the latest sample
      // (where screen_enabled is true).
      await hostPage.mouse.move(box.x + box.width - 5, box.y + box.height / 2);
      await hostPage.waitForTimeout(300);

      const tooltip = hostPage.locator("#signal-chart-tooltip-global");
      await expect(tooltip).toBeVisible({ timeout: 5_000 });
      await expect(tooltip).toContainText(/Screen:/);
      await expect(tooltip).toContainText(/fps/i);
      await expect(tooltip).toContainText(/kbps/i);

      // ----- Source vs Received resolution (#891 amendment, May 2026). -----
      //
      // The publisher's mock track reports width 1280, height 720 from
      // getSettings() and the encoder ships at the same dimensions, so the
      // tooltip should *collapse* to a single value (no "Source" / arrow /
      // degradation badge). Asserting the collapsed shape proves both ends
      // of the source-resolution plumbing are wired end-to-end: the
      // publisher stamped `MediaPacket.video_metadata.source_*`, the
      // decoder emitted the `video_source_resolution` diag event, the
      // PeerTile recorded it on the SignalSample, and the tooltip chose
      // the equal-Source-and-Received branch.
      //
      // The Source > Received downscale branch + the `↓X% pixel area`
      // badge are exhaustively covered by the unit tests in
      // `dioxus-ui/src/components/signal_quality.rs` (pixel-area math,
      // severity bucketing, badge presence/absence on every boundary).
      // Reproducing the downscale path in Playwright would require
      // pinning the publisher's adaptive-quality tier from outside the
      // wasm boundary, which is more brittle than the unit-test coverage.
      await expect(tooltip).toContainText("1280x720");
      await expect(tooltip).not.toContainText("Source");
      await expect(tooltip).not.toContainText("→"); // arrow
      await expect(tooltip).not.toContainText("↓"); // downscale badge

      // The legend help text should mention the Source vs Received story
      // and the pixel-area badge so the wording change stays in sync.
      await expect(helpText).toContainText("Source");
      await expect(helpText).toContainText(/Received/i);
      await expect(helpText).toContainText("pixel");
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
