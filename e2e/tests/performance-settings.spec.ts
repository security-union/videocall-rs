import { test, expect, Page, Locator } from "@playwright/test";
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
 * namespace (further down).
 *
 *   #1131 RELOCATION: the whole panel MOVED out of the Settings → Performance
 *   modal tab into the right-side **Diagnostics drawer** (`#diagnostics-sidebar`),
 *   mounted as "Group A — Quality controls" above the live diagnostics sections.
 *   The drawer title became "Performance & Diagnostics". The Settings modal now
 *   has FOUR tabs (Audio / Video / Network / Appearance) plus a transitional
 *   `settings-perf-moved` link-row that closes the modal and opens the drawer.
 *   The `perf-open-diagnostics` / `diag-open-performance` cross-nav buttons (and
 *   their `#settings-panel-performance` tabpanel wrapper) are GONE — the panel
 *   now renders directly inside `.sidebar-content`, so its `perf-*` testids are
 *   scoped to `#diagnostics-sidebar` (NOT a settings tabpanel). The `perf-*`
 *   COMPONENTS themselves are byte-for-byte unchanged — only the mount moved, so
 *   every slider / Auto / meter / strip assertion below survives with just the
 *   opening flow swapped from "open Settings → Performance tab" to "open the
 *   Diagnostics drawer".
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
 * The Performance controls now live in the in-meeting **Diagnostics drawer**, so
 * each test must be in a real meeting room first. We reuse the PROVEN in-meeting
 * flow: inject the session cookie (helpers/auth.ts) on the default
 * `dioxus`-project `page` (whose Chromium flags already include
 * `--use-fake-device-for-media-stream` so the camera produces a synthetic
 * stream), drive the home-page meeting form, click through "Start/Join Meeting"
 * to `#grid-container`, then open the drawer via the toolbar "Open Diagnostics"
 * button (the canonical opener also used by protocol-selection.spec.ts /
 * diagnostics-peer-transport.spec.ts). The perf panel renders as Group A inside
 * `#diagnostics-sidebar`; `openPerformanceDrawer` waits on the migrated simulcast
 * strip (`perf-simulcast-strip`) appearing INSIDE that scope as readiness +
 * relocation proof.
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

/**
 * The Diagnostics drawer root. The Performance panel (#1131) is mounted as
 * "Group A — Quality controls" inside `.sidebar-content` here, so EVERY perf
 * assertion scopes to this locator (not the dead `#settings-panel-performance`
 * tabpanel). Scoping inside the drawer is what makes the relocation a real
 * regression guard: a `perf-*` testid that resurfaced anywhere ELSE on the page
 * (e.g. a relapsed Settings tab) would NOT satisfy `sidebar.locator(...)`.
 */
function perfDrawer(page: Page): Locator {
  return page.locator("#diagnostics-sidebar");
}

/**
 * Open the in-meeting Diagnostics drawer via the toolbar "Open Diagnostics"
 * button — the new (and only) home of the Performance controls (#1131). Returns
 * once the drawer is open with the migrated panel inside it.
 *
 * MUTATION DISCIPLINE: we wait on the migrated simulcast strip
 * (`perf-simulcast-strip`) being visible *inside* `#diagnostics-sidebar`, not
 * merely anywhere on the page. If the panel failed to mount in the drawer (the
 * relocation regressed), this helper throws and every dependent test fails.
 */
async function openPerformanceDrawer(page: Page): Promise<void> {
  // The diagnostics button carries no data-testid; locate it via its tooltip
  // text (mirrors protocol-selection.spec.ts::openDiagnosticsPanel).
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await diagButton.click();
  const sidebar = perfDrawer(page);
  await expect(sidebar).toBeVisible({ timeout: 10_000 });
  // The drawer title renamed to "Performance & Diagnostics" (#1131 §4).
  await expect(sidebar.getByRole("heading", { name: "Performance & Diagnostics" })).toBeVisible({
    timeout: 5_000,
  });
  // Relocation proof: the migrated panel's simulcast strip is present INSIDE the
  // drawer (Group A). This fails if the panel didn't move into the drawer.
  await expect(sidebar.locator('[data-testid="perf-simulcast-strip"]')).toBeVisible({
    timeout: 5_000,
  });
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

/**
 * Assert a SEND layer slider's PINNED FLOOR (min thumb) contract.
 *
 * WEBKIT REGRESSION GUARD: the floor must be non-interactive WITHOUT the HTML
 * `disabled` attribute. A `disabled`, full-width, on-top range input swallows the
 * pointer-down meant for the max (ceiling) thumb beneath it in WebKit/Safari
 * (WebKit doesn't reliably pass `pointer-events:none` through a disabled control),
 * making the ceiling undraggable. So the floor is pinned via `tabindex=-1` (no
 * keyboard) + `aria-disabled` (SR) + CSS `pointer-events:none` (no pointer) +
 * `z-index:0` (below the max) — but is NOT HTML-`disabled`. Playwright's
 * `toBeEnabled()` also treats `aria-disabled="true"` as disabled, so assert the
 * raw DOM attribute directly. Pinned at position 0 (the always-sent base layer).
 */
async function expectPinnedFloor(minInput: Locator): Promise<void> {
  // NOT HTML-`disabled` — the WebKit fix. `aria-disabled` is expected and is
  // asserted below, so `toBeEnabled()` would be the wrong matcher here.
  await expect
    .poll(async () => minInput.evaluate((el) => (el as HTMLInputElement).hasAttribute("disabled")))
    .toBe(false);
  await expect(minInput).toHaveAttribute("tabindex", "-1");
  await expect(minInput).toHaveAttribute("aria-disabled", "true");
  await expect(minInput).toHaveValue("0");
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
    await openPerformanceDrawer(page);
    // Both directions render together now (no toggle); this guards the send
    // meters rendered before we assert them.
    await selectSendDirection(page);

    const panel = perfDrawer(page);

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
      // FLOOR thumb is PINNED at the base layer (position 0): the base is always
      // published. Non-interactive WITHOUT the `disabled` attr (WebKit fix — see
      // expectPinnedFloor).
      await expectPinnedFloor(minInput);
      // CEILING thumb is interactive and defaults to the TOP position = full
      // ladder. Read the rendered top position (the `max` attr) and assert the
      // default value matches it — works for a 1-, 2-, or 3-layer ladder.
      await expect(maxInput).toBeEnabled();
      const topPos = await maxInput.getAttribute("max");
      expect(topPos, "ceiling slider exposes its max position").not.toBeNull();
      await expect(maxInput).toHaveValue(topPos as string);
      // TICK MARKS: one decorative notch per step position (0..=max → max+1
      // ticks), aligned to the thumb stops. The ticks layer is aria-hidden +
      // pointer-events:none (must never block the drag — the WebKit fix).
      const expectedTicks = Number(topPos) + 1;
      const ticksLayer = panel.locator(`[data-testid="perf-${stream}-range-ticks"]`);
      await expect(ticksLayer).toHaveAttribute("aria-hidden", "true");
      await expect(ticksLayer.locator(".perf-range-tick")).toHaveCount(expectedTicks);
    }
  });

  test("desktop layout: Group A perf cards stay contained in the drawer (#1208/#1213, adapted to the drawer)", async ({
    page,
  }) => {
    // The drawer (#1131) is a SINGLE SCROLLING SURFACE by design: Group A (perf
    // controls) sits above the live diagnostics groups, so the content is
    // legitimately taller than the viewport and the drawer scrolls vertically.
    // The #1208/#1213 lesson: do NOT assert vertical overflow in either
    // direction (a scrolling-by-design surface SHOULD overflow vertically). The
    // stable contract is (a) NO horizontal overflow, and (b) the LAST Group-A
    // perf card is reachable by scrolling the drawer's own scroll container.
    await page.setViewportSize({ width: 1280, height: 768 });
    await joinMeeting(page, "drawer_containment");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);
    await expect(panel).toBeVisible();
    // All three Group-A perf cards must be present (so we're measuring the full
    // content, not a partially-rendered panel).
    await expect(panel.locator(".perf-kind-card")).toHaveCount(3);

    // The drawer's scroll container is `.sidebar-content` (it carries
    // overflow-y:auto inside `#diagnostics-sidebar`).
    const scrollContainer = panel.locator(".sidebar-content");
    await expect(scrollContainer).toBeVisible();
    const horizontalOverflow = await scrollContainer.evaluate(
      (el) => el.scrollWidth - el.clientWidth,
    );
    expect(
      horizontalOverflow,
      "drawer Group-A content must not overflow horizontally",
    ).toBeLessThanOrEqual(1);

    // Scroll to the bottom of the drawer and confirm the LAST Group-A perf card
    // is reachable (it sits above Groups B/C, so this also proves Group A is not
    // clipped off the top of an over-tall surface).
    await scrollContainer.evaluate((el) => {
      el.scrollTop = el.scrollHeight;
    });
    await scrollContainer.evaluate((el) => {
      // The last perf card is above the live groups; scroll it into view.
      const cards = el.querySelectorAll(".perf-kind-card");
      cards[cards.length - 1]?.scrollIntoView({ block: "center" });
    });
    await expect(panel.locator(".perf-kind-card").last()).toBeVisible();
  });

  test("Reset button: absent at the full range, appears after a thumb drag, and clears back to the full range when clicked (#1131)", async ({
    page,
  }) => {
    await joinMeeting(page, "reset_button");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);
    // The former "Auto" toggle is now a conditionally-rendered "Reset" button; the
    // `perf-video-auto` testid was repurposed onto it. It is not a toggle (no
    // aria-pressed) and is only present while the stream is constrained.
    const resetBtn = panel.locator('[data-testid="perf-video-auto"]');
    const minInput = panel.locator('[data-testid="perf-video-range-min"]');
    const maxInput = panel.locator('[data-testid="perf-video-range-max"]');

    // VIDEO SEND is now a LAYER-COUNT control: the floor (min) thumb is PINNED at
    // the base layer (position 0 — always published, non-interactive but NOT
    // `disabled`; see expectPinnedFloor / the WebKit fix), and only the ceiling
    // (max) thumb moves. Default = full ladder (ceiling at the top) → Reset ABSENT.
    await expect(resetBtn).toHaveCount(0);
    await expectPinnedFloor(minInput);
    await expect(maxInput).toBeEnabled();
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
    // the floor stays pinned (non-interactive, not `disabled`), and the ceiling
    // stays enabled throughout.
    await resetBtn.click();
    await expect(maxInput).toHaveValue(String(topPos));
    await expect(resetBtn).toHaveCount(0);
    await expectPinnedFloor(minInput);
    await expect(maxInput).toBeEnabled();
  });

  test("help popover: '?' opens an explanation, Escape and outside-click dismiss it", async ({
    page,
  }) => {
    await joinMeeting(page, "help_popover");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);
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

  test("panel intro is collapsed behind a header info (i) icon that reveals the explanation on click", async ({
    page,
  }) => {
    await joinMeeting(page, "intro_collapsed");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);

    // The big always-visible intro paragraph (it used to be a
    // `.settings-section-description` in the panel) is GONE — collapsed behind a
    // header (i) info button (the shared HelpPopover). Guard the always-visible
    // intro prose is not rendered, and the explanation is reachable on demand.
    const introBtn = panel.locator('[data-testid="perf-intro-help"]');
    const introPopover = page.locator("#perf-intro-help-popover");

    // The intro icon exists; the popover is closed by default (copy not visible).
    await expect(introBtn).toBeVisible();
    await expect(introBtn).toHaveAttribute("aria-expanded", "false");
    await expect(introPopover).toHaveCount(0);
    // The distinctive intro phrasing is NOT shown as always-visible body text.
    await expect(panel).not.toContainText("Each stream adapts to your connection");

    // Click the (i) → the explanation appears in the popover with the intro copy
    // (source of truth: HELP_PERF_INTRO in performance_settings.rs). Match the
    // distinctive opening + the "meter shows what's flowing" tail.
    await introBtn.click();
    await expect(introBtn).toHaveAttribute("aria-expanded", "true");
    await expect(introPopover).toBeVisible();
    await expect(introPopover).toContainText(
      /Each stream adapts to your connection automatically/i,
    );
    await expect(introPopover).toContainText(/the meter shows what's flowing right now/i);

    // Escape closes it (keyboard-operable) and returns aria-expanded to false.
    await page.keyboard.press("Escape");
    await expect(introBtn).toHaveAttribute("aria-expanded", "false");
    await expect(introPopover).toHaveCount(0);
  });

  test("setting a video threshold persists to localStorage and restores after reload", async ({
    page,
  }) => {
    await joinMeeting(page, "persist");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);

    // VIDEO SEND is a LAYER-COUNT control: the floor (min) thumb is PINNED at the
    // base layer (non-interactive, NOT `disabled` — WebKit fix), and lowering the
    // CEILING (max) thumb publishes fewer top layers. CAPABILITY-ADAPTIVE: read the
    // rendered top position (== effective layers - 1); on a single-layer runner
    // (<6-core CI, topPos === 0) there is no ceiling to lower, so skip the
    // drag/persist/reload assertions there (see helpers/simulcast-config.ts).
    const minInput = panel.locator('[data-testid="perf-video-range-min"]');
    const maxInputLoc = panel.locator('[data-testid="perf-video-range-max"]');
    await expectPinnedFloor(minInput);
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
    // The stored layer COUNT is `lowered + 1` (position+1). With the standard
    // 3-layer ladder this is position 1 → count 2.
    const lowered = topPos - 1;
    const expectedCount = lowered + 1;
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
    // range-value caption is SOURCE-AWARE: with the camera ON it reads
    // "Sending {N} of {M} layers"; with the camera OFF (this test doesn't drive
    // the camera) it reads "Will send {N} layers when the camera is on". Either
    // way it must (a) name the configured count N, (b) never read "Auto", and
    // (c) survive the reload unchanged. We assert on the COUNT (the persistence
    // contract), not the present/future verb.
    const rangeValueBefore = await panel
      .locator('[data-testid="perf-video-range-value"]')
      .textContent();
    expect(rangeValueBefore).not.toBeNull();
    expect(rangeValueBefore).toMatch(new RegExp(`\\b${expectedCount}\\b.*layer`));
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

    await openPerformanceDrawer(page);
    // Both directions render together after reload (no toggle); guard that the
    // send meters are present before re-reading the persisted send-side video
    // preference.
    await selectSendDirection(page);
    const panelAfter = perfDrawer(page);

    // Lowered ceiling restored (not the full ladder) → the Reset button is
    // RENDERED, the ceiling thumb is back at the lowered position, and the base
    // floor is still pinned (non-interactive, not `disabled`).
    await expect(panelAfter.locator('[data-testid="perf-video-auto"]')).toBeVisible();
    await expectPinnedFloor(panelAfter.locator('[data-testid="perf-video-range-min"]'));
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
    await openPerformanceDrawer(page);
    // Both directions render together now; the send meters are always present.
    await selectSendDirection(page);

    const panel = perfDrawer(page);

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
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);

    // The "Fixed" badge was a TIER-slider concept (both thumbs pinned to one
    // tier). ALL THREE SEND controls (video, screen, AND audio) are now
    // layer-count sliders with a fixed base-layer floor and an adjustable ceiling
    // — there is no tier-pinning and therefore no SEND "Fixed" badge for any kind.
    // Guard that none renders (a regression that revived the tier slider would
    // bring the badge back). The floor for every kind is pinned (non-interactive
    // but NOT `disabled` — the WebKit fix) rather than a draggable tier min.
    for (const stream of ["video", "audio", "screen"] as const) {
      await expect(panel.locator(`[data-testid="perf-${stream}-fixed-badge"]`)).toHaveCount(0);
      await expectPinnedFloor(panel.locator(`[data-testid="perf-${stream}-range-min"]`));
    }
  });

  test("SEND ceiling thumb is grabbable (WebKit pinned-floor regression): max draggable + value-settable; floor non-interactive without `disabled`", async ({
    page,
  }) => {
    // REGRESSION GUARD for the WebKit pinned-floor bug: a `disabled`, on-top,
    // full-width range input (the old way of pinning the floor) swallowed the
    // pointer-down meant for the max (ceiling) thumb in WebKit/Safari, so the
    // ceiling could not be dragged. The fix pins the floor via tabindex=-1 +
    // aria-disabled + CSS pointer-events:none + z-index:0 (below the max) and does
    // NOT set `disabled`. We use AUDIO because its effective ladder is
    // `min(flag, 3)` and is NOT CPU-clamped, so it always has ≥2 ceiling positions
    // on any runner (no skip-guard needed) — the deterministic drag target.
    await enableSimulcastFlag(page.context(), 3);
    await joinMeeting(page, "send_ceiling_grabbable");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);
    const minInput = panel.locator('[data-testid="perf-audio-range-min"]');
    const maxInput = panel.locator('[data-testid="perf-audio-range-max"]');

    // (a) DOM contract: the max is interactive/enabled and the floor is pinned
    // WITHOUT HTML-`disabled` (the WebKit fix).
    await expect(maxInput).toBeEnabled();
    await expectPinnedFloor(minInput);
    // Audio is capability-independent (~3 layers), so the ceiling starts at the
    // top with at least one position to lower into.
    const topPos = Number(await maxInput.getAttribute("max"));
    expect(topPos, "audio ceiling must have ≥1 position to drag").toBeGreaterThanOrEqual(1);
    await expect(maxInput).toHaveValue(String(topPos));

    // (b) REAL POINTER DRAG on the max thumb's pixels — this is what the bug broke.
    // The thumb sits at the far right (value == max). Press at the right edge of
    // the track and drag left toward the track centre; the drag must reach the max
    // input (nothing disabled/opaque above it now) and lower the value. If the
    // pinned floor were still swallowing the pointer, the value would not change.
    const box = await maxInput.boundingBox();
    expect(box, "max input has a layout box").not.toBeNull();
    const b = box as { x: number; y: number; width: number; height: number };
    const y = b.y + b.height / 2;
    await page.mouse.move(b.x + b.width - 6, y); // near the right-edge thumb
    await page.mouse.down();
    await page.mouse.move(b.x + b.width * 0.4, y, { steps: 12 }); // drag left
    await page.mouse.up();

    // The ceiling value must have DROPPED below the top (the drag landed on the
    // max thumb). Poll because Dioxus's controlled re-render is async to the event.
    await expect
      .poll(async () => Number(await maxInput.getAttribute("value")), { timeout: 5_000 })
      .toBeLessThan(topPos);
    // The floor never moved (stayed pinned at the base).
    await expect(minInput).toHaveValue("0");

    // (c) Value-settable fallback (independent of pointer hit-testing): driving the
    // max input updates the persisted audio layer count + the caption, proving the
    // control is live-wired regardless of the drag path.
    await setRangeValue(page, "perf-audio-range-max", 0);
    await expect
      .poll(async () => (await readPerfPref(page))?.audio_layers, { timeout: 10_000 })
      .toBe(1); // position 0 → 1 layer (base only)
    // The caption reflects the new count of 1. It is SOURCE-AWARE: mic on →
    // "Sending 1 layer"; mic off (this single-page test) → "Will send 1 layer when
    // the mic is on". Assert on the COUNT (the wiring contract), not the verb.
    await expect(panel.locator('[data-testid="perf-audio-range-value"]')).toContainText(
      /\b1 layer/,
    );
  });

  test("send rung strip + directional arrows render (#1131)", async ({ page }) => {
    // §2: AUDIO now renders a SELECTION-DRIVEN multi-pip SEND rung strip (the
    // layer-count control builds one pip per effective audio layer from the user's
    // ceiling, like video/screen — no dependence on a live encoder snapshot). The
    // strip is present even single-page with no peers, so it is the deterministic
    // anchor for the strip markup. §1: each side title is prefixed with an
    // aria-hidden directional arrow (`.perf-dir-arrow`).
    await joinMeeting(page, "send_rungs");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);

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

  test("SEND caption is source-aware: OFF sources read 'Will send …' (not 'Sending')", async ({
    page,
  }) => {
    // This is a single-page meeting with no media driven (no ensureCameraOn, mic
    // off), so all three SEND sources are OFF. The caption must NOT falsely claim
    // to be "Sending" — it reads the future "Will send {N} … when {…}" form using
    // the configured count, and names each kind's trigger.
    await enableSimulcastFlag(page.context(), 3);
    await joinMeeting(page, "caption_source_aware");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);
    // Per-kind trigger phrase in the OFF-state caption.
    const triggers: Record<string, RegExp> = {
      video: /Will send \d+ layers? when the camera is on/,
      screen: /Will send \d+ layers? when sharing/,
      audio: /Will send \d+ layers? when the mic is on/,
    };
    for (const stream of ["video", "audio", "screen"] as const) {
      const caption = panel.locator(`[data-testid="perf-${stream}-range-value"]`);
      // Future form present; never the present-tense "Sending …" while OFF.
      await expect(caption).toHaveText(triggers[stream]);
      await expect(caption).not.toContainText("Sending");
    }
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
    await openPerformanceDrawer(page);

    const panel = perfDrawer(page);

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

    // SINGLE SURFACE (#1131): the panel now lives INSIDE the Diagnostics drawer,
    // so the former Perf→Diagnostics cross-nav button (`perf-open-diagnostics`) is
    // gone — there is nowhere left to navigate to. Assert it no longer renders,
    // and that the receive controls we just checked are inside `#diagnostics-sidebar`
    // (the relocation), not on a stray Settings tab.
    await expect(panel.locator('[data-testid="perf-open-diagnostics"]')).toHaveCount(0);
    await expect(
      perfDrawer(page).locator('[data-testid="perf-vu-recv-video"]'),
      "receive controls render inside the Diagnostics drawer (relocation proof)",
    ).toBeVisible();
  });

  test("receive row renders a range slider, needle, and help for each kind (Reset absent at full range)", async ({
    page,
  }) => {
    await joinMeeting(page, "recv_render");
    await openPerformanceDrawer(page);
    // Both directions render together (no toggle); guard the receive meters
    // rendered before asserting the receive controls.
    await selectReceiveDirection(page);

    const panel = perfDrawer(page);

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
    await openPerformanceDrawer(page);
    await selectReceiveDirection(page);

    const panel = perfDrawer(page);

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
    await openPerformanceDrawer(page);
    await selectReceiveDirection(page);

    const panel = perfDrawer(page);

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
    await openPerformanceDrawer(page);
    await selectReceiveDirection(page);

    const panel = perfDrawer(page);
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
    await openPerformanceDrawer(page);
    await selectReceiveDirection(page);

    const panel = perfDrawer(page);

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
    await openPerformanceDrawer(page);
    await selectReceiveDirection(page);

    const panel = perfDrawer(page);

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
// Single-surface unification (#1131) + the "Simulcast layers" diagnostics
// section (#1095, now Group B of the same drawer).
//
// #1131 collapsed the two surfaces into ONE: the Performance panel moved INTO
// the Diagnostics drawer, so the former Perf↔Diag cross-nav buttons
// (`perf-open-diagnostics` / `diag-open-performance`) are GONE — there is no
// second surface to navigate to. The old round-trip cross-nav test is replaced
// below by (a) a single-surface assertion that the perf panel (Group A) and the
// "Simulcast layers" section (Group B) coexist in ONE open drawer, and (b) a
// round-trip test of the transitional Settings `settings-perf-moved` row.
//
// The "Simulcast layers" section is fed by the live `DiagnosticsReader` Host
// publishes, so:
//   * Video (sending) static line "Camera — off" when the camera is off — the
//     #1101 stale-count regression, now asserted in its new home.
//   * Screen (sending) static line "Screen — not sharing" with no active share.
//   * The per-layer ladder (`diag-simulcast-ladder` + `diag-simulcast-rung-{id}`)
//     renders >= 1 rung when simulcast is active (capability-gated, SHAPE only).
//
// The drawer is opened via the single `openPerformanceDrawer` helper (the
// toolbar "Open Diagnostics" button). All live-content assertions use
// `expect.poll` (the section refreshes on a ~4 Hz tick).
// ---------------------------------------------------------------------------

/** Trimmed text of the named "Simulcast layers" SEND ladder block (by title). */
async function simulcastSendText(page: Page, title: string): Promise<string> {
  const block = page
    .locator(".simulcast-send")
    .filter({ has: page.locator(`.simulcast-send-title:text-is("${title}")`) });
  const t = await block.textContent().catch(() => null);
  return (t ?? "").trim();
}

test.describe("Unified Performance + Diagnostics drawer (#1131) + Simulcast layers (#1095)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL, page }) => {
    await injectSessionCookie(context, { baseURL });
    // Flag ON for THIS browser only so the publisher CAN run simulcast (the
    // ladder-shape test is still capability-gated below).
    await enableSimulcastFlag(page.context(), 3);
  });

  test("single surface: the perf panel (Group A) and Simulcast layers (Group B) coexist in ONE open drawer", async ({
    page,
  }) => {
    await joinMeeting(page, "single_surface");

    // ONE open action puts BOTH the perf controls and the live diagnostics on
    // screen at once — the whole point of #1131. `openPerformanceDrawer` already
    // asserts the title + the migrated simulcast strip inside the drawer.
    await openPerformanceDrawer(page);
    const sidebar = perfDrawer(page);

    // GROUP A — the migrated Performance panel. Assert a Group-A perf control is
    // visible INSIDE the drawer (relocation proof, not anywhere on the page).
    await expect(
      sidebar.locator('[data-testid="perf-simulcast-strip"]'),
      "Group A perf strip inside the drawer",
    ).toBeVisible();
    await expect(
      sidebar.locator('[data-testid="perf-vu-video"]'),
      "Group A send video meter inside the drawer",
    ).toBeVisible();

    // GROUP B — the "Simulcast layers" section coexists in the SAME drawer
    // (#1095 §6 MOVE). Assert the heading AND the moved sub-structure so this
    // fails if the section regressed (a bare heading could survive an empty
    // section). Single-context (camera off / no peers) so the live ladder +
    // per-peer testids are NOT in the DOM; assert the always-present structure:
    // both SEND blocks (by title) and the per-peer RECEIVE sub-section header.
    await expect(sidebar.getByRole("heading", { name: "Simulcast layers" })).toBeVisible();
    await expect(sidebar.locator('.simulcast-send-title:text-is("Video (sending)")')).toBeVisible();
    await expect(
      sidebar.locator('.simulcast-send-title:text-is("Screen (sending)")'),
    ).toBeVisible();
    await expect(sidebar.locator(".simulcast-recv-title")).toBeVisible();

    // The cross-nav buttons are GONE on both sides of the (now single) surface.
    await expect(sidebar.locator('[data-testid="diag-open-performance"]')).toHaveCount(0);
    await expect(sidebar.locator('[data-testid="perf-open-diagnostics"]')).toHaveCount(0);
  });

  test("Settings has FOUR tabs and the `settings-perf-moved` row opens the drawer (real round-trip)", async ({
    page,
  }) => {
    await joinMeeting(page, "perf_moved_row");

    // ── The Settings modal now has exactly FOUR tabs (Performance tab removed) ──
    await openSettingsModal(page);
    const modal = page.locator(".device-settings-modal");
    const tabs = modal.getByRole("tab");
    await expect(tabs).toHaveCount(4);
    // The four surviving tabs, by accessible name.
    for (const name of ["Audio", "Video", "Network", "Appearance"] as const) {
      await expect(modal.getByRole("tab", { name }), `${name} tab present`).toBeVisible();
    }
    // The Performance tab is GONE — it is NOT one of the tabs.
    await expect(modal.getByRole("tab", { name: "Performance" })).toHaveCount(0);
    await expect(modal.locator('[data-testid="settings-nav-performance"]')).toHaveCount(0);

    // ── The transitional "moved" row: present below the tablist, NOT a tab ──
    const movedRow = modal.locator('[data-testid="settings-perf-moved"]');
    await expect(movedRow).toBeVisible();
    // It is a link-styled affordance (role="link"), not a tab.
    await expect(movedRow).toHaveAttribute("role", "link");
    await expect(movedRow).not.toHaveAttribute("role", "tab");

    // ── REAL ROUND-TRIP: clicking it CLOSES the modal and OPENS the drawer ──
    // MUTATION DISCIPLINE: this fails if the row reopened the dead Performance tab
    // (no `#settings-panel-performance` exists any more) instead of routing to the
    // drawer. We assert (a) the modal is gone, (b) the drawer is open with its new
    // title, and (c) the migrated perf panel is visible INSIDE the drawer.
    await movedRow.click();
    await expect(modal, "settings modal closes when the moved row is clicked").toHaveCount(0);
    const sidebar = perfDrawer(page);
    await expect(sidebar).toBeVisible({ timeout: 5_000 });
    await expect(
      sidebar.getByRole("heading", { name: "Performance & Diagnostics" }),
      "the moved row routes to the unified drawer, not the dead tab",
    ).toBeVisible();
    await expect(
      sidebar.locator('[data-testid="perf-simulcast-strip"]'),
      "the migrated perf panel is the destination of the moved row",
    ).toBeVisible();
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
    await openPerformanceDrawer(page);

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
    await openPerformanceDrawer(page);

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
    await openPerformanceDrawer(page);

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

    // 1-BASED DISPLAY: the BASE rung's data-testid stays 0-based
    // (`diag-simulcast-rung-0`, so selectors/protobuf don't churn) but its visible
    // id label is 1-based "L1" (matching the receive side). The ladder must NEVER
    // render a "L0" label.
    const baseRung = ladder.locator('[data-testid="diag-simulcast-rung-0"]');
    await expect(baseRung.locator(".simulcast-rung-id")).toHaveText("L1");
    await expect(ladder.locator(".simulcast-rung-id", { hasText: /^L0$/ })).toHaveCount(0);
  });
});
