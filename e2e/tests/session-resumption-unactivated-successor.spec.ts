import { test, expect, chromium } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { setRuntimeConfig } from "../helpers/runtime-config";
import { waitForServices } from "../helpers/wait-for-services";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { relayReconnectGraceMs } from "../helpers/relay-constants";
import {
  PEER_TILE,
  connSeq,
  holdReconnectCandidate,
  instanceId,
  peerTileIds,
  recordedToasts,
  startToastRecorder,
} from "../helpers/resume-harness";

/**
 * #3181: active session A drops, the same tab reconnects as B, and the relay
 * publishes PARTICIPANT_SESSION_RESUMED A->B while B is still Testing. B then
 * drops before ActivateConnection; the host must still be told the peer left.
 *
 * WebSocket only: `routeWebSocket` cannot intercept WebTransport.
 */

const HOST_EMAIL = "resume-unactivated-host@videocall.rs";
const PEER_EMAIL = "resume-unactivated-peer@videocall.rs";
const HOST_NAME = "HostUser";
const PEER_NAME = "GuestUser";

const LOBBY_SOCKET = /\/lobby/;
const RECONNECT_GRACE_MS = relayReconnectGraceMs();
const LEFT_BUDGET_MS = RECONNECT_GRACE_MS + 10_000;
const RESUMED_BUDGET_MS = 15_000;

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

test.describe("A resumed session dropped before activation is announced as a departure (#3181)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("the host gets a departure notice when the unactivated successor's grace expires @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3181_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const peerBrowser = await chromium.launch({ args: BROWSER_ARGS });
    let releaseHeld = () => {};

    try {
      const hostCtx = await createAuthenticatedContext(hostBrowser, HOST_EMAIL, HOST_NAME, uiURL);
      const peerCtx = await createAuthenticatedContext(peerBrowser, PEER_EMAIL, PEER_NAME, uiURL);
      await pinWebSocketTransport(hostCtx);
      await pinWebSocketTransport(peerCtx);
      await setRuntimeConfig(peerCtx, { webTransportEnabled: "false" });
      await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      await peerCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

      const hold = await holdReconnectCandidate(peerCtx, LOBBY_SOCKET);
      releaseHeld = () => hold.release();

      const hostPage = await hostCtx.newPage();
      const peerPage = await peerCtx.newPage();
      const hostConsole: string[] = [];
      hostPage.on("console", (msg) => hostConsole.push(msg.text()));

      await enterTwoUserMeeting(hostPage, peerPage, meetingId);

      await expect(hostPage.locator(PEER_TILE), "the host must hold one remote tile").toHaveCount(
        1,
        { timeout: 30_000 },
      );
      const [tileA] = await peerTileIds(hostPage);
      const sessionA = /^peer-video-(\d+)-div$/.exec(tileA)?.[1];
      expect(sessionA, `unexpected tile id ${tileA}`).toBeTruthy();
      await expect(hostPage.locator(`#${tileA} .floating-name-text`)).toHaveText(PEER_NAME, {
        timeout: 15_000,
      });
      await expect(hostPage.locator(".peer-toast")).toHaveCount(0, { timeout: 20_000 });
      await startToastRecorder(hostPage);

      const candidate = await hold.sever();
      expect(instanceId(candidate.url)).toBeTruthy();
      expect(
        instanceId(candidate.url),
        "the candidate must reuse the departed session's instance_id",
      ).toBe(instanceId(candidate.severedUrl));
      const severedSeq = connSeq(candidate.severedUrl);
      const candidateSeq = connSeq(candidate.url);
      expect(
        severedSeq,
        `the severed URL carries no numeric conn_seq: ${candidate.severedUrl}`,
      ).toMatch(/^\d+$/);
      expect(
        candidateSeq,
        `the candidate URL carries no numeric conn_seq: ${candidate.url}`,
      ).toMatch(/^\d+$/);
      expect(
        BigInt(candidateSeq!) > BigInt(severedSeq!),
        `the candidate's conn_seq ${candidateSeq} must exceed the severed session's ${severedSeq}`,
      ).toBe(true);

      const resumedRe = new RegExp(
        `Session resumed for ${escapeRegExp(PEER_EMAIL)}: ${sessionA} -> (\\d+)`,
      );
      await expect
        .poll(() => hostConsole.some((line) => resumedRe.test(line)), {
          timeout: RESUMED_BUDGET_MS,
          intervals: [100],
          message: `the host never received PARTICIPANT_SESSION_RESUMED ${sessionA} -> B`,
        })
        .toBe(true);
      const sessionB = resumedRe.exec(hostConsole.find((l) => resumedRe.test(l))!)![1];
      expect(sessionB).not.toBe(sessionA);
      await expect(hostPage.locator(`#${tileA}`), "adopt_resumed_session retires A").toHaveCount(0);

      const peerLeft = `Peer left: ${PEER_EMAIL}`;
      expect(
        hostConsole.filter((line) => line.includes(peerLeft)),
        "A's departure was resumed, so no LEFT may have reached the host yet",
      ).toEqual([]);
      expect(await recordedToasts(hostPage), "no toast may mark an in-grace resume").toEqual([]);

      await candidate.drop();
      const droppedAt = Date.now();

      let leftAfterMs: number | null = null;
      while (Date.now() < droppedAt + LEFT_BUDGET_MS) {
        if (hostConsole.some((line) => line.includes(peerLeft))) {
          leftAfterMs = Date.now() - droppedAt;
          break;
        }
        await hostPage.waitForTimeout(100);
      }
      expect(
        leftAfterMs,
        `no PARTICIPANT_LEFT for resumed session ${sessionB} within ${LEFT_BUDGET_MS}ms of its drop`,
      ).not.toBeNull();
      expect(
        leftAfterMs!,
        "LEFT must come from B's grace expiry, not an immediate path",
      ).toBeGreaterThanOrEqual(RECONNECT_GRACE_MS - 500);
      await expect
        .poll(
          async () =>
            ((await recordedToasts(hostPage)) ?? []).filter((t) => t.includes("left the meeting")),
          { timeout: 2_000, intervals: [100], message: "the host must announce the departure" },
        )
        .toHaveLength(1);

      hold.release();

      await expect
        .poll(async () => (await peerTileIds(hostPage)).length, {
          timeout: 45_000,
          intervals: [500],
          message: "the peer never rejoined after its held reconnects were released",
        })
        .toBe(1);
    } finally {
      releaseHeld();
      await hostBrowser.close();
      await peerBrowser.close();
    }
  });
});
