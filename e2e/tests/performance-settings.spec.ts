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
 *   1. That stream's live inline bar-meter (`perf-vu-{stream}` container, with a
 *      `data-level` attr the rAF driver writes) plus a one-line readout
 *      (`perf-vu-{stream}-readout`). Readouts show `{w}x{h}·{fps}fps·{kbps}kbps`
 *      for video/screen, `{kbps} kbps` for audio, and the screen meter shows the
 *      literal placeholder "Screen — not sharing" while no screen share is active.
 *      (#1095 redesign — the VU NEEDLE gauge was replaced by the bar-meter; the
 *      `perf-vu-*` testids were KEPT on the new container so selectors resolve.)
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
 * The panel is three per-kind CARDS (Video / Audio / Content; PR #1095), each
 * split into a **Sending** column AND a **Receiving** column rendered together
 * (the old Receive | Send direction toggle was removed). SEND-side testids are
 * UNCHANGED (below); RECEIVE-side uses the `perf-recv-*` / `perf-vu-recv-*`
 * namespace (further down). Cross-nav: `perf-open-diagnostics` (Perf→Diagnostics)
 * and `diag-open-performance` (Diagnostics→Perf).
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
 * No-op kept for call-site compatibility. The `Receive | Send` direction toggle
 * was REMOVED in the #1095 redesign: every per-kind card now renders both a
 * Sending column and a Receiving column at once, so the send-side controls
 * (`perf-{kind}-*`, `perf-vu-{kind}`) are always in the DOM. Asserting the send
 * video meter is present is a cheap guard that the cards rendered.
 */
async function selectSendDirection(page: Page): Promise<void> {
  await expect(page.locator('[data-testid="perf-vu-video"]')).toBeVisible({ timeout: 5_000 });
}

/**
 * No-op kept for call-site compatibility (see {@link selectSendDirection}). Both
 * directions render together now; the receive controls are always present.
 */
async function selectReceiveDirection(page: Page): Promise<void> {
  await expect(page.locator('[data-testid="perf-vu-recv-video"]')).toBeVisible({ timeout: 5_000 });
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
    // Both directions render together now (no toggle); this guards the send
    // meters rendered before we assert them.
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

  test("no-scroll: the Performance panel content fits the modal body without scrolling on a desktop display (#1095)", async ({
    page,
  }) => {
    // The user's headline #1095 requirement: "all metrics fit in one dialog
    // without scrolling". This is a DESKTOP requirement (the spec accepts a small
    // scroll on mobile), so assert at exactly the 768px desktop target the layout
    // was budgeted against (~622px content vs ~634px usable). The ~12px headroom
    // is deliberately tight so a regression that re-grows a card (+padding/margin)
    // or re-splits a caption onto a second line pushes scrollHeight past
    // clientHeight and fails here, rather than hiding behind generous slack.
    await page.setViewportSize({ width: 1280, height: 768 });
    await joinMeeting(page, "no_scroll");
    await openPerformanceTab(page);
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");
    await expect(panel).toBeVisible();
    // All three cards must be present (so we're measuring the full content, not a
    // partially-rendered panel).
    await expect(panel.locator(".perf-kind-card")).toHaveCount(3);

    // The scrollable container is `.settings-panel` (it carries overflow-y:auto),
    // which WRAPS the `#settings-panel-performance` tabpanel. It must NOT be
    // overflowing: scrollHeight <= clientHeight (allow 1px for sub-pixel
    // rounding). If this fails, the panel scrolls — the no-scroll requirement is
    // broken.
    const scrollContainer = page.locator(".settings-panel");
    const overflow = await scrollContainer.evaluate((el) => el.scrollHeight - el.clientHeight);
    expect(
      overflow,
      `Performance panel overflows its modal body by ${overflow}px (no-scroll requirement)`,
    ).toBeLessThanOrEqual(1);
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

    // Click "?" → popover opens with the explanation copy. The SEND video help
    // body (`HELP_VIDEO_SEND` in performance_settings.rs) reads "Your camera
    // sends several quality versions ('layers') … The slider caps the best and
    // worst versions you'll send." Match the distinctive "best and worst …
    // send" tail so a copy change here breaks the test (the source of truth is
    // the Rust constant, not this regex).
    await helpBtn.click();
    await expect(helpBtn).toHaveAttribute("aria-expanded", "true");
    await expect(popover).toBeVisible();
    await expect(popover).toContainText(/best and worst versions you'll send/i);

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
    // Both directions render together after reload (no toggle); guard that the
    // send meters are present before re-reading the persisted send-side video
    // preference.
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

  test("bar-meters are live: video readout shows a real value, screen shows 'Screen — not sharing'", async ({
    page,
  }) => {
    // ensureCameraOn drives the pre-join card to actually start the camera (grant
    // + toggle + live track) so the in-meeting SEND encoder runs — otherwise the
    // video meter readout stays "Camera — off" (the LS preference alone doesn't
    // populate the pre-join device list that resolve_initial_enabled requires).
    await joinMeeting(page, "vu_live", { ensureCameraOn: true });
    await openPerformanceTab(page);
    // Both directions render together now; the send meters are always present.
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");

    // The video meter readout is updated by a ~4 Hz rAF loop from the live
    // encoder snapshot. With the fake camera producing a synthetic stream, the
    // readout should leave any placeholder and show a real
    // `{w}x{h}·{fps}fps·{kbps}kbps` line. Match the resolution/kbps shape
    // (digits + "kbps") rather than an exact resolution, which can vary.
    await expect
      .poll(async () => panel.locator("#perf-vu-video-readout").textContent(), { timeout: 15_000 })
      .toMatch(/\d+x\d+.*kbps/);

    // No screen share is active, so the screen meter shows the literal
    // "Screen — not sharing" placeholder (SCREEN_EMPTY_READOUT in the Rust impl).
    await expect(panel.locator("#perf-vu-screen-readout")).toHaveText("Screen — not sharing", {
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

  test("both directions render at once: each kind card shows Send AND Receive controls (no direction toggle)", async ({
    page,
  }) => {
    await joinMeeting(page, "both_directions");
    await openPerformanceTab(page);

    const panel = page.locator("#settings-panel-performance");

    // The #1095 redesign removed the `Receive | Send` segmented toggle: the
    // panel now shows three per-kind cards, each split into a Sending column and
    // a Receiving column, so BOTH directions' controls are in the DOM together.
    await expect(panel.locator('[data-testid="perf-direction-receive"]')).toHaveCount(0);
    await expect(panel.locator('[data-testid="perf-direction-send"]')).toHaveCount(0);

    // For EVERY kind (video / audio / content) the Send-side meter + dual-range
    // AND the Receive-side meter + dual-range must be visible AT THE SAME TIME.
    // Asserting both sides per kind is what makes this a real regression guard
    // for the "show both directions" requirement: a redesign that hid one side
    // (or only kept video's receive column) would fail here, whereas checking a
    // single direction would silently pass.
    for (const kind of ["video", "audio", "screen"] as const) {
      // Send side present…
      await expect(
        panel.locator(`[data-testid="perf-vu-${kind}"]`),
        `${kind} SEND meter visible`,
      ).toBeVisible();
      await expect(
        panel.locator(`[data-testid="perf-${kind}-range-min"]`),
        `${kind} SEND min thumb visible`,
      ).toBeVisible();
      // …and the receive side present simultaneously.
      await expect(
        panel.locator(`[data-testid="perf-vu-recv-${kind}"]`),
        `${kind} RECEIVE meter visible`,
      ).toBeVisible();
      await expect(
        panel.locator(`[data-testid="perf-recv-${kind}-range-min"]`),
        `${kind} RECEIVE min thumb visible`,
      ).toBeVisible();
    }

    // The Diagnostics cross-nav button lives in the panel header (#1095 §4a).
    await expect(panel.locator('[data-testid="perf-open-diagnostics"]')).toBeVisible();
  });

  test("receive row renders a range slider, Auto toggle, and needle for each kind", async ({
    page,
  }) => {
    await joinMeeting(page, "recv_render");
    await openPerformanceTab(page);
    // Both directions render together (no toggle); guard the receive meters
    // rendered before asserting the receive controls.
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
// Cross-nav + the relocated "Simulcast layers" diagnostics section (#1095).
//
// The #1095 redesign REMOVED the in-panel per-row diagnostics footers (the old
// `perf-{kind}-diag-*` disclosure/ladder/peer rows) and MOVED that detail into
// the Call Diagnostics panel's new "Simulcast layers" section. The Performance
// panel now offers a "Diagnostics" cross-nav button (`perf-open-diagnostics`)
// that closes settings and opens the diagnostics sidebar; the diagnostics header
// offers a "Performance" cross-nav button (`diag-open-performance`) back.
//
// The relocated section is fed by the live `DiagnosticsReader` Host publishes to
// the parent, so:
//   * Video (sending) static line "Camera — off" when the camera is off — the
//     #1101 stale-count regression, now asserted in its new home.
//   * Screen (sending) static line "Screen — not sharing" with no active share.
//   * The per-layer ladder (`diag-simulcast-ladder` + `diag-simulcast-rung-{id}`)
//     renders >= 1 rung when simulcast is active (capability-gated, SHAPE only).
//
// Open the diagnostics sidebar via the cross-nav button itself, so these tests
// also cover the Perf→Diagnostics nav. All live-content assertions use
// `expect.poll` (the section refreshes on a ~4 Hz tick).
// ---------------------------------------------------------------------------

/** Open Performance, then click the "Diagnostics" cross-nav button (#1095 §4a). */
async function openDiagnosticsFromPerformance(page: Page): Promise<void> {
  await openPerformanceTab(page);
  await page.locator('[data-testid="perf-open-diagnostics"]').click();
  // The settings overlay closes and the diagnostics sidebar becomes visible.
  await expect(page.locator("#diagnostics-sidebar.visible")).toBeVisible({ timeout: 5_000 });
}

/** Trimmed text of the named "Simulcast layers" SEND ladder block (by title). */
async function simulcastSendText(page: Page, title: string): Promise<string> {
  const block = page
    .locator(".simulcast-send")
    .filter({ has: page.locator(`.simulcast-send-title:text-is("${title}")`) });
  const t = await block.textContent().catch(() => null);
  return (t ?? "").trim();
}

test.describe("Performance ⇄ Diagnostics cross-nav + Simulcast layers (#1095)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL, page }) => {
    await injectSessionCookie(context, { baseURL });
    // Flag ON for THIS browser only so the publisher CAN run simulcast (the
    // ladder-shape test is still capability-gated below).
    await enableSimulcastFlag(page.context(), 3);
  });

  test("cross-nav: Performance → Diagnostics opens the panel, Diagnostics → Performance returns", async ({
    page,
  }) => {
    await joinMeeting(page, "xnav");

    // ── ROUND-TRIP 1: Performance → Diagnostics ──
    // Before navigating, the Performance panel must be the thing on screen.
    await openPerformanceTab(page);
    await expect(page.locator("#settings-panel-performance")).toBeVisible();
    // Click the in-panel "Diagnostics" cross-nav button.
    await page.locator('[data-testid="perf-open-diagnostics"]').click();
    // The settings modal is gone AND the diagnostics sidebar is open.
    await expect(page.locator(".device-settings-modal")).toHaveCount(0);
    await expect(page.locator("#diagnostics-sidebar.visible")).toBeVisible({ timeout: 5_000 });

    // The relocated "Simulcast layers" section (#1095 §6 MOVE) is now present in
    // the Diagnostics sidebar — assert the heading AND the moved sub-structure so
    // this fails if the MOVE regressed (a bare heading could survive an empty
    // section). Single-context (camera off / no peers) so the live ladder +
    // per-peer testids are NOT in the DOM; assert the always-present structure:
    // both SEND blocks (by title) and the per-peer RECEIVE sub-section header.
    const sidebar = page.locator("#diagnostics-sidebar");
    await expect(sidebar.getByRole("heading", { name: "Simulcast layers" })).toBeVisible();
    await expect(sidebar.locator('.simulcast-send-title:text-is("Video (sending)")')).toBeVisible();
    await expect(
      sidebar.locator('.simulcast-send-title:text-is("Screen (sending)")'),
    ).toBeVisible();
    await expect(sidebar.locator(".simulcast-recv-title")).toBeVisible();

    // ── ROUND-TRIP 2: Diagnostics → Performance (must LAND on the Performance
    //    tab, not just reopen settings) ──
    await page.locator('[data-testid="diag-open-performance"]').click();
    // Settings reopens AND the Performance tabpanel is the active one (the
    // `device_settings_initial_section = "performance"` wiring) …
    await expect(page.locator("#settings-panel-performance")).toBeVisible({ timeout: 5_000 });
    // …proven by a Performance-only control being visible (the panel content, not
    // just the tabpanel wrapper, is mounted) …
    await expect(page.locator('[data-testid="perf-vu-video"]')).toBeVisible({ timeout: 5_000 });
    // …and the diagnostics sidebar has closed (unmounted).
    await expect(page.locator("#diagnostics-sidebar.visible")).toHaveCount(0);
  });

  // Camera-off regression (the #1101 fix), now in its new home: the relocated
  // "Video (sending)" line must read the static "Camera — off" with NO layer
  // count when the camera is off. Capability-INDEPENDENT (the snapshot is gated
  // to None while the camera is disabled).
  test("Simulcast layers: Video (sending) reads 'Camera — off' when the camera is off", async ({
    page,
  }) => {
    // Join WITHOUT turning the camera on, so send_video is gated to None.
    await joinMeeting(page, "diag_cam_off");
    await openDiagnosticsFromPerformance(page);

    await expect
      .poll(async () => simulcastSendText(page, "Video (sending)"), { timeout: 15_000 })
      .toContain("Camera — off");
    // The stale-count bug would leave "N of M layers active" — assert it is gone.
    await expect
      .poll(async () => simulcastSendText(page, "Video (sending)"), { timeout: 15_000 })
      .not.toMatch(/\d+ of \d+ layers active/);
  });

  // Screen "not sharing" — capability-INDEPENDENT (the screen encoder is inactive
  // so its snapshot is None regardless of the CPU ceiling).
  test("Simulcast layers: Screen (sending) reads 'Screen — not sharing' with no active share", async ({
    page,
  }) => {
    await joinMeeting(page, "diag_screen_idle");
    await openDiagnosticsFromPerformance(page);

    await expect
      .poll(async () => simulcastSendText(page, "Screen (sending)"), { timeout: 15_000 })
      .toContain("Screen — not sharing");
  });

  // Live per-layer ladder — SHAPE only (>= 1 rung; never a hard-coded layer count
  // M, because the CI capability ceiling can clamp the publisher to one layer).
  test("Simulcast layers: video ladder renders at least one rung when simulcast is active", async ({
    page,
  }) => {
    await joinMeeting(page, "diag_ladder", { ensureCameraOn: true });
    await openDiagnosticsFromPerformance(page);

    const sidebar = page.locator("#diagnostics-sidebar");
    // Wait for the camera-on Video (sending) line to settle into one of its two
    // shapes; if the runner clamped to a single layer there is no ladder.
    await expect
      .poll(async () => simulcastSendText(page, "Video (sending)"), { timeout: 15_000 })
      .toMatch(/(\d+ of \d+ layers active|Single layer)/);
    const text = await simulcastSendText(page, "Video (sending)");
    test.skip(
      !/\d+ of \d+ layers active/.test(text),
      "runner capability ceiling clamped the camera to a single layer; no per-layer " +
        "ladder renders (see helpers/simulcast-config.ts)",
    );

    const ladder = sidebar.locator('[data-testid="diag-simulcast-ladder"]').first();
    await expect(ladder).toBeVisible();
    // SHAPE assertion: >= 1 rung. Do NOT hard-code the layer count M.
    await expect
      .poll(async () => ladder.locator('[data-testid^="diag-simulcast-rung-"]').count(), {
        timeout: 15_000,
      })
      .toBeGreaterThanOrEqual(1);
  });
});
