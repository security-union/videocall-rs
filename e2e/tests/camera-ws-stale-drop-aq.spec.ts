import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { joinAndStartCamera, joinAsGuestAndStartCamera } from "../helpers/camera-publisher";
import { CAMERA_AQ_AXES } from "../helpers/rust-mirrored-constants";
import { enableSimulcastFlag } from "../helpers/simulcast-config";
import { enterMeetingAsHost } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Covers drop counter -> AQ axis -> forced video step-down. The send-path drop DECISION is in
 * native unit tests; receiver-side audio survival is camera-uplink-audio-survival.spec.ts.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

const { CAMERA_WS_STALE_DROP_THRESHOLD, WS_SELF_CONGESTION_DROP_THRESHOLD } = CAMERA_AQ_AXES;

const DROPS_ABOVE_THRESHOLD = CAMERA_WS_STALE_DROP_THRESHOLD + 3;

/** Below the threshold, and above the WS-overflow axis's so a spec wired to that counter fails. */
const DROPS_BELOW_THRESHOLD = WS_SELF_CONGESTION_DROP_THRESHOLD + 2;

const BELOW_THRESHOLD_SETTLE_MS = 4_000;

const AQ_AXIS_RE =
  /CameraEncoder: client WS stale-delta backpressure detected \(\d+ camera deltas dropped in [0-9.]+ms\), forcing video step-down/;

// A camera publisher starts at `active_layer_count == 1` (`initial_active_layer_count`), where
// `drop_top_layer` returns false without logging, so the tier is the only observable shed here.
const TIER_STEP_DOWN_RE =
  /AdaptiveQuality: CONGESTION forced video step-down to tier '[a-z_]+' \(index \d+\)/;

const AQ_TICK_RE = /AQ_STATUS: .*ladder=camera/;

const PROBE_HELD_RE = /AQ_LAYER_PROBE: held by uplink axis activity \(stamp within \d+ms\)/;
const PROBE_ADDED_RE =
  /AQ_LAYER_PROBE: headroom probe ADDED a layer \(\d+ -> \d+ active of \d+ ceiling\)/;

const TOP_LAYER_RE = /AdaptiveQuality: simulcast dropped TOP layer \(\d+ -> \d+ active of \d+\)/;

const FORCED_SHED_OSCILLATION_RE =
  /AQ_LAYER_PROBE: oscillation detected \(probed layer shed by forced_step_down within 8000ms\) — penalty box armed for \d+ms/;

// host.rs logs `min(experimentalSimulcastMaxLayers, capability ceiling)` once per meeting mount.
const EFFECTIVE_LAYERS_RE = /CameraEncoder: effective simulcast layers = (\d+)/;

const RESTORED_RE = /AdaptiveQuality: simulcast restored TOP layer \(\d+ -> \d+ active of \d+\)/;

// Emitted by the manager in the same tick as the tier step-up's `restored TOP layer`.
const TIER_STEP_UP_RE = /AdaptiveQuality: video stepped UP to tier/;
const SAME_TICK_MS = 50;

const PROBE_DIAG_RE =
  /effective simulcast layers|SIMULCAST:|AQ_STATUS: .*ladder=camera|AQ_LAYER_PROBE|AQ_LAYER_SHED|simulcast (dropped|restored) TOP layer|LAYER_HINT|testCapabilityMaxLayersOverride|experimentalSimulcastMaxLayers/;

// `LAYER_PROBE_OSCILLATION_WINDOW_MS`: a shed later than this after the add is not a flap.
const OSCILLATION_WINDOW_MS = 8_000;

// Inside `LAYER_PROBE_PENALTY_BASE_MS` (15000): the assert must land before the penalty expires.
const PENALTY_HOLD_ASSERT_MS = 12_000;

const RECURRING_BUMP_WINDOW_MS = 20_000;
// Under AQ_TICK_INTERVAL_MS (1000) so every camera AQ tick sees an advance.
const RECURRING_BUMP_INTERVAL_MS = 500;
// Must span >= 2 AQ ticks: the hold stamps only once 2 of the last 3 ticks saw an advance.
const HOLD_ARM_MS = 2_500;
const REOPEN_POLL_MS = 15_000;

// The GATE's own log, emitted only by `record_camera_ws_stale_drop` — i.e. only by a REAL drop,
// never by the netsim bump. Its throttle starts unarmed, so the first real drop always logs:
// zero matching lines therefore means the process-global counter took zero real increments.
const GATE_REAL_DROP_RE =
  /CameraEncoder: dropping stale camera delta\(s\) under WS backpressure \(issue 2809\)/;

// `complete_election`'s winner line; connection ids are keyed `ws_*` / `wt_*`.
const WS_ELECTED_RE = /Elected connection ws_/;

// `force_video_step_down` discards a request inside `QUALITY_WARMUP_MS` (5000) of manager
// creation or `MIN_TIER_TRANSITION_INTERVAL_MS` (1500) of the last tier move.
const AQ_SETTLE_MS = 8_000;

type WsStaleDropHook = { bumpWsStaleDeltaDrop?: (n: number) => unknown } | undefined;

function collectConsole(page: Page): string[] {
  const lines: string[] = [];
  page.on("console", (msg) => lines.push(msg.text()));
  return lines;
}

type TimedLine = { text: string; at: number };

function collectTimedConsole(page: Page): TimedLine[] {
  const lines: TimedLine[] = [];
  page.on("console", (msg) => lines.push({ text: msg.text(), at: Date.now() }));
  return lines;
}

async function waitUntil(
  pred: () => Promise<boolean> | boolean,
  timeoutMs: number,
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await pred()) return true;
    await new Promise((r) => setTimeout(r, 250));
  }
  return pred();
}

async function probeDiagnostics(page: Page, timed: TimedLine[]): Promise<string> {
  const served = await page
    .evaluate(() => fetch("/config.js").then((r) => r.text()))
    .catch((e) => `fetch(/config.js) failed: ${e}`);
  return [
    "--- served /config.js:",
    served,
    "--- publisher console (AQ / probe / hint lines):",
    ...timed.filter((l) => PROBE_DIAG_RE.test(l.text)).map((l) => `${l.at} ${l.text}`),
  ].join("\n");
}

async function newViewerContext(
  browser: Browser,
  email: string,
  uiURL: string,
): Promise<BrowserContext> {
  const ctx = await createAuthenticatedContext(browser, email, "WsStaleDropViewer", uiURL);
  await pinWebSocketTransport(ctx);
  await enableSimulcastFlag(ctx, 3, { capabilityMaxLayersOverride: 3 });
  return ctx;
}

async function newPublisherContext(
  browser: Browser,
  email: string,
  uiURL: string,
): Promise<BrowserContext> {
  const ctx = await createAuthenticatedContext(browser, email, "WsStaleDropPublisher", uiURL);
  await pinWebSocketTransport(ctx);
  // `camera_ws_gate_threshold_bytes` scales with ladder depth, so run the 3-layer production
  // posture; the stack's config.js pins 1 and the capability ceiling is core-count sniffed.
  await enableSimulcastFlag(ctx, 3, { capabilityMaxLayersOverride: 3 });
  return ctx;
}

async function assertHookPresent(page: Page): Promise<void> {
  const ready = await page.evaluate(
    () => typeof (window.__vcNetsim as unknown as WsStaleDropHook)?.bumpWsStaleDeltaDrop,
  );
  expect(
    ready,
    "window.__vcNetsim.bumpWsStaleDeltaDrop is missing. Either the dioxus UI image was built " +
      "WITHOUT the `netsim` cargo feature (docker/docker-compose.e2e.yaml pins " +
      "TRUNK_BUILD_FEATURES=netsim; rebuild with `make e2e-build`), or the image predates the " +
      "issue-2809 hook in videocall-client/src/connection/netsim_control.rs.",
  ).toBe("function");
}

async function bumpStaleDrops(page: Page, n: number): Promise<void> {
  await page.evaluate((count) => {
    (window.__vcNetsim as unknown as WsStaleDropHook)?.bumpWsStaleDeltaDrop?.(count);
  }, n);
}

function realDropLines(consoleLines: string[]): string[] {
  return consoleLines.filter((line) => GATE_REAL_DROP_RE.test(line));
}

function expectNoRealDrops(consoleLines: string[], when: string): void {
  expect(
    realDropLines(consoleLines),
    `${when}: the live WS freshness gate took REAL drops on the same process-global counter ` +
      `this test injects into, so the injected delta is no longer ${DROPS_BELOW_THRESHOLD} and ` +
      `the margin to ${CAMERA_WS_STALE_DROP_THRESHOLD} may have been consumed by the harness ` +
      "rather than left intact. Do not read this run as evidence about the threshold.",
  ).toEqual([]);
}

async function waitForWsElectionAndAqTick(consoleLines: string[]): Promise<void> {
  await expect
    .poll(() => consoleLines.some((line) => WS_ELECTED_RE.test(line)), {
      timeout: 30_000,
      intervals: [500, 1000],
      message:
        "the publisher did not elect a WebSocket connection. The injected counter would still " +
        "drive the axis, so the axis assertion would pass in a posture where the gate that feeds " +
        "it cannot run (`send_queue_depth()` is None on WebTransport).",
    })
    .toBe(true);
  await expect
    .poll(() => consoleLines.some((line) => AQ_TICK_RE.test(line)), {
      timeout: 30_000,
      intervals: [500, 1000],
      message: "no camera AQ tick line appeared, so the AQ manager never started.",
    })
    .toBe(true);
}

async function settleAq(page: Page, consoleLines: string[]): Promise<void> {
  await waitForWsElectionAndAqTick(consoleLines);
  await page.waitForTimeout(AQ_SETTLE_MS);
}

test.describe("issue 2809 camera WS stale-delta AQ axis", () => {
  test.describe.configure({ mode: "serial", retries: 1 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("camera publisher steps video down on WS stale-delta drops @bvt1", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const meetingId = `e2e_ws_stale_aq_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const ctx = await newPublisherContext(
        browser,
        "ws-stale-aq@videocall.rs",
        baseURL || DEFAULT_UI_URL,
      );
      const page = await ctx.newPage();
      const consoleLines = collectConsole(page);

      await joinAndStartCamera(page, meetingId);
      await assertHookPresent(page);

      await settleAq(page, consoleLines);

      await expect
        .poll(
          async () => {
            // Re-bump each iteration: a single injection can land just after the monitor tick
            // read the counter, and only this test needs to clear the threshold.
            await bumpStaleDrops(page, DROPS_ABOVE_THRESHOLD);
            return consoleLines.some((line) => AQ_AXIS_RE.test(line));
          },
          {
            timeout: 30_000,
            intervals: [500, 1000],
            message:
              `${DROPS_ABOVE_THRESHOLD} drops injected per poll exceed the threshold of ` +
              `${CAMERA_WS_STALE_DROP_THRESHOLD}, ` +
              "but the camera AQ monitor loop never logged `stale-delta backpressure detected ... " +
              "forcing video step-down`.",
          },
        )
        .toBe(true);

      const axisIndex = consoleLines.findIndex((line) => AQ_AXIS_RE.test(line));
      await expect
        .poll(() => consoleLines.some((line, i) => i > axisIndex && TIER_STEP_DOWN_RE.test(line)), {
          timeout: 15_000,
          intervals: [500, 1000],
          message:
            "the axis fired but no `AdaptiveQuality: CONGESTION forced video step-down` line " +
            "followed it, so the forced step-down changed nothing.",
        })
        .toBe(true);
    } finally {
      await browser.close();
    }
  });

  test("below-threshold stale-delta drops do NOT step video down @bvt1", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const meetingId = `e2e_ws_stale_aq_neg_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const ctx = await newPublisherContext(
        browser,
        "ws-stale-aq-neg@videocall.rs",
        baseURL || DEFAULT_UI_URL,
      );
      const page = await ctx.newPage();
      const consoleLines = collectConsole(page);

      await joinAndStartCamera(page, meetingId);
      await assertHookPresent(page);

      await settleAq(page, consoleLines);

      expectNoRealDrops(consoleLines, "before injecting");
      await bumpStaleDrops(page, DROPS_BELOW_THRESHOLD);
      await page.waitForTimeout(BELOW_THRESHOLD_SETTLE_MS);
      expectNoRealDrops(consoleLines, "after the settle");

      expect(
        consoleLines.filter((line) => AQ_AXIS_RE.test(line)),
        `${DROPS_BELOW_THRESHOLD} injected drops are below the threshold of ` +
          `${CAMERA_WS_STALE_DROP_THRESHOLD} but the axis ` +
          "fired anyway, so it steps video down on any non-zero drop count.",
      ).toEqual([]);
      expect(
        consoleLines.filter((line) => TIER_STEP_DOWN_RE.test(line)),
        "a forced video step-down happened without the axis firing, so the positive test's tier " +
          "assertion could pass without this axis.",
      ).toEqual([]);
    } finally {
      await browser.close();
    }
  });

  test("a forced shed of a probed layer holds the headroom probe for the penalty window @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const meetingId = `e2e_ws_stale_aq_flap_${Date.now()}`;
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const viewerCtx = await newViewerContext(browser, "ws-stale-aq-viewer@videocall.rs", uiURL);
      const viewerPage = await viewerCtx.newPage();
      await enterMeetingAsHost(viewerPage, meetingId);

      const ctx = await newPublisherContext(browser, "ws-stale-aq-flap@videocall.rs", uiURL);
      const page = await ctx.newPage();
      const consoleLines = collectConsole(page);
      const timed = collectTimedConsole(page);

      await joinAsGuestAndStartCamera(viewerPage, page, meetingId);
      await assertHookPresent(page);
      await waitForWsElectionAndAqTick(consoleLines);

      const effective = timed.map((l) => EFFECTIVE_LAYERS_RE.exec(l.text)?.[1]).find(Boolean);
      expect(
        effective,
        "the publisher did not log `CameraEncoder: effective simulcast layers = 3`, so the " +
          "3-layer `enableSimulcastFlag` override was not honoured and the probe has no layer " +
          "to add.\n" +
          (await probeDiagnostics(page, timed)),
      ).toBe("3");

      const addedBefore = timed.filter((l) => PROBE_ADDED_RE.test(l.text)).length;
      const sawAdded = await waitUntil(
        () => timed.filter((l) => PROBE_ADDED_RE.test(l.text)).length > addedBefore,
        30_000,
      );
      expect(
        sawAdded,
        "no new `headroom probe ADDED a layer` line in 30 s with 3 layers configured, a viewer " +
          "present and the queue clear; the no-re-add assertion below would pass vacuously.\n" +
          (await probeDiagnostics(page, timed)),
      ).toBe(true);
      const addedAt = timed.filter((l) => PROBE_ADDED_RE.test(l.text)).at(-1)!.at;
      expect(
        timed.some((l) => l.at <= addedAt && RESTORED_RE.test(l.text)),
        "the probe add logged no `simulcast restored TOP layer` line, so the restore regex no " +
          "longer matches its emitter and the bare-restore check below would pass empty.",
      ).toBe(true);

      const sawDrop = await waitUntil(async () => {
        await bumpStaleDrops(page, DROPS_ABOVE_THRESHOLD);
        return timed.some((l) => l.at >= addedAt && TOP_LAYER_RE.test(l.text));
      }, 15_000);
      expect(
        sawDrop,
        `${DROPS_ABOVE_THRESHOLD} drops injected per poll after the probe add never produced a ` +
          "`simulcast dropped TOP layer` line.\n" +
          (await probeDiagnostics(page, timed)),
      ).toBe(true);
      const drop = timed.find((l) => l.at >= addedAt && TOP_LAYER_RE.test(l.text))!;
      expect(
        timed.some((l) => l.at >= addedAt && l.at <= drop.at && AQ_AXIS_RE.test(l.text)),
        "the top layer was dropped without a preceding stale-delta axis line, so the shed did " +
          "not come from `force_video_step_down`.",
      ).toBe(true);

      expect(
        drop.at - addedAt,
        `the shed landed ${drop.at - addedAt}ms after the probe add; the penalty only arms ` +
          `for a shed within ${OSCILLATION_WINDOW_MS}ms (LAYER_PROBE_OSCILLATION_WINDOW_MS), ` +
          "so this run cannot judge the fix.",
      ).toBeLessThan(OSCILLATION_WINDOW_MS);

      await expect
        .poll(() => timed.some((l) => FORCED_SHED_OSCILLATION_RE.test(l.text)), {
          timeout: 5_000,
          intervals: [250, 500],
          message:
            "a probed layer was force-shed inside the oscillation window but no " +
            "`oscillation detected (probed layer shed by forced_step_down ...)` line appeared.",
        })
        .toBe(true);

      await page.waitForTimeout(Math.max(0, drop.at + PENALTY_HOLD_ASSERT_MS - Date.now()));
      const inWindow = (l: TimedLine) => l.at > drop.at && l.at <= drop.at + PENALTY_HOLD_ASSERT_MS;
      expect(
        timed.filter((l) => inWindow(l) && PROBE_ADDED_RE.test(l.text)),
        "the probe re-added the shed layer inside the penalty window — the forced shed did not " +
          "arm the anti-flap penalty box",
      ).toEqual([]);
      // Only the tier step-up (paired with `stepped UP`) may restore a layer in the window.
      expect(
        timed.filter(
          (l) =>
            inWindow(l) &&
            RESTORED_RE.test(l.text) &&
            !timed.some(
              (p) => TIER_STEP_UP_RE.test(p.text) && Math.abs(p.at - l.at) <= SAME_TICK_MS,
            ),
        ),
        "a restore other than the tier step-up re-added the force-shed layer inside the penalty " +
          "window (the probe, or the layer-cap restore climbing above the forced shed)",
      ).toEqual([]);
    } finally {
      await browser.close();
    }
  });

  /** Recurring below-threshold drops hold the probe. */
  test("the headroom probe does not climb while camera uplink drops recur @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const meetingId = `e2e_ws_stale_aq_recur_${Date.now()}`;
    const uiURL = baseURL || DEFAULT_UI_URL;
    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const viewerCtx = await newViewerContext(
        browser,
        "ws-stale-aq-recur-viewer@videocall.rs",
        uiURL,
      );
      const viewerPage = await viewerCtx.newPage();
      await enterMeetingAsHost(viewerPage, meetingId);

      const ctx = await newPublisherContext(browser, "ws-stale-aq-recur@videocall.rs", uiURL);
      const page = await ctx.newPage();
      const consoleLines = collectConsole(page);
      const timed = collectTimedConsole(page);

      await joinAsGuestAndStartCamera(viewerPage, page, meetingId);
      await assertHookPresent(page);
      await waitForWsElectionAndAqTick(consoleLines);

      // Arm before the diagnostics fetches below.
      const armStart = Date.now();
      while (Date.now() - armStart < HOLD_ARM_MS) {
        await bumpStaleDrops(page, 1);
        await page.waitForTimeout(RECURRING_BUMP_INTERVAL_MS);
      }
      const windowStart = Date.now();

      const effective = timed.map((l) => EFFECTIVE_LAYERS_RE.exec(l.text)?.[1]).find(Boolean);
      expect(
        effective,
        "the publisher did not log `CameraEncoder: effective simulcast layers = 3`, so the " +
          "probe has no layer to add.\n" +
          (await probeDiagnostics(page, timed)),
      ).toBe("3");
      expect(
        timed.filter((l) => PROBE_ADDED_RE.test(l.text)),
        "the probe added a layer before the recurring drops began, so the video layer 1 add " +
          "this test holds already happened and the no-ADDED assertion below is vacuous.\n" +
          (await probeDiagnostics(page, timed)),
      ).toEqual([]);
      let lastBumpAt = windowStart;
      while (Date.now() - windowStart < RECURRING_BUMP_WINDOW_MS) {
        await bumpStaleDrops(page, 1);
        lastBumpAt = Date.now();
        await page.waitForTimeout(RECURRING_BUMP_INTERVAL_MS);
      }
      const windowEnd = lastBumpAt;

      const inWindow = (l: TimedLine) => l.at >= windowStart && l.at <= windowEnd;
      expect(
        timed.filter(
          (l) => l.at >= armStart && l.at <= windowEnd && TIER_STEP_DOWN_RE.test(l.text),
        ),
        "a video tier step-down landed during arming or the window, so gate 8 (not gate 9) held " +
          "the probe and the no-ADDED assertion below proves nothing about the uplink-drop hold.\n" +
          (await probeDiagnostics(page, timed)),
      ).toEqual([]);
      expectNoRealDrops(consoleLines, "after the recurring-drop window");
      expect(
        timed.filter((l) => inWindow(l) && PROBE_ADDED_RE.test(l.text)),
        "the headroom probe climbed into a dropping uplink (issue 2811 a4): an ADDED line landed " +
          "while below-threshold camera WS stale-delta drops were still advancing.\n" +
          (await probeDiagnostics(page, timed)),
      ).toEqual([]);
      expect(
        timed.filter((l) => l.at >= armStart && l.at <= windowEnd && PROBE_HELD_RE.test(l.text))
          .length,
        "no `held by uplink axis activity` line between arming and windowEnd, so gate 9 was never " +
          "the holder and the no-ADDED assertion above proves nothing about it.\n" +
          (await probeDiagnostics(page, timed)),
      ).toBeGreaterThan(0);

      const sawReopen = await waitUntil(
        () => timed.some((l) => l.at > windowEnd && PROBE_ADDED_RE.test(l.text)),
        REOPEN_POLL_MS,
      );
      expect(
        sawReopen,
        `no \`headroom probe ADDED a layer\` line within ${REOPEN_POLL_MS}ms of the last drop; the ` +
          "hold must release once the drops stop, so either the gate wedges or this harness " +
          "cannot add and the in-window hold above proved nothing. real gate " +
          `drops=${timed.filter((l) => GATE_REAL_DROP_RE.test(l.text)).length}\n` +
          (await probeDiagnostics(page, timed)),
      ).toBe(true);
    } finally {
      await browser.close();
    }
  });
});
