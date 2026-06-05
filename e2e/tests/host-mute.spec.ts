import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

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
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });
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

/**
 * Admit a guest from the waiting room if needed.
 * Returns once the guest is fully in the meeting (grid visible).
 */
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

/**
 * Enable the local microphone by clicking the "Unmute" control button.
 * The MicButton uses accessible name from its tooltip span, so
 * getByRole("button", { name: "Unmute" }) reliably targets it.
 */
async function enableMic(page: Page): Promise<void> {
  const unmuteBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Unmute" }),
  });
  await expect(unmuteBtn).toBeVisible({ timeout: 10_000 });
  await unmuteBtn.click();
  await page.waitForTimeout(500);
}

/**
 * Mute a remote peer via the canvas tile's three-dot menu.
 *
 * The host-only mute control is rendered inside each remote peer's grid tile
 * (see `canvas_generator.rs`). It only renders when the host viewer sees the
 * peer with `audio_enabled=true`. The button is hidden via `visibility:
 * hidden` until the parent `.grid-item` is hovered, so the test must hover
 * before interacting.
 *
 * The flow is two-step:
 *   1. Click the menu-toggle button (`title="Host actions"`, an
 *      `.tile-mute-btn`) to open the tile context menu.
 *   2. Click the inner "Mute" item (`.tile-context-menu-item`) to actually
 *      invoke `on_mute` and broadcast the host-mute via NATS.
 *
 * We scope to a `.grid-item` that contains a `.tile-mute-btn` to avoid
 * matching the host's own tile, which never renders the mute button.
 */
async function hostMutePeerViaTile(page: Page): Promise<void> {
  const guestTile = page.locator(".grid-item:has(.tile-mute-btn)").first();
  await expect(guestTile).toBeVisible({ timeout: 30_000 });

  // Hover to reveal `.tile-mute-btn` (CSS sets visibility:hidden until
  // `.grid-item:hover`).
  await guestTile.hover();

  const muteToggle = guestTile.getByTitle("Host actions");
  await expect(muteToggle).toBeVisible({ timeout: 15_000 });
  await muteToggle.click();

  // The inner "Mute" item only appears once the menu is open. It has no
  // title attribute — match by text inside `.tile-context-menu-item`.
  const muteMenuItem = guestTile.locator(".tile-context-menu-item", {
    hasText: "Mute",
  });
  await expect(muteMenuItem).toBeVisible({ timeout: 5_000 });
  await muteMenuItem.click();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Host mute controls", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Test 1: Host mutes a single participant via the per-tile three-dot menu.
   *
   * The tile mute menu (rendered in `canvas_generator.rs`) appears only when:
   *   - the viewer is the host (is_owner = true), AND
   *   - the target peer is not the viewer themselves, AND
   *   - the peer's audio_enabled is reported as true by diagnostics.
   * Therefore the guest must have their mic on before the host can see the
   * three-dot menu on the guest's tile.
   */
  test("host mutes a single participant", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostmute_single_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-mute@videocall.rs",
        "MuteHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-mute@videocall.rs",
        "MuteGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "MuteHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "MuteGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for the peer connection to establish (host sees guest's tile).
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 45_000,
      });

      // Brief stabilization for the WebRTC data channel to finish setup
      // after the tile renders — audio track state propagation needs this.
      await hostPage.waitForTimeout(2000);

      // Guest enables their microphone so the host's per-tile diagnostics
      // reflect audio_enabled=true and the tile mute menu is rendered.
      await enableMic(guestPage);

      // ---- Host opens the tile menu and clicks the inner "Mute" item ----
      // The tile menu-toggle (`title="Host actions"`) only opens the
      // context menu — it does not call on_mute directly. The inner "Mute"
      // menu item is what actually triggers the host-mute broadcast.
      await hostMutePeerViaTile(hostPage);

      // ---- Guest receives the host-mute NATS event and sees the toast ----
      const guestMuteToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host muted your microphone",
      });
      await expect(guestMuteToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- "Mute" item gone from the tile context menu (peer is now muted) ----
      //
      // The three-dot "Host actions" button stays visible (kick/disable-video
      // are still available) but the "Mute" menu item inside is no longer
      // rendered because on_mute becomes None when audio_enabled is false.
      //
      // #1034 supersedes #985 here. Previously the removal was gated on the
      // guest's diagnostics RE-BROADCAST carrying `audio_enabled=false` — a
      // slow multi-hop chain (host.mute -> NATS -> guest mutes locally ->
      // guest's next diagnostics tick, ~1-5s -> host receives -> on_mute
      // becomes None -> re-render). That ~5s+ lag is exactly why #985 widened
      // this bound from 10s to 30s.
      //
      // With the #1034 fix the host's OWN client also receives the
      // HOST_MUTE_PARTICIPANT broadcast it just sent, and
      // `force_peer_media_off(target, audio_off=true)` sets the target peer's
      // `audio_enabled=false` DIRECTLY on the host's decode manager
      // (bypassing the heartbeat freshness window) and broadcasts peer-status.
      // So `is_audio_enabled_for_peer(target)` is false within a render frame,
      // on_mute becomes None, and the "Mute" item is removed almost
      // immediately — no longer waiting on the guest's diagnostics tick.
      //
      // The bound is therefore tightened back to a FAR-below-30s value. 5s is
      // generous slack for NATS round-trip + Dioxus re-render under busy-CI
      // load, but a regression of #1034 (reverting to the heartbeat-only path)
      // re-introduces the ~5s+ lag and would flake/fail this assertion.
      const hostActionsBtn = hostPage.getByTitle("Host actions");
      await hostActionsBtn.hover();
      await hostActionsBtn.click();
      await expect(hostPage.locator(".tile-context-menu-item", { hasText: "Mute" })).toHaveCount(
        0,
        { timeout: 5_000 },
      );
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 2 (#1036): "Mute all" reflects on a non-host OBSERVER's screen
   * immediately for EVERY other participant, while the host's OWN tile stays
   * UNMUTED on that observer's screen.
   *
   * Three peers: host + a target guest + an observer guest. The host issues
   * mute-all; the observer (a non-host third peer) is the witness.
   *
   * #1035 made the SPECIFIC-target host mute fast (force-off on every client)
   * but left mute-all on the slow heartbeat path, because the client had no
   * way to know which peer was the host and would otherwise force-mute the
   * host's own tile too. #1036 fixes this: the server now carries the issuing
   * host's `user_id` in the broadcast's `creator_id`, so the receiving client
   * calls `force_all_peers_media_off_except(host_id, …)` — force-muting every
   * peer EXCEPT the host, immediately and bypassing the ~5s heartbeat freshness
   * window.
   *
   * Assertions, all from the observer's vantage point:
   *   1. The TARGET guest's mic icon flips to MUTED within a TIGHT 3s bound
   *      (the muted MicIcon's slash line `<line x1="1">` appears). A regression
   *      to the heartbeat-only mute-all path would lag ~5s and fail this.
   *   2. The HOST's own tile stays UNMUTED — the muted slash line never appears
   *      on the host tile. This is the host-exclusion guarantee of #1036: a
   *      regression that force-muted everyone (no `creator_id` exclusion) would
   *      flip the host tile too and fail this.
   * Plus the original guarantees: the guest sees the mute toast, the host does
   * not (on_host_mute is None for owner), and the host's own mic stays active.
   *
   * Reuses the per-tile DOM selectors established by Test 4: tiles are scoped
   * by `[data-tile-root="true"]` containing an `h4.floating-name`, and the
   * muted state is the `.audio-indicator svg line[x1="1"]` slash.
   */
  test("host mute-all mutes every guest but not the host (observer view, #1036)", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostmute_all_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    const browser3 = await chromium.launch({ args: BROWSER_ARGS });

    // Display names: the observer scopes the host's and target's tiles by these
    // names rendered in each tile's `h4.floating-name`.
    const HOST_NAME = "MuteAllHost";
    const TARGET_NAME = "MuteAllTarget";

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-muteall@videocall.rs",
        HOST_NAME,
        uiURL,
      );
      const targetCtx = await createAuthenticatedContext(
        browser2,
        "target-muteall@videocall.rs",
        TARGET_NAME,
        uiURL,
      );
      const observerCtx = await createAuthenticatedContext(
        browser3,
        "observer-muteall@videocall.rs",
        "MuteAllObserver",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const targetPage = await targetCtx.newPage();
      const observerPage = await observerCtx.newPage();

      // ---- Host joins first (becomes owner) ----
      await navigateToMeeting(hostPage, meetingId, HOST_NAME);
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // ---- Target joins and is admitted ----
      await navigateToMeeting(targetPage, meetingId, TARGET_NAME);
      const targetResult = await joinMeetingFromPage(targetPage);
      await admitGuestIfNeeded(hostPage, targetPage, targetResult);

      // ---- Observer joins and is admitted ----
      await navigateToMeeting(observerPage, meetingId, "MuteAllObserver");
      const observerResult = await joinMeetingFromPage(observerPage);
      await admitGuestIfNeeded(hostPage, observerPage, observerResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(targetPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(observerPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Host must see a remote tile (peer connection established) before muting.
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 45_000,
      });

      // Brief stabilization for audio track state to propagate before unmuting.
      await hostPage.waitForTimeout(2000);

      // Enable mic on the host and the target so the observer sees BOTH unmuted
      // before mute-all. (The observer's own mic is irrelevant.)
      await enableMic(hostPage);
      await enableMic(targetPage);

      // Confirm host mic is currently on — "Mute" tooltip means it's active.
      const hostActiveMicBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Mute" }),
      });
      await expect(hostActiveMicBtn).toBeVisible({ timeout: 5_000 });

      // Observer's tiles for the TARGET and the HOST, scoped by display name.
      const observerTargetTile = observerPage
        .locator('[data-tile-root="true"]', {
          has: observerPage.locator("h4.floating-name", { hasText: TARGET_NAME }),
        })
        .first();
      const observerHostTile = observerPage
        .locator('[data-tile-root="true"]', {
          has: observerPage.locator("h4.floating-name", { hasText: HOST_NAME }),
        })
        .first();
      await expect(observerTargetTile).toBeVisible({ timeout: 45_000 });
      await expect(observerHostTile).toBeVisible({ timeout: 45_000 });

      // Pre-state: the observer sees BOTH the target and the host UNMUTED — the
      // muted slash line is absent on each. This proves we observe a real mute
      // transition on the target, and that the host starts unmuted (so the
      // post-mute-all "host stays unmuted" check is meaningful).
      await expect(observerTargetTile.locator('.audio-indicator svg line[x1="1"]')).toHaveCount(0, {
        timeout: 30_000,
      });
      await expect(observerHostTile.locator('.audio-indicator svg line[x1="1"]')).toHaveCount(0, {
        timeout: 30_000,
      });

      // ---- Host opens peer list then clicks "Mute all" via context menu ----

      // The "Open Peers" button lives inside `.controls-secondary`, which
      // collapses to `max-width: 0; opacity: 0` after a 1s no-mouse-activity
      // timer fires (see attendants.rs auto-hide hook). Hover the controls
      // container first so `controls-expanded` stays set and the secondary
      // buttons remain visible/clickable while we navigate the menu.
      await hostPage.locator(".video-controls-container").hover();

      // Wake auto-hidden controls bar before clicking.
      await hostPage.mouse.move(400, 400);
      await hostPage.waitForTimeout(300);

      const openPeersBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Open Peers" }),
      });
      await expect(openPeersBtn).toBeVisible({ timeout: 10_000 });
      await openPeersBtn.click();
      await hostPage.waitForTimeout(1000);

      // Scope to `.in-call-menu-wrapper` so we match the peer-list panel's
      // "Host actions" button, not the per-tile button (same aria-label).
      const hostActionsBtn = hostPage.locator(
        '.in-call-menu-wrapper button[aria-label="Host actions"]',
      );
      await expect(hostActionsBtn).toBeVisible({ timeout: 10_000 });
      await hostActionsBtn.click();

      const muteAllItem = hostPage.locator("button.context-menu-item", { hasText: "Mute all" });
      await expect(muteAllItem).toBeVisible({ timeout: 5_000 });
      await muteAllItem.click();

      // ---- #1036: observer sees the TARGET's mic flip to MUTED FAST ----
      // The muted MicIcon slash line appears only when
      // `is_audio_enabled_for_peer(target)` is false on the observer's client.
      // With #1036 the authoritative mute-all flips that immediately for every
      // non-host peer (no heartbeat-window wait), so a TIGHT 3s bound holds.
      await expect(
        observerTargetTile.locator('.audio-indicator svg line[x1="1"]').first(),
      ).toBeVisible({ timeout: 3_000 });

      // ---- #1036 host-exclusion: the HOST's tile stays UNMUTED on the
      // observer's screen. The host issued mute-all; it must not mute itself.
      // The observer must NEVER see the muted slash on the host tile. We hold
      // this for a window comfortably past the 3s fast-path bound to catch a
      // regression that (wrongly) force-muted everyone including the host.
      await expect(observerHostTile.locator('.audio-indicator svg line[x1="1"]')).toHaveCount(0);
      await observerPage.waitForTimeout(3000);
      await expect(observerHostTile.locator('.audio-indicator svg line[x1="1"]')).toHaveCount(0);

      // ---- Target receives the NATS broadcast and sees the mute toast ----
      const targetMuteToast = targetPage.locator(".peer-toast .toast-name", {
        hasText: "Host muted your microphone",
      });
      await expect(targetMuteToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- Host does NOT see the mute toast (on_host_mute is None for owner) ----
      await expect(
        hostPage.locator(".peer-toast .toast-name", {
          hasText: "Host muted your microphone",
        }),
      ).toHaveCount(0);

      // ---- Host's own mic remains active after mute-all ----
      await expect(hostActiveMicBtn).toBeVisible();
    } finally {
      await browser1.close();
      await browser2.close();
      await browser3.close();
    }
  });

  /**
   * Test 3: A participant can self-unmute after being muted by the host.
   *
   * After the host mutes the guest, the guest's on_host_mute callback sets
   * mic_enabled=false. The guest can re-enable their mic by clicking the
   * "Unmute" button (same toggle they use for self-mute).
   */
  test("participant can self-unmute after being muted by host", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_selfunmute_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-selfunmute@videocall.rs",
        "SelfUnmuteHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-selfunmute@videocall.rs",
        "SelfUnmuteGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "SelfUnmuteHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "SelfUnmuteGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait for peer connection
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- Guest enables their mic ----
      await enableMic(guestPage);

      // Confirm guest mic is active ("Mute" tooltip visible = mic on).
      const guestMuteBtn = guestPage.locator("button.video-control-button", {
        has: guestPage.locator("span.tooltip", { hasText: "Mute" }),
      });
      await expect(guestMuteBtn).toBeVisible({ timeout: 5_000 });

      // ---- Host mutes the guest via the per-tile three-dot menu ----
      await hostMutePeerViaTile(hostPage);

      // ---- Guest sees the mute toast ----
      const guestMuteToast = guestPage.locator(".peer-toast .toast-name", {
        hasText: "Host muted your microphone",
      });
      await expect(guestMuteToast.first()).toBeVisible({ timeout: 15_000 });

      // ---- Guest's mic button now shows "Unmute" (mic is off) ----
      const guestUnmuteBtn = guestPage.locator("button.video-control-button", {
        has: guestPage.locator("span.tooltip", { hasText: "Unmute" }),
      });
      await expect(guestUnmuteBtn).toBeVisible({ timeout: 10_000 });

      // ---- Guest self-unmutes ----
      await guestUnmuteBtn.click();

      // ---- Guest's mic is active again — "Mute" button reappears ----
      await expect(
        guestPage.locator("button.video-control-button", {
          has: guestPage.locator("span.tooltip", { hasText: "Mute" }),
        }),
      ).toBeVisible({ timeout: 10_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Test 4 (#1034): a NON-host, NON-target THIRD peer (the "observer") sees the
   * target's mic icon flip to MUTED **immediately** after the host mutes the
   * target — bypassing the receiver-side heartbeat freshness window.
   *
   * Before the fix, the observer's mic icon for the target lagged ~5s behind
   * the host action, because the target's own off-heartbeat (audio_enabled
   * false) was suppressed inside the `MEDIA_FRESH_WINDOW_MS` guard while
   * straggler audio packets were still arriving. The fix makes the
   * authoritative HOST_MUTE_PARTICIPANT command call `force_peer_media_off`
   * on every client, setting the target peer's `audio_enabled=false` directly
   * and broadcasting peer-status — so the observer's
   * `is_audio_enabled_for_peer(target)` goes false at once and the tile's
   * `MicIcon { muted: true }` renders.
   *
   * DOM signal: the muted MicIcon SVG (icons/mic.rs) renders a slash line
   * `<line x1="1" y1="1" x2="23" y2="23">` that the unmuted icon never has
   * (the unmuted icon's only `<line>` uses x1="12"). We scope to the target's
   * tile by display name and assert that slash line appears within a TIGHT 3s
   * bound. A regression to the heartbeat-only path would not surface the
   * muted icon for ~5s and would blow this bound.
   */
  test("non-target third peer sees target muted immediately (#1034)", async ({ baseURL }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostmute_observer_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    const browser3 = await chromium.launch({ args: BROWSER_ARGS });

    // Display name of the target guest — the observer scopes the target's tile
    // by this name (rendered in the tile's `h4.floating-name`).
    const TARGET_NAME = "MuteObsTarget";

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-muteobserver@videocall.rs",
        "MuteObsHost",
        uiURL,
      );
      const targetCtx = await createAuthenticatedContext(
        browser2,
        "target-muteobserver@videocall.rs",
        TARGET_NAME,
        uiURL,
      );
      const observerCtx = await createAuthenticatedContext(
        browser3,
        "observer-muteobserver@videocall.rs",
        "MuteObsObserver",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const targetPage = await targetCtx.newPage();
      const observerPage = await observerCtx.newPage();

      // ---- Host joins first (becomes owner) ----
      await navigateToMeeting(hostPage, meetingId, "MuteObsHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      // ---- Target joins and is admitted ----
      await navigateToMeeting(targetPage, meetingId, TARGET_NAME);
      const targetResult = await joinMeetingFromPage(targetPage);
      await admitGuestIfNeeded(hostPage, targetPage, targetResult);

      // ---- Observer joins and is admitted ----
      await navigateToMeeting(observerPage, meetingId, "MuteObsObserver");
      const observerResult = await joinMeetingFromPage(observerPage);
      await admitGuestIfNeeded(hostPage, observerPage, observerResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(targetPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(observerPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Host must see the target's tile (per-tile host menu) so it can mute.
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 45_000,
      });

      // Brief stabilization for audio track state to propagate before the
      // target unmutes (mirrors Test 1).
      await hostPage.waitForTimeout(2000);

      // ---- Target turns its mic ON ----
      await enableMic(targetPage);

      // The observer's tile for the TARGET, scoped by the target's display
      // name in the floating-name header.
      const observerTargetTile = observerPage
        .locator('[data-tile-root="true"]', {
          has: observerPage.locator("h4.floating-name", { hasText: TARGET_NAME }),
        })
        .first();
      await expect(observerTargetTile).toBeVisible({ timeout: 45_000 });

      // Pre-state: the observer sees the target UNMUTED — the muted slash line
      // is absent. This proves we observe a real mute transition rather than
      // an already-muted tile. (The unmuted MicIcon has no `line[x1="1"]`.)
      await expect(observerTargetTile.locator('.audio-indicator svg line[x1="1"]')).toHaveCount(0, {
        timeout: 30_000,
      });

      // ---- Host mutes the target via the per-tile menu ----
      await hostMutePeerViaTile(hostPage);

      // ---- #1034: observer sees the target mic icon flip to MUTED FAST ----
      // The muted MicIcon's slash line appears only when
      // `is_audio_enabled_for_peer(target)` is false on the observer's client.
      // With the fix, the authoritative host command flips that immediately
      // (no heartbeat-window wait), so a TIGHT 3s bound holds. A regression to
      // the heartbeat-only path would lag ~5s and fail this assertion.
      await expect(
        observerTargetTile.locator('.audio-indicator svg line[x1="1"]').first(),
      ).toBeVisible({ timeout: 3_000 });
    } finally {
      await browser1.close();
      await browser2.close();
      await browser3.close();
    }
  });
});
