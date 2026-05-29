/**
 * WebTransport-vs-WebSocket asymmetry coverage for the screen-share
 * split-layout activation path (bug #1 from PR #940).
 *
 * Background
 * ----------
 * PR #940 fixed bug #1 — "WT viewers cannot see shared screen content
 * except in a small tile" — by introducing the `apply_heartbeat_enabled_flag`
 * helper plus a `MEDIA_FRESH_WINDOW_MS = 5000` per-frame freshness window
 * in `videocall-client/src/decode/peer_decode_manager.rs`. The root cause
 * was a QUIC-stream-ordering race specific to WebTransport: heartbeats
 * (Control stream) and SCREEN frames (Screen stream) ride on separate QUIC
 * streams with no global FIFO ordering, so a stale heartbeat carrying
 * `screen_enabled=false` could arrive AFTER a SCREEN keyframe had set the
 * flag true and clobber it back to false, collapsing the split layout to
 * a tiny peer tile.
 *
 * PR #940 added a 12-case Rust unit suite for `apply_heartbeat_enabled_flag`
 * and structural Playwright coverage in `screen-share-panel.spec.ts`. What
 * was deferred to follow-up issue #942 is end-to-end coverage that drives
 * the WT transport path under the bug-triggering conditions and asserts
 * that the WT viewer renders the same split layout as the WS viewer.
 *
 * What this spec covers
 * ---------------------
 * Two test cases, sharing identical assertions but differing in forced
 * transport preference:
 *
 *   1. `wt_viewer_sees_split_layout_when_publisher_shares_screen` — both
 *      host and guest contexts seed `localStorage` with
 *      `vc_transport_preference=webtransport` and `vc_transport_sticky=true`
 *      before any wasm boot (mirrors the pattern used by
 *      `wt-persistent-streams-freeze-regression.spec.ts` and
 *      `protocol-selection.spec.ts`). When the guest starts a mocked
 *      `getDisplayMedia()` screen share, the host VIEWER's `#grid-container`
 *      must transition to the split-layout shape: a flex container with
 *      `.split-screen-tile` (left), `.screen-share-resize-handle`, and a
 *      right-panel CSS grid (`grid-template-columns: 1fr( 1fr)?`).
 *
 *   2. `ws_viewer_sees_split_layout_when_publisher_shares_screen` —
 *      identical scenario with both contexts forced to WebSocket
 *      (`vc_transport_preference=websocket`). The same structural
 *      assertions must pass. This is the symmetry guard called out in
 *      issue #942: WT and WS viewers must produce structurally
 *      indistinguishable split layouts.
 *
 * If a future regression reintroduces the heartbeat-clobber race on the
 * WT path, the first test fails while the second still passes — the
 * asymmetry is named in the failure (the test name itself ends with
 * "_when_publisher_shares_screen" qualified by transport).
 *
 * Why a structural assertion is sufficient
 * ----------------------------------------
 * The PR #940 race is a timing window between two QUIC streams in the
 * `peer_decode_manager`. Driving the wire-level race deterministically
 * from Playwright would require a transport-injection harness that does
 * not exist in this repo (and adding it would be a major project — see
 * "Known limitations" below). The structural test asserts the layout
 * activates when `screen_enabled` flips true, on both transports — which
 * is the user-visible symptom of bug #1. A wire-race regression that
 * silently clobbered the WT flag would surface as a failure here because
 * the host viewer's `#grid-container` would not contain `.split-screen-tile`
 * within the 15-second activation window.
 *
 * Known limitations
 * -----------------
 *   - We cannot reproduce the exact race timing without a custom WT
 *     injector. We rely on the natural variance of the Chromium QUIC
 *     stack plus the 15-second activation timeout to give the heartbeat
 *     stream multiple opportunities to interleave with the screen frames.
 *   - The `getDisplayMedia()` mock returns a `MediaStream` from a canvas;
 *     this still drives the publisher's encode and the on-wire SCREEN
 *     packets, but skips the OS-level picker that Playwright cannot
 *     drive. This matches the pattern used by `screen-share-panel.spec.ts`.
 *   - When the e2e stack runs without a WebTransport server (legacy
 *     `docker-compose.e2e.yaml` shape — see infra gap noted in
 *     `wt-persistent-streams-freeze-regression.spec.ts`), the WT test
 *     case may transparently fall back to WebSocket via election. We
 *     detect this with a console-log sniffer and skip the WT case with
 *     a descriptive message rather than reporting a false green.
 */

import { test, expect, chromium, Page, BrowserContext, Browser } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

type TransportMode = "webtransport" | "websocket";

const SPLIT_LAYOUT_ACTIVATION_TIMEOUT_MS = 15_000;

/**
 * Seed `localStorage` with the given sticky transport preference so the
 * wasm boot path elects that transport before any signaling happens.
 *
 * Mirrors the storage shape that the device-settings modal writes when
 * the user enables "Sticky" + selects a transport in the Network tab
 * (see `dioxus-ui/src/context.rs::TRANSPORT_PREF_KEY` /
 * `TRANSPORT_STICKY_KEY`).
 */
async function forceTransportSticky(
  context: BrowserContext,
  mode: TransportMode,
  baseURL: string,
): Promise<void> {
  const url = new URL(baseURL);
  await context.addInitScript((m: string) => {
    try {
      localStorage.setItem("vc_transport_preference", m);
      localStorage.setItem("vc_transport_sticky", "true");
    } catch {
      // localStorage may be unavailable in unusual sandbox states; the
      // test will then exercise the Auto election path and the WT-vs-WS
      // assertions below will not be meaningful, but the structural
      // split-layout check still runs.
    }
  }, mode);
  // Touch a page once to ensure storage is committed before any nav.
  const seedPage = await context.newPage();
  await seedPage.goto(url.toString(), { waitUntil: "domcontentloaded" });
  await seedPage.evaluate((m: string) => {
    localStorage.setItem("vc_transport_preference", m);
    localStorage.setItem("vc_transport_sticky", "true");
  }, mode);
  await seedPage.close();
}

/**
 * Mock `navigator.mediaDevices.getDisplayMedia` to return a synthetic
 * canvas-derived `MediaStream`. Injected via `addInitScript` so the
 * stub is in place before the wasm calls it.
 *
 * Same pattern as `screen-share-panel.spec.ts`. The canvas-derived stream
 * is a real `MediaStream` — the publisher's encode path runs end-to-end,
 * which is what lets the viewer's `screen_enabled` flag actually flip
 * via on-wire SCREEN frames.
 */
const MOCK_GET_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = 640; canvas.height = 480;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 640, 480);
      ctx.fillStyle = '#fff'; ctx.font = '24px sans-serif';
      ctx.fillText('Mock Screen Share', 160, 240);
      return canvas.captureStream(5);
    };
    Object.defineProperty(mediaDevices, 'getDisplayMedia', {
      configurable: true, value: async () => createStream(),
    });
  })();
`;

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
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
}

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting" || result === "waiting-for-meeting") {
    return result;
  }
  if (result === "auto-joined") {
    return "in-meeting";
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult === "in-meeting") {
    return;
  }
  if (guestResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

    const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
    const guestGrid = guestPage.locator("#grid-container");

    const postAdmit = await Promise.race([
      guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
      guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);

    if (postAdmit === "join-button") {
      await guestPage.waitForTimeout(1000);
      await guestJoinButton.click();
      await guestPage.waitForTimeout(3000);
      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    }
  }
}

interface TransportSniffer {
  websocketFallbackHits: number;
}

/**
 * Watch console output for indications that the WT election lost and
 * the client fell back to WS. Used to skip the WT case cleanly when the
 * e2e stack has no WebTransport server configured (legacy compose).
 */
function attachTransportSniffer(page: Page): TransportSniffer {
  const stats: TransportSniffer = { websocketFallbackHits: 0 };
  page.on("console", (msg) => {
    const text = msg.text();
    if (
      text.includes("transport=websocket") ||
      text.includes("falling back to websocket") ||
      text.includes("WebTransport failed")
    ) {
      stats.websocketFallbackHits += 1;
    }
  });
  return stats;
}

/**
 * Click the screen share button on the publisher's page. Mirrors the
 * helper in `screen-share-panel.spec.ts`. Returns true if the viewer's
 * split layout activated within the timeout window.
 */
async function startScreenShareAndAwaitSplitLayout(
  publisherPage: Page,
  viewerPage: Page,
): Promise<boolean> {
  // Wake auto-hidden controls bar, then find the share button by tooltip.
  await publisherPage.mouse.move(400, 400);
  await publisherPage.waitForTimeout(300);
  const shareButton = publisherPage.locator("button.video-control-button", {
    has: publisherPage.locator(".tooltip", { hasText: "Share Screen" }),
  });

  await expect(shareButton).toBeVisible({ timeout: 10_000 });
  await shareButton.click();

  try {
    await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({
      timeout: SPLIT_LAYOUT_ACTIVATION_TIMEOUT_MS,
    });
    return true;
  } catch {
    return false;
  }
}

interface MeetingFixture {
  hostPage: Page;
  guestPage: Page;
  browser1: Browser;
  browser2: Browser;
  hostSniffer: TransportSniffer;
  guestSniffer: TransportSniffer;
}

async function setupTwoUserMeetingWithTransport(
  uiURL: string,
  meetingId: string,
  hostName: string,
  guestName: string,
  transport: TransportMode,
): Promise<MeetingFixture> {
  const browser1 = await chromium.launch({ args: BROWSER_ARGS });
  const browser2 = await chromium.launch({ args: BROWSER_ARGS });

  const hostCtx = await createAuthenticatedContext(
    browser1,
    `${hostName.toLowerCase()}@videocall.rs`,
    hostName,
    uiURL,
  );
  const guestCtx = await createAuthenticatedContext(
    browser2,
    `${guestName.toLowerCase()}@videocall.rs`,
    guestName,
    uiURL,
  );

  // Force transport for BOTH contexts before any page loads the wasm.
  await forceTransportSticky(hostCtx, transport, uiURL);
  await forceTransportSticky(guestCtx, transport, uiURL);

  // Mock getDisplayMedia on the guest (the publisher in our scenarios).
  await guestCtx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);
  // Also on the host so a future test variant where the host shares
  // doesn't surprise-fail; cost is negligible (an init-script eval).
  await hostCtx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);

  const hostPage = await hostCtx.newPage();
  const guestPage = await guestCtx.newPage();

  const hostSniffer = attachTransportSniffer(hostPage);
  const guestSniffer = attachTransportSniffer(guestPage);

  await navigateToMeeting(hostPage, meetingId, hostName);
  const hostResult = await joinMeetingFromPage(hostPage);
  expect(hostResult).toBe("in-meeting");

  await navigateToMeeting(guestPage, meetingId, guestName);
  const guestResult = await joinMeetingFromPage(guestPage);
  await admitGuestIfNeeded(hostPage, guestPage, guestResult);

  // Confirm the sticky preference survived the boot — guards against
  // a future change to context.rs that resets storage on entry.
  const hostPref = await hostPage.evaluate(() => ({
    preference: localStorage.getItem("vc_transport_preference"),
    sticky: localStorage.getItem("vc_transport_sticky"),
  }));
  const guestPref = await guestPage.evaluate(() => ({
    preference: localStorage.getItem("vc_transport_preference"),
    sticky: localStorage.getItem("vc_transport_sticky"),
  }));
  expect(hostPref.preference).toBe(transport);
  expect(hostPref.sticky).toBe("true");
  expect(guestPref.preference).toBe(transport);
  expect(guestPref.sticky).toBe("true");

  // Both peer tiles must appear before we drive screen-share so the
  // publisher actually has a viewer to send SCREEN packets to.
  await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 30_000,
  });
  await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 30_000,
  });

  return { hostPage, guestPage, browser1, browser2, hostSniffer, guestSniffer };
}

/**
 * Assert the host's `#grid-container` is in split-layout shape.
 *
 * Structural checks taken from `screen-share-panel.spec.ts` (the
 * "right panel renders 2-column grid during screen share" case):
 *   - `.split-screen-tile` is visible (left panel content).
 *   - `.screen-share-resize-handle` is present.
 *   - `#grid-container > div:nth-child(3)` is the right panel and
 *     declares `grid-template-columns: 1fr( 1fr)?`.
 *
 * Bug #1 manifestation: the WT viewer's grid would NOT contain a
 * `.split-screen-tile` because the stale heartbeat cleared
 * `screen_enabled` before the layout transition committed.
 */
async function assertSplitLayoutActive(viewerPage: Page): Promise<void> {
  // 1. Left panel has the screen-share tile.
  await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({ timeout: 10_000 });

  // 2. Resize handle is present.
  await expect(viewerPage.locator(".screen-share-resize-handle")).toHaveCount(1);

  // 3. Right panel uses a CSS grid with 1 or 2 columns.
  const rightPanel = viewerPage.locator("#grid-container > div:nth-child(3)");
  await expect(rightPanel).toBeVisible({ timeout: 10_000 });
  const rightPanelStyle = await rightPanel.getAttribute("style");
  expect(rightPanelStyle).toBeTruthy();
  expect(rightPanelStyle).toMatch(/grid-template-columns: 1fr( 1fr)?/);

  // 4. Left panel has explicit pixel width > 0 (issue #942 acceptance).
  const leftPanel = viewerPage.locator("#grid-container > div:nth-child(1)");
  await expect(leftPanel).toBeVisible({ timeout: 10_000 });
  const leftBox = await leftPanel.boundingBox();
  expect(leftBox).not.toBeNull();
  expect(leftBox!.width).toBeGreaterThan(0);

  // 5. At least one peer tile in the right panel.
  const peerTiles = viewerPage.locator(".split-peer-tile");
  const peerCount = await peerTiles.count();
  expect(peerCount).toBeGreaterThan(0);
}

test.describe("WT-vs-WS asymmetry: screen-share split-layout (issue #942)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * The WebTransport case — the path that bug #1 affected. Failure here
   * with the WS case still green = the heartbeat-vs-media race fix in
   * PR #940 regressed.
   */
  test("wt_viewer_sees_split_layout_when_publisher_shares_screen", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wt_split_${Date.now()}`;

    const fixture = await setupTwoUserMeetingWithTransport(
      uiURL,
      meetingId,
      "WtSplitHost",
      "WtSplitGuest",
      "webtransport",
    );

    try {
      // Give time for WT peer discovery + heartbeat warm-up. This window
      // is also where a stale heartbeat could race a SCREEN keyframe.
      await fixture.hostPage.waitForTimeout(3000);

      const splitActivated = await startScreenShareAndAwaitSplitLayout(
        fixture.guestPage,
        fixture.hostPage,
      );

      // If the WT election fell back to WS (e.g. e2e stack has no WT
      // server), this case cannot exercise the WT-specific code path —
      // skip cleanly rather than report a misleading green.
      if (
        fixture.hostSniffer.websocketFallbackHits > 0 ||
        fixture.guestSniffer.websocketFallbackHits > 0
      ) {
        test.skip(
          true,
          "WebTransport election fell back to WebSocket in this environment " +
            "(no WT server in the e2e stack). The WT-asymmetry path was not " +
            "exercised; rerun once docker/docker-compose.e2e.yaml runs a " +
            "webtransport-api service (issue #777).",
        );
        return;
      }

      expect(
        splitActivated,
        "WT viewer did not transition to the split-layout within " +
          `${SPLIT_LAYOUT_ACTIVATION_TIMEOUT_MS}ms after the publisher started ` +
          "sharing. This is the bug #1 regression signature: a stale heartbeat " +
          "on the Control stream clobbered `screen_enabled` after the SCREEN " +
          "frame arrived on the Screen stream, collapsing the layout to a " +
          "tiny peer tile.",
      ).toBe(true);

      await assertSplitLayoutActive(fixture.hostPage);
    } finally {
      await fixture.browser1.close();
      await fixture.browser2.close();
    }
  });

  /**
   * The WebSocket case — the path that bug #1 did NOT affect (single
   * ordered stream guarantees heartbeat/media FIFO). This is the
   * symmetry guard: structurally identical assertions to the WT case.
   * If WT and WS produce different layouts, the asymmetry will surface
   * here vs. the WT test.
   */
  test("ws_viewer_sees_split_layout_when_publisher_shares_screen", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_ws_split_${Date.now()}`;

    const fixture = await setupTwoUserMeetingWithTransport(
      uiURL,
      meetingId,
      "WsSplitHost",
      "WsSplitGuest",
      "websocket",
    );

    try {
      await fixture.hostPage.waitForTimeout(3000);

      const splitActivated = await startScreenShareAndAwaitSplitLayout(
        fixture.guestPage,
        fixture.hostPage,
      );

      expect(
        splitActivated,
        "WS viewer did not transition to the split-layout. WS uses a single " +
          "ordered stream — if this fails, the failure is not the WT-asymmetry " +
          "race; it indicates a wider regression in the split-layout " +
          "activation path (attendants.rs / canvas_generator.rs).",
      ).toBe(true);

      await assertSplitLayoutActive(fixture.hostPage);
    } finally {
      await fixture.browser1.close();
      await fixture.browser2.close();
    }
  });
});
