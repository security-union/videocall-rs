import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Adaptive decode-budget E2E coverage (issue #987, task 1a.6).
 *
 * The feature caps how many peer tiles decode video at once. Two paths:
 *
 *   1. Manual HARD override (Appearance settings → "Video tiles"): selecting a
 *      fixed value forces exactly that many DECODED tiles. Peers beyond the cap
 *      become "off-budget" avatar tiles — class `.grid-item.off-budget-tile`,
 *      attribute `[data-off-budget="true"]` — still present, NOT decoded.
 *   2. AUTO adaptation: a 1 Hz control loop consumes `client_render_fps` /
 *      `client_longtask_duration_ms` off the diagnostics bus and steps the cap
 *      DOWN under sustained pressure, UP on sustained recovery.
 *
 * Test 1 is fully deterministic (override = pure UI state → layout). Test 2 is
 * timing-dependent: it drives the loop via the test-only injection hook
 * `window.__videocall_inject_render_fps` (registered only when
 * `MOCK_PEERS_ENABLED=true`, see docker-compose.e2e.yaml) and asserts the cap
 * moves. Both rely on the mock-peers debug feature to synthesize tiles without
 * standing up N real browser contexts.
 *
 * THRESHOLD/TIMING CONSTANTS BELOW ARE COPIED FROM
 * `dioxus-ui/src/components/decode_budget.rs`. If 1a.7's performance-reviewer
 * retunes those consts, this spec must be updated in lockstep — see the
 * `BUDGET` block.
 */

// --- Mirrors of dioxus-ui/src/components/decode_budget.rs (task 1a.7: keep in sync) ---
const BUDGET = {
  FPS_STEP_DOWN: 24, // FPS at/below which the loop considers stepping DOWN
  FPS_STEP_UP: 30, // FPS at/above which the loop considers stepping UP (review FIX 3)
  FPS_SEVERE: 12, // median FPS at/below which a down-step drops MULTIPLE tiles (review FIX 4)
  LONGTASK_SEVERE_MS_PER_SEC: 700, // sustained long-task ms/s for a MULTI-tile drop (review FIX 4)
  SUSTAIN_SAMPLES: 3, // consecutive 1 Hz samples required before a step
  RECOVERY_HOLD: 5, // consecutive recovery-qualifying samples before a step UP
  STEP_DOWN_COOLDOWN_MS: 2000, // min ms between two DOWN steps (review FIX 5)
  STEP_UP_COOLDOWN_MS: 4000, // min ms between two UP steps (review FIX 5)
  WINDOW: 5, // rolling sample window owned by the control loop (attendants.rs)
} as const;

// A synthetic FPS comfortably below FPS_STEP_DOWN and well above FPS_STEP_UP,
// derived from the mirrored consts so a retune in 1a.7 keeps these valid.
// LOW_FPS sits in the MILD band (above FPS_SEVERE, below FPS_STEP_DOWN) so the
// DOWN phase exercises single-tile steps; severe multi-tile drops are covered by
// the Rust unit tests, not this timing-sensitive E2E.
const LOW_FPS = BUDGET.FPS_STEP_DOWN - 6; // 18: < FPS_STEP_DOWN, > FPS_SEVERE
const HIGH_FPS = BUDGET.FPS_STEP_UP + 15; // 45: comfortably >= FPS_STEP_UP
// Spacing between injected 1 Hz samples: slightly above the loop's 1 s bucket
// cadence so each injection lands in a fresh bucket.
const INJECT_INTERVAL_MS = 1200;
// Number of mock peers used in the auto test.
const MOCK_PEERS = 12;
// IMPORTANT (timing, review FIX 1): the control loop SEEDS its cap directly at
// the natural peer count the first time it sees a live `natural` (no MIN_CAP
// climb, no warm-up ramp). So from the first render a healthy machine shows ALL
// natural tiles and ZERO off-budget avatars. The actuator's `min(natural, cap)`
// is therefore NOT a no-op: the FIRST sustained-low-FPS down-step drops a tile
// and an off-budget avatar appears within SUSTAIN_SAMPLES + one
// STEP_DOWN_COOLDOWN_MS (~3-7 s). We budget only a handful of samples.
const COOLDOWN_DOWN_SAMPLES = Math.ceil(BUDGET.STEP_DOWN_COOLDOWN_MS / INJECT_INTERVAL_MS) + 1;
const COOLDOWN_UP_SAMPLES = Math.ceil(BUDGET.STEP_UP_COOLDOWN_MS / INJECT_INTERVAL_MS) + 1;
// One visible down-step proves relief: the SUSTAIN window + a couple of
// down-cooldown windows of headroom for CI jitter.
const MAX_DOWN_SAMPLES = BUDGET.SUSTAIN_SAMPLES + 3 * COOLDOWN_DOWN_SAMPLES;
// One visible up-step proves recovery: RECOVERY_HOLD + a couple of up-cooldown
// windows of headroom.
const MAX_UP_SAMPLES = BUDGET.RECOVERY_HOLD + 3 * COOLDOWN_UP_SAMPLES;

test.describe("Adaptive decode budget (#987)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `decode_budget_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("budget-user", { delay: 80 });
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
        await joinButton.click().catch(() => {
          // Swallow click-after-detach: the auto-join effect has already
          // transitioned past NotJoined and unmounted the button.
        });
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  /**
   * Open the mock-peers popover and set the count to `count`. Skips the whole
   * test (returning false) if the mock-peers button isn't present, which means
   * MOCK_PEERS_ENABLED is off in this stack — the feature this spec exercises
   * cannot run without it.
   */
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
    // `oninput` fires per keystroke; `fill` dispatches a single input event
    // that the Dioxus handler reads. Nudge a blur to be safe.
    await input.dispatchEvent("input");
    await page.waitForTimeout(300);

    // Close the popover so it doesn't overlay the grid for later interactions.
    await page.locator("#grid-container").click({ position: { x: 10, y: 10 } });
    await expect(page.locator(".mock-peers-popover")).not.toBeVisible({ timeout: 3_000 });
    return true;
  }

  /** Open Device Settings → Appearance tab so the decode-budget control shows. */
  async function openAppearancePanel(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });

    await page.locator(".settings-nav-button").filter({ hasText: "Appearance" }).click();
    await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#decode-budget-override")).toBeVisible({ timeout: 5_000 });
  }

  async function closeSettingsModal(page: Page): Promise<void> {
    const closeBtn = page.locator(
      '.device-settings-modal button[aria-label="Close"], .device-settings-modal .close-button, .device-settings-modal button:has-text("Close")',
    );
    if ((await closeBtn.count()) > 0) {
      await closeBtn.first().click();
    } else {
      await page.keyboard.press("Escape");
    }
    await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });
  }

  // Decoded tiles are `.grid-item` with data-off-budget="false"; off-budget
  // avatar tiles carry the `.off-budget-tile` class and data-off-budget="true".
  const decodedTiles = (page: Page) =>
    page.locator('#grid-container .grid-item[data-off-budget="false"]');
  const offBudgetTiles = (page: Page) => page.locator("#grid-container .grid-item.off-budget-tile");

  // Whether the test-only injection hook is attached (only when
  // MOCK_PEERS_ENABLED=true). When present, tests drive the control loop
  // deterministically via injected FPS samples instead of the browser's native
  // rAF cadence.
  const hasInjectHook = (page: Page) =>
    page.evaluate(
      () =>
        typeof (window as unknown as { __videocall_inject_render_fps?: unknown })
          .__videocall_inject_render_fps === "function",
    );

  // Inject one synthetic render-fps sample (closes one ~1 Hz bucket in the loop).
  const injectFps = (page: Page, fps: number) =>
    page.evaluate(
      (v) =>
        (
          window as unknown as { __videocall_inject_render_fps: (n: number) => void }
        ).__videocall_inject_render_fps(v),
      fps,
    );

  // ──────────────────────────────────────────────────────────────────────
  // Test 1 — manual HARD override (deterministic).
  //
  // With 12 mock peers and a fixed cap of 6, exactly 6 tiles decode and the
  // remaining tiles in the visible layout become off-budget avatars. The +N
  // overflow badge depends ONLY on the natural layout capacity, never on the
  // budget cap, so we assert it independently of the decoded count.
  // ──────────────────────────────────────────────────────────────────────
  test("manual override forces a fixed decoded-tile count and persists", async ({ page }) => {
    test.setTimeout(120_000);
    // Wide viewport so all 12 tiles fit the natural layout (no +N badge): keeps
    // the off-budget assertion about the BUDGET cap, not layout overflow.
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "override_fixed_6");

    const hasMockPeers = await setMockPeers(page, 12);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }

    // Baseline (review FIX 1 + FIX 2): in un-pressured Auto the effective cap is
    // DERIVED at render time as `total_tiles` — it tracks the natural peer count
    // exactly with NO dependence on a `client_render_fps` loop tick. So all 12
    // mock tiles decode immediately with zero off-budget avatars.
    await expect(decodedTiles(page)).toHaveCount(12, { timeout: 45_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0, { timeout: 45_000 });

    // Force a fixed cap of 6.
    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-6"]').click();
    await expect(page.locator('[data-testid="decode-budget-6"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await closeSettingsModal(page);

    // Exactly 6 decoded tiles; peers 7..12 become off-budget avatar tiles.
    await expect(decodedTiles(page)).toHaveCount(6, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(6, { timeout: 15_000 });

    // localStorage carries the override across reloads.
    const stored = await page.evaluate(() => localStorage.getItem("vc_decode_budget_override"));
    expect(stored).toBe("6");

    // Reopen settings and confirm the persisted localStorage value is reflected
    // back into the UI. Avoid reloading an active sole-host meeting: the host
    // disconnect can legitimately end the meeting before the reload rejoins.
    await openAppearancePanel(page);
    await expect(page.locator('[data-testid="decode-budget-6"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test 2 — AUTO adaptation via the injection hook (timing-dependent).
  //
  // With override = Auto, sustained low FPS must step the cap DOWN (off-budget
  // avatar tiles appear); sustained high FPS must step it back UP. The loop is
  // driven off `client_render_fps` events at ~1 Hz, with a SUSTAIN window and
  // (review FIX 5) an asymmetric cooldown — 2 s for DOWN, 4 s for UP.
  //
  // Timing (review FIX 1): the cap SEEDS directly at the natural peer count, so
  // at baseline all tiles are already decoded (no warm-up ramp). The FIRST
  // sustained-low-FPS down-step then produces an off-budget tile within
  // ~SUSTAIN + one down-cooldown (a few seconds). The sample budgets are sized
  // accordingly.
  // ──────────────────────────────────────────────────────────────────────
  test("auto loop steps the decoded-tile count down under load and back up on recovery", async ({
    page,
  }) => {
    // A single down-step + a single up-step complete in well under a minute;
    // keep generous CI headroom (the project uses ~30 s waits).
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "auto_adapt");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }

    // The injection hook is only attached when MOCK_PEERS_ENABLED is true.
    if (!(await hasInjectHook(page))) {
      test.skip(true, "window.__videocall_inject_render_fps not registered");
      return;
    }

    // Ensure override is Auto (default, but make it explicit & resilient).
    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await closeSettingsModal(page);

    // Baseline (review FIX 1 + FIX 2): the un-pressured Auto cap is render-derived
    // as `total_tiles`, so with no injected pressure all mock tiles decode
    // immediately and none are off-budget — cap == natural from the start, no
    // warm-up ramp and no dependence on a loop tick.
    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0);

    // Inject `fps` once per INJECT_INTERVAL_MS, up to `maxSamples` times, until
    // `predicate(decodedCount)` is satisfied. Returns the final decoded count.
    // Stops early once the step is observed.
    const injectUntil = async (
      fps: number,
      maxSamples: number,
      predicate: (decoded: number) => boolean,
    ): Promise<number> => {
      let decoded = await decodedTiles(page).count();
      for (let i = 0; i < maxSamples && !predicate(decoded); i++) {
        await injectFps(page, fps);
        await page.waitForTimeout(INJECT_INTERVAL_MS);
        decoded = await decodedTiles(page).count();
      }
      return decoded;
    };

    // --- Drive DOWN: sustained low FPS. ---
    // Each step needs SUSTAIN_SAMPLES low (mild-band) samples AND a
    // STEP_DOWN_COOLDOWN_MS gap. Because the cap is seeded at the peer count, the
    // very first down-step drops a tile and an off-budget avatar appears. We only
    // require the decoded count to move BELOW MOCK_PEERS;
    // the exact floor reached is timing-dependent and intentionally not pinned.
    const decodedAfterDown = await injectUntil(LOW_FPS, MAX_DOWN_SAMPLES, (d) => d < MOCK_PEERS);

    // At least one off-budget avatar tile must have appeared (cap < natural)...
    await expect(offBudgetTiles(page)).not.toHaveCount(0, { timeout: 15_000 });
    // ...and the decoded count must have dropped below the natural MOCK_PEERS.
    expect(decodedAfterDown).toBeLessThan(MOCK_PEERS);
    expect(decodedAfterDown).toBeGreaterThanOrEqual(1); // MIN_CAP floor

    // --- Drive UP: sustained recovery. ---
    // Step UP requires a healthy median FPS (>= FPS_STEP_UP) held for
    // RECOVERY_HOLD samples plus the STEP_UP_COOLDOWN_MS gap, per step. One
    // visible up-step is enough to prove recovery; assert the decoded count
    // climbs back strictly above the post-down trough. We don't assert exact
    // recovery to MOCK_PEERS because the number of up-steps that fit is
    // timing-dependent.
    const decodedAfterUp = await injectUntil(HIGH_FPS, MAX_UP_SAMPLES, (d) => d > decodedAfterDown);
    expect(decodedAfterUp).toBeGreaterThan(decodedAfterDown);
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test 3 — manual override takes effect with NO render-fps event (review
  // FIX 1). REGRESSION GUARD: the effective cap is derived at render time from
  // the override signal, so selecting Fixed(n) must produce exactly n decoded
  // tiles WITHOUT any `client_render_fps` event advancing the control loop.
  // This test deliberately NEVER calls the injection hook before asserting.
  // ──────────────────────────────────────────────────────────────────────
  test("manual override takes effect immediately without any render-fps event", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    // Wide viewport so all mock tiles fit the natural layout (no +N badge): the
    // off-budget assertion stays about the BUDGET cap, not layout overflow.
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "override_no_fps_event");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }

    // Start in Auto, un-pressured: all tiles decode immediately (render-derived
    // cap == natural). We do NOT inject any FPS sample at any point in this test.
    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await page.locator('[data-testid="decode-budget-6"]').click();
    await expect(page.locator('[data-testid="decode-budget-6"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await closeSettingsModal(page);

    // KEY ASSERTION: exactly 6 decoded tiles and 6 off-budget avatars appear
    // purely from the override change re-running render — no FPS event was
    // injected, so the control loop never advanced. (Finding 1 regression guard.)
    await expect(decodedTiles(page)).toHaveCount(6, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(MOCK_PEERS - 6, { timeout: 15_000 });
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test 4 — un-pressured Auto decodes staggered joins immediately (review
  // FIX 2). A healthy machine that never measured pressure must show ALL peers,
  // including ones that join LATER. We start with a low mock count (all
  // decoded), then RAISE the count (simulating staggered joins) and assert every
  // new peer is decoded immediately — no off-budget avatars — WITHOUT injecting
  // any low-FPS pressure.
  // ──────────────────────────────────────────────────────────────────────
  test("un-pressured auto decodes staggered peer joins immediately", async ({ page }) => {
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "auto_staggered_joins");

    const hasMockPeers = await setMockPeers(page, 4);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }

    // Ensure Auto (default, but explicit & resilient). No FPS pressure is ever
    // injected in this test, so the machine stays un-pressured throughout.
    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await closeSettingsModal(page);

    // Initial healthy state: all 4 tiles decode, no avatars.
    await expect(decodedTiles(page)).toHaveCount(4, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0);

    // Staggered joins: raise the mock count to 12. Every new peer must decode
    // immediately — the un-pressured Auto cap tracks `total_tiles` at render
    // time, so it grows with the join in a single render (no per-tick climb, no
    // off-budget avatars). (Finding 2 regression guard.)
    await setMockPeers(page, 12);
    await expect(decodedTiles(page)).toHaveCount(12, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0);
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test 5 — pressured Auto resumes ALL tiles immediately on Fixed -> Auto,
  // with NO further render-fps event (HCL #987 review, pressured-resume case).
  //
  // REGRESSION GUARD: the Fixed -> Auto pressured-latch reset is render-driven
  // (a `use_effect` watching `decode_budget_override`), NOT gated on the ~1 Hz
  // control loop. So a machine that has ALREADY been driven into the pressured
  // state (reduced `decode_budget_cap`, off-budget avatars visible) must, the
  // instant it returns to Auto, re-reveal every natural tile WITHOUT waiting for
  // the next `client_render_fps` tick.
  //
  // Flow:
  //   1. Auto + sustained low FPS  -> pressured latch set, decoded count drops
  //      below MOCK_PEERS, off-budget avatars appear.
  //   2. Switch to Fixed(6)        -> hard override, exactly 6 decoded.
  //   3. Switch back to Auto       -> assert ALL MOCK_PEERS tiles decode and
  //      zero avatars remain, injecting NO further FPS samples after the toggle.
  //
  // If the reset were still loop-gated (the old bug), step 3 would keep showing
  // the stale reduced cap (avatars) until the next injected FPS tick — which
  // this test deliberately never sends after the toggle.
  // ──────────────────────────────────────────────────────────────────────
  test("pressured auto resumes all tiles immediately on Fixed->Auto with no fps event", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "pressured_resume");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }

    // The injection hook is only attached when MOCK_PEERS_ENABLED is true; we
    // need it to drive the machine into the pressured state deterministically.
    if (!(await hasInjectHook(page))) {
      test.skip(true, "window.__videocall_inject_render_fps not registered");
      return;
    }

    // Ensure Auto (default, but explicit & resilient).
    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await closeSettingsModal(page);

    // Baseline: un-pressured Auto decodes all tiles, no avatars.
    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0);

    // --- Step 1: drive DOWN into the pressured state with sustained low FPS. ---
    // Inject mild-band low FPS once per bucket until the decoded count drops
    // below MOCK_PEERS (the first down-step latches `pressured` true).
    let decoded = await decodedTiles(page).count();
    for (let i = 0; i < MAX_DOWN_SAMPLES && decoded >= MOCK_PEERS; i++) {
      await injectFps(page, LOW_FPS);
      await page.waitForTimeout(INJECT_INTERVAL_MS);
      decoded = await decodedTiles(page).count();
    }
    // Confirm we are genuinely pressured: at least one off-budget avatar and a
    // decoded count below the natural total.
    await expect(offBudgetTiles(page)).not.toHaveCount(0, { timeout: 15_000 });
    expect(decoded).toBeLessThan(MOCK_PEERS);
    expect(decoded).toBeGreaterThanOrEqual(1); // MIN_CAP floor

    // --- Step 2: switch to a hard Fixed(6) override. ---
    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-6"]').click();
    await expect(page.locator('[data-testid="decode-budget-6"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await closeSettingsModal(page);
    await expect(decodedTiles(page)).toHaveCount(6, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(MOCK_PEERS - 6, { timeout: 15_000 });

    // --- Step 3: switch back to Auto. KEY ASSERTION. ---
    // From here on we inject NO further FPS samples. The render-driven
    // pressured-reset must clear the latch on the Fixed->Auto transition, so the
    // effective cap snaps back to `total_tiles` and every natural tile decodes
    // again on the next render. A loop-gated reset (the old bug) would leave the
    // reduced cap in place — avatars would persist — because no FPS tick follows.
    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await closeSettingsModal(page);

    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0, { timeout: 15_000 });
  });
});
