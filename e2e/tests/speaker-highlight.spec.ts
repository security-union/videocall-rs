import path from "node:path";
import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Speaking-glow E2E tests.
 *
 * The speaking glow is rendered as:
 * - `.speaking-tile` CSS class on the outer tile div (peer: `.grid-item`,
 *    host self-view: `#host-controls-nav`)
 * - Inline `box-shadow` + `transition` via `speak_style()` on the same element
 *
 * Most tests still exercise the silent baseline. Two targeted regressions now
 * use richer fixtures:
 * - a real WAV fixture for fake microphone input so remote speaking can be observed
 * - a synthetic `getDisplayMedia()` shim so host screen share can start without
 *   the native picker
 *
 * Pinned-peer suppression still depends on a broader speaking matrix than this
 * file currently drives, so that slice remains documented with structural
 * checks plus Rust-level unit coverage.
 */

const DEFAULT_TILE_BORDER_COLOR = "rgba(100, 100, 100, 0.30)";
const SPEAKING_AUDIO_FIXTURE = path.resolve(__dirname, "../../dioxus-ui/assets/hi.wav");

function browserArgs(fakeAudioFile?: string) {
  if (!fakeAudioFile) {
    return [...BROWSER_ARGS];
  }

  return [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${fakeAudioFile}`];
}

async function installSyntheticDisplayCapture(page: Page) {
  await page.addInitScript(() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) {
      return;
    }

    const createSyntheticDisplayStream = () => {
      const canvas = document.createElement("canvas");
      canvas.width = 1280;
      canvas.height = 720;

      const context = canvas.getContext("2d");
      if (!context) {
        throw new Error("2D canvas context unavailable for synthetic display capture");
      }

      let frame = 0;
      const paint = () => {
        frame += 1;
        context.fillStyle = "#0b1220";
        context.fillRect(0, 0, canvas.width, canvas.height);

        context.fillStyle = "#4fd1c5";
        context.fillRect(80 + (frame % 320), 120, 280, 160);

        context.fillStyle = "#f8fafc";
        context.font = "bold 64px sans-serif";
        context.fillText("Synthetic screen share", 80, 420);

        context.fillStyle = "#94a3b8";
        context.font = "32px sans-serif";
        context.fillText(`frame ${frame}`, 80, 480);

        requestAnimationFrame(paint);
      };

      paint();
      return canvas.captureStream(12);
    };

    Object.defineProperty(mediaDevices, "getDisplayMedia", {
      configurable: true,
      value: async () => createSyntheticDisplayStream(),
    });
  });
}

async function navigateToMeeting(page: Page, meetingId: string, username: string) {
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

async function ensureMicrophoneEnabled(page: Page) {
  const unmuteButton = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Unmute" }),
  });
  if (await unmuteButton.count()) {
    await unmuteButton.first().click();
    await page.waitForTimeout(1_000);
  }
}

async function muteMicrophone(page: Page) {
  const muteButton = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Mute" }),
  });
  await expect(muteButton.first()).toBeVisible({ timeout: 10_000 });
  await muteButton.first().click();
  await page.waitForTimeout(1_000);
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
 * Set up a two-user meeting (host + guest) and return both pages
 * along with browser handles for cleanup.
 */
async function setupTwoUserMeeting(
  uiURL: string,
  meetingId: string,
  hostName: string,
  guestName: string,
  options?: {
    hostFakeAudioFile?: string;
    guestFakeAudioFile?: string;
    prepareHostPage?: (page: Page) => Promise<void>;
    prepareGuestPage?: (page: Page) => Promise<void>;
  },
) {
  const browser1 = await chromium.launch({
    args: browserArgs(options?.hostFakeAudioFile),
  });
  const browser2 = await chromium.launch({
    args: browserArgs(options?.guestFakeAudioFile),
  });

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

  const hostPage = await hostCtx.newPage();
  const guestPage = await guestCtx.newPage();

  if (options?.prepareHostPage) {
    await options.prepareHostPage(hostPage);
  }
  if (options?.prepareGuestPage) {
    await options.prepareGuestPage(guestPage);
  }

  await navigateToMeeting(hostPage, meetingId, hostName);
  const hostResult = await joinMeetingFromPage(hostPage);
  expect(hostResult).toBe("in-meeting");

  await navigateToMeeting(guestPage, meetingId, guestName);
  const guestResult = await joinMeetingFromPage(guestPage);
  await admitGuestIfNeeded(hostPage, guestPage, guestResult);

  // Wait for peer tile to appear on the host side
  const peerTile = hostPage.locator("#grid-container .grid-item");
  await expect(peerTile.first()).toBeVisible({ timeout: 30_000 });

  return { hostPage, guestPage, browser1, browser2 };
}

async function waitForTileToSpeak(page: Page) {
  // Works in both grid layout (.grid-item) and split layout (.split-peer-tile)
  const peerTile = page.locator(".split-peer-tile, #grid-container .grid-item").first();
  await expect(peerTile).toBeVisible({ timeout: 30_000 });

  await expect
    .poll(
      async () => {
        const className = (await peerTile.getAttribute("class")) || "";
        const style = (await peerTile.getAttribute("style")) || "";
        const hasExplicitGlow = style.includes("box-shadow") && !style.includes("box-shadow: none");
        return className.includes("speaking-tile") || hasExplicitGlow;
      },
      {
        timeout: 30_000,
        message: "expected peer tile to enter the speaking-highlight state",
      },
    )
    .toBe(true);

  return peerTile;
}

test.describe("Speaker highlight glow on video tiles", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ──────────────────────────────────────────────────────────────────────
  // 1. Glow on outer tile only — silent state
  // ──────────────────────────────────────────────────────────────────────
  test("peer tile outer div has box-shadow:none and no speaking-tile class when silent", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_peer_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "GlowHost",
      "GlowGuest",
    );

    try {
      // The outer tile div is a .grid-item inside #grid-container.
      // When silent it must NOT have .speaking-tile class and its inline
      // style must contain "box-shadow: none".
      const outerTile = hostPage.locator("#grid-container .grid-item").first();
      await expect(outerTile).toBeVisible({ timeout: 10_000 });

      const tileClass = await outerTile.getAttribute("class");
      expect(tileClass).toBeTruthy();
      expect(tileClass).not.toContain("speaking-tile");

      const tileStyle = await outerTile.getAttribute("style");
      expect(tileStyle).toBeTruthy();
      expect(tileStyle).toContain("box-shadow: none");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("remote participant border resets to the default color after they stop speaking", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_reset_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "ResetHost",
      "ResetGuest",
      {
        guestFakeAudioFile: SPEAKING_AUDIO_FIXTURE,
      },
    );

    try {
      await ensureMicrophoneEnabled(guestPage);

      const peerTile = await waitForTileToSpeak(hostPage);

      await muteMicrophone(guestPage);

      await expect
        .poll(
          async () => ({
            className: (await peerTile.getAttribute("class")) || "",
            style: (await peerTile.getAttribute("style")) || "",
          }),
          {
            timeout: 30_000,
            message: "expected remote peer highlight to clear after speech stops",
          },
        )
        .toMatchObject({
          className: expect.not.stringContaining("speaking-tile"),
          style: expect.stringContaining(`border-color: ${DEFAULT_TILE_BORDER_COLOR}`),
        });

      const clearedStyle = (await peerTile.getAttribute("style")) || "";
      expect(clearedStyle).toContain("box-shadow: none");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 2. Transition property present for smooth glow animation
  // ──────────────────────────────────────────────────────────────────────
  test("peer tile has transition property in inline style for glow animation", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_trans_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "TransHost",
      "TransGuest",
    );

    try {
      const outerTile = hostPage.locator("#grid-container .grid-item").first();
      await expect(outerTile).toBeVisible({ timeout: 10_000 });

      const style = await outerTile.getAttribute("style");
      expect(style).toBeTruthy();
      // speak_style() always emits transition: for both silent and active states
      expect(style).toContain("transition:");
      expect(style).toContain("box-shadow");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 3. Host controls nav (#host-controls-nav) — silent state
  // ──────────────────────────────────────────────────────────────────────
  test("host-controls-nav has class 'host' without speaking-tile when silent", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_hostnav_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "host-nav@videocall.rs",
        "HostNav",
        uiURL,
      );
      const page = await ctx.newPage();

      await navigateToMeeting(page, meetingId, "HostNav");
      const result = await joinMeetingFromPage(page);
      expect(result).toBe("in-meeting");

      const hostNav = page.locator("#host-controls-nav");
      await expect(hostNav).toBeVisible({ timeout: 15_000 });

      // Class should be "host" (no "speaking-tile")
      const navClass = await hostNav.getAttribute("class");
      expect(navClass).toBeTruthy();
      expect(navClass).toContain("host");
      expect(navClass).not.toContain("speaking-tile");
    } finally {
      await browser.close();
    }
  });

  test("host-controls-nav inline style has box-shadow:none when silent", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_hostbox_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "host-box@videocall.rs",
        "HostBox",
        uiURL,
      );
      const page = await ctx.newPage();

      await navigateToMeeting(page, meetingId, "HostBox");
      const result = await joinMeetingFromPage(page);
      expect(result).toBe("in-meeting");

      const hostNav = page.locator("#host-controls-nav");
      await expect(hostNav).toBeVisible({ timeout: 15_000 });

      const style = await hostNav.getAttribute("style");
      expect(style).toBeTruthy();
      expect(style).toContain("box-shadow: none");
      expect(style).toContain("transition:");
    } finally {
      await browser.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 3b. Host controls nav — overflow clipping and video wrapper radius
  //
  // Regression for PR #1844/#1804: outside customize mode the host-controls-nav
  // must clip overflow (hidden) and .host-video-wrapper must have a non-zero
  // border-top-left-radius. The bug removed overflow:hidden and zeroed the
  // radius, causing visual bleed.
  // ──────────────────────────────────────────────────────────────────────
  test("host-controls-nav has overflow hidden and host-video-wrapper has non-zero top-left radius", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_overflow_radius_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "host-overflow@videocall.rs",
        "HostOverflow",
        uiURL,
      );
      const page = await ctx.newPage();

      await navigateToMeeting(page, meetingId, "HostOverflow");
      const result = await joinMeetingFromPage(page);
      expect(result).toBe("in-meeting");

      // Verify #host-controls-nav.host is visible (default non-customize mode)
      const hostNav = page.locator("#host-controls-nav.host");
      await expect(hostNav).toBeVisible({ timeout: 15_000 });

      // Computed overflow must be "hidden" in default (non-customize) mode
      const overflow = await hostNav.evaluate((el) => {
        return window.getComputedStyle(el).overflow;
      });
      expect(overflow).toBe("hidden");

      // .host-video-wrapper must have a non-zero border-top-left-radius
      const videoWrapper = page.locator(".host-video-wrapper");
      await expect(videoWrapper).toBeVisible({ timeout: 10_000 });

      const borderTopLeftRadius = await videoWrapper.evaluate((el) => {
        return window.getComputedStyle(el).borderTopLeftRadius;
      });
      expect(borderTopLeftRadius).not.toBe("0px");
    } finally {
      await browser.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 4. Screen-share tiles — no glow
  //
  // The generic no-screen-share baseline is still covered structurally below.
  // For the host-speaking-while-screen-sharing regression, this file now also
  // includes a synthetic `getDisplayMedia()` path that can drive the split
  // layout without the native OS picker.
  //
  // Screen-share tiles themselves should still never receive speaking glow:
  //   - In the grid layout, screen-share tiles are rendered inside a
  //     separate `.grid-item` div WITHOUT speaking-tile or box-shadow.
  //   - In the split layout (TileMode::ScreenOnly), the screen share
  //     renders inside a `.split-screen-tile` div with NO glow props.
  //   - The Rust source (`canvas_generator.rs`) suppresses glow for the
  //     screen-share canvas while leaving the speaker's participant tile
  //     eligible for highlight.
  //
  // The baseline test below verifies that in a normal (no-screen-share)
  // meeting the grid-item tiles do NOT have stale split-layout artifacts.
  // ──────────────────────────────────────────────────────────────────────
  test("grid-item tiles in a normal meeting have no stale screen-share glow artifacts", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_ss_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSHost",
      "SSGuest",
    );

    try {
      // No .split-screen-tile should exist when nobody is screen-sharing
      const splitScreenTile = hostPage.locator(".split-screen-tile");
      await expect(splitScreenTile).toHaveCount(0);

      // The peer tile should have box-shadow: none (no glow) and no
      // speaking-tile class — confirming normal silent rendering without
      // screen-share-related artifacts.
      const outerTile = hostPage.locator("#grid-container .grid-item").first();
      await expect(outerTile).toBeVisible({ timeout: 10_000 });

      const style = await outerTile.getAttribute("style");
      expect(style).toContain("box-shadow: none");

      const cls = await outerTile.getAttribute("class");
      expect(cls).not.toContain("speaking-tile");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // FIXME(#741): Requires real VAD-triggered speaking state — fake media
  // devices produce no audio so waitForTileToSpeak() never resolves and
  // the glow border is never applied. Unblock: inject a synthetic audio
  // track with non-zero samples, or mock the VAD signal directly in the
  // Dioxus client.
  test.fixme("host remains highlighted for other participants while screen sharing", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_host_ss_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "ScreenHost",
      "ScreenGuest",
      {
        hostFakeAudioFile: SPEAKING_AUDIO_FIXTURE,
        prepareHostPage: installSyntheticDisplayCapture,
      },
    );

    try {
      await ensureMicrophoneEnabled(hostPage);

      const shareBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareBtn).toBeVisible({ timeout: 10_000 });
      await shareBtn.click();

      await expect(guestPage.locator(".split-screen-tile")).toBeVisible({ timeout: 30_000 });
      await expect(guestPage.locator(".screen-share-resize-handle")).toBeVisible({
        timeout: 30_000,
      });

      const hostParticipantTile = await waitForTileToSpeak(guestPage);
      const highlightedStyle = (await hostParticipantTile.getAttribute("style")) || "";

      expect(highlightedStyle).not.toContain(`border-color: ${DEFAULT_TILE_BORDER_COLOR}`);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 5. Pinned suppression (structural verification)
  //
  // LIMITATION: Full pinned-glow suppression testing requires a speaking
  // peer (to see glow suppressed on non-pinned peers). Since fake devices
  // produce no audio, we cannot trigger the speaking state. Instead we
  // verify:
  //   a. Pin button is present on peer tiles
  //   b. Clicking it does not introduce spurious glow on silent tiles
  //   c. The glow-related inline styles remain in the "silent" state
  //
  // The suppression logic itself (`is_speaking_suppressed()`) is tested
  // at the unit level in Rust.
  // ──────────────────────────────────────────────────────────────────────
  test("pin button is present on peer tiles and toggling preserves silent glow state", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_pin_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "PinHost",
      "PinGuest",
    );

    try {
      const outerTile = hostPage.locator("#grid-container .grid-item").first();
      await expect(outerTile).toBeVisible({ timeout: 10_000 });

      // Hover to reveal the pin icon (visible on hover via CSS)
      await outerTile.hover();
      await hostPage.waitForTimeout(500);

      const pinButton = outerTile.locator(".pin-icon");
      // Pin icon should be in the DOM
      const pinCount = await pinButton.count();
      expect(pinCount).toBeGreaterThan(0);

      // Click pin to toggle pinned state
      await pinButton.first().click({ force: true });
      await hostPage.waitForTimeout(500);

      // After pinning: the tile should still have box-shadow: none (silent)
      const styleAfterPin = await outerTile.getAttribute("style");
      expect(styleAfterPin).toContain("box-shadow: none");

      // And no speaking-tile class
      const classAfterPin = await outerTile.getAttribute("class");
      expect(classAfterPin).not.toContain("speaking-tile");

      // Unpin by clicking again
      await outerTile.hover();
      await hostPage.waitForTimeout(300);
      await pinButton.first().click({ force: true });
      await hostPage.waitForTimeout(500);

      // Still silent after unpin
      const styleAfterUnpin = await outerTile.getAttribute("style");
      expect(styleAfterUnpin).toContain("box-shadow: none");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 5b. Pin SURVIVES a reactive re-render of the tile (normal grid) — regression
  //
  // Bug: pinning a tile worked, but the instant the PINNED peer started
  // speaking their tile auto-un-pinned. Root cause is NOT specific to speech:
  // the maximize state was added imperatively as the `grid-item-pinned` class,
  // while the tile root's `class` attribute is reactively managed by Dioxus.
  // ANY change to the reactive class STRING makes Dioxus rewrite the `class`
  // attribute and silently drop the imperatively-added `grid-item-pinned`. The
  // pinned peer speaking is one such trigger (they alone are exempt from glow
  // suppression, so only they toggle their own `speaking-tile`); a peer-count
  // change that flips the `full-bleed` class is another — and, unlike the VAD
  // path, it is deterministic in the local stack.
  //
  // This test drives the deterministic trigger: pin the sole peer (tile is
  // `full-bleed`), then a THIRD participant joins, so the pinned tile loses
  // `full-bleed` — exactly the class-attribute rewrite that un-pinned the tile
  // on speech. The fix makes `grid-item-pinned` part of the reactive class, so
  // it survives.
  //
  // Mutation sensitivity: after the rewrite the tile's class no longer contains
  // `full-bleed` (proving the rewrite happened) but MUST still contain
  // `grid-item-pinned`. On the un-fixed code the same rewrite that removed
  // `full-bleed` also wiped the imperative `grid-item-pinned`, so the pin class
  // would be gone → the assertion fails.
  // ──────────────────────────────────────────────────────────────────────
  test("pinned peer tile stays maximized (grid-item-pinned) across a reactive class rewrite", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_survives_rerender_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "PinRerHost",
      "PinRerGuest",
    );

    let browser3: Awaited<ReturnType<typeof chromium.launch>> | undefined;

    try {
      // Capture the first guest's stable tile id while it is the sole tile, so
      // we can track THIS peer through the grid changes below.
      const guestTile = hostPage.locator("#grid-container .grid-item").first();
      await expect(guestTile).toBeVisible({ timeout: 30_000 });
      const guestTileId = await guestTile.getAttribute("id");
      expect(guestTileId).toBeTruthy();
      const pinnedTile = hostPage.locator(`#${guestTileId}`);

      // Bring in a THIRD participant FIRST (before any pin, so the admit control
      // is not occluded by a maximized tile). The host grid now has two remote
      // tiles, so the first guest's tile is NOT full-bleed.
      browser3 = await chromium.launch({ args: browserArgs() });
      const guest3Ctx = await createAuthenticatedContext(
        browser3,
        "pinrerguest2@videocall.rs",
        "PinRerGuest2",
        uiURL,
      );
      const guest3Page = await guest3Ctx.newPage();
      await navigateToMeeting(guest3Page, meetingId, "PinRerGuest2");
      const guest3Result = await joinMeetingFromPage(guest3Page);
      await admitGuestIfNeeded(hostPage, guest3Page, guest3Result);

      await expect
        .poll(async () => hostPage.locator("#grid-container .grid-item").count(), {
          timeout: 30_000,
          message: "expected the host grid to show two remote tiles after the 3rd peer joined",
        })
        .toBeGreaterThanOrEqual(2);

      // Pin the first guest while it is NOT full-bleed (two peers present).
      await pinnedTile.hover();
      await hostPage.waitForTimeout(400);
      await pinnedTile.locator(".pin-icon").first().click({ force: true });

      await expect
        .poll(async () => (await pinnedTile.getAttribute("class")) || "", {
          timeout: 10_000,
          message: "expected the tile to gain grid-item-pinned after clicking pin",
        })
        .toEqual(expect.stringMatching(/^(?!.*\bfull-bleed\b)(?=.*\bgrid-item-pinned\b).*$/));

      // The third participant LEAVES → the pinned peer becomes the sole tile, so
      // its class string GAINS `full-bleed`: the exact reactive class-attribute
      // rewrite that "the pinned peer speaks" also causes.
      await browser3.close();
      browser3 = undefined;

      await expect
        .poll(async () => hostPage.locator("#grid-container .grid-item").count(), {
          timeout: 30_000,
          message: "expected the host grid to drop back to one remote tile after the 3rd peer left",
        })
        .toBe(1);

      // The reactive rewrite happened (full-bleed is now present) AND the pin
      // survived it. On the un-fixed code, the same rewrite that ADDED
      // full-bleed wiped the imperative grid-item-pinned → assertion fails.
      await expect
        .poll(async () => (await pinnedTile.getAttribute("class")) || "", {
          timeout: 30_000,
          message:
            "expected the pinned tile to keep grid-item-pinned after gaining full-bleed (regression: the class rewrite wiped the pin)",
        })
        .toEqual(expect.stringMatching(/(?=.*\bfull-bleed\b)(?=.*\bgrid-item-pinned\b)/));
    } finally {
      await browser1.close();
      await browser2.close();
      if (browser3) {
        await browser3.close();
      }
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 5c. Pin has a VISIBLE effect during screen share — regression
  //
  // Bug: pinning a side-panel peer during screen share did nothing at all.
  // Root cause: the `.grid-item-pinned` maximize rule is defined EARLIER in
  // style.css than the base `.split-peer-tile` rule, so at equal single-class
  // specificity `.split-peer-tile` won the `position`/size cascade and the
  // maximize never applied. The fix adds a compound-specificity
  // `.split-peer-tile.grid-item-pinned` rule.
  //
  // Mutation sensitivity: the class alone was added even on the old code, so a
  // class check would NOT catch this. We assert the COMPUTED layout — the
  // pinned split tile must become `position: fixed` and fill the viewport.
  // On the old CSS it stays `position: relative` in its small grid cell.
  // ──────────────────────────────────────────────────────────────────────
  test("pinning a side-panel peer during screen share maximizes their tile", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_ss_maximize_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SsPinHost",
      "SsPinGuest",
      {
        prepareHostPage: installSyntheticDisplayCapture,
      },
    );

    try {
      const shareBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareBtn).toBeVisible({ timeout: 10_000 });
      await shareBtn.click();

      // Guest now sees the split layout: shared screen + a side-panel peer tile.
      await expect(guestPage.locator(".split-screen-tile")).toBeVisible({ timeout: 30_000 });
      const peerTile = guestPage.locator(".split-peer-tile").first();
      await expect(peerTile).toBeVisible({ timeout: 30_000 });

      // Sanity: before pinning, the side-panel tile is NOT maximized.
      const positionBefore = await peerTile.evaluate((el) => getComputedStyle(el).position);
      expect(positionBefore).not.toBe("fixed");

      // Pin the side-panel peer.
      await peerTile.hover();
      await guestPage.waitForTimeout(400);
      await peerTile.locator(".pin-icon").first().click({ force: true });

      // The maximize must actually take effect: computed position becomes fixed
      // and the tile fills (near) the whole viewport.
      await expect
        .poll(async () => peerTile.evaluate((el) => getComputedStyle(el).position), {
          timeout: 10_000,
          message:
            "expected the pinned split tile to become position:fixed (regression: CSS override lost to .split-peer-tile)",
        })
        .toBe("fixed");

      const box = await peerTile.boundingBox();
      const viewport = guestPage.viewportSize();
      expect(box).not.toBeNull();
      if (box && viewport) {
        // Fills essentially the full viewport (allow a small margin).
        expect(box.width).toBeGreaterThan(viewport.width * 0.9);
        expect(box.height).toBeGreaterThan(viewport.height * 0.9);
      }
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 5d. Pin the SCREEN tile (LEFT panel) — maximizes the SCREEN, not the camera
  //     — regression for the kind-aware pin fix (PR #1892 review finding).
  //
  // Bug: during a screen share the sharer renders as TWO tiles that share one
  // user_id — the shared SCREEN (`.split-screen-tile`) and their CAMERA
  // (`.split-peer-tile`). Pin identity keyed on user_id ALONE could not tell the
  // two apart, so clicking the SCREEN tile's pin maximized the CAMERA tile
  // (which reactively matched the same user_id and had a higher z-index),
  // covering the screen the viewer actually asked to enlarge — the opposite of
  // the intent.
  //
  // The fix makes pin identity kind-aware (`PinnedTile { user_id, kind }`), so
  // pinning the screen maximizes ONLY the screen. This test clicks the SCREEN
  // tile's pin and asserts the SCREEN becomes position:fixed and fills the
  // viewport WHILE the camera tile stays un-maximized. On the pre-fix code the
  // camera tile is the one that goes fixed, so the camera assertion fails.
  // ──────────────────────────────────────────────────────────────────────
  test("pinning the screen tile during screen share maximizes the SCREEN, not the camera", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_ss_screen_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SsScreenPinHost",
      "SsScreenPinGuest",
      {
        prepareHostPage: installSyntheticDisplayCapture,
      },
    );

    try {
      const shareBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareBtn).toBeVisible({ timeout: 10_000 });
      await shareBtn.click();

      // Guest sees the split layout: shared screen (left) + peer camera (right).
      const screenTile = guestPage.locator(".split-screen-tile");
      const cameraTile = guestPage.locator(".split-peer-tile").first();
      await expect(screenTile).toBeVisible({ timeout: 30_000 });
      await expect(cameraTile).toBeVisible({ timeout: 30_000 });

      // Sanity: before pinning neither tile is maximized.
      expect(await screenTile.evaluate((el) => getComputedStyle(el).position)).not.toBe("fixed");
      expect(await cameraTile.evaluate((el) => getComputedStyle(el).position)).not.toBe("fixed");

      // Pin the SCREEN tile specifically.
      await screenTile.hover();
      await guestPage.waitForTimeout(400);
      await screenTile.locator(".pin-icon").first().click({ force: true });

      // The SCREEN must maximize (position:fixed, fills the viewport)...
      await expect
        .poll(async () => screenTile.evaluate((el) => getComputedStyle(el).position), {
          timeout: 10_000,
          message: "expected the pinned SCREEN tile to become position:fixed",
        })
        .toBe("fixed");

      const screenBox = await screenTile.boundingBox();
      const viewport = guestPage.viewportSize();
      expect(screenBox).not.toBeNull();
      if (screenBox && viewport) {
        expect(screenBox.width).toBeGreaterThan(viewport.width * 0.9);
        expect(screenBox.height).toBeGreaterThan(viewport.height * 0.9);
      }

      // ...and the CAMERA tile must NOT be maximized. This is the regression
      // assertion: on the pre-fix code the camera (same user_id, z-index:100)
      // went position:fixed and covered the screen the viewer pinned.
      expect(await cameraTile.evaluate((el) => getComputedStyle(el).position)).not.toBe("fixed");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 5e. Unpinning the SCREEN tile releases the maximize cleanly — regression
  //     for the stuck-DOM-class toggle loop (PR #1892 review finding, bug #2).
  //
  // The pre-fix screen tile toggled `grid-item-pinned` IMPERATIVELY on the DOM,
  // independent of `pinned_peer_id`. Clearing the pin from another surface left
  // the imperative class behind, so the screen stayed visually maximized while
  // the signal said nothing was pinned. The fix routes the screen tile's
  // maximize through the SAME reactive class as every other tile, so a single
  // source of truth governs it. This test pins the screen then clicks the
  // (now-fullscreen) screen tile's pin again and asserts the maximize is fully
  // released — the tile returns to non-fixed AND no element retains
  // `grid-item-pinned`.
  // ──────────────────────────────────────────────────────────────────────
  test("unpinning the screen tile releases the maximize with no stuck class", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_ss_unpin_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SsUnpinHost",
      "SsUnpinGuest",
      {
        prepareHostPage: installSyntheticDisplayCapture,
      },
    );

    try {
      const shareBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareBtn).toBeVisible({ timeout: 10_000 });
      await shareBtn.click();

      const screenTile = guestPage.locator(".split-screen-tile");
      await expect(screenTile).toBeVisible({ timeout: 30_000 });

      // Pin the screen.
      await screenTile.hover();
      await guestPage.waitForTimeout(400);
      await screenTile.locator(".pin-icon").first().click({ force: true });
      await expect
        .poll(async () => screenTile.evaluate((el) => getComputedStyle(el).position), {
          timeout: 10_000,
        })
        .toBe("fixed");

      // Unpin via the same tile's pin (it is topmost while maximized).
      await screenTile.locator(".pin-icon").first().click({ force: true });

      // The maximize must be fully released: back to non-fixed...
      await expect
        .poll(async () => screenTile.evaluate((el) => getComputedStyle(el).position), {
          timeout: 10_000,
          message:
            "expected the screen tile to return to non-fixed after unpin (regression: stuck imperative grid-item-pinned)",
        })
        .not.toBe("fixed");

      // ...and no element anywhere may retain the maximize class.
      await expect(guestPage.locator(".grid-item-pinned")).toHaveCount(0, { timeout: 10_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 5f. Switch the spotlight between a sharer's two tiles — regression for the
  //     kind-aware switch semantics (PR #1892 review finding).
  //
  // With the screen pinned, activating the CAMERA tile's pin must SWITCH the
  // spotlight (screen un-maximizes, camera maximizes) rather than toggle the pin
  // off — the two tile kinds are distinct pin targets. Because a maximized tile
  // fills the viewport and visually covers the other tile's pin button, a real
  // pointer click cannot reach the covered camera pin; this test dispatches the
  // camera pin's onclick directly on the element to exercise the handler → pin
  // reducer → reactive re-render chain end-to-end (the `next_pin_target` switch
  // logic itself is additionally unit-tested with mutation sensitivity).
  // ──────────────────────────────────────────────────────────────────────
  test("switching pin from the screen tile to the camera tile un-maximizes the screen", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_ss_switch_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SsSwitchHost",
      "SsSwitchGuest",
      {
        prepareHostPage: installSyntheticDisplayCapture,
      },
    );

    try {
      const shareBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareBtn).toBeVisible({ timeout: 10_000 });
      await shareBtn.click();

      const screenTile = guestPage.locator(".split-screen-tile");
      const cameraTile = guestPage.locator(".split-peer-tile").first();
      await expect(screenTile).toBeVisible({ timeout: 30_000 });
      await expect(cameraTile).toBeVisible({ timeout: 30_000 });

      // Pin the screen first.
      await screenTile.hover();
      await guestPage.waitForTimeout(400);
      await screenTile.locator(".pin-icon").first().click({ force: true });
      await expect
        .poll(async () => screenTile.evaluate((el) => getComputedStyle(el).position), {
          timeout: 10_000,
        })
        .toBe("fixed");

      // Activate the camera tile's pin (covered by the maximized screen, so fire
      // its click handler directly). This must SWITCH — not toggle off.
      await cameraTile
        .locator(".pin-icon")
        .first()
        .evaluate((el: HTMLElement) => el.click());

      // The camera must now be maximized...
      await expect
        .poll(async () => cameraTile.evaluate((el) => getComputedStyle(el).position), {
          timeout: 10_000,
          message: "expected the camera tile to maximize after switching the pin to it",
        })
        .toBe("fixed");

      // ...and the screen must have released its maximize (switch, not both-on).
      await expect
        .poll(async () => screenTile.evaluate((el) => getComputedStyle(el).position), {
          timeout: 10_000,
          message: "expected the screen tile to un-maximize when the pin switched to the camera",
        })
        .not.toBe("fixed");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 6. Mic icon audio-indicator — no speaking class when silent
  // ──────────────────────────────────────────────────────────────────────
  test("mic icon does not have speaking class when silent", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_mic_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "MicHost",
      "MicGuest",
    );

    try {
      // The audio-indicator div on peer tiles should NOT have "speaking" class
      const audioIndicator = hostPage.locator("#grid-container .audio-indicator").first();
      await expect(audioIndicator).toBeVisible({ timeout: 10_000 });
      await expect(audioIndicator).not.toHaveClass(/speaking/);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 6b. Peer-list mic icon — muted peer must NOT show speaking glow
  //
  // Regression: the speaking flag and muted flag are independent signals.
  // Before the fix, a muted peer whose VAD hadn't cleared would show the
  // mic icon with the "speaking" CSS class (green glow) in the peer list,
  // contradicting the muted state.
  // ──────────────────────────────────────────────────────────────────────
  test("peer-list mic icon does not show speaking class when peer is muted", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_mutemic_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "MuteMicHost",
      "MuteMicGuest",
      {
        guestFakeAudioFile: SPEAKING_AUDIO_FIXTURE,
      },
    );

    try {
      await ensureMicrophoneEnabled(guestPage);

      // Wait for the guest to be detected as speaking on the host side
      await waitForTileToSpeak(hostPage);

      // Open the peer list
      await hostPage.locator(".video-controls-container").hover();
      const peerBtn = hostPage.locator(".video-controls-container button", {
        has: hostPage.locator('.tooltip:has-text("Open Peers")'),
      });
      await peerBtn.first().click();
      await expect(hostPage.locator("#peer-list-container")).toHaveClass(/visible/, {
        timeout: 5_000,
      });

      // The guest's peer_item_mic should have "speaking" class while unmuted
      const guestMicIcon = hostPage.locator(".peer_item_mic").last();
      await expect(guestMicIcon).toBeVisible({ timeout: 5_000 });
      await expect(guestMicIcon).toHaveClass(/speaking/, { timeout: 15_000 });

      // Now mute the guest
      await muteMicrophone(guestPage);

      // The mic icon must lose the "speaking" class once muted
      await expect(guestMicIcon).not.toHaveClass(/speaking/, { timeout: 15_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 7. Host controls nav structural pattern — class + inline style
  //
  // Verifies #host-controls-nav carries both a CSS class and an inline
  // style from speak_style(), so that when speaking IS triggered the glow
  // will render correctly via the `.speaking-tile` CSS rule + box-shadow.
  // ──────────────────────────────────────────────────────────────────────
  test("host-controls-nav has both class and inline style from speak_style", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_hostpat_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "host-pattern@videocall.rs",
        "HostPattern",
        uiURL,
      );
      const page = await ctx.newPage();

      await navigateToMeeting(page, meetingId, "HostPattern");
      const result = await joinMeetingFromPage(page);
      expect(result).toBe("in-meeting");

      const hostNav = page.locator("#host-controls-nav");
      await expect(hostNav).toBeVisible({ timeout: 15_000 });

      // Verify the element has BOTH class and style attributes set:
      //   class = "host" (silent) or "host speaking-tile" (speaking)
      //   style = output of speak_style() — always includes transition + box-shadow
      const cls = await hostNav.getAttribute("class");
      expect(cls).toMatch(/\bhost\b/);

      const style = await hostNav.getAttribute("style");
      expect(style).toBeTruthy();
      // speak_style() always emits these two properties
      expect(style).toContain("transition:");
      expect(style).toContain("box-shadow");

      // Specifically for silent state: ease-out transition for fade-out
      expect(style).toContain("ease-out");
    } finally {
      await browser.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 8. Pinned peer leaves → pin state resets
  //
  // Regression test: when a pinned peer disconnects, the UI must:
  //   a. Remove the grid-item-pinned class from the DOM
  //   b. Keep the host's own glow infrastructure functional so speaking
  //      glow can still trigger for any remaining/future peers
  //
  // The underlying fix clears pinned_peer_id to None when the pinned
  // peer is no longer in the display_peers list, which in turn stops
  // is_speaking_suppressed() from suppressing glow for everyone.
  // ──────────────────────────────────────────────────────────────────────
  test("pinned peer leaving resets pin state and preserves glow infrastructure", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pinleave_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "PinLeaveHost",
      "PinLeaveGuest",
    );

    try {
      // ---- Step 1: Pin the guest's tile on the host side ----
      const outerTile = hostPage.locator("#grid-container .grid-item").first();
      await expect(outerTile).toBeVisible({ timeout: 10_000 });

      // Hover to reveal the pin icon
      await outerTile.hover();
      await hostPage.waitForTimeout(500);

      const pinButton = outerTile.locator(".pin-icon");
      await expect(pinButton.first()).toBeVisible({ timeout: 5_000 });
      await pinButton.first().click({ force: true });
      await hostPage.waitForTimeout(1000);

      // Verify the tile is pinned (has grid-item-pinned class)
      const pinnedTiles = hostPage.locator(".grid-item-pinned");
      await expect(pinnedTiles).toHaveCount(1, { timeout: 5_000 });

      // ---- Step 2: Guest leaves by closing their browser ----
      await browser2.close();

      // ---- Step 3: Wait for the peer tile to disappear ----
      // The host should detect the peer disconnect and remove the tile.
      await expect(hostPage.locator("#grid-container .grid-item")).toHaveCount(0, {
        timeout: 30_000,
      });

      // ---- Step 4: Verify pin state is fully cleared ----
      // No element in the DOM should retain the grid-item-pinned class.
      const stalePinned = hostPage.locator(".grid-item-pinned");
      await expect(stalePinned).toHaveCount(0);

      // ---- Step 5: Host's own glow infrastructure still functional ----
      // #host-controls-nav should still have proper speak_style() output
      // (transition + box-shadow) so that glow can activate if a new peer
      // joins and speaks.
      const hostNav = hostPage.locator("#host-controls-nav");
      await expect(hostNav).toBeVisible({ timeout: 10_000 });

      const navStyle = await hostNav.getAttribute("style");
      expect(navStyle).toBeTruthy();
      expect(navStyle).toContain("box-shadow");
      expect(navStyle).toContain("transition:");

      const navClass = await hostNav.getAttribute("class");
      expect(navClass).toContain("host");
      expect(navClass).not.toContain("speaking-tile");
    } finally {
      // browser2 already closed above; close browser1
      await browser1.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 5e. A pinned camera tile fills the viewport IDENTICALLY with and without
  //     an active screen share — regression (user report on PR #1892).
  //
  // Bug: pinning a peer's camera during a screen share made the video CONTENT
  // render slightly smaller than pinning the same camera in a plain meeting.
  // Root cause (CSS half): `.split-peer-tile` is `box-sizing: border-box` and
  // carries a 2px thumbnail border, so when maximized its `.canvas-container`
  // resolved `100%` against the 4px-smaller CONTENT box (1276x716 on a 1280x720
  // viewport) instead of the full viewport the normal-grid `.grid-item-pinned`
  // reaches (1280x720, its content-box border sits off-screen). The corner
  // radius was never actually mismatched — both contexts already resolve to
  // 10px (`.split-peer-tile .canvas-container`'s literal 10px vs. the normal
  // grid's `calc(--radius-lg - 2px)` = 10px) — so the fix is `border: none` on
  // `.split-peer-tile.grid-item-pinned` only.
  //
  // Mutation sensitivity: the assertion compares the pinned camera's
  // `.canvas-container` box + corner radius in the normal grid vs. under screen
  // share and requires them to MATCH. On the un-fixed CSS the screen-share
  // canvas-container is 4px smaller in each dimension, so the width/height
  // equality assertions fail — that's the mutation-sensitive guard. The
  // `borderRadius` equality passes in both the fixed and un-fixed CSS (it was
  // never the bug); it's asserted here as a forward-looking guard, not a
  // currently-failing check. Reverting `border: none` re-breaks the
  // width/height assertions.
  //
  // NOTE ON RESOLUTION: the user's perceived "smaller" is dominated by a
  // SEPARATE, intentional sender-side behavior — the AdaptiveQuality
  // "screen_share_coordination" step-down that drops the CameraEncoder to a
  // single 480x360 layer while screen sharing (vs 640x480 in a plain meeting).
  // That is an encoder/bandwidth trade-off in videocall-aq / camera_encoder,
  // NOT a UI concern, and is deliberately out of scope for this CSS symmetry
  // regression. This test asserts only the CSS box geometry.
  // ──────────────────────────────────────────────────────────────────────
  test("pinned camera canvas fills the viewport identically with and without screen share", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_ss_symmetry_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SymHost",
      "SymGuest",
      { prepareHostPage: installSyntheticDisplayCapture },
    );

    // Reads the pinned tile's `.canvas-container` bounding box + corner radius
    // from the guest side. `outerSel` selects whichever maximized tile is live.
    const measurePinned = async (outerSel: string) => {
      return guestPage.evaluate((sel) => {
        const outer = document.querySelector(sel) as HTMLElement | null;
        if (!outer) return null;
        const cc = outer.querySelector(".canvas-container") as HTMLElement | null;
        if (!cc) return null;
        const box = cc.getBoundingClientRect();
        return {
          width: box.width,
          height: box.height,
          borderRadius: getComputedStyle(cc).borderTopLeftRadius,
        };
      }, outerSel);
    };

    try {
      // Host camera ON so the guest renders a real letterboxed <canvas> tile
      // (default join is camera-OFF → placeholder, no `.canvas-container` video).
      await hostPage.locator('[data-testid="camera-toggle-button"]').click();
      await guestPage.waitForTimeout(2000);

      // ---- Reference: pin the host camera in the NORMAL grid ----
      const gridTile = guestPage.locator("#grid-container .grid-item").first();
      await expect(gridTile).toBeVisible({ timeout: 30_000 });
      await expect
        .poll(async () => gridTile.locator("canvas").count(), { timeout: 30_000 })
        .toBeGreaterThan(0);
      await gridTile.hover();
      await guestPage.waitForTimeout(400);
      await gridTile.locator(".pin-icon").first().click({ force: true });
      await expect
        .poll(async () => gridTile.evaluate((el) => getComputedStyle(el).position), {
          timeout: 10_000,
        })
        .toBe("fixed");
      await guestPage.waitForTimeout(1500);
      const normal = await measurePinned("#grid-container .grid-item.grid-item-pinned");
      expect(normal, "normal-grid pinned canvas-container should be measurable").not.toBeNull();

      // Unpin before switching layouts.
      await gridTile.hover();
      await guestPage.waitForTimeout(300);
      await gridTile.locator(".pin-icon").first().click({ force: true });
      await guestPage.waitForTimeout(1000);

      // ---- Under test: host screen-shares, pin the side-panel camera ----
      const shareBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareBtn).toBeVisible({ timeout: 10_000 });
      await shareBtn.click();

      await expect(guestPage.locator(".split-screen-tile")).toBeVisible({ timeout: 30_000 });
      const peerTile = guestPage.locator(".split-peer-tile").first();
      await expect(peerTile).toBeVisible({ timeout: 30_000 });
      await peerTile.hover();
      await guestPage.waitForTimeout(400);
      await peerTile.locator(".pin-icon").first().click({ force: true });
      await expect
        .poll(async () => peerTile.evaluate((el) => getComputedStyle(el).position), {
          timeout: 10_000,
        })
        .toBe("fixed");
      await guestPage.waitForTimeout(1500);
      const share = await measurePinned(".split-peer-tile.grid-item-pinned");
      expect(share, "screen-share pinned canvas-container should be measurable").not.toBeNull();

      // The pinned camera must present an IDENTICAL viewport box in both layouts.
      // Allow a sub-pixel tolerance for rounding; the pre-fix delta is 4px.
      if (normal && share) {
        expect(Math.abs(share.width - normal.width)).toBeLessThanOrEqual(0.5);
        expect(Math.abs(share.height - normal.height)).toBeLessThanOrEqual(0.5);
        // ...and the same square corners (pre-fix: 10px vs 0px).
        expect(share.borderRadius).toBe(normal.borderRadius);
      }
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
