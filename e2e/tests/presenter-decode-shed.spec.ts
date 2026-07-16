import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Presenter-aware decode-shedding E2E coverage (issue #1559).
 *
 * The decode budget caps how many peer tiles decode video at once. Issue #1559
 * adds a PRESENTER bias: while the LOCAL user is screen-sharing, the budget
 * sheds peer decodes MORE aggressively under pressure so CPU is freed for the
 * screen ENCODER (which in a ~15-peer meeting is otherwise starved by the many
 * concurrent peer DECODES). The bias is PRESSURE-GATED (a powerful, unpressured
 * device sharing keeps decoding peers) and recovers when sharing stops.
 *
 * ## Why this is deterministic / GPU-independent (unlike #1530)
 *
 * `screen-share-decode-budget.spec.ts`'s force-decode test is `test.fixme`
 * because headless Chrome's SwiftShader software decode cannot generate enough
 * REAL decode load to trip the budget with 3+ camera-on peers. This spec sits on
 * the OTHER harness: it drives the control loop SYNTHETICALLY via the test-only
 * `window.__videocall_inject_render_fps` hook (registered when
 * `MOCK_PEERS_ENABLED=true`, see docker-compose.e2e.yaml), so no real decode load
 * is needed. The presenter "step down sooner" lever is observable precisely
 * because it changes WHICH injected FPS level counts as pressure.
 *
 * ## The differential signal
 *
 * At an injected median FPS in the 24-30 HYSTERESIS BAND (BAND_FPS = 27):
 *   - NOT sharing → `decide_step` Holds (the band is the normal hysteresis dead
 *     zone), so NO off-budget tiles appear. This is the control.
 *   - SHARING     → `presenter_extra_shed_pressure` fires (median < FPS_STEP_UP
 *     while sharing), latches pressure, and sheds tiles → off-budget tiles
 *     appear. This is the presenter bias.
 *
 * The local sharer stays in the NORMAL grid (the split layout deliberately skips
 * SELF — `attendants.rs` `active_screen_sharer` excludes the local user), so the
 * `#grid-container` off-budget tiles are the assertion surface, identical to
 * `decode-budget.spec.ts`.
 *
 * MUTATION SENSITIVITY: if the presenter bias were removed, the SHARING phase
 * would also Hold at BAND_FPS (no off-budget tiles), so the "off-budget tiles
 * appear only while sharing" assertion fails.
 *
 * THRESHOLD/TIMING CONSTANTS below mirror
 * `dioxus-ui/src/components/decode_budget.rs`; keep in lockstep with a retune.
 */

// --- Mirrors of dioxus-ui/src/components/decode_budget.rs (keep in sync) ---
const BUDGET = {
  FPS_STEP_DOWN: 24, // normal step-down threshold
  FPS_STEP_UP: 30, // step-up threshold == presenter step-down while sharing
  SUSTAIN_SAMPLES: 3,
  STEP_DOWN_COOLDOWN_MS: 2000,
  STEP_UP_COOLDOWN_MS: 4000,
} as const;

// An injected FPS strictly INSIDE the 24-30 hysteresis band: above the normal
// step-down floor (so a non-presenter Holds) but below the presenter step-down
// threshold (so a sharing presenter sheds). Derived from the mirrored consts so
// a retune keeps it valid; assert the invariant defensively at runtime below.
const BAND_FPS = Math.round((BUDGET.FPS_STEP_DOWN + BUDGET.FPS_STEP_UP) / 2); // 27
// A healthy FPS comfortably above FPS_STEP_UP, used to confirm recovery on stop.
const HIGH_FPS = BUDGET.FPS_STEP_UP + 15; // 45
// Spacing between injected 1 Hz samples (just above the loop's 1 s bucket).
const INJECT_INTERVAL_MS = 1200;
const MOCK_PEERS = 12;
const COOLDOWN_DOWN_SAMPLES = Math.ceil(BUDGET.STEP_DOWN_COOLDOWN_MS / INJECT_INTERVAL_MS) + 1;
const COOLDOWN_UP_SAMPLES = Math.ceil(BUDGET.STEP_UP_COOLDOWN_MS / INJECT_INTERVAL_MS) + 1;
// One visible presenter shed proves the bias: SUSTAIN window + a couple of
// down-cooldown windows of CI headroom.
const MAX_SHED_SAMPLES = BUDGET.SUSTAIN_SAMPLES + 3 * COOLDOWN_DOWN_SAMPLES;
// One visible re-grow proves recovery on stop.
const MAX_REGROW_SAMPLES = BUDGET.SUSTAIN_SAMPLES + 3 * COOLDOWN_UP_SAMPLES;
// A short "should NOT shed" budget for the control phase: long enough that a
// real shed would have appeared, short enough to keep the test bounded.
const CONTROL_SAMPLES = BUDGET.SUSTAIN_SAMPLES + 2 * COOLDOWN_DOWN_SAMPLES;

// Deterministic getDisplayMedia() shim so a LOCAL screen share starts headlessly
// (mirrors screen-share-state.spec.ts). Installed before page load.
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

test.describe("Presenter-aware decode shedding (#1559)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, page, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
    // Install the getDisplayMedia shim before any navigation so the local
    // Share Screen click can start a synthetic share headlessly.
    await page.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `presenter_shed_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("presenter-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await waitForVisibleState(
      [
        { name: "join", locator: joinButton },
        { name: "grid", locator: grid },
      ],
      20_000,
    );
    if (which === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {});
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  async function setMockPeers(page: Page, count: number): Promise<boolean> {
    await page.locator(".video-controls-container").hover();
    const mockBtn = page
      .locator(".video-controls-container button")
      .filter({ has: page.locator('.tooltip:has-text("Mock Peers")') });
    if ((await mockBtn.count()) === 0) {
      return false;
    }
    await mockBtn.first().click();
    await expect(page.locator(".mock-peers-popover")).toBeVisible({ timeout: 5_000 });
    const input = page.locator("#mock-count-input");
    await input.fill(String(count));
    await input.dispatchEvent("input");
    await page.waitForTimeout(300);
    await page.locator("#grid-container").click({ position: { x: 10, y: 10 } });
    await expect(page.locator(".mock-peers-popover")).not.toBeVisible({ timeout: 3_000 });
    return true;
  }

  async function ensureAutoOverride(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.locator(".settings-nav-button").filter({ hasText: "Appearance" }).click();
    await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await page.locator('.device-settings-modal button[aria-label="Close settings"]').click();
    await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });
  }

  const decodedTiles = (page: Page) =>
    page.locator('#grid-container .grid-item[data-off-budget="false"]');
  const offBudgetTiles = (page: Page) => page.locator("#grid-container .grid-item.off-budget-tile");

  const hasInjectHook = (page: Page) =>
    page.evaluate(
      () =>
        typeof (window as unknown as { __videocall_inject_render_fps?: unknown })
          .__videocall_inject_render_fps === "function",
    );

  const injectFps = (page: Page, fps: number) =>
    page.evaluate(
      (v) =>
        (
          window as unknown as { __videocall_inject_render_fps: (n: number) => void }
        ).__videocall_inject_render_fps(v),
      fps,
    );

  // Inject `fps` up to `maxSamples` times, returning the off-budget tile count
  // once `predicate(offBudget)` holds or the budget is exhausted.
  const injectUntilOffBudget = async (
    page: Page,
    fps: number,
    maxSamples: number,
    predicate: (offBudget: number) => boolean,
  ): Promise<number> => {
    let off = await offBudgetTiles(page).count();
    for (let i = 0; i < maxSamples && !predicate(off); i++) {
      await injectFps(page, fps);
      await page.waitForTimeout(INJECT_INTERVAL_MS);
      off = await offBudgetTiles(page).count();
    }
    return off;
  };

  // The Screen Share control is `button.video-control-button` carrying a
  // `span.tooltip` whose `.tooltip-title` is "Screen share — Share Screen"
  // (idle) or "Screen share — Stop Screen Share" (active), plus a
  // `.tooltip-desc` description span — see video_control_buttons.rs
  // ScreenShareButton. Use a case-insensitive substring (Playwright's
  // `hasText: "Share Screen"`) that appears in the idle title but NOT in
  // the active title: "Stop Screen Share" does not contain "Share Screen"
  // as a substring (the order is reversed), and neither tooltip's
  // description text contains the phrase, so start/stop stay distinct.
  // An anchored regex (`/^Share Screen$/`) cannot be used here because the
  // tooltip now wraps the label in a `.tooltip-title` span plus a
  // `.tooltip-desc` span, so `.tooltip`'s full textContent concatenates
  // both children and can never equal just "Share Screen".
  const idleShareBtn = (page: Page) =>
    page.locator(".video-controls-container button.video-control-button", {
      has: page.locator(".tooltip", { hasText: "Share Screen" }),
    });
  const activeShareBtn = (page: Page) =>
    page.locator(".video-controls-container button.video-control-button", {
      has: page.locator(".tooltip", { hasText: "Stop Screen Share" }),
    });

  // Start the LOCAL screen share via the shimmed getDisplayMedia. Returns true
  // once the share button flips to its active ("Stop Screen Share") state, i.e.
  // is_sharing() == true.
  async function startLocalShare(page: Page): Promise<boolean> {
    await wakeControls(page);
    await page.waitForTimeout(400);
    const btn = idleShareBtn(page);
    if ((await btn.count()) === 0) {
      return false;
    }
    await expect(btn.first()).toBeVisible({ timeout: 10_000 });
    await btn.first().click();
    // The mock resolves quickly → StreamReady/Active (is_sharing() == true),
    // which flips the tooltip to "Stop Screen Share".
    try {
      await expect(activeShareBtn(page).first()).toBeVisible({ timeout: 10_000 });
      return true;
    } catch {
      return false;
    }
  }

  async function stopLocalShare(page: Page): Promise<void> {
    await wakeControls(page);
    await page.waitForTimeout(400);
    await activeShareBtn(page).first().click();
    // Back to idle: is_sharing() == false.
    await expect(idleShareBtn(page).first()).toBeVisible({ timeout: 10_000 });
  }

  // ──────────────────────────────────────────────────────────────────────
  // Presenter bias: at a 24-30 BAND FPS, off-budget tiles appear ONLY while
  // the local user is screen-sharing; not-sharing Holds (pressure-gated); and
  // stopping the share lets the budget recover (re-grow) the shed tiles.
  // ──────────────────────────────────────────────────────────────────────
  test("a sharing presenter sheds peer tiles in the hysteresis band; a non-presenter does not", async ({
    page,
  }) => {
    test.setTimeout(180_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    // Defensive: BAND_FPS must sit strictly inside the hysteresis band, else the
    // differential is meaningless.
    expect(BAND_FPS).toBeGreaterThan(BUDGET.FPS_STEP_DOWN);
    expect(BAND_FPS).toBeLessThan(BUDGET.FPS_STEP_UP);

    await joinMeeting(page, "band_shed");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }
    if (!(await hasInjectHook(page))) {
      test.skip(true, "window.__videocall_inject_render_fps not registered");
      return;
    }

    await ensureAutoOverride(page);

    // Baseline: all mock tiles decode, none off-budget.
    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0);

    // ---- CONTROL: NOT sharing, inject BAND_FPS → must HOLD (no shed). ----
    const offWhileNotSharing = await injectUntilOffBudget(
      page,
      BAND_FPS,
      CONTROL_SAMPLES,
      (off) => off > 0,
    );
    expect(
      offWhileNotSharing,
      "a non-presenter in the 24-30 hysteresis band must NOT shed peer tiles",
    ).toBe(0);
    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS);

    // ---- PRESENTER: start local share, inject BAND_FPS → MUST shed. ----
    const shared = await startLocalShare(page);
    if (!shared) {
      test.skip(true, "Local screen share could not start in this environment.");
      return;
    }

    const offWhileSharing = await injectUntilOffBudget(
      page,
      BAND_FPS,
      MAX_SHED_SAMPLES,
      (off) => off > 0,
    );
    expect(
      offWhileSharing,
      "a sharing presenter in the 24-30 band MUST shed at least one peer tile (#1559)",
    ).toBeGreaterThan(0);
    // The decoded count dropped below natural — peer-decode CPU was freed.
    const decodedWhileSharing = await decodedTiles(page).count();
    expect(decodedWhileSharing).toBeLessThan(MOCK_PEERS);
    expect(decodedWhileSharing).toBeGreaterThanOrEqual(1); // MIN_CAP floor

    // ---- RECOVER ON STOP: stop sharing + healthy FPS → tiles re-grow. ----
    await stopLocalShare(page);
    const offAfterStop = await injectUntilOffBudget(
      page,
      HIGH_FPS,
      MAX_REGROW_SAMPLES,
      (off) => off < offWhileSharing,
    );
    expect(
      offAfterStop,
      "once sharing stops the presenter ceiling lifts and shed tiles re-grow",
    ).toBeLessThan(offWhileSharing);
  });
});
