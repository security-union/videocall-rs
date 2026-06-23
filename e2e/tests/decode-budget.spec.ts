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
  // min ms between two DOWN steps (review FIX 5). Issue #1557 REUSES this exact
  // constant as the cascade SETTLE WINDOW: after received layers reach floor, the
  // loop waits STEP_DOWN_COOLDOWN_MS since the last REAL layer drop before
  // escalating from lowering layers to PAUSING (capping) tiles. One constant,
  // two roles — `settle_window_elapsed(now, last_layer_drop_ms)` in
  // decode_budget.rs compares against this same 2000 ms.
  STEP_DOWN_COOLDOWN_MS: 2000,
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

// --- Issue #1557 console-log witnesses (tier-before-pause ordering) ---
// The wasm control loop emits these `log::info!` lines (surfaced to the browser
// console via `console_log`; the e2e config sets logLevel:"info" so they are
// captured). They are the NATIVE-INTERLEAVING-PROOF distinguisher between the
// new tiered ordering and the old concurrent-drop: a healthy native rAF FPS
// sample emits NO DecodeBudget log at all, so only real down-pressure ticks add
// lines, and their ORDER is what the #1557 guard pins.
//   - LOWER_LAYER_LATCH_LOG: emitted on the FIRST Down edge (the pressured-latch
//     tick) while the cap is UNTOUCHED — this log did NOT EXIST before #1557, and
//     under the old code the cap dropped on this same tick.
//   - CAP_DOWN_LOG: emitted only when a tile is actually PAUSED (cap N->M). Under
//     #1557 this must come AFTER the lower-layer latch log, never on the same tick.
const LOWER_LAYER_LATCH_LOG = "DecodeBudget: cascade=lower_layer pressured_latch=true";
const CAP_DOWN_LOG_PREFIX = "DecodeBudget: cap ";
const CAP_DOWN_LOG_SUFFIX = "dir=down";

// Attach a console collector BEFORE the page's first navigation so the cascade
// logs emitted by the control loop are captured in order. Returns the live array.
function collectConsole(page: Page): string[] {
  const lines: string[] = [];
  page.on("console", (msg) => {
    lines.push(msg.text());
  });
  return lines;
}

// Index of the first captured line that is a real cap-drop ("cap N->M ... dir=down").
// Returns -1 if none yet. The substring match tolerates the variable N->M and the
// trailing telemetry fields.
const firstCapDownIndex = (lines: string[]): number =>
  lines.findIndex((l) => l.includes(CAP_DOWN_LOG_PREFIX) && l.includes(CAP_DOWN_LOG_SUFFIX));

// Index of the first lower-layer pressured-latch line. Returns -1 if none yet.
const firstLowerLayerLatchIndex = (lines: string[]): number =>
  lines.findIndex((l) => l.includes(LOWER_LAYER_LATCH_LOG));

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
    await page.locator('.device-settings-modal button[aria-label="Close settings"]').click();
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

  const injectUntilDecoded = async (
    page: Page,
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

  async function enterPressuredAutoState(page: Page): Promise<number> {
    const decodedAfterDown = await injectUntilDecoded(
      page,
      LOW_FPS,
      MAX_DOWN_SAMPLES,
      (d) => d < MOCK_PEERS,
    );

    await expect(offBudgetTiles(page)).not.toHaveCount(0, { timeout: 15_000 });
    expect(decodedAfterDown).toBeLessThan(MOCK_PEERS);
    expect(decodedAfterDown).toBeGreaterThanOrEqual(1);
    return decodedAfterDown;
  }

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
  //
  // Issue #1557 (tier-before-pause): the cap now drops ONE control-loop sample
  // LATER than it used to. The FIRST qualifying Down edge only lowers RECEIVED
  // simulcast layers (the cap is UNCHANGED → zero off-budget tiles on that tick);
  // the cap drops on the NEXT Down tick, once layers are at floor and the settle
  // window has elapsed. This test is intentionally written as a loop-until-shed
  // with MAX_DOWN_SAMPLES headroom, so the +1-tick shift is absorbed and the test
  // still proves the down→up round trip. It does NOT, however, DISTINGUISH
  // tier-before-pause from the old concurrent-drop ordering — it would pass under
  // both. The dedicated ordering guard is the test
  // "first Down edge lowers received layers before pausing any tile (#1557)"
  // below, which keys on the cascade=lower_layer log preceding the cap-drop.
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

    // --- Drive DOWN: sustained low FPS. ---
    // Each step needs SUSTAIN_SAMPLES low (mild-band) samples AND a
    // STEP_DOWN_COOLDOWN_MS gap. Because the cap is seeded at the peer count, the
    // very first down-step drops a tile and an off-budget avatar appears. We only
    // require the decoded count to move BELOW MOCK_PEERS;
    // the exact floor reached is timing-dependent and intentionally not pinned.
    const decodedAfterDown = await enterPressuredAutoState(page);

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
    const decodedAfterUp = await injectUntilDecoded(
      page,
      HIGH_FPS,
      MAX_UP_SAMPLES,
      (d) => d > decodedAfterDown,
    );
    expect(decodedAfterUp).toBeGreaterThan(decodedAfterDown);
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test 2b (#1557 ORDERING GUARD) — the first Down edge LOWERS RECEIVED
  // LAYERS before PAUSING any tile (tier-before-pause).
  //
  // WHAT CHANGED: previously, the first qualifying Down edge under sustained low
  // FPS dropped the tile cap IMMEDIATELY — an off-budget avatar appeared on the
  // very tick the pressure latch fired (layer-drop and cap-shed were CONCURRENT).
  // Now the loop is TIERED: the first Down edge drops only RECEIVED simulcast
  // layers (the cap is UNCHANGED → ZERO off-budget tiles on that tick), and the
  // cap is lowered (off-budget tiles appear) only on the NEXT Down tick, once
  // received layers are at floor AND the settle window (STEP_DOWN_COOLDOWN_MS
  // since the last real layer drop) has elapsed.
  //
  // HARNESS LIMITATION (investigated, documented): the mock-peer harness has NO
  // real connected peers — `mock-N` tiles are pure UI-layer placeholders
  // (attendants.rs builds them into the tile list; they never enter the client's
  // `PeerDecodeManager::connected_peers`). So the loop's layer-drop actuator,
  // `apply_local_cpu_pressure_congestion()` →
  // `seed_downlink_congestion_for_connected_peers`, iterates an EMPTY peer set
  // and returns false on the first call. That means there is NO real received
  // layer to drop here: `layers_at_floor` flips true on the first LowerLayer tick
  // and the settle clock is frozen at the loop-init `last_layer_drop_ms = 0.0`
  // (so `settle_window_elapsed(now, 0.0)` is already true against wall-clock
  // `now`). The NET observable in THIS harness is therefore exactly: the cap drop
  // is delayed by ~one control-loop sample versus pre-#1557 — the first Down edge
  // pauses NOTHING. We assert that observable two ways, the second being immune to
  // native rAF FPS interleaving:
  //
  //   (1) DOM invariant: at the tick the pressure latch fires (lower-layer stage),
  //       off-budget count is STILL 0 — no tile paused yet.
  //   (2) Console-log ordering (AUTHORITATIVE, native-interleaving-proof): the
  //       `cascade=lower_layer pressured_latch=true` log MUST appear, and MUST
  //       precede the first `cap N->M ... dir=down` log. Under the OLD
  //       concurrent-drop code the lower_layer log did not exist AND the cap
  //       dropped on the latch tick, so this ordering is UNSATISFIABLE under the
  //       old behavior — the test fails if tier-before-pause is reverted.
  //
  // This test is deliberately NOT written so it could pass under both orderings:
  // requiring the lower-layer latch log to EXIST and STRICTLY PRECEDE the cap-drop
  // log distinguishes tier-before-pause from concurrent-drop.
  // ──────────────────────────────────────────────────────────────────────
  test("first Down edge lowers received layers before pausing any tile (#1557)", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    // Attach the console collector BEFORE the first navigation so the latch-tick
    // cascade log is captured in emission order.
    const consoleLines = collectConsole(page);

    await joinMeeting(page, "tier_before_pause");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }
    if (!(await hasInjectHook(page))) {
      test.skip(true, "window.__videocall_inject_render_fps not registered");
      return;
    }

    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await closeSettingsModal(page);

    // Baseline: all tiles decode, none off-budget, and no cascade log yet.
    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0);
    expect(firstLowerLayerLatchIndex(consoleLines)).toBe(-1);
    expect(firstCapDownIndex(consoleLines)).toBe(-1);

    // --- Drive sustained LOW_FPS one sample at a time. We watch for the
    // lower-layer LATCH log to appear. The moment it does (the first Down edge),
    // assert the cap has NOT yet dropped: off-budget is STILL 0 and NO cap-drop
    // log has been emitted. This is the tier-before-pause invariant (1)+(2-part-a).
    //
    // The loop is robust to native rAF FPS interleaving: a healthy native sample
    // only delays the latch (it cannot manufacture an early cap drop), so the
    // assertions below remain valid however the native samples land. We keep
    // injecting up to MAX_DOWN_SAMPLES — the same headroom the AUTO test uses.
    let latchSeen = false;
    let offBudgetAtLatch = -1;
    let capDownIndexAtLatch = -1;
    for (let i = 0; i < MAX_DOWN_SAMPLES && !latchSeen; i++) {
      await injectFps(page, LOW_FPS);
      await page.waitForTimeout(INJECT_INTERVAL_MS);
      if (firstLowerLayerLatchIndex(consoleLines) !== -1) {
        latchSeen = true;
        // Read the cap-drop log index and the off-budget count AT the latch tick,
        // BEFORE injecting any further samples — so we observe the lower-layer
        // stage in isolation.
        capDownIndexAtLatch = firstCapDownIndex(consoleLines);
        offBudgetAtLatch = await offBudgetTiles(page).count();
      }
    }

    expect(
      latchSeen,
      "the lower-layer pressured-latch log must appear under sustained LOW_FPS",
    ).toBe(true);
    // INVARIANT (1): no tile paused on the latch (lower-layer) tick.
    expect(
      offBudgetAtLatch,
      "no off-budget tile may appear on the first Down edge — layers drop first, not the cap",
    ).toBe(0);
    // INVARIANT (2-part-a): the cap-drop log has NOT been emitted yet at the latch
    // tick. Under the old concurrent-drop code the cap dropped on this tick, so a
    // cap-drop log would already be present here.
    expect(
      capDownIndexAtLatch,
      "the cap must NOT have dropped on the first Down edge (no cap N->M dir=down log yet)",
    ).toBe(-1);

    // --- Continue injecting: the cap must drop on a SUBSEQUENT Down tick and
    // off-budget tiles must appear. We use the same loop-until-shed pattern as the
    // AUTO test, which is headroom-safe against native interleaving.
    const decodedAfterDown = await injectUntilDecoded(
      page,
      LOW_FPS,
      MAX_DOWN_SAMPLES,
      (d) => d < MOCK_PEERS,
    );
    await expect(offBudgetTiles(page)).not.toHaveCount(0, { timeout: 15_000 });
    expect(decodedAfterDown).toBeLessThan(MOCK_PEERS);
    expect(decodedAfterDown).toBeGreaterThanOrEqual(1); // MIN_CAP floor

    // INVARIANT (2-part-b, AUTHORITATIVE ORDERING): the cap-drop log now exists,
    // and the lower-layer latch log STRICTLY PRECEDES it. This is impossible under
    // concurrent-drop (no lower-layer log; cap dropped on the latch tick) — the
    // single strongest distinguisher, immune to native FPS interleaving.
    const latchIdx = firstLowerLayerLatchIndex(consoleLines);
    const capIdx = firstCapDownIndex(consoleLines);
    expect(latchIdx, "lower-layer latch log must be present").toBeGreaterThanOrEqual(0);
    expect(capIdx, "cap-drop log must be present once a tile is paused").toBeGreaterThanOrEqual(0);
    expect(
      latchIdx,
      "received layers must be lowered (cascade=lower_layer) BEFORE the cap drops (cap N->M dir=down)",
    ).toBeLessThan(capIdx);
  });

  test("pressured auto shows the paused-tiles banner and Show all videos reveals every tile", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "banner_show_all");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }
    if (!(await hasInjectHook(page))) {
      test.skip(true, "window.__videocall_inject_render_fps not registered");
      return;
    }

    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await closeSettingsModal(page);

    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0);

    await enterPressuredAutoState(page);

    const pausedCount = await offBudgetTiles(page).count();
    expect(pausedCount).toBeGreaterThan(0);

    const banner = page.locator('[data-testid="decode-budget-banner"]');
    await expect(banner).toBeVisible({ timeout: 10_000 });
    const pausedPlural = pausedCount === 1 ? "tile" : "tiles";
    await expect(banner).toContainText(`${pausedCount} ${pausedPlural} paused`);

    await page.locator('[data-testid="decode-budget-show-all"]').click();
    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0, { timeout: 15_000 });
    await expect(banner).not.toBeVisible({ timeout: 5_000 });

    // Issue #1466: the banner "Show all videos" button now pins the override to
    // the `All` variant (persisted as "all"), NOT a frozen integer cap. `All`
    // tracks the live natural count so newly-joining peers also decode. (Before
    // #1466 this wrote the current natural count as a bare integer, e.g. "12".)
    const stored = await page.evaluate(() => localStorage.getItem("vc_decode_budget_override"));
    expect(stored).toBe("all");
  });

  test("pressured auto banner dismiss button hides the current episode", async ({ page }) => {
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "banner_dismiss");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }
    if (!(await hasInjectHook(page))) {
      test.skip(true, "window.__videocall_inject_render_fps not registered");
      return;
    }

    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await closeSettingsModal(page);

    await enterPressuredAutoState(page);

    const banner = page.locator('[data-testid="decode-budget-banner"]');
    await expect(banner).toBeVisible({ timeout: 10_000 });

    await page.locator('[data-testid="decode-budget-dismiss"]').click();
    await expect(banner).not.toBeVisible({ timeout: 5_000 });
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test A (#1142 FINAL DESIGN) — THE GAP-1 REGRESSION GUARD.
  //
  // The persistent "N videos paused" pill
  // (`data-testid="decode-paused-pill"`, see
  // dioxus-ui/src/components/decode_paused_pill.rs) and the onset BANNER
  // (`decode-budget-banner`) are MUTUALLY EXCLUSIVE: the pill suppresses while
  // the banner is on screen and TAKES OVER the instant the banner hides —
  // including when the banner is DISMISSED by the user. This pins issue #1142
  // Gap 1: dismissing the alert banner must NOT leave the user in a silent
  // paused state. The pill is the always-available signpost that survives the
  // dismiss; it has NO back-off and NO dismiss button.
  //
  // WHY this is the high-value guard: if the pill used the OLD shadow-damper
  // (an approximation that could not see the banner's dismiss), after dismiss
  // the pill would stay suppressed and the paused state would go silent — the
  // very Gap-1 defect. The fix wires the banner's TRUE on-screen visibility
  // into a shared `banner_on_screen` signal the pill reads; the eligibility
  // streak runs purely on `avatar_count` and is NOT disturbed by the banner, so
  // when the banner hides an already-eligible pill appears immediately. The
  // assertions below are ORDERED so that exact contract is what's pinned:
  //   banner visible + pill suppressed → banner dismissed + tiles STILL paused
  //   → pill now visible. Revert the Gap-1 fix and step 4 fails.
  //
  // Timing: the pill needs PILL_APPEAR_MS (2 s) of sustained off-budget tiles
  // before it is eligible, then a 1 Hz poll publishes its output. By the time
  // `enterPressuredAutoState` has the banner up, tiles have been paused well
  // past 2 s, so the pill is already eligible-but-suppressed; after dismiss it
  // surfaces on the next poll. We keep injecting LOW_FPS across the dismiss +
  // poll window so the control loop does not let pressure decay and unpause the
  // tiles before the pill assertion (no pass-by-timing-luck).
  // ──────────────────────────────────────────────────────────────────────
  test("dismissing the banner under sustained pressure surfaces the persistent paused pill", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "pill_after_dismiss");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }
    if (!(await hasInjectHook(page))) {
      test.skip(true, "window.__videocall_inject_render_fps not registered");
      return;
    }

    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await closeSettingsModal(page);

    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0);

    await enterPressuredAutoState(page);

    const banner = page.locator('[data-testid="decode-budget-banner"]');
    const pill = page.locator('[data-testid="decode-paused-pill"]');

    // The banner fires on onset; while it is on screen the pill is SUPPRESSED
    // (the two are mutually exclusive).
    await expect(banner).toBeVisible({ timeout: 10_000 });
    await expect(pill).toBeHidden();

    // Dismiss the banner. The banner hides; the pill's eligibility streak is
    // untouched by the dismiss (it ran purely on avatar_count), so it should
    // now take over.
    await page.locator('[data-testid="decode-budget-dismiss"]').click();
    await expect(banner).not.toBeVisible({ timeout: 5_000 });

    // KEY ASSERTION (Gap-1 fix). Keep injecting LOW_FPS so tiles stay paused
    // across the ~1 Hz poll window, then prove tiles are STILL paused and the
    // pill has surfaced. Generous timeout covers the poll cadence; the
    // interleaved injections stop pressure decaying out from under the assert.
    const pillAppeared = expect(pill).toBeVisible({ timeout: 15_000 });
    for (let i = 0; i < 8; i++) {
      await injectFps(page, LOW_FPS);
      await page.waitForTimeout(INJECT_INTERVAL_MS);
    }
    await pillAppeared;

    // Tiles must STILL be paused at the moment the pill is up — the pill is the
    // signpost for the live paused state, not a stale artifact of the dismiss.
    await expect(offBudgetTiles(page)).not.toHaveCount(0);

    // The pill names the paused count ("N videos paused" on this desktop width).
    await expect(pill).toContainText("paused");
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test B (#1142 FINAL DESIGN) — the paused pill's "Show all" reveals every
  // tile and persists the `All` override.
  //
  // The pill's action button (`decode-paused-pill-show-all`, visible text
  // "Show all") takes the SAME escape-hatch path as the banner's Show-all and
  // the appearance panel: it sets the decode-budget override to `All` (issue
  // #1466) and persists the literal "all" to localStorage
  // (`vc_decode_budget_override`). `All` tracks the live natural count, so every
  // present peer decodes and stays decoded as peers join. Once avatar_count hits
  // 0 the pill settles back to Hidden on its own — it has no dismiss.
  //
  // We reach the pill via the deterministic dismiss-then-pill route proven in
  // Test A (banner up → dismiss → pill takes over), then click the pill's
  // action and assert: all tiles decode, the pill auto-hides, and the override
  // persisted as "all" (mirrors the banner Show-all test's assertion exactly).
  // ──────────────────────────────────────────────────────────────────────
  test("the paused pill Show all reveals every tile and persists the All override", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "pill_show_all");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }
    if (!(await hasInjectHook(page))) {
      test.skip(true, "window.__videocall_inject_render_fps not registered");
      return;
    }

    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await closeSettingsModal(page);

    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0);

    await enterPressuredAutoState(page);

    const banner = page.locator('[data-testid="decode-budget-banner"]');
    const pill = page.locator('[data-testid="decode-paused-pill"]');

    // Deterministic route to the pill: surface the banner, dismiss it, let the
    // pill take over (the path Test A pins). Keep injecting LOW_FPS so tiles
    // stay paused across the dismiss + poll window.
    await expect(banner).toBeVisible({ timeout: 10_000 });
    await page.locator('[data-testid="decode-budget-dismiss"]').click();
    await expect(banner).not.toBeVisible({ timeout: 5_000 });

    const pillAppeared = expect(pill).toBeVisible({ timeout: 15_000 });
    for (let i = 0; i < 8; i++) {
      await injectFps(page, LOW_FPS);
      await page.waitForTimeout(INJECT_INTERVAL_MS);
    }
    await pillAppeared;

    // Click the pill's "Show all" — override → All.
    await page.locator('[data-testid="decode-paused-pill-show-all"]').click();

    // Every tile decodes again; no off-budget avatars remain. `All` tracks the
    // live natural count, so this holds even though we never stopped the
    // (now-ignored) low-FPS pressure injected above.
    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0, { timeout: 15_000 });

    // avatar_count → 0, so the pill settles to Hidden on its own (no dismiss).
    await expect(pill).not.toBeVisible({ timeout: 10_000 });

    // Persistence: the override is pinned to the `All` variant, persisted as the
    // literal "all" (mirror of the banner Show-all test's assertion).
    const stored = await page.evaluate(() => localStorage.getItem("vc_decode_budget_override"));
    expect(stored).toBe("all");
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
    await expect(decodedTiles(page)).toHaveCount(12, { timeout: 45_000 });
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

  // ──────────────────────────────────────────────────────────────────────
  // Test 7 — persistent recovery toggle, two-way semantics (issue #1466).
  //
  // `data-testid="decode-budget-show-all-persistent"` is the always-reachable
  // GLOBAL recovery control in Settings — independent of the transient banner
  // (no "tiles paused" episode is required to reach it). It is a TOGGLE:
  //   - when the override is Auto  → label "Show all videos", click sets
  //     override = All  (persisted "all");
  //   - when the override is non-Auto (All or any Fixed) → label "Back to
  //     automatic", click sets override = Auto (persisted "auto").
  //
  // We assert the label flips and the localStorage value round-trips for the
  // full Auto → All → Auto cycle. Mock peers (no FPS pressure needed) make the
  // tile-count side deterministic too: All decodes everything, Auto un-pressured
  // also decodes everything, so we additionally confirm all tiles stay decoded.
  //
  // Note (#1466 S1): the segmented picker no longer has an "All" option — the
  // toggle is now the sole control that sets the `All` override — so this test
  // proves the variant via the persisted "all" literal, not a picker selection.
  // ──────────────────────────────────────────────────────────────────────
  test("persistent show-all toggle flips between All and Auto and persists", async ({ page }) => {
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "persistent_toggle");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }

    // Start from a known Auto baseline.
    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-auto"]').click();
    await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );

    const toggle = page.locator('[data-testid="decode-budget-show-all-persistent"]');
    await expect(toggle).toBeVisible({ timeout: 5_000 });

    // In Auto the toggle offers "Show all videos".
    await expect(toggle).toHaveText("Show all videos");

    // Click once → override becomes All, persisted "all", label flips to the
    // recovery wording. (The picker has no "All" option (#1466 S1); the
    // persisted literal is the source of truth for the `All` variant.)
    await toggle.click();
    await expect(toggle).toHaveText("Back to automatic");
    let stored = await page.evaluate(() => localStorage.getItem("vc_decode_budget_override"));
    expect(stored).toBe("all");

    // Click again → override returns to Auto, persisted "auto", label flips back.
    await toggle.click();
    await expect(toggle).toHaveText("Show all videos");
    await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    stored = await page.evaluate(() => localStorage.getItem("vc_decode_budget_override"));
    expect(stored).toBe("auto");

    await closeSettingsModal(page);

    // Un-pressured throughout (no FPS injected), so every tile stays decoded
    // regardless of the toggle position.
    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0, { timeout: 15_000 });
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test 8 — persistent toggle shows "Back to automatic" when a Fixed cap is
  // active (issue #1466 toggle wording is non-Auto-wide, not All-specific).
  //
  // The toggle's label depends only on `override != Auto`, so a Fixed(6) cap
  // must also surface "Back to automatic", and clicking it must return to Auto
  // (persisted "auto") and re-reveal all tiles — proving the recovery hatch is
  // reachable from a Fixed cap, not just from All.
  // ──────────────────────────────────────────────────────────────────────
  test("persistent toggle recovers from a Fixed cap back to Auto", async ({ page }) => {
    test.setTimeout(120_000);
    await page.setViewportSize({ width: 1920, height: 1080 });

    await joinMeeting(page, "persistent_toggle_fixed");

    const hasMockPeers = await setMockPeers(page, MOCK_PEERS);
    if (!hasMockPeers) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; cannot synthesize peer tiles");
      return;
    }

    await openAppearancePanel(page);
    await page.locator('[data-testid="decode-budget-6"]').click();
    await expect(page.locator('[data-testid="decode-budget-6"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );

    // A Fixed cap is non-Auto, so the toggle offers the recovery wording.
    const toggle = page.locator('[data-testid="decode-budget-show-all-persistent"]');
    await expect(toggle).toHaveText("Back to automatic");

    await closeSettingsModal(page);
    await expect(decodedTiles(page)).toHaveCount(6, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(MOCK_PEERS - 6, { timeout: 15_000 });

    // Click the recovery toggle: override → Auto, persisted "auto", all tiles
    // decode again (un-pressured Auto cap == natural).
    await openAppearancePanel(page);
    await toggle.click();
    await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    const stored = await page.evaluate(() => localStorage.getItem("vc_decode_budget_override"));
    expect(stored).toBe("auto");
    await closeSettingsModal(page);

    await expect(decodedTiles(page)).toHaveCount(MOCK_PEERS, { timeout: 15_000 });
    await expect(offBudgetTiles(page)).toHaveCount(0, { timeout: 15_000 });
  });
});
