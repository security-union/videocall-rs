import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import {
  BROWSER_ARGS,
  CERT_HASH_INIT_SCRIPT,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { joinMeetingFromPage } from "../helpers/two-user-meeting";
import { setRuntimeConfig } from "../helpers/runtime-config";
import { waitForServices } from "../helpers/wait-for-services";
import { createMeeting, endMeeting, leaveMeeting } from "../helpers/meeting-api";
import { readStaleInstanceRefusedTotal } from "../helpers/relay-metrics";
import { installMediaSocketRecorder, severMediaWebSocket } from "../helpers/media-socket-sever";

/**
 * A participant disconnected while the meeting ended and was restarted
 * (new instance, waiting room ON) must not silently re-enter the grid on its
 * cached room token. The relay refuses the stale token with
 * MEETING_INSTANCE_SUPERSEDED; the UI shows the "Meeting Restarted" overlay,
 * and Rejoin re-runs the normal join flow, which lands in the waiting room.
 */

const HOST_EMAIL = "restart-token-host@videocall.rs";
const HOST_NAME = "RestartTokenHost";
const PEER_EMAIL = "restart-token-peer@videocall.rs";
const PEER_NAME = "RestartTokenPeer";

const REMOTE_TILE = '[id^="peer-video-"][id$="-div"]';
const OVERLAY = '[data-testid="meeting-restarted-overlay"]';
const REJOIN_BTN = "#meeting-restarted-rejoin-btn";
const HOME_BTN = "#meeting-restarted-home-btn";
const HOST_COUNT = '[data-testid="meeting-footer-participants"] .meeting-footer-count-text';
const RESTARTED_MESSAGE =
  "This meeting ended and a new one started while you were disconnected. Rejoin to continue — you may need to be admitted again.";

const NO_LONGER_MESSAGE =
  "You are no longer in this meeting. Rejoin to continue — you may need to be admitted again.";

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await fillAndSubmitJoinForm(page, meetingId, username);
  await page.waitForTimeout(1500);
}

async function joinFromPage(page: Page): Promise<"in-meeting" | "waiting"> {
  const result = await joinMeetingFromPage(page);
  if (result === "waiting-for-meeting")
    throw new Error("unexpected 'Waiting for meeting to start'");
  return result;
}

/** The relay can be mid-restart (cargo-watch rebuild in the dev stack), so retry the scrape. */
async function refusedTotal(): Promise<number> {
  let last: unknown;
  for (let i = 0; i < 10; i++) {
    try {
      return await readStaleInstanceRefusedTotal("websocket");
    } catch (e) {
      last = e;
      await new Promise((r) => setTimeout(r, 1_000));
    }
  }
  throw last;
}

async function enterAsGuest(page: Page, meetingId: string, name: string): Promise<void> {
  await page.goto(`/meeting/${meetingId}/guest`);
  const nameField = page.locator("#guest-name");
  await expect(nameField).toBeVisible({ timeout: 20_000 });
  await nameField.fill(name);
  await nameField.press("Enter");
  await page.waitForTimeout(1500);
}

async function admitFromHost(hostPage: Page): Promise<void> {
  const admitButton = hostPage.getByTitle("Admit").first();
  await expect(admitButton).toBeVisible({ timeout: 30_000 });
  await hostPage.waitForTimeout(1000);
  await admitButton.dispatchEvent("click");
}

async function enterGridAfterAdmit(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Join Meeting|Start Meeting/ });
  const grid = page.locator("#grid-container");
  await expect
    .poll(
      async () => {
        if (await grid.isVisible()) return true;
        if (await joinButton.isVisible()) {
          await joinButton.click({ timeout: 5_000 }).catch(() => {});
        }
        return grid.isVisible();
      },
      { timeout: 60_000, intervals: [1_000] },
    )
    .toBe(true);
}

interface Scenario {
  hostPage: Page;
  peerPage: Page;
  /** Relay stale-refusal counter before the peer's held socket was released. */
  refusedBefore: number;
  /** Let new media sockets through (already-held ones stay pending). */
  stopHoldingSockets?: () => void;
  /** Peer /status requests that passed the route after the held one was released. */
  statusPollsAfterRelease?: () => number;
  closeAll: () => Promise<void>;
}

/**
 * Safety bound on one held reconnect socket; must stay under the client connect
 * timeout. The scenario releases the socket only after the host's restart has
 * committed and fails loudly if this bound was hit first.
 */
const HOLD_CAP_MS = 28_000;
const CLIENT_WS_CONNECT_TIMEOUT_MS = 30_000;

/**
 * Host + peer in a waiting-room meeting. The peer's media socket is severed and
 * its reconnect socket is HELD pending, while the host ends and restarts the meeting.
 * Releasing the held socket makes the peer present that now-stale token to the
 * relay. Leaves the peer facing whatever the relay did with it.
 */
async function restartWhilePeerReconnectIsHeld(
  baseURL: string,
  tag: string,
  asGuest = false,
  mode: "restart" | "left" = "restart",
): Promise<Scenario> {
  const meetingId = `e2e_restart_token_${tag}_${Date.now()}`;
  await createMeeting(HOST_EMAIL, HOST_NAME, {
    meetingId,
    waitingRoomEnabled: true,
    allowGuests: asGuest,
  });

  const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
  const peerBrowser = await chromium.launch({ args: BROWSER_ARGS });
  const closeAll = async () => {
    await hostBrowser.close();
    await peerBrowser.close();
  };

  try {
    const hostCtx = await createAuthenticatedContext(hostBrowser, HOST_EMAIL, HOST_NAME, baseURL);
    await pinWebSocketTransport(hostCtx);
    let peerCtx: BrowserContext;
    if (asGuest) {
      peerCtx = await peerBrowser.newContext({ baseURL, ignoreHTTPSErrors: true });
      await peerCtx.addInitScript(CERT_HASH_INIT_SCRIPT);
    } else {
      peerCtx = await createAuthenticatedContext(peerBrowser, PEER_EMAIL, PEER_NAME, baseURL);
    }
    await pinWebSocketTransport(peerCtx);
    await setRuntimeConfig(peerCtx, {
      webTransportEnabled: "false",
      webSocketConnectTimeoutMs: CLIENT_WS_CONNECT_TIMEOUT_MS,
    });
    await installMediaSocketRecorder(peerCtx);

    let holding = false;
    let held = 0;
    let capExpired = false;
    let release!: () => void;
    const released = new Promise<void>((resolve) => {
      release = resolve;
    });
    await peerCtx.routeWebSocket(/\/lobby/, async (ws) => {
      if (holding) {
        held += 1;
        if (mode === "left") {
          await released;
        } else {
          await Promise.race([
            released,
            new Promise<void>((resolve) =>
              setTimeout(() => {
                capExpired = true;
                resolve();
              }, HOLD_CAP_MS),
            ),
          ]);
        }
      }
      ws.connectToServer();
    });

    let holdingStatus = false;
    let statusHeld = 0;
    let statusReleased = false;
    let statusPollsAfterRelease = 0;
    let releaseStatus!: () => void;
    const statusGate = new Promise<void>((resolve) => {
      releaseStatus = resolve;
    });
    await peerCtx.route(/\/api\/v1\/meetings\/[^/]+\/status/, async (route) => {
      if (holdingStatus) {
        statusHeld += 1;
        await statusGate;
      } else if (statusReleased) {
        statusPollsAfterRelease += 1;
      }
      await route.continue();
    });

    const hostPage = await hostCtx.newPage();
    const peerPage = await peerCtx.newPage();

    await navigateToMeeting(hostPage, meetingId, HOST_NAME);
    expect(await joinFromPage(hostPage)).toBe("in-meeting");
    if (asGuest) {
      await enterAsGuest(peerPage, meetingId, PEER_NAME);
    } else {
      await navigateToMeeting(peerPage, meetingId, PEER_NAME);
    }
    expect(await joinFromPage(peerPage)).toBe("waiting");
    await admitFromHost(hostPage);
    await enterGridAfterAdmit(peerPage);

    // Sever the peer's media socket and hold its reconnect socket open-pending.
    // The connection must be fully established (the host's tile is rendered) before
    // the sever, or the client treats the drop as a failed handshake and never reconnects.
    await expect(peerPage.locator(REMOTE_TILE).first()).toBeVisible({ timeout: 30_000 });
    await expect
      .poll(
        () =>
          peerPage.evaluate(() => {
            const sockets =
              (window as unknown as Record<string, WebSocket[] | undefined>)[
                "__e2269_media_sockets"
              ] ?? [];
            return sockets.some((ws) => ws.readyState === 1 && ws.url.includes("/lobby"));
          }),
        { timeout: 30_000, message: "the peer's media WebSocket never reached OPEN" },
      )
      .toBe(true);
    holding = true;
    holdingStatus = mode === "left";
    const severed = await severMediaWebSocket(peerPage);
    expect(
      severed.severed,
      "the peer's media WebSocket must have been open to sever",
    ).toBeGreaterThanOrEqual(1);
    if (mode === "left") {
      await expect
        .poll(() => statusHeld, {
          timeout: 90_000,
          message: "no /status request reached the route after the media socket was severed",
        })
        .toBeGreaterThanOrEqual(1);
      // NOT_IN_MEETING means the row was already left.
      await leaveMeeting(PEER_EMAIL, PEER_NAME, meetingId).catch((e: Error) => {
        if (!/NOT_IN_MEETING/.test(e.message)) throw e;
      });
      holdingStatus = false;
      statusReleased = true;
      releaseStatus();
      return {
        hostPage,
        peerPage,
        refusedBefore: 0,
        statusPollsAfterRelease: () => statusPollsAfterRelease,
        stopHoldingSockets: () => {
          holding = false;
        },
        closeAll,
      };
    }
    await expect.poll(() => held, { timeout: 15_000 }).toBeGreaterThanOrEqual(1);

    await endMeeting(HOST_EMAIL, HOST_NAME, meetingId);
    await navigateToMeeting(hostPage, meetingId, HOST_NAME);
    expect(await joinFromPage(hostPage)).toBe("in-meeting");

    expect(
      capExpired,
      "the hold cap expired before the restart committed; the relay never saw the stale token after it",
    ).toBe(false);
    const refusedBefore = await refusedTotal();
    release();
    return { hostPage, peerPage, refusedBefore, closeAll };
  } catch (e) {
    await closeAll();
    throw e;
  }
}

/** Overlay shown, relay refused and the host never saw the peer, then Rejoin -> waiting room -> admit -> grid. */
async function expectOverlayThenRejoinThroughWaitingRoom(s: Scenario): Promise<void> {
  const { peerPage, hostPage } = s;
  await expect(peerPage.locator(OVERLAY)).toBeVisible({ timeout: 60_000 });
  await expect(peerPage.locator(OVERLAY)).toContainText("Meeting Restarted");
  await expect(peerPage.locator(OVERLAY)).toContainText(RESTARTED_MESSAGE);
  await expect(peerPage.locator(REJOIN_BTN)).toBeVisible();
  await expect(peerPage.locator(REJOIN_BTN)).toBeFocused();
  await expect(peerPage.locator(HOME_BTN)).toBeVisible();

  await expect
    .poll(async () => (await refusedTotal()) - s.refusedBefore, {
      message: "the relay must have refused the stale room token",
      timeout: 15_000,
    })
    .toBeGreaterThanOrEqual(1);

  // Stable negative: the refused peer never reaches the host's grid.
  const stableUntil = Date.now() + 5_000;
  while (Date.now() < stableUntil) {
    expect(await hostPage.locator(REMOTE_TILE).count(), "host must see no remote tile").toBe(0);
    await expect(hostPage.locator(HOST_COUNT)).toHaveText("1 participant", { timeout: 1_000 });
    await hostPage.waitForTimeout(500);
  }

  await peerPage.locator(REJOIN_BTN).click();
  await expect(peerPage.getByText("Waiting to be admitted")).toBeVisible({ timeout: 30_000 });
  await expect(peerPage.locator("#grid-container")).toHaveCount(0);
  await expect(peerPage.locator(OVERLAY)).toHaveCount(0);

  await admitFromHost(hostPage);
  await enterGridAfterAdmit(peerPage);
  await expect(hostPage.locator(REMOTE_TILE).first()).toBeVisible({ timeout: 30_000 });
  await expect(hostPage.locator(HOST_COUNT)).toHaveText("2 participants", { timeout: 30_000 });
}

test.describe("Stale room token after meeting restart (#3287)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("reconnecting after a restart shows the overlay; Rejoin lands in the waiting room, not the grid @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const s = await restartWhilePeerReconnectIsHeld(baseURL || "http://localhost:3001", "rejoin");
    try {
      await expectOverlayThenRejoinThroughWaitingRoom(s);
    } finally {
      await s.closeAll();
    }
  });

  test("a guest reconnecting after a restart sees the overlay; Rejoin goes through the guest waiting room", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const s = await restartWhilePeerReconnectIsHeld(
      baseURL || "http://localhost:3001",
      "guest",
      true,
    );
    try {
      await expectOverlayThenRejoinThroughWaitingRoom(s);
    } finally {
      await s.closeAll();
    }
  });

  test("a reconnect that finds the user's row left shows the disconnected overlay; Rejoin goes through the join flow", async ({
    baseURL,
  }) => {
    test.setTimeout(420_000);
    const s = await restartWhilePeerReconnectIsHeld(
      baseURL || "http://localhost:3001",
      "left",
      false,
      "left",
    );
    try {
      const { peerPage, hostPage } = s;
      // The `left` answer must be retried, not acted on at once: the overlay stays hidden
      // through several further polls.
      await expect
        .poll(
          async () => {
            expect(await peerPage.locator(OVERLAY).count(), "overlay shown too early").toBe(0);
            return s.statusPollsAfterRelease?.() ?? 0;
          },
          { timeout: 120_000, intervals: [500] },
        )
        .toBeGreaterThanOrEqual(3);
      await expect(peerPage.locator(OVERLAY)).toBeVisible({ timeout: 200_000 });
      await expect(peerPage.locator(OVERLAY)).toContainText("Disconnected From Meeting");
      await expect(peerPage.locator(OVERLAY)).toContainText(NO_LONGER_MESSAGE);
      await expect(peerPage.locator(REJOIN_BTN)).toBeFocused();

      s.stopHoldingSockets?.();
      await peerPage.locator(REJOIN_BTN).click();
      await expect(peerPage.locator(OVERLAY)).toHaveCount(0, { timeout: 30_000 });
      const waiting = peerPage.getByText("Waiting to be admitted");
      const joinBtn = peerPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
      await expect(waiting.or(joinBtn).first()).toBeVisible({ timeout: 30_000 });
      if (await waiting.isVisible()) await admitFromHost(hostPage);
      await enterGridAfterAdmit(peerPage);
      await expect(hostPage.locator(REMOTE_TILE).first()).toBeVisible({ timeout: 30_000 });
      await expect(hostPage.locator(HOST_COUNT)).toHaveText("2 participants", {
        timeout: 30_000,
      });
    } finally {
      await s.closeAll();
    }
  });

  test("Home on the restart overlay leaves the meeting", async ({ baseURL }) => {
    test.setTimeout(240_000);
    const s = await restartWhilePeerReconnectIsHeld(baseURL || "http://localhost:3001", "home");
    try {
      const { peerPage } = s;
      await expect(peerPage.locator(OVERLAY)).toBeVisible({ timeout: 60_000 });
      await peerPage.locator(HOME_BTN).click();
      await expect(peerPage).not.toHaveURL(/\/meeting\//, { timeout: 15_000 });
      await expect(peerPage.locator("#meeting-id")).toBeVisible({ timeout: 15_000 });
    } finally {
      await s.closeAll();
    }
  });
});
