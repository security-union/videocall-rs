import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { joinMeetingFromPage } from "../helpers/two-user-meeting";
import { createMeeting, fetchMeetingState } from "../helpers/meeting-api";
import { waitForServices } from "../helpers/wait-for-services";

interface User {
  email: string;
  name: string;
}

interface Session {
  browser: Browser;
  context: BrowserContext;
  page: Page;
}

type JoinResult = Awaited<ReturnType<typeof joinMeetingFromPage>>;

// videocall-meeting-types/src/presence.rs / meeting-api/src/nats_consumers.rs
const PRESENCE_LEASE_SECS = 90;
const PRESENCE_SWEEP_INTERVAL_SECS = 30;
const SWEEP_MARGIN_SECS = 50;
const SWEEP_WAIT_MS =
  (PRESENCE_LEASE_SECS + PRESENCE_SWEEP_INTERVAL_SECS + SWEEP_MARGIN_SECS) * 1000;

async function launch(uiURL: string, user: User): Promise<Session> {
  const browser = await chromium.launch({ args: BROWSER_ARGS });
  const context = await createAuthenticatedContext(browser, user.email, user.name, uiURL);
  const page = await context.newPage();
  return { browser, context, page };
}

async function closeAll(sessions: Session[]): Promise<void> {
  await Promise.all(sessions.map((s) => s.browser.close()));
}

async function enterMeeting(page: Page, meetingId: string, user: User): Promise<JoinResult> {
  await fillAndSubmitJoinForm(page, meetingId, user.name);
  return joinMeetingFromPage(page);
}

test.describe("Presence second-tab sweep", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a connected host and peer stay in the call after the owner's second-tab lobby opens and closes unjoined", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_presence_sweep_${Date.now()}`;
    const OWNER = { email: "presence-sweep-owner@videocall.rs", name: "SweepOwner" };
    const PEER = { email: "presence-sweep-peer@videocall.rs", name: "SweepPeer" };
    await createMeeting(OWNER.email, OWNER.name, { meetingId, waitingRoomEnabled: false });

    const owner = await launch(uiURL, OWNER);
    const peer = await launch(uiURL, PEER);
    try {
      expect(await enterMeeting(owner.page, meetingId, OWNER)).toBe("in-meeting");
      expect(await enterMeeting(peer.page, meetingId, PEER)).toBe("in-meeting");

      const secondTab = await owner.context.newPage();
      await secondTab.goto(`/meeting/${meetingId}`);
      const startButton = secondTab.getByRole("button", { name: "Start Meeting" });
      await expect(startButton).toBeVisible({ timeout: 20_000 });
      await secondTab.close();

      await owner.page.waitForTimeout(SWEEP_WAIT_MS);

      await expect(owner.page.locator(".meeting-ended-message")).toHaveCount(0);
      await expect(peer.page.locator(".meeting-ended-message")).toHaveCount(0);
      await expect(owner.page.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(peer.page.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      await expect
        .poll(() => fetchMeetingState(OWNER.email, OWNER.name, meetingId), { timeout: 20_000 })
        .toBe("active");
    } finally {
      await closeAll([owner, peer]);
    }
  });
});
