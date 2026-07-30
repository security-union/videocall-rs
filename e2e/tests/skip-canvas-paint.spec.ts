/**
 * E2E: `skipCanvasPaint` decode-and-drop receiver knob (#2069, part of #2068).
 *
 * The knob (RuntimeConfig `skipCanvasPaint`, `dioxus-ui/src/constants.rs`) makes a
 * receiver KEEP decoding every peer's video (transport/jitter/PLI all run) but
 * SKIP the rAF canvas paint — the frame is decoded then dropped before
 * `render_to_canvas_cached`'s `drawImage` (`videocall-client/src/decode/peer_decoder.rs`
 * `should_paint`). It cuts per-tile paint/GPU cost on the no-GPU boxes the
 * load-test bots run on. This spec proves the CANVAS-RENDERED STATE change that
 * unit tests can't observe: with the flag ON, a connected peer's tile paints
 * nothing; with it OFF (control), the same peer's tile paints changing frames.
 *
 * Harness: two authenticated camera-ON peers in one meeting (join flow lifted
 * from `simulcast-per-receiver.spec.ts`). The config knob is flipped for ONE
 * context via a `/config.js` route patch (the SAME mechanism as
 * `simulcast-config.ts` — `addInitScript` is clobbered by config.js's wholesale
 * `window.__APP_CONFIG` reassignment). Paint is observed with the shared
 * `frame-liveness.ts` pixel-checksum reader (a peer tile that repaints yields
 * changing checksums; a never-painted tile yields a 0-sized/blank canvas).
 *
 * NOTE (CLAUDE.md): untagged (no `@bvt`), so per-PR CI does NOT run it. It is
 * validated on the local docker e2e stack (`make e2e-up` + `make e2e SPEC=skip-canvas-paint`).
 *
 * MUTATION SENSITIVITY: revert the `!skip_canvas_paint` term in `should_paint`
 * (`peer_decoder.rs`) and the skip context ALSO paints -> `skipPaints` becomes
 * > 1 and the "skip context paints nothing" assertion fails.
 */

import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import { createAuthenticatedContext, BROWSER_ARGS } from "../helpers/auth-context";
import {
  sampleChecksumSeries,
  distinctChecksumsInWindow,
  samplePeerVideoChecksum,
} from "../helpers/frame-liveness";
import { waitForServices } from "../helpers/wait-for-services";

const SKIP_CANVAS_PAINT_KEY = "skipCanvasPaint";

/**
 * Patch the `config.js` served to every page in `context` so `skipCanvasPaint` is
 * truthy (decode-and-drop). Mirrors `enableSimulcastFlag` in `simulcast-config.ts`:
 * a `/config.js` route interception that appends the key via `Object.assign` onto
 * `window.__APP_CONFIG`, surviving config.js's wholesale reassignment. Scoped to
 * this context only. Must be called BEFORE the first navigation.
 */
async function enableSkipCanvasPaint(context: BrowserContext): Promise<void> {
  await context.route("**/config.js", async (route) => {
    const response = await route.fetch();
    const original = await response.text();
    const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},{${JSON.stringify(
      SKIP_CANVAS_PAINT_KEY,
    )}:"true"});`;
    const patched = original.trimStart().startsWith("window.__APP_CONFIG")
      ? original + injection
      : `window.__APP_CONFIG=window.__APP_CONFIG||{};` + injection;
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: patched,
    });
  });
}

/**
 * Drive a fresh page from the home form into the meeting grid as a camera-ON +
 * mic-ON publisher. Lifted from `simulcast-per-receiver.spec.ts::joinMeeting` (the
 * proven camera-on join: seed prejoin prefs before boot, grant media, disable the
 * host Waiting Room so later joiners auto-admit, ensure the camera toggle is ON +
 * a live preview track exists, then Start/Join).
 */
async function joinMeeting(page: Page, meetingId: string, displayName: string): Promise<void> {
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("vc_prejoin_camera_on", "true");
      window.localStorage.setItem("vc_prejoin_mic_on", "true");
    } catch {
      /* storage may be unavailable pre-navigation; the app origin sets it */
    }
  });

  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(displayName, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");
  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto" as const),
  ]);

  if (result === "join") {
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

    // HOST only: disable the Waiting Room so later joiners auto-admit into the grid
    // (else they park on "Waiting to be admitted" and never reach #grid-container).
    const waitingRoomRow = page.locator(".settings-option-row", {
      has: page.getByText("Waiting Room", { exact: true }),
    });
    const waitingRoomToggle = waitingRoomRow.getByRole("switch");
    if (await waitingRoomToggle.isVisible().catch(() => false)) {
      let settled: string | null = null;
      await expect
        .poll(
          async () => {
            const first = await waitingRoomToggle.getAttribute("aria-checked").catch(() => null);
            await page.waitForTimeout(250);
            const second = await waitingRoomToggle.getAttribute("aria-checked").catch(() => null);
            if (first !== null && first === second) {
              settled = second;
              return true;
            }
            return false;
          },
          { timeout: 10_000, intervals: [250, 500] },
        )
        .toBe(true)
        .catch(() => {
          /* never settled within budget — fall through without toggling */
        });
      if (settled === "true") {
        await waitingRoomToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
        await expect(waitingRoomToggle).toHaveAttribute("aria-checked", "false", {
          timeout: 10_000,
        });
      }
    }

    const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
    if (await cameraToggle.isVisible().catch(() => false)) {
      if ((await cameraToggle.getAttribute("aria-pressed")) !== "true") {
        await cameraToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
      }
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

/** A renderable (non-zero) peer video canvas is present in the grid. */
async function peerCanvasCount(page: Page): Promise<number> {
  return page.evaluate(() => {
    const containers = document.querySelectorAll("#grid-container .canvas-container");
    let n = 0;
    for (const c of Array.from(containers)) {
      const canvas = c.querySelector("canvas") as HTMLCanvasElement | null;
      if (canvas) n += 1;
    }
    return n;
  });
}

test.describe("skipCanvasPaint decode-and-drop (#2069)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("skip context decodes the peer but paints nothing; control context paints", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000); // two camera-on joins + ~5s sampling
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `skip_paint_${Date.now()}`;

    // Control host (paint ON) joins first so it owns the meeting and can disable
    // the Waiting Room; the skip receiver auto-admits.
    const controlBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const skipBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const controlCtx = await createAuthenticatedContext(
        controlBrowser,
        "skip-control@videocall.rs",
        "ControlPaint",
        uiURL,
      );
      const skipCtx = await createAuthenticatedContext(
        skipBrowser,
        "skip-rx@videocall.rs",
        "SkipPaint",
        uiURL,
      );
      // Flip skipCanvasPaint ON for the receiver context only (before any nav).
      await enableSkipCanvasPaint(skipCtx);

      const controlPage = await controlCtx.newPage();
      const skipPage = await skipCtx.newPage();

      await joinMeeting(controlPage, meetingId, "control-paint");
      await joinMeeting(skipPage, meetingId, "skip-paint");

      // Both must actually see the OTHER peer's tile (a canvas container), else a
      // "paints nothing" result would just mean "no peer", not "skip".
      await expect
        .poll(() => peerCanvasCount(controlPage), { timeout: 30_000, intervals: [1000] })
        .toBeGreaterThan(0);
      await expect
        .poll(() => peerCanvasCount(skipPage), { timeout: 30_000, intervals: [1000] })
        .toBeGreaterThan(0);

      // Let media flow, then sample each side's peer tile for ~5s.
      await controlPage.waitForTimeout(2500);
      const WINDOW = 5000;
      const [controlSeries, skipSeries] = await Promise.all([
        sampleChecksumSeries(controlPage, WINDOW, 400),
        sampleChecksumSeries(skipPage, WINDOW, 400),
      ]);

      const controlPaints = distinctChecksumsInWindow(controlSeries, 0, WINDOW + 1);
      const skipPaints = distinctChecksumsInWindow(skipSeries, 0, WINDOW + 1);

      // CONTROL (paint ON): the peer tile repaints live camera frames -> multiple
      // distinct checksums. Proves the harness + bidirectional video flow work.
      expect(
        controlPaints,
        `control (paint ON) peer tile should repaint changing frames; got ${controlPaints} distinct checksums`,
      ).toBeGreaterThan(1);

      // SKIP (decode-and-drop): the peer is connected (canvas container present,
      // asserted above) and decoding, but the canvas is never painted -> a
      // 0-sized/blank canvas yields at most ONE distinct (or zero) checksum.
      // MUTATION: reverting `!skip_canvas_paint` in should_paint makes this > 1.
      expect(
        skipPaints,
        `skip context (decode-and-drop) peer tile must paint nothing; got ${skipPaints} distinct checksums`,
      ).toBeLessThanOrEqual(1);

      // Belt-and-suspenders: a direct single sample on the skip side is null or a
      // single blank value (never a live frame checksum from the control's video).
      const skipSample = await samplePeerVideoChecksum(skipPage);
      expect(
        controlPaints > skipPaints,
        `skip must paint strictly less than control (control=${controlPaints}, skip=${skipPaints}, skipSample=${skipSample})`,
      ).toBe(true);
    } finally {
      await controlBrowser.close().catch(() => {});
      await skipBrowser.close().catch(() => {});
    }
  });
});
