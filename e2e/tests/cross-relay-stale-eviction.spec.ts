import { test, expect, chromium } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { setRuntimeConfig } from "../helpers/runtime-config";
import { waitForServices } from "../helpers/wait-for-services";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { installMediaSocketRecorder, liveMediaSocketUrls } from "../helpers/media-socket-sever";
import {
  PEER_TILE,
  connSeq,
  instanceId,
  peerTileIds,
  recordedToasts,
  startToastRecorder,
} from "../helpers/resume-harness";
import { publishEvictInstance, randomU64, roomTokenClaims } from "../helpers/evict-instance";

/**
 * #3285: another relay's eviction carrying a conn_seq no newer than the local
 * holder's is ignored; a strictly newer one still evicts.
 */

const HOST_EMAIL = "stale-evict-host@videocall.rs";
const PEER_EMAIL = "stale-evict-peer@videocall.rs";
const HOST_NAME = "HostUser";
const PEER_NAME = "GuestUser";

const SURVIVAL_WINDOW_MS = 3_000;
const RESUMED_BUDGET_MS = 15_000;

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

test.describe("A stale cross-relay eviction does not evict the newer session (#3285)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("an equal conn_seq eviction is ignored and a newer one evicts @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3285_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const peerBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(hostBrowser, HOST_EMAIL, HOST_NAME, uiURL);
      const peerCtx = await createAuthenticatedContext(peerBrowser, PEER_EMAIL, PEER_NAME, uiURL);
      await pinWebSocketTransport(hostCtx);
      await pinWebSocketTransport(peerCtx);
      await setRuntimeConfig(peerCtx, { webTransportEnabled: "false" });
      await installMediaSocketRecorder(peerCtx);
      await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      await peerCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

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

      const joinedUrls = async () =>
        (await liveMediaSocketUrls(peerPage)).filter((url) => roomTokenClaims(url).joined);
      await expect
        .poll(joinedUrls, {
          timeout: 15_000,
          intervals: [250],
          message: "the peer must settle on exactly one joined media socket",
        })
        .toHaveLength(1);
      const [peerUrl] = await joinedUrls();
      const iid = instanceId(peerUrl);
      expect(iid, `the peer's connect URL carries no instance_id: ${peerUrl}`).toBeTruthy();
      const seqRaw = connSeq(peerUrl);
      expect
        .soft(seqRaw, `the peer's connect URL carries no numeric conn_seq: ${peerUrl}`)
        .toMatch(/^\d+$/);
      const seq = BigInt(seqRaw ?? "0");
      const { room, sub } = roomTokenClaims(peerUrl);

      const forgedEvict = (newSessionId: bigint, newConnSeq: bigint) =>
        publishEvictInstance({
          instanceId: iid!,
          room,
          userId: sub,
          newSessionId,
          originRelay: randomU64(),
          newConnSeq,
        });
      const resumedRe = new RegExp(
        `Session resumed for ${escapeRegExp(sub)}: ${sessionA} -> (\\d+)`,
      );
      const peerLeft = `Peer left: ${sub}`;

      let staleSession = randomU64();
      while (String(staleSession) === sessionA) staleSession = randomU64();
      await forgedEvict(staleSession, seq);
      await hostPage.waitForTimeout(SURVIVAL_WINDOW_MS);

      expect(
        hostConsole.filter((line) => resumedRe.test(line)),
        `an eviction at the holder's own conn_seq ${seq} must not re-key ${sessionA}`,
      ).toEqual([]);
      expect(
        hostConsole.filter((line) => line.includes(peerLeft)),
        "an ignored eviction must not announce a departure",
      ).toEqual([]);
      await expect(hostPage.locator(`#${tileA}`), "the peer's tile must survive").toHaveCount(1);
      expect(await recordedToasts(hostPage), "an ignored eviction must raise no toast").toEqual([]);
      expect(
        await joinedUrls(),
        "the peer must keep its one joined media socket, with no reconnect",
      ).toEqual([peerUrl]);

      let newerSession = randomU64();
      while (String(newerSession) === sessionA || newerSession === staleSession) {
        newerSession = randomU64();
      }
      await forgedEvict(newerSession, seq + 1n);
      const evicted = `Session resumed for ${sub}: ${sessionA} -> ${newerSession}`;
      await expect
        .poll(() => hostConsole.some((line) => line.includes(evicted)), {
          timeout: RESUMED_BUDGET_MS,
          intervals: [100],
          message: `a conn_seq ${seq + 1n} eviction must re-key ${sessionA} -> ${newerSession} on the host`,
        })
        .toBe(true);
    } finally {
      await hostBrowser.close();
      await peerBrowser.close();
    }
  });
});
