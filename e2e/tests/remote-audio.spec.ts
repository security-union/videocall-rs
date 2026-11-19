import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { waitForServices } from "../helpers/wait-for-services";
import {
  BROWSER_ARGS,
  controlButton,
  createAuthenticatedContext,
  joinAsGuest,
  mute,
  peerAudioIndicator,
  startTwoPartyMeeting,
  unmute,
} from "../helpers/meeting";
import {
  StatsSample,
  audioLevelsSince,
  firstAudibleSince,
  installNetEqProbe,
  statsSince,
} from "../helpers/neteq-probe";

// Remote audio path under test: the speaker's fake mic (Chromium
// --use-fake-device-for-media-stream emits a periodic beep) is Opus-encoded,
// sent through the server, inserted into the listener's NetEq worker, and
// pulled 10 ms at a time by an AudioWorklet. The listener page carries the
// NetEq probe (helpers/neteq-probe.ts), which records the worker's audio
// level and stats messages, with stats forced to the 1 Hz diagnostics rate.
//
// Transport: the native e2e stack (nix/dev-stack.nix `e2eStack`) runs only the
// WebSocket server and writes `webTransportEnabled: false` into config.js, so
// these tests cover the WebSocket path. WebTransport needs the webtransport
// server added to that stack first.

const LISTENER = "AudioListener";
const SPEAKER = "AudioSpeaker";

interface Call {
  listenerBrowser: Browser;
  speakerBrowser: Browser;
  listener: Page;
  speaker: Page;
  meetingId: string;
  /** Uncaught page errors and encoder-related console errors, both pages. */
  errors: string[];
}

// The speaker encodes with ropus in a Rust wasm worker, with Opus DTX on:
// during silence it sends almost no audio packets. The fake mic beeps with
// gaps, so nothing here may assume a steady packet rate. Receive-side checks
// wait for packets after the beep starts instead of asserting a rate.
const ENCODER_ERROR = /audio encoder|encoder worker|audio_encoder_worker|media source/i;

function collectErrors(page: Page, label: string, errors: string[]): void {
  page.on("pageerror", (err) => errors.push(`${label} pageerror: ${err.message}`));
  page.on("console", (msg) => {
    if (msg.type() === "error" && ENCODER_ERROR.test(msg.text())) {
      errors.push(`${label} console: ${msg.text()}`);
    }
  });
}

function expectNoErrors(call: Call): void {
  expect(call.errors, "page errors or audio encoder failures").toEqual([]);
}

async function startCall(tag: string, uiURL: string, fullRateStats = true): Promise<Call> {
  const meetingId = `e2e_audio_${tag}_${Date.now()}`;
  const listenerBrowser = await chromium.launch({ args: BROWSER_ARGS });
  const speakerBrowser = await chromium.launch({ args: BROWSER_ARGS });

  const listenerCtx = await createAuthenticatedContext(
    listenerBrowser,
    `listener-${tag}@videocall.rs`,
    LISTENER,
    uiURL,
  );
  // Stats every second without opening the diagnostics panel; the tests
  // below sample them while the panel is closed.
  await installNetEqProbe(listenerCtx, { fullRateStats });
  const speakerCtx = await createAuthenticatedContext(
    speakerBrowser,
    `speaker-${tag}@videocall.rs`,
    SPEAKER,
    uiURL,
  );

  const listener = await listenerCtx.newPage();
  const speaker = await speakerCtx.newPage();
  const errors: string[] = [];
  collectErrors(listener, "listener", errors);
  collectErrors(speaker, "speaker", errors);

  // The listener hosts, so the speaker can leave and come back.
  await startTwoPartyMeeting(listener, LISTENER, speaker, SPEAKER, meetingId);
  await expect(
    listener.locator("#grid-container .floating-name", { hasText: SPEAKER }).first(),
  ).toBeVisible({ timeout: 30_000 });

  return { listenerBrowser, speakerBrowser, listener, speaker, meetingId, errors };
}

async function endCall(call: Call | undefined): Promise<void> {
  await call?.listenerBrowser.close();
  await call?.speakerBrowser.close();
}

/** Wait until the listener's worklet has played a non-silent frame after `since`. */
async function expectAudibleSince(page: Page, since: number, timeout = 20_000): Promise<number> {
  await expect
    .poll(() => firstAudibleSince(page, since), {
      timeout,
      message: "listener never played a non-silent frame from the remote peer",
    })
    .not.toBeNull();
  return (await firstAudibleSince(page, since)) as number;
}

/** Wait for `count` stats reports after `since` from workers that are receiving packets. */
async function collectFlowingStats(
  page: Page,
  since: number,
  count: number,
): Promise<StatsSample[]> {
  const flowing = async () => (await statsSince(page, since)).filter((s) => s.packetsPerSec > 0);
  await expect
    .poll(async () => (await flowing()).length, { timeout: count * 1_000 + 15_000 })
    .toBeGreaterThanOrEqual(count);
  return flowing();
}

/** Median gap in ms between consecutive stats reports from one worker. */
function medianStatsGap(stats: StatsSample[]): number {
  const gaps: number[] = [];
  for (let i = 1; i < stats.length; i++) {
    if (stats[i].worker === stats[i - 1].worker) {
      gaps.push(stats[i].t - stats[i - 1].t);
    }
  }
  expect(gaps.length, "not enough stats reports to measure their cadence").toBeGreaterThan(0);
  return median(gaps);
}

/** Wait for `count` stats reports (any content) after `since`. */
async function collectStats(page: Page, since: number, count: number, timeout: number) {
  await expect
    .poll(async () => (await statsSince(page, since)).length, { timeout })
    .toBeGreaterThanOrEqual(count);
  return statsSince(page, since);
}

function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
}

/** The AudioWorklet paces GetAudio at one call per 10 ms: about 100 a second. */
function expectWorkletPaced(stats: StatsSample[]): void {
  const rate = median(stats.map((s) => s.getAudioPerSec));
  expect(rate, `median GetAudio/sec was ${rate}`).toBeGreaterThanOrEqual(80);
  expect(rate, `median GetAudio/sec was ${rate}`).toBeLessThanOrEqual(120);
}

test.describe("Remote audio playback (NetEq worker + AudioWorklet, WebSocket)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("remote mic audio reaches the listener's worklet and diagnostics show it", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    let call: Call | undefined;
    try {
      call = await startCall("plays", baseURL || "http://localhost:3001");
      const { listener, speaker } = call;

      const unmutedAt = Date.now();
      await unmute(speaker);

      await expectAudibleSince(listener, unmutedAt);
      expectWorkletPaced(await collectFlowingStats(listener, unmutedAt, 3));

      // Diagnostics panel renders the new telemetry schema.
      await controlButton(listener, "Open Diagnostics").click();
      const sidebar = listener.locator("#diagnostics-sidebar");
      await expect(sidebar).toBeVisible({ timeout: 10_000 });
      await expect(sidebar.getByText("GetAudio/sec").first()).toBeAttached({ timeout: 20_000 });
      await expect(sidebar.getByText("Concealment events").first()).toBeAttached();
      const packetsPerSec = sidebar
        .locator(".neteq-status .status-item", { hasText: "PACKETS/S" })
        .locator(".status-value");
      await expect
        .poll(async () => Number(await packetsPerSec.textContent()), { timeout: 15_000 })
        .toBeGreaterThan(0);
      expectNoErrors(call);
    } finally {
      await endCall(call);
    }
  });

  test("speaking indicator turns on while the remote speaks and off when they mute", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    let call: Call | undefined;
    try {
      call = await startCall("speak", baseURL || "http://localhost:3001");
      const { listener, speaker } = call;
      const indicator = peerAudioIndicator(listener, SPEAKER);
      await expect(indicator).toBeVisible({ timeout: 10_000 });
      await expect(indicator).not.toHaveClass(/speaking/);

      await unmute(speaker);
      await expect(indicator).toHaveClass(/speaking/, { timeout: 20_000 });

      await mute(speaker);
      // The mic icon holds green for 1 s after the last audible frame.
      await expect(indicator).not.toHaveClass(/speaking/, { timeout: 8_000 });
      // And it stays off for 4 s: nothing buffered lights it up again.
      for (let sample = 0; sample < 16; sample++) {
        await expect(indicator).not.toHaveClass(/speaking/, { timeout: 0 });
        await listener.waitForTimeout(250);
      }
    } finally {
      await endCall(call);
    }
  });

  test("mute flushes NetEq: silence while muted and no stale burst after unmute", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    let call: Call | undefined;
    try {
      call = await startCall("mute", baseURL || "http://localhost:3001");
      const { listener, speaker } = call;

      const firstUnmute = Date.now();
      await unmute(speaker);
      await expectAudibleSince(listener, firstUnmute);
      await collectFlowingStats(listener, firstUnmute, 2);

      await mute(speaker);
      const mutedAt = Date.now();
      // Give the mute signal 1.5 s to cross the network, then watch 4 s of
      // muted time. Nothing audible may play in that window.
      await listener.waitForTimeout(5_500);
      const unmutedAt = Date.now();
      const leaked = (await audioLevelsSince(listener, mutedAt + 1_500)).filter(
        (l) => l.t < unmutedAt && l.rms > 0,
      );
      expect(leaked, "audible frames played while the speaker was muted").toEqual([]);

      await unmute(speaker);
      await expectAudibleSince(listener, unmutedAt);

      // A stale burst shows up as a buffer full of pre-mute audio being
      // played out (or accelerated through). After a flush the buffer sits
      // near its target, well under the 500 ms stall-reset threshold.
      const after = await collectFlowingStats(listener, unmutedAt, 3);
      const maxBuffer = Math.max(...after.map((s) => s.bufferMs));
      expect(maxBuffer, `max NetEq buffer after unmute was ${maxBuffer} ms`).toBeLessThan(500);
      expectWorkletPaced(after);
    } finally {
      await endCall(call);
    }
  });

  test("audio resumes after the remote peer leaves and rejoins", async ({ baseURL }) => {
    test.setTimeout(240_000);
    let call: Call | undefined;
    try {
      call = await startCall("rejoin", baseURL || "http://localhost:3001");
      const { listener, speaker, meetingId } = call;

      const firstUnmute = Date.now();
      await unmute(speaker);
      await expectAudibleSince(listener, firstUnmute);

      // Speaker hangs up; the listener's tile for them goes away.
      await controlButton(speaker, "Hang Up").click();
      const speakerTile = listener.locator("#grid-container .floating-name", {
        hasText: SPEAKER,
      });
      await expect(speakerTile).toHaveCount(0, { timeout: 30_000 });

      // Speaker comes back and turns the mic on again.
      await joinAsGuest(listener, speaker, SPEAKER, meetingId);
      await expect(speakerTile.first()).toBeVisible({ timeout: 30_000 });
      const rejoinedAt = Date.now();
      await unmute(speaker);

      await expectAudibleSince(listener, rejoinedAt);
      expectWorkletPaced(await collectFlowingStats(listener, rejoinedAt, 3));
      await expect(peerAudioIndicator(listener, SPEAKER)).toHaveClass(/speaking/, {
        timeout: 20_000,
      });
      expectNoErrors(call);
    } finally {
      await endCall(call);
    }
  });

  test("mic toggle off and on again keeps audio flowing (encoder restarts cleanly)", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    let call: Call | undefined;
    try {
      call = await startCall("toggle", baseURL || "http://localhost:3001");
      const { listener, speaker } = call;

      for (let cycle = 1; cycle <= 2; cycle++) {
        const unmutedAt = Date.now();
        await unmute(speaker);
        await expectAudibleSince(listener, unmutedAt);
        await collectFlowingStats(listener, unmutedAt, 2);
        await mute(speaker);
      }
      expectNoErrors(call);
    } finally {
      await endCall(call);
    }
  });

  test("switching microphone while live keeps audio flowing", async ({ baseURL }) => {
    test.setTimeout(180_000);
    let call: Call | undefined;
    try {
      call = await startCall("switchmic", baseURL || "http://localhost:3001");
      const { listener, speaker } = call;

      await unmute(speaker);
      await expectAudibleSince(listener, Date.now() - 1_000);

      await speaker.locator('[data-testid="open-settings"]').click();
      const micSelect = speaker.locator("#modal-audio-select");
      await expect(micSelect).toBeVisible({ timeout: 10_000 });
      const options = await micSelect.locator("option").evaluateAll((nodes) =>
        nodes.map((n) => ({
          value: (n as HTMLOptionElement).value,
          selected: (n as HTMLOptionElement).selected,
        })),
      );
      // Chromium's fake device exposes several fake inputs.
      test.skip(options.length < 2, "fewer than two microphones available");
      const target = options.find((o) => !o.selected) ?? options[0];

      const switchedAt = Date.now();
      await micSelect.selectOption(target.value);
      await expect(micSelect).toHaveValue(target.value);

      await expectAudibleSince(listener, switchedAt, 30_000);
      await collectFlowingStats(listener, switchedAt, 2);
      expectNoErrors(call);
    } finally {
      await endCall(call);
    }
  });

  test("NetEq stats come every 5 s with diagnostics closed and every 1 s while open", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    let call: Call | undefined;
    try {
      // No fullRateStats: observe the app's own stats rate.
      call = await startCall("cadence", baseURL || "http://localhost:3001", false);
      const { listener, speaker } = call;
      await unmute(speaker);
      await expectAudibleSince(listener, Date.now() - 1_000);

      // Panel closed: one report every five seconds.
      const closedSince = Date.now();
      const closed = await collectStats(listener, closedSince, 4, 30_000);
      const closedGap = medianStatsGap(closed);
      expect(closedGap, `closed-panel stats gap ${closedGap} ms`).toBeGreaterThanOrEqual(4_000);
      expect(closedGap, `closed-panel stats gap ${closedGap} ms`).toBeLessThanOrEqual(6_000);

      // Panel open: one report a second.
      await controlButton(listener, "Open Diagnostics").click();
      await expect(listener.locator("#diagnostics-sidebar")).toBeVisible({ timeout: 10_000 });
      // Let the slow interval's in-flight tick pass before measuring.
      const openSince = Date.now() + 1_500;
      const open = await collectStats(listener, openSince, 6, 20_000);
      const openGap = medianStatsGap(open);
      expect(openGap, `open-panel stats gap ${openGap} ms`).toBeGreaterThanOrEqual(700);
      expect(openGap, `open-panel stats gap ${openGap} ms`).toBeLessThanOrEqual(1_400);

      // Closing the panel slows the workers back down.
      // The open sidebar covers the bottom bar, so close it from its own header.
      await listener.locator("#diagnostics-sidebar .close-button").click();
      await expect(listener.locator("#diagnostics-sidebar")).toHaveCount(0);
      const reclosedSince = Date.now() + 1_500;
      const reclosed = await collectStats(listener, reclosedSince, 3, 25_000);
      const reclosedGap = medianStatsGap(reclosed);
      expect(reclosedGap, `re-closed stats gap ${reclosedGap} ms`).toBeGreaterThanOrEqual(4_000);
    } finally {
      await endCall(call);
    }
  });
});
