/**
 * Issue #2728 — a main-thread stall must not starve the WebTransport receive path.
 *
 * What is being measured, and why not the metric the issue names
 * ---------------------------------------------------------------
 * The issue's acceptance criterion is `wt_datagram_read_loop_max_gap_ms < 100`.
 * That metric cannot answer the question on a #2724 relay: audio moved to the
 * receiver-scoped class-3 reliable stream, so the datagram loop now carries only
 * the #2721 RTT probes and sub-MTU control, and its cadence says nothing about
 * reader starvation. It also reaches only the health packet, and the e2e stack
 * runs no `metrics_server` to collect one.
 *
 * The load-bearing signal is the audio lane: the worst gap between successive
 * `.read()` resolutions on that stream. At about 50 packets/s per speaker a
 * healthy gap is tens of milliseconds, so a 3 s stall reads about 3000 ms.
 * `window.__videocall_wt_receive_stats()` reports the production tracker
 * (`videocall-transport/src/inbound.rs`), not a recomputation, and every read is
 * non-draining so it cannot steal a reporting window from the health tick.
 *
 * Why the stall is a real busy-wait
 * ---------------------------------
 * `window.__videocall_inject_longtask(ms)` pushes a SYNTHETIC sample onto the
 * diagnostics bus and stalls nothing, so a test built on it would pass on the
 * un-fixed code. The stall below is a synchronous `while` loop inside
 * `page.evaluate`, which blocks the renderer's main thread for real.
 *
 * Why there are two tests
 * -----------------------
 * The tracker is new, so "it reports a small gap" proves nothing on its own.
 * The second test disables the Worker with `window.__VC_WT_RECEIVE_WORKER = "0"`
 * — the production rollback lever — and asserts the OPPOSITE outcome on the very
 * same build. The two assertions point in opposite directions, so no single
 * mistake can make both pass.
 *
 * Not claimed here: "audio-datagram loss stays at 0". Against a #2724 relay
 * there is no audio on datagrams to lose, so that half of the issue's criterion
 * passes vacuously and is deliberately not asserted.
 */

import { test, expect, chromium, request, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

/** Long enough to dwarf a healthy audio-lane gap, short of #2726's ~10 s close. */
const STALL_MS = 3_000;
/** A healthy class-3 reader resolves about every 20 ms. */
const HEALTHY_GAP_CEILING_MS = 100;
/** Streaming time before the stall, so the lane has a measured cadence. */
const WARMUP_MS = 8_000;
/** One Worker telemetry push is 500 ms; allow several after the stall. */
const DRAIN_MS = 2_500;
const API_BASE_URL = process.env.API_BASE_URL || "http://localhost:8081";

/**
 * Create the meeting up front with no waiting room.
 *
 * Load-bearing, not tidiness: left to the join flow the guest lands on "Waiting
 * for meeting to start" and never enters, so the host's grid stays empty and
 * nothing is measured. Mirrors `wt-persistent-streams-freeze-regression.spec.ts`.
 */
async function createMeeting(meetingId: string, hostEmail: string, hostName: string) {
  const api = await request.newContext();
  const res = await api.post(`${API_BASE_URL}/api/v1/meetings`, {
    headers: {
      "Content-Type": "application/json",
      Cookie: `session=${generateSessionToken(hostEmail, hostName)}`,
    },
    data: {
      meeting_id: meetingId,
      attendees: [],
      allow_guests: true,
      waiting_room_enabled: false,
      admitted_can_admit: false,
      end_on_host_leave: false,
    },
  });
  expect([201, 409]).toContain(res.status());
  await api.dispose();
}

interface WtReceiveStats {
  audioLaneSessionMaxGapMs: number;
  maxHandoffDelayMs: number;
  framesReceived: number;
  inboxShedCount: number;
  unistreamReadyStallCount: number;
  unistreamQueueDepthBytes: number;
  unistreamBytesOfferedTotal: number;
}

async function readStats(page: Page): Promise<WtReceiveStats> {
  return page.evaluate(() => {
    const fn = (window as unknown as Record<string, unknown>).__videocall_wt_receive_stats;
    if (typeof fn !== "function") {
      throw new Error(
        "__videocall_wt_receive_stats is absent — MOCK_PEERS_ENABLED must be true on the dioxus-ui service",
      );
    }
    return (fn as () => WtReceiveStats)();
  });
}

/** Print what the run actually measured, so a re-run is self-documenting. */
function reportStats(arm: string, before: WtReceiveStats, after: WtReceiveStats): void {
  console.log(
    `[2728 ${arm}] audioLaneSessionMaxGapMs ${before.audioLaneSessionMaxGapMs.toFixed(1)} -> ` +
      `${after.audioLaneSessionMaxGapMs.toFixed(1)}; maxHandoffDelayMs ` +
      `${before.maxHandoffDelayMs.toFixed(1)} -> ${after.maxHandoffDelayMs.toFixed(1)}; ` +
      `frames ${before.framesReceived} -> ${after.framesReceived}; ` +
      `shed ${before.inboxShedCount} -> ${after.inboxShedCount}; ` +
      `unistreamReadyStallCount ${before.unistreamReadyStallCount} -> ` +
      `${after.unistreamReadyStallCount}; unistreamQueueDepthBytes ` +
      `${before.unistreamQueueDepthBytes} -> ${after.unistreamQueueDepthBytes}; ` +
      `unistreamBytesOfferedTotal ${before.unistreamBytesOfferedTotal} -> ` +
      `${after.unistreamBytesOfferedTotal}`,
  );
}

/** Block the renderer's main thread for real. */
async function stallMainThread(page: Page, ms: number): Promise<void> {
  await page.evaluate((duration) => {
    const end = Date.now() + duration;
    // Intentionally synchronous: an await here would yield to the event loop
    // and the reader would keep draining, which is the bug this test exists to
    // detect.
    while (Date.now() < end) {
      /* spin */
    }
  }, ms);
}

async function seedSession(
  context: BrowserContext,
  opts: { disableWorker: boolean },
): Promise<void> {
  // WebTransport is what #2728 changes; the e2e stack elects it when the
  // preference is sticky (Chrome is launched with
  // --origin-to-force-quic-on=127.0.0.1:4433 in BROWSER_ARGS).
  await context.addInitScript(`localStorage.setItem("vc_transport_preference", "webtransport");`);
  await context.addInitScript(`localStorage.setItem("vc_transport_sticky", "true");`);
  // Camera and mic both default OFF. Mic-on is load-bearing: the fake audio
  // device drives VAD, which is what puts packets on the class-3 audio lane.
  await context.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
  await context.addInitScript(`localStorage.setItem("vc_prejoin_mic_on", "true");`);
  if (opts.disableWorker) {
    await context.addInitScript(`window.__VC_WT_RECEIVE_WORKER = "0";`);
  }
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

  const result = await waitForVisibleState(
    [
      { name: "join", locator: joinButton },
      { name: "waiting", locator: waitingRoom },
      { name: "waiting-for-meeting", locator: waitingForMeeting },
      { name: "auto-joined", locator: grid },
    ] as const,
    30_000,
  );

  if (result === "waiting") {
    return result;
  }
  if (result === "waiting-for-meeting") {
    // The guest arrived before the host's join had propagated. The screen
    // resolves itself once the host is in; wait for whichever control the app
    // settles on rather than treating the interstitial as a terminal state.
    const settled = await waitForVisibleState(
      [
        { name: "join", locator: joinButton },
        { name: "waiting", locator: waitingRoom },
        { name: "grid", locator: grid },
      ] as const,
      45_000,
    );
    if (settled === "waiting") {
      return "waiting";
    }
    if (settled === "grid") {
      return "in-meeting";
    }
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
  if (guestResult !== "waiting") {
    return;
  }
  const admitButton = hostPage.getByTitle("Admit").first();
  await expect(admitButton).toBeVisible({ timeout: 20_000 });
  await hostPage.waitForTimeout(1000);
  await admitButton.dispatchEvent("click");
  await hostPage.waitForTimeout(3000);

  const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
  const guestGrid = guestPage.locator("#grid-container");
  const postAdmit = await waitForVisibleState(
    [
      { name: "join-button", locator: guestJoinButton },
      { name: "grid", locator: guestGrid },
    ] as const,
    20_000,
  );
  if (postAdmit === "join-button") {
    await guestPage.waitForTimeout(1000);
    await guestJoinButton.click();
    await guestPage.waitForTimeout(3000);
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }
}

/**
 * Two peers on WebTransport, both publishing camera and fake audio. The HOST is
 * the receiver under test — only its session's Worker is toggled.
 */
async function setupStallHarness(uiURL: string, meetingId: string, disableWorker: boolean) {
  const browser1 = await chromium.launch({ args: BROWSER_ARGS });
  const browser2 = await chromium.launch({ args: BROWSER_ARGS });

  const hostCtx = await createAuthenticatedContext(
    browser1,
    "stallhost@videocall.rs",
    "StallHost",
    uiURL,
  );
  const guestCtx = await createAuthenticatedContext(
    browser2,
    "stallguest@videocall.rs",
    "StallGuest",
    uiURL,
  );
  await seedSession(hostCtx, { disableWorker });
  await seedSession(guestCtx, { disableWorker: false });
  await createMeeting(meetingId, "stallhost@videocall.rs", "StallHost");

  const hostPage = await hostCtx.newPage();
  const guestPage = await guestCtx.newPage();

  await navigateToMeeting(hostPage, meetingId, "StallHost");
  const hostResult = await joinMeetingFromPage(hostPage);
  expect(hostResult).toBe("in-meeting");
  // Let the host's join reach the backend before the guest asks to join, so the
  // guest does not land on the "waiting for meeting to start" interstitial.
  await hostPage.waitForTimeout(3000);

  await navigateToMeeting(guestPage, meetingId, "StallGuest");
  const guestResult = await joinMeetingFromPage(guestPage);
  await admitGuestIfNeeded(hostPage, guestPage, guestResult);

  // A decoding peer tile on the host means the downlink is live.
  await expect(hostPage.locator(".grid-item:has(canvas)").first()).toBeVisible({ timeout: 30_000 });
  await hostPage.waitForTimeout(WARMUP_MS);

  return { hostPage, browser1, browser2 };
}

test.describe("WebTransport session worker under a main-thread stall (issue 2728)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("the audio-lane reader keeps draining through a 3s stall", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const { hostPage, browser1, browser2 } = await setupStallHarness(
      uiURL,
      `wt_worker_stall_${Date.now()}`,
      false,
    );

    try {
      const before = await readStats(hostPage);
      expect(
        before.audioLaneSessionMaxGapMs,
        "precondition: the #2724 class-3 stream must actually be being read. At 0 " +
          "the lane never opened and every assertion below passes vacuously",
      ).toBeGreaterThan(0);
      expect(
        before.audioLaneSessionMaxGapMs,
        "precondition: and it must be reading at a healthy cadence",
      ).toBeLessThan(HEALTHY_GAP_CEILING_MS);

      await stallMainThread(hostPage, STALL_MS);
      await hostPage.waitForTimeout(DRAIN_MS);
      const after = await readStats(hostPage);
      reportStats("worker", before, after);

      expect(
        after.audioLaneSessionMaxGapMs,
        `the Worker owns the reader, so the stall must not appear as a read gap ` +
          `(got ${after.audioLaneSessionMaxGapMs}ms; on the un-fixed path it is about ${STALL_MS}ms)`,
      ).toBeLessThan(HEALTHY_GAP_CEILING_MS);

      // Delta-guarded: `maxHandoffDelayMs` is a monotonic session max.
      expect(
        before.maxHandoffDelayMs,
        "precondition: the warmup must not already have produced a 1 s hand-off " +
          "delay, or the assertion below cannot attribute one to the stall",
      ).toBeLessThan(1_000);
      expect(
        after.maxHandoffDelayMs - before.maxHandoffDelayMs,
        "frames must have been RECEIVED during the stall and only handed over " +
          "afterwards; a zero here would mean the Worker was idle, not that it kept up",
      ).toBeGreaterThan(1_000);

      expect(after.framesReceived).toBeGreaterThan(before.framesReceived);
      // `unistreamQueueDepthBytes === 0` reads the same for "never backed
      // up" and "nothing was ever sent".
      expect(
        after.unistreamBytesOfferedTotal,
        "precondition for the two uplink readings: this peer must actually be " +
          "publishing over a persistent unistream",
      ).toBeGreaterThan(0);
      // The byte watermark cannot fire at two peers, so this counts the
      // main-silence ceiling (#2728 S4).
      expect(
        after.inboxShedCount,
        `the ${STALL_MS}ms stall must leave main silent past the camera ` +
          `ceiling; a zero means the silence gate never engaged`,
      ).toBeGreaterThan(before.inboxShedCount);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("with the worker disabled the same stall starves the reader", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const { hostPage, browser1, browser2 } = await setupStallHarness(
      uiURL,
      `wt_worker_stall_legacy_${Date.now()}`,
      true,
    );

    try {
      const before = await readStats(hostPage);
      expect(
        before.audioLaneSessionMaxGapMs,
        "precondition: the lane must be open on this arm too, or the contrast " +
          "with the Worker arm proves nothing",
      ).toBeGreaterThan(0);
      expect(before.audioLaneSessionMaxGapMs).toBeLessThan(HEALTHY_GAP_CEILING_MS);
      expect(
        before.maxHandoffDelayMs,
        "the in-page path produces and consumes each frame on one thread, so " +
          "there is no hand-off to delay",
      ).toBe(0);
      expect(
        before.framesReceived,
        "the DIRECT proof the kill switch took effect: no frame crosses a port " +
          "because there is no Worker. maxHandoffDelayMs === 0 only infers it, " +
          "and it is a subtraction that can round to 0 for a fast hand-off",
      ).toBe(0);

      await stallMainThread(hostPage, STALL_MS);
      await hostPage.waitForTimeout(DRAIN_MS);
      const after = await readStats(hostPage);
      reportStats("legacy", before, after);

      expect(
        after.audioLaneSessionMaxGapMs,
        "this is the defect #2728 fixes: with the reader on the stalled thread " +
          "the gap is the length of the stall",
      ).toBeGreaterThan(1_000);
      expect(after.maxHandoffDelayMs).toBe(0);
      expect(
        after.framesReceived,
        "and no Worker frame arrives during or after the stall either",
      ).toBe(0);
      expect(
        after.inboxShedCount,
        "the inbox cap and its silence ceiling both live in the Worker, so " +
          "the legacy path cannot shed",
      ).toBe(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
