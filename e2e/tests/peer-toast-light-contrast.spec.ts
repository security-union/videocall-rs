import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";

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
  // Hydration-robust submit (helpers/join-meeting.ts): gates on the
  // post-hydration submit button and waits for the home form to detach (the
  // route-changed signal that is robust to the History-API URL lag under load),
  // instead of a fixed wait + single Enter + URL poll.
  await fillAndSubmitJoinForm(page, meetingId, username);
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

// data-testid on the synthetic container so every assertion below is scoped to
// OUR injected nodes and can never collide with a real toast the app happens to
// render (e.g. a "Host muted your microphone" toast, which also carries
// `.toast-left`).
const SYNTHETIC_TESTID = "synthetic-peer-toasts";

/**
 * Inject the leave / loading / error toast variants into the LIVE meeting
 * document, faithfully mirroring the markup that `attendants.rs` renders
 * (peer join/leave block ~L6616-6685, screen-share loading ~L6444-6465, error
 * ~L6496-6528). We inject rather than produce these toasts through the app for
 * two reasons the harness makes unavoidable:
 *
 *   1. The "left the meeting" toast is SUPPRESSED in this harness by a known
 *      videocall-client callback-ordering bug — the peer is removed from the
 *      decode manager before `on_peer_left` fires, so `has_peer_with_user_id()`
 *      returns false and the toast is never pushed. This is why the two
 *      leave-toast tests in toast-notifications.spec.ts are `test.skip`; a real
 *      `guestPage.close()` leave flow TIMES OUT and cannot run green.
 *   2. The screen-share loading/error toasts require a `ScreenShareToastState`
 *      transition the fake-media harness does not deterministically produce.
 *      Issue #1281 explicitly sanctions driving these "synthetically".
 *
 * What is under test here is the CSS RULE, not the toast-production path: the
 * real stylesheet is loaded, the real `html[data-theme="light"]` is set via the
 * real toggle, and the injected DOM matches the exact selectors the rules
 * target. Reverting any of the guarded rules to a theme-flipping token makes
 * `getComputedStyle().color` near-black under light theme and fails the
 * assertion below — the same mutation-honesty the join-toast test above relies
 * on. The nodes are appended OUTSIDE the Dioxus tree, so the framework's
 * reconciler never touches or removes them.
 */
async function injectSyntheticToasts(page: Page): Promise<void> {
  await page.evaluate((testid: string) => {
    // Remove a prior injection if the test re-runs within the same page.
    document.querySelector(`[data-testid="${testid}"]`)?.remove();

    // NOTE: `<svg>` inside an HTML `innerHTML` string is parsed into the SVG
    // namespace by the HTML fragment parser, so these become real SVGElements
    // whose computed `color` (driving `stroke="currentColor"`) is exactly what
    // the `.toast-icon svg` rules set.
    const container = document.createElement("div");
    container.className = "peer-toasts";
    container.setAttribute("data-testid", testid);
    container.innerHTML = `
      <div class="peer-toast toast-left">
        <span class="toast-icon">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"></path>
            <circle cx="9" cy="7" r="4"></circle>
            <line x1="22" y1="11" x2="16" y2="11"></line>
          </svg>
        </span>
        <span class="toast-text">
          <span class="toast-name">SyntheticLeaver</span>
          <br />
          <span class="toast-action">left the meeting</span>
        </span>
      </div>
      <div class="peer-toast toast-loading screen-share-toast" role="status" aria-live="polite" aria-label="Starting to share content">
        <span class="toast-icon">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21 12a9 9 0 1 1-6.219-8.56"></path>
          </svg>
        </span>
        <span class="toast-text">
          <span class="toast-name">Starting to share content...</span>
        </span>
      </div>
      <div class="peer-toast toast-error screen-share-toast" role="alert" aria-live="assertive" aria-label="Screen share visibility error">
        <span class="toast-icon">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="10"></circle>
            <line x1="12" y1="8" x2="12" y2="12"></line>
            <line x1="12" y1="16" x2="12.01" y2="16"></line>
          </svg>
        </span>
        <span class="toast-text">
          <span class="toast-name">Screen share failed</span>
        </span>
      </div>
    `;
    document.body.appendChild(container);
  }, SYNTHETIC_TESTID);
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

  // Companion to the join-toast test above. The join flow only exercises TWO of
  // the six literal-light rules PR #1280 changed (`.toast-name` and the green
  // join `.toast-action`); the leave-variant faint text/icon, the base
  // `.peer-toast` color, and the loading/error icons went unpinned — issue
  // #1281. Because the leave toast is suppressed in the harness (callback-order
  // bug; see toast-notifications.spec.ts `test.skip`) and the screen-share
  // loading/error states are not deterministically producible with fake media,
  // we drive those variants synthetically (issue #1281 sanctions this) against
  // the REAL stylesheet under the REAL light-theme toggle. See
  // `injectSyntheticToasts` for the full rationale and mutation-honesty note.
  test("leave/loading/error toast variants stay light-on-dark in light theme @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_toast_light_variants_${Date.now()}`;

    // Single participant: we inject the toast DOM, so no guest is needed. The
    // host only has to reach the grid (for the in-meeting Appearance toggle).
    const browser1 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-toast-variants@videocall.rs",
        "ToastVariantsHost",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();

      // Host starts the meeting.
      await navigateToMeeting(hostPage, meetingId, "ToastVariantsHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Force LIGHT theme via the real Appearance toggle — the theme under which
      // the #1189 bug manifested.
      await setLightThemeFromMeeting(hostPage);

      // Inject the leave/loading/error toast variants into the live document.
      await injectSyntheticToasts(hostPage);

      const container = hostPage.locator(`[data-testid="${SYNTHETIC_TESTID}"]`);
      await expect(container).toBeVisible();

      // Confirm we are still in light theme at assertion time (a stray re-init
      // flipping back to dark would make the assertions pass for the wrong
      // reason — every guarded token is already light on dark).
      expect(
        await hostPage.evaluate(() => document.documentElement.getAttribute("data-theme")),
      ).toBe("light");

      // ── Base `.peer-toast` color (guards the `--on-dark-text` / former
      // `#ffffff` base rule; a revert to `--text-primary` = `#1a1a1a` under
      // light theme has luminance ~26, failing the floor). Read on the leave
      // toast div itself, whose own `color` is the un-overridden base value. ──
      const baseColor = await container
        .locator(".peer-toast.toast-left")
        .evaluate((el) => window.getComputedStyle(el).color);
      expect(luminance(baseColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);

      // ── Leave-variant action text ("left the meeting"). The strongest gap:
      // #1189 explicitly names "join/leave". Reverting `.toast-left
      // .toast-action` restores `--text-on-glass-faint` = rgba(0,0,0,0.44)
      // under light theme (luminance 0), failing the floor. ──
      const leaveActionColor = await container
        .locator(".peer-toast.toast-left .toast-action")
        .evaluate((el) => window.getComputedStyle(el).color);
      expect(luminance(leaveActionColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);

      // ── Leave-variant icon SVG (reverts to rgba(0,0,0,0.45), luminance 0).
      // getComputedStyle(svg).color drives the `stroke="currentColor"`. ──
      const leaveIconColor = await container
        .locator(".peer-toast.toast-left .toast-icon svg")
        .evaluate((el) => window.getComputedStyle(el).color);
      expect(luminance(leaveIconColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);

      // ── Loading-variant icon SVG (screen-share "Starting..." toast). ──
      const loadingIconColor = await container
        .locator(".peer-toast.toast-loading .toast-icon svg")
        .evaluate((el) => window.getComputedStyle(el).color);
      expect(luminance(loadingIconColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);

      // ── Error-variant icon SVG (screen-share failure toast). ──
      const errorIconColor = await container
        .locator(".peer-toast.toast-error .toast-icon svg")
        .evaluate((el) => window.getComputedStyle(el).color);
      expect(luminance(errorIconColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);
    } finally {
      await browser1.close();
    }
  });
});
