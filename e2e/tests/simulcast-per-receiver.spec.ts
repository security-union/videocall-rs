/**
 * E2E: per-receiver simulcast (PR #1079, issue #989 P1–P5).
 *
 * The feature is FLAG-GATED OFF in production (`experimentalSimulcastMaxLayers`
 * defaults to 1 = single layer; effective layers =
 * `min(flag, device-capability-ceiling)`). This spec ENABLES the flag for the
 * test browser only via `enableSimulcastFlag` (a `/config.js` route patch — it
 * does NOT modify the committed `dioxus-ui/scripts/config.js` nor the
 * developer's gitignored `config.local.js`).
 *
 * ## STATUS: MULTI-PARTY TESTS ARE `test.fixme` PENDING #1093
 *
 * EVERY test in this spec joins TWO (or three) authenticated browser contexts —
 * a publisher + receiver(s) — each running camera + simulcast encode/decode.
 * In headless CI that crashes the renderer ("Target page/context closed") so the
 * 2nd context never reaches the grid, AND the capability ceiling clamps the
 * runner to 1 layer (so the multi-layer assertions would skip anyway). All of
 * these are therefore marked `test.fixme` (skipped, not run) until issue #1093
 * lands a renderer-crash-resilient / netsim runner + a capability-override hook
 * to force >=2 layers. The single-context structural coverage of the receive
 * Performance panel lives in `performance-settings.spec.ts` (#1078 Receive-side
 * controls), which is green. The `@impair` WS-divergence test stays `@impair`-
 * gated (and is subject to the same #1093 limits); the WT case stays `fixme`.
 *
 * The descriptions below document the INTENDED behaviour each `fixme` test will
 * assert once #1093 unblocks them.
 *
 * ## What runs in the default suite vs. the `@impair` project
 *
 * Most tests run in the default `dioxus` suite. The per-receiver congestion
 * DIVERGENCE test (issue #1080) instead needs per-client downlink shaping, which
 * the harness now provides for the WS path via the toxiproxy `impair` compose
 * profile + `helpers/downlink-impair.ts`. That test is tagged `@impair` and is
 * grep-inverted OUT of the default `dioxus`/bvt suites — it runs ONLY under
 * `--project=impair` against a stack started with `make e2e-up-impair`, so the
 * standard CI Playwright run is unaffected. Therefore:
 *
 *   - RUNS IN CI:
 *       1. Flag-on multi-layer SEND is active (proxy: a healthy receiver's
 *          received-layer ladder reports `layer_count > 1`).
 *       3. Receive-threshold enforcement via the Performance panel (the user's
 *          key requirement — needle never exceeds the configured max).
 *       4. Default Auto (full range; needle free to reflect auto-selection).
 *
 *       5. Performance panel renders ALL THREE received-quality controls
 *          (video + audio + content) — #1082 structural assertion.
 *       6. AUDIO layering active under the flag (#1082-B: 2 → 3 layers). The
 *          audio needle readout's `L{i}/{N} · {kbps} kbps` reports the live
 *          per-snapshot `layer_count` (the only DOM signal of audio simulcast);
 *          the ladder-length and bitrate invariants are asserted unconditionally
 *          and the >1-layer assertion is capability-gated like VIDEO send.
 *
 *   - FLAG-OFF CONTROL (separate describe block at the bottom):
 *       Flag pinned to 1 via `pinSimulcastMaxLayers(ctx, 1)` = single layer =
 *       feature OFF. (The runtime DEFAULT was flipped 1 → 3, so the OFF path can
 *       no longer be reached by simply omitting the flag — it must be pinned to 1
 *       explicitly.) The publisher then emits a SINGLE layer for every kind, so
 *       each received-quality readout reports `/1`. This guards the
 *       no-regression byte-identical single-layer path for #1082 (the ladder
 *       machinery went N-generic but the single-layer behavior
 *       must be unchanged).
 *
 *   - RUNS UNDER THE `@impair` PROJECT ONLY (issue #1080):
 *       2. Per-receiver congestion DIVERGENCE (WS path) — one of two
 *          co-receivers has its WS downlink bandwidth-clamped via toxiproxy,
 *          which overflows the relay's bounded per-receiver outbound channel;
 *          the relay sheds that receiver's video frames, the resulting sequence
 *          gaps push its `loss_per_sec` over the chooser's step-down threshold,
 *          and ONLY that receiver drops to a lower layer (sender + healthy peer
 *          unaffected). See `helpers/downlink-impair.ts` for the full verified
 *          mechanism — the relay-side overflow is what manufactures the loss a
 *          raw bandwidth throttle alone could not. This test is TAGGED `@impair`
 *          and is grep-inverted OUT of the default `dioxus` suite + bvt0/bvt1
 *          (playwright.config.ts), so the standard CI run never touches it. It
 *          runs ONLY under `--project=impair`, which needs the toxiproxy
 *          `impair` compose profile up (`make e2e-up-impair`; run via
 *          `make e2e-impair`). See `TODO(ci)` in that test for the dedicated
 *          CI-job follow-up.
 *
 *   - STILL BLOCKED (documented `test.fixme`) — issue #1080, WT path:
 *       The same divergence over WebTransport/QUIC cannot be produced: toxiproxy
 *       is TCP-only and Playwright's proxy cannot carry QUIC/UDP, and per-client
 *       UDP `netem` needs an isolated netns the shared Playwright harness does
 *       not provide. Kept as `test.fixme` with the concrete blocker inline.
 *
 * ## Capability-ceiling caveat (see helpers/simulcast-config.ts)
 *
 * `capability_max_simulcast_layers()` reads a live ~100ms CPU benchmark with no
 * test override. On a weak CI runner the ceiling can clamp to 1 even with the
 * flag = 3, in which case the publisher emits a single layer. Test 1 detects
 * this (`layer_count <= 1`) and SKIPS rather than asserts a false negative.
 *
 * Selectors used (all stable, defined in dioxus-ui source). This spec targets
 * the RECEIVE side only; since the unified send+receive panel landed (#1078) the
 * receive controls/needles live under the `perf-recv-*` / `perf-vu-recv-*`
 * namespace (the bare `perf-*` / `perf-vu-*` ids are now the SEND side):
 *   - `[data-testid="open-settings"]`               toolbar gear (settings modal)
 *   - `.device-settings-modal`                      the settings modal root
 *   - `role="tab" name="Performance"`               Performance nav tab
 *   - `#settings-panel-performance`                 the perf tabpanel
 *   - `#perf-vu-recv-video-readout`                 video received-quality readout
 *       text format: `L{idx+1}/{count} · {w}x{h}` or "Not receiving"
 *   - `#perf-vu-recv-audio-readout`                 audio received-quality readout
 *       text format: `L{idx+1}/{count} · {kbps} kbps` or "Not receiving"
 *   - `[data-testid="perf-recv-video-range-max"]`   video max-layer range thumb
 *   - `[data-testid="perf-recv-video-auto"]`        video Auto toggle (aria-pressed)
 */

import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { createAuthenticatedContext, BROWSER_ARGS } from "../helpers/auth-context";
import { enableSimulcastFlag, pinSimulcastMaxLayers } from "../helpers/simulcast-config";
import {
  routeDownlinkThroughProxy,
  impairDownlink,
  healDownlink,
  assertProxyUp,
} from "../helpers/downlink-impair";
import { waitForServices } from "../helpers/wait-for-services";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Drive a fresh page from the HOME FORM into the meeting grid, navigating the
 * #1061 pre-join device-preview screen on the way.
 *
 * This mirrors the PROVEN 2-context flow in `two-users-meeting.spec.ts`
 * (which also uses `createAuthenticatedContext`): go to the home page, type the
 * meeting id + display name, press Enter, then race the pre-join Start/Join
 * action button against the grid. A direct `goto('/meeting/{id}')` did NOT work
 * for these contexts (it failed to surface the pre-join card / crashed) — the
 * home-form path is what reliably establishes the display-name context the
 * meeting page needs, so we replicate it exactly here.
 *
 * `vc_prejoin_camera_on=true` is seeded via an init script BEFORE the app boots
 * so the publisher's camera is ON and the encoder actually emits video — the
 * receive-side needle assertions need a real decoded stream, and the pre-join
 * camera defaults to OFF. (`AttendantsComponent` reads `load_preferred_camera_on`
 * at join, so this carries through both the Start-Meeting click and the
 * auto-join effect.)
 *
 * Applies to BOTH publisher and receiver contexts.
 */
async function joinMeeting(page: Page, meetingId: string, displayName: string): Promise<void> {
  // Pre-join camera defaults to OFF; force it ON before the app boots so the
  // publisher emits video. addInitScript runs on every navigation in this page
  // before the page's own scripts.
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("vc_prejoin_camera_on", "true");
    } catch {
      /* storage may be unavailable pre-navigation; the app origin sets it */
    }
  });

  // ── Home form: enter the meeting id + display name, then submit (Enter). ──
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });

  // Display name is a controlled input — clear before typing to handle pre-fill.
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(displayName, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);

  // ── Pre-join card → grid. The meeting page may auto-join straight to the grid
  //    once the display name is set, OR present the pre-join card with a
  //    Start/Join action button. Race both. ──
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto" as const),
  ]);

  if (result === "join") {
    // Deterministically start the camera on the pre-join card BEFORE joining so
    // the publisher actually emits video (the receive-side needle assertions
    // need a real decoded stream). The persisted camera-ON preference alone is
    // NOT sufficient: `resolve_initial_enabled` (context.rs) only enables the
    // camera at join when the pre-join device list is populated, which requires
    // getUserMedia to have run. So grant media + ensure the camera toggle is ON
    // + await a live preview track, then click the action button.
    const allow = page.locator('[data-testid="prejoin-permission-allow"]');
    if (await allow.isVisible().catch(() => false)) {
      await allow.click();
      await page
        .locator('[data-testid="prejoin-permission-prompt"]')
        .waitFor({ state: "hidden", timeout: 15_000 })
        .catch(() => {
          /* already granted / prompt absent */
        });
    }

    const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
    if (await cameraToggle.isVisible().catch(() => false)) {
      if ((await cameraToggle.getAttribute("aria-pressed")) !== "true") {
        await cameraToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
      }
      // Best-effort wait for a live preview track so the device list is
      // populated before join (this is what starts the in-meeting encoder).
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
    }

    await page.waitForTimeout(500);
    await joinButton.click().catch(() => {
      /* auto-join already unmounted the pre-join button */
    });
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/**
 * Open Settings → Performance and return the visible perf tabpanel locator.
 *
 * The unified panel (#1078) has a `Receive | Send` direction toggle and renders
 * ONLY the active direction's three rows. Receive is the default, but this whole
 * spec reads RECEIVE needles/controls, so we click the Receive segment defensively
 * to guarantee the receive rows are mounted (the `@impair` divergence test reads
 * the receive needle and must be on this direction).
 */
async function openPerformancePanel(page: Page) {
  await page.locator('[data-testid="open-settings"]').click();
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  await page.getByRole("tab", { name: "Performance" }).click();
  const panel = page.locator("#settings-panel-performance");
  await expect(panel).toBeVisible({ timeout: 10_000 });
  // Ensure the RECEIVE direction is active (default, but assert for isolation).
  const recvSeg = page.locator('[data-testid="perf-direction-receive"]');
  await recvSeg.click();
  await expect(recvSeg).toHaveAttribute("aria-checked", "true", { timeout: 5_000 });
  return panel;
}

/**
 * Parse the video received-quality readout `#perf-vu-recv-video-readout`.
 * Returns null while the readout reads "Not receiving" (nothing decoded yet),
 * otherwise `{ layerIndex, layerCount }` (1-based "L{idx+1}/{count}" → 0-based).
 */
async function readVideoLayer(
  page: Page,
): Promise<{ layerIndex: number; layerCount: number } | null> {
  const text = (await page.locator("#perf-vu-recv-video-readout").textContent())?.trim() ?? "";
  const m = text.match(/^L(\d+)\/(\d+)/);
  if (!m) return null;
  return { layerIndex: Number(m[1]) - 1, layerCount: Number(m[2]) };
}

/**
 * Parse the AUDIO received-quality readout `#perf-vu-recv-audio-readout`.
 *
 * The audio readout format (see `format_readout` in
 * `dioxus-ui/src/components/performance_settings.rs`) is
 * `"L{idx+1}/{count} · {kbps} kbps"` while receiving, or "Not receiving" before
 * the first audio frame is decoded.
 *
 * `count` is the LIVE per-snapshot `layer_count` reported by the publisher's
 * audio ladder — this is the only DOM-observable signal of #1082-B (AUDIO went
 * 2 → 3 layers: 24/32/50 kbps). Note the audio *slider* labels intentionally
 * still expose 2 rungs (a product decision in `AUDIO_LAYER_LABELS`); the readout
 * `count`, by contrast, mirrors what the encoder actually emitted, so it is what
 * we assert here.
 *
 * Returns null while the readout reads "Not receiving"; otherwise
 * `{ layerIndex (0-based), layerCount, kbps }`.
 */
async function readAudioLayer(
  page: Page,
): Promise<{ layerIndex: number; layerCount: number; kbps: number } | null> {
  const text = (await page.locator("#perf-vu-recv-audio-readout").textContent())?.trim() ?? "";
  const m = text.match(/^L(\d+)\/(\d+)\s+·\s+(\d+)\s+kbps/);
  if (!m) return null;
  return {
    layerIndex: Number(m[1]) - 1,
    layerCount: Number(m[2]),
    kbps: Number(m[3]),
  };
}

/**
 * The supported AUDIO ladder length after #1082-B (24/32/50 kbps). The readout's
 * reported `layer_count` must never exceed this — a higher value would mean the
 * publisher/receiver ladders silently diverged from the documented #1082 ladder.
 */
const AUDIO_MAX_SUPPORTED_LAYERS = 3;

/** Per-rung AUDIO bitrates from #1082-B, lowest layer first (kbps). */
const AUDIO_LADDER_KBPS = [24, 32, 50] as const;

// ---------------------------------------------------------------------------
// Suite
// ---------------------------------------------------------------------------

test.describe("Per-receiver simulcast (flag-on)", () => {
  // Two real browser contexts (publisher + receiver) drive several specs; the
  // peer-discovery + layer-adaptation waits make these slower than a unit test.
  test.describe.configure({ timeout: 180_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  // -------------------------------------------------------------------------
  // 1. Multi-layer SEND active (flag-on) — proxy via received ladder size.
  //
  // FIXME(#1093): multi-party (2-context) — needs a renderer-crash-resilient
  // runner + a capability-override hook to force >=2 layers. In headless CI the
  // two authenticated contexts each running camera + simulcast encode/decode
  // crash the renderer ("Target page/context closed") so the 2nd context never
  // reaches the grid, AND the capability ceiling clamps the runner to 1 layer so
  // the multi-layer assertion would skip anyway.
  // -------------------------------------------------------------------------
  test.fixme("publisher emits >1 simulcast layer when the flag is on", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_send_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub@videocall.rs",
        "SimPublisher",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx@videocall.rs",
        "SimReceiver",
        uiURL,
      );
      // Flag ON for BOTH ends: the publisher must encode multiple layers, and
      // the receiver must be allowed to climb above the base layer.
      await enableSimulcastFlag(pubCtx, 3);
      await enableSimulcastFlag(rxCtx, 3);

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimPublisher");
      await joinMeeting(rxPage, meetingId, "SimReceiver");

      // Each side should see the other's tile (peers connected).
      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // The receiver's Performance panel exposes the received-layer ladder.
      await openPerformancePanel(rxPage);

      // Poll until the receiver is actually decoding the publisher's video
      // (readout leaves the "Not receiving" placeholder).
      let snapshot: { layerIndex: number; layerCount: number } | null = null;
      await expect
        .poll(
          async () => {
            snapshot = await readVideoLayer(rxPage);
            return snapshot !== null;
          },
          { timeout: 45_000, intervals: [500, 1000, 2000] },
        )
        .toBe(true);

      const layerCount = snapshot!.layerCount;
      // CAPABILITY CEILING: a weak/containerized CI runner whose CPU benchmark
      // scores < 5000 clamps the publisher to 1 layer regardless of the flag.
      // That is not a feature failure — skip rather than assert a false neg.
      test.skip(
        layerCount <= 1,
        `runner capability ceiling clamped the publisher to ${layerCount} layer(s); ` +
          "multi-layer send cannot be exercised on this runner (see helpers/simulcast-config.ts)",
      );

      // Flag-on success signal: the publisher is producing a >1-layer ladder
      // and the receiver sees it. (Layer emission isn't directly observable
      // from the client DOM; the received-ladder size is the closest proxy.)
      expect(layerCount).toBeGreaterThan(1);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 3. Receive-threshold enforcement (the user's key requirement).
  //    Drag the video max thumb to the lowest layer with a HEALTHY downlink
  //    and assert the needle never exceeds that threshold.
  //
  // FIXME(#1093): multi-party (2-context) — needs a renderer-crash-resilient
  // runner + a capability-override hook to force >=2 layers. Headless CI crashes
  // the 2nd context ("Target page/context closed") and clamps to 1 layer.
  // -------------------------------------------------------------------------
  test.fixme("receive needle never exceeds the user's max-layer threshold", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_thresh_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub2@videocall.rs",
        "SimPublisher2",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx2@videocall.rs",
        "SimReceiver2",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3);
      await enableSimulcastFlag(rxCtx, 3);

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimPublisher2");
      await joinMeeting(rxPage, meetingId, "SimReceiver2");

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      await openPerformancePanel(rxPage);

      // Wait until the receiver is decoding video so the ladder is known.
      await expect
        .poll(async () => (await readVideoLayer(rxPage)) !== null, {
          timeout: 45_000,
          intervals: [500, 1000, 2000],
        })
        .toBe(true);

      const before = await readVideoLayer(rxPage);
      const layerCount = before!.layerCount;
      test.skip(
        layerCount <= 1,
        `single-layer ladder (count=${layerCount}); the threshold has no headroom ` +
          "to clamp on this runner (capability ceiling). See helpers/simulcast-config.ts",
      );

      // Turn Auto OFF (so the manual range applies) then drag the max thumb to
      // the lowest layer (index 0 = "360p"). The slider is an <input type=range>
      // with min=0 / max=top; setting value to 0 pins max → layer 0.
      const autoToggle = rxPage.locator('[data-testid="perf-recv-video-auto"]');
      // The toggle reports state via aria-pressed; only click it off if on.
      const autoPressed = await autoToggle.getAttribute("aria-pressed");
      if (autoPressed === "true") {
        await autoToggle.click();
      }

      const maxThumb = rxPage.locator('[data-testid="perf-recv-video-range-max"]');
      await expect(maxThumb).toBeVisible({ timeout: 10_000 });
      // Set the range input to its lowest position and fire input so Dioxus's
      // oninput handler persists the new bound.
      await maxThumb.focus();
      await maxThumb.fill("0");
      await maxThumb.dispatchEvent("input");
      await expect(maxThumb).toHaveValue("0");

      // Auto-retrying: within the adaptation window the needle must drop to the
      // base layer and NEVER exceed it. We sample repeatedly to catch any
      // transient overshoot — the app must not request above the threshold.
      await expect
        .poll(
          async () => {
            const s = await readVideoLayer(rxPage);
            // While clamping, the readout may briefly read "Not receiving";
            // treat that as within-bound (index 0).
            return s === null ? 0 : s.layerIndex;
          },
          { timeout: 30_000, intervals: [500, 1000, 1500] },
        )
        .toBeLessThanOrEqual(0);

      // Hold the assertion over several more samples to prove it never climbs
      // back above the threshold even with a healthy local downlink.
      for (let i = 0; i < 6; i++) {
        const s = await readVideoLayer(rxPage);
        const idx = s === null ? 0 : s.layerIndex;
        expect(
          idx,
          `received layer must stay <= max threshold (0); sample ${i}`,
        ).toBeLessThanOrEqual(0);
        await rxPage.waitForTimeout(1000);
      }
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 4. Default Auto — with no threshold set the panel shows Auto (full range)
  //    and the needle is free to reflect auto-selection across the full ladder.
  //
  // FIXME(#1093): multi-party (2-context) — needs a renderer-crash-resilient
  // runner + a capability-override hook to force >=2 layers. Headless CI crashes
  // the 2nd context ("Target page/context closed") and clamps to 1 layer.
  // -------------------------------------------------------------------------
  test.fixme("default receive preference is Auto (full range)", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_auto_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub3@videocall.rs",
        "SimPublisher3",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx3@videocall.rs",
        "SimReceiver3",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3);
      await enableSimulcastFlag(rxCtx, 3);

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimPublisher3");
      await joinMeeting(rxPage, meetingId, "SimReceiver3");

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      const panel = await openPerformancePanel(rxPage);

      // Default state: video Auto is ON (aria-pressed="true"), and both thumbs
      // sit at the extremes (min=0, max=top) = full range = Auto.
      const autoToggle = rxPage.locator('[data-testid="perf-recv-video-auto"]');
      await expect(autoToggle).toHaveAttribute("aria-pressed", "true");

      const minThumb = rxPage.locator('[data-testid="perf-recv-video-range-min"]');
      const maxThumb = rxPage.locator('[data-testid="perf-recv-video-range-max"]');
      await expect(minThumb).toHaveValue("0");
      // The max thumb sits at the top index (full range). The exact top value is
      // the ladder size minus one; assert it is non-zero (range spans up).
      const topValue = await maxThumb.getAttribute("max");
      await expect(maxThumb).toHaveValue(String(topValue));

      // The needle gauge is present and the readout reflects auto-selection
      // (either actively decoding "L../.." or "Not receiving" before first
      // frame). It must NOT be artificially clamped — full range is in effect.
      await expect(panel.locator("#perf-vu-recv-video-readout")).toBeVisible();
      await expect
        .poll(
          async () => (await rxPage.locator("#perf-vu-recv-video-readout").textContent())?.trim(),
          {
            timeout: 45_000,
            intervals: [500, 1000, 2000],
          },
        )
        .toMatch(/^(L\d+\/\d+|Not receiving)/);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 5. Performance panel renders ALL THREE received-quality controls (#1082).
  //    #1082 keeps video + content at 3 layers and brings AUDIO to 3 layers
  //    (24/32/50 kbps). The receive Performance panel must expose a needle gauge
  //    + Auto toggle + range slider for every kind — video, audio, AND content —
  //    so a user can independently bound each. This is a pure structural
  //    assertion (no capability ceiling dependency): the controls are always
  //    rendered regardless of how many layers the runner ends up emitting.
  //
  // FIXME(#1093): multi-party (2-context) — although the assertion itself is
  // structural (capability-independent), it still requires the publisher +
  // receiver 2-context join, which crashes the 2nd renderer in headless CI
  // ("Target page/context closed"). Needs a renderer-crash-resilient runner (a
  // capability-override hook is not strictly required for this one, but the join
  // is). The single-context structural coverage of the receive panel lives in
  // performance-settings.spec.ts (#1078 Receive-side controls).
  // -------------------------------------------------------------------------
  test.fixme("receive Performance panel renders video + audio + content controls", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_panel_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub5@videocall.rs",
        "SimPublisher5",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx5@videocall.rs",
        "SimReceiver5",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3);
      await enableSimulcastFlag(rxCtx, 3);

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimPublisher5");
      await joinMeeting(rxPage, meetingId, "SimReceiver5");

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      const panel = await openPerformancePanel(rxPage);

      // Every kind must expose its full RECEIVE control set: needle gauge, Auto
      // toggle, and dual-thumb range (min + max). The unified panel (#1078) puts
      // the receive controls under the `perf-recv-*` / `perf-vu-recv-*`
      // namespace. Content === Screen kind (testid prefix `perf-recv-screen`,
      // labelled "Shared content" in the UI).
      for (const kind of ["video", "audio", "screen"] as const) {
        await expect(
          panel.locator(`[data-testid="perf-vu-recv-${kind}"]`),
          `${kind} receive needle gauge present`,
        ).toBeVisible();
        await expect(
          panel.locator(`[data-testid="perf-recv-${kind}-auto"]`),
          `${kind} receive Auto toggle present`,
        ).toBeVisible();
        await expect(
          panel.locator(`[data-testid="perf-recv-${kind}-range-min"]`),
          `${kind} receive min thumb present`,
        ).toBeAttached();
        await expect(
          panel.locator(`[data-testid="perf-recv-${kind}-range-max"]`),
          `${kind} receive max thumb present`,
        ).toBeAttached();
      }

      // The audio readout must be present and reflect a valid state: either
      // actively decoding ("L../.. kbps") or the "Not receiving" placeholder
      // before the first audio frame. (Layer-count content is asserted in the
      // dedicated audio-layering test below.)
      await expect(panel.locator("#perf-vu-recv-audio-readout")).toBeVisible();
      await expect
        .poll(
          async () => (await rxPage.locator("#perf-vu-recv-audio-readout").textContent())?.trim(),
          {
            timeout: 45_000,
            intervals: [500, 1000, 2000],
          },
        )
        .toMatch(/^(L\d+\/\d+ · \d+ kbps|Not receiving)/);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 6. AUDIO layering is active under the flag (#1082-B: audio 2 → 3 layers).
  //    The only DOM-observable signal of audio simulcast is the audio needle
  //    readout's reported `layer_count` (`L{i}/{N} · {kbps} kbps`). With the
  //    flag on and a capable runner, the publisher emits up to 3 audio layers,
  //    so the receiver's readout `N` rises above 1. As with VIDEO send, a weak
  //    CI runner's capability ceiling can clamp audio to a single layer — in
  //    that case we SKIP (a single layer is not a feature failure), but we
  //    ALWAYS assert the invariant that `N` never exceeds the documented
  //    3-rung ladder and the reported bitrate is one of {24,32,50} kbps.
  //
  // FIXME(#1093): multi-party (2-context) — needs a renderer-crash-resilient
  // runner + a capability-override hook to force >=2 layers. Headless CI crashes
  // the 2nd context ("Target page/context closed") and clamps audio to 1 layer.
  // -------------------------------------------------------------------------
  test.fixme("audio readout reflects the multi-layer ladder when the flag is on", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_audio_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub6@videocall.rs",
        "SimPublisher6",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx6@videocall.rs",
        "SimReceiver6",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3);
      await enableSimulcastFlag(rxCtx, 3);

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimPublisher6");
      await joinMeeting(rxPage, meetingId, "SimReceiver6");

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      await openPerformancePanel(rxPage);

      // Poll until the receiver is actually decoding the publisher's AUDIO
      // (readout leaves the "Not receiving" placeholder).
      let snapshot: { layerIndex: number; layerCount: number; kbps: number } | null = null;
      await expect
        .poll(
          async () => {
            snapshot = await readAudioLayer(rxPage);
            return snapshot !== null;
          },
          { timeout: 45_000, intervals: [500, 1000, 2000] },
        )
        .toBe(true);

      const { layerCount, layerIndex, kbps } = snapshot!;

      // INVARIANT (always holds, even on a single-layer runner): the audio
      // ladder reported to the receiver must never exceed the documented #1082-B
      // 3-rung ladder, the selected index must be in range, and the reported
      // bitrate must be a known rung. This catches a silent publisher/receiver
      // ladder drift regardless of the capability ceiling.
      expect(layerCount).toBeGreaterThanOrEqual(1);
      expect(layerCount).toBeLessThanOrEqual(AUDIO_MAX_SUPPORTED_LAYERS);
      expect(layerIndex).toBeGreaterThanOrEqual(0);
      expect(layerIndex).toBeLessThan(layerCount);
      expect(AUDIO_LADDER_KBPS).toContain(kbps);

      // CAPABILITY CEILING: a weak/containerized CI runner clamps the publisher
      // to a single audio layer regardless of the flag. That is not a feature
      // failure — skip the multi-layer assertion (see helpers/simulcast-config.ts).
      test.skip(
        layerCount <= 1,
        `runner capability ceiling clamped audio to ${layerCount} layer(s); ` +
          "multi-layer audio send cannot be exercised on this runner",
      );

      // Flag-on success signal for #1082-B: the publisher produced a >1-layer
      // AUDIO ladder (2 or 3 rungs) and the receiver sees it.
      expect(layerCount).toBeGreaterThan(1);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 1b. RECEIVE-side per-row diagnostics footer (PR #1101 / issue #1095).
  //
  // FIXME(#1093): multi-PEER (>= 2 publishers + 1 receiver, i.e. 3 contexts) —
  // needs a renderer-crash-resilient runner + a capability-override hook. The
  // RECEIVE diagnostics footer's DISCLOSURE only appears when the receiver is
  // decoding the SAME kind from MORE THAN ONE peer:
  //   * 0 peers → static "Not receiving" (covered single-context, green, in
  //     performance-settings.spec.ts → receive-needle/readout tests),
  //   * 1 peer  → static inline "From {peer} · L{i}/{N} · {res}" (no disclosure),
  //   * >= 2 peers → a disclosure `<button>` (`perf-recv-{kind}-diag-summary`,
  //     "{n} peers · L{lo}–L{hi}") whose detail (`perf-recv-{kind}-diag-detail`)
  //     lists the top-3 peers as `perf-recv-{kind}-diag-peer-{sessionId}` rows
  //     plus, when n > 3, a `perf-recv-{kind}-diag-more` tail ("+{n-3} more …").
  //
  // Exercising the per-peer rows + the "+N more" tail therefore requires a real
  // multi-peer simulcast meeting (>= 2 senders so the receiver has >= 2 peers for
  // a kind, and ideally >= 4 to render the "+more" tail). That is blocked on the
  // same harness gaps as every other multi-party test here (#1093): headless CI
  // crashes the extra contexts ("Target page/context closed") and the capability
  // ceiling clamps layers to 1. Documented as `test.fixme` (the single-context
  // structural receive coverage lives in performance-settings.spec.ts #1078).
  //
  // INTENDED assertions once #1093 unblocks this (sketch — left unimplemented on
  // purpose so it is a documented stub, not a runnable test):
  //   1. Join >= 2 publishers (cameras ON, flag ON) + 1 receiver into one room.
  //   2. openPerformancePanel(rxPage) (Receive direction).
  //   3. expect.poll `perf-recv-video-diag-summary` to read /\d+ peers · L/.
  //   4. Click it → `perf-recv-video-diag-detail` visible; assert a
  //      `perf-recv-video-diag-peer-{sessionId}` row exists for each visible peer
  //      (top-3), and with >= 4 publishers assert `perf-recv-video-diag-more`
  //      reads /\+\d+ more/.
  //   5. Capability-gate the per-peer LAYER assertions (rung/layer counts) on the
  //      received ladder size, mirroring the send-side single-layer skip.
  // -------------------------------------------------------------------------
  test.fixme("receive diagnostics list per-peer rows and a '+N more' tail with multiple publishers", async () => {
    // Blocked on #1093 (multi-peer harness): see the block comment above for
    // the intended multi-publisher flow and the `perf-recv-*-diag-peer-{id}` /
    // `perf-recv-*-diag-more` assertions this will perform.
  });

  // -------------------------------------------------------------------------
  // 2. Per-receiver congestion DIVERGENCE over WebSocket (issue #1080).
  //
  //    Now EXERCISED via the per-client downlink-impairment infra
  //    (`helpers/downlink-impair.ts` + the toxiproxy `impair` compose profile).
  //    One of two co-receivers has its WS downlink bandwidth-clamped, which
  //    overflows the relay's bounded per-receiver outbound channel; the relay
  //    sheds that receiver's VIDEO frames, the gaps raise its `loss_per_sec`
  //    above the chooser's step-down threshold, and ONLY that receiver drops to
  //    a lower layer. The sender and the healthy receiver share neither the
  //    proxy nor the relay channel, so they are unaffected. See the helper's
  //    header for the full verified mechanism.
  //
  //    NOTE: like the other multi-party tests it joins 3 contexts and is subject
  //    to the same headless-CI renderer-crash + capability limits — see #1093;
  //    running it needs the impair runner described below AND that resilience.
  //
  //    GATING: tagged `@impair` — EXCLUDED from the default `dioxus` suite
  //    (grepInvert in playwright.config.ts) and from bvt0/bvt1. It runs ONLY
  //    under `--project=impair`, which requires the toxiproxy proxy to be up
  //    (`make e2e-up-impair`). On the default CI Playwright run this test does
  //    not even appear. `assertProxyUp()` below fails fast with an actionable
  //    message if someone runs the impair project without the proxy.
  //
  //    SCOPE: WebSocket only — `routeDownlinkThroughProxy` pins the degraded
  //    context to WS because toxiproxy is TCP-only. The WT/QUIC equivalent
  //    stays `test.fixme` immediately below with its concrete blocker.
  //
  //    TODO(ci): this `@impair` test is NOT yet wired into a CI job. The
  //    existing CI workflows run `--project=dioxus` (full, e2e-hcl.yaml) and
  //    `--project=bvt1` (smoke, pr-check-e2e-smoke-hcl.yaml), neither of which
  //    starts the toxiproxy `impair` profile, so this test never runs in CI
  //    today. To run it in CI, add a dedicated job mirroring
  //    pr-check-e2e-smoke-hcl.yaml but: (a) bring the stack up with
  //    `COMPOSE_PROFILES=impair ... up -d` (or `make e2e-up-impair`), (b) wait
  //    for toxiproxy's control API on :8474, and (c) run
  //    `npx playwright test --project=impair`. Locally: `make e2e-impair`.
  // -------------------------------------------------------------------------
  test("congested receiver pulls a LOWER video layer than the healthy peer (WS) @impair", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_diverge_${Date.now()}`;

    // Fail fast (before launching 3 browsers) if the impair profile is not up.
    await assertProxyUp();
    // Start from a clean toxic state so a prior run's leftover toxic cannot
    // pre-degrade the "let layers climb" phase.
    await healDownlink();

    // 1 publisher + 2 receivers. Healthy receiver: normal downlink. Degraded
    // receiver: WS downlink routed through toxiproxy so we can clamp it.
    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const healthyBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const degradedBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub-d@videocall.rs",
        "SimPublisherD",
        uiURL,
      );
      const healthyCtx = await createAuthenticatedContext(
        healthyBrowser,
        "sim-healthy@videocall.rs",
        "SimHealthy",
        uiURL,
      );
      const degradedCtx = await createAuthenticatedContext(
        degradedBrowser,
        "sim-degraded@videocall.rs",
        "SimDegraded",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3);
      await enableSimulcastFlag(healthyCtx, 3);
      await enableSimulcastFlag(degradedCtx, 3);

      // Route the degraded receiver's media WebSocket through toxiproxy and pin
      // it to WS. MUST run before its first navigation (it patches /config.js).
      await routeDownlinkThroughProxy(degradedCtx);

      const pubPage = await pubCtx.newPage();
      const healthyPage = await healthyCtx.newPage();
      const degradedPage = await degradedCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimPublisherD");
      await joinMeeting(healthyPage, meetingId, "SimHealthy");
      await joinMeeting(degradedPage, meetingId, "SimDegraded");

      await openPerformancePanel(healthyPage);
      await openPerformancePanel(degradedPage);

      // PHASE 1 — let both receivers climb above the base layer on a healthy
      // (un-impaired) downlink. Capability ceiling can clamp to a single layer
      // on a weak runner; in that case there is no headroom to diverge, so SKIP
      // rather than assert a false negative (mirrors tests 1 & 6).
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage);
            if (!healthy || !degraded) return -1;
            return Math.min(healthy.layerCount, degraded.layerCount);
          },
          { timeout: 45_000, intervals: [1000, 2000, 3000] },
        )
        .toBeGreaterThan(0);

      const healthyStart = await readVideoLayer(healthyPage);
      const degradedStart = await readVideoLayer(degradedPage);
      test.skip(
        (healthyStart?.layerCount ?? 1) <= 1 || (degradedStart?.layerCount ?? 1) <= 1,
        "capability ceiling clamped the publisher to a single layer; there is no " +
          "ladder headroom to diverge on this runner (see helpers/simulcast-config.ts)",
      );

      // Let the degraded receiver actually reach a layer above base before we
      // impair it — otherwise "stepped down" is unobservable (it is already at 0).
      await expect
        .poll(async () => (await readVideoLayer(degradedPage))?.layerIndex ?? 0, {
          timeout: 30_000,
          intervals: [1000, 2000, 3000],
        })
        .toBeGreaterThan(0);

      // PHASE 2 — clamp ONLY the degraded receiver's downlink hard enough to
      // overflow the relay's 128-slot outbound channel (sheds video → loss →
      // step down). ~120 kbps is far below one HD layer's byte rate.
      await impairDownlink({ rateKb: 15 });

      // PHASE 3 — the degraded receiver's chosen layer must drop strictly BELOW
      // the healthy receiver's. The sender and the healthy receiver share
      // neither the proxy nor the relay channel, so the healthy peer stays high.
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage);
            // Degraded may briefly read "Not receiving" mid-step-down; treat
            // that as the base layer (the lowest possible — still a divergence
            // only if the healthy peer is above base).
            const degradedIdx = degraded?.layerIndex ?? 0;
            if (!healthy) return false;
            return degradedIdx < healthy.layerIndex;
          },
          { timeout: 90_000, intervals: [2000, 3000, 5000] },
        )
        .toBe(true);

      // Healthy receiver unaffected: still decoding and still ABOVE the base
      // layer (its layer was not dragged down by the other receiver's congestion).
      const healthyFinal = await readVideoLayer(healthyPage);
      expect(healthyFinal, "healthy receiver must still be decoding").not.toBeNull();
      expect(
        healthyFinal!.layerIndex,
        "healthy receiver must stay above the base layer (unaffected by peer congestion)",
      ).toBeGreaterThan(0);

      // PHASE 4 — heal the downlink and prove the degraded receiver climbs back
      // up (recovery), confirming the divergence was the impairment, not a
      // permanent failure. Climb-back is conservative (hysteresis), so allow a
      // generous window; if the runner is too slow to re-climb within it this is
      // a soft check (the core divergence above is the load-bearing assertion).
      await healDownlink();
      await expect
        .poll(async () => (await readVideoLayer(degradedPage))?.layerIndex ?? 0, {
          timeout: 90_000,
          intervals: [2000, 3000, 5000],
        })
        .toBeGreaterThan(0);
    } finally {
      // Always remove the toxic so a failure does not leave the proxy degraded
      // for a subsequent run.
      await healDownlink();
      await pubBrowser.close();
      await healthyBrowser.close();
      await degradedBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 2b. WT/QUIC per-receiver divergence — STILL BLOCKED (documented).
  //
  // The same relay-side overflow → loss → step-down mechanism applies on the
  // WebTransport path, but we cannot impair ONE WT client from this Playwright
  // harness:
  //   - WebTransport is QUIC over UDP. toxiproxy (used by the WS case above) is
  //     TCP-only, and Playwright's `newContext({ proxy })` only carries the
  //     browser's TCP/HTTP(S) traffic — neither can shape QUIC/UDP datagrams.
  //   - Per-client UDP impairment needs `tc qdisc … netem` keyed to that
  //     client's 5-tuple in an ISOLATED netns/veth. Playwright runs Chromium on
  //     the host in a SHARED netns, so a netem qdisc there degrades EVERY
  //     context (sender + both receivers), not just the degraded one.
  // When the bots-app netsim orchestrator can drive a per-client veth, this can
  // reuse the WS case's identical assertion against a UDP netem hook.
  // (Multi-party renderer-crash + capability concerns also apply here — see #1093.)
  // -------------------------------------------------------------------------
  test.fixme("congested receiver pulls a LOWER video layer than the healthy peer (WT) — needs per-client UDP netem", async () => {
    // Intentionally empty: the assertion is identical to the WS case above;
    // only the per-client WT/QUIC downlink-impairment hook is missing (see the
    // block comment for the concrete blocker). Kept as `fixme` so the gap is
    // visible in the test report rather than silently absent.
  });
});

// ---------------------------------------------------------------------------
// Flag-OFF control — single-layer no-regression guard for #1082.
//
// IMPORTANT: the runtime default of `experimentalSimulcastMaxLayers` was flipped
// from 1 → 3 (multicast ON by default). So "set no flag" no longer means OFF —
// it now means 3. To genuinely exercise the single-layer / feature-OFF path this
// test PINS the flag to 1 explicitly via `pinSimulcastMaxLayers(ctx, 1)` on both
// ends. The #1082 ladder machinery went N-generic but MUST NOT change the
// single-layer path: with the flag at 1 the publisher emits a single layer for
// every kind, byte-identical to the pre-simulcast encoders. The DOM-observable
// proof is that every received readout reports `/1` (a single-layer ladder)
// once decoding begins.
//
// FIXME(#1093): multi-party (2-context) — this control also joins a publisher +
// receiver and polls the receiver decoding the publisher's stream, so it hits
// the same headless-CI renderer crash ("Target page/context closed") as the
// flag-ON tests. It does NOT need a capability-override hook (single-layer is the
// expected outcome here), but it DOES need the renderer-crash-resilient runner
// for the 2-context join + cross-peer decode.
// ---------------------------------------------------------------------------
test.describe("Simulcast flag OFF (pinned to 1) — single-layer no-regression", () => {
  test.describe.configure({ timeout: 180_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test.fixme("flag pinned to 1 emits a single layer for video, audio, and content", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_off_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-off-pub@videocall.rs",
        "SimOffPublisher",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-off-rx@videocall.rs",
        "SimOffReceiver",
        uiURL,
      );
      // Explicitly PIN the flag to 1 (= single layer / OFF) on BOTH ends. The
      // runtime default is now 3, so omitting the flag would NOT exercise the
      // OFF path — it would emit 3 layers. Must run before the first navigation.
      await pinSimulcastMaxLayers(pubCtx, 1);
      await pinSimulcastMaxLayers(rxCtx, 1);

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimOffPublisher");
      await joinMeeting(rxPage, meetingId, "SimOffReceiver");

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      await openPerformancePanel(rxPage);

      // Wait until the receiver is decoding the publisher's VIDEO, then assert
      // the ladder is a SINGLE layer (count == 1). With the flag off the encoder
      // produces exactly one layer, so the readout must report `L1/1`.
      let video: { layerIndex: number; layerCount: number } | null = null;
      await expect
        .poll(
          async () => {
            video = await readVideoLayer(rxPage);
            return video !== null;
          },
          { timeout: 45_000, intervals: [500, 1000, 2000] },
        )
        .toBe(true);
      expect(video!.layerCount, "flag-off video must be single-layer").toBe(1);
      expect(video!.layerIndex).toBe(0);

      // AUDIO must likewise be single-layer with the flag off — the #1082-B
      // 3-rung ladder is gated behind the flag and must not leak into the
      // default path. The base rung is the lowest (24 kbps).
      let audio: { layerIndex: number; layerCount: number; kbps: number } | null = null;
      await expect
        .poll(
          async () => {
            audio = await readAudioLayer(rxPage);
            return audio !== null;
          },
          { timeout: 45_000, intervals: [500, 1000, 2000] },
        )
        .toBe(true);
      expect(audio!.layerCount, "flag-off audio must be single-layer").toBe(1);
      expect(audio!.layerIndex).toBe(0);
      expect(AUDIO_LADDER_KBPS).toContain(audio!.kbps);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });
});
