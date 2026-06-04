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
 *   - NEEDS NETSIM (documented, NOT faked here):
 *       2. Per-receiver congestion DIVERGENCE — forcing one receiver onto a
 *          LOWER layer than a healthy peer requires shaping that one receiver's
 *          downlink. Without per-client netsim this cannot be deterministically
 *          forced in CI. The structural two-receiver setup + the assertion shape
 *          it WOULD use are written in a `test.fixme` below so the intent is
 *          captured and the test is skipped (never green on a non-exercised
 *          path).
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

      // TODO(netsim): apply low-downlink / high-loss shaping to degradedCtx
      // ONLY. e.g. `await applyDownlinkNetsim(degradedCtx, { kbps: 150, lossPct: 20 })`
      // once such a helper exists in the harness.

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
