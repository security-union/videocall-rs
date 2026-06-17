import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Per-tile decode-budget PLAY button E2E coverage (issue #1466).
 *
 * BEHAVIOR UNDER TEST (issue #1466):
 *   A decode-budget-PAUSED tile — `force_avatar == true` AND the peer's camera
 *   is ON (`is_video_enabled_for_peer == true`), i.e. the tile the budget shed,
 *   rendered with the dashed `.off-budget-tile` outline, the "Video paused"
 *   label and the `.placeholder-content--paused` placeholder — now exposes an
 *   interactive PLAY <button>:
 *     - `data-testid="decode-play-btn"`, class `decode-play-overlay`,
 *       `aria-label="Play <name>'s video"`.
 *   Clicking it force-decodes THAT one peer (its session_id is added to the
 *   `user_requested_decode` set → `merge_user_requested_decode` folds it into
 *   `active_decode_set` → `peer.visible = true` → frames decode), so the tile
 *   gains a live `<canvas>` and loses the paused placeholder.
 *
 *   NEGATIVE: a camera-OFF tile ("Video Disabled" / "Camera Off", which after
 *   #1465 is NOT dashed and lands in the plain placeholder arm) must NOT expose
 *   the PLAY button — `paused_by_device = force_avatar && is_video_enabled_for_peer`
 *   is false for a camera-off peer, so the PLAY affordance is unreachable there.
 *
 * WHY THIS NEEDS REAL BROWSER CONTEXTS (not mock peers):
 *   1. The PLAY button only renders when `is_video_enabled_for_peer` is true.
 *      Mock peers have a NON-numeric session_id (`mock-<i>`), for which
 *      `is_video_enabled_for_peer` is FALSE — a mock budget-shed tile renders
 *      the plain "Video Disabled" arm with NO PLAY button. A mock therefore
 *      cannot reproduce a recoverable "paused" tile.
 *   2. The force-decode merge parses the session_id as u64
 *      (`merge_user_requested_decode`); a `mock-<i>` id fails that parse and is
 *      silently skipped, so a PLAY click on a mock tile could never promote it.
 *   Reproducing #1466 deterministically therefore requires genuine remote peers
 *   that join with their camera ON and have real numeric session_ids — the
 *   proven multi-context pattern from `screen-share-panel.spec.ts` /
 *   `decode-budget-camera-off.spec.ts`. We force a hard Fixed cap on the host so
 *   the budget deterministically sheds a camera-ON tile into the paused tier.
 *
 * DOM contract (canvas_generator.rs):
 *   - Every peer tile is `#grid-container .grid-item`, with `data-off-budget`
 *     "true" (budget-shed, dashed `.off-budget-tile`) or "false".
 *   - A budget-shed camera-ON tile renders the
 *     `.placeholder-content--paused` placeholder with text "Video paused" and
 *     the `[data-testid="decode-play-btn"]` PLAY button.
 *   - A decoded tile has a live `<canvas>` and no `.placeholder-content--paused`.
 *   - A tile's display name is rendered in its `<h4 class="floating-name">`.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

// Same flags as decode-budget-camera-off.spec.ts: fake media so guests can
// publish a synthetic camera without hardware; QUIC origin for WebTransport;
// single renderer process per context to keep CI memory bounded.
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
// The interactive per-tile PLAY button (#1466), scoped to a single tile.
const playButtonIn = (tile: ReturnType<Page["locator"]>) =>
  tile.locator('[data-testid="decode-play-btn"]');
// A decode-budget-paused placeholder (#1466 "Video paused" tier).
const pausedPlaceholderIn = (tile: ReturnType<Page["locator"]>) =>
  tile.locator(".placeholder-content--paused");
// Locate a specific peer's grid tile by its rendered display name.
const tileByName = (page: Page, name: string) =>
  page.locator("#grid-container .grid-item", {
    has: page.locator(`h4.floating-name:has-text("${name}")`),
  });

test.describe("Decode-budget per-tile PLAY button (#1466)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ──────────────────────────────────────────────────────────────────────
  // A camera-ON peer that the budget shed renders a paused tile with a PLAY
  // button; clicking it force-decodes THAT peer (its tile gains a live canvas
  // and loses the paused placeholder). A camera-OFF peer has NO PLAY button.
  //
  // Setup: host on a hard Fixed(2) cap with 3 camera-ON guests. The cap sheds
  // exactly one camera-ON tile into the dashed "Video paused" tier — that is
  // the tile under test. (A 4th camera-OFF guest provides the negative case:
  // post-#1465 it is partitioned out, plain avatar, no PLAY button.)
  // ──────────────────────────────────────────────────────────────────────
  test("paused camera-on tile exposes a PLAY button that force-decodes that peer", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `decode_budget_play_${Date.now()}`;

    // Fixed(2): with 3 camera-ON remote peers, the cap sheds exactly one into
    // the paused (PLAY-button) tier. The shed tile is the one the PLAY button
    // must recover.
    const FORCED_BUDGET = 2;

    const browserHost = await chromium.launch({ args: BROWSER_ARGS });
    const browserOn1 = await chromium.launch({ args: BROWSER_ARGS });
    const browserOn2 = await chromium.launch({ args: BROWSER_ARGS });
    const browserOn3 = await chromium.launch({ args: BROWSER_ARGS });
    const browserOff = await chromium.launch({ args: BROWSER_ARGS });

    const hostCtx = await createAuthenticatedContext(
      browserHost,
      "playhost@videocall.rs",
      "PlayHost",
      uiURL,
    );
    // Pin the host's decode budget to a fixed cap BEFORE navigation so the
    // shed is deterministic (not auto-adaptation-timing dependent).
    await hostCtx.addInitScript(
      `localStorage.setItem("vc_decode_budget_override", "${FORCED_BUDGET}");`,
    );

    // Three camera-ON guests: prejoin camera flag true → they publish video →
    // is_video_enabled_for_peer true → they feed the budget and (when shed)
    // become recoverable "paused" tiles with a PLAY button.
    const on1Ctx = await createAuthenticatedContext(
      browserOn1,
      "playon1@videocall.rs",
      "PlayOn1",
      uiURL,
    );
    await on1Ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
    const on2Ctx = await createAuthenticatedContext(
      browserOn2,
      "playon2@videocall.rs",
      "PlayOn2",
      uiURL,
    );
    await on2Ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
    const on3Ctx = await createAuthenticatedContext(
      browserOn3,
      "playon3@videocall.rs",
      "PlayOn3",
      uiURL,
    );
    await on3Ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

    // One camera-OFF guest for the negative assertion: it never publishes
    // video, is partitioned out by #1465, and must NOT show a PLAY button.
    const offCtx = await createAuthenticatedContext(
      browserOff,
      "playoff@videocall.rs",
      "PlayOff",
      uiURL,
    );
    await offCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "false");`);

    const hostPage = await hostCtx.newPage();
    const on1Page = await on1Ctx.newPage();
    const on2Page = await on2Ctx.newPage();
    const on3Page = await on3Ctx.newPage();
    const offPage = await offCtx.newPage();

    try {
      // Host joins first, then the camera-ON guests, then the camera-OFF guest.
      await navigateToMeeting(hostPage, meetingId, "PlayHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(on1Page, meetingId, "PlayOn1");
      const on1Result = await joinMeetingFromPage(on1Page);
      await admitGuestIfNeeded(hostPage, on1Page, on1Result);

      await navigateToMeeting(on2Page, meetingId, "PlayOn2");
      const on2Result = await joinMeetingFromPage(on2Page);
      await admitGuestIfNeeded(hostPage, on2Page, on2Result);

      await navigateToMeeting(on3Page, meetingId, "PlayOn3");
      const on3Result = await joinMeetingFromPage(on3Page);
      await admitGuestIfNeeded(hostPage, on3Page, on3Result);

      await navigateToMeeting(offPage, meetingId, "PlayOff");
      const offResult = await joinMeetingFromPage(offPage);
      await admitGuestIfNeeded(hostPage, offPage, offResult);

      // Wait for all 4 remote peer tiles to appear on the host.
      const gridTiles = hostPage.locator("#grid-container .grid-item");
      await expect(gridTiles.nth(3)).toBeVisible({ timeout: 60_000 });

      // Precondition: with a Fixed(2) cap and 3 camera-ON peers, the budget
      // must shed at least one camera-ON tile into the dashed paused tier.
      await expect(offBudgetTiles(hostPage)).not.toHaveCount(0, { timeout: 45_000 });

      // The shed tile carries the #1466 paused placeholder + PLAY button. Pick
      // the first dashed off-budget tile (a camera-ON peer the budget shed —
      // camera-off peers are NOT dashed after #1465, so a dashed tile is always
      // a recoverable paused tile).
      const pausedTile = offBudgetTiles(hostPage).first();
      await expect(pausedPlaceholderIn(pausedTile)).toBeVisible({ timeout: 15_000 });
      await expect(pausedTile.locator(".placeholder-text")).toHaveText("Video paused", {
        timeout: 15_000,
      });
      const playBtn = playButtonIn(pausedTile);
      await expect(playBtn).toBeVisible({ timeout: 15_000 });
      // aria-label is "Play <name>'s video" — assert the shape without pinning
      // which of the 3 camera-ON guests happened to be the shed one.
      await expect(playBtn).toHaveAttribute("aria-label", /^Play .+'s video$/);
      // No live canvas yet — the tile is paused.
      await expect(pausedTile.locator("canvas")).toHaveCount(0);

      // ---- NEGATIVE: the camera-OFF peer has NO PLAY button ----
      const offTile = tileByName(hostPage, "PlayOff");
      await expect(offTile).toBeVisible({ timeout: 15_000 });
      await expect(offTile).not.toHaveClass(/off-budget-tile/);
      await expect(playButtonIn(offTile)).toHaveCount(0);
      await expect(pausedPlaceholderIn(offTile)).toHaveCount(0);

      // ---- ACT: click PLAY on the paused camera-ON tile ----
      // Capture the shed peer's display name BEFORE the click so we can assert
      // on the SAME tile after the promotion re-renders it.
      const shedName = await pausedTile.locator("h4.floating-name").first().innerText();
      await playBtn.click();

      // ---- ASSERT: that peer is force-decoded ----
      // The click adds the peer's session_id to user_requested_decode, the
      // parent re-renders, the partition + merge promote it into
      // active_decode_set, peer.visible becomes true and its frames decode. On
      // the next render `force_avatar` is false for the promoted tile, so the
      // paused placeholder is gone and a live <canvas> appears.
      const promotedTile = tileByName(hostPage, shedName.trim().split("\n")[0]);
      await expect(promotedTile.locator("canvas")).toHaveCount(1, { timeout: 45_000 });
      await expect(promotedTile.locator("canvas")).toBeVisible({ timeout: 15_000 });
      // The paused placeholder is gone for that tile, and it is no longer
      // dashed off-budget.
      await expect(pausedPlaceholderIn(promotedTile)).toHaveCount(0, { timeout: 15_000 });
      await expect(promotedTile).not.toHaveClass(/off-budget-tile/);
    } finally {
      await browserHost.close();
      await browserOn1.close();
      await browserOn2.close();
      await browserOn3.close();
      await browserOff.close();
    }
  });
});
