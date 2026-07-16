import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Screen-share panel decode-budget render-wiring E2E coverage (issue #1471,
 * a PR #1467 follow-up).
 *
 * The pure helpers the screen-share (SS) panel calls — `partition_camera_tiles`,
 * `expand_decoded_for_requested`, `promote_requested_into_decoded` — are
 * host-unit-tested in `decode_budget.rs`. What was UNCOVERED is the SS RENDER
 * WIRING: that the SS right panel actually routes camera-OFF peers into the
 * plain-avatar group (`ss_camera_off_tiles`) and that the per-tile PLAY button
 * (#1466) force-decodes a budget-shed SS tile into live video. This spec closes
 * both gaps end-to-end.
 *
 * Why this is distinct from `screen-share-panel.spec.ts`:
 *   - That file's "off-budget camera-on peers render as paused avatars during
 *     screen share" test covers the camera-ON shed tier (`.placeholder-content
 *     --paused`). It does NOT cover (a) a camera-OFF peer rendering as a PLAIN
 *     avatar in the SS panel (the `ss_camera_off_tiles` partition), nor (b) the
 *     PLAY force-decode affordance inside the SS panel.
 *
 * Why real browser peers (not `addMockPeers`):
 *   Mock peers are video-OFF (`is_video_enabled_for_peer` is false for "mock-N"
 *   ids) AND their ids do not parse as a u64 session_id, so the force-decode
 *   merge silently skips them — a mock can reproduce NEITHER a camera-off real
 *   peer NOR a PLAY-promotable shed tile. Both require genuine remote Chromium
 *   peers whose camera state is driven by the prejoin `vc_prejoin_camera_on`
 *   flag (the proven pattern from `decode-budget-camera-off.spec.ts` and
 *   `decode-budget-play-button.spec.ts`).
 *
 * DOM contract (canvas_generator.rs + the SS panel render in attendants.rs):
 *   - SS peer tiles are `.split-peer-tile`.
 *   - A budget-shed CAMERA-ON tile shows `.placeholder-content--paused` (text
 *     "Video paused") and the `[data-testid="decode-play-btn"]` PLAY button.
 *   - A CAMERA-OFF tile shows the plain `.placeholder-content` ("Video
 *     Disabled"), NO `--paused` placeholder, and NO PLAY button.
 *   - A decoded tile has a live `<canvas>`.
 *   - A tile's display name is in its `<h4 class="floating-name">`.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

// Same flags as screen-share-panel.spec.ts: fake media so guests publish a
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
  // Auto-accept getDisplayMedia() so the sharer's stream starts headlessly.
  "--auto-select-desktop-capture-source=Entire screen",
];

// Deterministic getDisplayMedia() shim: some headless environments do not honor
// the desktop-capture flag, so install a synthetic screen stream too (mirrors
// screen-share-panel.spec.ts). Either path produces a real outgoing SS track.
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

/**
 * Click the screen-share button on `sharerPage`; resolve true once the split
 * layout appears on `viewerPage`. Mirrors screen-share-panel.spec.ts.
 */
async function startScreenShare(sharerPage: Page, viewerPage: Page): Promise<boolean> {
  await wakeControls(sharerPage);
  await sharerPage.waitForTimeout(300);
  const shareButton = sharerPage.locator("button.video-control-button", {
    has: sharerPage.locator(".tooltip", { hasText: "Share Screen" }),
  });
  await expect(shareButton).toBeVisible({ timeout: 10_000 });
  await shareButton.click();
  try {
    await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({ timeout: 15_000 });
    return true;
  } catch {
    return false;
  }
}

// SS-panel tile locators (canvas_generator.rs DOM contract).
const ssPeerTiles = (page: Page) => page.locator(".split-peer-tile");
const ssTileByName = (page: Page, name: string) =>
  page.locator(".split-peer-tile", {
    has: page.locator(`h4.floating-name:has-text("${name}")`),
  });
const pausedPlaceholderIn = (tile: ReturnType<Page["locator"]>) =>
  tile.locator(".placeholder-content--paused");
const playButtonIn = (tile: ReturnType<Page["locator"]>) =>
  tile.locator('[data-testid="decode-play-btn"]');

test.describe("Screen-share panel decode-budget wiring (#1471)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ──────────────────────────────────────────────────────────────────────
  // (a) A camera-OFF peer renders as a PLAIN avatar in the SS right panel.
  //
  // The SS partition routes camera-OFF real peers into `ss_camera_off_tiles`
  // (plain avatars, never dashed, never PLAY-able) — the SS analog of the
  // normal-grid #1465 partition. A camera-OFF SS tile must therefore show the
  // plain "Video Disabled" placeholder, NO `--paused` placeholder, and NO PLAY
  // button. If the SS partition wiring regressed (camera-off folded into the
  // budgeted/avatar group), the camera-off tile would gain a `--paused`
  // placeholder or PLAY button and this test fails.
  // ──────────────────────────────────────────────────────────────────────
  test("camera-off peer renders as a plain avatar in the screen-share panel", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `ss_decode_budget_camoff_${Date.now()}`;

    const browserHost = await chromium.launch({ args: BROWSER_ARGS });
    const browserOn = await chromium.launch({ args: BROWSER_ARGS });
    const browserOff = await chromium.launch({ args: BROWSER_ARGS });

    // Host is the VIEWER (the `.split-screen-tile` split layout renders for the
    // viewer of a REMOTE share, not the local sharer — so a guest must share).
    const hostCtx = await createAuthenticatedContext(
      browserHost,
      "ssdbhost@videocall.rs",
      "SSDBHost",
      uiURL,
    );

    // The camera-ON guest is the SHARER: it publishes video AND shares its
    // screen, so it carries the getDisplayMedia shim.
    const onCtx = await createAuthenticatedContext(
      browserOn,
      "ssdbon@videocall.rs",
      "SSDBOn",
      uiURL,
    );
    await onCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
    await onCtx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);
    // The camera-OFF guest is the partition case under test.
    const offCtx = await createAuthenticatedContext(
      browserOff,
      "ssdboff@videocall.rs",
      "SSDBOff",
      uiURL,
    );
    await offCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "false");`);

    const hostPage = await hostCtx.newPage();
    const onPage = await onCtx.newPage();
    const offPage = await offCtx.newPage();

    try {
      await navigateToMeeting(hostPage, meetingId, "SSDBHost");
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");

      await navigateToMeeting(onPage, meetingId, "SSDBOn");
      const onResult = await joinMeetingFromPage(onPage);
      await admitGuestIfNeeded(hostPage, onPage, onResult);

      await navigateToMeeting(offPage, meetingId, "SSDBOff");
      const offResult = await joinMeetingFromPage(offPage);
      await admitGuestIfNeeded(hostPage, offPage, offResult);

      // Both remote peer tiles present on the host before sharing.
      const gridTiles = hostPage.locator("#grid-container .grid-item");
      await expect(gridTiles.nth(1)).toBeVisible({ timeout: 45_000 });

      // The camera-ON guest shares → the HOST's view switches to split layout.
      const shareActivated = await startScreenShare(onPage, hostPage);
      if (!shareActivated) {
        test.skip(true, "Screen share could not be auto-accepted in this environment.");
        return;
      }
      // Let the split layout + partition settle.
      await hostPage.waitForTimeout(5000);

      // Both remote peers appear in the SS right panel.
      await expect(ssPeerTiles(hostPage).nth(1)).toBeVisible({ timeout: 30_000 });

      // ---- ASSERT: the camera-OFF peer is a PLAIN avatar in the SS panel ----
      const offTile = ssTileByName(hostPage, "SSDBOff");
      await expect(offTile).toBeVisible({ timeout: 15_000 });
      // Plain camera-off placeholder, NOT the camera-on "Video paused" shed arm.
      await expect(offTile.locator(".placeholder-text")).toHaveText("Video Disabled", {
        timeout: 15_000,
      });
      // Camera-off tiles are never the recoverable paused tier: no --paused
      // placeholder and no PLAY affordance.
      await expect(pausedPlaceholderIn(offTile)).toHaveCount(0);
      await expect(playButtonIn(offTile)).toHaveCount(0);
      // It is an avatar, not a decoded canvas.
      await expect(offTile.locator("canvas")).toHaveCount(0);
    } finally {
      await browserHost.close();
      await browserOn.close();
      await browserOff.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // (b) PLAY on a budget-shed camera-ON tile in the SS panel force-decodes it.
  //
  // The SS panel renders shed camera-ON peers via the SAME `PeerTile` with
  // `force_avatar: true`, so the #1466 PLAY button appears there. Clicking it
  // adds the peer's session_id to `user_requested_decode`; the SS expand +
  // `promote_requested_into_decoded` path then admits it into the decoded
  // window, so its tile gains a live `<canvas>` and loses the paused
  // placeholder. This is the SS force-decode wiring #1471 asks to exercise.
  //
  // Fixed(2) with 3 camera-ON peers guarantees ≥1 shed (paused) SS tile.
  // ──────────────────────────────────────────────────────────────────────
  // Issue #1530: headless Chrome with SwiftShader (software GPU) cannot reliably
  // trigger decode-budget shedding with 3+ simultaneous camera-on peers — the
  // software decode path is too slow to produce enough load. Re-enable once a
  // GPU-equipped CI runner is available.
  test.fixme("PLAY on a paused screen-share tile force-decodes that peer", async ({ baseURL }) => {
    test.setTimeout(200_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `ss_decode_budget_play_${Date.now()}`;
    const FORCED_BUDGET = 2;

    const browserHost = await chromium.launch({ args: BROWSER_ARGS });
    const browserOn1 = await chromium.launch({ args: BROWSER_ARGS });
    const browserOn2 = await chromium.launch({ args: BROWSER_ARGS });
    const browserOn3 = await chromium.launch({ args: BROWSER_ARGS });

    // Host is the Fixed(2) VIEWER (the split layout renders for the viewer of a
    // REMOTE share, so a guest shares). Fixed(2) makes the host's SS panel shed
    // at least one of the 3 camera-on peers into the paused tier.
    const hostCtx = await createAuthenticatedContext(
      browserHost,
      "ssplayhost@videocall.rs",
      "SSPlayHost",
      uiURL,
    );
    await hostCtx.addInitScript(
      `localStorage.setItem("vc_decode_budget_override", "${FORCED_BUDGET}");`,
    );

    const mkOn = async (
      browser: typeof browserOn1,
      email: string,
      name: string,
      sharer = false,
    ): Promise<BrowserContext> => {
      const ctx = await createAuthenticatedContext(browser, email, name, uiURL);
      await ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      // The sharer guest carries the getDisplayMedia shim.
      if (sharer) {
        await ctx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);
      }
      return ctx;
    };
    // on1 is the sharer (camera-on so it also publishes video).
    const on1Ctx = await mkOn(browserOn1, "ssplay1@videocall.rs", "SSPlay1", true);
    const on2Ctx = await mkOn(browserOn2, "ssplay2@videocall.rs", "SSPlay2");
    const on3Ctx = await mkOn(browserOn3, "ssplay3@videocall.rs", "SSPlay3");

    const hostPage = await hostCtx.newPage();
    const on1Page = await on1Ctx.newPage();
    const on2Page = await on2Ctx.newPage();
    const on3Page = await on3Ctx.newPage();

    try {
      await navigateToMeeting(hostPage, meetingId, "SSPlayHost");
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");

      await navigateToMeeting(on1Page, meetingId, "SSPlay1");
      await admitGuestIfNeeded(hostPage, on1Page, await joinMeetingFromPage(on1Page));
      await navigateToMeeting(on2Page, meetingId, "SSPlay2");
      await admitGuestIfNeeded(hostPage, on2Page, await joinMeetingFromPage(on2Page));
      await navigateToMeeting(on3Page, meetingId, "SSPlay3");
      await admitGuestIfNeeded(hostPage, on3Page, await joinMeetingFromPage(on3Page));

      // Publication gate (load-bearing): the SS panel only sheds into the paused
      // tier when ss_all has MORE camera-on peers than ss_base_budget (Fixed(2)).
      // If a guest hasn't published video yet, ss_all has < 3 camera-on peers,
      // Fixed(2) covers them all, and nothing is shed. Before sharing, assert the
      // SAME shed in the host's NORMAL grid: Fixed(2) + 3 camera-on peers sheds
      // exactly one into a dashed `.off-budget-tile`. That tile's presence proves
      // all 3 guests published AND the budget is actively shedding — the exact
      // precondition the SS panel inherits when the layout switches.
      const hostGridTiles = hostPage.locator("#grid-container .grid-item");
      await expect(hostGridTiles.nth(2)).toBeVisible({ timeout: 60_000 });
      const normalGridShed = hostPage.locator("#grid-container .grid-item.off-budget-tile");
      await expect(normalGridShed.first()).toBeVisible({ timeout: 45_000 });

      // The camera-ON guest on1 shares → the HOST's view switches to split.
      const shareActivated = await startScreenShare(on1Page, hostPage);
      if (!shareActivated) {
        test.skip(true, "Screen share could not be auto-accepted in this environment.");
        return;
      }
      await hostPage.waitForTimeout(5000);

      // All 3 peers in the SS panel; Fixed(2) sheds ≥1 into the paused tier.
      await expect(ssPeerTiles(hostPage).nth(2)).toBeVisible({ timeout: 30_000 });
      const pausedTiles = hostPage.locator(".split-peer-tile .placeholder-content--paused");
      await expect(pausedTiles.first()).toBeVisible({ timeout: 30_000 });

      // The shed SS tile carries the PLAY button. Locate it via the tile that
      // contains the paused placeholder.
      const pausedTile = ssPeerTiles(hostPage)
        .filter({ has: hostPage.locator(".placeholder-content--paused") })
        .first();
      await expect(pausedTile.locator(".placeholder-text")).toHaveText("Video paused", {
        timeout: 15_000,
      });
      const playBtn = playButtonIn(pausedTile);
      await expect(playBtn).toBeVisible({ timeout: 15_000 });
      await expect(playBtn).toHaveAttribute("aria-label", /^Play .+'s video$/);
      await expect(pausedTile.locator("canvas")).toHaveCount(0);

      // Capture the shed peer's name so we assert on the SAME tile post-promote.
      const shedName = (await pausedTile.locator("h4.floating-name").first().innerText())
        .trim()
        .split("\n")[0];

      // ---- ACT: click PLAY in the SS panel ----
      await playBtn.click();

      // ---- ASSERT: that SS tile is force-decoded (live canvas, no --paused) ----
      const promotedTile = ssTileByName(hostPage, shedName);
      await expect(promotedTile.locator("canvas")).toHaveCount(1, { timeout: 45_000 });
      await expect(promotedTile.locator("canvas")).toBeVisible({ timeout: 15_000 });
      await expect(pausedPlaceholderIn(promotedTile)).toHaveCount(0, { timeout: 15_000 });
    } finally {
      await browserHost.close();
      await browserOn1.close();
      await browserOn2.close();
      await browserOn3.close();
    }
  });
});
