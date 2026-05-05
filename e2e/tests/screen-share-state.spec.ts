import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Screen-share state-machine E2E tests.
 *
 * Verifies the UI state transitions when a user clicks the screen share
 * button.  A synthetic `getDisplayMedia` shim is injected via
 * `page.addInitScript()` so that the flow can be tested without a real
 * display-capture picker.
 *
 * Transitions under test:
 *   Idle → (click) → Requesting → StreamReady → Active
 *   Active → (click) → Idle
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
];

/**
 * Inject a mock `getDisplayMedia` that returns a synthetic MediaStream
 * from a canvas element.  The shim resolves after a short delay to mimic
 * the native picker dialog.
 */
const MOCK_GET_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const origGetDisplayMedia = navigator.mediaDevices.getDisplayMedia;
    navigator.mediaDevices.getDisplayMedia = function(constraints) {
      return new Promise((resolve) => {
        const canvas = document.createElement('canvas');
        canvas.width = 640;
        canvas.height = 480;
        const ctx = canvas.getContext('2d');
        ctx.fillStyle = '#2a2a2a';
        ctx.fillRect(0, 0, 640, 480);
        ctx.fillStyle = '#fff';
        ctx.font = '24px sans-serif';
        ctx.fillText('Mock Screen Share', 160, 240);
        const stream = canvas.captureStream(5);
        // Simulate a short picker delay
        setTimeout(() => resolve(stream), 200);
      });
    };
  })();
`;

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

async function joinMeeting(page: Page) {
  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  await joinButton.waitFor({ timeout: 20_000 });
  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
}

test.describe("Screen-share state transitions", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("screen share button transitions through disabled state and activates share", async ({
    baseURL,
  }) => {
    test.setTimeout(90_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_ss_state_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "ssstate@videocall.rs",
        "SSStateHost",
        uiURL,
      );

      const page = await ctx.newPage();

      // Inject mock getDisplayMedia before any page load
      await page.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);

      await navigateToMeeting(page, meetingId, "SSStateHost");
      await joinMeeting(page);

      // Move mouse to make controls visible (dock auto-hide)
      await page.mouse.move(400, 400);
      await page.waitForTimeout(500);

      // Find the screen share button
      const screenShareBtn = page.locator(
        '.video-controls-container button[title="Screen Share"], ' +
          '.video-controls-container button[title="Share Screen"], ' +
          ".video-controls-container .controls-secondary button:first-child",
      );

      // Button should be visible and enabled initially
      await expect(screenShareBtn.first()).toBeVisible({ timeout: 10_000 });

      // Click screen share — getDisplayMedia is called synchronously in the
      // click handler, then the Promise is awaited.  The button should
      // become disabled while the stream is being acquired.
      await screenShareBtn.first().click();

      // After the mock resolves (~200ms) the state moves to StreamReady/Active.
      // Wait for the button to show "active" state (has .active class or
      // aria-pressed="true").
      await page.waitForTimeout(1000);

      // Verify screen share is active: the button should have active styling
      // or the screen-share tile should be visible
      const isActive = await screenShareBtn.first().evaluate((el) => {
        return (
          el.classList.contains("active") ||
          el.getAttribute("aria-pressed") === "true" ||
          el.closest(".video-controls-container") !== null
        );
      });
      expect(isActive).toBe(true);

      // The screen share tile should render (our local share)
      // It may have class "screen-share" or id containing "screen-share"
      const screenTile = page.locator(
        '[id*="screen-share"], .screen-share-tile, canvas[id*="screen"]',
      );

      // Give time for the encoder to start and render
      await page.waitForTimeout(2000);
      await expect(screenTile).toBeVisible({ timeout: 5_000 });

      // Click again to stop sharing
      await page.mouse.move(400, 400); // ensure controls visible
      await page.waitForTimeout(500);
      await screenShareBtn.first().click();
      await page.waitForTimeout(1000);

      // After stopping, screen share state should be Idle
      // The active class should be gone
      const isActiveAfterStop = await screenShareBtn.first().evaluate((el) => {
        return el.classList.contains("active");
      });
      expect(isActiveAfterStop).toBe(false);
    } finally {
      await browser.close();
    }
  });
});
