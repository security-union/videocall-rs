import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E coverage for the in-meeting "Performance" settings panel (issue #961,
 * branch `feat/961-performance-quality-thresholds`).
 *
 * ─── Feature under test ──────────────────────────────────────────────────────
 * A new "Performance" tab inside the in-meeting device-settings modal. It renders
 * three per-stream "Thresholds" sections (Video / Audio / Screen Share), each of
 * which contains, side by side:
 *   1. That stream's live VU-meter needle gauge with a live numeric readout
 *      (`perf-vu-{stream}-readout`). Readouts show `{w}x{h}·{fps}fps·{kbps}kbps`
 *      for video/screen, `{kbps} kbps` for audio, and the screen gauge shows the
 *      literal placeholder "Not sharing" while no screen share is active.
 *   2. A dual-thumb range slider (two overlaid native `<input type="range">`:
 *      min/left = worst quality, max/right = best quality) plus a per-stream
 *      "?" help button and an "Auto" toggle button (`aria-pressed`).
 *
 * Auto is the default for all three streams. While Auto is ON the toggle is
 * `aria-pressed="true"` (green `is-active`) AND the slider is **fully
 * interactive** (NOT disabled) with both thumbs pinned at the extremes (min at
 * position 0, max at the top position) so it visibly shows the full ladder span.
 * Turning Auto off flips the toggle to `aria-pressed="false"`; dragging a thumb
 * inward also turns Auto off. There is no `disabled` state on the inputs.
 *
 * Choices persist to `localStorage["vc_performance_quality"]` as a
 * `PerformancePreference` JSON object:
 *   { video_max, video_min, audio_max, audio_min, screen_max, screen_min:
 *     usize|null, video_auto, audio_auto, screen_auto: bool }
 * Default = all-Auto (every `*_auto` true, every bound null).
 *
 * ─── Stable testids (from the Rust implementation) ───────────────────────────
 *   Tab/nav/panel:  settings-tab-performance (id) · settings-nav-performance
 *                   (data-testid, role="tab") · settings-panel-performance (id)
 *   VU gauges:      perf-vu-video / -audio / -screen (one per section)
 *                   readouts: perf-vu-{video,audio,screen}-readout (by id)
 *   Range inputs:   perf-{video,audio,screen}-range-min / -range-max
 *                   (native <input type=range>; always enabled)
 *   Auto toggles:   perf-{video,audio,screen}-auto (have aria-pressed)
 *   Help buttons:   perf-{video,audio,screen}-help (aria-expanded popover)
 *   Range value:    perf-{video,audio,screen}-range-value
 *   Fixed badge:    perf-{video,audio,screen}-fixed-badge
 *
 * ─── How the panel is reached ────────────────────────────────────────────────
 * The Performance tab lives inside the in-meeting device-settings modal, so each
 * test must be in a real meeting room first. We reuse the PROVEN in-meeting
 * modal flow from `settings-modal.spec.ts`: inject the session cookie
 * (helpers/auth.ts) on the default `dioxus`-project `page` (whose Chromium flags
 * already include `--use-fake-device-for-media-stream` so the camera produces a
 * synthetic stream), drive the home-page meeting form, click through
 * "Start/Join Meeting" to `#grid-container`, then open the gear
 * (`[data-testid="open-settings"]`) → `.device-settings-modal` and click the
 * Performance tab.
 *
 * ─── Local vs CI ─────────────────────────────────────────────────────────────
 * Reaching the in-meeting settings modal requires a real meeting-room
 * connection, which needs the full compose stack (Dioxus UI :3001 + actix-api +
 * meeting-api + NATS) up. EVERY assertion below therefore runs only against the
 * full stack in CI / `make e2e`; on a laptop without the Docker stack the join
 * times out at `#grid-container` — the expected harness gap, not a test bug.
 * All async-dependent assertions (localStorage write, gauge readout updates) use
 * auto-retrying matchers (`expect.poll` / `toHaveAttribute` / `toHaveText`).
 */

const PERF_PREF_KEY = "vc_performance_quality";

/** Shape of the persisted PerformancePreference (mirrors the Rust struct). */
interface PerformancePreference {
  video_max: number | null;
  video_min: number | null;
  audio_max: number | null;
  audio_min: number | null;
  screen_max: number | null;
  screen_min: number | null;
  video_auto: boolean;
  audio_auto: boolean;
  screen_auto: boolean;
}

/**
 * Drive the home page → meeting form → into the grid, mirroring the proven
 * in-meeting flow used by `settings-modal.spec.ts`. Resolves once
 * `#grid-container` is visible (the marker that the in-meeting UI is up).
 */
async function joinMeeting(page: Page, testLabel: string): Promise<void> {
  const meetingId = `e2e_perf_${testLabel}_${Date.now()}`;

  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially("perf-user", { delay: 80 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

  // The meeting page may auto-join (grid appears directly) or present a
  // Start/Join button. Race both so either path lands us in the grid.
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");
  const which = await Promise.race([
    joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
  ]);
  if (which === "join") {
    if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
      // The button is not interactive the instant it renders; a brief settle
      // mirrors the proven helper. Swallow click-after-detach in case auto-join
      // already transitioned past the button.
      await page.waitForTimeout(1000);
      await joinButton.click().catch(() => {
        /* auto-join already unmounted the button */
      });
    }
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/** Open the in-meeting device-settings modal via the toolbar gear. */
async function openSettingsModal(page: Page): Promise<void> {
  // Reveal the action bar in case autohide is active, then click the gear.
  await page.locator(".video-controls-container").hover();
  await page.locator('[data-testid="open-settings"]').click();
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
}

/** Open the modal (if needed) and switch to the Performance tab. */
async function openPerformanceTab(page: Page): Promise<void> {
  await openSettingsModal(page);
  // The nav button carries role="tab" + data-testid="settings-nav-performance".
  await page.locator('[data-testid="settings-nav-performance"]').click();
  await expect(page.locator("#settings-panel-performance")).toBeVisible({ timeout: 5_000 });
}

/** Read and parse localStorage["vc_performance_quality"], or null if unset. */
async function readPerfPref(page: Page): Promise<PerformancePreference | null> {
  const raw = await page.evaluate((key) => localStorage.getItem(key), PERF_PREF_KEY);
  if (!raw) {
    return null;
  }
  return JSON.parse(raw) as PerformancePreference;
}

/**
 * Set a native `<input type="range">` (matched by testid) to `value` and fire
 * the input + change events Dioxus listens for. Playwright's `fill` already
 * dispatches `input`/`change` for range inputs, but we also dispatch them
 * explicitly so the controlled-component handler runs deterministically.
 */
async function setRangeValue(page: Page, testid: string, value: number): Promise<void> {
  const input = page.locator(`[data-testid="${testid}"]`);
  await input.evaluate((el, v) => {
    const range = el as HTMLInputElement;
    range.value = String(v);
    range.dispatchEvent(new Event("input", { bubbles: true }));
    range.dispatchEvent(new Event("change", { bubbles: true }));
  }, value);
}

test.describe("Performance settings panel (#961)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("panel renders: 3 VU gauges, 3 threshold groups, all Auto by default (enabled sliders, thumbs at extremes)", async ({
    page,
  }) => {
    await joinMeeting(page, "render");
    await openPerformanceTab(page);

    const panel = page.locator("#settings-panel-performance");

    // ── Three live VU gauges visible (one per stream section) ──
    await expect(panel.locator('[data-testid="perf-vu-video"]')).toBeVisible();
    await expect(panel.locator('[data-testid="perf-vu-audio"]')).toBeVisible();
    await expect(panel.locator('[data-testid="perf-vu-screen"]')).toBeVisible();

    // ── Three threshold groups: assert each stream's slider pair + Auto + help ──
    for (const stream of ["video", "audio", "screen"] as const) {
      await expect(panel.locator(`[data-testid="perf-${stream}-range-min"]`)).toBeVisible();
      await expect(panel.locator(`[data-testid="perf-${stream}-range-max"]`)).toBeVisible();
      await expect(panel.locator(`[data-testid="perf-${stream}-auto"]`)).toBeVisible();
      await expect(panel.locator(`[data-testid="perf-${stream}-help"]`)).toBeVisible();
    }

    // ── Default = all-Auto: every toggle pressed, but sliders stay ENABLED with
    //    both thumbs pinned at the extremes (min at 0, max at the top position).
    const topPos = { video: 7, audio: 3, screen: 2 } as const;
    for (const stream of ["video", "audio", "screen"] as const) {
      await expect(panel.locator(`[data-testid="perf-${stream}-auto"]`)).toHaveAttribute(
        "aria-pressed",
        "true",
      );
      const minInput = panel.locator(`[data-testid="perf-${stream}-range-min"]`);
      const maxInput = panel.locator(`[data-testid="perf-${stream}-range-max"]`);
      // Sliders are interactive while Auto (the refinement: never disabled).
      await expect(minInput).toBeEnabled();
      await expect(maxInput).toBeEnabled();
      // Thumbs span the full ladder: min at position 0, max at the top position.
      await expect(minInput).toHaveValue("0");
      await expect(maxInput).toHaveValue(String(topPos[stream]));
    }
  });

  test("Auto toggle: flips aria-pressed and snaps thumbs to extremes; slider stays enabled throughout", async ({
    page,
  }) => {
    await joinMeeting(page, "auto_toggle");
    await openPerformanceTab(page);

    const panel = page.locator("#settings-panel-performance");
    const autoBtn = panel.locator('[data-testid="perf-video-auto"]');
    const minInput = panel.locator('[data-testid="perf-video-range-min"]');
    const maxInput = panel.locator('[data-testid="perf-video-range-max"]');

    // Default: Auto on → pressed, inputs ENABLED, thumbs at extremes (0 and 7).
    await expect(autoBtn).toHaveAttribute("aria-pressed", "true");
    await expect(minInput).toBeEnabled();
    await expect(maxInput).toBeEnabled();
    await expect(minInput).toHaveValue("0");
    await expect(maxInput).toHaveValue("7");

    // Turn Auto OFF via the button → pressed flips false; inputs stay enabled.
    await autoBtn.click();
    await expect(autoBtn).toHaveAttribute("aria-pressed", "false");
    await expect(minInput).toBeEnabled();
    await expect(maxInput).toBeEnabled();

    // Drag a thumb inward → range narrows and Auto remains off.
    await setRangeValue(page, "perf-video-range-max", 5);
    await expect(autoBtn).toHaveAttribute("aria-pressed", "false");
    await expect(maxInput).toHaveValue("5");

    // Turn Auto back ON → pressed true, thumbs snap back to the extremes, and the
    // inputs are STILL enabled (never disabled by Auto).
    await autoBtn.click();
    await expect(autoBtn).toHaveAttribute("aria-pressed", "true");
    await expect(minInput).toBeEnabled();
    await expect(maxInput).toBeEnabled();
    await expect(minInput).toHaveValue("0");
    await expect(maxInput).toHaveValue("7");
  });

  test("help popover: '?' opens an explanation, Escape and outside-click dismiss it", async ({
    page,
  }) => {
    await joinMeeting(page, "help_popover");
    await openPerformanceTab(page);

    const panel = page.locator("#settings-panel-performance");
    const helpBtn = panel.locator('[data-testid="perf-video-help"]');
    const popover = page.locator("#perf-video-help-popover");

    // Closed by default.
    await expect(helpBtn).toHaveAttribute("aria-expanded", "false");
    await expect(popover).toHaveCount(0);

    // Click "?" → popover opens with the explanation copy.
    await helpBtn.click();
    await expect(helpBtn).toHaveAttribute("aria-expanded", "true");
    await expect(popover).toBeVisible();
    await expect(popover).toContainText(/best.*worst.*video quality/i);

    // Escape closes it.
    await page.keyboard.press("Escape");
    await expect(helpBtn).toHaveAttribute("aria-expanded", "false");
    await expect(popover).toHaveCount(0);

    // Re-open, then outside-click dismisses.
    await helpBtn.click();
    await expect(popover).toBeVisible();
    await panel.locator('[data-testid="perf-video-range-value"]').click();
    await expect(helpBtn).toHaveAttribute("aria-expanded", "false");
    await expect(popover).toHaveCount(0);
  });

  test("setting a video threshold persists to localStorage and restores after reload", async ({
    page,
  }) => {
    await joinMeeting(page, "persist");
    await openPerformanceTab(page);

    const panel = page.locator("#settings-panel-performance");

    // Turn video Auto off so the stream is in manual mode (bounds persist).
    await panel.locator('[data-testid="perf-video-auto"]').click();
    await expect(panel.locator('[data-testid="perf-video-auto"]')).toHaveAttribute(
      "aria-pressed",
      "false",
    );
    await expect(panel.locator('[data-testid="perf-video-range-min"]')).toBeEnabled();

    // Move both thumbs inward to a non-extreme range. Slider positions are in
    // "position space" (0 = worst/left … max = best/right); 8 video tiers ⇒
    // positions 0..7. Pick min=position 2, max=position 5 — strictly interior so
    // neither bound is an Auto-extreme (the panel maps an extreme back to null).
    await setRangeValue(page, "perf-video-range-min", 2);
    await setRangeValue(page, "perf-video-range-max", 5);

    // localStorage must reflect: video no longer Auto, and concrete bounds set.
    // Poll because the controlled-component write is async to the event.
    await expect
      .poll(async () => (await readPerfPref(page))?.video_auto, { timeout: 10_000 })
      .toBe(false);

    const pref = await readPerfPref(page);
    expect(pref, "vc_performance_quality must be written").not.toBeNull();
    // With video off Auto and interior thumbs, both bounds are concrete (the
    // max/best maps to a lower tier index, the min/worst to a higher index).
    expect(pref?.video_max, "video_max (best bound) is a concrete tier index").not.toBeNull();
    expect(pref?.video_min, "video_min (worst bound) is a concrete tier index").not.toBeNull();
    // The other streams stay on Auto (we only touched video).
    expect(pref?.audio_auto).toBe(true);
    expect(pref?.screen_auto).toBe(true);

    // Capture the visible range text to compare after reload.
    const rangeValueBefore = await panel
      .locator('[data-testid="perf-video-range-value"]')
      .textContent();
    expect(rangeValueBefore).not.toBeNull();
    // While Auto the text is literally "Range: Auto"; once a manual bound is set
    // it shows a concrete tier range, so it must NOT be the Auto placeholder.
    expect(rangeValueBefore).not.toContain("Auto");

    // ── Reload and rejoin; the preference must restore from localStorage ──
    await page.reload();
    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await page.waitForTimeout(1000);
        await joinButton.click().catch(() => {
          /* auto-join already unmounted the button */
        });
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });

    await openPerformanceTab(page);
    const panelAfter = page.locator("#settings-panel-performance");

    // Auto still off after restore.
    await expect(panelAfter.locator('[data-testid="perf-video-auto"]')).toHaveAttribute(
      "aria-pressed",
      "false",
    );
    await expect(panelAfter.locator('[data-testid="perf-video-range-min"]')).toBeEnabled();

    // The restored range text matches what was set before the reload.
    await expect(panelAfter.locator('[data-testid="perf-video-range-value"]')).toHaveText(
      rangeValueBefore as string,
    );
    // And localStorage still carries the persisted, non-Auto video preference.
    await expect
      .poll(async () => (await readPerfPref(page))?.video_auto, { timeout: 10_000 })
      .toBe(false);
  });

  test("VU gauges are live: video readout shows a real value, screen shows 'Not sharing'", async ({
    page,
  }) => {
    await joinMeeting(page, "vu_live");
    await openPerformanceTab(page);

    const panel = page.locator("#settings-panel-performance");

    // The video gauge readout is updated by a ~4 Hz rAF loop from the live
    // encoder snapshot. With the fake camera producing a synthetic stream, the
    // readout should leave any placeholder and show a real
    // `{w}x{h}·{fps}fps·{kbps}kbps` line. Match the resolution/kbps shape
    // (digits + "kbps") rather than an exact resolution, which can vary.
    await expect
      .poll(async () => panel.locator("#perf-vu-video-readout").textContent(), { timeout: 15_000 })
      .toMatch(/\d+x\d+.*kbps/);

    // No screen share is active, so the screen gauge shows the literal
    // "Not sharing" placeholder (SCREEN_EMPTY_READOUT in the Rust impl).
    await expect(panel.locator("#perf-vu-screen-readout")).toHaveText("Not sharing", {
      timeout: 15_000,
    });
  });

  test("fixed badge appears when a stream's two thumbs collapse to the same tier", async ({
    page,
  }) => {
    await joinMeeting(page, "fixed_badge");
    await openPerformanceTab(page);

    const panel = page.locator("#settings-panel-performance");

    // Turn video Auto off so the stream is in manual mode (bounds persist).
    await panel.locator('[data-testid="perf-video-auto"]').click();
    await expect(panel.locator('[data-testid="perf-video-auto"]')).toHaveAttribute(
      "aria-pressed",
      "false",
    );
    await expect(panel.locator('[data-testid="perf-video-range-min"]')).toBeEnabled();

    // Pin both thumbs to the SAME interior position so best == worst (a single
    // fixed tier). The thumbs cannot cross, so set min first then max to the
    // same position. Position 3 is interior for the 8-tier video slider.
    await setRangeValue(page, "perf-video-range-min", 3);
    await setRangeValue(page, "perf-video-range-max", 3);

    // The "Fixed" badge for the video stream becomes visible once both bounds
    // resolve to the same tier.
    await expect(panel.locator('[data-testid="perf-video-fixed-badge"]')).toBeVisible({
      timeout: 10_000,
    });
  });
});
