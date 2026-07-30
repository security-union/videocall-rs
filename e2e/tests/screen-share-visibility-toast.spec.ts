import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Screen-share visibility toast E2E (HCL issue 893).
 *
 * Verifies the toast state machine that confirms a publisher's shared
 * content is actually being decoded by at least one other peer:
 *
 *   Idle → click share → "Starting to share content..."
 *     → peer decodes first frame → "Others can now see your shared content"
 *     → autodismiss
 *
 *   no peer decodes within ~10s → error toast
 */

/**
 * Inject a mock `getDisplayMedia` that returns a synthetic MediaStream
 * from a canvas. Mirrors the helper in `screen-share-state.spec.ts`.
 */
const MOCK_GET_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    navigator.mediaDevices.getDisplayMedia = function(_constraints) {
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
        const stream = canvas.captureStream(10);
        setTimeout(() => resolve(stream), 150);
      });
    };
  })();
`;

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

  if (result === "waiting" || result === "waiting-for-meeting") {
    return result;
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
 * Start a screen share by clicking the (inactive) screen-share control.
 *
 * The `ScreenShareButton` (dioxus-ui/src/components/video_control_buttons.rs)
 * has NO `title` attribute — it is a `button.video-control-button` whose
 * `.tooltip-title` reads "Screen share — Share Screen" when inactive and
 * "Screen share — Stop Screen Share" when active. Match by tooltip text, the
 * same proven approach the bvt1 screen-share-panel.spec.ts uses. The old
 * `title=`/`slot-secondary button:first-child` selector matched nothing (or the
 * wrong slot — Reactions is the first secondary slot in DEFAULT_SLOTS).
 */
async function clickScreenShareButton(page: Page): Promise<void> {
  const btn = page.locator("button.video-control-button", {
    has: page.locator(".tooltip", { hasText: "Share Screen" }),
  });
  await expect(btn).toBeVisible({ timeout: 10_000 });
  await btn.click();
}

/**
 * Stop an in-progress screen share by clicking the (active) screen-share
 * control. When active the tooltip reads "…Stop Screen Share" — which never
 * contains the substring "Share Screen", so the two selectors are unambiguous.
 */
async function stopScreenShareButton(page: Page): Promise<void> {
  const btn = page.locator("button.video-control-button", {
    has: page.locator(".tooltip", { hasText: "Stop Screen Share" }),
  });
  await expect(btn).toBeVisible({ timeout: 10_000 });
  await btn.click();
}

/**
 * Whether the test-only injection hook is attached (only when
 * MOCK_PEERS_ENABLED=true; the e2e docker stack sets it). Mirrors the
 * presence-probe every sibling injection spec uses — decode-budget.spec.ts's
 * `hasInjectHook` and freshness-skip.spec.ts's `hasInjectHook`. Unlike those
 * siblings (which `test.skip` when MOCK is off), `injectScreenFirstRender`
 * asserts on this because the spec is untagged and only ever runs against the
 * MOCK-enabled e2e stack — see the rationale there.
 */
async function hasScreenFirstRenderHook(page: Page): Promise<boolean> {
  return page.evaluate(
    () =>
      typeof (window as unknown as { __videocall_inject_screen_first_render?: unknown })
        .__videocall_inject_screen_first_render === "function",
  );
}

/**
 * Deterministically simulate "a screen frame just finished painting" on the
 * receiver, via the test-only `window.__videocall_inject_screen_first_render()`
 * hook (gated behind MOCK_PEERS_ENABLED, which the e2e docker stack sets true).
 *
 * The hook arms a one-shot flag; the NEXT real SCREEN packet's `decode()`
 * (screen packets already flow ~10/s once a share starts) consumes it through
 * the exact same `mark_first_render` the real paint pipeline uses, so the
 * manager's unmodified ack path fires PEER_EVENT(screen_decode_started) back to
 * the publisher. No shortcut, no parallel toast logic. Re-armable across a
 * stop→restart because the manager's `rearm_first_render_ack()` (HCL #893)
 * resets the latch on the screen-off edge.
 *
 * Asserts (via `hasScreenFirstRenderHook`) that the hook is present: a missing
 * hook in this spec's only runtime (the MOCK-enabled e2e stack) is a broken
 * harness, so it fails loudly rather than skipping — see the guard body.
 */
async function injectScreenFirstRender(page: Page): Promise<void> {
  // FAIL LOUD — do NOT skip. This spec is UNTAGGED, so it runs ONLY under the
  // `dioxus` Playwright project (playwright.config.ts), which is only ever
  // pointed at the e2e docker stack (docker/docker-compose.e2e.yaml, via
  // `make e2e` or e2e-hcl.yaml `--project=dioxus`). That stack hardcodes
  // `MOCK_PEERS_ENABLED=true` (a literal, not `${VAR:-...}`), and that flag is
  // the ONLY gate on registration of this hook (dioxus-ui
  // screen_first_render_inject.rs). So in every environment this spec actually
  // runs, a missing hook is NEVER an "unsupported deployment" — it is a broken
  // harness (renamed/removed hook, regressed MOCK gate, broken config.local.js
  // generation, or a netsim/trunk build that stopped registering the module).
  // Skipping would silently delete the screen-share + re-share (HCL #893)
  // coverage this spec exists to add and report a false green; a clear
  // assertion also replaces the opaque `TypeError: … is not a function` the
  // original unguarded call threw.
  const hasHook = await hasScreenFirstRenderHook(page);
  expect(
    hasHook,
    "window.__videocall_inject_screen_first_render is not attached. The e2e stack " +
      "must run with MOCK_PEERS_ENABLED=true (docker/docker-compose.e2e.yaml) so the " +
      "screen-first-render inject hook is registered (dioxus-ui screen_first_render_inject.rs). " +
      "A missing hook here means the harness is broken, not an unsupported deployment.",
  ).toBe(true);

  await page.evaluate(() => {
    (
      window as unknown as { __videocall_inject_screen_first_render: () => void }
    ).__videocall_inject_screen_first_render();
  });
}

test.describe("Screen-share visibility toast", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Happy path: host + peer in the same meeting. Host shares screen.
   * The peer's renderer decodes the first frame and emits a PEER_EVENT
   * back; the host UI transitions to the success toast.
   *
   * Previously skipped because headless-Chromium/SwiftShader made the
   * guest's real WebCodecs paint (and thus `mark_first_render`)
   * non-deterministic. Now driven deterministically by the test-only
   * `window.__videocall_inject_screen_first_render()` hook: it arms a
   * one-shot flag consumed by the next REAL SCREEN `decode()`, routing
   * through the same `mark_first_render` the real paint uses, so the
   * manager's unmodified ack path fires. See `injectScreenFirstRender`.
   */
  test("transitions to success when a peer decodes the shared content", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_ss_vis_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "ss-vis-host@videocall.rs",
        "ShareHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "ss-vis-guest@videocall.rs",
        "ShareGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host needs the mock getDisplayMedia before any navigation.
      await hostPage.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);

      await navigateToMeeting(hostPage, meetingId, "ShareHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "ShareGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });
      await expect(guestPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });

      // Wait for bidirectional peer connection: host sees guest AND guest
      // sees host. The ack path requires the guest to send packets back
      // through the relay, so both directions must be established.
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 45_000,
      });
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 45_000,
      });

      // Reveal dock controls.
      await wakeControls(hostPage);
      await hostPage.waitForTimeout(500);

      await clickScreenShareButton(hostPage);

      // The "Starting" toast must appear first.
      await expect(
        hostPage.locator(".peer-toast.toast-loading.screen-share-toast", {
          hasText: "Starting to share content",
        }),
      ).toBeVisible({ timeout: 5_000 });

      // The guest's split layout appears from the screen-on heartbeat,
      // confirming the share is active and SCREEN media packets are flowing
      // to the guest's decode() path.
      await expect(guestPage.locator(".split-screen-tile")).toBeVisible({
        timeout: 15_000,
      });

      // Deterministically drive the guest's "first screen frame painted" so
      // the next real SCREEN decode() returns first_frame:true and the manager
      // fires PEER_EVENT(screen_decode_started). Call it promptly so the
      // success-toast assertion proves the hook is load-bearing rather than
      // racing real WebCodecs paint luck.
      await injectScreenFirstRender(guestPage);

      // The "SuccessfullyShared" toast must replace it once the guest
      // decodes the first frame and acks via PEER_EVENT.
      await expect(
        hostPage.locator(".peer-toast.toast-success.screen-share-toast", {
          hasText: "Others can now see your shared content",
        }),
      ).toBeVisible({ timeout: 30_000 });

      // The success toast must auto-dismiss after a few seconds.
      await expect(hostPage.locator(".peer-toast.toast-success.screen-share-toast")).toHaveCount(
        0,
        { timeout: 10_000 },
      );
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Re-share path (HCL #893, the exact bug `rearm_first_render_ack` fixes):
   * share → success → STOP → share AGAIN → the success toast must fire on the
   * SECOND share too.
   *
   * The screen decoder is reused across stop→restart (only flushed, never
   * recreated), so its once-per-lifetime first-render ack latch survives. On
   * the screen-off heartbeat edge the manager calls `rearm_first_render_ack()`
   * to reset that latch; without it the second share's decode() never returns
   * first_frame:true again (even with the injection hook firing, because
   * `mark_first_render` no-ops once `fired` is already true), so the publisher
   * falsely toasts "No peers received the shared content within 10 seconds".
   *
   * Mutation sensitivity: reverting `rearm_first_render`/`rearm_first_render_ack`
   * to a no-op makes this test's SECOND success-toast assertion fail.
   */
  test("re-fires the success toast on a second share after stopping (HCL #893)", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_ss_vis_reshare_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "ss-vis-rehost@videocall.rs",
        "ReshareHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "ss-vis-reguest@videocall.rs",
        "ReshareGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // Host needs the mock getDisplayMedia before any navigation.
      await hostPage.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);

      await navigateToMeeting(hostPage, meetingId, "ReshareHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "ReshareGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });
      await expect(guestPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });

      // Both directions of the peer connection must be established — the ack
      // path requires the guest to send packets back through the relay.
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 45_000,
      });
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 45_000,
      });

      await wakeControls(hostPage);
      await hostPage.waitForTimeout(500);

      // ---- First share: must succeed. ----
      await clickScreenShareButton(hostPage);

      await expect(
        hostPage.locator(".peer-toast.toast-loading.screen-share-toast", {
          hasText: "Starting to share content",
        }),
      ).toBeVisible({ timeout: 5_000 });

      await expect(guestPage.locator(".split-screen-tile")).toBeVisible({
        timeout: 15_000,
      });

      await injectScreenFirstRender(guestPage);

      await expect(
        hostPage.locator(".peer-toast.toast-success.screen-share-toast", {
          hasText: "Others can now see your shared content",
        }),
      ).toBeVisible({ timeout: 30_000 });

      // Let the first success toast auto-dismiss before the second share so the
      // second assertion cannot pass on a lingering first-share toast.
      await expect(hostPage.locator(".peer-toast.toast-success.screen-share-toast")).toHaveCount(
        0,
        { timeout: 10_000 },
      );

      // ---- Stop the share: this is the screen-off edge that must re-arm. ----
      await wakeControls(hostPage);
      await hostPage.waitForTimeout(300);
      await stopScreenShareButton(hostPage);

      // The guest's split layout tears down when the share stops, confirming
      // the screen-off heartbeat reached the guest (where rearm_first_render_ack
      // runs).
      await expect(guestPage.locator(".split-screen-tile")).toHaveCount(0, {
        timeout: 15_000,
      });

      // ---- Second share: must ALSO succeed (this is what #893 guards). ----
      await wakeControls(hostPage);
      await hostPage.waitForTimeout(300);
      await clickScreenShareButton(hostPage);

      await expect(
        hostPage.locator(".peer-toast.toast-loading.screen-share-toast", {
          hasText: "Starting to share content",
        }),
      ).toBeVisible({ timeout: 5_000 });

      await expect(guestPage.locator(".split-screen-tile")).toBeVisible({
        timeout: 15_000,
      });

      await injectScreenFirstRender(guestPage);

      // On the un-fixed code the latch is still `true` from the first share, so
      // this second success toast never appears and the host times out into the
      // error toast instead. With the #893 fix it must appear.
      await expect(
        hostPage.locator(".peer-toast.toast-success.screen-share-toast", {
          hasText: "Others can now see your shared content",
        }),
      ).toBeVisible({ timeout: 30_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * Error path: host shares but no peer ever decodes the share (we
   * intercept and drop every PEER_EVENT packet leaving the guest's
   * outgoing send queue). After ~10s the host UI must show the error
   * toast.
   *
   * Implementation note: rather than mocking the relay, we just join
   * a meeting alone — there is no peer to ack. The host should see
   * the error toast after the 10-second window expires.
   */
  test("transitions to error after timeout when no peer acks", async ({ baseURL }) => {
    test.setTimeout(60_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_ss_vis_err_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser,
        "ss-vis-solo@videocall.rs",
        "SoloHost",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      await hostPage.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);

      await navigateToMeeting(hostPage, meetingId, "SoloHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await expect(hostPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });

      await wakeControls(hostPage);
      await hostPage.waitForTimeout(500);

      await clickScreenShareButton(hostPage);

      await expect(
        hostPage.locator(".peer-toast.toast-loading.screen-share-toast", {
          hasText: "Starting to share content",
        }),
      ).toBeVisible({ timeout: 5_000 });

      // After the 10-second visibility window with no peer ack, the
      // toast must transition to the error variant.
      await expect(hostPage.locator(".peer-toast.toast-error.screen-share-toast")).toBeVisible({
        timeout: 15_000,
      });
    } finally {
      await browser.close();
    }
  });
});
