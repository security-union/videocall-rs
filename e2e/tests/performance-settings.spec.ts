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
 *   - AUDIO publishes one layer (#2279). There is NO SEND "Fixed" badge for any
 *     kind now.
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
 *   mounted as the "Quality controls" group. After #1131 ITERATION 4 (#1222) the
 *   drawer group order is investigation-first: "Connection & system" (the
 *   incident-investigation anchor, always rendered) FIRST, then "Quality controls"
 *   (this panel) second, then "Live stream state" (the live/NetEq sections) LAST.
 *   Within "Connection & system" the four
 *   low-level pre-dumps (Reception / Sending / Encoder / Media Status) were merged
 *   into ONE collapsed `<details class="diag-disclosure">` "Raw stats" disclosure
 *   (`#diag-h-raw-stats`), and "Build info" was demoted to a second collapsed
 *   `<details>` at the very bottom — both CLOSED by default. Within "Live stream
 *   state" the NetEq sections are now per-PEER: with the "All Peers" aggregate
 *   selected (the ZERO-remote-peer solo-meeting default) a single placeholder
 *   section is shown
 *   ("Select a specific peer to view time-series charts and current status.")
 *   IN PLACE of the Current Status tiles + charts. Selecting a specific peer —
 *   or, with EXACTLY ONE remote peer, the #1222 1:1 AUTO-SELECT picking it
 *   automatically — renders the redesigned two-tier Current Status (`.neteq-status`
 *   > `.status-primary` / `.status-secondary` / `.status-reorder`) and the
 *   now-horizontally-scrollable NetEq
 *   charts (`.neteq-chart-scroll` overflow-x box + a fixed `.neteq-chart-y-axis`
 *   sibling, stacked 1-up via `.neteq-charts-stack`), each chart carrying its own
 *   per-chart `diag-chart-{buffer,decode,packets,reorder}-help` "?" icon (the old
 *   section-level `diag-charts-help` was removed).
 *   The drawer title became "Performance & Diagnostics". The Settings modal now
 *   has exactly FIVE tabs (Audio / Video / Network / Appearance / Preferences —
 *   Preferences was added later, in the Action Bar Customization work) and NO
 *   performance affordance at all: the transitional `settings-perf-moved` redirect
 *   row was REMOVED in the iteration (the drawer is the only home of the
 *   Performance controls; the "performance" deep link still routes to the drawer
 *   via attendants).
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
 *                   format (#1222 quality-letter, {Q}=L/M/H or "1" single-layer):
 *                           `{Q} · {i}/{N} · {w}x{h}` (video/screen),
 *                           `— kbps` (audio: one layer, so no chip and no
 *                           receiver-observable bitrate),
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
 * diagnostics-peer-transport.spec.ts). The perf panel renders as the "Quality
 * controls" group inside `#diagnostics-sidebar`; `openPerformanceDrawer` waits on
 * the migrated simulcast strip (`perf-simulcast-strip`) appearing INSIDE that
 * scope as readiness + relocation proof.
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
  // or null = Auto / full ladder. `#[serde(default)]` on the Rust side means these
  // keys may be absent in prefs persisted before the control existed, hence
  // optional here.
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

  /**
   * When true, seed the persisted camera preference OFF (`vc_prejoin_camera_on=
   * false`) so the camera is GENUINELY off in the meeting. This is required by
   * the camera-OFF caption tests: the default seed turns the pre-join camera ON,
   * and combined with the #1304 pre-join auto-getUserMedia, the Chromium
   * `--use-fake-ui-for-media-stream` auto-grant, and #959 (camera state carries
   * into the meeting), that leaves the in-meeting camera LIVE — which makes the
   * "Camera — off" / "Will send … when the camera is on" captions false. Seeding
   * OFF makes those OFF-state captions the truth. Mutually exclusive with
   * `ensureCameraOn`. Defaults to false.
   */
  cameraOff?: boolean;
}

async function joinMeeting(page: Page, testLabel: string, opts: JoinOpts = {}): Promise<void> {
  const meetingId = `e2e_perf_${testLabel}_${Date.now()}`;

  // Seed the persisted camera preference before the app boots. By default this
  // makes the pre-join camera toggle default ON; combined with `ensureCameraOn`
  // (which drives the UI so the device list is populated) it guarantees the SEND
  // encoder runs. When `cameraOff` is set the seed is FALSE so the camera is
  // genuinely off in the meeting (the OFF-state caption tests depend on this).
  // addInitScript runs on every navigation (incl. post-reload).
  const seedCameraOn = opts.cameraOff ? "false" : "true";
  await page.addInitScript((value) => {
    try {
      window.localStorage.setItem("vc_prejoin_camera_on", value);
    } catch {
      /* storage may be unavailable pre-navigation; the app origin sets it */
    }
  }, seedCameraOn);

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

    // Grant media if the permission prompt is still showing. NOTE (#1134): the
    // pre-join screen AUTO-requests media on mount, so permission usually
    // resolves on its own — the "Allow" button is only a manual fallback for the
    // brief in-flight window. Once permission resolves, `media_access_granted`
    // flips true and the whole prompt block (button included) is REPLACED by the
    // device UI, so the button detaches. We therefore (a) make the click
    // best-effort (it may race the auto-grant and detach mid-click) and (b) do
    // NOT depend on the click landing: we wait for the GRANTED state directly —
    // the prompt being hidden — which the auto-request reaches regardless.
    const allow = page.locator('[data-testid="prejoin-permission-allow"]');
    if (await allow.isVisible().catch(() => false)) {
      await allow.click({ timeout: 5_000 }).catch(() => {
        /* auto-grant already replaced the prompt with the device UI */
      });
    }
    // Either the click or the #1134 auto-request lands us in the granted state.
    await expect(page.locator('[data-testid="prejoin-permission-prompt"]')).toBeHidden({
      timeout: 15_000,
    });

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
  // The controls bar auto-hides after ~1s of mouse inactivity and slides
  // in/out on a transform transition, so a raw click can hit it mid-animation
  // ("element is not stable"). Wake it the same way openDrawer does: hover the
  // container, nudge the pointer to an interior viewport point, and let the
  // slide-in settle before clicking the gear.
  await page.locator(".video-controls-container").hover();
  const vp = page.viewportSize() ?? { width: 800, height: 600 };
  await page.mouse.move(Math.floor(vp.width / 2), Math.floor(vp.height / 2));
  await page.waitForTimeout(300);

  const gear = page.locator('[data-testid="open-settings"]');
  await expect(gear).toBeVisible({ timeout: 10_000 });
  // Force-click as a belt-and-suspenders guard: even after the settle the bar
  // can still report a sub-pixel transform drift on slower runners, which the
  // actionability "stable" check rejects. The gear's hit target is unaffected.
  await gear.click({ force: true });
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
}

/**
 * The Diagnostics drawer root. The Performance panel (#1131) is mounted as the
 * "Quality controls" group inside `.sidebar-content` here, so EVERY perf
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
  // drawer (the "Quality controls" group). This fails if the panel didn't move
  // into the drawer.
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
async function expectPinnedFloor(minInput: Locator, expectedValuetext?: string): Promise<void> {
  // NOT HTML-`disabled` — the WebKit fix. `aria-disabled` is expected and is
  // asserted below, so `toBeEnabled()` would be the wrong matcher here.
  await expect
    .poll(async () => minInput.evaluate((el) => (el as HTMLInputElement).hasAttribute("disabled")))
    .toBe(false);
  await expect(minInput).toHaveAttribute("tabindex", "-1");
  await expect(minInput).toHaveAttribute("aria-disabled", "true");
  await expect(minInput).toHaveValue("0");
  // WHAT THE FLOOR ANNOUNCES. The floor is pinned to the BASE rung, so its
  // `aria-valuetext` must name the base — but `position_label` applies the TIER-mode
  // inversion (position 0 = best = LAST label), so a layer-mode slider announced the
  // TOP of the ladder: "Base video layer — always sent (fixed), 720p" when the base
  // rung is 180p, and the same one-off on the other kinds. Pinning the attributes
  // alone left that invisible — the value is "0" either way, because only the SPOKEN
  // text was wrong.
  //
  // Only discriminating when the caller has forced a ladder whose base differs from
  // its top — at depth 1 the buggy and fixed lookups agree, which since #2279 is every
  // kind but VIDEO. Callers that pass this MUST therefore assert the ladder depth
  // (`max="2"`), as both video callers in this file do.
  //
  // Passing it from a caller that sits above a `test.skip` ladder-depth guard buys
  // nothing: on a 1-layer runner the assertion passes VACUOUSLY (depth 1 → buggy ==
  // fixed) and the test then reports "skipped", so it contributes no signal either
  // way. To be precise about the mechanism — MEASURED with a probe spec, because an
  // earlier revision of this comment asserted the opposite without checking: a
  // genuinely failing assertion above `test.skip` is reported as `1 failed`, with the
  // full diff. Playwright does NOT swallow it; the hard `expect` throws before the
  // skip is reached. The hazard is silent vacuity, not a hidden failure.
  if (expectedValuetext !== undefined) {
    await expect(
      minInput,
      `the pinned floor must ANNOUNCE the base rung, not the top of the ladder`,
    ).toHaveAttribute("aria-valuetext", expectedValuetext);
  }
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
    //    reset → the slot is empty). SEND controls use layer counts: the
    //    base-layer floor is PINNED (min
    //    thumb DISABLED at position 0 — the base is always published), and only the
    //    ceiling (max) thumb moves, defaulting to the top position = the full
    //    ladder.
    //
    // CAPABILITY note: VIDEO's effective depth is
    // `min(flag, capability_max_simulcast_layers())` and clamps to 1 on weak
    // (<6-core) runners. AUDIO and SCREEN are always 1 since issues #2279 and
    // #2343 — screen's arm of
    // `send_layer_labels_with_top` ignores `layer_max` entirely, so neither the
    // flag nor the core count moves it. We read each ceiling thumb's `max` attribute (== effective
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

  test("desktop layout: the Quality-controls perf cards stay contained in the drawer (#1208/#1213, adapted to the drawer)", async ({
    page,
  }) => {
    // The drawer (#1131) is a SINGLE SCROLLING SURFACE by design. After #1131
    // iteration 3 the group order is usage-frequency: "Quality controls" (the
    // perf cards — now the FIRST group) → "Live stream state" → "Connection &
    // system" LAST. The perf cards therefore sit at the TOP of the drawer, the
    // "Live stream state" + "Connection & system" groups sit below them, the
    // content is legitimately taller than the viewport, and the drawer scrolls
    // vertically. The #1208/#1213 lesson: do NOT assert vertical overflow in
    // either direction (a scrolling-by-design surface SHOULD overflow
    // vertically). The stable contract is (a) NO horizontal overflow, and (b)
    // the LAST perf card (.perf-kind-card) is reachable by scrolling the
    // drawer's own scroll container — the explicit scrollIntoView below brings
    // the card into view from the bottom-most scroll position, proving it is not
    // clipped on either fold of the over-tall surface.
    await page.setViewportSize({ width: 1280, height: 768 });
    await joinMeeting(page, "drawer_containment");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);
    await expect(panel).toBeVisible();
    // All three Quality-controls perf cards must be present (so we're measuring
    // the full content, not a partially-rendered panel).
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
      "drawer perf-controls content must not overflow horizontally",
    ).toBeLessThanOrEqual(1);

    // Scroll the drawer to the bottom, then explicitly bring the LAST perf card
    // into view. After the reorder the perf cards are at the TOP of the drawer
    // (above "Live stream state" + "Connection & system"), so scrolling all the
    // way down scrolls PAST them; the scrollIntoView is what proves the card is
    // reachable and not clipped on either fold of the over-tall surface.
    await scrollContainer.evaluate((el) => {
      el.scrollTop = el.scrollHeight;
    });
    await scrollContainer.evaluate((el) => {
      // The last perf card sits in the top "Quality controls" group; scroll it
      // back into view from the bottom-most scroll position.
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
    // range-value caption is SOURCE-AWARE: with the camera ON it reads the
    // capacity form "Up to {N} of {M} layers"; with the camera OFF (this test
    // doesn't drive the camera) it reads "Will send {N} layers when the camera
    // is on". Either
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
    // THREE rungs, deliberately. The pinned-floor `aria-valuetext` assertions below
    // are only discriminating when a ladder's base differs from its top. At the
    // committed `experimentalSimulcastMaxLayers: 1` every SEND ladder is
    // single-element, and `position_to_tier_index(0, 1) == 0`, so the buggy tier-mode
    // lookup and the fixed layer-mode lookup return the SAME string — the assertions
    // would pass identically on fixed and unfixed code. With 3 rungs VIDEO diverges
    // (base `180p` vs top `720p`), which is what makes it a guard rather than a
    // tautology. SCREEN no longer diverges at any flag value (issue #2343 collapsed it
    // to one rung) and neither does AUDIO (#2279) — see the note above
    // `expectedFloorValuetext`.
    //
    // `capabilityMaxLayersOverride: 3` is load-bearing for the CPU-clamped VIDEO kind:
    // video takes `min(flag, capability_max_simulcast_layers())`, so on a
    // low-core runner the sniffed ceiling clamps it back to 1 and its assertion
    // goes vacuous again.
    // MUST precede the first navigation: `enableSimulcastFlag` installs a
    // `**/config.js` route interception, so a `joinMeeting` above this line would
    // serve the un-patched config and silently drop the ladder back to 1 rung —
    // which makes the assertions below VACUOUS rather than red. The `max` assertion
    // after the drawer opens is what catches that if it ever regresses.
    await enableSimulcastFlag(page.context(), 3, { capabilityMaxLayersOverride: 3 });
    // `cameraOff` matches every other 3-layer caller in this file: the ladder depth
    // and rung labels come from `send_layer_max` (a `use_hook` over flag+capability)
    // and `send_layer_labels`, neither of which consults capture state — so the
    // assertions are identical with the camera off, and we don't ask a possibly
    // 1-core container to stand up live camera encoders it doesn't need.
    await joinMeeting(page, "no_fixed_badge", { cameraOff: true });
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);

    // LADDER-DEPTH PREMISE, asserted rather than assumed. Everything below is only
    // discriminating at ≥2 rungs; at depth 1 the buggy and fixed lookups agree and the
    // `aria-valuetext` assertions pass on BOTH. So if the config.js injection above
    // ever stops taking effect, this fails LOUDLY here instead of going green while
    // testing nothing. Same guard, same reason, as the RECEIVE rung-label describe's
    // `max` assertion in this file.
    await expect(
      panel.locator('[data-testid="perf-video-range-max"]'),
      "the 3-rung injection must have taken effect, or the valuetext assertions below are vacuous",
    ).toHaveAttribute("max", "2");

    // The "Fixed" badge was a TIER-slider concept (both thumbs pinned to one
    // tier). ALL THREE SEND controls (video, screen, AND audio) are now
    // layer-count sliders with a fixed base-layer floor and an adjustable ceiling
    // — there is no tier-pinning and therefore no SEND "Fixed" badge for any kind.
    // Guard that none renders (a regression that revived the tier slider would
    // bring the badge back). The floor for every kind is pinned (non-interactive
    // but NOT `disabled` — the WebKit fix) rather than a draggable tier min.
    //
    // CAVEAT on the `toHaveCount(0)` below, so nobody over-reads it: the SEND
    // `perf-{video,audio,screen}-fixed-badge` testid is emitted NOWHERE in production
    // (only the `perf-recv-*` variants exist, from the receive module's
    // `fixed_testid`), so that assertion cannot currently fail. It is a
    // never-come-back guard, not a live one. The load-bearing assertions in this test
    // are the `aria-valuetext` ones below — if a future cleanup deletes the dead-badge
    // check, KEEP those (or move them, with the ladder-depth guard above, to a
    // dedicated a11y test; there is no a11y-focused spec in e2e/tests/ yet to host
    // them). `SEND ceiling thumb is grabbable…` carries a capability-independent
    // duplicate of the audio floor assertion for exactly this reason.
    //
    // The expected `aria-valuetext` per kind is the BASE of that kind's lowest-first
    // ladder — video `180p` on its 3-rung ladder, screen `Native` and audio `Single`
    // on their single rungs. Unfixed, `position_label`'s tier inversion made video
    // announce the TOP (`720p`).
    //
    // MUTATION RUN: reverting the CALL SITE only (`min_valuetext =
    // position_label(sel.min_pos, &labels)`) while leaving the helper intact fails
    // here with `Expected: "180p" / Received: "720p"`, the resolved DOM node showing
    // `aria-valuetext="720p"` and `max="2"` (confirming 3 rungs took effect). This is
    // the mutation that MATTERS: the helper's own unit test stays GREEN under it, so
    // this assertion is the only guard on the call site. Video is the first iteration,
    // so it aborts the loop before audio/screen assert — the shared one-off across all
    // three kinds is pinned by the helper's unit test instead.
    //
    // The 3-rung injection is load-bearing, not belt-and-braces — and the
    // ladder-depth guard above is what enforces that. Removing `enableSimulcastFlag`
    // leaves the committed `config.js` pin of `experimentalSimulcastMaxLayers: 1`, so
    // video renders `max="0"` and the guard FAILS before these assertions run.
    //
    // (Historical note, since the reasoning matters more than the artifact: before
    // that guard existed, removing the flag with a call-site mutation in place made
    // this test pass silently — measured at the time. The guard converts that silent
    // vacuity into a loud failure, which is why it was added.)
    //
    //
    // SCREEN is `Native` (issue #2343). Screen no longer simulcasts: the Screen arm
    // of `send_layer_labels_with_top` returns a single `screen_display_label("native")`
    // and ignores `layer_max`, so its ladder is `["Native"]` whatever the flag says.
    //
    // BE HONEST ABOUT WHAT THE SCREEN AND AUDIO ITERATIONS PROVE NOW. The mutation
    // this test was built for — `position_label`'s tier inversion making the floor
    // announce the TOP rung — is only detectable when base != top, which now holds for
    // VIDEO alone (`180p` vs `720p`). That is where this test's discriminating power
    // lives ENTIRELY. Screen and audio are single-rung, so the inverted and correct
    // lookups return the same string; both are retained as LABEL guards. Screen still
    // fails if its ladder regains rungs (the loop would read `720p` at the floor) or if
    // `screen_display_label`'s `native` arm is dropped (which returns `"?"`); audio
    // still fails if the floor regains a ladder BITRATE — `12k` against a 48 kbps
    // publisher is the wrong number #2279 removed. Do not cite either as inversion
    // coverage.
    const expectedFloorValuetext = { video: "180p", audio: "Single", screen: "Native" } as const;
    for (const stream of ["video", "audio", "screen"] as const) {
      await expect(panel.locator(`[data-testid="perf-${stream}-fixed-badge"]`)).toHaveCount(0);
      await expectPinnedFloor(
        panel.locator(`[data-testid="perf-${stream}-range-min"]`),
        expectedFloorValuetext[stream],
      );
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
    // NOT set `disabled`. VIDEO is the drag target: since #2279 audio publishes one
    // layer, so its ceiling has no position to drag into and cannot exercise the bug.
    // `capabilityMaxLayersOverride` defeats the cores-based clamp that would otherwise
    // leave a low-core runner at one video layer too; the `max` assertion below fails
    // LOUDLY if the override does not land, rather than going vacuous.
    await enableSimulcastFlag(page.context(), 3, { capabilityMaxLayersOverride: 3 });
    await joinMeeting(page, "send_ceiling_grabbable");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);
    const minInput = panel.locator('[data-testid="perf-video-range-min"]');
    const maxInput = panel.locator('[data-testid="perf-video-range-max"]');

    // LADDER-DEPTH PREMISE: at depth 1 there is no ceiling position to drag into and
    // the floor's buggy and fixed lookups agree, so everything below goes vacuous.
    await expect(
      maxInput,
      "the 3-rung injection must have taken effect, or the drag has no position to move into",
    ).toHaveAttribute("max", "2");

    // (a) DOM contract: the max is interactive/enabled and the floor is pinned
    // WITHOUT HTML-`disabled` (the WebKit fix).
    await expect(maxInput).toBeEnabled();
    // The pinned floor must ANNOUNCE the base rung ("180p"), not the top of the
    // ladder — at depth 3 the buggy tier-mode lookup returns `720p`.
    await expectPinnedFloor(minInput, "180p");
    const topPos = Number(await maxInput.getAttribute("max"));
    expect(topPos, "video ceiling must have ≥1 position to drag").toBeGreaterThanOrEqual(1);
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
    // max input updates the persisted video layer count + the caption, proving the
    // control is live-wired regardless of the drag path.
    await setRangeValue(page, "perf-video-range-max", 0);
    await expect
      .poll(async () => (await readPerfPref(page))?.video_layers, { timeout: 10_000 })
      .toBe(1); // position 0 → 1 layer (base only)
    // The caption reflects the new count of 1. Camera-on names the ladder max
    // ("1 of 3 layers"), camera-off does not ("1 layer"), so match either.
    await expect(panel.locator('[data-testid="perf-video-range-value"]')).toContainText(
      /\b1 (of \d+ layers|layer\b)/,
    );
  });

  test("send rung strip + directional arrows render (#1131)", async ({ page }) => {
    // §2: AUDIO now renders a SEND rung strip with one pip (#2279). The strip is
    // present even single-page with no peers, so it is the deterministic anchor
    // for the strip markup. §1: each side title is prefixed with an
    // aria-hidden directional arrow (`.perf-dir-arrow`).
    await joinMeeting(page, "send_rungs");
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);

    // The audio send rung strip is a role=img container with at least one pip.
    const audioStrip = panel.locator('[data-testid="perf-audio-send-rungs"]');
    await expect(audioStrip).toBeVisible({ timeout: 5_000 });
    await expect(audioStrip).toHaveAttribute("role", "img");
    // EXACTLY one pip: the publisher emits one audio layer (#2279), so a second pip
    // would mean the strip and the publisher disagree. `>= 1` passed on both the
    // 3-layer and the 1-layer build, which is why it is pinned to the count here.
    const audioPips = panel.locator('[data-testid^="perf-audio-send-rung-"]');
    await expect(audioPips).toHaveCount(1);
    await expect(panel.locator('[data-testid="perf-audio-send-rung-0"]')).toBeVisible();
    await expect(panel.locator('[data-testid="perf-audio-send-rung-1"]')).toHaveCount(0);

    // §1 directional arrows: present on BOTH a Sending and a Receiving title, and
    // aria-hidden so they are decorative (the title text is the a11y label).
    const arrows = panel.locator(".perf-dir-arrow");
    expect(await arrows.count()).toBeGreaterThanOrEqual(2);
    await expect(arrows.first()).toHaveAttribute("aria-hidden", "true");
  });

  test("SEND caption is source-aware: OFF sources read 'Will send …' (not 'Sending')", async ({
    page,
  }) => {
    // This is a single-page meeting with no media driven (camera seeded OFF via
    // `cameraOff`, mic off), so all three SEND sources are OFF. Without
    // `cameraOff` the default seed (`vc_prejoin_camera_on=true`) + the #1304
    // pre-join auto-getUserMedia + the Chromium fake-UI auto-grant + #959
    // (camera carries into the meeting) would leave the in-meeting camera LIVE,
    // making the VIDEO caption read "Currently sending N of M layers" and
    // breaking the OFF-state premise. The caption must NOT falsely claim to be
    // "Sending" — it reads the future "Will send {N} … when {…}" form using the
    // configured count, and names each kind's trigger.
    await enableSimulcastFlag(page.context(), 3);
    await joinMeeting(page, "caption_source_aware", { cameraOff: true });
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

  // Issue #2343: the Screen arm of `send_layer_labels_with_top` returns a single
  // `screen_display_label("native")` == "Native" and IGNORES `layer_max`, so the
  // strip is one pip whatever the flag says. `enableSimulcastFlag(ctx, 3)` is kept
  // to prove that flag-independence — and it is what makes this fail on the old
  // code two ways: the old arm built the strip from `simulcast_screen_layers` so a
  // flag of 3 rendered THREE pips reading `720p`/`1080p`/`1440p`.
  //
  // ANCHOR: the label is the pip's `.perf-rung__label` child span, which renders
  // `rung.res_label` bare; the pip's `title` is a composed sentence and is the
  // wrong surface to assert on.
  test("screen SEND rung strip is a single Native pip, even with the simulcast flag at 3", async ({
    page,
  }) => {
    await enableSimulcastFlag(page.context(), 3);
    await joinMeeting(page, "screen_single_rung", { cameraOff: true });
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);

    const strip = panel.locator('[data-testid="perf-screen-send-rungs"]');
    await expect(strip).toBeVisible({ timeout: 5_000 });

    // Exactly one rung — the whole point of #2343. `toHaveCount` retries, so this
    // does not race the strip's first paint.
    const pips = panel.locator('[data-testid^="perf-screen-send-rung-"]');
    await expect(pips, "screen must publish exactly ONE simulcast rung").toHaveCount(1);

    // ...and it is layer_id 0, named for the capture itself rather than a tier box.
    await expect(panel.locator('[data-testid="perf-screen-send-rung-0"]')).toBeVisible();
    await expect(
      pips.first().locator(".perf-rung__label"),
      "the single screen rung must read Native, not a tier resolution",
    ).toHaveText("Native");
  });

  // The idle promise deliberately names NO resolution: the reachable top rung
  // depends on the sender's core count and stream count, so any fixed number
  // (the old "up to 1080p") goes stale. `CONTENT_SEND_IDLE_SUMMARY`,
  // performance_settings.rs.
  test("screen SEND idle promise names the source's own resolution, not a fixed number", async ({
    page,
  }) => {
    await joinMeeting(page, "screen_idle_promise", { cameraOff: true });
    await openPerformanceDrawer(page);
    await selectSendDirection(page);

    const panel = perfDrawer(page);

    await expect(
      panel.getByText("Will send up to your screen's own resolution when you share"),
    ).toBeVisible({ timeout: 10_000 });
    // The stale-number form must be gone.
    await expect(panel).not.toContainText("Will send up to 1080p when you share");
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

  // REMOVED (issue #2343): "pre-#2179 receive screen bounds migrate …". The screen
  // receive ladder is one rung, so `top_index(Screen)` == 0 and the slider has a
  // single reachable state: migrated (0..=2 → 0) and un-migrated (`sanitized()`
  // drops the out-of-range index) both render `max="0"` with both thumbs at 0, and
  // `is_fixed` is true either way. No DOM assertion can separate them. The migration
  // is guarded instead by `v0_screen_indices_are_repointed_onto_the_new_ladder`
  // (performance_settings.rs), which runs in per-PR CI via `cargo test -p videocall-ui --lib`.

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
    // the `{Q} · {i}/{N} · {w}x{h}` shape (#1222 quality-letter format: {Q} is a
    // quality letter L/M/H, or "1" single-layer); assert the union so the test is
    // correct in both states (mirrors how the send-side test asserts its needle).
    await expect(panel.locator("#perf-vu-recv-video-readout")).toBeVisible();
    await expect
      .poll(
        async () => (await panel.locator("#perf-vu-recv-video-readout").textContent())?.trim(),
        {
          timeout: 15_000,
        },
      )
      .toMatch(/^(\S+ · \d+\/\d+ · \d+x\d+|Not receiving)$/);

    // This solo-page test has no remote publisher, so the receive readout is the
    // placeholder.
    await expect
      .poll(
        async () => (await panel.locator("#perf-vu-recv-audio-readout").textContent())?.trim(),
        {
          timeout: 15_000,
        },
      )
      .toMatch(/^Not receiving$/);
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
// section (#1095, in the "Live stream state" group of the same drawer).
//
// #1131 collapsed the two surfaces into ONE: the Performance panel moved INTO
// the Diagnostics drawer, so the former Perf↔Diag cross-nav buttons
// (`perf-open-diagnostics` / `diag-open-performance`) are GONE — there is no
// second surface to navigate to. The old round-trip cross-nav test is replaced
// below by (a) a single-surface assertion that the perf panel and the
// "Simulcast layers" section coexist in ONE open drawer, and (b) a Settings-modal
// assertion that the modal has exactly FIVE tabs and NO performance affordance.
//
// #1131 ITERATION: the transitional `settings-perf-moved` redirect row was
// REMOVED (the product decision reversed the one-release link). The contract is
// now: the Settings modal has exactly five tabs and offers NO path to Performance
// at all — the drawer is the only home of the Performance controls, opened via the
// toolbar "Open Diagnostics" button (and the "performance" deep link, which still
// routes to the drawer via attendants). The old "moved row opens the drawer"
// round-trip test is therefore deleted and replaced by a "no performance
// affordance" assertion below.
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

  test("single surface: the Quality-controls perf panel and Simulcast layers coexist in ONE open drawer", async ({
    page,
  }) => {
    await joinMeeting(page, "single_surface");

    // ONE open action puts BOTH the perf controls and the live diagnostics on
    // screen at once — the whole point of #1131. `openPerformanceDrawer` already
    // asserts the title + the migrated simulcast strip inside the drawer.
    await openPerformanceDrawer(page);
    const sidebar = perfDrawer(page);

    // QUALITY CONTROLS — the migrated Performance panel (the FIRST group after
    // the #1131 iteration-3 reorder). Assert a perf control is visible INSIDE the
    // drawer (relocation proof, not anywhere on the page).
    await expect(
      sidebar.locator('[data-testid="perf-simulcast-strip"]'),
      "perf strip inside the drawer",
    ).toBeVisible();
    await expect(
      sidebar.locator('[data-testid="perf-vu-video"]'),
      "send video meter inside the drawer",
    ).toBeVisible();

    // LIVE STREAM STATE — the "Simulcast layers" section coexists in the SAME
    // drawer (#1095 §6 MOVE). Assert the heading AND the moved sub-structure so this
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

  // TAGGED @bvt1 (issue 2158): this is the ONE test in this file that runs in
  // per-PR CI. It is here because the tablist assertion below drifted for ~6
  // weeks — the spec is otherwise untagged, so `--project=bvt1` never ran it and
  // nothing failed when a 5th tab landed. ~22s against a smoke job that already
  // spends minutes on npm ci + browser install + docker build, in exchange for
  // closing the root cause. Do NOT tag the whole file: it is 5.3 min.
  test("@bvt1 Settings has exactly FIVE tabs and offers NO performance affordance (#1131 iteration removed the moved row)", async ({
    page,
  }) => {
    await joinMeeting(page, "settings_no_perf");

    // ── The Settings modal has exactly FIVE tabs (Performance tab removed) ──
    // MIRRORS RUST: this list is the DOM mirror of `SettingsSection::all()` in
    // `dioxus-ui/src/components/device_settings_modal.rs`, which is itself pinned
    // by the unit test `tablist_has_five_sections_without_performance` (same
    // count, same titles, same "no Performance" clause).
    //
    // Do NOT read that pairing as drift-proof: the two halves are NOT gated
    // equally per-PR. The Rust test runs on every PR (`cargo test -p videocall-ui
    // --lib`); this spec is tagged `@bvt1` (below) specifically so it does too.
    // Before that tag, only the Rust half was gated — which is exactly how #2158
    // happened: the Preferences tab landed WITH its Rust test correctly updated
    // to 5, while this assertion sat at 4 and failed unnoticed for ~6 weeks
    // because per-PR CI (`--project=bvt1`) never ran the untagged spec.
    await openSettingsModal(page);
    const modal = page.locator(".device-settings-modal");
    const tabs = modal.getByRole("tab");
    await expect(tabs).toHaveCount(5);
    // The five surviving tabs, by accessible name. The nav button renders an
    // `aria-hidden="true"` icon plus a `.settings-nav-label` span, so the
    // accessible name is `SettingsSection::title()`.
    //
    // MATCHER ASYMMETRY — deliberate, and verified empirically against the
    // installed Playwright (a bare `name` string defaults to `exact: false`,
    // i.e. case-insensitive SUBSTRING). The two directions need OPPOSITE modes:
    //   * POSITIVE (this loop) uses `exact: true` so the DOM label must equal
    //     `title()`. Bare would accept a drifted "Preferences (Beta)" and let the
    //     titles silently diverge from the Rust list this block claims to mirror.
    //   * NEGATIVE (`name: "Performance"` below) stays BARE on purpose, because
    //     substring is what catches a Performance tab smuggled in under a renamed
    //     label ("Video Performance"). `exact: true` there would MISS it and
    //     quietly gut the #1131 contract this test exists to enforce.
    // Verified: bare finds "Video Performance" (count 1) where exact does not
    // (count 0); exact rejects "Preferences (Beta)" where bare accepts it.
    for (const name of ["Audio", "Video", "Network", "Appearance", "Preferences"] as const) {
      await expect(
        modal.getByRole("tab", { name, exact: true }),
        `${name} tab present`,
      ).toBeVisible();
    }
    // The Performance tab is GONE — it is NOT one of the tabs. Bare (substring)
    // matcher on purpose — see the MATCHER ASYMMETRY note above.
    await expect(modal.getByRole("tab", { name: "Performance" })).toHaveCount(0);
    await expect(modal.locator('[data-testid="settings-nav-performance"]')).toHaveCount(0);

    // ── NEW CONTRACT (#1131 iteration): the transitional `settings-perf-moved`
    //    redirect row was REMOVED — the modal offers NO path to Performance at
    //    all. The Performance controls live ONLY in the Diagnostics drawer.
    // MUTATION DISCIPLINE: if a regression re-added the moved row (or any
    // performance redirect affordance) in the modal, the count below flips to ≥1
    // and this fails. The previous iteration asserted the row was PRESENT and
    // round-tripped to the drawer; this asserts its ABSENCE — break the source by
    // re-adding the `settings-perf-moved` button and this test goes red.
    await expect(
      modal.locator('[data-testid="settings-perf-moved"]'),
      "the transitional 'Performance moved to Diagnostics' row is gone",
    ).toHaveCount(0);
    // No performance affordance survives under any of the usual ids either.
    await expect(modal.locator('[data-testid="perf-open-diagnostics"]')).toHaveCount(0);
    await expect(modal.getByRole("link", { name: /performance/i })).toHaveCount(0);

    // The modal still works as a settings modal: it does NOT auto-route to the
    // drawer, and the drawer is NOT open just because Settings is.
    await expect(perfDrawer(page)).toHaveCount(0);

    // ── The drawer remains reachable via the ONLY remaining opener (Open
    //    Diagnostics). Close Settings first (the modal is open from
    //    openSettingsModal above), then open the drawer the canonical way and
    //    confirm the migrated perf panel is its destination. ──
    // Close via the modal's dedicated close button, NOT by re-clicking the gear:
    // while the modal is open its `.device-settings-modal-overlay` covers the
    // toolbar and intercepts pointer events to the gear, so a gear click is
    // swallowed by the overlay and the modal never closes. (Escape is unreliable
    // here too — its handler lives on the dialog div, which is not guaranteed to
    // hold focus after the getByRole tab interactions above.) The close button's
    // onclick calls `on_close` directly. See device_settings_modal.rs.
    await modal.locator(".settings-modal-close").click();
    await expect(modal).toHaveCount(0);

    await openPerformanceDrawer(page);
    const sidebar = perfDrawer(page);
    await expect(sidebar).toBeVisible({ timeout: 5_000 });
    await expect(
      sidebar.getByRole("heading", { name: "Performance & Diagnostics" }),
      "the drawer is the only home of the Performance controls",
    ).toBeVisible();
    await expect(
      sidebar.locator('[data-testid="perf-simulcast-strip"]'),
      "the migrated perf panel lives in the drawer",
    ).toBeVisible();
  });

  // Camera-off regression (the #1101 fix), now in its new home: the relocated
  // "Video (sending)" line must read the static "Camera — off" with NO layer
  // count when the camera is off. Capability-INDEPENDENT (the snapshot is gated
  // to None while the camera is disabled).
  test("Simulcast layers: Video (sending) reads 'Camera — off' when the camera is off", async ({
    page,
  }) => {
    // Join with the camera GENUINELY off (`cameraOff` seeds
    // `vc_prejoin_camera_on=false`), so send_video is gated to None. The default
    // seed would instead leave the camera LIVE (#1304 auto-getUserMedia + fake-UI
    // auto-grant + #959 carry-into-meeting), making this line read "Currently N
    // of M layers active" and defeating the off-state regression guard.
    await joinMeeting(page, "diag_cam_off", { cameraOff: true });
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

    // QUALITY-LETTER DISPLAY (#1222 Directive 4): the BASE rung's data-testid
    // stays 0-based (`diag-simulcast-rung-0`, so selectors/protobuf don't churn)
    // but its visible id chip is now the quality LETTER — the base (index 0) is
    // "L" (Low) in any multi-layer ladder, "1" only in the degenerate 1-layer
    // case (which this active-ladder arm has already skipped past). The old
    // 1-based "L1"/"L0" numeric labels are GONE: assert the base reads exactly
    // "L" and that NO rung chip carries the retired "L1"/"L0" numeric literals.
    const baseRung = ladder.locator('[data-testid="diag-simulcast-rung-0"]');
    await expect(baseRung.locator(".simulcast-rung-id")).toHaveText("L");
    await expect(ladder.locator(".simulcast-rung-id", { hasText: /^L0$/ })).toHaveCount(0);
    await expect(ladder.locator(".simulcast-rung-id", { hasText: /^L1$/ })).toHaveCount(0);

    // SEND LED on/off (issue #1607): every rung carries exactly one LED dot
    // (`diag-simulcast-led-video-{layer_id}`), and the LEDs are NOT all-on — the
    // count of LIT (`is-on`) LEDs equals the number of layers actually being sent
    // (the active count parsed from the "<a> of <b> layers active" header), which
    // is < the rung count whenever the AQ has shed the top layer(s). The base
    // rung's LED is ALWAYS lit (the base layer is always published). This is the
    // core fix: shed layers read as OFF (hollow), not lit.
    const rungCount = await ladder.locator('[data-testid^="diag-simulcast-rung-"]').count();
    const ledCount = await ladder.locator('[data-testid^="diag-simulcast-led-video-"]').count();
    expect(ledCount, "one LED dot per rung").toBe(rungCount);
    // Base layer LED is on.
    await expect(baseRung.locator('[data-testid="diag-simulcast-led-video-0"]')).toHaveClass(
      /is-on/,
    );
    // The number of lit LEDs equals the active count from the header (never all-on
    // unless the encoder genuinely has every rung active).
    const sendText = await simulcastSendText(page, "Video (sending)");
    const activeMatch = sendText.match(/(\d+) of (\d+) layers active/);
    expect(activeMatch, `header carries the active count: ${sendText}`).not.toBeNull();
    const activeCount = Number(activeMatch![1]);
    await expect
      .poll(async () => ladder.locator(".simulcast-led.is-on").count(), { timeout: 10_000 })
      .toBe(activeCount);
    // The shed rungs (rungCount - activeCount of them) render an OFF LED.
    await expect(ladder.locator(".simulcast-led.is-off")).toHaveCount(rungCount - activeCount);
  });

  // ── Drawer help popovers + position:fixed viewport-clip regression (#1131) ──
  //
  // CHANGE #4: the drawer's HelpPopover info ("?") icons share ONE single-open
  // signal per cluster (opening one closes its sibling), use role="dialog", and
  // wire aria-haspopup/aria-expanded/aria-controls. The "Live stream state"
  // group's NetEq sections carry help popovers — `diag-status-help` on "Current
  // Status" and, after #1222 iteration 4, a PER-CHART "?" on each of the four
  // NetEq charts (`diag-chart-buffer-help`, `diag-chart-decode-help`,
  // `diag-chart-packets-help`, `diag-chart-reorder-help`; popover ids
  // `*-popover`). The old section-level `diag-charts-help` on the "NetEQ charts"
  // heading was REMOVED in favor of the per-chart icons. Identical mechanism
  // (component, styles, clamp/flip) to the Quality-controls panel's per-kind help
  // (`perf-{video,audio,screen}-help`).
  //
  // CHANGE #5 (the user-reported clipping bug): every `.perf-help-popover` is now
  // `position: fixed` with a JS clamp/flip (`use_help_popover_anchor` /
  // `compute_help_popover_position`). They previously clipped at the drawer's
  // right border and bottom fold because an `absolute` popover anchored INSIDE
  // `#diagnostics-sidebar { overflow-y: auto }` is clipped on BOTH axes.
  //
  // ITERATION 3/4 reality (why this test no longer hard-targets the NetEq "?"):
  // the NetEq Current-Status + charts sections (and so their "?" buttons) now
  // render ONLY for a SINGLE selected peer. With the "All Peers" aggregate
  // selected — the ZERO-remote-peer solo-meeting default this spec produces —
  // those sections are replaced by a placeholder, so `diag-status-help` and the
  // per-chart `diag-chart-*-help` buttons are ABSENT. (#1222 ITERATION 4 adds a
  // 1:1 auto-select: with EXACTLY ONE remote peer the drawer auto-picks it and
  // those sections DO render by default — but that path needs a real second peer
  // and cannot be exercised on this solo runner; see the placeholder test below.)
  // The viewport-clip regression mechanism is shared by ALL drawer
  // HelpPopovers, so the load-bearing regression guard below opens a
  // Quality-controls help popover (`perf-screen-help`, low in the FIRST group
  // after a full scroll) — always reachable in a solo meeting — and asserts its
  // box stays inside the viewport. The NetEq-specific aria + single-open contract
  // is then asserted CONDITIONALLY, only when a real peer made the NetEq "?"
  // buttons render (a multi-peer runner). Both paths use the same fixed+clamp+flip
  // code, so reverting `position: fixed` (or the clamp/flip in
  // `compute_help_popover_position`) turns the guard red.
  //
  // MUTATION REASONING: we scroll the drawer to the bottom, open the target "?"
  // near the bottom fold, then assert its boundingBox is fully inside the viewport
  // (x ≥ 0, y ≥ 0, right ≤ innerWidth, bottom ≤ innerHeight). Under the OLD
  // `position:absolute` (top: calc(100% + 8px); anchored inside the scroll box) a
  // popover opened that low extended past the bottom fold AND past the right
  // border — box.bottom/right would exceed the viewport and the check would FAIL.
  test("Drawer help popovers (position:fixed clamp/flip): a bottom-of-drawer popover stays within the viewport (#1131)", async ({
    page,
  }) => {
    // Use a SHORT viewport so the bottom-of-drawer popover would clearly overflow
    // the bottom fold under the old absolute positioning — making the regression
    // unambiguous on any runner.
    await page.setViewportSize({ width: 1280, height: 720 });
    await joinMeeting(page, "neteq_help_viewport");
    await openPerformanceDrawer(page);

    const sidebar = perfDrawer(page);
    const scrollContainer = sidebar.locator(".sidebar-content");
    await expect(scrollContainer).toBeVisible();

    // ── Load-bearing regression guard: a Quality-controls help popover.
    // `perf-screen-help` is the Content/screen card's "?", reliably present in a
    // solo meeting (the Quality-controls panel renders once Host publishes the
    // controls — `openPerformanceDrawer` already waited on its simulcast strip).
    const guardBtn = sidebar.locator('[data-testid="perf-screen-help"]');
    await expect(guardBtn).toBeVisible({ timeout: 15_000 });
    const guardPopoverId = await guardBtn.getAttribute("aria-controls");
    expect(guardPopoverId, "help button wires aria-controls to its popover id").not.toBeNull();
    const guardPopover = page.locator(`#${guardPopoverId}`);

    // Scroll the drawer all the way down so the help button sits near the bottom
    // fold — the worst case for the reported clip — then open it.
    await scrollContainer.evaluate((el) => {
      el.scrollTop = el.scrollHeight;
    });
    await guardBtn.scrollIntoViewIfNeeded();
    await expect(guardBtn).toHaveAttribute("aria-haspopup", "dialog");
    await expect(guardBtn).toHaveAttribute("aria-expanded", "false");
    await guardBtn.click();
    await expect(guardBtn).toHaveAttribute("aria-expanded", "true");
    await expect(guardPopover).toBeVisible();
    await expect(guardPopover).toHaveAttribute("role", "dialog");

    // ── VIEWPORT-CLIP REGRESSION (CHANGE #5) ──
    // The popover's layout box must be fully inside the viewport. position:fixed +
    // clamp/flip guarantees this; the old absolute-in-scroll-box clipped past the
    // bottom fold and the right border, so box.bottom > innerHeight (or box.right >
    // innerWidth) and this assertion would fail.
    const viewport = page.viewportSize();
    expect(viewport, "viewport size is known").not.toBeNull();
    const vp = viewport as { width: number; height: number };
    const box = await guardPopover.boundingBox();
    expect(box, "the open popover has a layout box").not.toBeNull();
    const b = box as { x: number; y: number; width: number; height: number };
    // Left/top edges on-screen…
    expect(b.x, "popover left edge ≥ 0 (not clipped off the left)").toBeGreaterThanOrEqual(0);
    expect(b.y, "popover top edge ≥ 0 (not clipped off the top)").toBeGreaterThanOrEqual(0);
    // …right edge inside the viewport width (the right-border clip half of the bug)…
    expect(
      b.x + b.width,
      "popover right edge ≤ viewport width (not clipped at the drawer's right border)",
    ).toBeLessThanOrEqual(vp.width + 1);
    // …bottom edge inside the viewport height (the bottom-fold clip half of the bug).
    expect(
      b.y + b.height,
      "popover bottom edge ≤ viewport height (not clipped at the drawer's bottom fold)",
    ).toBeLessThanOrEqual(vp.height + 1);
    // Close the guard popover so its open state doesn't bleed into the NetEq path.
    await page.keyboard.press("Escape");
    await expect(guardBtn).toHaveAttribute("aria-expanded", "false");

    // ── NetEq-specific aria + single-open contract (CHANGE #4) ──
    // Only assertable when a real peer made the per-peer NetEq sections render
    // (the "?" buttons are ABSENT under the solo "All Peers" placeholder). When
    // present, exercise the same fixed+clamp+flip guarantee on the NetEq popover
    // and the shared single-open behaviour.
    // #1222 ITERATION 4: the section-level `diag-charts-help` was removed; each
    // chart now carries its OWN "?" (`diag-chart-{buffer,decode,packets,reorder}-help`).
    // We use the per-chart Buffer help as the second NetEq popover here. It shares
    // the SAME single-open `open_help` signal as `diag-status-help`, and renders
    // under the same gate the old charts help did (single peer selected AND that
    // peer has NetEq history), so the status-present / charts-maybe-present
    // conditional structure below is preserved exactly.
    const statusBtn = sidebar.locator('[data-testid="diag-status-help"]');
    const chartsBtn = sidebar.locator('[data-testid="diag-chart-buffer-help"]');
    const statusPresent = (await statusBtn.count()) > 0;
    if (!statusPresent) {
      // Solo / All-Peers runner: the NetEq help buttons legitimately do not
      // render (placeholder shown). The regression guard above already covered
      // the fixed-popover contract. Nothing more to assert here.
      return;
    }

    const chartsPresent = (await chartsBtn.count()) > 0;
    const neteqBtn = chartsPresent ? chartsBtn : statusBtn;
    const neteqTestid = chartsPresent ? "diag-chart-buffer-help" : "diag-status-help";
    const neteqPopoverId = `${neteqTestid}-popover`;
    const neteqPopover = page.locator(`#${neteqPopoverId}`);

    await neteqBtn.scrollIntoViewIfNeeded();
    await expect(neteqBtn).toHaveAttribute("aria-haspopup", "dialog");
    await expect(neteqBtn).toHaveAttribute("aria-expanded", "false");
    await expect(neteqBtn).toHaveAttribute("aria-controls", neteqPopoverId);
    await expect(neteqPopover).toHaveCount(0);

    await neteqBtn.click();
    await expect(neteqBtn).toHaveAttribute("aria-expanded", "true");
    await expect(neteqPopover).toBeVisible();
    await expect(neteqPopover).toHaveAttribute("role", "dialog");

    // Same viewport-clip guarantee on the NetEq popover (it sits in the MIDDLE
    // "Live stream state" group, still well down the drawer).
    const neteqBox = await neteqPopover.boundingBox();
    expect(neteqBox, "the open NetEq popover has a layout box").not.toBeNull();
    const nb = neteqBox as { x: number; y: number; width: number; height: number };
    expect(nb.x, "NetEq popover left edge ≥ 0").toBeGreaterThanOrEqual(0);
    expect(nb.y, "NetEq popover top edge ≥ 0").toBeGreaterThanOrEqual(0);
    expect(nb.x + nb.width, "NetEq popover right edge ≤ viewport width").toBeLessThanOrEqual(
      vp.width + 1,
    );
    expect(nb.y + nb.height, "NetEq popover bottom edge ≤ viewport height").toBeLessThanOrEqual(
      vp.height + 1,
    );

    // Shared single-open behaviour: assertable only when BOTH NetEq popovers
    // render (history present). Opening the other "?" closes the first.
    if (chartsPresent) {
      const statusPopover = page.locator("#diag-status-help-popover");
      // The per-chart Buffer popover is currently open; opening the status "?"
      // must close it (single-open contract across the shared open_help signal).
      await statusBtn.scrollIntoViewIfNeeded();
      await statusBtn.click();
      await expect(statusBtn).toHaveAttribute("aria-expanded", "true");
      await expect(statusPopover).toBeVisible();
      // The previously-open per-chart Buffer popover is now closed (single-open).
      await expect(chartsBtn).toHaveAttribute("aria-expanded", "false");
      await expect(page.locator("#diag-chart-buffer-help-popover")).toHaveCount(0);
      // Escape closes the open status popover and returns aria-expanded to false.
      await page.keyboard.press("Escape");
      await expect(statusBtn).toHaveAttribute("aria-expanded", "false");
      await expect(statusPopover).toHaveCount(0);
    } else {
      // Single-popover path: Escape still closes it (keyboard-operable).
      await page.keyboard.press("Escape");
      await expect(neteqBtn).toHaveAttribute("aria-expanded", "false");
      await expect(neteqPopover).toHaveCount(0);
    }
  });

  // ── ITERATION 4 — group reorder (#1131 / #1222) ──────────────────────────
  //
  // The drawer body's three top-level groups are `div.diag-group-label`
  // elements. After iteration 4 their DOM order is investigation-first:
  //   "Connection & system" → "Quality controls" → "Live stream state".
  // Connection & system is the always-rendered incident-investigation anchor
  // (no gate, so it leads and unconditionally owns the `--first` modifier);
  // Quality controls (the editable sliders/Auto) is second; Live stream state
  // (passive read-only telemetry) is last. (Quality controls renders once Host
  // publishes the perf controls, which `openPerformanceDrawer` already awaits
  // via the simulcast strip.)
  //
  // OLD-STRUCTURE FAILURE: in iteration 3 the order was
  //   ["Quality controls", "Live stream state", "Connection & system"].
  // Asserting the exact ordered text array fails against that order (and against
  // any future reshuffle), so this is a real sync guard, not a tautology.
  test("group order: the three drawer groups render Connection & system → Quality controls → Live stream state (#1131 iter 4)", async ({
    page,
  }) => {
    await joinMeeting(page, "group_order");
    await openPerformanceDrawer(page);

    const sidebar = perfDrawer(page);
    const groupLabels = sidebar.locator(".diag-group-label");
    // All three groups present (Quality controls is guaranteed by
    // openPerformanceDrawer's simulcast-strip wait).
    await expect(groupLabels).toHaveCount(3, { timeout: 15_000 });
    await expect(groupLabels).toHaveText([
      "Connection & system",
      "Quality controls",
      "Live stream state",
    ]);
    // The first (Connection & system) group owns the `--first` modifier — it is
    // the always-rendered anchor, so `--first` can never be orphaned (#1222).
    await expect(groupLabels.first()).toHaveClass(/diag-group-label--first/);
    await expect(groupLabels.first()).toHaveText("Connection & system");
  });

  // ── ITERATION 3 — Raw stats + Build info collapsed disclosures (#1131) ────
  //
  // Inside "Connection & system" (now the LAST group) the four low-level
  // pre-dumps (Reception / Sending / Encoder / Media Status) were merged into a
  // single collapsed `<details class="diag-disclosure">` "Raw stats" disclosure
  // (`summary#diag-h-raw-stats`), and "Build info" became a second collapsed
  // `<details>` at the very bottom. Both are CLOSED by default (no `open` attr).
  //
  // OLD-STRUCTURE FAILURE: in iteration 2 the same four pre-dumps were ALWAYS
  // visible as standalone `Reception Stats` / `Sending Stats` / `Encoder
  // Settings` / `Media Status` `<section>`s (their `<pre>` content visible
  // without any click), and Build info was an always-open `<section>`. Asserting
  // the content is HIDDEN until the summary is clicked fails against that
  // structure (the content would already be visible). Asserting all four blocks
  // live inside ONE disclosure also fails (they were four separate sections).
  test("Connection & system: Raw stats + Build info are collapsed <details> that expand on click (#1131 iter 3)", async ({
    page,
  }) => {
    await joinMeeting(page, "raw_stats_disclosure");
    await openPerformanceDrawer(page);

    const sidebar = perfDrawer(page);

    // ── Raw stats disclosure ──
    const rawSummary = sidebar.locator("summary#diag-h-raw-stats");
    await expect(rawSummary).toBeVisible({ timeout: 15_000 });
    await expect(rawSummary).toHaveText(/Raw stats/);
    // The chevron affordance (iteration 3) lives inside the summary.
    await expect(rawSummary.locator(".diag-disclosure-chev")).toHaveCount(1);

    // The enclosing <details> is the disclosure; its merged-blocks container.
    // Use the CSS :has() child form (NOT { has: rawSummary }): Playwright
    // re-scopes an inner locator that is ITSELF `sidebar`-scoped against the
    // outer match, double-scoping it to `#diagnostics-sidebar … #diagnostics-
    // sidebar …` and resolving to 0 matches. The CSS form keys off the summary id
    // directly and avoids that quirk.
    const rawDetails = sidebar.locator("details.diag-disclosure:has(> summary#diag-h-raw-stats)");
    const rawBody = rawDetails.locator(".diagnostics-data");
    const rawBlocks = rawBody.locator(".diag-raw-block");

    // COLLAPSED BY DEFAULT: a closed <details> does not render its non-summary
    // content, so the merged blocks are not visible until the summary is clicked.
    await rawSummary.scrollIntoViewIfNeeded();
    await expect(rawBody).toBeHidden();

    // EXPAND: clicking the summary opens the disclosure and reveals the four
    // merged blocks (Reception / Sending / Encoder / Media Status), each with its
    // own sub-heading <h4>. (The standalone iteration-2 sections are gone.)
    await rawSummary.click();
    await expect(rawBody).toBeVisible();
    await expect(rawBlocks).toHaveCount(4);
    const rawHeadings = rawBlocks.locator("h4");
    await expect(rawHeadings).toHaveText([
      "Reception Stats",
      "Sending Stats",
      "Encoder Settings",
      "Media Status",
    ]);

    // ── Build info disclosure (the second, bottom-most collapsed <details>) ──
    const buildSummary = sidebar.locator("summary#diag-h-build-info");
    await buildSummary.scrollIntoViewIfNeeded();
    await expect(buildSummary).toBeVisible();
    await expect(buildSummary).toHaveText(/Build info/);
    // Same CSS :has() child form as the Raw-stats disclosure above, to avoid the
    // Playwright same-scope `{ has: <sidebar-scoped> }` double-scoping quirk.
    const buildDetails = sidebar.locator(
      "details.diag-disclosure:has(> summary#diag-h-build-info)",
    );
    const buildTable = buildDetails.locator(".build-info-table");
    // COLLAPSED BY DEFAULT (was an always-open section in iteration 2).
    await expect(buildTable).toBeHidden();
    await buildSummary.click();
    await expect(buildTable).toBeVisible();

    // Issue #1480 — Build info columns. The `Built` column is now ALWAYS present
    // (it was not before #1480). The Commit/Branch columns are gated on
    // showBuildGitInfo: the committed e2e config.js ships it "true", so this
    // solo-runner stack renders the 4-col `--git` variant. The header cells
    // (`.build-info-cell` inside `.build-info-header`) pin the exact columns.
    const buildHeaderCells = buildTable.locator(".build-info-header .build-info-cell");
    await expect(buildTable).toHaveClass(/build-info-table--git/);
    await expect(buildHeaderCells).toHaveText(["Component", "Commit", "Branch", "Built"]);
  });

  // ── ITERATION 3/4 — All-Peers placeholder vs single-peer NetEq charts (#1131 / #1222) ─
  //
  // The NetEq Current-Status + time-series charts are now per-PEER. The placeholder
  // ("Select a specific peer to view time-series charts and current status.")
  // shows whenever the aggregate "All Peers" view is selected — which is the
  // case at ZERO remote peers (this solo runner's reality) AND at 2+ remote peers
  // until the user picks one. Selecting a specific peer renders the redesigned
  // two-tier Current Status (`.neteq-status` > `.status-primary` with two
  // `.status-stat`, `.status-secondary` with four `.status-row`, plus a
  // `.status-reorder` micro-row) and the horizontally-scrollable charts: each
  // chart is a fixed `.neteq-chart-y-axis` <svg> sibling + a `.neteq-chart-scroll`
  // overflow-x box (stacked 1-up inside `.neteq-charts-stack`).
  //
  // #1222 ITERATION 4 — 1:1 AUTO-SELECT: with EXACTLY ONE remote peer the drawer
  // now auto-selects it on first appearance, so the Current Status + charts
  // render BY DEFAULT (no placeholder) for that one peer. That path needs a real
  // second participant emitting NetEq stats and CANNOT be exercised on this solo
  // (zero-remote-peer) runner — the same multi-peer-harness gap (#1093) that gates
  // the single-peer arm below. The auto-select itself is host-tested in Rust
  // (`auto_select_peer` in diagnostics.rs). On this solo runner the placeholder
  // arm is the correct, accurate assertion: zero peers → All-Peers placeholder.
  //
  // The peer selector itself only appears with > 1 entry in available_peers
  // (All Peers + >= 1 remote peer that emitted NetEq stats), so the single-peer
  // arm runs only on a multi-peer runner; the placeholder arm always runs.
  //
  // OLD-STRUCTURE FAILURE: in iteration 2 the default "All Peers" selection
  // showed Current Status tiles (concatenated history / "--" tiles) and the 2×2
  // `.charts-grid`, never a "Select a specific peer…" placeholder. Asserting the
  // placeholder text under All Peers fails against iteration 2. Asserting a
  // `.neteq-chart-scroll` overflow-x box (not a fixed-width grid cell) fails
  // against the old non-scrollable 2×2 grid. Asserting the two-tier `.neteq-status`
  // structure (below) fails against the old 9-uniform-tile `.status-grid`.
  test("Live stream state: NetEq shows the All-Peers placeholder, and a selected peer shows the two-tier status + scrollable charts (#1131 iter 4)", async ({
    page,
  }) => {
    await joinMeeting(page, "neteq_placeholder");
    await openPerformanceDrawer(page);

    const sidebar = perfDrawer(page);

    // The Peer Selection dropdown only renders with > 1 entry in available_peers
    // (i.e. at least one remote peer that emitted NetEq stats). In a solo meeting
    // it is absent and "All Peers" is the implicit selection.
    const peerSelect = sidebar.locator("select.peer-selector", {
      has: page.locator('option:text-is("All Peers")'),
    });
    const placeholder = sidebar.locator(".diag-neteq-placeholder");

    // ── All-Peers arm (always runs; this solo runner has ZERO remote peers, so
    //    the 1:1 auto-select does NOT fire and the placeholder is the correct
    //    default — see the block comment on the iter-4 auto-select gap) ──
    // Under the All-Peers aggregate the NetEq section is the single placeholder,
    // and the per-peer Current-Status / per-chart help "?" buttons are ABSENT.
    await expect(placeholder).toBeVisible({ timeout: 15_000 });
    await expect(placeholder).toHaveText(
      "Select a specific peer to view time-series charts and current status.",
    );
    await expect(sidebar.locator('[data-testid="diag-status-help"]')).toHaveCount(0);
    // The section-level diag-charts-help was removed (#1222); the four per-chart
    // help icons render only with a selected peer + history, so none under the
    // All-Peers placeholder.
    await expect(sidebar.locator('[data-testid="diag-chart-buffer-help"]')).toHaveCount(0);
    await expect(sidebar.locator('[data-testid="diag-chart-decode-help"]')).toHaveCount(0);
    await expect(sidebar.locator('[data-testid="diag-chart-packets-help"]')).toHaveCount(0);
    await expect(sidebar.locator('[data-testid="diag-chart-reorder-help"]')).toHaveCount(0);
    // The new two-tier Current Status (`.neteq-status`) is not rendered either.
    await expect(sidebar.locator(".neteq-status")).toHaveCount(0);
    // No scrollable charts render under the placeholder.
    await expect(sidebar.locator(".neteq-chart-scroll")).toHaveCount(0);

    // ── Single-peer arm (multi-peer runner only) ──
    const hasSelector = (await peerSelect.count()) > 0;
    test.skip(
      !hasSelector,
      "solo meeting: no remote peer emitted NetEq stats, so the Peer Selection " +
        "dropdown is absent and only the All-Peers placeholder can be exercised",
    );

    // Pick the first specific peer (the option after "All Peers").
    const peerOptions = peerSelect.locator("option");
    const optionCount = await peerOptions.count();
    expect(optionCount, "peer selector lists All Peers + at least one peer").toBeGreaterThan(1);
    const firstPeerValue = await peerOptions.nth(1).getAttribute("value");
    expect(firstPeerValue, "first specific peer has a value").not.toBeNull();
    await peerSelect.selectOption(firstPeerValue as string);

    // Now the placeholder is gone and the Current Status cluster (with its "?"
    // help) renders for the selected peer.
    await expect(placeholder).toHaveCount(0);
    await expect(sidebar.locator('[data-testid="diag-status-help"]')).toBeVisible({
      timeout: 15_000,
    });

    // #1222 ITERATION 4 — two-tier Current Status redesign: the old 9-uniform-tile
    // `.status-grid` is GONE, replaced by `.neteq-status` (scoped to avoid the
    // identically-named `.status-*` classes the Connection Manager section uses)
    // with a primary stat pair, a four-row flow group, and a reorder micro-row.
    // Assert the structure exists for the selected peer.
    const neteqStatus = sidebar.locator(".neteq-status");
    await expect(neteqStatus).toBeVisible({ timeout: 15_000 });
    // Tier 1 — primary: exactly two `.status-stat` (Buffer + Target).
    await expect(neteqStatus.locator(".status-primary")).toBeVisible();
    await expect(neteqStatus.locator(".status-primary .status-stat")).toHaveCount(2);
    // Tier 2 — flow group: exactly four `.status-row` (Packets awaiting /
    // Packets per s / Expand rate / Accelerate rate).
    await expect(neteqStatus.locator(".status-secondary")).toBeVisible();
    await expect(neteqStatus.locator(".status-secondary .status-row")).toHaveCount(4);
    // Tier 3 — reorder micro-row: three `.status-reorder__item` (Rate /
    // Reordered / Max dist).
    await expect(neteqStatus.locator(".status-reorder")).toBeVisible();
    await expect(neteqStatus.locator(".status-reorder .status-reorder__item")).toHaveCount(3);
    // The retired iteration-2/3 tile classes must NOT resurface inside the NetEq
    // status block (the Connection Manager section owns its own same-named classes,
    // hence the `.neteq-status` scope above).
    await expect(neteqStatus.locator(".status-grid")).toHaveCount(0);
    await expect(neteqStatus.locator(".status-item")).toHaveCount(0);

    // The charts section is gated on the peer having NetEq HISTORY. A fresh
    // selection may legitimately have no samples yet → the "NetEQ Buffer / Jitter
    // History" fallback (no scrollable charts) is shown instead. Handle BOTH:
    //   - history present → `.neteq-chart-scroll` (overflow-x) + a fixed
    //     `.neteq-chart-y-axis` sibling, stacked in `.neteq-charts-stack`;
    //   - no history yet → the buffer/jitter fallback heading.
    const chartScroll = sidebar.locator(".neteq-chart-scroll").first();
    const fallbackHeading = sidebar.getByRole("heading", {
      name: "NetEQ Buffer / Jitter History",
    });
    await expect(chartScroll.or(fallbackHeading).first()).toBeVisible({ timeout: 15_000 });

    if ((await sidebar.locator(".neteq-chart-scroll").count()) > 0) {
      // Scrollable charts present: assert the time-series structure.
      const stack = sidebar.locator(".neteq-charts-stack");
      await expect(stack).toBeVisible();
      // Four charts stacked 1-up, each with its own scroll box and a fixed Y-axis.
      await expect(sidebar.locator(".neteq-chart-scroll")).toHaveCount(4);
      await expect(sidebar.locator(".neteq-chart-y-axis")).toHaveCount(4);

      // The scroll box is a horizontal-overflow container (the iteration-3 change
      // from the old fixed 2×2 grid). Assert computed overflow-x is scrollable —
      // this is the load-bearing "now scrollable" contract; it does NOT assert a
      // fixed direction "fits", honouring the #1208/#1213 lesson.
      const overflowX = await chartScroll.evaluate((el) => getComputedStyle(el).overflowX);
      expect(overflowX, "the NetEq chart container scrolls horizontally").toMatch(/auto|scroll/);

      // The fixed Y-axis svg is a SIBLING of the scroll box inside the wrapper
      // (rendered OUTSIDE the scroll container so it stays pinned while the
      // timeline scrolls). Each `.neteq-chart-wrapper` holds exactly one of each.
      const wrapper = sidebar.locator(".neteq-chart-wrapper").first();
      await expect(wrapper.locator(".neteq-chart-y-axis")).toHaveCount(1);
      await expect(wrapper.locator(".neteq-chart-scroll")).toHaveCount(1);
      // Sibling proof: the y-axis is NOT a descendant of the scroll box (it would
      // otherwise be clipped). The scroll box contains the growing SVG, not the
      // fixed axis.
      await expect(
        wrapper.locator(".neteq-chart-scroll .neteq-chart-y-axis"),
        "the fixed Y-axis must live OUTSIDE the scroll box (a sibling, not a child)",
      ).toHaveCount(0);
    }
  });

  /**
   * Issue #2170 — THE discriminator for the self-view readout, and it needs no AQ ramp.
   *
   * `live_quality_snapshot` used to report `VIDEO_QUALITY_TIERS[v_idx]` — the AQ
   * tier's BOUNDING BOX — while the encoder emitted the 640x480 fake capture fitted
   * into the ladder. It now reports the top ACTIVE rung's dims as published by the
   * encode loop. At cold start `initial_active_layer_count() == 1`, so this reads the
   * BASE rung immediately and climbs as the ramp earns layers; either way it can
   * never be a tier box, because fitting only SHRINKS and preserves the capture's 4:3
   * aspect, which no 16:9 tier box shares.
   *
   * MUTATION, RUN rather than predicted: reverting `live_quality_snapshot` to
   * `video_width: v.max_width, video_height: v.max_height` makes the readout report a
   * tier box and the poll fails. Note the observed mutant value is NOT the cold-start
   * `854x480` a first guess suggests — `medium` is only the STARTING tier, and over
   * the poll window the AQ PID ramps up on a localhost link, so the mutant reports a
   * higher tier's box. That is why the negative below enumerates EVERY box rather
   * than pinning one.
   *
   * NOT tagged @bvt0/@bvt1, so this does NOT run in per-PR CI (`--project=bvt1`).
   * The per-PR guard for the RENDERED readout is the wasm component test
   * `dioxus-ui/tests/send_video_readout_unknown_dims.rs` (which covers the SENTINEL
   * branch); this spec is the real-encoder check on the PUBLISHED branch, which no
   * host or wasm test can reach — it needs a live encode loop. Validate via the
   * local docker stack:
   *   make e2e-up && make e2e SPEC=performance-settings.spec.ts
   */
  test("self-view readout shows the FITTED encode size, never the AQ tier box", async ({
    page,
  }) => {
    await enableSimulcastFlag(page.context(), 3, { capabilityMaxLayersOverride: 3 });
    await joinMeeting(page, "fitted_selfview", { ensureCameraOn: true });
    await openPerformanceDrawer(page);
    await selectSendDirection(page);
    const panel = perfDrawer(page);

    // LADDER-DEPTH PREMISE — assert the 3-rung injection took effect BEFORE the
    // accept-list below depends on it. Same precedent as the `max="2"` assertion in
    // the `no SEND "Fixed" badge` test above.
    //
    // Load-bearing, not decoration: the accept-list `240x180|480x360|640x480` is the
    // 3-RUNG fit set. If injection silently failed the encoder drops to
    // single-stream, where the capture fits against the AQ TIER box instead — green
    // at tiers 0-5 (all fit to 640x480) but RED at tiers 6-7 (360x270 / 320x240), a
    // latent flake rather than a false green. Injection really can lose: this
    // describe's `beforeEach` already registers a `**/config.js` route via
    // `enableSimulcastFlag(ctx, 3)` with NO capability override, so the per-test call
    // above is competing with it on the same context.
    //
    // `max` is `labels.len() - 1`, so "2" == a 3-rung ladder as the encoder actually
    // resolved it (flag AND capability), not as the flag requested it.
    await expect(
      panel.locator('[data-testid="perf-video-range-max"]'),
      "the 3-rung injection must have taken effect, or the accept-list below is the " +
        "wrong fit set and this test flakes at low AQ tiers",
    ).toHaveAttribute("max", "2");

    // Capture the value the poll ACCEPTED. A second, unpolled read can land on a
    // later state and make the negative assertions below pass vacuously — the
    // anti-pattern this file's own #2170 review flagged.
    let readout = "";
    await expect
      .poll(
        async () => {
          readout = (await panel.locator("#perf-vu-video-readout").textContent()) ?? "";
          return readout;
        },
        { timeout: 25_000 },
      )
      .toMatch(/^(240x180|480x360|640x480)·\d+fps·\d+kbps$/);

    // Belt-and-braces negative over every `VIDEO_QUALITY_TIERS` bounding box.
    //
    // Be honest about what this is: given the anchored poll above
    // (`/^(240x180|480x360|640x480)·…$/`), `readout` provably contains none of these,
    // so this loop CANNOT fail — it is documentation of the excluded set, not an
    // independent check. The poll is the real assertion. Kept because it names the
    // eight values a reader would otherwise have to look up, and because if the poll
    // regex is ever loosened this becomes load-bearing again.
    for (const tierBox of [
      "1920x1080",
      "1600x900",
      "1280x720",
      "960x540",
      "854x480",
      "640x360",
      "480x270",
      "426x240",
    ]) {
      expect(readout, `readout must not report the ${tierBox} AQ tier box`).not.toContain(tierBox);
    }
  });

  /**
   * The SELF-TILE overlay consumer, on the real production path — issue #2170.
   *
   * `host.rs::self_metrics_overlay` reads the same `LiveQualitySnapshot` as the
   * drawer readout and renders it on the local `.self-camera` element. Its
   * resolution field is now the published encode geometry rather than the AQ tier
   * box, and it is gated on `video_width > 0 && video_height > 0` — a gate that
   * could NEVER fire before #2170, because a tier box is never zero.
   *
   * WHY THIS LIVES HERE AND NOT IN `media-metrics-overlay.spec.ts`: the SELF overlay
   * needs no remote peer. It renders from `Host`'s own snapshot reader, gated only on
   * the diagnostics checkbox. The 2-peer spec is currently unable to reach any
   * assertion at all (issue #2193 — the peer canvas tile never appears), so putting
   * the only guard for this consumer there would leave it permanently unverified.
   * A solo meeting traverses the entire production chain this PR changed — live
   * encoder → `publish_layer_dims` → `top_published_layer_dims` →
   * `live_quality_snapshot` → `host.rs` → rendered DOM — which is exactly what the
   * host and wasm tests CANNOT do (they inject a snapshot).
   *
   * What a solo meeting deliberately does NOT exercise: the `[..active]` bound and
   * the `.rev()` scan non-degenerately, since with no receiver the relay's
   * layer-union hint pins `active = 1` and both reduce to "read slot 0". Those are
   * covered by the client-side host tests
   * (`quality_snapshot_tracks_the_shed_and_does_not_report_shed_rungs`,
   * `top_published_layer_dims_picks_the_highest_published_active_rung`), and the
   * 2-peer assertion in `media-metrics-overlay.spec.ts` will cover them end-to-end
   * once #2193 is fixed.
   *
   * The overlay formats dims with `×` (U+00D7), unlike the readout's ASCII `x` —
   * different formatters, deliberately not unified.
   */
  test("self-tile overlay reports the FITTED encode size, never the AQ tier box", async ({
    page,
  }) => {
    await enableSimulcastFlag(page.context(), 3, { capabilityMaxLayersOverride: 3 });
    await joinMeeting(page, "fitted_selftile", { ensureCameraOn: true });
    // The overlay is off by default and lives behind the diagnostics checkbox.
    await openPerformanceDrawer(page);
    const toggle = page.locator('[data-testid="media-metrics-overlay-toggle"]');
    await expect(toggle).toBeVisible({ timeout: 10_000 });
    await toggle.check();

    const selfOverlay = page.locator('[data-testid="media-metrics-overlay-self"]');
    await expect(selfOverlay).toBeVisible({ timeout: 20_000 });

    // LADDER-DEPTH PREMISE, same reasoning as the readout test above: assert the
    // 3-rung injection took effect before the geometry assertions rely on it. The
    // perf panel is already open (this test opened the drawer to reach the overlay
    // checkbox), so the slider is in the DOM.
    await expect(
      perfDrawer(page).locator('[data-testid="perf-video-range-max"]'),
      "the 3-rung injection must have taken effect, or a single-stream fit against " +
        "the AQ tier box could satisfy the 4:3 invariant at a lower tier",
    ).toHaveAttribute("max", "2");

    // Capture the polled value and assert on THAT string, so the negatives cannot
    // read a different observation than the poll accepted. The regex requires
    // digits, so the poll also waits out the pre-first-frame em-dash rather than
    // asserting against it.
    let overlayText = "";
    await expect
      .poll(
        async () => {
          overlayText = (await selfOverlay.textContent()) ?? "";
          return overlayText;
        },
        { timeout: 30_000 },
      )
      .toMatch(/↑\s*\d+×\d+\s*·/);

    const m = overlayText.match(/(\d+)×(\d+)/);
    expect(m, `self overlay must report a dimension pair: ${overlayText}`).not.toBeNull();
    const [w, h] = [Number(m![1]), Number(m![2])];

    // Chromium's fake capture is 4:3 and `fit_within_preserving_aspect` only ever
    // SHRINKS while preserving aspect, so every legitimate value stays 4:3 — while
    // every simulcast rung AND every AQ tier box is 16:9. Asserting the invariant
    // rather than enumerating values means this does not rot when a rung's box
    // changes. Tolerance covers `round_down_even`'s per-axis flooring.
    expect(
      Math.abs(w / h - 4 / 3),
      `the self overlay reported ${w}×${h}; a fitted 4:3 capture must stay 4:3, ` +
        `whereas every rung and tier box is 16:9 (${(16 / 9).toFixed(3)})`,
    ).toBeLessThan(0.05);
    // Fitting never upscales past the capture, which also excludes the five
    // largest tier boxes outright.
    expect(w, "fitting never upscales beyond the capture width").toBeLessThanOrEqual(640);
    expect(h, "fitting never upscales beyond the capture height").toBeLessThanOrEqual(480);
  });

  /**
   * NOT COVERED HERE — the em-dash `format_video_readout` renders for the `(0, 0)`
   * NOT-YET-PUBLISHED sentinel. Read the whole note before extending this spec: an
   * earlier version of it got the reason WRONG in a way worth recording.
   *
   * What is correct: `cameraOff: true` does NOT reach this branch. It yields
   * `"Camera — off"`, because `format_video_readout(s)` is only called when the
   * snapshot is `Some`, and `host.rs`'s reader gates on `s.prev_video_enabled`, so a
   * camera-off snapshot is `None` and `VIDEO_EMPTY_READOUT` preempts it (the `match`
   * in `gauge_state_from_snapshot`). Verified in code.
   *
   * What was WRONG: this note used to claim the sentinel window is "milliseconds"
   * and therefore not deterministically pollable. It is not milliseconds. `host.rs`'s
   * camera effect sets `s.prev_video_enabled = video_enabled` BEFORE calling
   * `camera.start()`, and `stop()` clears the published dims — so across a mid-meeting
   * camera OFF→ON the reader returns `Some` with `(0, 0)` for the ENTIRE
   * `getUserMedia` + acquire + `build_layer` window. That is hundreds of ms, and
   * longer on a device switch, which re-runs acquisition.
   *
   * And a harness for it EXISTS (this is the search this note previously skipped):
   * `e2e/tests/prejoin-device-passthrough.spec.ts::"OFF then switch camera then ON
   * binds the newly selected camera @camera"` already drives exactly that sequence
   * mid-meeting under `device-count=3`, and
   * `::"join with camera ON converges to a live track after a forced devicechange
   * double-start @camera"` drives the double-`start()` path. Neither opens the
   * performance drawer today, so neither observes this readout — but the state IS
   * reachable there, so "not pollable" was false.
   *
   * Why it is still deferred, stated as a scope decision rather than an
   * impossibility: reaching the readout from that spec means also opening the
   * diagnostics drawer inside a camera-switch flow, and the assertion would race the
   * publish it is trying to observe — a poll that must catch a transient BEFORE the
   * first frame lands is a race the spec cannot lock (a fast local acquire can beat
   * the drawer open). That is the "an e2e spec cannot lock a race" lesson; the
   * deterministic guard for a transient belongs at the unit/component level, which is
   * where it is:
   *   - `dioxus-ui/tests/send_video_readout_unknown_dims.rs` — the RENDERED DOM text,
   *     in a real browser, by injecting a sentinel snapshot. Runs in per-PR CI.
   *   - host tests `video_readout_renders_an_em_dash_for_the_unpublished_sentinel`
   *     and `gauge_state_video_text_is_the_em_dash_when_nothing_is_published`.
   *
   * `media_metrics_overlay.rs::self_line_renders_an_em_dash_for_unpublished_encode_geometry`
   * is deliberately NOT in that list, though it covers the self-TILE rendering of the
   * same sentinel: it passes `None` by hand to a pure formatter #2170 does not change,
   * so reverting `live_quality_snapshot` leaves it green (mutation run). It documents
   * a newly-reachable state; it does not guard this change.
   */
});

/**
 * The camera ladder's ONE user-visible SEND surface: the drawer's rung strip. The
 * Rust unit tests pin the label logic; only this spec sees it RENDERED — the pip
 * text and its tooltip, through the real config.js -> host.rs -> panel chain, at
 * the capability-clamped layer count a real session gets.
 *
 * NOT tagged @bvt0/@bvt1, so this does NOT run in per-PR CI (`--project=bvt1`);
 * validate via the local docker stack:
 *   make e2e-up && make e2e SPEC=performance-settings.spec.ts
 */
test.describe("camera ladder SEND rung labels", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // Auth is bypassed via a JWT session cookie, exactly as every other describe in
  // this file does. WITHOUT this the app redirects to the meeting-api login page and
  // `joinMeeting`'s `toHaveURL(/\/meeting\/…/)` times out.
  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  /**
   * `capabilityMaxLayersOverride: 3` is LOAD-BEARING, not belt-and-braces.
   * `host.rs` computes `effective_max_layers = min(flag, capability_ceiling)`, and
   * the containerized CI/dev runner reports 1–2 logical cores, which clamps the
   * sniffed ceiling to 1. At one layer the Video arm returns just `["180p"]`, the
   * top-rung pip is never rendered, and the assertion below would be VACUOUSLY
   * green — testing nothing. Forcing 3 is what makes the top pip exist.
   */
  const THREE_LAYERS = { capabilityMaxLayersOverride: 3 } as const;

  /**
   * Read the top video SEND pip's label after opening the drawer.
   *
   * Returns `null` when fewer than 3 pips rendered — with the caller forcing
   * `capabilityMaxLayersOverride: 3` that means the config injection did not take,
   * so the caller treats a null as a HARD FAILURE rather than a skip.
   */
  async function topVideoSendPipLabel(page: Page): Promise<string | null> {
    await openPerformanceDrawer(page);
    await selectSendDirection(page);
    const panel = perfDrawer(page);
    const pips = panel.locator('[data-testid^="perf-video-send-rung-"]');
    await expect(pips.first()).toBeVisible({ timeout: 10_000 });
    if ((await pips.count()) < 3) return null;
    return (
      await panel.locator('[data-testid="perf-video-send-rung-2"] .perf-rung__label').innerText()
    ).trim();
  }

  test("the top SEND pip reads 720p (the shipped ladder)", async ({ page }) => {
    await enableSimulcastFlag(page.context(), 3, THREE_LAYERS);
    await joinMeeting(page, "ladder_default", { cameraOff: true });

    // Deliberately NOT `test.skip`: `THREE_LAYERS` guarantees 3 pips, so a null
    // means the config injection broke, not that the runner is weak.
    const label = await topVideoSendPipLabel(page);
    expect(
      label,
      "3 video SEND pips must render (testCapabilityMaxLayersOverride: 3 forces them). " +
        "A null here means the config.js injection did not take.",
    ).not.toBeNull();
    expect(
      label,
      "flag off must keep the shipped 720p top rung — this is the default-OFF guarantee",
    ).toBe("720p");
  });
});

/**
 * The RECEIVE-side rung labels must come from the camera ladder on all three
 * surfaces: the slider end label, both thumbs' `aria-valuetext` (WCAG 2.2 4.1.2),
 * and the band readout. The Rust unit tests pin the label logic; only this spec
 * sees all three RENDERED from one mounted panel.
 *
 * WHY IT CANNOT VACUOUSLY PASS. Unlike the SEND rung strip, the RECEIVE slider's
 * depth is NOT CPU-clamped: it comes from `receive::labels_for(kind).len()`, a
 * constant 3, so `max="2"` and the top-rung end label ALWAYS render regardless of
 * the runner's core count. There is no skip path — a failure here is a real failure.
 *
 * NOT tagged @bvt0/@bvt1, so this does NOT run in per-PR CI (`--project=bvt1`);
 * validate via the local docker stack:
 *   make e2e-up && make e2e SPEC=performance-settings.spec.ts
 */
test.describe("camera ladder RECEIVE rung labels", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // Auth bypass via JWT session cookie, as every other describe in this file does.
  // Without it the app redirects to the meeting-api login page and `joinMeeting`'s
  // `toHaveURL(/\/meeting\/…/)` times out.
  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  /** Same 3-layer capability override the #1768 SEND describe uses. */
  const THREE_LAYERS = { capabilityMaxLayersOverride: 3 } as const;

  /**
   * Open the drawer and read the RECEIVE video slider's three user-visible
   * rung-label surfaces at the TOP rung:
   *   - `endLabel`  — the right-hand `.perf-range-end-label` text
   *   - `valueText` — `aria-valuetext` on the MAX thumb (what a screen reader speaks)
   *   - `bandText`  — the "Receiving: {lo} – {hi}" caption
   *
   * The min thumb is driven to the TOP too, so the MIN thumb's `aria-valuetext` is
   * read at index 2 as well: the issue names only the max thumb, but the min thumb
   * speaks the same wrong label when a user drags it up.
   */
  async function readReceiveVideoTopLabels(page: Page): Promise<{
    endLabel: string;
    maxValueText: string | null;
    minValueText: string | null;
    bandText: string;
  }> {
    await openPerformanceDrawer(page);
    await selectReceiveDirection(page);
    const panel = perfDrawer(page);

    const maxInput = panel.locator('[data-testid="perf-recv-video-range-max"]');
    const minInput = panel.locator('[data-testid="perf-recv-video-range-min"]');
    await expect(maxInput).toBeVisible({ timeout: 10_000 });

    // The receive ladder is a fixed 3 rungs (NOT capability-clamped), asserted
    // rather than tolerated: every label assertion below is vacuous at depth 1.
    await expect(
      maxInput,
      "the receive video ladder is a constant 3 rungs, so the top thumb position must be 2",
    ).toHaveAttribute("max", "2");

    // Drive the MIN thumb to the top as well so its aria-valuetext is read at the
    // top rung too (it collapses the band, which is a legal state).
    await setRangeValue(page, "perf-recv-video-range-min", 2);
    await expect(minInput).toHaveValue("2");

    const endLabel = (
      await panel.locator('[data-testid="perf-recv-video-range-max"]').evaluate((el) => {
        // The right-hand end label is the LAST `.perf-range-end-label` inside this
        // slider's own `.perf-range` container — scoped to the container so the
        // send-side sliders' end labels in the same card cannot be picked up.
        const range = el.closest(".perf-range");
        const labels = range?.querySelectorAll(".perf-range-end-label");
        return labels && labels.length > 0 ? (labels[labels.length - 1].textContent ?? "") : "";
      })
    ).trim();

    const maxValueText = await maxInput.getAttribute("aria-valuetext");
    const minValueText = await minInput.getAttribute("aria-valuetext");
    const bandText = (
      await panel.locator('[data-testid="perf-recv-video-range-value"]').innerText()
    ).trim();

    return { endLabel, maxValueText, minValueText, bandText };
  }

  test("the RECEIVE video top rung reads 720p (the shipped ladder)", async ({ page }) => {
    await enableSimulcastFlag(page.context(), 3, THREE_LAYERS);
    await joinMeeting(page, "recv_ladder_default", { cameraOff: true });

    const labels = await readReceiveVideoTopLabels(page);
    expect(labels.endLabel, "the receive end label must read the ladder top rung").toBe("720p");
    expect(labels.maxValueText, "the MAX thumb must SPEAK 720p (WCAG 2.2 4.1.2)").toBe("720p");
    expect(labels.minValueText, "the MIN thumb at index 2 must also speak 720p").toBe("720p");
    expect(labels.bandText, "the visible band readout must say 720p").toContain("720p");
  });
});
