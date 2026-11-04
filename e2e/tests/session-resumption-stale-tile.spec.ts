import { test, expect, chromium, Page, WebSocketRoute } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { setRuntimeConfig } from "../helpers/runtime-config";
import { waitForServices } from "../helpers/wait-for-services";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { installMediaSocketRecorder, severMediaWebSocket } from "../helpers/media-socket-sever";
import {
  CARD_TITLE,
  cardLog,
  installConnectionCardRecorder,
  phases,
  settledCardLog,
} from "../helpers/connection-status-card";
import { connSeq, instanceId } from "../helpers/resume-harness";

const HOST_EMAIL = "resume-host@videocall.rs";
const PEER_EMAIL = "resume-peer@videocall.rs";
const HOST_NAME = "HostUser";
const PEER_NAME = "GuestUser";

const PEER_TILE = '[id^="peer-video-"][id$="-div"]';

// Milliseconds, bracketed by the ~1s reconnect and the ~15s watchdog floor.
const STALE_TILE_BUDGET_MS = 8_000;

const JOIN_LEAVE_TOAST_RE = /joined the meeting|left the meeting/;

const LOBBY_SOCKET = /\/lobby/;
const RECONNECT_GRACE_MS = 3_000;
const LEFT_BUDGET_MS = RECONNECT_GRACE_MS + 3_000;
const JOIN_ROOM_SETTLE_MS = 1_000;

interface ToastWindow {
  __e2269_toasts?: string[];
}

async function startToastRecorder(page: Page): Promise<void> {
  await page.evaluate(() => {
    const seen: string[] = [];
    (window as unknown as ToastWindow).__e2269_toasts = seen;
    const capture = () => {
      document.querySelectorAll(".peer-toast .toast-action").forEach((el) => {
        const text = (el.textContent || "").trim();
        if (text && !seen.includes(text)) {
          seen.push(text);
        }
      });
    };
    capture();
    new MutationObserver(capture).observe(document.body, { childList: true, subtree: true });
  });
}

// `null`, not `[]`, when the recorder was never installed: defaulted to `[]`, an
// uninstalled recorder and a quiet reconnect would be indistinguishable.
async function recordedToasts(page: Page): Promise<string[] | null> {
  return page.evaluate(() => (window as unknown as ToastWindow).__e2269_toasts ?? null);
}

async function peerTileIds(page: Page): Promise<string[]> {
  return page.locator(PEER_TILE).evaluateAll((els) => els.map((el) => el.id));
}

test.describe("Session resumption across a transport reconnect (#2269)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("an in-grace reconnect retires the peer's stale tile and carries its display name", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_resume_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const peerBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(hostBrowser, HOST_EMAIL, HOST_NAME, uiURL);
      const peerCtx = await createAuthenticatedContext(peerBrowser, PEER_EMAIL, PEER_NAME, uiURL);
      await pinWebSocketTransport(hostCtx);
      await pinWebSocketTransport(peerCtx);
      // Camera defaults OFF in E2E; without this seed neither side publishes
      // video and every tile assertion below would be vacuous.
      await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      await peerCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      await installMediaSocketRecorder(peerCtx);
      await installConnectionCardRecorder(peerCtx);

      const hostPage = await hostCtx.newPage();
      const peerPage = await peerCtx.newPage();
      const hostConsole: string[] = [];
      hostPage.on("console", (msg) => hostConsole.push(msg.text()));

      await enterTwoUserMeeting(hostPage, peerPage, meetingId);

      const hostTiles = hostPage.locator(PEER_TILE);
      await expect(hostTiles, "the host must hold exactly one remote tile").toHaveCount(1, {
        timeout: 30_000,
      });
      const [staleTileId] = await peerTileIds(hostPage);
      expect(staleTileId).toMatch(/^peer-video-\d+-div$/);

      const staleName = hostPage.locator(`#${staleTileId} .floating-name-text`);
      await expect(staleName).toBeVisible({ timeout: 15_000 });
      await expect(staleName, "the pre-drop tile must already show the peer's name").toHaveText(
        PEER_NAME,
      );

      // Let the peer's own join toast expire before watching for a new one.
      await expect(hostPage.locator(".peer-toast")).toHaveCount(0, { timeout: 20_000 });
      await startToastRecorder(hostPage);

      const cardMark = (await cardLog(peerPage)).length;
      const sever = await severMediaWebSocket(peerPage);
      const severedAt = Date.now();
      expect(
        sever.severed,
        `no live media socket was closed (recorder saw ${sever.recorded} socket(s)) — ` +
          `everything below would assert against an undisturbed call`,
      ).toBeGreaterThanOrEqual(1);

      const staleTile = hostPage.locator(`#${staleTileId}`);
      const deadline = severedAt + STALE_TILE_BUDGET_MS;
      let staleGoneAfterMs: number | null = null;
      while (Date.now() < deadline && staleGoneAfterMs === null) {
        if ((await staleTile.count()) === 0) {
          staleGoneAfterMs = Date.now() - severedAt;
          break;
        }
        await hostPage.waitForTimeout(200);
      }

      await expect
        .poll(async () => (await peerTileIds(hostPage)).filter((id) => id !== staleTileId).length, {
          timeout: 45_000,
          intervals: [500],
          message: "the peer never re-registered under a new session id",
        })
        .toBeGreaterThanOrEqual(1);

      const reconnectCard = await settledCardLog(peerPage, CARD_TITLE.connectedWs, cardMark);
      expect(phases(reconnectCard), "the reconnect must show the card again").toEqual([
        "preferred",
        "connected",
      ]);
      expect(reconnectCard.find((s) => s.phase === "preferred")).toMatchObject({
        title: CARD_TITLE.preferred,
        detail: "Using WebSocket",
      });

      const resumedIds = (await peerTileIds(hostPage)).filter((id) => id !== staleTileId);
      expect(resumedIds, "the reconnect must produce exactly one new session").toHaveLength(1);
      const resumedTileId = resumedIds[0];

      const toasts = await recordedToasts(hostPage);
      expect(
        toasts,
        "the toast recorder never installed — its verdict means nothing",
      ).not.toBeNull();
      expect(
        (toasts ?? []).filter((t) => JOIN_LEAVE_TOAST_RE.test(t)),
        "an in-grace reconnect must be invisible as churn: no leave and no join toast",
      ).toEqual([]);

      expect(
        staleGoneAfterMs,
        `the superseded tile ${staleTileId} was still in the host's grid ` +
          `${STALE_TILE_BUDGET_MS}ms after the drop. Un-fixed, only the 3-miss / 5s ` +
          `heartbeat watchdog reaps it (~15s); the fix retires it on ` +
          `PARTICIPANT_SESSION_RESUMED.`,
      ).not.toBeNull();

      const resumedName = hostPage.locator(`#${resumedTileId} .floating-name-text`);
      await expect(resumedName).toBeVisible({ timeout: 15_000 });
      await expect(
        resumedName,
        "the resumed session's PARTICIPANT_JOINED is suppressed, so only the " +
          "carry-over in adopt_resumed_session can name this tile — un-fixed it " +
          "falls back to the raw user_id",
      ).toHaveText(PEER_NAME);
      expect(await resumedName.textContent()).not.toContain("@");

      expect(
        hostConsole.some((line) => line.includes("Session resumed for")),
        "expected the client's PARTICIPANT_SESSION_RESUMED log line on the host",
      ).toBe(true);
    } finally {
      await hostBrowser.close();
      await peerBrowser.close();
    }
  });
});

test.describe("A never-activated candidate does not swallow the peer's departure (#3180)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("the host retires the departed peer's tile at grace expiry @bvt1", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3180_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const peerBrowser = await chromium.launch({ args: BROWSER_ARGS });

    let phase: "forward" | "candidate" | "hold" = "forward";
    let release!: () => void;
    const released = new Promise<void>((resolve) => {
      release = resolve;
    });
    const open = new Map<WebSocketRoute, WebSocketRoute>();
    let candidateUrl = "";
    let candidateJoined!: (pair: { page: WebSocketRoute; server: WebSocketRoute }) => void;
    const candidate = new Promise<{ page: WebSocketRoute; server: WebSocketRoute }>((resolve) => {
      candidateJoined = resolve;
    });

    try {
      const hostCtx = await createAuthenticatedContext(hostBrowser, HOST_EMAIL, HOST_NAME, uiURL);
      const peerCtx = await createAuthenticatedContext(peerBrowser, PEER_EMAIL, PEER_NAME, uiURL);
      await pinWebSocketTransport(hostCtx);
      await pinWebSocketTransport(peerCtx);
      await setRuntimeConfig(peerCtx, { webTransportEnabled: "false" });
      await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      await peerCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

      await peerCtx.routeWebSocket(LOBBY_SOCKET, async (ws) => {
        if (phase === "forward") {
          const server = ws.connectToServer();
          open.set(ws, server);
          const forget = () => open.delete(ws);
          server.onClose(forget);
          ws.onClose((code, reason) => {
            forget();
            if (phase === "forward") {
              server.close({ code: code === 1005 || !code ? 1000 : code, reason });
            }
          });
          return;
        }
        if (phase === "candidate") {
          phase = "hold";
          candidateUrl = ws.url();
          const server = ws.connectToServer();
          ws.onMessage(() => undefined);
          ws.onClose(() => undefined);
          let joined = false;
          server.onMessage(() => {
            if (!joined) {
              joined = true;
              candidateJoined({ page: ws, server });
            }
          });
          return;
        }
        await released;
        try {
          ws.connectToServer();
        } catch {
          /* the page abandoned this socket while it was held */
        }
      });

      const hostPage = await hostCtx.newPage();
      const peerPage = await peerCtx.newPage();

      await enterTwoUserMeeting(hostPage, peerPage, meetingId);

      await expect(hostPage.locator(PEER_TILE), "the host must hold one remote tile").toHaveCount(
        1,
        { timeout: 30_000 },
      );
      const [staleTileId] = await peerTileIds(hostPage);
      await expect(hostPage.locator(`#${staleTileId} .floating-name-text`)).toHaveText(PEER_NAME, {
        timeout: 15_000,
      });
      await expect(hostPage.locator(".peer-toast")).toHaveCount(0, { timeout: 20_000 });
      await startToastRecorder(hostPage);

      const [livePage, activeServer] = [...open].at(-1) ?? [];
      expect(livePage, "the peer's media socket never went through the route").toBeDefined();
      phase = "candidate";
      await livePage!.close({ code: 3001, reason: "e2e-3180-sever" });

      const { page: candidatePage, server: candidateServer } = await Promise.race([
        candidate,
        new Promise<never>((_, reject) =>
          setTimeout(() => reject(new Error("no reconnect candidate reached the relay")), 30_000),
        ),
      ]);
      expect(
        instanceId(candidateUrl),
        "the candidate must reuse the departed session's instance_id",
      ).toBe(instanceId(livePage!.url()));
      expect(instanceId(candidateUrl)).toBeTruthy();
      const severedSeq = connSeq(livePage!.url());
      const candidateSeq = connSeq(candidateUrl);
      expect(severedSeq, `the severed URL carries no numeric conn_seq: ${livePage!.url()}`).toMatch(
        /^\d+$/,
      );
      expect(
        candidateSeq,
        `the candidate URL carries no numeric conn_seq: ${candidateUrl}`,
      ).toMatch(/^\d+$/);
      expect(
        BigInt(candidateSeq!) > BigInt(severedSeq!),
        `the candidate's conn_seq ${candidateSeq} must exceed the severed session's ${severedSeq}`,
      ).toBe(true);

      await hostPage.waitForTimeout(JOIN_ROOM_SETTLE_MS);
      await activeServer!.close({ code: 1000, reason: "e2e-3180-active-departs" });
      const departedAt = Date.now();
      await hostPage.waitForTimeout(JOIN_ROOM_SETTLE_MS);
      await candidateServer.close({ code: 1000, reason: "e2e-3180-candidate-drops" });
      await candidatePage.close({ code: 3001, reason: "e2e-3180-candidate-drops" });

      const staleTile = hostPage.locator(`#${staleTileId}`);
      let goneAfterMs: number | null = null;
      while (Date.now() < departedAt + LEFT_BUDGET_MS) {
        if ((await staleTile.count()) === 0) {
          goneAfterMs = Date.now() - departedAt;
          break;
        }
        await hostPage.waitForTimeout(100);
      }
      expect(
        goneAfterMs,
        `the departed peer's tile ${staleTileId} was still on the host ${LEFT_BUDGET_MS}ms ` +
          `after its active socket closed; only the client watchdog would reap it`,
      ).not.toBeNull();
      await expect
        .poll(
          async () =>
            ((await recordedToasts(hostPage)) ?? []).filter((t) => t.includes("left the meeting")),
          { timeout: 2_000, intervals: [100], message: "the host must announce the departure" },
        )
        .toHaveLength(1);

      phase = "forward";
      release();

      await expect
        .poll(async () => (await peerTileIds(hostPage)).filter((id) => id !== staleTileId).length, {
          timeout: 45_000,
          intervals: [500],
          message: "the peer never rejoined after its held reconnects were released",
        })
        .toBe(1);
    } finally {
      release();
      await hostBrowser.close();
      await peerBrowser.close();
    }
  });
});
