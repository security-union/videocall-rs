import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { joinMeetingFromPage } from "../helpers/two-user-meeting";
import { createMeeting } from "../helpers/meeting-api";
import { installMediaSocketRecorder, severMediaWebSocket } from "../helpers/media-socket-sever";

/**
 * #2267: on transport loss the client stages a roster reconcile (`Reconnecting =>
 * handle_reconnecting_roster_stage`, video_call_client.rs:2222) and reaps sessions the
 * relay's replay never confirms on the first monitor pass, not the 3-miss/5s watchdog.
 * Measured 3.08s fixed vs 7.9s/12.9s un-fixed, so the budget is one monitor period.
 * Sever OBSERVER ONCE: re-closing hits `Failed` -> clear_all_peers() (:2140), a false
 * green. WS only; UNTAGGED, so run it with --project=dioxus.
 */

const OBSERVER_EMAIL = "reconcile-observer@videocall.rs";
const OBSERVER_NAME = "ReconcileObserver";
const RESUMER_EMAIL = "reconcile-resumer@videocall.rs";
const RESUMER_NAME = "ReconcileResumer";
const STAYER_EMAIL = "reconcile-stayer@videocall.rs";
const STAYER_NAME = "ReconcileStayer";

/** Remote tiles only: `canvas_generator.rs:1705` emits the id, `:1803` the name span. */
const PEER_TILE = '[id^="peer-video-"][id$="-div"]';

const CONNECTED_LOG = "Connection state changed: Connected";

/** MUST stay under 2.4s: with OBSERVER's ~2.6s reconnect this keeps the gap under one
 *  5s period, so the phantom banks <=1 miss. Raising it silently re-breaks (a). */
const RESUMER_REKEY_MS = 1_500;

const PHANTOM_BUDGET_MS = 5_000;

async function peerTileIds(page: Page): Promise<string[]> {
  return page.locator(PEER_TILE).evaluateAll((els) => els.map((el) => el.id));
}

async function tileIdsByName(page: Page): Promise<Record<string, string>> {
  return page.locator(PEER_TILE).evaluateAll((els) => {
    const out: Record<string, string> = {};
    for (const el of els) {
      const name = (el.querySelector(".floating-name-text")?.textContent || "").trim();
      if (name) out[name] = el.id;
    }
    return out;
  });
}

test.describe("Reconnect roster reconcile (#2267)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a superseded peer session is reaped against the relay replay, not the watchdog", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_reconcile_${Date.now()}`;

    await createMeeting(OBSERVER_EMAIL, OBSERVER_NAME, {
      meetingId,
      waitingRoomEnabled: false,
      allowGuests: false,
      endOnHostLeave: false,
    });

    const observerBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const resumerBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const stayerBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const observerCtx = await createAuthenticatedContext(
        observerBrowser,
        OBSERVER_EMAIL,
        OBSERVER_NAME,
        uiURL,
      );
      const resumerCtx = await createAuthenticatedContext(
        resumerBrowser,
        RESUMER_EMAIL,
        RESUMER_NAME,
        uiURL,
      );
      const stayerCtx = await createAuthenticatedContext(
        stayerBrowser,
        STAYER_EMAIL,
        STAYER_NAME,
        uiURL,
      );

      // Camera defaults OFF in e2e; these two need it ON so RESUMER's successor tile
      // appears. STAYER stays OFF: publishing no media is the case (b) guards, and
      // heartbeats alone keep its peer entry, so its tile still renders (measured).
      for (const ctx of [observerCtx, resumerCtx]) {
        await ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      }
      await installMediaSocketRecorder(observerCtx);
      await installMediaSocketRecorder(resumerCtx);

      const observerPage = await observerCtx.newPage();
      const resumerPage = await resumerCtx.newPage();
      const stayerPage = await stayerCtx.newPage();
      const observerConsole: string[] = [];
      observerPage.on("console", (msg) => observerConsole.push(msg.text()));

      await fillAndSubmitJoinForm(observerPage, meetingId, OBSERVER_NAME);
      expect(await joinMeetingFromPage(observerPage)).toBe("in-meeting");
      await fillAndSubmitJoinForm(resumerPage, meetingId, RESUMER_NAME);
      expect(await joinMeetingFromPage(resumerPage)).toBe("in-meeting");
      await fillAndSubmitJoinForm(stayerPage, meetingId, STAYER_NAME);
      expect(await joinMeetingFromPage(stayerPage)).toBe("in-meeting");

      await expect(
        observerPage.locator(PEER_TILE),
        "the observer must hold one tile per remote peer before anything is severed",
      ).toHaveCount(2, { timeout: 60_000 });

      const baseline = await tileIdsByName(observerPage);
      const phantomTileId = baseline[RESUMER_NAME];
      const stayerTileId = baseline[STAYER_NAME];
      expect(
        phantomTileId,
        `no observer tile is named ${RESUMER_NAME} (saw ${JSON.stringify(baseline)})`,
      ).toMatch(/^peer-video-\d+-div$/);
      expect(
        stayerTileId,
        `no observer tile is named ${STAYER_NAME} (saw ${JSON.stringify(baseline)})`,
      ).toMatch(/^peer-video-\d+-div$/);

      const resumerSever = await severMediaWebSocket(resumerPage);
      expect(
        resumerSever.severed,
        `no live media socket closed on ${RESUMER_NAME} (recorder saw ` +
          `${resumerSever.recorded} socket(s)) — nothing was superseded`,
      ).toBeGreaterThanOrEqual(1);
      await observerPage.waitForTimeout(RESUMER_REKEY_MS);

      const connectedMark = observerConsole.filter((l) => l.includes(CONNECTED_LOG)).length;
      const observerSever = await severMediaWebSocket(observerPage);
      expect(
        observerSever.severed,
        `no live media socket closed on ${OBSERVER_NAME} (recorder saw ` +
          `${observerSever.recorded} socket(s)) — the reconcile never armed`,
      ).toBeGreaterThanOrEqual(1);

      await expect
        .poll(() => observerConsole.filter((l) => l.includes(CONNECTED_LOG)).length, {
          timeout: 60_000,
          intervals: [200],
          message: "the observer never reached Connected again after its transport was severed",
        })
        .toBeGreaterThan(connectedMark);
      const reconnectedAt = Date.now();
      const phantomTile = observerPage.locator(`#${phantomTileId}`);
      const phantomHeldToReconnect = (await phantomTile.count()) === 1;

      const deadline = reconnectedAt + PHANTOM_BUDGET_MS;
      let phantomGoneAfterMs: number | null = null;
      while (Date.now() < deadline && phantomGoneAfterMs === null) {
        if ((await phantomTile.count()) === 0) {
          phantomGoneAfterMs = Date.now() - reconnectedAt;
          break;
        }
        await observerPage.waitForTimeout(200);
      }

      await expect
        .poll(
          async () =>
            (await peerTileIds(observerPage)).filter(
              (id) => id !== phantomTileId && id !== stayerTileId,
            ).length,
          {
            timeout: 45_000,
            intervals: [500],
            message: `${RESUMER_NAME} never re-registered under a new session id`,
          },
        )
        .toBeGreaterThanOrEqual(1);

      await expect(
        observerPage.locator(`#${stayerTileId}`),
        `GUARD (b): ${STAYER_NAME} stayed connected across the whole reconnect and must ` +
          "keep its tile. It is doubly protected — the replay confirms it AND its " +
          "heartbeats keep activity_count > 0 — so this guards a blanket roster wipe, " +
          "not confirm_peer_present specifically. It passes on un-fixed code too.",
      ).toHaveCount(1, { timeout: 30_000 });

      expect(
        phantomHeldToReconnect,
        `the superseded tile ${phantomTileId} was already gone the moment the observer ` +
          "reconnected, so nothing here is about the reconcile. Either the observer hit " +
          "ConnectionState::Failed and clear_all_peers() wiped the roster, or a " +
          "PARTICIPANT_SESSION_RESUMED (#2269) retired the tile before the reconnect.",
      ).toBe(true);
      expect(
        phantomGoneAfterMs,
        `DISCRIMINATOR (a): the superseded tile ${phantomTileId} was STILL PRESENT on the ` +
          `observer's grid ${PHANTOM_BUDGET_MS}ms after it reconnected. Un-fixed the ` +
          "watchdog needs 3 consecutive misses, so the reap lands on monitor pass 2 or " +
          "later (measured 7.9s and 12.9s); the fix drops it on pass 1 (measured 3.1s).",
      ).not.toBeNull();
    } finally {
      await observerBrowser.close();
      await resumerBrowser.close();
      await stayerBrowser.close();
    }
  });
});
