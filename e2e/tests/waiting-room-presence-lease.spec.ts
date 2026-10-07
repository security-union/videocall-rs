import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import {
  createMeeting,
  fetchMeetingState,
  fetchWaiting,
  joinMeeting,
  patchMeetingSettings,
} from "../helpers/meeting-api";
import { enterMeetingAsHost, joinMeetingFromPage } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

// videocall-meeting-types/src/presence.rs
const PRESENCE_HEARTBEAT_INTERVAL_MS = 30_000;
const PRESENCE_LEASE_MS = 3 * PRESENCE_HEARTBEAT_INTERVAL_MS;
// dioxus-ui/src/components/host_controls.rs
const HOST_POLL_INTERVAL_MS = 10_000;
const SLACK_MS = 30_000;
const LAPSE_TIMEOUT_MS = PRESENCE_LEASE_MS + HOST_POLL_INTERVAL_MS + SLACK_MS;
// dioxus-ui/src/components/waiting_room.rs: every 3rd 5s tick while the observer is up
const WAITER_STATUS_POLL_MS = 15_000;
const WAITING_ROOM_CARD = '[data-testid="meeting-waiting-room"]';

interface User {
  email: string;
  name: string;
}

interface KeepaliveGate {
  setBlocked(blocked: boolean): void;
  renewedSince(t: number): number;
  lastRenewalAt(): number;
  blockedAttempts(): number;
}

async function launchUser(uiURL: string, user: User): Promise<{ browser: Browser; page: Page }> {
  const browser = await chromium.launch({ args: BROWSER_ARGS });
  const context = await createAuthenticatedContext(browser, user.email, user.name, uiURL);
  return { browser, page: await context.newPage() };
}

/** Aborts the page's presence keepalives while blocked; records each one that got through. */
async function gateKeepalive(page: Page, meetingId: string): Promise<KeepaliveGate> {
  const url = new RegExp(`/api/v1/meetings/${meetingId}/presence/keepalive$`);
  const renewals: number[] = [];
  let blocked = false;
  let attempts = 0;
  await page.route(url, (route) => {
    if (blocked && route.request().method() === "POST") {
      attempts += 1;
      return route.abort("connectionrefused");
    }
    return route.continue();
  });
  page.on("response", (res) => {
    if (url.test(res.url()) && res.request().method() === "POST" && res.status() === 200) {
      renewals.push(Date.now());
    }
  });
  return {
    setBlocked: (b) => {
      blocked = b;
    },
    renewedSince: (t) => renewals.filter((at) => at >= t).length,
    lastRenewalAt: () => Math.max(...renewals),
    blockedAttempts: () => attempts,
  };
}

async function queueRenewingWaiter(
  page: Page,
  gate: KeepaliveGate,
  meetingId: string,
  name: string,
): Promise<void> {
  const joinedAt = Date.now();
  await fillAndSubmitJoinForm(page, meetingId, name);
  expect(await joinMeetingFromPage(page)).toBe("waiting");
  await expect
    .poll(() => gate.renewedSince(joinedAt), {
      timeout: 15_000,
      message: "the waiting page renews its presence lease (200) on mount",
    })
    .toBeGreaterThan(0);
}

async function reachGrid(page: Page): Promise<void> {
  const grid = page.locator("#grid-container");
  const joinButton = page.getByRole("button", { name: /Join Meeting|Start Meeting/ });
  await expect(grid.or(joinButton).first()).toBeVisible({ timeout: 30_000 });
  if (!(await grid.isVisible())) {
    await joinButton.click({ timeout: 5_000 }).catch(() => {});
  }
  await expect(grid, "the admitted page reaches the call").toBeVisible({ timeout: 20_000 });
}

function countKnockPlays(): void {
  const w = window as unknown as { __knockPlays: number };
  w.__knockPlays = 0;
  const play = HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play = function (this: HTMLMediaElement) {
    if (/\/assets\/knock\.wav$/.test(this.src)) w.__knockPlays += 1;
    return play.call(this);
  };
}

function readKnockPlays(page: Page): Promise<number> {
  return page.evaluate(() => (window as unknown as { __knockPlays?: number }).__knockPlays ?? -1);
}

test.describe("Waiting-room presence lease (#2912)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a waiter whose keepalive stops drops off the host's list within the lease, is relisted when it resumes, and knocks once", async ({
    baseURL,
  }) => {
    test.setTimeout(360_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wr_lease_${Date.now()}`;
    const HOST = { email: "wr-lease-host@videocall.rs", name: "LeaseHost" };
    const WAITER = { email: "wr-lease-waiter@videocall.rs", name: "LeaseWaiter" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: true });

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const waiter = await launchUser(uiURL, WAITER);
    try {
      const hostContext = await createAuthenticatedContext(
        hostBrowser,
        HOST.email,
        HOST.name,
        uiURL,
      );
      await hostContext.addInitScript(countKnockPlays);
      const hostPage = await hostContext.newPage();
      await enterMeetingAsHost(hostPage, meetingId, HOST.name);

      const gate = await gateKeepalive(waiter.page, meetingId);
      await queueRenewingWaiter(waiter.page, gate, meetingId, WAITER.name);

      const waiterRow = hostPage.locator(".waiting-participant").filter({ hasText: WAITER.name });
      await expect(waiterRow, "the host lists the live waiter").toBeVisible({ timeout: 30_000 });
      await expect(
        hostPage.getByRole("button", { name: `Admit ${WAITER.name}`, exact: true }),
      ).toBeVisible();
      await expect(
        hostPage.getByRole("button", { name: `Reject ${WAITER.name}`, exact: true }),
      ).toBeVisible();
      await expect
        .poll(() => readKnockPlays(hostPage), {
          timeout: 5_000,
          message: "the waiter's arrival knocks once",
        })
        .toBe(1);

      gate.setBlocked(true);
      await expect(
        waiterRow,
        "a waiter that stops renewing drops off the host's list once the lease lapses",
      ).toHaveCount(0, { timeout: LAPSE_TIMEOUT_MS });
      expect(
        Date.now() - gate.lastRenewalAt(),
        "the waiter is not dropped before its lease runs out",
      ).toBeGreaterThanOrEqual(PRESENCE_LEASE_MS - 10_000);
      expect(
        gate.blockedAttempts(),
        "the waiter kept trying to renew through the failures",
      ).toBeGreaterThanOrEqual(2);
      await expect(
        waiter.page.getByText("Waiting to be admitted"),
        "the lapsed waiter's own page stays in the waiting room",
      ).toBeVisible();

      gate.setBlocked(false);
      const resumedAt = Date.now();
      await expect
        .poll(() => gate.renewedSince(resumedAt), {
          timeout: PRESENCE_HEARTBEAT_INTERVAL_MS + SLACK_MS,
          message: "the waiter's next keepalive after the block succeeds",
        })
        .toBeGreaterThan(0);
      await expect(waiterRow, "a waiter whose renewals resume is relisted").toBeVisible({
        timeout: HOST_POLL_INTERVAL_MS + SLACK_MS,
      });

      expect(
        await readKnockPlays(hostPage),
        "the same waiter lapsing and returning does not knock again",
      ).toBe(1);
    } finally {
      await hostBrowser.close();
      await waiter.browser.close();
    }
  });

  test("a lapsed waiter left behind when the host turns the waiting room off re-joins once and lands in the call", async ({
    baseURL,
  }) => {
    test.setTimeout(360_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wr_lease_off_${Date.now()}`;
    const HOST = { email: "wr-lease-off-host@videocall.rs", name: "LeaseOffHost" };
    const WAITER = { email: "wr-lease-off-waiter@videocall.rs", name: "LeaseOffWaiter" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: true });

    const host = await launchUser(uiURL, HOST);
    const waiter = await launchUser(uiURL, WAITER);
    try {
      await enterMeetingAsHost(host.page, meetingId, HOST.name);

      const joinPath = `/api/v1/meetings/${meetingId}/join`;
      const joins: number[] = [];
      waiter.page.on("request", (r) => {
        if (r.method() === "POST" && new URL(r.url()).pathname === joinPath) joins.push(Date.now());
      });
      const gate = await gateKeepalive(waiter.page, meetingId);
      await queueRenewingWaiter(waiter.page, gate, meetingId, WAITER.name);
      expect(joins, "the waiter joined once to reach the waiting room").toHaveLength(1);

      const waiterRow = host.page.locator(".waiting-participant").filter({ hasText: WAITER.name });
      await expect(waiterRow, "the host lists the live waiter").toBeVisible({ timeout: 30_000 });
      gate.setBlocked(true);
      await expect(waiterRow, "the waiter's lease lapses").toHaveCount(0, {
        timeout: LAPSE_TIMEOUT_MS,
      });

      await waiter.page.evaluate(() => {
        (window as unknown as { __sameDocument?: boolean }).__sameDocument = true;
      });
      const turnedOffAt = Date.now();
      await patchMeetingSettings(HOST.email, HOST.name, meetingId, { waiting_room_enabled: false });
      gate.setBlocked(false);

      await expect(
        waiter.page.locator(WAITING_ROOM_CARD),
        "a waiter the bulk admit skipped leaves the waiting room on its own",
      ).toBeHidden({ timeout: WAITER_STATUS_POLL_MS + SLACK_MS });
      await reachGrid(waiter.page);
      expect(
        joins.filter((at) => at >= turnedOffAt),
        "the stranded waiter re-joins exactly once",
      ).toHaveLength(1);
      expect(
        await waiter.page.evaluate(
          () => (window as unknown as { __sameDocument?: boolean }).__sameDocument ?? null,
        ),
        "the re-join happened in place, not by a reload",
      ).toBe(true);
    } finally {
      await host.browser.close();
      await waiter.browser.close();
    }
  });

  test("a guest waiter renews its lease and leaves the waiting room with its observer token", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wr_lease_guest_${Date.now()}`;
    const HOST = { email: "wr-lease-guest-host@videocall.rs", name: "LeaseGuestHost" };
    const GUEST_NAME = "LeaseGuest";
    await createMeeting(HOST.email, HOST.name, {
      meetingId,
      waitingRoomEnabled: true,
      allowGuests: true,
    });
    expect((await joinMeeting(HOST.email, HOST.name, meetingId, HOST.name)).status).toBe(
      "admitted",
    );
    await expect
      .poll(() => fetchMeetingState(HOST.email, HOST.name, meetingId), { timeout: 20_000 })
      .toBe("active");

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const context = await browser.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
      const page = await context.newPage();
      const renewal = page.waitForRequest(
        (r) =>
          r.method() === "POST" &&
          r.url().endsWith(`/api/v1/meetings/${meetingId}/presence/keepalive-guest`),
        { timeout: 30_000 },
      );

      await page.goto(`/meeting/${meetingId}/guest`);
      const nameInput = page.locator("#guest-name");
      await nameInput.waitFor({ state: "visible", timeout: 20_000 });
      await nameInput.click();
      await nameInput.pressSequentially(GUEST_NAME, { delay: 50 });
      await nameInput.press("Enter");
      await expect(page.getByText("Waiting to be admitted")).toBeVisible({ timeout: 20_000 });

      const renewalRequest = await renewal;
      expect(await renewalRequest.headerValue("authorization")).toMatch(/^Bearer \S+$/);
      expect((await renewalRequest.response())?.status()).toBe(200);

      await expect
        .poll(async () => (await fetchWaiting(HOST.email, HOST.name, meetingId)).length, {
          timeout: 10_000,
          message: "the host's waiting list holds the guest",
        })
        .toBe(1);

      const leave = page.waitForRequest(
        (r) =>
          r.method() === "POST" && r.url().endsWith(`/api/v1/meetings/${meetingId}/leave-guest`),
        { timeout: 15_000 },
      );
      await page.getByRole("button", { name: "Leave waiting room" }).click();
      const leaveRequest = await leave;
      expect(await leaveRequest.headerValue("authorization")).toMatch(/^Bearer \S+$/);
      expect((await leaveRequest.response())?.status()).toBe(200);

      await expect
        .poll(async () => (await fetchWaiting(HOST.email, HOST.name, meetingId)).length, {
          timeout: 10_000,
          message: "Leave removes the guest from the host's waiting list",
        })
        .toBe(0);
    } finally {
      await browser.close();
    }
  });
});
