import { test, expect, chromium, Page } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { enableCamera } from "../helpers/controls";
import {
  AQ_SETTLE_MS,
  GATE_REAL_DROP_RE,
  WS_BUFFERED_OVERRIDE_BYTES,
  assertGateHooksPresent,
  collectTimedConsoleFromContext,
  gateDropLines,
  setWsBufferedOverride,
  waitForWsCameraAq,
} from "../helpers/camera-ws-gate";
import { samplePeerVideoChecksum } from "../helpers/frame-liveness";
import {
  MeetingMember,
  admitGuestIfNeeded,
  clickJoinAndEnterGrid,
  joinMeetingAs,
} from "../helpers/screen-share-meeting";
import { waitForServices } from "../helpers/wait-for-services";

const DEFAULT_UI_URL = "http://localhost:3001";

const BASELINE_QUIET_MS = 2_000;
const IN_FLIGHT_DRAIN_MS = 1_000;
const KEYFRAME_REACH_TIMEOUT_MS = 15_000;
const POST_CLEAR_THROTTLE_MS = 1_500;
const POST_CLEAR_QUIET_MS = 3_000;

test.describe("issue 2834 camera WS freshness gate driven by the bufferedAmount override", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("override drops camera deltas, keyframes still reach the viewer, clearing stops drops @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ws_gate_override_${Date.now()}`;
    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];
    try {
      const profiles = [
        { email: "ws-gate-host@videocall.rs", name: "WsGateHost" },
        { email: "ws-gate-guest@videocall.rs", name: "WsGateGuest" },
      ];
      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        await pinWebSocketTransport(ctx);
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      const hostLines = collectTimedConsoleFromContext(members[0].context);
      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      const hostPage = members[0].page;
      await clickJoinAndEnterGrid(hostPage);

      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      const guestPage = members[1].page;
      await admitGuestIfNeeded(hostPage, guestPage);

      await enableCamera(hostPage);
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      await waitForWsCameraAq(hostLines);
      await assertGateHooksPresent(hostPage, ["setWsBufferedOverride"]);
      await hostPage.waitForTimeout(AQ_SETTLE_MS);
      const settledDrops = gateDropLines(hostLines).length;
      await hostPage.waitForTimeout(BASELINE_QUIET_MS);

      expect(
        gateDropLines(hostLines)
          .slice(settledDrops)
          .map((l) => l.text),
        "the gate dropped camera deltas before the override was set, so the real bufferedAmount " +
          "already exceeds the threshold and the drops below cannot be attributed to the override.",
      ).toEqual([]);

      await setWsBufferedOverride(hostPage, WS_BUFFERED_OVERRIDE_BYTES);
      await expect
        .poll(() => gateDropLines(hostLines).length, {
          timeout: 10_000,
          intervals: [250, 500],
          message:
            `bufferedAmount is overridden to ${WS_BUFFERED_OVERRIDE_BYTES} B but the camera WS ` +
            "freshness gate never logged a dropped delta, so the encode loop does not consult " +
            "`camera_ws_send_decision`.",
        })
        .toBeGreaterThan(settledDrops);
      const buffered = Number(
        GATE_REAL_DROP_RE.exec(gateDropLines(hostLines)[settledDrops].text)![1],
      );
      expect(buffered, "the gate read a depth other than the override").toBe(
        WS_BUFFERED_OVERRIDE_BYTES,
      );

      await guestPage.waitForTimeout(IN_FLIGHT_DRAIN_MS);
      const baseline = await samplePeerVideoChecksum(guestPage);
      expect(baseline, "the guest has no renderable host video tile").not.toBeNull();
      await expect
        .poll(
          async () => {
            const sample = await samplePeerVideoChecksum(guestPage);
            return sample !== null && sample !== baseline;
          },
          {
            timeout: KEYFRAME_REACH_TIMEOUT_MS,
            intervals: [250, 500],
            message:
              "the host tile never repainted while every camera delta was dropped, so keyframes " +
              "are not passing the gate.",
          },
        )
        .toBe(true);

      await setWsBufferedOverride(hostPage, null);
      await hostPage.waitForTimeout(POST_CLEAR_THROTTLE_MS);
      const afterClear = gateDropLines(hostLines).length;
      await hostPage.waitForTimeout(POST_CLEAR_QUIET_MS);
      expect(
        gateDropLines(hostLines).length,
        "the gate kept dropping after the override was cleared, so `null` did not restore the " +
          "real bufferedAmount read.",
      ).toBe(afterClear);
    } finally {
      for (const m of members) {
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });
});
