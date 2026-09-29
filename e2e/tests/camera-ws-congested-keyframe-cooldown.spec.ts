import { test, expect, chromium } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { joinAndStartCamera } from "../helpers/camera-publisher";
import {
  FORCED_KEYFRAME_LOG,
  PERIODIC_KEYFRAME_LOG,
  TIER_CHANGE_CAUSE,
  TIER_CHANGE_LOG,
} from "../helpers/camera-tier-keyframe-log";
import {
  AQ_SETTLE_MS,
  TimedLine,
  WS_BUFFERED_OVERRIDE_BYTES,
  assertGateHooksPresent,
  bumpWsDrop,
  collectTimedConsole,
  forceCameraKeyframe,
  overrideAndBumpWsDrop,
  setWsBufferedOverride,
  waitForWsCameraAq,
} from "../helpers/camera-ws-gate";
import { CAMERA_AQ_AXES, CAMERA_WS_KEYFRAME_HOLD } from "../helpers/rust-mirrored-constants";
import { setRuntimeLogLevel } from "../helpers/runtime-log-level";
import { waitForServices } from "../helpers/wait-for-services";

const DEFAULT_UI_URL = "http://localhost:3001";

const { AQ_TICK_INTERVAL_MS, WS_SELF_CONGESTION_DROP_THRESHOLD } = CAMERA_AQ_AXES;
const { MIN_TIER_TRANSITION_INTERVAL_MS, CAMERA_WS_CONGESTED_KEYFRAME_COOLDOWN_MS } =
  CAMERA_WS_KEYFRAME_HOLD;

const WS_DROP_BUMP = WS_SELF_CONGESTION_DROP_THRESHOLD + 2;

/** First tick after `t1` that the tier-transition floor lets serve a second step-down. */
const SERVED_TICK_OFFSET_MS =
  Math.ceil(MIN_TIER_TRANSITION_INTERVAL_MS / AQ_TICK_INTERVAL_MS) * AQ_TICK_INTERVAL_MS;

const PLI_OFFSET_MS = 1200;
const BUMP_DEADLINE_OFFSET_MS = SERVED_TICK_OFFSET_MS - 100;
const T2_MIN_OFFSET_MS = SERVED_TICK_OFFSET_MS - 300;
const T2_MAX_OFFSET_MS = SERVED_TICK_OFFSET_MS + 400;
const TIER_KEYFRAME_AFTER_T1_MS = 500;
const PLI_KEYFRAME_WAIT_MS = 500;
const HOLD_CHECK_MS = 600;
const RELEASE_TIMEOUT_MS = 1500;
const CLEAR_ORDER_SLACK_MS = 300;

const isTierChange = (l: TimedLine) => l.text.includes(TIER_CHANGE_LOG);
const isAttributedKeyframe = (l: TimedLine) =>
  l.text.includes(FORCED_KEYFRAME_LOG) && l.text.includes(TIER_CHANGE_CAUSE);
const isPliKeyframe = (l: TimedLine) =>
  l.text.includes(FORCED_KEYFRAME_LOG) && l.text.includes("(PLI");
const isPeriodicKeyframe = (l: TimedLine) => l.text.includes(PERIODIC_KEYFRAME_LOG);

async function waitForLine(
  lines: TimedLine[],
  fromIndex: number,
  pred: (l: TimedLine) => boolean,
  timeout: number,
  message: string,
): Promise<{ line: TimedLine; index: number }> {
  const find = () => lines.findIndex((l, i) => i >= fromIndex && pred(l));
  await expect.poll(find, { timeout, intervals: [25], message }).toBeGreaterThanOrEqual(0);
  const index = find();
  return { line: lines[index], index };
}

test.describe("issue 2834 congested camera tier change keeps the keyframe cooldown", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a tier change under WS congestion holds its keyframe behind the 2 s cooldown @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const meetingId = `e2e_ws_kf_cooldown_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "ws-kf-cooldown@videocall.rs",
        "WsKfCooldownPublisher",
        baseURL || DEFAULT_UI_URL,
      );
      await pinWebSocketTransport(ctx);
      await setRuntimeLogLevel(ctx, "debug");
      const page = await ctx.newPage();
      const lines = collectTimedConsole(page);

      await joinAndStartCamera(page, meetingId);
      await assertGateHooksPresent(page, [
        "setWsBufferedOverride",
        "forceCameraKeyframe",
        "bumpWsDrop",
      ]);
      await waitForWsCameraAq(lines);
      await page.waitForTimeout(AQ_SETTLE_MS);

      const beforeFirstBump = lines.length;
      await bumpWsDrop(page, WS_DROP_BUMP);
      const first = await waitForLine(
        lines,
        beforeFirstBump,
        isTierChange,
        5_000,
        "the WS-drop axis never stepped the camera tier down, so there is no first tier change.",
      );
      const t1 = first.line.at;
      const firstKf = await waitForLine(
        lines,
        first.index + 1,
        isAttributedKeyframe,
        TIER_KEYFRAME_AFTER_T1_MS * 2,
        "precondition: the uncongested tier change emitted no attributed keyframe.",
      );
      expect(
        firstKf.line.at - t1,
        "precondition: the uncongested tier change's keyframe was not emitted promptly.",
      ).toBeLessThanOrEqual(TIER_KEYFRAME_AFTER_T1_MS);

      await page.waitForTimeout(Math.max(0, t1 + PLI_OFFSET_MS - Date.now()));
      const beforePli = lines.length;
      await forceCameraKeyframe(page);
      const pli = await waitForLine(
        lines,
        beforePli,
        isPliKeyframe,
        PLI_KEYFRAME_WAIT_MS,
        "precondition: the PLI keyframe was not emitted while the override was still clear.",
      );
      const tPli = pli.line.at;

      await overrideAndBumpWsDrop(page, WS_BUFFERED_OVERRIDE_BYTES, WS_DROP_BUMP);
      const bumpAt = Date.now();
      expect(
        bumpAt - t1,
        "harness timing: the second bump landed too late to be served on the target AQ tick.",
      ).toBeLessThan(BUMP_DEADLINE_OFFSET_MS);

      const second = await waitForLine(
        lines,
        first.index + 1,
        isTierChange,
        3_000,
        "the congested WS-drop bump never produced a second tier change.",
      );
      const t2 = second.line.at;
      const gap = t2 - t1;
      expect(
        gap >= T2_MIN_OFFSET_MS && gap <= T2_MAX_OFFSET_MS,
        `harness timing: second tier change ${gap} ms after the first missed the target tick ` +
          `window [${T2_MIN_OFFSET_MS}, ${T2_MAX_OFFSET_MS}]; not a product failure.`,
      ).toBe(true);
      expect(
        tPli + CAMERA_WS_CONGESTED_KEYFRAME_COOLDOWN_MS > t2 + HOLD_CHECK_MS,
        "harness timing: the PLI keyframe is too early for its cooldown to cover the hold window.",
      ).toBe(true);

      await page.waitForTimeout(Math.max(0, t2 + HOLD_CHECK_MS - Date.now()));
      const early = lines.filter(
        (l, i) => i > second.index && l.at <= t2 + HOLD_CHECK_MS && isAttributedKeyframe(l),
      );
      expect(
        early.map((l) => `+${l.at - t2}ms ${l.text}`),
        "a tier change made while bufferedAmount exceeded the gate threshold reset the keyframe " +
          `cooldown: its keyframe landed inside ${CAMERA_WS_CONGESTED_KEYFRAME_COOLDOWN_MS} ms of ` +
          "the PLI keyframe.",
      ).toEqual([]);

      await setWsBufferedOverride(page, null);
      const clearedAt = Date.now();
      const afterT2 = () => {
        const next = lines.findIndex((l, i) => i > second.index && isTierChange(l));
        return lines.filter((_, i) => i > second.index && (next === -1 || i < next));
      };
      await expect
        .poll(
          () =>
            afterT2().some(isAttributedKeyframe) ||
            afterT2().some(
              (l) => isPeriodicKeyframe(l) && l.at <= clearedAt + CLEAR_ORDER_SLACK_MS,
            ),
          {
            timeout: RELEASE_TIMEOUT_MS,
            intervals: [25],
            message:
              "clearing the override released no tier-change keyframe and no periodic keyframe " +
              "had satisfied it in the meantime, so the congested tier change never requested one.",
          },
        )
        .toBe(true);
    } finally {
      await browser.close();
    }
  });
});
