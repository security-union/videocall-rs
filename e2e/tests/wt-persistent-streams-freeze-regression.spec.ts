/**
 * WebTransport persistent-streams + split-writer regression test.
 *
 * Background
 * ----------
 * In production the user observed a ~5-minute audio+video freeze on
 * WebTransport. Investigation (HCL discussion #756 / RCA doc 2026-05-12)
 * traced the symptom to TWO compounding architectural bugs:
 *
 *   Bug A (client): every reliable media packet opened a fresh QUIC
 *     uni-stream (~80 streams/sec/sender), destroying ordering at the
 *     server's `accept_uni` loop and saturating the outbound channel.
 *   Bug B (server): a single writer task drained BOTH the persistent
 *     uni-stream AND the datagram queue. When QUIC flow-control credits
 *     on the video stream drained to zero, `write_all` blocked, and the
 *     audio datagram path — which is supposed to bypass stream HOL
 *     blocking — was starved behind the parked task.
 *
 * The fix on `feat/wt-persistent-streams-phase2` replaces both halves:
 *
 *   - Client (`videocall-client/.../webtransport.rs`): per-media-type
 *     persistent length-prefix-framed uni-streams (audio / video / screen
 *     / control), collapsing ~80 streams/sec/sender into 3 long-lived
 *     streams per connection.
 *   - Server (`actix-api/src/webtransport/bridge.rs`): split per-primitive
 *     writer tasks — one for the persistent uni-stream, one for datagrams.
 *     Audio datagrams can no longer be blocked by a stalled video stream.
 *
 * Scope of this E2E test
 * ----------------------
 * This is a STRUCTURAL SMOKE TEST, not a 5-minute soak. The 5-minute
 * production symptom is the *time-to-failure* under specific load and
 * network conditions; we have neither the time budget for a 5-minute CI
 * run nor an in-tree network-impairment helper to accelerate the failure.
 * The unit tests added in this same PR (`split_writer_topology_*` in
 * `wt_chat_session.rs`) lock the architectural property; this test
 * verifies the new framing protocol works end-to-end against a real
 * server and that no obvious WT-path regressions appear under a normal
 * 2-peer audio+video session.
 *
 * What this test asserts
 * ----------------------
 *   1. Two participants can join the same meeting with transport
 *      preference forced to WebTransport (sticky=true).
 *   2. Both peers enable mic + camera and remain visible to each other
 *      via the `#grid-container .canvas-container` tile.
 *   3. After ~60s of streaming, the remote peer's video canvas is still
 *      producing distinct frames (pixel-sample at t=20s vs t=60s differs).
 *      A frozen tile (the production symptom) would yield identical
 *      pixel buffers at both samples.
 *   4. Neither peer's console emits the "Sending KEYFRAME_REQUEST"
 *      storm signature beyond a low threshold (5 per peer). A persistent
 *      stall would manifest as the peer-decode-manager repeatedly
 *      requesting keyframes to recover from missing frames.
 *   5. The WebTransport path is exercised throughout — the test fails if
 *      the client transparently falls back to WebSocket (we sniff for a
 *      "transport=websocket" or election fallback log line).
 *
 * CI deployment note
 * ------------------
 * Per PR #752 review: the `pull_request` workflow on github01 does NOT
 * include the Playwright stack. Only `push-e2e-hcl.yaml` runs Playwright
 * on push to PR-staging / hcl-main. This regression test therefore only
 * runs POST-MERGE. Reviewers cannot use it as a pre-merge gate; the
 * unit-test coverage in `wt_chat_session.rs` is the synchronous guard.
 *
 * Infrastructure gap (test currently marked fixme)
 * ------------------------------------------------
 * The Playwright E2E stack defined by `docker/docker-compose.e2e.yaml`
 * does NOT run a WebTransport server. Only `websocket-api` is in the
 * compose file; `webtransport-api` (which the full `docker-compose.yaml`
 * does provide on UDP 4433) is omitted. The dioxus-ui container sets
 * `WEBTRANSPORT_ENABLED=false` as well.
 *
 * With sticky `vc_transport_preference=webtransport`, the client picks
 * `TransportPreference::WebTransportOnly`, which forces the connection
 * to `https://127.0.0.1:4433` and explicitly empties the WebSocket URL
 * list (see `resolve_transport_config` in `dioxus-ui/src/context.rs`).
 * Against the current e2e stack that connection has nowhere to land,
 * the peer tile never appears, and every assertion below fails for
 * infrastructure reasons unrelated to the architectural property under
 * test.
 *
 * The test is therefore marked `test.fixme()` to land the regression
 * scaffolding without breaking the green push-e2e-hcl pipeline. The
 * follow-up needed to activate it:
 *
 *   1. Add `webtransport-api` to `docker/docker-compose.e2e.yaml`
 *      (mirroring its config in the full `docker-compose.yaml`) and
 *      expose UDP 4433.
 *   2. Set `WEBTRANSPORT_ENABLED=true` and a `WEBTRANSPORT_HOST`
 *      pointing at the e2e WT service on the `dioxus-ui` container.
 *   3. Ensure Playwright launches Chromium with
 *      `--origin-to-force-quic-on=127.0.0.1:4433` (already present in
 *      BROWSER_ARGS — no change needed).
 *   4. Flip this test from `test.fixme` to `test`.
 *
 * Until then, the unit tests in `wt_chat_session.rs`
 * (`split_writer_topology_*`) are the only synchronous coverage of the
 * fix. Those are sufficient as a correctness guard; the role of this
 * spec is to catch protocol-wire regressions that can only surface
 * when a real client + real server exchange real bytes.
 *
 * Known limitations even once infra is in place
 * ---------------------------------------------
 *   - We do not simulate the network conditions that triggered the
 *     5-minute production freeze (high-RTT, packet loss, multi-peer
 *     fanout). A real regression of the writer-task topology bug under
 *     localhost+2-peer conditions is unlikely to manifest as a frozen
 *     tile within 60s — but it would still surface as a degraded
 *     `Sending KEYFRAME_REQUEST` rate or a transport-fallback signal,
 *     which we DO assert.
 *   - The pixel-sample check requires the receiver-side decoder to draw
 *     onto the canvas. With `--use-fake-device-for-media-stream` Chrome
 *     produces a moving synthetic pattern, so frames at t=20s and t=60s
 *     should differ by design.
 */

import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

const TEST_DURATION_MS = 60_000;
const PIXEL_SAMPLE_EARLY_MS = 20_000;
const KEYFRAME_REQUEST_THRESHOLD = 5;

/**
 * Seed `localStorage` with sticky WebTransport so the wasm boot path
 * elects WT and only WT.
 *
 * Mirrors the storage shape that the Network-tab "Apply with sticky"
 * flow writes (see `protocol-selection.spec.ts` tests 6 and 8).
 */
async function forceWebTransportSticky(context: BrowserContext, baseURL: string): Promise<void> {
  const url = new URL(baseURL);
  await context.addInitScript(() => {
    try {
      localStorage.setItem("vc_transport_preference", "webtransport");
      localStorage.setItem("vc_transport_sticky", "true");
    } catch {
      // localStorage may be unavailable in unusual sandbox states; the
      // test will then exercise the Auto election path and the assertions
      // below will still catch any regression because at least one peer
      // typically still lands on WT.
    }
  });
  // Touch a page once to ensure storage is committed before any nav.
  const seedPage = await context.newPage();
  await seedPage.goto(url.toString(), { waitUntil: "domcontentloaded" });
  await seedPage.evaluate(() => {
    localStorage.setItem("vc_transport_preference", "webtransport");
    localStorage.setItem("vc_transport_sticky", "true");
  });
  await seedPage.close();
}

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);
}

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting" || result === "waiting-for-meeting") {
    return result;
  }
  if (result === "auto-joined") {
    return "in-meeting";
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult === "in-meeting") {
    return;
  }
  if (guestResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

    const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
    const guestGrid = guestPage.locator("#grid-container");

    const postAdmit = await Promise.race([
      guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
      guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);

    if (postAdmit === "join-button") {
      await guestPage.waitForTimeout(1000);
      await guestJoinButton.click();
      await guestPage.waitForTimeout(3000);
      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    }
  }
}

/**
 * Click the local "Unmute" control to enable the mic. The MicButton's
 * tooltip span text flips from "Unmute" to "Mute" once the mic is on.
 */
async function enableMic(page: Page): Promise<void> {
  const unmuteBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Unmute" }),
  });
  await expect(unmuteBtn).toBeVisible({ timeout: 10_000 });
  await unmuteBtn.click();
  await page.waitForTimeout(500);
}

/**
 * Click the local "Start Video" control to enable the camera. The
 * CameraButton's tooltip flips from "Start Video" to "Stop Video" once
 * the camera is on. Fake camera capture is provided by the Chrome
 * `--use-fake-device-for-media-stream` flag in BROWSER_ARGS.
 */
async function enableCamera(page: Page): Promise<void> {
  const startVideoBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Start Video" }),
  });
  if ((await startVideoBtn.count()) === 0) {
    // Camera may already be enabled by default in some builds.
    return;
  }
  await startVideoBtn.first().click();
  await page.waitForTimeout(500);
}

/**
 * Sample the first peer-video canvas inside `#grid-container` and return
 * a short pixel-checksum string. Returns `null` if no canvas-container
 * tile is yet present.
 *
 * We deliberately read a small sub-region (32x32 px from the center) so
 * the checksum runs cheaply and yet still varies frame-to-frame with the
 * Chrome synthetic camera pattern.
 */
async function samplePeerVideoChecksum(page: Page): Promise<string | null> {
  return page.evaluate(() => {
    const containers = document.querySelectorAll("#grid-container .canvas-container");
    for (const container of Array.from(containers)) {
      const canvas = container.querySelector("canvas") as HTMLCanvasElement | null;
      if (!canvas || canvas.width === 0 || canvas.height === 0) {
        continue;
      }
      const ctx = canvas.getContext("2d");
      if (!ctx) {
        continue;
      }
      const w = Math.min(32, canvas.width);
      const h = Math.min(32, canvas.height);
      const x = Math.max(0, Math.floor((canvas.width - w) / 2));
      const y = Math.max(0, Math.floor((canvas.height - h) / 2));
      try {
        const data = ctx.getImageData(x, y, w, h).data;
        // Cheap checksum: sum every 17th byte. Sufficient to detect a
        // changing image vs a frozen tile without doing a full hash.
        let sum = 0;
        for (let i = 0; i < data.length; i += 17) {
          sum = (sum + data[i]) >>> 0;
        }
        return `${canvas.width}x${canvas.height}:${sum}`;
      } catch {
        // SecurityError on tainted canvas — should not happen for our
        // own decoded media, but ignore and try the next container.
      }
    }
    return null;
  });
}

interface ConsoleStats {
  keyframeRequestLines: number;
  websocketFallbackLines: number;
  allLines: string[];
}

function attachConsoleSniffer(page: Page, label: string): ConsoleStats {
  const stats: ConsoleStats = {
    keyframeRequestLines: 0,
    websocketFallbackLines: 0,
    allLines: [],
  };
  page.on("console", (msg) => {
    const text = msg.text();
    stats.allLines.push(`[${label}] ${text}`);
    if (text.includes("Sending KEYFRAME_REQUEST")) {
      stats.keyframeRequestLines += 1;
    }
    // Heuristic for "WT failed, falling back to WS". The connection
    // manager emits a log when election picks WS after WT errored.
    if (
      text.includes("transport=websocket") ||
      text.includes("falling back to websocket") ||
      text.includes("WebTransport failed")
    ) {
      stats.websocketFallbackLines += 1;
    }
  });
  return stats;
}

test.describe("WebTransport persistent-streams + split-writer freeze regression", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // `test.fixme` because the current e2e docker-compose stack does not
  // run a WebTransport server (no `webtransport-api` service, UDP 4433
  // not bound). See "Infrastructure gap" in the file-level doc-comment
  // above for the steps needed to flip this to a live test.
  test.fixme("audio+video on WT survives a 60s 2-peer call without freeze signatures", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wt_freeze_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "wt-freeze-host@videocall.rs",
        "WtFreezeHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "wt-freeze-guest@videocall.rs",
        "WtFreezeGuest",
        uiURL,
      );

      // Force WebTransport for BOTH contexts before any page loads the wasm.
      await forceWebTransportSticky(hostCtx, uiURL);
      await forceWebTransportSticky(guestCtx, uiURL);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      const hostStats = attachConsoleSniffer(hostPage, "host");
      const guestStats = attachConsoleSniffer(guestPage, "guest");

      // ---- Both users join the meeting ----
      await navigateToMeeting(hostPage, meetingId, "WtFreezeHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "WtFreezeGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Confirm WebTransport stuck (sticky preference still in storage).
      const hostPref = await hostPage.evaluate(() => ({
        preference: localStorage.getItem("vc_transport_preference"),
        sticky: localStorage.getItem("vc_transport_sticky"),
      }));
      const guestPref = await guestPage.evaluate(() => ({
        preference: localStorage.getItem("vc_transport_preference"),
        sticky: localStorage.getItem("vc_transport_sticky"),
      }));
      expect(hostPref.preference).toBe("webtransport");
      expect(hostPref.sticky).toBe("true");
      expect(guestPref.preference).toBe("webtransport");
      expect(guestPref.sticky).toBe("true");

      // Peer tiles should appear once the WT session connects.
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- Enable audio + video on both sides ----
      // This is the precondition that distinguishes the freeze case
      // ("WT + audio + video") from the working case ("WT + video-only")
      // per the RCA table at section 1 of the doc.
      await enableMic(hostPage);
      await enableMic(guestPage);
      await enableCamera(hostPage);
      await enableCamera(guestPage);

      // Let the streams settle.
      await hostPage.waitForTimeout(5_000);

      // ---- Sample the remote video canvas early (~20s in) ----
      // We capture a checksum after the encoders have ramped up and the
      // first keyframe has been decoded on both sides.
      await hostPage.waitForTimeout(PIXEL_SAMPLE_EARLY_MS - 5_000);

      const hostEarly = await samplePeerVideoChecksum(hostPage);
      const guestEarly = await samplePeerVideoChecksum(guestPage);

      // ---- Hold the meeting open for the remainder of the window ----
      await hostPage.waitForTimeout(TEST_DURATION_MS - PIXEL_SAMPLE_EARLY_MS);

      // ---- Sample again at ~60s ----
      const hostLate = await samplePeerVideoChecksum(hostPage);
      const guestLate = await samplePeerVideoChecksum(guestPage);

      console.log(`Host pixel checksum early=${hostEarly} late=${hostLate}`);
      console.log(`Guest pixel checksum early=${guestEarly} late=${guestLate}`);
      console.log(
        `Host KEYFRAME_REQUEST count: ${hostStats.keyframeRequestLines}, ` +
          `WS-fallback hits: ${hostStats.websocketFallbackLines}`,
      );
      console.log(
        `Guest KEYFRAME_REQUEST count: ${guestStats.keyframeRequestLines}, ` +
          `WS-fallback hits: ${guestStats.websocketFallbackLines}`,
      );

      // ---- Assertion 1: peer tiles never disappeared ----
      // The grid layout demotes a tile to "audio-only" placeholder if
      // video stops; a real freeze on Chrome usually leaves the tile in
      // place but stops repainting it. We assert the tile is still in
      // the DOM regardless.
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible();
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible();

      // ---- Assertion 2: video pixels changed between samples ----
      // The Chrome synthetic camera produces a moving ball + clock; a
      // healthy decoder draws new frames so the 32x32 center patch
      // checksum must differ. Identical checksums = frozen tile.
      //
      // We allow the host OR guest sample to be null if the tile shape
      // changed (e.g. layout switch); both being null is a failure
      // because it means no canvas-container existed at sample time.
      if (hostEarly !== null && hostLate !== null) {
        expect(
          hostEarly,
          "host's remote-peer video tile produced identical pixels 40s apart " +
            "— this is the freeze signature the fix is meant to prevent",
        ).not.toBe(hostLate);
      }
      if (guestEarly !== null && guestLate !== null) {
        expect(
          guestEarly,
          "guest's remote-peer video tile produced identical pixels 40s apart " +
            "— this is the freeze signature the fix is meant to prevent",
        ).not.toBe(guestLate);
      }
      expect(hostEarly === null && guestEarly === null).toBe(false);
      expect(hostLate === null && guestLate === null).toBe(false);

      // ---- Assertion 3: no KEYFRAME_REQUEST storm ----
      // PLI storms are the upstream-issue-#814 signature of the
      // death-spiral mechanic. With the architectural fix in place
      // we should see well under 5 per peer over 60s of clean local
      // network conditions. A regression would push this to dozens.
      expect(
        hostStats.keyframeRequestLines,
        `host emitted ${hostStats.keyframeRequestLines} KEYFRAME_REQUEST log lines; ` +
          `threshold is ${KEYFRAME_REQUEST_THRESHOLD}. A storm here means the ` +
          `peer-decode-manager is repeatedly recovering from missing frames — ` +
          `the freeze regression is back.`,
      ).toBeLessThan(KEYFRAME_REQUEST_THRESHOLD);
      expect(
        guestStats.keyframeRequestLines,
        `guest emitted ${guestStats.keyframeRequestLines} KEYFRAME_REQUEST log lines; ` +
          `threshold is ${KEYFRAME_REQUEST_THRESHOLD}.`,
      ).toBeLessThan(KEYFRAME_REQUEST_THRESHOLD);

      // ---- Assertion 4: did not silently fall back to WebSocket ----
      // The sticky-WT preference should hold throughout. If the wasm
      // transport layer panicked or the election logic picked WS, the
      // freeze symptom would not reproduce and a green test would
      // give a false sense of security.
      expect(
        hostStats.websocketFallbackLines,
        "host appears to have fallen back to WebSocket — the WT path was not exercised",
      ).toBe(0);
      expect(
        guestStats.websocketFallbackLines,
        "guest appears to have fallen back to WebSocket — the WT path was not exercised",
      ).toBe(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
