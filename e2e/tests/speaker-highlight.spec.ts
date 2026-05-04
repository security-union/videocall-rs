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
  const unmuteButton = page.getByRole("button", { name: "Unmute" });
  if (await unmuteButton.count()) {
    await unmuteButton.first().click();
    await page.waitForTimeout(1_000);
  }
}

async function muteMicrophone(page: Page) {
  const muteButton = page.getByRole("button", { name: "Mute" });
  await expect(muteButton.first()).toBeVisible({ timeout: 10_000 });
  await muteButton.first().click();
  await page.waitForTimeout(1_000);
}

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 20_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 20_000 }).then(() => "waiting-for-meeting" as const),
  ]);

  if (result === "waiting") {
    return "waiting";
  }

  if (result === "waiting-for-meeting") {
    return "waiting-for-meeting";
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);

  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
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

    const guestJoinButton = guestPage.getByText(/Join Meeting|Start Meeting/);
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

  test("host remains highlighted for other participants while screen sharing", async ({
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

      await hostPage.getByRole("button", { name: "Share Screen" }).click();

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
});
