import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { enableSimulcastFlag } from "../helpers/simulcast-config";

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
 *
 * The panel unified into ONE component (PR #1078) with, per kind, a **Send** row
 * AND a **Receive** row. SEND-side testids are UNCHANGED (below); RECEIVE-side
 * uses the `perf-recv-*` / `perf-vu-recv-*` namespace (further down).
 *
 *   Tab/nav/panel:  settings-tab-performance (id) · settings-nav-performance
 *                   (data-testid, role="tab") · settings-panel-performance (id)
 *
 *   SEND row (this spec's primary coverage; testids unchanged by #1078):
 *   VU gauges:      perf-vu-video / -audio / -screen (one per section)
 *                   readouts: perf-vu-{video,audio,screen}-readout (by id)
 *   Range inputs:   perf-{video,audio,screen}-range-min / -range-max
 *                   (native <input type=range>; always enabled)
 *   Auto toggles:   perf-{video,audio,screen}-auto (have aria-pressed)
 *   Help buttons:   perf-{video,audio,screen}-help (aria-expanded popover)
 *   Range value:    perf-{video,audio,screen}-range-value
 *   Fixed badge:    perf-{video,audio,screen}-fixed-badge
 *
 *   RECEIVE row (#1078; covered by the "Receive-side controls" describe block):
 *   VU gauges:      perf-vu-recv-video / -audio / -screen
 *                   readouts: perf-vu-recv-{video,audio,screen}-readout (by id)
 *                   format: `L{i}/{N} · {w}x{h}` (video/screen),
 *                           `L{i}/{N} · {kbps} kbps` (audio),
 *                           "Not receiving" placeholder when nothing decoded.
 *   Range inputs:   perf-recv-{video,audio,screen}-range-min / -range-max
 *   Auto toggles:   perf-recv-{video,audio,screen}-auto (have aria-pressed)
 *   Help buttons:   perf-recv-{video,audio,screen}-help
 *   Range value:    perf-recv-{video,audio,screen}-range-value
 *   Fixed badge:    perf-recv-{video,audio,screen}-fixed-badge
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
interface JoinOpts {
  /**
   * When true, deterministically drive the #1061 pre-join card to turn the
   * camera ON before joining (grant media → toggle camera → await a live video
   * track → join). Use this for tests that assert the live SEND video VU
   * readout — the LS preference seed alone is NOT enough because
   * `resolve_initial_enabled` (context.rs) only enables the camera at join when
   * the pre-join device list is populated, which requires getUserMedia to have
   * run first. Defaults to false (camera left at its persisted state).
   */
  ensureCameraOn?: boolean;
}

async function joinMeeting(page: Page, testLabel: string, opts: JoinOpts = {}): Promise<void> {
  const meetingId = `e2e_perf_${testLabel}_${Date.now()}`;

  // Seed the persisted camera-ON preference before the app boots. This makes the
  // pre-join camera toggle default ON; combined with `ensureCameraOn` (which
  // drives the UI so the device list is populated) it guarantees the SEND
  // encoder runs. addInitScript runs on every navigation (incl. post-reload).
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("vc_prejoin_camera_on", "true");
    } catch {
      /* storage may be unavailable pre-navigation; the app origin sets it */
    }
  });

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

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  if (opts.ensureCameraOn) {
    // Deterministic camera-on path (mirrors prejoin-device-preview.spec.ts
    // "camera ON in pre-join carries into the meeting"). The pre-join card must
    // be present (it coexists with the action button); grant media so the device
    // list populates, ensure the camera toggle is ON and a live track is
    // acquired, THEN click the action button. This guarantees the in-meeting
    // SEND encoder actually starts (so the VU readout shows {w}x{h}…kbps).
    await joinButton.waitFor({ timeout: 30_000 });

    // Grant media if the permission prompt is showing (auto-granted by the
    // --use-fake-ui-for-media-stream flag once clicked).
    const allow = page.locator('[data-testid="prejoin-permission-allow"]');
    if (await allow.isVisible().catch(() => false)) {
      await allow.click();
      await expect(page.locator('[data-testid="prejoin-permission-prompt"]')).toBeHidden({
        timeout: 15_000,
      });
    }

    // Ensure the camera toggle is ON (it defaults ON via the seeded preference,
    // but click it on if it somehow reads off).
    const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
    await cameraToggle.waitFor({ timeout: 15_000 });
    if ((await cameraToggle.getAttribute("aria-pressed")) !== "true") {
      await cameraToggle.click();
    }
    await expect(cameraToggle).toHaveAttribute("aria-pressed", "true", { timeout: 5_000 });

    // Wait for a live preview video track so the device list is populated before
    // join (this is what makes the in-meeting encoder start).
    await expect
      .poll(
        async () =>
          page
            .locator('[data-testid="prejoin-camera-preview"]')
            .evaluate((el) => {
              const v = el as HTMLVideoElement;
              const s = v.srcObject as MediaStream | null;
              return s ? s.getVideoTracks().filter((t) => t.readyState === "live").length : 0;
            })
            .catch(() => 0),
        { timeout: 15_000 },
      )
      .toBeGreaterThan(0);

    await joinButton.click();
    await expect(grid).toBeVisible({ timeout: 15_000 });
    return;
  }

  // Default path: the meeting page may auto-join (grid appears directly) or
  // present a Start/Join button. Race both so either path lands us in the grid.
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

/**
 * Select the SEND direction in the panel's `Receive | Send` segmented toggle
 * (#1078). The panel renders ONLY the active direction's three rows, so the
 * send-side controls (`perf-{kind}-*`, `perf-vu-{kind}`) are NOT in the DOM
 * until this runs. The default direction is Receive, so every send-side test
 * must call this after `openPerformanceTab`. Idempotent: confirms the Send
 * segment ends up `aria-checked="true"`.
 */
async function selectSendDirection(page: Page): Promise<void> {
  const sendSeg = page.locator('[data-testid="perf-direction-send"]');
  await sendSeg.click();
  await expect(sendSeg).toHaveAttribute("aria-checked", "true", { timeout: 5_000 });
}

/**
 * Select (or re-assert) the RECEIVE direction — the panel default. Receive-side
 * tests do not strictly need this on a fresh page, but calling it is a cheap
 * isolation guard in case the panel was left on Send. Idempotent.
 */
async function selectReceiveDirection(page: Page): Promise<void> {
  const recvSeg = page.locator('[data-testid="perf-direction-receive"]');
  await recvSeg.click();
  await expect(recvSeg).toHaveAttribute("aria-checked", "true", { timeout: 5_000 });
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
    // Default direction is Receive; reveal the SEND rows before asserting them.
    await selectSendDirection(page);

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
    await selectSendDirection(page);

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
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");
    const helpBtn = panel.locator('[data-testid="perf-video-help"]');
    const popover = page.locator("#perf-video-help-popover");

    // Closed by default.
    await expect(helpBtn).toHaveAttribute("aria-expanded", "false");
    await expect(popover).toHaveCount(0);

    // Click "?" → popover opens with the explanation copy. The SEND help body
    // (C1 rewording) reads "Sets the best (right) and worst (left) quality this
    // device PUBLISHES, …" — match the best/worst + publishes shape.
    await helpBtn.click();
    await expect(helpBtn).toHaveAttribute("aria-expanded", "true");
    await expect(popover).toBeVisible();
    await expect(popover).toContainText(/best.*worst.*quality.*publish/i);

    // Escape closes it.
    await page.keyboard.press("Escape");
    await expect(helpBtn).toHaveAttribute("aria-expanded", "false");
    await expect(popover).toHaveCount(0);

    // Re-open, then outside-click dismisses. The outside-click is implemented as
    // a transparent full-viewport scrim (`.perf-help-scrim`) rendered above the
    // panel while the popover is open; clicking it is the dismiss mechanism.
    // (Clicking a panel control like the range-value would be intercepted by the
    // overlaying scrim, so target the scrim directly — that is what a real
    // outside click hits.)
    await helpBtn.click();
    await expect(popover).toBeVisible();
    const scrim = page.locator(".perf-help-scrim");
    await expect(scrim).toBeVisible();
    await scrim.click();
    await expect(helpBtn).toHaveAttribute("aria-expanded", "false");
    await expect(popover).toHaveCount(0);
  });

  test("setting a video threshold persists to localStorage and restores after reload", async ({
    page,
  }) => {
    await joinMeeting(page, "persist");
    await openPerformanceTab(page);
    await selectSendDirection(page);

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
    // The send range-value reads "Sending: {worst – best}". With a manual bound
    // set it shows a concrete tier range; it must never read the "Auto" word
    // (span_text emits tier labels, not "Auto").
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
    // The panel reopens on the default Receive direction after reload; switch
    // back to Send to re-read the persisted send-side video preference.
    await selectSendDirection(page);
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
    // ensureCameraOn drives the pre-join card to actually start the camera (grant
    // + toggle + live track) so the in-meeting SEND encoder runs — otherwise the
    // video VU readout stays "Camera off" (the LS preference alone doesn't
    // populate the pre-join device list that resolve_initial_enabled requires).
    await joinMeeting(page, "vu_live", { ensureCameraOn: true });
    await openPerformanceTab(page);
    // The live SEND needles (encoder snapshot) only mount on the Send direction.
    await selectSendDirection(page);

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
    await selectSendDirection(page);

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

// ---------------------------------------------------------------------------
// RECEIVE-side controls (#1078 — unified send+receive panel).
//
// The unified panel renders a Receive row PER KIND alongside the Send row, each
// with its own dual-thumb range slider, Auto toggle, "?" help, Fixed badge, and
// a live "Receiving" needle. These use the `perf-recv-*` / `perf-vu-recv-*`
// namespace (distinct from the send-side ids the block above covers). This block
// asserts the receive row renders and behaves; it is single-page (no peer), so
// the receive needle shows the "Not receiving" placeholder (the cross-peer
// received-layer assertions live in simulcast-per-receiver.spec.ts).
// ---------------------------------------------------------------------------
test.describe("Performance settings panel — Receive-side controls (#1078)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("direction toggle: defaults to Receive, and Send|Receive swap which rows render", async ({
    page,
  }) => {
    await joinMeeting(page, "direction_toggle");
    await openPerformanceTab(page);

    const panel = page.locator("#settings-panel-performance");
    const recvSeg = panel.locator('[data-testid="perf-direction-receive"]');
    const sendSeg = panel.locator('[data-testid="perf-direction-send"]');

    // ── Default = Receive: receive segment checked, RECEIVE rows mounted, SEND
    //    rows ABSENT (the panel renders only the active direction's 3 rows). ──
    await expect(recvSeg).toHaveAttribute("aria-checked", "true");
    await expect(sendSeg).toHaveAttribute("aria-checked", "false");
    await expect(panel.locator('[data-testid="perf-vu-recv-video"]')).toBeVisible();
    await expect(panel.locator('[data-testid="perf-recv-video-range-min"]')).toBeVisible();
    // Send-side controls are not in the DOM while Receive is active.
    await expect(panel.locator('[data-testid="perf-vu-video"]')).toHaveCount(0);
    await expect(panel.locator('[data-testid="perf-video-range-min"]')).toHaveCount(0);

    // ── Click Send: send segment checked, SEND rows mounted, RECEIVE rows gone. ──
    await sendSeg.click();
    await expect(sendSeg).toHaveAttribute("aria-checked", "true");
    await expect(recvSeg).toHaveAttribute("aria-checked", "false");
    await expect(panel.locator('[data-testid="perf-vu-video"]')).toBeVisible();
    await expect(panel.locator('[data-testid="perf-video-range-min"]')).toBeVisible();
    await expect(panel.locator('[data-testid="perf-vu-recv-video"]')).toHaveCount(0);
    await expect(panel.locator('[data-testid="perf-recv-video-range-min"]')).toHaveCount(0);

    // ── Click Receive again: back to the receive rows (round-trip). ──
    await recvSeg.click();
    await expect(recvSeg).toHaveAttribute("aria-checked", "true");
    await expect(sendSeg).toHaveAttribute("aria-checked", "false");
    await expect(panel.locator('[data-testid="perf-vu-recv-video"]')).toBeVisible();
    await expect(panel.locator('[data-testid="perf-vu-video"]')).toHaveCount(0);
  });

  test("receive row renders a range slider, Auto toggle, and needle for each kind", async ({
    page,
  }) => {
    await joinMeeting(page, "recv_render");
    await openPerformanceTab(page);
    // Receive is the default direction; assert it explicitly for isolation.
    await selectReceiveDirection(page);

    const panel = page.locator("#settings-panel-performance");

    // Per kind, the RECEIVE row exposes its full control set in the perf-recv-*
    // namespace: needle gauge, dual-thumb range (min + max), Auto toggle, help.
    for (const kind of ["video", "audio", "screen"] as const) {
      await expect(
        panel.locator(`[data-testid="perf-vu-recv-${kind}"]`),
        `${kind} receive needle gauge present`,
      ).toBeVisible();
      await expect(
        panel.locator(`[data-testid="perf-recv-${kind}-range-min"]`),
        `${kind} receive min thumb present`,
      ).toBeVisible();
      await expect(
        panel.locator(`[data-testid="perf-recv-${kind}-range-max"]`),
        `${kind} receive max thumb present`,
      ).toBeVisible();
      await expect(
        panel.locator(`[data-testid="perf-recv-${kind}-auto"]`),
        `${kind} receive Auto toggle present`,
      ).toBeVisible();
      await expect(
        panel.locator(`[data-testid="perf-recv-${kind}-help"]`),
        `${kind} receive help button present`,
      ).toBeVisible();
    }
  });

  test("receive row defaults to Auto with both thumbs at the extremes", async ({ page }) => {
    await joinMeeting(page, "recv_auto_default");
    await openPerformanceTab(page);
    await selectReceiveDirection(page);

    const panel = page.locator("#settings-panel-performance");

    for (const kind of ["video", "audio", "screen"] as const) {
      // Default = Auto ON (aria-pressed="true"), slider fully interactive with
      // thumbs pinned to the extremes (min at 0, max at the ladder top).
      await expect(panel.locator(`[data-testid="perf-recv-${kind}-auto"]`)).toHaveAttribute(
        "aria-pressed",
        "true",
      );

      const minInput = panel.locator(`[data-testid="perf-recv-${kind}-range-min"]`);
      const maxInput = panel.locator(`[data-testid="perf-recv-${kind}-range-max"]`);
      await expect(minInput).toBeEnabled();
      await expect(maxInput).toBeEnabled();
      await expect(minInput).toHaveValue("0");
      // Max sits at the ladder top (its own `max` attribute).
      const top = await maxInput.getAttribute("max");
      await expect(maxInput).toHaveValue(String(top));
    }
  });

  test("receive Auto toggle flips aria-pressed and keeps the slider interactive", async ({
    page,
  }) => {
    await joinMeeting(page, "recv_auto_toggle");
    await openPerformanceTab(page);
    await selectReceiveDirection(page);

    const panel = page.locator("#settings-panel-performance");
    const autoBtn = panel.locator('[data-testid="perf-recv-video-auto"]');
    const minInput = panel.locator('[data-testid="perf-recv-video-range-min"]');

    await expect(autoBtn).toHaveAttribute("aria-pressed", "true");
    await autoBtn.click();
    await expect(autoBtn).toHaveAttribute("aria-pressed", "false");
    // The slider is never disabled (Auto is conveyed by the toggle + thumb
    // positions, not a disabled state) — mirrors the send-side behavior.
    await expect(minInput).toBeEnabled();
  });

  test("receive needle readout shows a valid received-layer line or the placeholder", async ({
    page,
  }) => {
    await joinMeeting(page, "recv_needle");
    await openPerformanceTab(page);
    await selectReceiveDirection(page);

    const panel = page.locator("#settings-panel-performance");

    // Single-page: no peer is sending, so the receive video needle must read the
    // "Not receiving" placeholder. If a stream WERE being decoded it would show
    // the `L{i}/{N} · {w}x{h}` shape; assert the union so the test is correct in
    // both states (mirrors how the send-side test asserts its needle readout).
    await expect(panel.locator("#perf-vu-recv-video-readout")).toBeVisible();
    await expect
      .poll(
        async () => (await panel.locator("#perf-vu-recv-video-readout").textContent())?.trim(),
        {
          timeout: 15_000,
        },
      )
      .toMatch(/^(L\d+\/\d+ · \d+x\d+|Not receiving)$/);

    // Audio receive readout: `L{i}/{N} · {kbps} kbps` or the placeholder.
    await expect
      .poll(
        async () => (await panel.locator("#perf-vu-recv-audio-readout").textContent())?.trim(),
        {
          timeout: 15_000,
        },
      )
      .toMatch(/^(L\d+\/\d+ · \d+ kbps|Not receiving)$/);
  });

  test("receive fixed badge appears when a kind's two thumbs collapse to one layer", async ({
    page,
  }) => {
    await joinMeeting(page, "recv_fixed_badge");
    await openPerformanceTab(page);
    await selectReceiveDirection(page);

    const panel = page.locator("#settings-panel-performance");

    // Turn the video RECEIVE Auto off (manual mode), then pin both thumbs to the
    // same interior layer so min == max → the receive Fixed badge appears.
    await panel.locator('[data-testid="perf-recv-video-auto"]').click();
    await expect(panel.locator('[data-testid="perf-recv-video-auto"]')).toHaveAttribute(
      "aria-pressed",
      "false",
    );

    await setRangeValue(page, "perf-recv-video-range-min", 1);
    await setRangeValue(page, "perf-recv-video-range-max", 1);

    await expect(panel.locator('[data-testid="perf-recv-video-fixed-badge"]')).toBeVisible({
      timeout: 10_000,
    });
  });
});

// ---------------------------------------------------------------------------
// SEND-side per-row simulcast diagnostics (PR #1101 / issue #1095).
//
// Each SEND row (Video / Audio / Screen) renders a live diagnostics FOOTER as
// the last child of `.perf-stream-controls`. For the per-layer kinds
// (video/screen) the footer is one of:
//   * SIMULCAST ACTIVE (effective layers > 1): a disclosure `<button>`
//     (`perf-{kind}-diag-summary`) whose text reads "N of M layers active"
//     (optionally "· X Mbps total" once bitrates arrive). Clicking it expands a
//     detail region (`perf-{kind}-diag-detail`) containing a per-layer ladder
//     (`perf-{kind}-diag-ladder`) with one rung chip per EFFECTIVE layer
//     (`perf-{kind}-diag-rung-{layerId}`). Single-open accordion via a shared
//     `open_diag` signal — opening one row's detail collapses any other.
//   * SINGLE LAYER (effective layers == 1): a STATIC `<span>` reading
//     "Single layer" (no disclosure, no detail).
//   * SOURCE OFF: a STATIC `<span>` — "Camera — off" (video row, camera
//     disabled) or "Screen — not sharing" (screen row, not sharing). This is the
//     #1101 fix (jay-boyd review): the video send footer used to render a STALE
//     "N of M layers active" while the camera was OFF because the encoder's
//     active-layer/bitrate atomics are not reset on disable. host.rs now gates
//     `send_video` on `prev_video_enabled`, so the OFF row surfaces
//     "Camera — off" with NO layer count.
//
// ── Capability ceiling (THE reason these tests are flag-on + capability-gated) ─
// The disclosure (and ladder) only render when simulcast is ACTIVE, i.e. the
// effective video layer count > 1. That count is
//   `min(experimentalSimulcastMaxLayers, capability_max_simulcast_layers())`
// (dioxus-ui/src/components/host.rs). The committed e2e `config.js` pins
// `experimentalSimulcastMaxLayers: 1` (feature OFF), so WITHOUT a flag override
// the camera always emits a single layer and the footer is the static
// "Single layer" span — no disclosure to exercise. We therefore enable the flag
// for the test browser via `enableSimulcastFlag(page.context(), 3)` (a
// `/config.js` route patch — it does NOT touch the committed config.js).
//
// Even with the flag on, `capability_max_simulcast_layers()` reads a live
// ~100ms CPU benchmark with NO test override (see helpers/simulcast-config.ts),
// so a weak/containerized CI runner can still clamp the publisher to 1 layer. In
// that case the camera-on footer is "Single layer" and there is no disclosure /
// ladder to assert. Mirroring `simulcast-per-receiver.spec.ts`, the
// disclosure-dependent tests DETECT that clamp and `test.skip` (a single layer
// is not a feature failure) rather than emit a false negative — and they assert
// rung SHAPE only (count >= 1), never a hard-coded layer count M.
//
// The two capability-INDEPENDENT guards always run on every runner:
//   * the MANDATORY camera-off regression (the OFF footer is static regardless
//     of simulcast), and
//   * screen "not sharing" (the screen encoder is inactive, so its snapshot is
//     None regardless of the ceiling).
//
// All live-content assertions use `expect.poll` because the footer summaries are
// driven by a ~4 Hz (250 ms) panel refresh tick — the same cadence the existing
// `#perf-vu-video-readout` test polls over a 15 s budget.
// ---------------------------------------------------------------------------

/** Matches the simulcast-active summary text, e.g. "2 of 3 layers active". */
const LAYERS_ACTIVE_RE = /\d+ of \d+ layers active/;

/**
 * Read the trimmed text of a SEND diagnostics summary (button or static span).
 * Both render with the same `perf-{kind}-diag-summary` testid.
 */
async function diagSummaryText(page: Page, kind: "video" | "screen"): Promise<string> {
  const t = await page
    .locator(`[data-testid="perf-${kind}-diag-summary"]`)
    .textContent()
    .catch(() => null);
  return (t ?? "").trim();
}

/**
 * Poll the SEND video diagnostics summary until it settles into one of its two
 * camera-ON shapes and report which. Returns `"simulcast"` when the disclosure
 * is live ("N of M layers active"), or `"single"` when the runner's capability
 * ceiling clamped the publisher to a single layer (static "Single layer").
 *
 * Polls on the ~4 Hz refresh cadence (15 s budget, mirroring the VU readout
 * test) so a slow encoder spin-up does not flake.
 */
async function awaitVideoDiagShape(page: Page): Promise<"simulcast" | "single"> {
  await expect
    .poll(async () => diagSummaryText(page, "video"), { timeout: 15_000 })
    .toMatch(/(\d+ of \d+ layers active|Single layer)/);
  const text = await diagSummaryText(page, "video");
  return LAYERS_ACTIVE_RE.test(text) ? "simulcast" : "single";
}

/**
 * Click the in-meeting camera toggle (stable testid added for #1101). The camera
 * button is a PRIMARY, always-visible control (not behind the secondary-controls
 * autohide), so a container hover to clear the dock autohide is enough.
 *
 * The settings modal must be CLOSED first: its overlay (`.device-settings-modal-
 * overlay`) blocks pointer events to the toolbar behind it. Callers toggle the
 * camera between modal sessions.
 */
async function clickCameraToggle(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.locator('[data-testid="camera-toggle-button"]').click();
}

/** Close the device-settings modal via Escape and wait for it to detach. */
async function closeSettingsModal(page: Page): Promise<void> {
  await page.keyboard.press("Escape");
  await expect(page.locator(".device-settings-modal")).toBeHidden({ timeout: 5_000 });
}

test.describe("Performance settings panel — SEND simulcast diagnostics (#1101)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL, page }) => {
    await injectSessionCookie(context, { baseURL });
    // Flip the simulcast flag ON for THIS test browser only (route patch on the
    // page's context; never touches the committed config.js). Must run before
    // the first navigation so the very first /config.js fetch is intercepted —
    // joinMeeting() performs that navigation.
    await enableSimulcastFlag(page.context(), 3);
  });

  // 1. MANDATORY camera-off regression — guards the exact stale-footer bug
  //    jay-boyd found. The OFF assertion is UNCONDITIONAL (runs on every runner,
  //    capability ceiling or not); the camera-ON disclosure assertion is
  //    best-effort (annotated, not skipped) so this guard always executes.
  test("camera ON shows live layers; toggling camera OFF clears them to 'Camera — off'", async ({
    page,
  }) => {
    await joinMeeting(page, "diag_camera_off", { ensureCameraOn: true });
    await openPerformanceTab(page);
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");
    const summary = panel.locator('[data-testid="perf-video-diag-summary"]');
    const detail = panel.locator('[data-testid="perf-video-diag-detail"]');

    // ── Camera ON: the footer must NOT be the off/not-sharing static line. ──
    const shape = await awaitVideoDiagShape(page);
    if (shape === "simulcast") {
      // Live disclosure: summary reads "N of M layers active"; expanding it
      // reveals the per-layer ladder. ("N of M layers active" lives on the
      // SUMMARY button text — `format_send_header` — and the DETAIL holds the
      // ladder chips, so we assert the regex on the summary and the ladder on
      // the expanded detail.)
      await expect(summary).toHaveAttribute("aria-expanded", "false");
      await expect
        .poll(async () => diagSummaryText(page, "video"), { timeout: 15_000 })
        .toMatch(LAYERS_ACTIVE_RE);
      await summary.click();
      await expect(summary).toHaveAttribute("aria-expanded", "true");
      await expect(detail).toBeVisible();
      await expect(panel.locator('[data-testid="perf-video-diag-ladder"]')).toBeVisible();
    } else {
      // Capability ceiling clamped to a single layer: there is no disclosure to
      // expand, but the OFF guard below still runs (that is the actual #1101
      // regression). Record it so the run is not silently weaker.
      test.info().annotations.push({
        type: "capability-ceiling",
        description:
          "runner clamped the camera to a single layer (static 'Single layer'); the " +
          "camera-ON disclosure/ladder portion was not exercised, but the camera-OFF " +
          "regression guard (the #1101 fix) still runs unconditionally below.",
      });
      await expect(summary).toHaveText("Single layer");
    }

    // ── Toggle the camera OFF — UNCONDITIONAL regression guard. ──
    // The modal overlay blocks the toolbar, so close it, flip the camera off via
    // the in-meeting toggle, then reopen the panel to read the footer. host.rs
    // gates `send_video` on the camera being enabled, so the footer must flip to
    // the static "Camera — off" line and must NEVER keep rendering a stale
    // "N of M layers active" count.
    await closeSettingsModal(page);
    await clickCameraToggle(page);
    await openPerformanceTab(page);
    await selectSendDirection(page);

    const panelOff = page.locator("#settings-panel-performance");
    await expect
      .poll(async () => diagSummaryText(page, "video"), { timeout: 15_000 })
      .toBe("Camera — off");
    // The stale-count bug would leave "… layers active" rendered with the camera
    // off — assert it is gone.
    await expect
      .poll(async () => diagSummaryText(page, "video"), { timeout: 15_000 })
      .not.toMatch(LAYERS_ACTIVE_RE);
    // And there must be no expandable detail while the source is off.
    await expect(panelOff.locator('[data-testid="perf-video-diag-detail"]')).toHaveCount(0);
  });

  // 2. Disclosure expand / collapse (video row). Capability-gated: needs an
  //    active simulcast ladder (a disclosure) to expand.
  test("video diagnostics disclosure expands and collapses", async ({ page }) => {
    await joinMeeting(page, "diag_disclosure", { ensureCameraOn: true });
    await openPerformanceTab(page);
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");
    const summary = panel.locator('[data-testid="perf-video-diag-summary"]');
    const detail = panel.locator('[data-testid="perf-video-diag-detail"]');

    const shape = await awaitVideoDiagShape(page);
    test.skip(
      shape === "single",
      "runner capability ceiling clamped the camera to a single layer; there is no " +
        "send diagnostics disclosure to expand (see helpers/simulcast-config.ts)",
    );

    // Collapsed by default: the detail region is not in the DOM.
    await expect(summary).toHaveAttribute("aria-expanded", "false");
    await expect(detail).toHaveCount(0);

    // Click → expands (detail visible, aria-expanded true).
    await summary.click();
    await expect(summary).toHaveAttribute("aria-expanded", "true");
    await expect(detail).toBeVisible();

    // Click again → collapses (detail removed, aria-expanded false).
    await summary.click();
    await expect(summary).toHaveAttribute("aria-expanded", "false");
    await expect(detail).toHaveCount(0);
  });

  // 3. Single-open accordion: opening the screen row's detail collapses the
  //    video row's. Needs TWO expandable rows, so it shares the screen (via a
  //    synthetic getDisplayMedia shim, mirroring screen-share-state.spec.ts) AND
  //    requires both encoders to run simulcast — capability-gated on both.
  test("diagnostics accordion is single-open across rows", async ({ page }) => {
    // Synthetic screen capture so the screen encoder runs and its diagnostics
    // footer becomes a disclosure (canvas-backed MediaStream; resolves after a
    // short picker-mimicking delay). Injected before navigation.
    await page.addInitScript(() => {
      const md = navigator.mediaDevices;
      md.getDisplayMedia = function () {
        return new Promise((resolve) => {
          const canvas = document.createElement("canvas");
          canvas.width = 640;
          canvas.height = 480;
          const cx = canvas.getContext("2d");
          if (cx) {
            cx.fillStyle = "#2a2a2a";
            cx.fillRect(0, 0, 640, 480);
            cx.fillStyle = "#fff";
            cx.font = "24px sans-serif";
            cx.fillText("Mock Screen Share", 160, 240);
          }
          const stream = (canvas as HTMLCanvasElement).captureStream(5);
          setTimeout(() => resolve(stream), 200);
        });
      } as typeof md.getDisplayMedia;
    });

    await joinMeeting(page, "diag_accordion", { ensureCameraOn: true });

    // Start the screen share from the in-meeting toolbar (tooltip selector — the
    // screen-share button has no testid; #1101 scoped the new testid to the
    // camera button only). The button lives in the auto-hiding secondary
    // controls, so hover the dock to keep them shown before clicking.
    await page.locator(".video-controls-container").hover();
    const screenBtn = page
      .locator('.video-controls-container button:has(span.tooltip:text-is("Share Screen"))')
      .first();
    await screenBtn.waitFor({ state: "visible", timeout: 10_000 });
    await screenBtn.click();

    await openPerformanceTab(page);
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");
    const videoSummary = panel.locator('[data-testid="perf-video-diag-summary"]');
    const videoDetail = panel.locator('[data-testid="perf-video-diag-detail"]');
    const screenSummary = panel.locator('[data-testid="perf-screen-diag-summary"]');
    const screenDetail = panel.locator('[data-testid="perf-screen-diag-detail"]');

    // Both rows must be live disclosures; if either clamped to a single layer
    // there is only one expandable row and the accordion cannot be exercised.
    const videoShape = await awaitVideoDiagShape(page);
    await expect
      .poll(async () => diagSummaryText(page, "screen"), { timeout: 15_000 })
      .toMatch(/(\d+ of \d+ layers active|Single layer|Screen — not sharing)/);
    const screenText = await diagSummaryText(page, "screen");
    test.skip(
      videoShape === "single" || !LAYERS_ACTIVE_RE.test(screenText),
      "capability ceiling clamped video and/or screen to a single layer (or the " +
        "screen share did not start), so there are not two expandable diagnostics " +
        "rows to exercise the single-open accordion (see helpers/simulcast-config.ts)",
    );

    // Expand the video row first.
    await videoSummary.click();
    await expect(videoSummary).toHaveAttribute("aria-expanded", "true");
    await expect(videoDetail).toBeVisible();

    // Expanding the screen row must collapse the video row (single-open).
    await screenSummary.click();
    await expect(screenSummary).toHaveAttribute("aria-expanded", "true");
    await expect(screenDetail).toBeVisible();
    await expect(videoSummary).toHaveAttribute("aria-expanded", "false");
    await expect(videoDetail).toHaveCount(0);
  });

  // 4. Screen "not sharing" — capability-INDEPENDENT (the screen encoder is
  //    inactive, so its snapshot is None regardless of the ceiling).
  test("screen diagnostics read 'Screen — not sharing' with no active share", async ({ page }) => {
    await joinMeeting(page, "diag_screen_idle");
    await openPerformanceTab(page);
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");
    const screenSummary = panel.locator('[data-testid="perf-screen-diag-summary"]');

    // No screen share active → static "Screen — not sharing" line, with NO layer
    // count and NO expandable detail.
    await expect
      .poll(async () => diagSummaryText(page, "screen"), { timeout: 15_000 })
      .toBe("Screen — not sharing");
    // No layer count in the not-sharing line.
    await expect
      .poll(async () => diagSummaryText(page, "screen"), { timeout: 15_000 })
      .not.toMatch(LAYERS_ACTIVE_RE);
    await expect(panel.locator('[data-testid="perf-screen-diag-detail"]')).toHaveCount(0);
    // A static span carries no disclosure semantics (it is a <span>, not the
    // disclosure <button>, so it has no aria-expanded attribute at all).
    await expect(screenSummary).not.toHaveAttribute("aria-expanded", /.*/);
  });

  // 5. Live ladder renders — SHAPE only (rung count >= 1; never a hard-coded M,
  //    because the CI capability ceiling can clamp the layer count).
  test("live per-layer ladder renders at least one rung when expanded", async ({ page }) => {
    await joinMeeting(page, "diag_ladder", { ensureCameraOn: true });
    await openPerformanceTab(page);
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");
    const summary = panel.locator('[data-testid="perf-video-diag-summary"]');

    const shape = await awaitVideoDiagShape(page);
    test.skip(
      shape === "single",
      "runner capability ceiling clamped the camera to a single layer; the per-layer " +
        "ladder only renders for an active simulcast ladder (see helpers/simulcast-config.ts)",
    );

    await summary.click();
    await expect(summary).toHaveAttribute("aria-expanded", "true");

    const ladder = panel.locator('[data-testid="perf-video-diag-ladder"]');
    await expect(ladder).toBeVisible();
    // The lowest rung (layer 0) is always present in an active ladder.
    await expect(panel.locator('[data-testid="perf-video-diag-rung-0"]')).toBeAttached();
    // SHAPE assertion: >= 1 rung. Do NOT hard-code the layer count M — the CI
    // capability ceiling can clamp it to a single (or otherwise reduced) ladder.
    await expect
      .poll(async () => ladder.locator('[data-testid^="perf-video-diag-rung-"]').count(), {
        timeout: 15_000,
      })
      .toBeGreaterThanOrEqual(1);
  });
});
