import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Regression test for the cross-transport / cross-server display-name bug.
 *
 * Topology (docker-compose.e2e.yaml): WebSocket and WebTransport are served by
 * SEPARATE actix-api processes (`websocket-api` on :8080 and `webtransport-api`
 * on :4433/udp) that share one NATS. Each process keeps its own in-memory
 * `room_members`, so a peer on the WT server is NOT in the WS server's
 * `room_members` and vice-versa.
 *
 * The bug: when a WebTransport user joins first and a WebSocket user joins
 * second, the WS server's existing-member replay found nothing for the WT peer
 * (it lives on the other server) and the WT peer's deferred PARTICIPANT_JOINED
 * had already been published to NATS before the WS user subscribed — so the WS
 * user never learned the WT peer's display name and rendered the raw user_id
 * (email) instead.
 *
 * The fix: a freshly-subscribed joiner publishes a PARTICIPANT_LIST_REQUEST to
 * `room.{room}.system`; every Active peer on any server answers by re-publishing
 * its PARTICIPANT_JOINED (addressed to the requester). This test pins that the
 * WS joiner sees the WT peer's display name, not the email.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
];

type Transport = "webtransport" | "websocket";

async function createAuthenticatedContext(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  email: string,
  name: string,
  uiURL: string,
  transport: Transport,
): Promise<BrowserContext> {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });

  // Force the transport BEFORE any app script runs, on every navigation, by
  // seeding the sticky transport preference the UI reads from localStorage
  // (see protocol-selection.spec.ts). This pins the user to a single server.
  await context.addInitScript((t: string) => {
    try {
      localStorage.setItem("vc_transport_preference", t);
      localStorage.setItem("vc_transport_sticky", "true");
    } catch {
      // localStorage may be unavailable on about:blank; ignored — the app
      // origin sets it on the next navigation.
    }
  }, transport);

  const token = generateSessionToken(email, name);
  const url = new URL(uiURL);
  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
  return context;
}

/** Enter the meeting from the home page and land in the grid (or waiting room). */
async function enterMeeting(
  page: Page,
  meetingId: string,
  username: string,
): Promise<"in-meeting" | "waiting"> {
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

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    return "waiting";
  }
  if (result === "join") {
    await page.waitForTimeout(1000);
    await joinButton.click();
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

test.describe("Cross-transport display name (WT peer seen by WS joiner)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("WebSocket joiner sees a WebTransport peer's display name, not the email", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_xtransport_${Date.now()}`;

    const wtBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const wsBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      // Host connects over WebTransport (→ webtransport-api server).
      const wtCtx = await createAuthenticatedContext(
        wtBrowser,
        "host@videocall.rs",
        "HostUser",
        uiURL,
        "webtransport",
      );
      // Guest connects over WebSocket (→ websocket-api server, a DIFFERENT process).
      const wsCtx = await createAuthenticatedContext(
        wsBrowser,
        "guest@videocall.rs",
        "GuestUser",
        uiURL,
        "websocket",
      );

      const wtPage = await wtCtx.newPage();
      const wsPage = await wsCtx.newPage();

      // ---- WebTransport host joins FIRST and becomes the existing peer. ----
      const wtResult = await enterMeeting(wtPage, meetingId, "HostUser");
      expect(wtResult).toBe("in-meeting");

      // Give the host's election + deferred PARTICIPANT_JOINED time to settle
      // BEFORE the WS user subscribes — this is the exact ordering that used to
      // make the WS joiner miss the WT peer.
      await wtPage.waitForTimeout(3000);

      // ---- WebSocket guest joins SECOND. ----
      const wsResult = await enterMeeting(wsPage, meetingId, "GuestUser");

      if (wsResult === "waiting") {
        const admitButton = wtPage.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await wtPage.waitForTimeout(1000);
        await admitButton.dispatchEvent("click");

        const guestGrid = wsPage.locator("#grid-container");
        const guestJoinButton = wsPage.getByRole("button", {
          name: /Join Meeting|Start Meeting/,
        });
        const postAdmit = await Promise.race([
          guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
          guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
        ]);
        if (postAdmit === "join-button") {
          await wsPage.waitForTimeout(1000);
          await guestJoinButton.click();
        }
        await expect(guestGrid).toBeVisible({ timeout: 15_000 });
      }

      // ---- Both are in the grid and see one remote peer. ----
      await expect(wtPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(wsPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(wtPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });
      await expect(wsPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ---- CORE ASSERTION (the bug) ----
      // The WebSocket guest must see the WebTransport host's DISPLAY NAME on the
      // peer tile, with the email only in the tooltip. Before the fix this tile
      // showed "host@videocall.rs" as its visible text.
      const hostNameOnGuest = wsPage.locator(".floating-name", { hasText: "HostUser" });
      await expect(hostNameOnGuest.first()).toBeVisible({ timeout: 15_000 });
      await expect(hostNameOnGuest.first()).toHaveAttribute(
        "title",
        /^(Host: )?host@videocall\.rs$/,
      );

      // No tile label on the WS guest side may show a raw email.
      const guestFloatingNames = wsPage.locator(".floating-name");
      const guestCount = await guestFloatingNames.count();
      expect(guestCount).toBeGreaterThan(0);
      for (let i = 0; i < guestCount; i++) {
        const text = await guestFloatingNames.nth(i).textContent();
        expect(text ?? "").not.toContain("@");
      }

      // ---- Reverse direction (should already work): WT host sees WS guest's name. ----
      const guestNameOnHost = wtPage.locator(".floating-name", { hasText: "GuestUser" });
      await expect(guestNameOnHost.first()).toBeVisible({ timeout: 15_000 });
      await expect(guestNameOnHost.first()).toHaveAttribute("title", "guest@videocall.rs");
    } finally {
      await wtBrowser.close();
      await wsBrowser.close();
    }
  });
});
