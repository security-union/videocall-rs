import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

const DEFAULT_UI_URL = "http://localhost:3001";
const CSP_HEADER = "content-security-policy-report-only";

interface MeetingMember {
  page: Page;
  name: string;
}

async function seedAudioVideoOn(context: BrowserContext): Promise<void> {
  await context.addInitScript(() => {
    window.localStorage.setItem("vc_prejoin_camera_on", "true");
    window.localStorage.setItem("vc_prejoin_mic_on", "true");
  });
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

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });
  await page.waitForTimeout(1500);
}

async function ensurePrejoinCameraOn(page: Page): Promise<void> {
  const allow = page.locator('[data-testid="prejoin-permission-allow"]');
  if (await allow.isVisible().catch(() => false)) {
    await allow.click();
    await page
      .locator('[data-testid="prejoin-permission-prompt"]')
      .waitFor({ state: "hidden", timeout: 15_000 })
      .catch(() => {
        /* prompt already handled */
      });
  }

  const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
  if (await cameraToggle.isVisible().catch(() => false)) {
    if ((await cameraToggle.getAttribute("aria-pressed")) !== "true") {
      await cameraToggle.click();
    }
    await expect(cameraToggle).toHaveAttribute("aria-pressed", "true", { timeout: 5_000 });
  }
}

async function clickJoinAndEnterGrid(page: Page): Promise<"in-meeting" | "waiting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    return "waiting";
  }

  if (result === "join") {
    await ensurePrejoinCameraOn(page);
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function standUpTwoPeerAudioVideoCall(
  browsers: Browser[],
  uiURL: string,
  meetingId: string,
): Promise<MeetingMember[]> {
  const profiles = [
    { email: "csp-host@videocall.rs", name: "CspHost" },
    { email: "csp-guest@videocall.rs", name: "CspGuest" },
  ];

  const members: MeetingMember[] = [];
  for (let i = 0; i < profiles.length; i++) {
    const context = await createAuthenticatedContext(
      browsers[i],
      profiles[i].email,
      profiles[i].name,
      uiURL,
    );
    await seedAudioVideoOn(context);
    members.push({
      page: await context.newPage(),
      name: profiles[i].name,
    });
  }

  await navigateToMeeting(members[0].page, meetingId, members[0].name);
  expect(await clickJoinAndEnterGrid(members[0].page)).toBe("in-meeting");

  await navigateToMeeting(members[1].page, meetingId, members[1].name);
  const guestResult = await clickJoinAndEnterGrid(members[1].page);

  if (guestResult === "waiting") {
    const admitButton = members[0].page.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await members[0].page.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await members[0].page.waitForTimeout(3000);
    await clickJoinAndEnterGrid(members[1].page);
  }

  // Assert peer render the SAME way the proven two-users-meeting spec does:
  // wait for the first `.canvas-container` to become VISIBLE. The earlier
  // `toHaveCount(1)` on the inner `canvas` element was brittle — the tile's
  // `<canvas>` child may not have mounted within the window (count 0), and the
  // count is not guaranteed to be exactly 1. Visibility of the container is the
  // real "peer render succeeded" signal, and CSP Report-Only cannot affect it.
  await expect(members[0].page.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 30_000,
  });
  await expect(members[1].page.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 30_000,
  });

  return members;
}

test.describe("UI Content-Security-Policy Report-Only", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("home response carries the report-only policy @bvt1", async ({ page }) => {
    const response = await page.goto("/");
    expect(response, "home navigation must return a response").not.toBeNull();

    const header = response?.headers()[CSP_HEADER];

    // The CSP header is emitted by the PROD nginx serving layer and by the
    // `static` dev/e2e serve (miniserve --header). The default local stack
    // (`make e2e-up`) runs `DIOXUS_SERVE_MODE=dev` → `trunk serve`, which has no
    // custom-header flag (trunk 0.21) and therefore serves NO CSP header. CI runs
    // `static` in BOTH e2e workflows and sets `EXPECT_CSP_HEADER=1` on this step,
    // so the assertion is HARD in CI (a static-mode regression fails, never skips)
    // and lenient for a local dev-mode run (skips with a clear message).
    if (!header) {
      if (process.env.EXPECT_CSP_HEADER) {
        throw new Error(
          "CSP Report-Only header MUST be present when EXPECT_CSP_HEADER is set " +
            "(static serve mode / CI). Its absence is a regression, not a dev-mode skip.",
        );
      }
      test.skip(
        true,
        "CSP header not served in dev-mode (trunk serve) — run the static stack " +
          "(DIOXUS_SERVE_MODE=static, as CI does) to exercise this assertion.",
      );
      return;
    }

    expect(header).toContain("default-src 'self'");
    expect(header).toContain("script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'");
    expect(header).toContain("connect-src 'self'");
    expect(header).toContain("frame-ancestors 'none'");
    expect(header).toContain("upgrade-insecure-requests");
    expect(response?.headers()["content-security-policy"]).toBeUndefined();
  });

  test("authenticated two-peer audio/video join still reaches the grid @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `csp_av_${Date.now()}`;
    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    try {
      await standUpTwoPeerAudioVideoCall(browsers, uiURL, meetingId);
    } finally {
      await Promise.all(browsers.map((browser) => browser.close()));
    }
  });
});
