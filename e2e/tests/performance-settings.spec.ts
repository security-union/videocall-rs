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
 *   2. A dual-thumb range slider (two overlaid native `<input type="range">`)
 *      plus a per-stream "?" help button and a small "Reset" button (#1131 §D —
 *      REPLACES the former "Auto" toggle; the `perf-{kind}-auto` testid was
 *      REPURPOSED onto Reset, so selectors resolve unchanged).
 *
 * The SEND control is a LAYER-COUNT control for ALL THREE kinds (the "layers
 * published" feature): the track ticks are the kind's simulcast rungs, the FLOOR
 * (min/left) thumb is **PINNED + DISABLED** at the base layer (position 0 — the
 * base is always published), and only the CEILING (max/right) thumb moves to set
 * how many layers you publish. Default = full ladder (ceiling at the top position
 * == effective max layers). Lowering the ceiling sets the persisted `*_layers`
 * COUNT (and frees that stream's tier bounds to Auto); Reset clears it back to the
 * full ladder. The min input IS disabled on all three.
 *   - VIDEO + SCREEN effective depth is CPU-clamped: `min(flag, capability)`, so
 *     it can be 1 on weak (<6-core) runners (tests read the rendered `max` attr
 *     and skip drag assertions at 1 layer — see helpers/simulcast-config.ts).
 *   - AUDIO effective depth is `min(flag, 3)` and is NOT CPU-clamped (audio Opus
 *     encode is cheap, off the main thread), so audio typically shows the full
 *     3-layer ladder even on weak runners; the publish-gate runs at runtime in the
 *     mic encoder (no restart). There is NO SEND "Fixed" badge for any kind now.
 *
 * At the full default the Reset button is ABSENT (nothing to reset). Lowering any
 * ceiling shows Reset; clicking it clears back to full. Reset is NOT a toggle (no
 * `aria-pressed`).
 *
 * Choices persist to `localStorage["vc_performance_quality"]` as a
 * `PerformancePreference` JSON object:
 *   { video_max, video_min, audio_max, audio_min, screen_max, screen_min:
 *     usize|null, video_auto, audio_auto, screen_auto: bool,
 *     video_layers, screen_layers, audio_layers: usize|null (SEND layer count) }
 * Default = all-Auto (every `*_auto` true, every bound null, every `*_layers`
 * null = full ladder).
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
 *                   (native <input type=range>; audio both enabled, video/screen
 *                   min is DISABLED — the pinned base-layer floor)
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
  // SEND layer-count ceilings (the "layers published" control). A layer COUNT,
  // or null = Auto / full ladder. ALL THREE kinds (video, screen, audio) are
  // driven by the layer-count slider; audio applies its ceiling at RUNTIME via the
  // mic encoder's per-layer publish-gate (base always sent). `#[serde(default)]` on
  // the Rust side → these keys may be absent in prefs persisted before the control
  // existed, hence optional here.
  video_layers?: number | null;
  screen_layers?: number | null;
  audio_layers?: number | null;
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

    // ── Three threshold groups: assert each stream's slider pair + help ──
    // (#1131 §D: the former "Auto" toggle was replaced by a "Reset" button that is
    // CONDITIONALLY RENDERED — absent at the default full range, present only when
    // constrained. The `perf-{stream}-auto` testid was REPURPOSED onto Reset.)
    for (const stream of ["video", "audio", "screen"] as const) {
      await expect(panel.locator(`[data-testid="perf-${stream}-range-min"]`)).toBeVisible();
      await expect(panel.locator(`[data-testid="perf-${stream}-range-max"]`)).toBeVisible();
      await expect(panel.locator(`[data-testid="perf-${stream}-help"]`)).toBeVisible();
    }

    // ── Default = full automatic range: the Reset button is ABSENT (nothing to
    //    reset → the slot is empty). ALL THREE SEND controls (video, screen, AND
    //    audio) are now LAYER-COUNT controls: the base-layer floor is PINNED (min
    //    thumb DISABLED at position 0 — the base is always published), and only the
    //    ceiling (max) thumb moves, defaulting to the top position = the full
    //    ladder.
    //
    // CAPABILITY note: video/screen's effective depth is
    // `min(flag, capability_max_simulcast_layers())` and clamps to 1 on weak
    // (<6-core) runners; AUDIO's is `min(flag, 3)` and is NOT CPU-clamped (audio
    // encode is cheap, off the main thread), so audio typically stays 3 even on
    // weak runners. We read each ceiling thumb's `max` attribute (== effective
    // layers - 1) from the DOM and assert the DEFAULT ceiling == that max (the
    // "default == effective max" guarantee), so the single loop is correct for all
    // three regardless of runner capability. The pinned-floor assertions are
    // capability-independent.
    for (const stream of ["video", "screen", "audio"] as const) {
      // Reset not rendered while at the full default ladder.
      await expect(panel.locator(`[data-testid="perf-${stream}-auto"]`)).toHaveCount(0);
      const minInput = panel.locator(`[data-testid="perf-${stream}-range-min"]`);
      const maxInput = panel.locator(`[data-testid="perf-${stream}-range-max"]`);
      // FLOOR thumb is PINNED (disabled) at the base layer (position 0): the base
      // layer is always published and cannot be dragged off.
      await expect(minInput).toBeDisabled();
      await expect(minInput).toHaveValue("0");
      // CEILING thumb is interactive and defaults to the TOP position = full
      // ladder. Read the rendered top position (the `max` attr) and assert the
      // default value matches it — works for a 1-, 2-, or 3-layer ladder.
      await expect(maxInput).toBeEnabled();
      const topPos = await maxInput.getAttribute("max");
      expect(topPos, "ceiling slider exposes its max position").not.toBeNull();
      await expect(maxInput).toHaveValue(topPos as string);
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

  test("Reset button: absent at the full range, appears after a thumb drag, and clears back to the full range when clicked (#1131)", async ({
    page,
  }) => {
    await joinMeeting(page, "reset_button");
    await openPerformanceTab(page);
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");
    // The former "Auto" toggle is now a conditionally-rendered "Reset" button; the
    // `perf-video-auto` testid was repurposed onto it. It is not a toggle (no
    // aria-pressed) and is only present while the stream is constrained.
    const resetBtn = panel.locator('[data-testid="perf-video-auto"]');
    const minInput = panel.locator('[data-testid="perf-video-range-min"]');
    const maxInput = panel.locator('[data-testid="perf-video-range-max"]');

    // VIDEO SEND is now a LAYER-COUNT control: the floor (min) thumb is PINNED
    // (disabled) at the base layer (position 0 — always published), and only the
    // ceiling (max) thumb moves. Default = full ladder (ceiling at the top
    // position) → Reset is ABSENT.
    await expect(resetBtn).toHaveCount(0);
    await expect(minInput).toBeDisabled();
    await expect(maxInput).toBeEnabled();
    await expect(minInput).toHaveValue("0");
    // CAPABILITY-ADAPTIVE: read the rendered top position (== effective layers - 1).
    const topPosStr = await maxInput.getAttribute("max");
    expect(topPosStr, "ceiling slider exposes its max position").not.toBeNull();
    const topPos = Number(topPosStr);
    await expect(maxInput).toHaveValue(String(topPos));

    // On a runner whose capability ceiling clamped the publisher to a SINGLE layer
    // (topPos === 0, <6-core CI — see helpers/simulcast-config.ts), there is no
    // ceiling to lower: the drag is impossible and Reset can never appear. Skip the
    // drag/clear assertions there (the pinned-floor + default-ceiling assertions
    // above already ran and are capability-independent).
    test.skip(
      topPos < 1,
      "runner capability ceiling clamped the publisher to a single layer; the SEND " +
        "ceiling has only one position and cannot be lowered (see helpers/simulcast-config.ts)",
    );

    // Lower the ceiling by one (publish fewer top layers) → Reset APPEARS as a live
    // button. (The base floor stays pinned at 0 throughout.)
    const lowered = topPos - 1;
    await setRangeValue(page, "perf-video-range-max", lowered);
    await expect(maxInput).toHaveValue(String(lowered));
    await expect(minInput).toHaveValue("0");
    await expect(resetBtn).toBeVisible();
    await expect(resetBtn).toHaveText("Reset");
    await expect(resetBtn).not.toHaveAttribute("aria-pressed", /.*/);

    // Click Reset → ceiling snaps back to the full ladder (top), Reset DISAPPEARS,
    // the floor stays pinned/disabled, and the ceiling stays enabled throughout.
    await resetBtn.click();
    await expect(minInput).toHaveValue("0");
    await expect(maxInput).toHaveValue(String(topPos));
    await expect(resetBtn).toHaveCount(0);
    await expect(minInput).toBeDisabled();
    await expect(maxInput).toBeEnabled();
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
    // body (`HELP_VIDEO_SEND` in performance_settings.rs) now reads "Your camera
    // sends several quality versions ('layers') … The left handle sets the lowest
    // version you'll send (floor), the right handle the highest (ceiling); it
    // adapts within that band. … Reset returns to the full automatic range."
    // (#1131 §D rewrote this copy: the old "caps the best and worst versions
    // you'll send" / Auto-toggle wording is GONE — the source of truth is the
    // Rust constant.) Match the distinctive floor/ceiling-handle phrasing AND the
    // "Reset returns to the full automatic range" tail so a copy regression — or a
    // relapse to "Auto" wording — breaks this test.
    await helpBtn.click();
    await expect(helpBtn).toHaveAttribute("aria-expanded", "true");
    await expect(popover).toBeVisible();
    // The SEND video help body (`HELP_VIDEO_SEND` in performance_settings.rs) now
    // describes the LAYER-COUNT ceiling model: the base layer is ALWAYS sent and
    // the right handle sets the highest layer you publish (how many layers you
    // send). Match that distinctive phrasing AND the "Reset returns to the full
    // automatic ladder" tail so a copy regression — or a relapse to the old
    // draggable-floor wording — breaks this test.
    await expect(popover).toContainText(/the base layer is ALWAYS sent/i);
    await expect(popover).toContainText(/the right handle sets the HIGHEST layer you publish/i);
    await expect(popover).toContainText(/reset returns to the full automatic ladder/i);
    // The retired "Auto" toggle wording must NOT resurface in the help copy.
    await expect(popover).not.toContainText(/\bAuto\b/);
    // The old draggable-floor wording must NOT resurface (the floor is pinned now).
    await expect(popover).not.toContainText(/lowest version you'll send/i);

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

    // VIDEO SEND is a LAYER-COUNT control: the floor (min) thumb is PINNED at the
    // base layer (disabled), and lowering the CEILING (max) thumb publishes fewer
    // top layers. CAPABILITY-ADAPTIVE: read the rendered top position (== effective
    // layers - 1); on a single-layer runner (<6-core CI, topPos === 0) there is no
    // ceiling to lower, so skip the drag/persist/reload assertions there (the
    // capability ceiling is documented in helpers/simulcast-config.ts).
    const minInput = panel.locator('[data-testid="perf-video-range-min"]');
    const maxInputLoc = panel.locator('[data-testid="perf-video-range-max"]');
    await expect(minInput).toBeDisabled();
    const topPosStr = await maxInputLoc.getAttribute("max");
    expect(topPosStr, "ceiling slider exposes its max position").not.toBeNull();
    const topPos = Number(topPosStr);
    test.skip(
      topPos < 1,
      "runner capability ceiling clamped the publisher to a single layer; the SEND " +
        "ceiling cannot be lowered, so there is nothing to persist (see " +
        "helpers/simulcast-config.ts)",
    );

    // Drag the ceiling DOWN by one → publish (topPos) layers (L0..=L{topPos-1}).
    // The stored layer COUNT is `lowered + 1` (position+1); the total ladder is
    // `topPos + 1`. With the standard 3-layer ladder this is position 1 → 2 of 3.
    const lowered = topPos - 1;
    const expectedCount = lowered + 1;
    const totalLayers = topPos + 1;
    await setRangeValue(page, "perf-video-range-max", lowered);
    await expect(maxInputLoc).toHaveValue(String(lowered));
    // Floor stays pinned at the base throughout.
    await expect(minInput).toHaveValue("0");
    // With the ceiling below full, the Reset button is now RENDERED.
    await expect(panel.locator('[data-testid="perf-video-auto"]')).toBeVisible();

    // localStorage must reflect the chosen layer COUNT.
    // Poll because the controlled-component write is async to the event.
    await expect
      .poll(async () => (await readPerfPref(page))?.video_layers, { timeout: 10_000 })
      .toBe(expectedCount);

    const pref = await readPerfPref(page);
    expect(pref, "vc_performance_quality must be written").not.toBeNull();
    // The SEND layer control OWNS the video send axis: it forces the tier bounds
    // to Auto so the AQ adapts each layer's bitrate freely (no competing slider).
    expect(pref?.video_layers, "video_layers persisted as a layer count").toBe(expectedCount);
    expect(pref?.video_auto, "tier axis freed to Auto").toBe(true);
    expect(pref?.video_max, "tier max cleared").toBeNull();
    expect(pref?.video_min, "tier min cleared").toBeNull();
    // The other streams' layer ceilings stay on Auto (we only touched video).
    expect(pref?.screen_layers ?? null).toBeNull();

    // Capture the visible count caption to compare after reload. The SEND layer
    // range-value reads "Sending {N} of {M} layers"; it must read the adaptive
    // count and never the "Auto" word.
    const rangeValueBefore = await panel
      .locator('[data-testid="perf-video-range-value"]')
      .textContent();
    expect(rangeValueBefore).not.toBeNull();
    expect(rangeValueBefore).toContain(`Sending ${expectedCount} of ${totalLayers} layers`);
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

    // Lowered ceiling restored (not the full ladder) → the Reset button is
    // RENDERED, the ceiling thumb is back at the lowered position, and the base
    // floor is still pinned (disabled).
    await expect(panelAfter.locator('[data-testid="perf-video-auto"]')).toBeVisible();
    await expect(panelAfter.locator('[data-testid="perf-video-range-min"]')).toBeDisabled();
    await expect(panelAfter.locator('[data-testid="perf-video-range-max"]')).toHaveValue(
      String(lowered),
    );

    // The restored count caption matches what was set before the reload.
    await expect(panelAfter.locator('[data-testid="perf-video-range-value"]')).toHaveText(
      rangeValueBefore as string,
    );
    // And localStorage still carries the persisted layer-count ceiling.
    await expect
      .poll(async () => (await readPerfPref(page))?.video_layers, { timeout: 10_000 })
      .toBe(expectedCount);
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

  test("no SEND 'Fixed' badge: all three SEND controls are pinned-floor layer-count sliders", async ({
    page,
  }) => {
    await joinMeeting(page, "no_fixed_badge");
    await openPerformanceTab(page);
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");

    // The "Fixed" badge was a TIER-slider concept (both thumbs pinned to one
    // tier). ALL THREE SEND controls (video, screen, AND audio) are now
    // layer-count sliders with a fixed base-layer floor and an adjustable ceiling
    // — there is no tier-pinning and therefore no SEND "Fixed" badge for any kind.
    // Guard that none renders (a regression that revived the tier slider would
    // bring the badge back). The audio floor is also pinned/disabled like the
    // others (it is no longer a draggable tier min).
    for (const stream of ["video", "audio", "screen"] as const) {
      await expect(panel.locator(`[data-testid="perf-${stream}-fixed-badge"]`)).toHaveCount(0);
      await expect(panel.locator(`[data-testid="perf-${stream}-range-min"]`)).toBeDisabled();
    }
  });

  test("send rung strip + directional arrows render (#1131)", async ({ page }) => {
    // §2: AUDIO now renders a SELECTION-DRIVEN multi-pip SEND rung strip (the
    // layer-count control builds one pip per effective audio layer from the user's
    // ceiling, like video/screen — no dependence on a live encoder snapshot). The
    // strip is present even single-page with no peers, so it is the deterministic
    // anchor for the strip markup. §1: each side title is prefixed with an
    // aria-hidden directional arrow (`.perf-dir-arrow`).
    await joinMeeting(page, "send_rungs");
    await openPerformanceTab(page);
    await selectSendDirection(page);

    const panel = page.locator("#settings-panel-performance");

    // The audio send rung strip is a role=img container with at least one pip.
    const audioStrip = panel.locator('[data-testid="perf-audio-send-rungs"]');
    await expect(audioStrip).toBeVisible({ timeout: 5_000 });
    await expect(audioStrip).toHaveAttribute("role", "img");
    // SHAPE assertion: >= 1 pip (one per effective audio layer). Do NOT hard-code
    // the count — audio's effective ladder is `min(flag, 3)`; the base pip is
    // always present. Its testid is `perf-audio-send-rung-{layer-id}`.
    const audioPips = panel.locator('[data-testid^="perf-audio-send-rung-"]');
    expect(await audioPips.count()).toBeGreaterThanOrEqual(1);
    // The base pip (layer 0) is always rendered + always active (the pinned floor).
    await expect(panel.locator('[data-testid="perf-audio-send-rung-0"]')).toBeVisible();

    // §1 directional arrows: present on BOTH a Sending and a Receiving title, and
    // aria-hidden so they are decorative (the title text is the a11y label).
    const arrows = panel.locator(".perf-dir-arrow");
    expect(await arrows.count()).toBeGreaterThanOrEqual(2);
    await expect(arrows.first()).toHaveAttribute("aria-hidden", "true");
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

  test("receive row renders a range slider, needle, and help for each kind (Reset absent at full range)", async ({
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
      // The Reset button is conditionally rendered (#1131 §D): ABSENT at the
      // default full range, present only when constrained. This block joins
      // single-page with no manual bounds, so it must be absent here.
      await expect(
        panel.locator(`[data-testid="perf-recv-${kind}-auto"]`),
        `${kind} receive Reset button absent at the full default range`,
      ).toHaveCount(0);
      await expect(
        panel.locator(`[data-testid="perf-recv-${kind}-help"]`),
        `${kind} receive help button present`,
      ).toBeVisible();
    }
  });

  test("receive per-peer disclosure: the empty state renders NO <details> for any kind (#1131 §3)", async ({
    page,
  }) => {
    // §3: the receive side renders a native <details> per-peer disclosure
    // (`perf-recv-{kind}-peers`) ONLY when at least one peer is decoding that
    // kind — the Rust source gates it behind `if !peers.is_empty()`
    // (performance_settings.rs). This test joins single-page (no peers), so the
    // EMPTY state must render zero disclosures for EVERY kind. The populated
    // disclosure + per-peer quality-dot / reason-chip assertions need a real
    // multi-peer meeting and live in simulcast-per-receiver.spec.ts (blocked on
    // the #1093 multi-peer harness). This is the single-context half of §3 and is
    // a real regression guard: if the empty-state gate regressed (a stray
    // disclosure rendered with no peers), this fails.
    await joinMeeting(page, "recv_peers_empty");
    await openPerformanceTab(page);
    await selectReceiveDirection(page);

    const panel = page.locator("#settings-panel-performance");

    for (const kind of ["video", "audio", "screen"] as const) {
      // No <details> disclosure…
      await expect(
        panel.locator(`[data-testid="perf-recv-${kind}-peers"]`),
        `${kind} receive per-peer disclosure absent in the empty (no-peer) state`,
      ).toHaveCount(0);
      // …and therefore no summary and no per-peer rows / dots / reason chips.
      await expect(
        panel.locator(`[data-testid="perf-recv-${kind}-peers-summary"]`),
        `${kind} receive per-peer summary absent in the empty state`,
      ).toHaveCount(0);
      await expect(
        panel.locator(`[data-testid^="perf-recv-${kind}-peer-"]`),
        `${kind} receive per-peer rows absent in the empty state`,
      ).toHaveCount(0);
    }
  });

  test("receive row defaults to the full range with the Reset button absent and thumbs at the extremes", async ({
    page,
  }) => {
    await joinMeeting(page, "recv_auto_default");
    await openPerformanceTab(page);
    await selectReceiveDirection(page);

    const panel = page.locator("#settings-panel-performance");

    for (const kind of ["video", "audio", "screen"] as const) {
      // Default = full automatic range: the Reset button (repurposed from the
      // former Auto toggle, same `perf-recv-{kind}-auto` testid) is ABSENT, the
      // slider is fully interactive, and thumbs are pinned to the extremes (min at
      // 0, max at the ladder top).
      await expect(panel.locator(`[data-testid="perf-recv-${kind}-auto"]`)).toHaveCount(0);

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

  test("receive Reset button appears after a thumb drag and clears back to the full range (#1131)", async ({
    page,
  }) => {
    await joinMeeting(page, "recv_reset_button");
    await openPerformanceTab(page);
    await selectReceiveDirection(page);

    const panel = page.locator("#settings-panel-performance");
    const resetBtn = panel.locator('[data-testid="perf-recv-video-auto"]');
    const minInput = panel.locator('[data-testid="perf-recv-video-range-min"]');
    const maxInput = panel.locator('[data-testid="perf-recv-video-range-max"]');

    // Default full range → Reset ABSENT (only rendered when constrained).
    await expect(resetBtn).toHaveCount(0);

    // Drag the max thumb inward → manual limits → Reset APPEARS as a live button
    // (not a toggle, so no aria-pressed); the slider is never disabled.
    await setRangeValue(page, "perf-recv-video-range-max", 0);
    await expect(maxInput).toHaveValue("0");
    await expect(resetBtn).toBeVisible();
    await expect(resetBtn).not.toHaveAttribute("aria-pressed", /.*/);
    await expect(minInput).toBeEnabled();

    // Click Reset → thumbs snap back to the full range and Reset DISAPPEARS.
    await resetBtn.click();
    await expect(minInput).toHaveValue("0");
    const top = await maxInput.getAttribute("max");
    await expect(maxInput).toHaveValue(String(top));
    await expect(resetBtn).toHaveCount(0);
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

    // Pin both video RECEIVE thumbs to the same interior layer so min == max →
    // the receive Fixed badge appears. Dragging a thumb leaves the full automatic
    // range (no toggle click — the former Auto toggle is now a Reset button).
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
