import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Speaking-glow E2E tests.
 *
 * The speaking glow is rendered as:
 * - `.speaking-tile` CSS class on the outer tile div (peer: `.grid-item`,
 *    host self-view: `#host-controls-nav`)
 * - Inline `box-shadow` + `transition` via `speak_style()` on the same element
 *
 * Since Playwright uses `--use-fake-device-for-media-stream`, no real audio is
 * produced, so all participants remain in the "silent" state.  Tests verify
 * the **silent baseline** (no glow, correct transition, no speaking class) and
 * structural correctness of the elements that would receive glow.
 *
 * Screen-share glow suppression cannot be fully automated because
 * `getDisplayMedia()` opens a system picker that Playwright cannot drive.
 * Pinned-peer suppression requires a speaking peer to observe the glow
 * difference.  Both are documented inline with best-effort structural checks.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
];

async function createAuthenticatedContext(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  email: string,
  name: string,
  uiURL: string,
) {
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
) {
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

  const hostPage = await hostCtx.newPage();
  const guestPage = await guestCtx.newPage();

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
  // LIMITATION: Cannot be fully automated. getDisplayMedia() opens a
  // system-level picker that Playwright cannot drive. However, the code
  // path is verified here structurally:
  //   - In the grid layout, screen-share tiles are rendered inside a
  //     separate `.grid-item` div WITHOUT speaking-tile or box-shadow.
  //   - In the split layout (TileMode::ScreenOnly), the screen share
  //     renders inside a `.split-screen-tile` div with NO glow props.
  //   - The Rust source (`canvas_generator.rs` line 216) sets
  //     `is_suppressed = true` when `is_screen_share_enabled_for_peer`
  //     is true, which forces `visible_audio_level = 0` and removes the
  //     `.speaking-tile` class.
  //
  // The test below verifies that in a normal (no-screen-share) meeting
  // the grid-item tiles do NOT have screen-share glow artifacts or stale
  // split-screen-tile elements.
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
});
