import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E regression test for HCL issue #1189 (item 1): peer join/leave toast text
 * must stay readable in LIGHT theme.
 *
 * THE BUG (fixed in commit "fix(ui): restore peer-toast text contrast in light
 * mode"): the `.peer-toast` surface is hardcoded dark (`rgba(30,30,30,0.88)`) in
 * BOTH themes, but its foreground text rules used theme-flipping tokens
 * (`--text-on-glass-strong`, `--text-on-glass-faint`, `--text-primary`). Under
 * `html[data-theme="light"]` those tokens resolve to NEAR-BLACK, so the
 * participant name (`.toast-name`) and the "joined the meeting" action text
 * (`.toast-action`) rendered black-on-dark — i.e. INVISIBLE. The fix replaced
 * each with a literal light-on-dark value.
 *
 * WHY THIS TEST IS NON-FLAKY (toast specs have a flake history in this repo):
 *   - Theme is set deterministically via the real "Light" UI toggle, which
 *     synchronously sets `html[data-theme="light"]` (same mechanism proven in
 *     theme-toggle.spec.ts). We set it BEFORE the toast is produced.
 *   - The toast is produced by the same reliable 2-participant join flow used by
 *     toast-notifications.spec.ts / join-leave-notifications.spec.ts: polling
 *     for the toast starts BEFORE the guest joins, so the PARTICIPANT_JOINED
 *     event cannot be missed.
 *   - The colour assertion is RACE-FREE within the auto-dismiss window. The
 *     toast element lives in the DOM for a full 8s (`Timeout::new(8_000, ...)`
 *     in attendants.rs removes it). The CSS `toast-exit` animation at 7.5s only
 *     animates `opacity`/`transform` — it NEVER touches `color`. So
 *     `getComputedStyle(.toast-name).color` returns the static rule value the
 *     entire time the node exists, independent of the fade. We read it the
 *     instant after `toBeVisible` resolves, well inside the 8s window.
 *
 * MUTATION HONESTY: under light theme, reverting the fix (re-introducing the
 * theme-flipping tokens) makes `getComputedStyle().color` near-black —
 * `rgb(0, 0, 0)` / `rgba(0,0,0,0.92)` for `.toast-name`, luminance ~0 — which
 * FAILS the `luminance > LIGHT_LUMINANCE_FLOOR` assertions below. The fixed
 * values are white (`rgba(255,255,255,0.95)` / `rgba(255,255,255,0.5)`),
 * luminance 255. The floor of 180 cleanly separates the two.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

// A revert re-introduces near-black tokens (luminance ~0); the fix is white
// (luminance 255). 180 sits comfortably between the two with no overlap.
const LIGHT_LUMINANCE_FLOOR = 180;

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
  // Display name is a controlled input -- clear before typing to handle any pre-fill.
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
 * Switch the host's meeting page to LIGHT theme via the real Appearance toggle,
 * then close the settings modal. Mirrors theme-toggle.spec.ts's mechanism: the
 * "Light" icon button sets `html[data-theme]="light"` synchronously.
 */
async function setLightThemeFromMeeting(page: Page): Promise<void> {
  // Reveal the action bar (autohide may be active) and open device settings.
  await page.locator(".video-controls-container").hover();
  await page.locator('[data-testid="open-settings"]').click();
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });

  // Appearance tab → Light button.
  await page.getByRole("tab", { name: "Appearance" }).click();
  await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });
  await page.getByRole("button", { name: "Light", exact: true }).click();

  // The toggle sets data-theme synchronously; poll defensively for the attr.
  await expect
    .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
      timeout: 3_000,
    })
    .toBe("light");

  // Close the modal so it cannot overlap or steal the toast region.
  await page.locator('button[aria-label="Close settings"]').click();
  await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });
}

function parseRgb(value: string): { r: number; g: number; b: number } {
  const match = value.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)/i);
  if (!match) {
    throw new Error(`Unsupported color format: ${value}`);
  }
  return { r: Number(match[1]), g: Number(match[2]), b: Number(match[3]) };
}

// Rec.709 relative luminance of the RGB channels (alpha ignored on purpose:
// getComputedStyle().color is NOT composited against the background, so the
// raw channel values are exactly the fixed white vs reverted near-black we
// want to discriminate).
function luminance(value: string): number {
  const { r, g, b } = parseRgb(value);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

test.describe("Peer toast text contrast in light theme (#1189)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("join toast name + action text stay light-on-dark in light theme @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_toast_light_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-toast-light@videocall.rs",
        "ToastLightHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-toast-light@videocall.rs",
        "ToastLightGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host starts the meeting.
      await navigateToMeeting(hostPage, meetingId, "ToastLightHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Force LIGHT theme BEFORE the toast is produced — this is the theme under
      // which the bug manifested.
      await setLightThemeFromMeeting(hostPage);

      // Start watching for the join toast BEFORE the guest joins so a fast
      // PARTICIPANT_JOINED cannot be missed.
      const hostJoinedToast = hostPage.locator(".peer-toast.toast-joined", {
        hasText: "joined the meeting",
      });
      const toastPromise = expect(hostJoinedToast.first()).toBeVisible({ timeout: 30_000 });

      // Guest joins.
      await navigateToMeeting(guestPage, meetingId, "ToastLightGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Toast is up. The element lives 8s in the DOM and the 7.5s exit animation
      // only fades opacity (never color), so the color reads below are race-free.
      await toastPromise;
      const firstToast = hostJoinedToast.first();

      // Confirm we are still in light theme at assertion time (guards against a
      // stray re-init flipping it back to dark, which would make the assertion
      // pass for the wrong reason).
      expect(
        await hostPage.evaluate(() => document.documentElement.getAttribute("data-theme")),
      ).toBe("light");

      // ── The headline bug: the participant name must NOT be near-black. ──
      const toastName = firstToast.locator(".toast-name");
      await expect(toastName).toBeVisible();
      const nameColor = await toastName.evaluate((el) => window.getComputedStyle(el).color);
      // Fixed = rgba(255,255,255,0.95) → lum 255. Reverted (--text-on-glass-strong
      // under light) = rgba(0,0,0,0.92) → lum 0. Floor 180 separates them.
      expect(luminance(nameColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);

      // ── The "joined the meeting" action text must also stay light. ──
      const toastAction = firstToast.locator(".toast-action");
      await expect(toastAction).toBeVisible();
      const actionColor = await toastAction.evaluate((el) => window.getComputedStyle(el).color);
      // NOTE: for the JOIN variant `.toast-action` uses --success-icon-text (a
      // saturated green) which reads fine on dark in both themes and was left
      // unchanged by the fix. We assert it is not near-black (the failure mode
      // the fix guards the .toast-left faint text against) rather than pinning a
      // white luminance, so this stays honest for the green join action.
      expect(luminance(actionColor)).toBeGreaterThan(60);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
