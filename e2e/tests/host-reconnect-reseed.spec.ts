import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import {
  routeDownlinkThroughProxy,
  assertProxyUp,
  severWsTransport,
  restoreWsTransport,
} from "../helpers/downlink-impair";
import { createMeeting, transferHost } from "../helpers/meeting-api";

/**
 * E2E: host controls re-sync on transport reconnect after a swallowed
 * HOST_REVOKED.
 *
 * ## What this guards
 *
 * A non-guest client's host state (`host_set_signal`) can drift when a
 * HOST_GRANTED/HOST_REVOKED event is delivered while its media transport is
 * down — the relay forwards those events over the media session, so a
 * disconnected client never sees them (they are "swallowed"). The client heals
 * this on RECONNECT: the media client's `on_connected` callback re-runs
 * `should_reconcile_host_on_connect` → `reseed_host_set_from_roster`, which
 * re-fetches the `/participants` roster (the source of truth) and replaces
 * `host_set_signal` (dioxus-ui/src/components/attendants.rs).
 *
 * This spec exercises that glue over a real transport drop + reconnect:
 *
 *   1. A (host) + B join; A's media WS is routed through toxiproxy.
 *   2. A's transport is SEVERED (`ws-downlink` disabled) → A enters its
 *      reconnect backoff and stays offline.
 *   3. While A is offline, host is transferred A→B via the meeting-api REST
 *      endpoint using A's own host token (the endpoint demotes the caller).
 *      B (online) flips to host; A never receives the HOST_REVOKED (swallowed).
 *   4. A's transport is RESTORED → A reconnects → the `on_connected` reseed
 *      heals `host_set_signal`.
 *
 * ## Why this is a genuine fail-when-removed guard (not a false-green)
 *
 * A's own "(You/Host)" indicator is driven by `host_set.is_host(self)`
 * (`is_host: is_current_user_host` in peer_list.rs), NOT by the `is_owner`
 * /status prop. On the WS-drop → `schedule_reconnect` path the meeting view is
 * not remounted and `is_owner` is not re-fetched (the media client is rebuilt
 * in place, no `host_refresh_nonce` bump), and there is no periodic /status or
 * /participants poll for an admitted participant. So after A reconnects, the
 * only thing that can rewrite `host_set_signal` is the `on_connected` reseed —
 * remove it and A's self row stays "(You/Host)" and the final assertion times
 * out.
 *
 * The middle assertion (A STILL shows "(You/Host)" after the transfer, while
 * severed) proves the swallow really happened: `host_set_signal` is not cleared
 * on disconnect, so the stale crown persists until the reconnect reseed clears
 * it.
 *
 * ## Scope
 *
 * WebSocket only — the sever/restore primitive toggles the `ws-downlink`
 * toxiproxy proxy, so A must be pinned+routed to WS via
 * `routeDownlinkThroughProxy` (there is no per-client WT/QUIC sever).
 *
 * UNTAGGED (no @bvt) so it does not run in per-PR CI, which has no toxiproxy.
 * Run it against the impair stack:
 *
 *   make e2e-up-impair            # or COMPOSE_PROFILES=impair make e2e-up
 *   cd e2e && npx playwright test host-reconnect-reseed.spec.ts
 */

// ---------------------------------------------------------------------------
// Identities. The e2e session JWT's `sub` (== the meeting-api participant
// user_id) is the EMAIL, so the transfer-host target is B's email.
// ---------------------------------------------------------------------------
const HOST_EMAIL = "host-reconnect-owner@videocall.rs";
const HOST_NAME = "HostReconnectOwner";
const PEER_EMAIL = "host-reconnect-peer@videocall.rs";
const PEER_NAME = "HostReconnectPeer";

// ---------------------------------------------------------------------------
// Shared helpers (mirror transfer-host.spec.ts / host-kick.spec.ts)
// ---------------------------------------------------------------------------

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);
}

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting" || result === "waiting-for-meeting") {
    return result;
  }
  if (result === "auto-joined") {
    return "in-meeting";
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

/** Admit a waiting participant from the host's waiting-room controls. */
async function admitIfNeeded(
  hostPage: Page,
  participantPage: Page,
  result: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (result === "in-meeting") {
    return;
  }
  if (result === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.click();
    await hostPage.waitForTimeout(3000);

    const joinButton = participantPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
    const grid = participantPage.locator("#grid-container");
    const postAdmit = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (postAdmit === "join") {
      await participantPage.waitForTimeout(1000);
      await joinButton.click();
      await participantPage.waitForTimeout(3000);
      await expect(grid).toBeVisible({ timeout: 15_000 });
    }
  }
}

/** Open the peer-list sidebar via the "Open Peers" video-controls button. */
async function openPeerListSidebar(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.mouse.move(400, 400);
  await page.waitForTimeout(300);

  const openPeersBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Open Peers" }),
  });
  await expect(openPeersBtn).toBeVisible({ timeout: 10_000 });
  await openPeersBtn.click();
  await page.waitForTimeout(800);
}

/** The `.peer-indicator` of a peer-list row whose name contains `name`. */
function peerIndicator(page: Page, name: string) {
  return page.locator(".peer_item", { hasText: name }).first().locator(".peer-indicator");
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

test.describe("Host controls re-sync on transport reconnect", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("swallowed HOST_REVOKED is healed by the on_connected reseed at reconnect", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_host_reconnect_${Date.now()}`;

    // Fail loud + early if the toxiproxy `impair` profile is not up — the
    // sever/restore primitive depends on it.
    await assertProxyUp();

    // Seed the meeting with end_on_host_leave=FALSE and the waiting room OFF.
    // Critical: this test SEVERS the HOST's transport; with end_on_host_leave
    // on, the host's presence dropping could END the meeting mid-test (and the
    // transfer-host call would then 404). Disabling it keeps the meeting alive
    // while A is offline. Waiting room off lets B auto-admit straight to the
    // grid. A joins this pre-created meeting as its owner (→ host).
    await createMeeting(HOST_EMAIL, HOST_NAME, {
      meetingId,
      waitingRoomEnabled: false,
      allowGuests: false,
      endOnHostLeave: false,
    });

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const peerBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(hostBrowser, HOST_EMAIL, HOST_NAME, uiURL);
      // Route the HOST's media WS through toxiproxy (pins WS) so we can sever it.
      // MUST happen before the context's first navigation.
      await routeDownlinkThroughProxy(hostCtx);
      const peerCtx = await createAuthenticatedContext(peerBrowser, PEER_EMAIL, PEER_NAME, uiURL);

      const hostPage = await hostCtx.newPage();
      const peerPage = await peerCtx.newPage();

      // ---- Join: host first (creates + starts the meeting), then peer ----
      await navigateToMeeting(hostPage, meetingId, HOST_NAME);
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");
      await navigateToMeeting(peerPage, meetingId, PEER_NAME);
      await admitIfNeeded(hostPage, peerPage, await joinMeetingFromPage(peerPage));

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(peerPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // ---- Baseline: A is host (host_set-driven self indicator) ----
      await openPeerListSidebar(hostPage);
      await expect(peerIndicator(hostPage, HOST_NAME)).toHaveText("(You/Host)", {
        timeout: 30_000,
      });

      // ---- Sever A's transport → A enters reconnect backoff (stays offline) ----
      await severWsTransport();
      // Give A's onclose + on_connection_lost a beat to register before we
      // transfer, so the HOST_REVOKED is genuinely delivered while A is down.
      await hostPage.waitForTimeout(3000);

      // ---- Transfer host A→B via REST with A's own host token (demotes A) ----
      // A is offline, so A never receives the HOST_REVOKED packet.
      await transferHost(HOST_EMAIL, HOST_NAME, meetingId, PEER_EMAIL);

      // B (online) receives the events and becomes host — confirms the transfer
      // actually propagated to connected clients.
      await openPeerListSidebar(peerPage);
      await expect(peerIndicator(peerPage, PEER_NAME)).toHaveText("(You/Host)", {
        timeout: 30_000,
      });

      // A's crown MUST still be stale here: A missed the event because it is
      // severed. (Had A been online it would already read "(You)".) A short
      // settle after B flipped closes the delivery race — by now the event has
      // fanned out to online clients, and A demonstrably did not get it.
      await hostPage.waitForTimeout(2000);
      await expect(peerIndicator(hostPage, HOST_NAME)).toHaveText("(You/Host)", { timeout: 5_000 });

      // ---- Restore A's transport → A reconnects → on_connected reseed heals ----
      await restoreWsTransport();

      // THE ASSERTION: A's self row flips to "(You)" purely as a result of the
      // reconnect reseed. No remount, no /status poll, no re-delivered event can
      // do this — remove `should_reconcile_host_on_connect`/
      // `reseed_host_set_from_roster` and this stays "(You/Host)" and times out.
      await expect(peerIndicator(hostPage, HOST_NAME)).toHaveText("(You)", { timeout: 45_000 });

      // And A's view of B now carries the host crown — the reseeded host_set
      // contains B (whom A never saw promoted while offline).
      await expect(peerIndicator(hostPage, PEER_NAME)).toHaveText("(Host)", { timeout: 30_000 });
    } finally {
      // Always re-enable the proxy so a failed run does not leave it severed
      // for the next test.
      await restoreWsTransport().catch(() => {
        /* proxy already up / stack down — nothing to restore */
      });
      await hostBrowser.close();
      await peerBrowser.close();
    }
  });
});
