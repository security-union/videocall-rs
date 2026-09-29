import { readFileSync } from "node:fs";
import { test, expect, chromium, Browser, BrowserContext, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { wakeControls } from "../helpers/controls";
import { openPerformancePanel, readNetEqPacketsPerSec } from "../helpers/diagnostics-drawer";
import {
  healUplink,
  impairUplink,
  resolveShapedWsUrl,
  routeDownlinkThroughProxy,
} from "../helpers/downlink-impair";
import {
  ChecksumSample,
  distinctChecksumsInWindow,
  longestFrozenRunMs,
  samplePeerVideoChecksum,
  sampleChecksumSeries,
} from "../helpers/frame-liveness";
import { enableSimulcastFlag } from "../helpers/simulcast-config";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 2809: a saturated WS camera uplink must shed VIDEO deltas, not the publisher's own AUDIO.
 * Sole guard on the gate -> counter wiring (`match camera_ws_send_decision`, camera_encoder.rs);
 * the @bvt1 spec injects that counter directly. No CI path: `make e2e-up-impair` then
 * `make e2e-impair`, after `sudo sysctl -w net.ipv4.tcp_wmem='4096 16384 131072'`.
 *
 * `freshness_skip` is reported per window half but NOT asserted: fixed-tree delivery is clumpy
 * (multi-second stalls from gate-exempt keyframes and layer-2 restore), so one stall landing in
 * either half swings a half-vs-half count. The receiver-side claims asserted instead are the
 * frozen-run bound and the NetEq floors.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

const UPLINK_CAP_KB = 30;

const NETEQ_WINDOW = 15;
const NETEQ_WINDOW_MEAN_FLOOR = 25;
const NETEQ_MEAN_FLOOR = 30;

const SETTLE_MS = 60_000;

const SAMPLE_COUNT = 30;
const SAMPLE_INTERVAL_MS = 1_000;

const MAX_FROZEN_RUN_MS = 15_000;
// > one 1s sampling cadence, so a single unsampleable frame does not re-anchor (under-report) a run.
const CHECKSUM_BRIDGE_MS = 2_500;
const RECOVERY_WINDOW_MS = 8_000;
const RECOVERY_SAMPLE_INTERVAL_MS = 500;
const MIN_RECOVERY_DISTINCT = 2;

const GATE_RE =
  /CameraEncoder: dropping stale camera delta\(s\) under WS backpressure \(issue 2809\).*dropped=\d+.*buffered=\d+.*threshold=\d+/;

const AQ_AXIS_RE =
  /CameraEncoder: client WS stale-delta backpressure detected \(\d+ camera deltas dropped in [0-9.]+ms\), forcing video step-down/;

const TOP_LAYER_RE = /AdaptiveQuality: simulcast dropped TOP layer \(\d+ -> \d+ active of \d+\)/;

const FRESHNESS_SKIP_RE = /\[JITTER_BUFFER\] freshness_skip/;

// `complete_election`'s winner line; connection ids are keyed `ws_*` / `wt_*`.
const WS_ELECTED_RE = /Elected connection ws_/;

// The PRE-EXISTING overflow axis (websocket_drop_count, threshold 3 in 1000ms), distinct from
// AQ_AXIS_RE: it fires only once bill_send has already reached its 1 MiB discard ceiling.
const WS_OVERFLOW_AXIS_RE = /CameraEncoder: client WS backpressure detected \(\d+ drops in/;

function meanOf(values: number[]): number {
  return values.reduce((a, b) => a + b, 0) / values.length;
}

interface StampedLine {
  t: number;
  text: string;
}

function collectConsole(page: Page): StampedLine[] {
  const lines: StampedLine[] = [];
  page.on("console", (msg) => {
    lines.push({ t: Date.now(), text: msg.text() });
  });
  return lines;
}

async function startCameraAndConfirm(page: Page): Promise<void> {
  await wakeControls(page);
  const startBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Start Video" }),
  });
  await expect(startBtn).toBeVisible({ timeout: 15_000 });
  await startBtn.click();
  await expect(
    page.locator("button.video-control-button", {
      has: page.locator("span.tooltip", { hasText: "Stop Video" }),
    }),
  ).toBeVisible({ timeout: 30_000 });
}

function micToggle(page: Page): Locator {
  return page.locator('[data-testid="mic-toggle-button"]');
}

async function expectMicState(toggle: Locator, on: boolean): Promise<void> {
  await expect(toggle).toHaveClass(on ? /\bactive\b/ : /\boff\b/, { timeout: 15_000 });
  await expect(toggle).toHaveAttribute(
    "aria-label",
    on ? "Microphone — Mute" : "Microphone — Unmute",
    { timeout: 15_000 },
  );
}

async function setMic(page: Page, on: boolean): Promise<void> {
  await wakeControls(page);
  const toggle = micToggle(page);
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  const isOn = ((await toggle.getAttribute("class")) || "").includes("active");
  if (isOn !== on) {
    await toggle.click();
  }
  await expectMicState(toggle, on);
}

/** Max host `tcp_wmem`; above this the kernel send buffer, not bufferedAmount, is the queue. */
const MAX_TCP_WMEM_BYTES = 262_144;

function assertSendBufferCapped(): void {
  let wmem: string;
  try {
    wmem = readFileSync("/proc/sys/net/ipv4/tcp_wmem", "utf8");
  } catch {
    console.warn(
      "issue 2809: could not read /proc/sys/net/ipv4/tcp_wmem (non-Linux host?). Proceeding, " +
        "but if the sender's kernel send buffer autotunes into the megabytes the gate will not " +
        "engage and the audio-floor assertion will fail for a harness reason.",
    );
    return;
  }
  const max = Number(wmem.trim().split(/\s+/)[2]);
  if (Number.isFinite(max) && max > MAX_TCP_WMEM_BYTES) {
    throw new Error(
      `issue 2809: host tcp_wmem max is ${max} bytes, needs <=${MAX_TCP_WMEM_BYTES} so the ` +
        "sender's kernel send buffer is not the queue instead of bufferedAmount. Run: sudo " +
        "sysctl -w net.ipv4.tcp_wmem='4096 16384 131072' (reversible; default max is 4194304).",
    );
  }
}

test.describe("issue 2809 camera WS uplink audio survival", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a saturated WS camera uplink sheds video, not the publisher's own audio @impair", async ({
    baseURL,
  }, testInfo) => {
    test.setTimeout(600_000);
    assertSendBufferCapped();
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_cam_uplink_audio_${Date.now()}`;

    let hostBrowser: Browser | undefined;
    let guestBrowser: Browser | undefined;

    try {
      hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
      guestBrowser = await chromium.launch({ args: BROWSER_ARGS });

      const hostCtx: BrowserContext = await createAuthenticatedContext(
        hostBrowser,
        "host-cam-uplink@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx: BrowserContext = await createAuthenticatedContext(
        guestBrowser,
        "guest-cam-uplink@videocall.rs",
        "GuestUser",
        uiURL,
      );

      // ORDER IS LOAD-BEARING: both register a `**/config.js` route, Playwright resolves
      // newest-first with no fallback, and the proxy helper's authoritative layer is its
      // separate `**/config.local.js` route — so simulcast must be registered LAST.
      await routeDownlinkThroughProxy(guestCtx, await resolveShapedWsUrl());
      // The stack's config.js pins `experimentalSimulcastMaxLayers: 1` — `drop_top_layer` is a
      // silent no-op at one layer, and one layer cannot saturate the cap.
      await enableSimulcastFlag(guestCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(hostCtx, 3, { capabilityMaxLayersOverride: 3 });

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      const guestConsole = collectConsole(guestPage);
      const hostConsole = collectConsole(hostPage);

      await enterTwoUserMeeting(hostPage, guestPage, meetingId);

      await startCameraAndConfirm(guestPage);
      await setMic(guestPage, true);

      await wakeControls(hostPage);
      const drawer = await openPerformancePanel(hostPage);

      await expect
        .poll(async () => readNetEqPacketsPerSec(drawer), {
          timeout: 60_000,
          intervals: [1000, 2000],
          message:
            "baseline: the host is not consuming the guest's audio before the uplink is shaped, " +
            "so every delivery assertion below would be vacuous (NaN = no sample rendered).",
        })
        .toBeGreaterThan(0);

      await impairUplink({ rateKb: UPLINK_CAP_KB });
      await hostPage.waitForTimeout(SETTLE_MS);

      const windowStart = Date.now();
      const samples: number[] = [];
      // nth=0 is the guest's tile: the host's camera is never started, so it renders no canvas.
      const frames: ChecksumSample[] = [];
      for (let i = 0; i < SAMPLE_COUNT; i++) {
        frames.push({
          atMs: Date.now() - windowStart,
          checksum: await samplePeerVideoChecksum(hostPage, 0),
        });
        samples.push(await readNetEqPacketsPerSec(drawer));
        await hostPage.waitForTimeout(SAMPLE_INTERVAL_MS);
      }
      const windowEnd = Date.now();

      const overflowAxisHits = guestConsole.filter((l) => WS_OVERFLOW_AXIS_RE.test(l.text)).length;
      const windowMid = windowStart + (windowEnd - windowStart) / 2;
      const freshnessSkipLines = hostConsole.filter(
        (l) => FRESHNESS_SKIP_RE.test(l.text) && l.t >= windowStart && l.t <= windowEnd,
      );
      const freshnessSkips = freshnessSkipLines.length;
      const freshnessSkipsFirstHalf = freshnessSkipLines.filter((l) => l.t < windowMid).length;
      const freshnessSkipsSecondHalf = freshnessSkips - freshnessSkipsFirstHalf;
      const topLayerHits = guestConsole.filter((l) => TOP_LAYER_RE.test(l.text)).length;
      const windowMeans = samples
        .slice(0, Math.max(0, samples.length - NETEQ_WINDOW + 1))
        .map((_, i) => ({ startIndex: i, mean: meanOf(samples.slice(i, i + NETEQ_WINDOW)) }));
      const overallMean = meanOf(samples);
      const framesSampled = frames.filter((f) => f.checksum !== null).length;
      const frozenRunMs = longestFrozenRunMs(frames, CHECKSUM_BRIDGE_MS);
      console.log(
        `issue 2809 host NetEq pkt/s samples (${UPLINK_CAP_KB} KB/s cap): ${JSON.stringify(samples)}`,
      );
      console.log(
        `issue 2809 delivery read-out: mean=${overallMean.toFixed(1)}, min sliding-${NETEQ_WINDOW} ` +
          `mean=${Math.min(...windowMeans.map((w) => w.mean)).toFixed(1)}, existing WS overflow ` +
          `axis hits=${overflowAxisHits}, dropped TOP layer hits=${topLayerHits}, guest tile ` +
          `frames sampled=${framesSampled}/${frames.length}, longest frozen run=${frozenRunMs}ms, ` +
          `host freshness_skip lines=${freshnessSkips} ` +
          `(first half=${freshnessSkipsFirstHalf}, second half=${freshnessSkipsSecondHalf})`,
      );
      await testInfo.attach("neteq-samples.json", {
        contentType: "application/json",
        body: Buffer.from(
          JSON.stringify(
            {
              uplinkCapKb: UPLINK_CAP_KB,
              windowMeanFloor: NETEQ_WINDOW_MEAN_FLOOR,
              meanFloor: NETEQ_MEAN_FLOOR,
              windowStart,
              windowEnd,
              samples,
              windowMeans,
              overallMean,
              overflowAxisHits,
              topLayerHits,
              framesSampled,
              frozenRunMs,
              freshnessSkips,
              freshnessSkipsFirstHalf,
              freshnessSkipsSecondHalf,
              frames,
            },
            null,
            2,
          ),
        ),
      });

      await expect
        .poll(() => guestConsole.some((l) => WS_ELECTED_RE.test(l.text)), {
          timeout: 10_000,
          intervals: [500, 1000],
          message:
            "the publisher did not elect a WebSocket connection, so the gate is inert " +
            "(send_queue_depth() is None on WebTransport) and every assertion below is vacuous.",
        })
        .toBe(true);

      await expect
        .poll(() => guestConsole.some((l) => GATE_RE.test(l.text)), {
          timeout: 20_000,
          intervals: [1000, 2000],
          message:
            `the send-side gate never engaged under a ${UPLINK_CAP_KB} KB/s uplink cap: no ` +
            "`dropping stale camera delta(s) under WS backpressure` line from the publisher.",
        })
        .toBe(true);

      // Recounted here: `overflowAxisHits` above is the read-out snapshot, and the console has
      // grown since (the gate/axis polls above run after it).
      const overflowAxisHitsNow = guestConsole.filter((l) =>
        WS_OVERFLOW_AXIS_RE.test(l.text),
      ).length;
      expect(
        overflowAxisHitsNow,
        "the gate engaged but bufferedAmount still reached `bill_send`'s 1 MiB discard ceiling, " +
          `where the publisher's own audio is shed alongside its video. hits=${overflowAxisHitsNow}`,
      ).toBe(0);

      const starvedWindows = windowMeans
        .filter((w) => !(w.mean >= NETEQ_WINDOW_MEAN_FLOOR))
        .map(
          (w) => `samples ${w.startIndex + 1}..${w.startIndex + NETEQ_WINDOW}=${w.mean.toFixed(1)}`,
        );
      expect(
        starvedWindows,
        `the host's audio intake fell below ${NETEQ_WINDOW_MEAN_FLOOR} pkt/s averaged over a ` +
          `${NETEQ_WINDOW}s window (of 50 nominal); NaN fails here too. starved windows=` +
          `[${starvedWindows.join("; ")}] samples=[${samples.join(", ")}]`,
      ).toEqual([]);

      expect(
        overallMean,
        `the host's mean audio intake over the impaired window fell below ${NETEQ_MEAN_FLOOR} ` +
          `pkt/s (of 50 nominal). mean=${Number.isFinite(overallMean) ? overallMean.toFixed(1) : String(overallMean)} ` +
          `samples=[${samples.join(", ")}]`,
      ).toBeGreaterThanOrEqual(NETEQ_MEAN_FLOOR);

      expect(
        framesSampled,
        "the guest's tile was unsampleable for most of the impaired window, so the frozen-run " +
          `bound below proves nothing. sampled=${framesSampled}/${frames.length}`,
      ).toBeGreaterThanOrEqual(Math.ceil(frames.length / 2));

      expect(
        frozenRunMs,
        `the guest's tile held one frame for longer than ${MAX_FROZEN_RUN_MS}ms (three GOPs), so ` +
          `gate-exempt keyframes were dropped too, not just deltas. run=${frozenRunMs}ms`,
      ).toBeLessThanOrEqual(MAX_FROZEN_RUN_MS);

      await expect
        .poll(() => guestConsole.some((l) => AQ_AXIS_RE.test(l.text)), {
          timeout: 20_000,
          intervals: [1000, 2000],
          message:
            "drops happened (the gate line fired) but the stale-delta counter never drove " +
            "force_video_step_down: no `stale-delta backpressure detected` line.",
        })
        .toBe(true);

      const axisIndex = guestConsole.findIndex((l) => AQ_AXIS_RE.test(l.text));
      expect(
        axisIndex,
        "no AQ axis line is in the console even though the poll above resolved on one — the " +
          "index below would be -1, under which every `dropped TOP layer` line counts as " +
          "following the axis and the ordering assertion is vacuous.",
      ).toBeGreaterThanOrEqual(0);
      await expect
        .poll(
          () => guestConsole.filter((l, i) => i > axisIndex && TOP_LAYER_RE.test(l.text)).length,
          {
            timeout: 15_000,
            intervals: [1000, 2000],
            message:
              "the AQ axis fired but no `dropped TOP layer` line followed it, so the forced " +
              "step-down was a no-op (already at the floor, or drop_top_layer returned false).",
          },
        )
        .toBeGreaterThan(0);

      await healUplink();

      await expect
        .poll(async () => readNetEqPacketsPerSec(drawer), {
          timeout: 90_000,
          intervals: [2000, 3000],
          message:
            "the host's audio intake stayed at 0 after the uplink healed, so the shed left the " +
            "audio path wedged rather than throttled.",
        })
        .toBeGreaterThan(0);

      const postHealFrames = await sampleChecksumSeries(
        hostPage,
        RECOVERY_WINDOW_MS,
        RECOVERY_SAMPLE_INTERVAL_MS,
        0,
      );
      const postHealDistinct = distinctChecksumsInWindow(postHealFrames, 0, RECOVERY_WINDOW_MS + 1);
      expect(
        postHealDistinct,
        "the guest's tile was still not painting changing frames after the uplink healed. " +
          `distinct checksums=${postHealDistinct} over ${RECOVERY_WINDOW_MS}ms`,
      ).toBeGreaterThanOrEqual(MIN_RECOVERY_DISTINCT);
    } finally {
      await healUplink().catch(() => {});
      await hostBrowser?.close();
      await guestBrowser?.close();
    }
  });
});
