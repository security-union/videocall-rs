import { test, expect, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { chromium } from "@playwright/test";

/**
 * Signal-quality popup — per-peer transport badge.
 *
 * Each remote peer's tile in the meeting grid renders a clickable signal-bars
 * icon (button with `aria-label="Show signal quality"`) that toggles the
 * `SignalQualityPopup` (`dioxus-ui/src/components/signal_quality.rs`). The
 * popup's header now renders a small WT / WS / em-dash badge inside a
 * `.popup-header-actions` cluster, indicating that peer's transport. CSS
 * classes mirror the diagnostics-popup badge:
 *
 *   - .connection-type type-webtransport  -> "WT", title="WebTransport"
 *   - .connection-type type-websocket     -> "WS", title="WebSocket"
 *   - .connection-type                    -> "—",  title="Transport unknown"
 *
 * Unlike the per-peer summary section in the diagnostics sidebar (which is
 * gated on `available_peers.len() > 2`), the signal-bars icon is rendered on
 * every remote peer tile. Two users (host + 1 guest) is sufficient to exercise
 * the popup — the host has a single remote peer whose tile carries the icon.
 *
 * In Playwright every browser defaults to "Auto" -> WebTransport, so the
 * remote peer should always report WT in this configuration.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

async function joinMeetingAs(
  context: BrowserContext,
  meetingId: string,
  username: string,
): Promise<Page> {
  const page = await context.newPage();
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

  return page;
}

/**
 * Click the "Start Meeting" / "Join Meeting" button and wait for the meeting
 * grid to appear. Mirrors `tests/diagnostics-peer-transport.spec.ts`.
 */
async function clickJoinAndEnterGrid(page: Page): Promise<void> {
  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  await expect(joinButton).toBeVisible({ timeout: 20_000 });
  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
}

test.describe("Signal-quality popup — per-peer transport badge", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("host opening the signal popup for a remote peer sees a WT/WS transport badge", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_sigq_xport_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sigq@videocall.rs", name: "SigQHost" },
        { email: "guest-sigq@videocall.rs", name: "SigQGuest" },
      ];

      // Spin up two authenticated contexts up front.
      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      // Host joins first so the meeting becomes "active" before the guest arrives.
      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);

      // Guest joins. Handle either direct-join or waiting-room admit flow.
      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);

      const joinButton = members[1].page.getByText(/Start Meeting|Join Meeting/);
      const waitingRoom = members[1].page.getByText("Waiting to be admitted");

      const result = await Promise.race([
        joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
        waitingRoom.waitFor({ timeout: 20_000 }).then(() => "waiting" as const),
      ]);

      if (result === "waiting") {
        const admitButton = members[0].page.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await members[0].page.waitForTimeout(1000);
        await admitButton.dispatchEvent("click");
        await members[0].page.waitForTimeout(3000);
        await expect(joinButton).toBeVisible({ timeout: 20_000 });
      }

      await clickJoinAndEnterGrid(members[1].page);

      // Wait for the mesh to settle — peer discovery + at least one
      // HeartbeatMetadata cycle so `peer_transport` flows through to the
      // signal-quality popup state.
      await members[0].page.waitForTimeout(8000);

      // Host should see exactly one remote peer tile in the grid.
      const hostPage = members[0].page;
      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
        timeout: 30_000,
      });

      // Open the signal-quality popup for the remote peer. The signal-bars
      // icon is the unique button with `aria-label="Show signal quality"`
      // inside the canvas container; with only one remote peer there is
      // exactly one such button to click.
      const signalButton = hostPage.locator(
        '#grid-container .canvas-container button[aria-label="Show signal quality"]',
      );
      await expect(signalButton).toBeVisible({ timeout: 15_000 });
      await signalButton.click();

      // Popup should appear and contain a header with the transport badge.
      const popup = hostPage.locator(".signal-quality-popup");
      await expect(popup).toBeVisible({ timeout: 10_000 });

      const badge = popup.locator(".popup-header .popup-header-actions .connection-type");
      await expect(badge).toBeVisible({ timeout: 15_000 });

      // The badge text resolves to either "WT" or "WS" once the first remote
      // heartbeat has been processed — never the em-dash placeholder. We poll
      // on text content so we wait through the first heartbeat tick rather
      // than introducing a fixed `waitForTimeout`.
      const expectedTransports = /^(WT|WS)$/;
      await expect(badge).toHaveText(expectedTransports, { timeout: 15_000 });

      // The badge must carry the matching CSS modifier class so we don't
      // regress to a stale "transport unknown" badge whose textContent
      // happened to include WT/WS via a child node.
      const cls = (await badge.getAttribute("class")) || "";
      expect(cls).toMatch(/\btype-(webtransport|websocket)\b/);

      // Title attribute should match the transport family, used as the
      // hover tooltip.
      const title = (await badge.getAttribute("title")) || "";
      expect(title).toMatch(/^(WebTransport|WebSocket)$/);

      // In Playwright every browser defaults to Auto -> WebTransport, so we
      // expect the remote peer to report WT. We assert this as a stronger
      // sanity check; if the default ever flips to WS this assertion will
      // need to be relaxed back to the WT|WS regex above.
      await expect(badge).toHaveText("WT");
      await expect(badge).toHaveClass(/type-webtransport/);
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });
});
