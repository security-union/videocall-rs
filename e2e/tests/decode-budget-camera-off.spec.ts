import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Decode-budget camera-OFF partition E2E coverage (issue #1465).
 *
 * BEHAVIOR UNDER TEST (issue #1465):
 *   BEFORE: a camera-OFF remote peer was counted against the decode-budget cap.
 *           When ranked beyond the cap it rendered with the dashed
 *           `.off-budget-tile` outline (looked "paused"), AND it consumed a
 *           budget slot — pushing a real camera-ON peer into the avatar bucket.
 *   AFTER:  camera-OFF real peers are partitioned OUT of the decode budget. They
 *           render as PLAIN avatars ("Video Disabled") with NO `.off-budget-tile`
 *           dashed class and `data-off-budget="false"`, and they do NOT consume a
 *           decode-budget slot. Only camera-ON real peers (and mock peers, which
 *           always feed the budget) participate in the budget split.
 *
 * WHY THIS NEEDS REAL BROWSER CONTEXTS (not mock peers):
 *   The mock-peer debug control used by `decode-budget.spec.ts` injects
 *   layout-only placeholders that BYPASS the camera partition entirely — they
 *   are appended to the tile list via `mock_ids`, not routed through
 *   `display_peers`, so they always feed the budget regardless of the camera
 *   predicate. A mock therefore CANNOT reproduce a camera-OFF real peer.
 *   Reproducing #1465 deterministically requires a genuine remote peer that
 *   joins with its camera OFF. We get that by standing up additional real
 *   Chromium contexts (host + guests) — the proven pattern from
 *   `screen-share-panel.spec.ts` — and controlling each guest's camera via the
 *   prejoin `vc_prejoin_camera_on` localStorage flag:
 *     - flag "true"          → guest publishes video → `is_video_enabled_for_peer`
 *                              true  → camera-ON  → feeds the budget.
 *     - flag absent / "false"→ guest joins camera-off → `is_video_enabled_for_peer`
 *                              false → camera-OFF → partitioned out (#1465).
 *
 * DOM contract (canvas_generator.rs):
 *   - Every peer tile is `#grid-container .grid-item` with `data-off-budget`
 *     "true" (budget-shed, force_avatar) or "false" (decoded OR camera-off).
 *   - Dashed off-budget tiles carry the `.off-budget-tile` class. Post-#1465 a
 *     camera-off real peer renders WITHOUT that class (force_avatar is false on
 *     its PeerTile), so `data-off-budget="false"` and no `.off-budget-tile`.
 *   - The avatar placeholder text is "Video Disabled" for a genuine camera-off
 *     peer (vs "Video paused" for a camera-ON peer the budget shed).
 *   - A tile's display name is rendered in its `<h4 class="floating-name">`, used
 *     here to locate a specific guest's tile on the host.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

// Same flags as screen-share-panel.spec.ts: fake media so guests can publish a
// synthetic camera without hardware; QUIC origin for WebTransport; single
// renderer process per context to keep CI memory bounded.
const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
];

async function createAuthenticatedContext(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  email: string,
  name: string,
  uiURL: string,
): Promise<BrowserContext> {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });
  const token = generateSessionToken(email, name);
  const url = new URL(uiURL);
  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
  return context;
}

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

  if (result === "waiting") {
    return "waiting";
  }
  if (result === "waiting-for-meeting") {
    return "waiting-for-meeting";
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
  if (guestResult !== "waiting") {
    return;
  }
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

// A `.grid-item` the budget chose not to decode: dashed outline + data flag.
const offBudgetTiles = (page: Page) => page.locator("#grid-container .grid-item.off-budget-tile");
// Decoded video tiles AND camera-off plain avatars both carry
// data-off-budget="false"; the dashed shed tiles carry "true".
const dashedDataTiles = (page: Page) =>
  page.locator('#grid-container .grid-item[data-off-budget="true"]');
// Locate a specific peer's grid tile by its rendered display name.
const tileByName = (page: Page, name: string) =>
  page.locator("#grid-container .grid-item", {
    has: page.locator(`h4.floating-name:has-text("${name}")`),
  });

test.describe("Decode-budget camera-off partition (#1465)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ──────────────────────────────────────────────────────────────────────
  // A camera-OFF remote peer is partitioned OUT of the decode budget:
  //   (a) it renders as a PLAIN avatar — no `.off-budget-tile`, data-off-budget
  //       "false", "Video Disabled" text;
  //   (b) it does NOT consume a budget slot, so under a Fixed(2) cap with
  //       2 camera-ON + 1 camera-OFF remote peers, BOTH camera-ON peers decode
  //       and ZERO tiles are dashed off-budget.
  //
  // Pre-#1465 the camera-off peer counted against the cap; with Fixed(2) and 3
  // peers it would have shed one tile → at least one `.off-budget-tile` would
  // appear and a camera-ON peer could be the one dashed. This test fails in that
  // world, so it is a genuine regression guard for the partition.
  // ──────────────────────────────────────────────────────────────────────
  test("camera-off peer renders as a plain avatar and does not consume a budget slot", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `decode_budget_cam_off_${Date.now()}`;

    // Fixed(2): if camera-off peers counted against the budget (old behavior),
    // 3 remote peers under a cap of 2 would shed exactly one tile. The #1465 fix
    // removes the camera-off peer from the budget population, so the 2 camera-ON
    // peers fit the cap of 2 with nothing shed.
    const FORCED_BUDGET = 2;

    const browserHost = await chromium.launch({ args: BROWSER_ARGS });
    const browserOn1 = await chromium.launch({ args: BROWSER_ARGS });
    const browserOn2 = await chromium.launch({ args: BROWSER_ARGS });
    const browserOff = await chromium.launch({ args: BROWSER_ARGS });

    const hostCtx = await createAuthenticatedContext(
      browserHost,
      "camoffhost@videocall.rs",
      "CamOffHost",
      uiURL,
    );
    // Force the host's decode budget to a fixed cap BEFORE navigation so the
    // assertion is about the BUDGET split, not auto-adaptation timing.
    await hostCtx.addInitScript(
      `localStorage.setItem("vc_decode_budget_override", "${FORCED_BUDGET}");`,
    );

    // Two camera-ON guests: prejoin camera flag true → they publish video →
    // is_video_enabled_for_peer true → they feed the budget.
    const on1Ctx = await createAuthenticatedContext(
      browserOn1,
      "camoffon1@videocall.rs",
      "CamOffOn1",
      uiURL,
    );
    await on1Ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
    const on2Ctx = await createAuthenticatedContext(
      browserOn2,
      "camoffon2@videocall.rs",
      "CamOffOn2",
      uiURL,
    );
    await on2Ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

    // The camera-OFF guest: deliberately leave the prejoin camera flag unset
    // (default OFF) → it never publishes video → is_video_enabled_for_peer
    // false → it is the #1465 camera-off case.
    const offCtx = await createAuthenticatedContext(
      browserOff,
      "camoffoff@videocall.rs",
      "CamOffOff",
      uiURL,
    );
    await offCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "false");`);

    const hostPage = await hostCtx.newPage();
    const on1Page = await on1Ctx.newPage();
    const on2Page = await on2Ctx.newPage();
    const offPage = await offCtx.newPage();

    try {
      // Host joins first.
      await navigateToMeeting(hostPage, meetingId, "CamOffHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // Camera-ON guests join first so they win the earliest join slots in the
      // budget ranking; the camera-off guest joins LAST. Under the OLD behavior
      // a last-joining peer is the one shed by the cap — so if camera-off still
      // counted, the LAST (camera-off) peer would be the dashed off-budget tile.
      await navigateToMeeting(on1Page, meetingId, "CamOffOn1");
      const on1Result = await joinMeetingFromPage(on1Page);
      await admitGuestIfNeeded(hostPage, on1Page, on1Result);

      await navigateToMeeting(on2Page, meetingId, "CamOffOn2");
      const on2Result = await joinMeetingFromPage(on2Page);
      await admitGuestIfNeeded(hostPage, on2Page, on2Result);

      await navigateToMeeting(offPage, meetingId, "CamOffOff");
      const offResult = await joinMeetingFromPage(offPage);
      await admitGuestIfNeeded(hostPage, offPage, offResult);

      // Wait for all 3 remote peer tiles to appear on the host.
      const gridTiles = hostPage.locator("#grid-container .grid-item");
      await expect(gridTiles.nth(2)).toBeVisible({ timeout: 45_000 });

      // Precondition: the two camera-ON peers must actually be publishing video,
      // otherwise a "no dashed tile" result could be a false pass (all peers
      // camera-off would also produce zero dashed tiles). Require >= 2 live
      // canvases before asserting the partition.
      const liveCanvases = hostPage.locator("#grid-container .grid-item canvas");
      await expect(liveCanvases.nth(1)).toBeVisible({ timeout: 30_000 });
      const canvasCount = await liveCanvases.count();
      expect(canvasCount).toBeGreaterThanOrEqual(2);

      // ---- ASSERT (b): the camera-off peer consumed no budget slot ----
      // Both camera-ON peers fit the Fixed(2) cap, so NO tile is dashed
      // off-budget. Pre-#1465 the camera-off peer would have occupied a budget
      // slot and forced one tile into the dashed avatar tier.
      await expect(offBudgetTiles(hostPage)).toHaveCount(0, { timeout: 15_000 });
      await expect(dashedDataTiles(hostPage)).toHaveCount(0, { timeout: 15_000 });

      // ---- ASSERT (a): the camera-off peer is a PLAIN avatar ----
      const offTile = tileByName(hostPage, "CamOffOff");
      await expect(offTile).toBeVisible({ timeout: 15_000 });
      // No dashed off-budget outline on the camera-off tile.
      await expect(offTile).not.toHaveClass(/off-budget-tile/);
      // data-off-budget is "false" (not a budget-shed tile).
      await expect(offTile).toHaveAttribute("data-off-budget", "false");
      // Plain camera-off placeholder text, NOT the "Video paused" shed wording.
      await expect(offTile.locator(".placeholder-text")).toHaveText("Video Disabled");
      // It is an avatar, not a decoded canvas.
      await expect(offTile.locator("canvas")).toHaveCount(0);
    } finally {
      await browserHost.close();
      await browserOn1.close();
      await browserOn2.close();
      await browserOff.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // MID-CALL camera-ON reactivity guard (#1465 follow-up regression).
  //
  // BUG: #1465 partitions camera-OFF peers OUT of the decode budget, so a
  // camera-OFF peer is not in `active_decode_set` and never gets
  // `peer.visible = true`. The partition runs in the AttendantsComponent
  // render body and classifies each peer via the NON-reactive
  // `is_video_enabled_for_peer`. The `peer_status` DiagEvent subscriber bumped
  // a parent-watched version ONLY on a `screen_enabled` change, NOT on a
  // `video_enabled` change. So when a peer joins camera-OFF (the DEFAULT —
  // `load_preferred_camera_on()` is false) and turns the camera ON mid-call,
  // the parent never re-renders, `active_decode_set` stays stale,
  // `peer.visible` stays false, and the peer's video frames are SKIPPED at the
  // `if !self.visible { return SKIPPED }` guard in `peer_decode_manager.rs`.
  // The host's tile shows a blank/frozen canvas (no live `<canvas>`) until
  // some UNRELATED re-render fires.
  //
  // The fix bumps `peer_list_version` (throttled) when an existing peer's
  // `video_enabled` flips, re-running the partition + `set_active_decode_set`.
  //
  // This test runs the host on DEFAULT Auto budget (NO `vc_decode_budget_override`)
  // — an UNPRESSURED host where the ~1 Hz budget loop does NOT continuously
  // re-render the parent. On an unpressured host, before the fix, nothing else
  // forces a parent re-render, so the canvas never appears and the
  // `toHaveCount(1)` assertion below times out. With the fix, the
  // `video_enabled` bump re-runs the partition and the canvas appears.
  // ──────────────────────────────────────────────────────────────────────
  test("camera turned ON mid-call gains a live canvas on an unpressured host", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `decode_budget_cam_on_midcall_${Date.now()}`;

    const browserHost = await chromium.launch({ args: BROWSER_ARGS });
    const browserGuest = await chromium.launch({ args: BROWSER_ARGS });

    // Host: DEFAULT Auto budget — deliberately NO `vc_decode_budget_override`.
    // An unpressured host's ~1 Hz budget loop does not keep re-rendering the
    // parent, so the only thing that can re-run the partition after a mid-call
    // camera-ON is the #1465 `video_enabled` bump under test.
    const hostCtx = await createAuthenticatedContext(
      browserHost,
      "midcallhost@videocall.rs",
      "MidCallHost",
      uiURL,
    );

    // Guest joins CAMERA-OFF (prejoin flag false). It will publish no video at
    // join, so the host renders it as a plain avatar (the #1465 camera-off
    // partition case), then we toggle its camera ON mid-call.
    const guestCtx = await createAuthenticatedContext(
      browserGuest,
      "midcallguest@videocall.rs",
      "MidCallGuest",
      uiURL,
    );
    await guestCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "false");`);

    const hostPage = await hostCtx.newPage();
    const guestPage = await guestCtx.newPage();

    try {
      await navigateToMeeting(hostPage, meetingId, "MidCallHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "MidCallGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // The guest's tile must appear on the host as a plain camera-off avatar
      // (no canvas) before we toggle. This is the #1465 partitioned state.
      // NOTE: with only one remote peer, `is_sole_real_tile` is true so the
      // tile renders FULL-BLEED — and a full-bleed camera-off tile shows the
      // "Camera Off" label (canvas_generator.rs `camera_off_label`), not the
      // grid "Video Disabled" wording (which only appears on non-full-bleed
      // camera-off tiles, exercised by test 1's 3-peer grid above).
      const guestTile = tileByName(hostPage, "MidCallGuest");
      await expect(guestTile).toBeVisible({ timeout: 45_000 });
      await expect(guestTile.locator(".placeholder-text")).toHaveText("Camera Off", {
        timeout: 30_000,
      });
      await expect(guestTile.locator("canvas")).toHaveCount(0);

      // Toggle the guest's camera ON mid-call. The in-meeting camera control
      // exposes the stable `camera-toggle-button` testid
      // (video_control_buttons.rs::CameraButton). Clicking it starts the
      // guest's camera so it begins publishing video.
      const guestCameraToggle = guestPage.locator('[data-testid="camera-toggle-button"]');
      await expect(guestCameraToggle).toBeVisible({ timeout: 15_000 });
      await guestCameraToggle.click();

      // ---- ASSERT: the host's tile for the guest gains a live <canvas> ----
      // The guest is now publishing video, its heartbeat flips video_enabled
      // true, and the #1465 fix bumps peer_list_version so the host re-runs the
      // partition, adds the guest to active_decode_set, sets peer.visible, and
      // decodes its frames. WITHOUT the fix, on this unpressured host nothing
      // re-runs the partition, peer.visible stays false, frames are SKIPPED,
      // and this canvas never appears (the assertion times out).
      await expect(guestTile.locator("canvas")).toHaveCount(1, { timeout: 45_000 });
      await expect(guestTile.locator("canvas")).toBeVisible({ timeout: 15_000 });
    } finally {
      await browserHost.close();
      await browserGuest.close();
    }
  });
});
