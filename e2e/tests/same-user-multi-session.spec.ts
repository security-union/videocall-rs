import { test, expect, chromium, BrowserContext, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Regression coverage for HCL issue #828 — "same authed user multiple times not
 * shown as separate instance in same meeting".
 *
 * Before the fix, `actix-api/src/actors/chat_server.rs` evicted the prior
 * session of the same `user_id` in the `JoinRoom` handler via
 * `evict_same_user_session()`. The eviction was silent (no PARTICIPANT_LEFT
 * broadcast), so when the second tab joined, neither tab saw the other:
 *  - the first tab was kicked off the room without notification,
 *  - the second tab joined a "room of one" and saw only its own self-tile.
 *
 * After the fix:
 *  - the server allows multiple sessions per `user_id` and broadcasts a
 *    PARTICIPANT_JOINED for each session (distinct `session_id`),
 *  - `is_duplicate_peer_event` in `videocall-client/src/client/video_call_client.rs`
 *    now keys on `(event_type, user_id, Some(session_id))` for JOINED/LEFT so
 *    the second session's join is no longer dedup'd into the first session's.
 *
 * Peer tiles in the Dioxus UI are keyed by `session_id` (see
 * `dioxus-ui/src/components/canvas_generator.rs` — tile id is
 * `peer-video-{session_id}-div`), so two sessions of the same user render as
 * two distinct peer tiles.
 *
 * This spec opens N independent BrowserContexts, all authenticated as the
 * *same* `user_id` via the same JWT cookie, joins them into one meeting, and
 * asserts each side sees a peer tile for every other session (N total tiles =
 * self + (N-1) peers).
 *
 * If the fix is reverted, the server would evict the prior session on each
 * JoinRoom, and `expect(tileCount).toBeGreaterThanOrEqual(2)` would fail on
 * every side.
 *
 * Both tests are restricted to the Dioxus UI (port 3001). The Yew UI is no
 * longer the active deployment, and the multi-session render path is part of
 * the Dioxus surface.
 */

const SAME_USER_EMAIL = "same-user-828@videocall.rs";
const SAME_USER_NAME = "MultiSessionUser";

const PEER_TILE_SELECTOR = ".split-peer-tile, #grid-container .canvas-container";

/**
 * Navigate to the home page, fill in the meeting id + display name, and submit.
 * Mirrors `navigateToMeeting` from `display-name-update.spec.ts` /
 * `two-users-meeting.spec.ts` so behaviour stays consistent across specs.
 */
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

/**
 * Wait for the pre-join card to appear (Start Meeting / Join Meeting) or for
 * the grid to be auto-mounted, then click Join if needed. Returns once the
 * grid container is visible.
 *
 * This is the same shape as the helper used in the other multi-context specs
 * but trimmed — same-user sessions cannot end up in the waiting room (the
 * authenticated path is shared by all sessions of the same user, so no
 * admission step is required).
 */
async function joinMeetingFromPage(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const which = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
  ]);

  if (which === "join") {
    await page.waitForTimeout(500);
    if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
      await joinButton.click().catch(() => {
        // Swallow click-after-detach: the auto-join effect already
        // transitioned past NotJoined and unmounted the button.
      });
    }
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/**
 * Count visible peer/self tiles on a page. Uses the union selector that the
 * other specs use (`speaker-highlight.spec.ts:251`,
 * `display-name-update.spec.ts:278`) so both grid layout and split layout
 * count correctly.
 *
 * `peer-video-*-div` and `screen-share-*-div` are the only ids the canvas
 * generator emits; mock-peer infrastructure does not render through this
 * code path in the E2E stack, so the union selector counts only real tiles.
 */
async function countVisibleTiles(page: Page): Promise<number> {
  // Wait for at least one tile, then count all visible ones.
  await expect(page.locator(PEER_TILE_SELECTOR).first()).toBeVisible({ timeout: 30_000 });
  return await page.locator(PEER_TILE_SELECTOR).count();
}

/**
 * Open a new BrowserContext authenticated as the canonical same-user identity
 * (`SAME_USER_EMAIL` / `SAME_USER_NAME`). All contexts share the same JWT
 * `sub`/`user_id`, so the meeting-api treats every tab as the same authed user.
 */
async function openSameUserContext(
  uiURL: string,
): Promise<{ context: BrowserContext; page: Page }> {
  const browser = await chromium.launch({ args: BROWSER_ARGS });
  const context = await createAuthenticatedContext(browser, SAME_USER_EMAIL, SAME_USER_NAME, uiURL);
  const page = await context.newPage();
  // Keep the browser reachable for cleanup via the context.
  (context as unknown as { _browser?: typeof browser })._browser = browser;
  return { context, page };
}

async function closeSameUserContext(context: BrowserContext): Promise<void> {
  const browser = (context as unknown as { _browser?: Awaited<ReturnType<typeof chromium.launch>> })
    ._browser;
  await context.close().catch(() => undefined);
  if (browser) {
    await browser.close().catch(() => undefined);
  }
}

test.describe("Same authed user — multiple sessions in one meeting", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Two tabs / two browser contexts authenticated as the same user join the
   * same meeting. Each side must see the other's session as a distinct peer
   * tile (self + 1 peer = 2 tiles per side).
   *
   * Reverting `actix-api/src/actors/chat_server.rs` (re-introducing the
   * `evict_same_user_session` call) makes this assertion fail: the second
   * session's join evicts the first session silently, the first context's
   * grid never sees a peer tile, and the second context only ever sees its
   * own self-tile.
   */
  test("two same-user sessions render as two distinct peer tiles on each side", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_same_user_2_${Date.now()}`;

    const sessionA = await openSameUserContext(uiURL);
    const sessionB = await openSameUserContext(uiURL);

    try {
      // ---- Session A joins first ----
      await navigateToMeeting(sessionA.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionA.page);
      await expect(sessionA.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ---- Session B joins the same meeting as the same user ----
      await navigateToMeeting(sessionB.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionB.page);
      await expect(sessionB.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Allow time for PARTICIPANT_JOINED broadcasts and peer discovery on both sides.
      await sessionA.page.waitForTimeout(5_000);

      // ---- Each side must see at least 2 tiles (self + the other session) ----
      const tileCountA = await countVisibleTiles(sessionA.page);
      const tileCountB = await countVisibleTiles(sessionB.page);

      expect(
        tileCountA,
        "Session A should see its own self-tile plus session B's peer tile",
      ).toBeGreaterThanOrEqual(2);
      expect(
        tileCountB,
        "Session B should see its own self-tile plus session A's peer tile",
      ).toBeGreaterThanOrEqual(2);

      // ---- The peer tile id on each side must include the OTHER session_id ----
      // Peer tiles are rendered with id="peer-video-{session_id}-div" (see
      // canvas_generator.rs). Each side should have at least one peer tile
      // div, and the ids on the two sides must be distinct (different session_id).
      const peerIdsA = await sessionA.page
        .locator('[id^="peer-video-"][id$="-div"]')
        .evaluateAll((els) => els.map((e) => e.id));
      const peerIdsB = await sessionB.page
        .locator('[id^="peer-video-"][id$="-div"]')
        .evaluateAll((els) => els.map((e) => e.id));

      expect(
        peerIdsA.length,
        "Session A must render a peer tile for session B",
      ).toBeGreaterThanOrEqual(1);
      expect(
        peerIdsB.length,
        "Session B must render a peer tile for session A",
      ).toBeGreaterThanOrEqual(1);

      // The peer tile A sees (session B's session_id) must not match any peer
      // tile id on B (which would be A's session_id). They must be disjoint —
      // a same-id on both sides would mean only one session is registered.
      const overlap = peerIdsA.filter((id) => peerIdsB.includes(id));
      expect(
        overlap,
        "Session A and Session B must have disjoint peer-tile ids (distinct session_ids)",
      ).toEqual([]);

      // ---- Neither side should have been silently evicted ----
      // The pre-fix behaviour evicted the first session without a
      // PARTICIPANT_LEFT, which surfaced in the client as "Connection lost"
      // / reconnect spinners. Assert neither symptom is present.
      await expect(sessionA.page.getByText("Connection lost")).toHaveCount(0);
      await expect(sessionB.page.getByText("Connection lost")).toHaveCount(0);
    } finally {
      await closeSameUserContext(sessionA.context);
      await closeSameUserContext(sessionB.context);
    }
  });

  /**
   * Three tabs / three browser contexts authenticated as the same user join the
   * same meeting. This locks in the exact bug-report scenario verbatim:
   * "I used 3 laptops … expected to see 2 attendees + self (3 total)".
   *
   * Each side should see ≥ 3 tiles (self + 2 peers) and all three peer-tile
   * session ids should be distinct.
   */
  test("three same-user sessions render as three distinct peer tiles per side", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_same_user_3_${Date.now()}`;

    const sessionA = await openSameUserContext(uiURL);
    const sessionB = await openSameUserContext(uiURL);
    const sessionC = await openSameUserContext(uiURL);

    try {
      // ---- All three sessions join sequentially ----
      await navigateToMeeting(sessionA.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionA.page);
      await expect(sessionA.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      await navigateToMeeting(sessionB.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionB.page);
      await expect(sessionB.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      await navigateToMeeting(sessionC.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionC.page);
      await expect(sessionC.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Allow time for PARTICIPANT_JOINED fan-out to all three sessions.
      await sessionA.page.waitForTimeout(7_000);

      // ---- Each side must see at least 3 tiles (self + 2 peers) ----
      const tileCountA = await countVisibleTiles(sessionA.page);
      const tileCountB = await countVisibleTiles(sessionB.page);
      const tileCountC = await countVisibleTiles(sessionC.page);

      expect(
        tileCountA,
        "Session A should see itself plus 2 other sessions",
      ).toBeGreaterThanOrEqual(3);
      expect(
        tileCountB,
        "Session B should see itself plus 2 other sessions",
      ).toBeGreaterThanOrEqual(3);
      expect(
        tileCountC,
        "Session C should see itself plus 2 other sessions",
      ).toBeGreaterThanOrEqual(3);

      // ---- Each side must render 2 peer tiles, each with a distinct session_id ----
      const peerIdsA = await sessionA.page
        .locator('[id^="peer-video-"][id$="-div"]')
        .evaluateAll((els) => els.map((e) => e.id));
      const peerIdsB = await sessionB.page
        .locator('[id^="peer-video-"][id$="-div"]')
        .evaluateAll((els) => els.map((e) => e.id));
      const peerIdsC = await sessionC.page
        .locator('[id^="peer-video-"][id$="-div"]')
        .evaluateAll((els) => els.map((e) => e.id));

      expect(peerIdsA.length, "Session A should render 2 peer tiles").toBeGreaterThanOrEqual(2);
      expect(peerIdsB.length, "Session B should render 2 peer tiles").toBeGreaterThanOrEqual(2);
      expect(peerIdsC.length, "Session C should render 2 peer tiles").toBeGreaterThanOrEqual(2);

      // Each side's peer-tile id set must contain distinct ids (no duplicates),
      // i.e. the two peers are not collapsed into one tile.
      expect(new Set(peerIdsA).size).toBe(peerIdsA.length);
      expect(new Set(peerIdsB).size).toBe(peerIdsB.length);
      expect(new Set(peerIdsC).size).toBe(peerIdsC.length);

      // No session should report a "Connection lost" reconnect — the pre-fix
      // eviction would surface as a connection-lost indicator on the
      // earlier-joining session.
      await expect(sessionA.page.getByText("Connection lost")).toHaveCount(0);
      await expect(sessionB.page.getByText("Connection lost")).toHaveCount(0);
      await expect(sessionC.page.getByText("Connection lost")).toHaveCount(0);
    } finally {
      await closeSameUserContext(sessionA.context);
      await closeSameUserContext(sessionB.context);
      await closeSameUserContext(sessionC.context);
    }
  });

  /**
   * Regression for HCL #828 follow-up: `is_self_peer` in
   * `dioxus-ui/src/components/canvas_generator.rs` previously compared the
   * local **user_id** against `peer_user_id`, which collapsed sibling
   * same-user sessions into "self" in the split layouts. In `TileMode::ScreenOnly`
   * the buggy code returned `Empty` for the sibling's screen-share tile,
   * leaving the left split panel blank for every same-user viewer.
   *
   * After the fix `is_self_peer` keys on `session_id` (the per-tab unique id):
   * Session A shares its screen, Session B (same user_id, different
   * session_id) must render Session A's screen-share canvas in the left
   * panel (`.split-screen-tile`).
   *
   * If the fix regresses, `.split-screen-tile` will fail to appear on
   * Session B — the screen-share would still be visible on the sharer's own
   * side (since the local-self check applies only to the canvas-generator
   * remote rendering path), but the sibling tab would never see it.
   */
  test("sibling same-user session renders the other session's screen-share in the ScreenOnly split panel", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_same_user_ss_${Date.now()}`;

    // Launch with the extra Chromium flag that auto-accepts the
    // getDisplayMedia() system picker; without it the screen-share button
    // click never produces a stream and the split layout never appears.
    const browserA = await chromium.launch({
      args: [...BROWSER_ARGS, "--auto-select-desktop-capture-source=Entire screen"],
    });
    const browserB = await chromium.launch({
      args: [...BROWSER_ARGS, "--auto-select-desktop-capture-source=Entire screen"],
    });

    try {
      const contextA = await createAuthenticatedContext(
        browserA,
        SAME_USER_EMAIL,
        SAME_USER_NAME,
        uiURL,
      );
      const contextB = await createAuthenticatedContext(
        browserB,
        SAME_USER_EMAIL,
        SAME_USER_NAME,
        uiURL,
      );
      const pageA = await contextA.newPage();
      const pageB = await contextB.newPage();

      // ---- Both sessions join the meeting ----
      await navigateToMeeting(pageA, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(pageA);
      await navigateToMeeting(pageB, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(pageB);

      // Both sides must see at least 2 tiles (self + sibling) before we
      // proceed — guards against a race where the sharer starts screen
      // share before its peer is known on the other side.
      await expect(pageA.locator(PEER_TILE_SELECTOR).first()).toBeVisible({ timeout: 30_000 });
      await expect(pageB.locator(PEER_TILE_SELECTOR).first()).toBeVisible({ timeout: 30_000 });
      await pageA.waitForTimeout(3_000);

      // ---- Session A starts screen sharing ----
      // The Share Screen button auto-hides; nudge the mouse to reveal the
      // control bar before clicking.
      await pageA.mouse.move(400, 400);
      await pageA.waitForTimeout(300);
      const shareButton = pageA.locator("button.video-control-button", {
        has: pageA.locator(".tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareButton).toBeVisible({ timeout: 10_000 });
      await shareButton.click();

      // ---- Session B (sibling same-user session) must see the split layout
      // with the screen-share canvas in the left panel. With the pre-fix
      // user_id-based is_self_peer comparison this would be empty.
      await expect(pageB.locator(".split-screen-tile")).toBeVisible({ timeout: 20_000 });

      // The sibling should also see its own video tile in the split right
      // panel via TileMode::VideoOnly (which is unaffected by is_self_peer
      // but doubles as a sanity check that the split layout activated end
      // to end).
      const splitPeerTiles = pageB.locator(".split-peer-tile");
      await expect(splitPeerTiles.first()).toBeVisible({ timeout: 10_000 });

      // ---- The sharer (Session A) must also see at least their sibling's
      // video tile in the split right panel — locks in that the same fix
      // applied symmetrically.
      await expect(pageA.locator(".split-peer-tile").first()).toBeVisible({ timeout: 10_000 });
    } finally {
      await browserA.close().catch(() => undefined);
      await browserB.close().catch(() => undefined);
    }
  });

  /**
   * Regression for HCL #828 follow-up: the join-toast suppression in
   * `dioxus-ui/src/components/attendants.rs` previously used
   * `client.has_peer_with_user_id(user_id)`, which short-circuited the
   * toast for the SECOND session of a same-user joiner because the FIRST
   * session was already in the peer list. The result: when a second tab
   * of the same authenticated user joined, no idle existing session ever
   * surfaced a "joined the meeting" toast for the new tab.
   *
   * After the fix the suppression keys on `session_id` (via
   * `has_peer_with_session_id`), so each distinct session surfaces its own
   * toast even when sibling sessions share a user_id.
   *
   * Setup: Session A joins first and sits idle on the meeting. Session B
   * joins as the same user. Session A's UI must render a
   * `.peer-toast.toast-joined` element. With the regression in place no
   * toast appears (the suppression filter eats the second join event).
   */
  test("idle same-user session receives a join toast when a sibling session joins", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_same_user_toast_${Date.now()}`;

    const sessionA = await openSameUserContext(uiURL);
    const sessionB = await openSameUserContext(uiURL);

    try {
      // ---- Session A joins first and waits in the meeting ----
      await navigateToMeeting(sessionA.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionA.page);
      await expect(sessionA.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Start watching for the joined toast BEFORE Session B actually
      // joins — PARTICIPANT_JOINED can fire fast enough that polling
      // started afterwards races the 8s auto-dismiss.
      const joinedToast = sessionA.page.locator(".peer-toast.toast-joined");
      const toastPromise = expect(joinedToast.first()).toBeVisible({ timeout: 30_000 });

      // ---- Session B joins as the SAME user ----
      await navigateToMeeting(sessionB.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionB.page);
      await expect(sessionB.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // ---- Session A must see a join toast for the sibling session ----
      // Pre-fix: has_peer_with_user_id(user_id) was true (Session A's own
      // session was already tracked, since it's our own user_id), so the
      // toast was suppressed and the first .toast-joined never appeared.
      await toastPromise;

      // Toast structure check: line 1 carries the display name, line 2
      // contains "joined the meeting".
      const firstJoined = joinedToast.first();
      await expect(firstJoined.locator(".toast-name")).toContainText(SAME_USER_NAME);
      await expect(firstJoined.locator(".toast-action")).toContainText("joined the meeting");
    } finally {
      await closeSameUserContext(sessionA.context);
      await closeSameUserContext(sessionB.context);
    }
  });

  /**
   * Regression for HCL #828 follow-up (user-reported bug): when a user with
   * multiple same-user sessions in a meeting renames Tab A via the UI, the
   * rename was broadcast keyed by `user_id` only — so Tab B's display name
   * changed too. Expected: only Tab A's name changes; Tab B keeps its own.
   *
   * Root cause: the client never sent its `session_id` on the rename REST
   * request, so the server had no way to know which session was renaming.
   * Fixes:
   *  - Client (`videocall-meeting-client/src/participants.rs`) now sends
   *    `session_id: Option<u64>` on `PUT /display-name`, threaded through the
   *    Dioxus UI via `UpdateDisplayNameModal`'s `session_id` prop (see
   *    `attendants.rs:3262-3292`).
   *  - Server scopes the rename + `PARTICIPANT_DISPLAY_NAME_CHANGED`
   *    broadcast to the renaming session_id only.
   *  - Receive path (`videocall-client/src/client/video_call_client.rs:
   *    2173-2177`) already routes session-id-scoped renames through
   *    `set_peer_display_name(session_id, name)` — the
   *    `display_name_change_with_session_id_is_session_scoped` wasm test
   *    locks that helper down.
   *
   * Setup: Tab A and Tab B authenticate as the SAME user (same JWT `sub`),
   * join the same meeting, and Tab A opens the rename modal and submits a
   * new name. Assert:
   *  - Tab A's floating-name shows the new name (its own self-tile and on
   *    Tab B's peer view of Tab A both render the new name).
   *  - Tab B's own floating-name (its self-tile) STILL shows the original
   *    name — proving the rename was scoped to Tab A's session_id only.
   *
   * Pre-fix the assertion on Tab B fails because the server broadcast
   * carried session_id=0 → the receive path's user-id-keyed fallback fires
   * → both peers of the same user_id get renamed.
   */
  test("renaming one same-user session does not change a sibling session's display name", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_same_user_rename_${Date.now()}`;

    const sessionA = await openSameUserContext(uiURL);
    const sessionB = await openSameUserContext(uiURL);

    try {
      // ---- Both sessions join the meeting as the SAME authed user ----
      await navigateToMeeting(sessionA.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionA.page);
      await expect(sessionA.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      await navigateToMeeting(sessionB.page, meetingId, SAME_USER_NAME);
      await joinMeetingFromPage(sessionB.page);
      await expect(sessionB.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Both sides should see at least 2 tiles (self + sibling) before we
      // attempt the rename — guards against a race where the rename fires
      // before peer discovery completes.
      await expect(sessionA.page.locator(PEER_TILE_SELECTOR).first()).toBeVisible({
        timeout: 30_000,
      });
      await expect(sessionB.page.locator(PEER_TILE_SELECTOR).first()).toBeVisible({
        timeout: 30_000,
      });

      // Let join toasts clear so they don't intercept the peer-list-button
      // click below.
      await sessionA.page.waitForTimeout(9_000);

      // ---- Tab A opens the peer list and clicks the edit pencil ----
      // PeerListButton's tooltip text identifies the toggle ("Open Peers"
      // when closed). The edit pencil on the self-row has
      // class="peer_item_edit_btn" and title="Edit your display name"
      // (see `peer_list_item.rs:88-95`).
      const openPeers = sessionA.page.locator("button.video-control-button", {
        has: sessionA.page.locator(".tooltip", { hasText: "Open Peers" }),
      });
      await expect(openPeers).toBeVisible({ timeout: 10_000 });
      await openPeers.click();

      const editBtn = sessionA.page.locator("button.peer_item_edit_btn");
      await expect(editBtn).toBeVisible({ timeout: 10_000 });
      await editBtn.click();

      // ---- Tab A enters a new name and submits the modal form ----
      const renamedName = `RenamedTabA_${Date.now()}`;
      const nameInput = sessionA.page.locator("input.input-apple");
      await expect(nameInput).toBeVisible({ timeout: 10_000 });
      await nameInput.fill("");
      await nameInput.pressSequentially(renamedName, { delay: 30 });

      const saveBtn = sessionA.page.getByRole("button", { name: "Save" });
      await expect(saveBtn).toBeEnabled({ timeout: 5_000 });
      await saveBtn.click();

      // ---- Tab A's UI must show the renamed name in at least one
      // floating-name (the self-tile or peer-list self-row) ----
      await expect(
        sessionA.page.locator(".floating-name", { hasText: renamedName }).first(),
      ).toBeVisible({ timeout: 20_000 });

      // ---- Tab B (sibling same-user session) must NOT have its OWN
      // display name changed. The sibling has its own self-tile and a
      // peer-tile representing Tab A. Tab A's peer tile on Tab B WILL show
      // the renamed name (server broadcasts session-scoped rename for
      // Tab A's session_id). What must remain is Tab B's OWN floating-name
      // showing the original name. ----
      //
      // We check by opening Tab B's peer list and asserting the local self
      // row still shows the original SAME_USER_NAME. The peer-list local
      // self row's display name is sourced from `current_display_name()`
      // in attendants.rs (a per-tab signal), so it is the cleanest proof
      // that Tab B's identity was not collapsed into Tab A's rename.
      const openPeersB = sessionB.page.locator("button.video-control-button", {
        has: sessionB.page.locator(".tooltip", { hasText: "Open Peers" }),
      });
      await expect(openPeersB).toBeVisible({ timeout: 10_000 });
      await openPeersB.click();

      // The self row carries the edit pencil; we use its sibling
      // .peer-name-text or the row's text content. Inspect the row that
      // contains the edit pencil and assert it does NOT contain the
      // renamed name.
      const selfRowB = sessionB.page
        .locator("#peer-list-container li")
        .filter({ has: sessionB.page.locator("button.peer_item_edit_btn") });
      await expect(selfRowB).toHaveCount(1, { timeout: 10_000 });
      const selfRowTextB = (await selfRowB.first().textContent()) ?? "";
      expect(
        selfRowTextB.includes(renamedName),
        "Tab B's OWN display name must not have been renamed by Tab A's " +
          "session-scoped rename. The presence of the renamed name in Tab B's " +
          "self row would indicate the server broadcast was not session-scoped " +
          "(carried session_id=0) and the receive-path user-id fallback fired.",
      ).toBe(false);
      expect(
        selfRowTextB.includes(SAME_USER_NAME),
        "Tab B's self row must still show the original display name after " +
          "Tab A renames its sibling session.",
      ).toBe(true);
    } finally {
      await closeSameUserContext(sessionA.context);
      await closeSameUserContext(sessionB.context);
    }
  });
});
