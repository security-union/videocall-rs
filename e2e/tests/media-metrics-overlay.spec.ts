import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Per-tile media-metrics overlay (issue 1768).
 *
 * The diagnostics drawer has a "Show media metrics on tiles" checkbox
 * (`input#diag-media-metrics-overlay`, testid `media-metrics-overlay-toggle`,
 * default OFF, persisted to localStorage key `diagnostics.media_metrics_overlay`).
 * When checked, each VIDEO tile renders a bottom-anchored, pointer-events:none
 * readout:
 *   - the local user's OWN tile → `[data-testid="media-metrics-overlay-self"]`
 *     with the SENDING resolution·fps·audio;
 *   - a REMOTE peer tile → `[data-testid="media-metrics-overlay-peer"]` with the
 *     RECEIVED resolution·fps·audio.
 * The text is always the three-segment `"{res} · {fps}fps · {audio}k"` shape (an
 * absent metric renders an em-dash), so every overlay contains `"fps"`.
 *
 * Issue #1784: the REMOTE peer's fps is the PAINTED rate (frames drawn to the
 * canvas), sourced from the decoder's per-peer `video_painted` diagnostics event —
 * not the arrival-rate `fps_received` bucket, which still feeds the diagnostics
 * drawer / signal popup unchanged. The visible three-segment format is unchanged;
 * the painted-vs-arrival source is guarded at the unit level (`overlay_painted_fps_
 * sample`, `PaintRateMeter` in videocall-client), while this spec asserts the
 * painted route reaches the overlay as a real number.
 *
 * Selectors below are anchored to the RSX authored in this change:
 *   - checkbox / label:  dioxus-ui/src/components/diagnostics.rs (Display options)
 *   - overlay element:   dioxus-ui/src/components/media_metrics_overlay.rs
 *     injected into `.canvas-container` by canvas_generator.rs (grid + split arms).
 *
 * A real second camera-on browser is required so the peer tile actually decodes
 * video (mock peers are video-OFF placeholders that never decode). Cameras AND
 * mics both default OFF, so `vc_prejoin_camera_on` and `vc_prejoin_mic_on` are
 * seeded to `"true"` for both users — camera-on drives the RECEIVED-video
 * metrics, mic-on drives the RECEIVED-audio kbps (issue #1769), which the overlay
 * asserts is a real number ("· {n}k"), not the em-dash placeholder ("· —k").
 * Untagged (no @bvt): runs in the dioxus full suite, not per-PR CI.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
  "--auto-select-desktop-capture-source=Entire screen",
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

  if (result === "waiting") return "waiting";
  if (result === "waiting-for-meeting") return "waiting-for-meeting";
  if (result === "auto-joined") return "in-meeting";

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
  if (guestResult === "in-meeting") return;

  if (guestResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

    const guestJoinButton = guestPage.getByRole("button", {
      name: /Join Meeting|Start Meeting/,
    });
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
 * Two-user meeting with real camera-on peers. Seeds `vc_prejoin_camera_on` so
 * both join with camera enabled (default is OFF), producing a decoding peer
 * tile — required for the RECEIVED-video overlay metrics.
 * (Pattern: crop-toggle.spec.ts::setupTwoUserMeeting.)
 */
async function setupTwoUserMeeting(uiURL: string, meetingId: string) {
  const browser1 = await chromium.launch({ args: BROWSER_ARGS });
  const browser2 = await chromium.launch({ args: BROWSER_ARGS });

  const hostCtx = await createAuthenticatedContext(
    browser1,
    "metricshost@videocall.rs",
    "MetricsHost",
    uiURL,
  );
  const guestCtx = await createAuthenticatedContext(
    browser2,
    "metricsguest@videocall.rs",
    "MetricsGuest",
    uiURL,
  );

  // Seed camera-on AND mic-on BEFORE page creation so the initial prejoin read
  // is true for both. Camera-on gives the RECEIVED-video overlay metrics; mic-on
  // gives the RECEIVED-audio kbps (issue #1769). BOTH default OFF
  // (`load_preferred_camera_on` / `load_preferred_mic_on` → false), so without
  // the mic seed the peer publishes no audio and the overlay's audio field would
  // legitimately read the em-dash "—k".
  await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
  await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_mic_on", "true");`);
  await guestCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
  await guestCtx.addInitScript(`localStorage.setItem("vc_prejoin_mic_on", "true");`);

  const hostPage = await hostCtx.newPage();
  const guestPage = await guestCtx.newPage();

  await navigateToMeeting(hostPage, meetingId, "MetricsHost");
  const hostResult = await joinMeetingFromPage(hostPage);
  expect(hostResult).toBe("in-meeting");

  await navigateToMeeting(guestPage, meetingId, "MetricsGuest");
  const guestResult = await joinMeetingFromPage(guestPage);
  await admitGuestIfNeeded(hostPage, guestPage, guestResult);

  // Wait for the remote peer tile (with a decoding canvas) on the host grid.
  const canvasTile = hostPage.locator(".grid-item:has(canvas)");
  await expect(canvasTile.first()).toBeVisible({ timeout: 30_000 });

  return { hostPage, guestPage, browser1, browser2 };
}

/** Open the diagnostics drawer via its tooltip button. */
async function openDiagnosticsPanel(page: Page): Promise<void> {
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await diagButton.click();
  await expect(page.locator("#diagnostics-sidebar")).toBeVisible({ timeout: 10_000 });
  await expect(page.locator("h3", { hasText: "Transport Preference" })).toBeVisible({
    timeout: 10_000,
  });
}

test.describe("Per-tile media-metrics overlay (issue 1768)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("checkbox toggles per-tile received/sending metric overlays", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `media_metrics_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(uiURL, meetingId);

    try {
      const overlays = hostPage.locator(".media-metrics-overlay");
      const selfOverlay = hostPage.locator('[data-testid="media-metrics-overlay-self"]');
      const peerOverlay = hostPage.locator('[data-testid="media-metrics-overlay-peer"]');

      // Default OFF: no overlay is rendered anywhere before the checkbox is on.
      await expect(overlays).toHaveCount(0);

      await openDiagnosticsPanel(hostPage);

      // The checkbox + its label must exist (fails outright on un-fixed code
      // that lacks the toggle).
      const toggle = hostPage.locator('[data-testid="media-metrics-overlay-toggle"]');
      await expect(toggle).toBeVisible({ timeout: 10_000 });
      await expect(hostPage.locator('label[for="diag-media-metrics-overlay"]')).toHaveText(
        "Show media metrics on tiles",
      );
      // Default OFF and still no overlays with the drawer merely open.
      await expect(toggle).not.toBeChecked();
      await expect(overlays).toHaveCount(0);

      // Turn the overlay ON.
      await toggle.check();
      await expect(toggle).toBeChecked();
      // Production writes the literal string "true"/"false" (local_storage::save_bool).
      await expect
        .poll(() =>
          hostPage.evaluate(() => localStorage.getItem("diagnostics.media_metrics_overlay")),
        )
        .toBe("true");

      // The local user's OWN tile shows its SENDING metrics. Assert visibility
      // BEFORE reading text; every overlay carries the "fps" segment, and the
      // self readout leads with the "↑" (sending) SHAPE cue — the CVD-safe self/
      // peer distinction (issue 1768; self text is white like peers, so colour is
      // never the sole cue).
      await expect(selfOverlay).toBeVisible({ timeout: 20_000 });
      await expect(selfOverlay).toContainText("fps");
      await expect(selfOverlay).toContainText("↑");

      // A REMOTE peer tile shows the RECEIVED metrics, led by the "↓" (receiving)
      // shape cue.
      await expect(peerOverlay.first()).toBeVisible({ timeout: 20_000 });
      await expect(peerOverlay.first()).toContainText("fps");
      await expect(peerOverlay.first()).toContainText("↓");

      // Issue #1784: the received fps is now the PAINTED rate (frames actually drawn
      // to the canvas), delivered per-peer on the decoder's `video_painted`
      // diagnostics event and consumed via `overlay_painted_fps_sample`. This polls
      // that the middle segment is a REAL integer ("· {n}fps ·"), not the em-dash
      // ("· —fps ·") — proving the painted-fps route actually reaches the overlay
      // (a broken route would leave the readout at the em-dash). With the peer's
      // camera seeded on, frames paint, so a positive painted-fps must appear.
      await expect
        .poll(async () => (await peerOverlay.first().textContent()) ?? "", {
          timeout: 30_000,
        })
        .toMatch(/·\s*\d+fps\s*·/);

      // Issue #1769: the RECEIVED-audio field is the trailing "· {n}k" segment and
      // must be a REAL number, not the em-dash placeholder "—k". With the guest's
      // mic seeded on (above), the host observes audio flowing from the peer and
      // the overlay sources the kbps from the per-tile `audio_bitrate` signal
      // (driven off the peer_status audio-on flag) instead of scanning the receive
      // snapshots. Poll because audio may take a moment to start flowing after the
      // tile appears; the em-dash form "· —k" never matches the numeric pattern.
      await expect
        .poll(async () => (await peerOverlay.first().textContent()) ?? "", {
          timeout: 30_000,
        })
        .toMatch(/·\s*\d+k\s*$/);

      // RELOAD PERSISTENCE — guards the `load_bool` read-back the spec would
      // otherwise leave unverified. After a reload the "true" preference must be
      // restored: the flag (seeded from localStorage on mount) drives the overlay
      // WITHOUT reopening the drawer, and the drawer checkbox reads checked.
      await hostPage.reload();
      const grid = hostPage.locator("#grid-container");
      const rejoinBtn = hostPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
      const landed = await Promise.race([
        grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
        rejoinBtn.waitFor({ timeout: 30_000 }).then(() => "join" as const),
      ]);
      if (landed === "join") {
        await rejoinBtn.click();
        await expect(grid).toBeVisible({ timeout: 15_000 });
      }
      // The self-view SENDING overlay reappears once the camera republishes —
      // proof the persisted flag was read back true on mount (no drawer needed).
      await expect(selfOverlay).toBeVisible({ timeout: 30_000 });
      await expect(selfOverlay).toContainText("fps");
      // And the drawer checkbox reflects the persisted "true".
      await openDiagnosticsPanel(hostPage);
      await expect(hostPage.locator('[data-testid="media-metrics-overlay-toggle"]')).toBeChecked();

      // Turn the overlay OFF: every overlay must leave the DOM (conditional
      // render, not visibility toggling) and the preference persists as "false".
      await toggle.uncheck();
      await expect(toggle).not.toBeChecked();
      await expect(overlays).toHaveCount(0, { timeout: 10_000 });
      await expect
        .poll(() =>
          hostPage.evaluate(() => localStorage.getItem("diagnostics.media_metrics_overlay")),
        )
        .toBe("false");
    } finally {
      await browser1.close().catch(() => undefined);
      await browser2.close().catch(() => undefined);
    }
  });
});
