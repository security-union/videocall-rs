import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
  pinWebTransportTransport,
} from "../helpers/auth-context";
import { wakeControls } from "../helpers/controls";
import { enableDiagnosticsTileIndicators } from "../helpers/diagnostics-tile-indicators";
import {
  createMeeting,
  fetchAdmittedUserIds,
  fetchMeetingState,
  joinMeeting,
} from "../helpers/meeting-api";
import { setTransportBadgeFlag } from "../helpers/transport-badge-config";
import { enterMeetingAsHost, guestJoinsMeeting } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * A relay-reported last-host departure defers the meeting end by
 * `HOST_LEFT_END_DEBOUNCE` (meeting-api/src/db/participants.rs) so a host who
 * comes back on the other relay keeps the meeting. Hang-up (REST /leave) still
 * ends it at once.
 */

const HOST_LEFT_END_DEBOUNCE_MS = 65_000;
// The in-process settle timer sleeps one second past the debounce.
const DEFERRED_END_MS = HOST_LEFT_END_DEBOUNCE_MS + 1_000;
// Relay RECONNECT_GRACE_PERIOD: LEFT cannot be applied sooner after the close.
const RECONNECT_GRACE_MS = 3_000;
// Presence sweep tick, the backstop settle if the timer misses.
const SWEEP_INTERVAL_MS = 30_000;

type Transport = "webtransport" | "websocket";

const BADGE_LABEL: Record<Transport, string> = {
  webtransport: "Your connection transport: WebTransport",
  websocket: "Your connection transport: WebSocket",
};

interface Host {
  email: string;
  name: string;
}

async function hostContext(
  browser: Browser,
  host: Host,
  uiURL: string,
  transport: Transport,
): Promise<BrowserContext> {
  const ctx = await createAuthenticatedContext(browser, host.email, host.name, uiURL);
  if (transport === "webtransport") {
    await pinWebTransportTransport(ctx);
  } else {
    await pinWebSocketTransport(ctx);
  }
  await setTransportBadgeFlag(ctx, "true");
  await enableDiagnosticsTileIndicators(ctx);
  return ctx;
}

async function expectElected(page: Page, transport: Transport): Promise<void> {
  await expect(
    page.locator(`.transport-badge[aria-label="${BADGE_LABEL[transport]}"]`),
    `host must have elected ${transport}`,
  ).toHaveCount(1, { timeout: 30_000 });
}

async function openPeerList(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await wakeControls(page);
  const openPeers = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Open Peers" }),
  });
  await expect(openPeers).toBeVisible({ timeout: 10_000 });
  await openPeers.click();
}

/** Resolves once meeting-api has applied the relay's LEFT (the host row is no longer admitted). */
async function expectHostLeftApplied(host: Host, meetingId: string): Promise<void> {
  await expect
    .poll(() => fetchAdmittedUserIds(host.email, host.name, meetingId), {
      message: "meeting-api must apply the relay's LEFT for the host",
      timeout: 45_000,
      intervals: [500],
    })
    .not.toContain(host.email);
}

/** Bounds on when meeting-api applied the host's LEFT: `earliest <= applied <= latest`. */
interface LeftApplied {
  earliest: number;
  latest: number;
}

async function hostDropsOut(
  hostCtx: BrowserContext,
  guestPage: Page,
  host: Host,
  meetingId: string,
): Promise<LeftApplied> {
  const hostTile = guestPage.locator(".floating-name", { hasText: host.name });
  await expect(hostTile.first()).toBeVisible({ timeout: 30_000 });
  const closedAt = Date.now();
  await hostCtx.close();
  await expect(hostTile).toHaveCount(0, { timeout: 45_000 });
  await expectHostLeftApplied(host, meetingId);
  return { earliest: closedAt + RECONNECT_GRACE_MS, latest: Date.now() };
}

async function waitUntil(page: Page, at: number): Promise<void> {
  await page.waitForTimeout(Math.max(0, at - Date.now()));
}

test.describe("Host transport drop defers the meeting end", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("host reloading onto the other relay after its LEFT keeps the meeting alive", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostleft_return_${Date.now()}`;
    const host = { email: "host-hostleft-return@videocall.rs", name: "HostLeftReturn" };
    const guestEmail = "guest-hostleft-return@videocall.rs";
    const guestName = "GuestLeftReturn";

    await createMeeting(host.email, host.name, {
      meetingId,
      waitingRoomEnabled: false,
      endOnHostLeave: true,
    });

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const firstHostCtx = await hostContext(hostBrowser, host, uiURL, "webtransport");
      const guestCtx = await createAuthenticatedContext(guestBrowser, guestEmail, guestName, uiURL);
      await pinWebSocketTransport(guestCtx);

      const firstHostPage = await firstHostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      await enterMeetingAsHost(firstHostPage, meetingId, host.name);
      await expectElected(firstHostPage, "webtransport");
      await guestJoinsMeeting(firstHostPage, guestPage, meetingId, guestName);

      const left = await hostDropsOut(firstHostCtx, guestPage, host, meetingId);
      const ended = guestPage.locator(".meeting-ended-overlay");
      await expect(ended, "the relay's LEFT must not end the meeting").toHaveCount(0);

      const returnedCtx = await hostContext(hostBrowser, host, uiURL, "websocket");
      const returnedPage = await returnedCtx.newPage();
      await enterMeetingAsHost(returnedPage, meetingId, host.name);
      await expectElected(returnedPage, "websocket");

      await openPeerList(returnedPage);
      await expect(
        returnedPage
          .locator(".peer_item", { hasText: host.name })
          .first()
          .locator(".peer-indicator"),
      ).toHaveText("(You/Host)", { timeout: 30_000 });
      await expect(guestPage.locator(".floating-name", { hasText: host.name }).first()).toBeVisible(
        { timeout: 30_000 },
      );

      await waitUntil(guestPage, left.latest + DEFERRED_END_MS + 10_000);
      await expect(ended).toHaveCount(0);
      await expect(returnedPage.locator(".meeting-ended-overlay")).toHaveCount(0);
      await expect(guestPage.locator("#grid-container")).toBeVisible();
      await expect(returnedPage.locator("#grid-container")).toBeVisible();
      expect(await fetchMeetingState(host.email, host.name, meetingId)).toBe("active");
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  test("host who never returns ends the meeting once the debounce elapses", async ({ baseURL }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostleft_gone_${Date.now()}`;
    const host = { email: "host-hostleft-gone@videocall.rs", name: "HostLeftGone" };
    const guestEmail = "guest-hostleft-gone@videocall.rs";
    const guestName = "GuestLeftGone";

    await createMeeting(host.email, host.name, {
      meetingId,
      waitingRoomEnabled: false,
      endOnHostLeave: true,
    });

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await hostContext(hostBrowser, host, uiURL, "webtransport");
      const guestCtx = await createAuthenticatedContext(guestBrowser, guestEmail, guestName, uiURL);
      await pinWebSocketTransport(guestCtx);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      await enterMeetingAsHost(hostPage, meetingId, host.name);
      await guestJoinsMeeting(hostPage, guestPage, meetingId, guestName);

      const left = await hostDropsOut(hostCtx, guestPage, host, meetingId);
      const ended = guestPage.locator(".meeting-ended-overlay");

      await waitUntil(guestPage, left.earliest + HOST_LEFT_END_DEBOUNCE_MS - 10_000);
      await expect(ended, "the meeting must survive the debounce window").toHaveCount(0);
      expect(await fetchMeetingState(host.email, host.name, meetingId)).toBe("active");

      await expect(ended, "the deferred end must fire").toBeVisible({
        timeout: left.latest + HOST_LEFT_END_DEBOUNCE_MS + SWEEP_INTERVAL_MS + 10_000 - Date.now(),
      });
      expect(await fetchMeetingState(host.email, host.name, meetingId)).toBe("ended");
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  test("a sole host's LEFT idles the meeting and an attendee cannot restart it", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostleft_idle_${Date.now()}`;
    const host = { email: "host-hostleft-idle@videocall.rs", name: "HostLeftIdle" };

    await createMeeting(host.email, host.name, {
      meetingId,
      waitingRoomEnabled: false,
      endOnHostLeave: true,
    });

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await hostContext(hostBrowser, host, uiURL, "webtransport");
      const hostPage = await hostCtx.newPage();
      await enterMeetingAsHost(hostPage, meetingId, host.name);
      await expectElected(hostPage, "webtransport");
      await expect
        .poll(() => fetchAdmittedUserIds(host.email, host.name, meetingId))
        .toContain(host.email);

      await hostCtx.close();
      await expectHostLeftApplied(host, meetingId);
      await expect
        .poll(() => fetchMeetingState(host.email, host.name, meetingId), { timeout: 15_000 })
        .toBe("idle");

      await expect(
        joinMeeting("attendee-hostleft-idle@videocall.rs", "AttendeeLeftIdle", meetingId),
      ).rejects.toThrow(/MEETING_NOT_ACTIVE/);
      expect(await fetchMeetingState(host.email, host.name, meetingId)).toBe("idle");
    } finally {
      await hostBrowser.close();
    }
  });

  test("host hang-up still ends the meeting for the guest at once", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostleft_hangup_${Date.now()}`;
    const host = { email: "host-hostleft-hangup@videocall.rs", name: "HostLeftHangup" };
    const guestEmail = "guest-hostleft-hangup@videocall.rs";
    const guestName = "GuestLeftHangup";

    await createMeeting(host.email, host.name, {
      meetingId,
      waitingRoomEnabled: false,
      endOnHostLeave: true,
    });

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await hostContext(hostBrowser, host, uiURL, "webtransport");
      const guestCtx = await createAuthenticatedContext(guestBrowser, guestEmail, guestName, uiURL);
      await pinWebSocketTransport(guestCtx);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      await enterMeetingAsHost(hostPage, meetingId, host.name);
      await guestJoinsMeeting(hostPage, guestPage, meetingId, guestName);

      await hostPage.locator(".video-controls-container").hover();
      await wakeControls(hostPage);
      await hostPage.locator("button.video-control-button.danger").click();

      // Well inside HOST_LEFT_END_DEBOUNCE_MS: hang-up must not be deferred.
      await expect(guestPage.locator(".meeting-ended-overlay")).toBeVisible({ timeout: 20_000 });
      expect(await fetchMeetingState(host.email, host.name, meetingId)).toBe("ended");
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });

  test("hang-up whose /leave lands after the relay LEFT still ends the meeting at once", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_hostleft_lateleave_${Date.now()}`;
    const host = { email: "host-hostleft-lateleave@videocall.rs", name: "HostLeftLateLeave" };
    const guestEmail = "guest-hostleft-lateleave@videocall.rs";
    const guestName = "GuestLeftLateLeave";

    await createMeeting(host.email, host.name, {
      meetingId,
      waitingRoomEnabled: false,
      endOnHostLeave: true,
    });

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await hostContext(hostBrowser, host, uiURL, "webtransport");
      const guestCtx = await createAuthenticatedContext(guestBrowser, guestEmail, guestName, uiURL);
      await pinWebSocketTransport(guestCtx);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      await enterMeetingAsHost(hostPage, meetingId, host.name);
      await guestJoinsMeeting(hostPage, guestPage, meetingId, guestName);
      const ended = guestPage.locator(".meeting-ended-overlay");

      // The client stops awaiting /leave after LEAVE_DEADLINE_MS and navigates
      // away, so deliver the held request from the context instead of the page.
      let releasedAt = 0;
      let settle!: { resolve: (status: number) => void; reject: (e: unknown) => void };
      const leaveStatus = new Promise<number>((resolve, reject) => {
        settle = { resolve, reject };
      });
      await hostCtx.route(`**/api/v1/meetings/${meetingId}/leave`, async (route) => {
        try {
          await expectHostLeftApplied(host, meetingId);
          await expect(ended, "the relay's LEFT must only defer the end").toHaveCount(0);
          releasedAt = Date.now();
          const res = await route.fetch();
          settle.resolve(res.status());
          await route.fulfill({ response: res }).catch(() => {});
        } catch (e) {
          settle.reject(e);
        }
      });

      await hostPage.locator(".video-controls-container").hover();
      await wakeControls(hostPage);
      await hostPage.locator("button.video-control-button.danger").click();

      expect(await leaveStatus).toBe(200);
      await expect(ended).toBeVisible({ timeout: releasedAt + 20_000 - Date.now() });
      expect(await fetchMeetingState(host.email, host.name, meetingId)).toBe("ended");
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});
