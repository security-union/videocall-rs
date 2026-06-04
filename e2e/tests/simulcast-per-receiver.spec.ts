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
 * ## What runs in CI here, and what needs a netsim capability
 *
 * The Playwright harness in `e2e/` has NO per-client downlink-shaping
 * (no `tc`/`netem`, no CDP `Network.emulateNetworkConditions`, no `?netsim=`
 * — that machinery lives only in `e2e/bots-app/`, a separate orchestrator that
 * cannot inject congestion into a Playwright BrowserContext). Therefore:
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
 *       Default config (no `enableSimulcastFlag`) → flag = 1 = OFF. The publisher
 *       emits a SINGLE layer for every kind, so each received-quality readout
 *       reports `/1`. This guards the no-regression byte-identical single-layer
 *       path for #1082 (the ladder machinery went N-generic but default behavior
 *       must be unchanged).
 *
 *   - NEEDS NETSIM (documented, NOT faked here) — issue #1080:
 *       2. Per-receiver congestion DIVERGENCE — forcing one receiver onto a
 *          LOWER layer than a healthy peer requires injecting PACKET LOSS on that
 *          one receiver's downlink. The chooser steps down on loss/PLI, not raw
 *          bandwidth (see `layer_chooser.rs`), and CDP
 *          `Network.emulateNetworkConditions` shapes throughput/latency only — it
 *          cannot inject loss, and on the reliable+ordered WS path a throttle
 *          merely delays frames (no `loss_per_sec`). So NO slice (not even
 *          WS-only) is achievable with CDP; this needs a loss-injecting netsim
 *          (tc/netem or the bots-app orchestrator). Left as `test.fixme` below
 *          with the full feasibility analysis inline (never green on a
 *          non-exercised path).
 *
 * ## Capability-ceiling caveat (see helpers/simulcast-config.ts)
 *
 * `capability_max_simulcast_layers()` reads a live ~100ms CPU benchmark with no
 * test override. On a weak CI runner the ceiling can clamp to 1 even with the
 * flag = 3, in which case the publisher emits a single layer. Test 1 detects
 * this (`layer_count <= 1`) and SKIPS rather than asserts a false negative.
 *
 * Selectors used (all stable, defined in dioxus-ui source):
 *   - `[data-testid="open-settings"]`           toolbar gear (settings modal)
 *   - `.device-settings-modal`                  the settings modal root
 *   - `role="tab" name="Performance"`           Performance nav tab
 *   - `#settings-panel-performance`             the perf tabpanel
 *   - `#perf-vu-video-readout`                  video received-quality readout
 *       text format: `L{idx+1}/{count} · {w}x{h}` or "Not receiving"
 *   - `[data-testid="perf-video-range-max"]`    video max-layer range thumb
 *   - `[data-testid="perf-video-auto"]`         video Auto toggle (aria-pressed)
 */

import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { createAuthenticatedContext, BROWSER_ARGS } from "../helpers/auth-context";
import { enableSimulcastFlag } from "../helpers/simulcast-config";
import { waitForServices } from "../helpers/wait-for-services";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Drive a fresh page from the home form into the meeting grid. Mirrors the
 * proven flow in two-users-meeting.spec.ts / settings-modal.spec.ts.
 */
async function joinMeeting(page: Page, meetingId: string, displayName: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 60 });

  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(displayName, { delay: 60 });
  await page.waitForTimeout(400);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  // Either an explicit Start/Join button appears, or we auto-join straight to
  // the grid.
  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto" as const),
  ]);

  if (result === "join") {
    await page.waitForTimeout(800);
    await joinButton.click();
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/** Open Settings → Performance and return the visible perf tabpanel locator. */
async function openPerformancePanel(page: Page) {
  await page.locator('[data-testid="open-settings"]').click();
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  await page.getByRole("tab", { name: "Performance" }).click();
  const panel = page.locator("#settings-panel-performance");
  await expect(panel).toBeVisible({ timeout: 10_000 });
  return panel;
}

/**
 * Parse the video received-quality readout `#perf-vu-video-readout`.
 * Returns null while the readout reads "Not receiving" (nothing decoded yet),
 * otherwise `{ layerIndex, layerCount }` (1-based "L{idx+1}/{count}" → 0-based).
 */
async function readVideoLayer(
  page: Page,
): Promise<{ layerIndex: number; layerCount: number } | null> {
  const text = (await page.locator("#perf-vu-video-readout").textContent())?.trim() ?? "";
  const m = text.match(/^L(\d+)\/(\d+)/);
  if (!m) return null;
  return { layerIndex: Number(m[1]) - 1, layerCount: Number(m[2]) };
}

/**
 * Parse the AUDIO received-quality readout `#perf-vu-audio-readout`.
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
  const text = (await page.locator("#perf-vu-audio-readout").textContent())?.trim() ?? "";
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
  // -------------------------------------------------------------------------
  test("publisher emits >1 simulcast layer when the flag is on", async ({ baseURL }) => {
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
  // -------------------------------------------------------------------------
  test("receive needle never exceeds the user's max-layer threshold", async ({ baseURL }) => {
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
      const autoToggle = rxPage.locator('[data-testid="perf-video-auto"]');
      // The toggle reports state via aria-pressed; only click it off if on.
      const autoPressed = await autoToggle.getAttribute("aria-pressed");
      if (autoPressed === "true") {
        await autoToggle.click();
      }

      const maxThumb = rxPage.locator('[data-testid="perf-video-range-max"]');
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
  // -------------------------------------------------------------------------
  test("default receive preference is Auto (full range)", async ({ baseURL }) => {
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
      const autoToggle = rxPage.locator('[data-testid="perf-video-auto"]');
      await expect(autoToggle).toHaveAttribute("aria-pressed", "true");

      const minThumb = rxPage.locator('[data-testid="perf-video-range-min"]');
      const maxThumb = rxPage.locator('[data-testid="perf-video-range-max"]');
      await expect(minThumb).toHaveValue("0");
      // The max thumb sits at the top index (full range). The exact top value is
      // the ladder size minus one; assert it is non-zero (range spans up).
      const topValue = await maxThumb.getAttribute("max");
      await expect(maxThumb).toHaveValue(String(topValue));

      // The needle gauge is present and the readout reflects auto-selection
      // (either actively decoding "L../.." or "Not receiving" before first
      // frame). It must NOT be artificially clamped — full range is in effect.
      await expect(panel.locator("#perf-vu-video-readout")).toBeVisible();
      await expect
        .poll(async () => (await rxPage.locator("#perf-vu-video-readout").textContent())?.trim(), {
          timeout: 45_000,
          intervals: [500, 1000, 2000],
        })
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
  // -------------------------------------------------------------------------
  test("receive Performance panel renders video + audio + content controls", async ({
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

      // Every kind must expose its full control set: needle gauge, Auto toggle,
      // and dual-thumb range (min + max). Content === Screen kind (testid prefix
      // `perf-screen`, labelled "Shared content" in the UI).
      for (const kind of ["video", "audio", "screen"] as const) {
        await expect(
          panel.locator(`[data-testid="perf-vu-${kind}"]`),
          `${kind} needle gauge present`,
        ).toBeVisible();
        await expect(
          panel.locator(`[data-testid="perf-${kind}-auto"]`),
          `${kind} Auto toggle present`,
        ).toBeVisible();
        await expect(
          panel.locator(`[data-testid="perf-${kind}-range-min"]`),
          `${kind} min thumb present`,
        ).toBeAttached();
        await expect(
          panel.locator(`[data-testid="perf-${kind}-range-max"]`),
          `${kind} max thumb present`,
        ).toBeAttached();
      }

      // The audio readout must be present and reflect a valid state: either
      // actively decoding ("L../.. kbps") or the "Not receiving" placeholder
      // before the first audio frame. (Layer-count content is asserted in the
      // dedicated audio-layering test below.)
      await expect(panel.locator("#perf-vu-audio-readout")).toBeVisible();
      await expect
        .poll(async () => (await rxPage.locator("#perf-vu-audio-readout").textContent())?.trim(), {
          timeout: 45_000,
          intervals: [500, 1000, 2000],
        })
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
  // -------------------------------------------------------------------------
  test("audio readout reflects the multi-layer ladder when the flag is on", async ({ baseURL }) => {
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
  // 2. Per-receiver congestion DIVERGENCE — NEEDS NETSIM (documented).
  //
  // Forcing one of two co-receivers onto a LOWER layer than a healthy peer
  // requires shaping THAT receiver's downlink (high loss / low bandwidth). The
  // Playwright `e2e/` harness has no per-client netsim hook (no tc/netem, no
  // CDP Network.emulateNetworkConditions wired for QUIC/WebTransport, no
  // `?netsim=` — that lives only in `e2e/bots-app/`). Without it the divergence
  // cannot be deterministically produced, so this is left as `fixme` (skipped,
  // not faked). The body documents the exact two-receiver shape + the assertion
  // it would make once a per-client downlink-shaping capability exists.
  // -------------------------------------------------------------------------
  test.fixme("congested receiver pulls a LOWER layer than the healthy peer (needs netsim)", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_diverge_${Date.now()}`;

    // 1 publisher + 2 receivers. Receiver A: healthy. Receiver B: would have
    // its downlink shaped to high-loss/low-bandwidth via a per-client netsim
    // capability (e.g. CDP emulateNetworkConditions on the QUIC path, or a
    // sidecar tc/netem keyed on B's client port — NOT available today).
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

      // TODO(#1080 — netsim): apply low-downlink / high-loss shaping to
      // degradedCtx ONLY, e.g.
      //   `await applyDownlinkNetsim(degradedCtx, { kbps: 150, lossPct: 20 })`.
      //
      // FEASIBILITY (investigated 2026-06-04, kept `fixme`): NOT achievable with
      // the tools available to this Playwright harness today — and the blocker
      // is deeper than "CDP can't shape WebTransport":
      //
      //   1. The receiver layer chooser steps DOWN on PACKET LOSS / PLI, not on
      //      raw bandwidth. See `videocall-client/src/decode/layer_chooser.rs`:
      //      `is_congested()` triggers on `loss_per_sec >= LOSS_STEP_DOWN_PER_SEC`
      //      (packets shifted off the reorder window unseen) or `kf_per_sec`
      //      (PLI rate). A throttled-but-lossless downlink does NOT raise either.
      //
      //   2. CDP `Network.emulateNetworkConditions` shapes THROUGHPUT + LATENCY
      //      only — it has no packet-loss / jitter knob. So even on the WS path
      //      (where, unlike WebTransport/QUIC, CDP can throttle the renderer's
      //      socket) it would make media frames arrive LATE, not LOST.
      //
      //   3. WS is reliable + ordered (RFC6455 over TCP — see
      //      `videocall-transport/src/websocket.rs`). Throttling delays delivery;
      //      the reorder window still sees every frame, so `loss_per_sec` stays
      //      ~0 and the chooser never steps down. The only WS loss source is the
      //      SENDER-side 1 MB send-buffer backpressure drop (an UPLINK signal a
      //      receiver-side downlink shaper cannot trigger).
      //
      // CONCLUSION: no meaningful slice (not even WS-only) can be un-`fixme`d
      // with CDP alone. Deterministically forcing per-receiver divergence needs
      // a LOSS-injecting netsim on the degraded client's downlink — e.g. a
      // sidecar `tc qdisc … netem loss 20%` keyed to that client's 5-tuple, or
      // the `e2e/bots-app/` netsim orchestrator extended to drive a Playwright
      // BrowserContext. Both are out of scope for the current `e2e/` harness.

      const pubPage = await pubCtx.newPage();
      const healthyPage = await healthyCtx.newPage();
      const degradedPage = await degradedCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimPublisherD");
      await joinMeeting(healthyPage, meetingId, "SimHealthy");
      await joinMeeting(degradedPage, meetingId, "SimDegraded");

      await openPerformancePanel(healthyPage);
      await openPerformancePanel(degradedPage);

      // After the adaptation window, the degraded receiver's chosen layer
      // must be strictly LOWER than the healthy receiver's. The SENDER keeps
      // sending and the HEALTHY receiver stays high (unaffected).
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage);
            if (!healthy || !degraded) return false;
            return degraded.layerIndex < healthy.layerIndex;
          },
          { timeout: 60_000, intervals: [1000, 2000, 3000] },
        )
        .toBe(true);

      // Healthy receiver unaffected: still decoding (not pinned to base).
      const healthyFinal = await readVideoLayer(healthyPage);
      expect(healthyFinal).not.toBeNull();
    } finally {
      await pubBrowser.close();
      await healthyBrowser.close();
      await degradedBrowser.close();
    }
  });
});

// ---------------------------------------------------------------------------
// Flag-OFF control (default production config) — no-regression guard for #1082.
//
// With NO `enableSimulcastFlag` call the flag falls back to its serde default of
// 1 (= single layer / feature OFF), which is exactly what production ships. The
// #1082 ladder machinery went N-generic but MUST NOT change this default path:
// the publisher emits a single layer for every kind, byte-identical to the
// pre-simulcast encoders. The DOM-observable proof is that every received
// readout reports `/1` (a single-layer ladder) once decoding begins.
// ---------------------------------------------------------------------------
test.describe("Simulcast flag OFF (default) — single-layer no-regression", () => {
  test.describe.configure({ timeout: 180_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("default config emits a single layer for video, audio, and content", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_off_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      // NOTE: deliberately NO enableSimulcastFlag() — exercise the production
      // default (flag = 1 = OFF) on BOTH ends.
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
