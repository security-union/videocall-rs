import { test, expect, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { chromium } from "@playwright/test";

/**
 * Diagnostics popup — per-peer transport badge.
 *
 * The diagnostics sidebar (`#diagnostics-sidebar`) renders a "Per-Peer Summary"
 * section that lists each remote peer with a small WT / WS / em-dash badge
 * indicating the transport that peer is connected via. The badge is rendered
 * by `dioxus-ui/src/components/diagnostics.rs` near the buffer/jitter section
 * and uses CSS classes:
 *
 *   - .connection-type type-webtransport  -> "WT"
 *   - .connection-type type-websocket     -> "WS"
 *   - .connection-type                    -> "—" (transport unknown)
 *
 * Source of truth: each peer stamps its own transport into its periodic
 * HeartbeatMetadata proto. Receivers track the latest value per peer and
 * forward it via a `peer_transport` text metric on the existing peer_status
 * DiagEvent.
 *
 * NOTE on three users: the Per-Peer Summary section is gated on
 * `available_peers.len() > 2` (i.e. >= 2 remote peers). With only two users
 * in the meeting each side sees a single remote peer and the Per-Peer Summary
 * section is not rendered. To exercise the badge we need three users so that
 * each side sees at least two remote peers.
 *
 * In Playwright, every browser defaults to "Auto" -> WebTransport, so all
 * three peers should show a WT badge for each other.
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
 * Click the "Start Meeting" / "Join Meeting" button for an admitted user
 * (no waiting room) and wait for the meeting grid to appear.
 *
 * We deliberately DO NOT handle the waiting-room transition here; this test
 * uses the default "no waiting room enabled" path by relying on the host
 * having already joined first so subsequent users go straight to the grid.
 * If your local stack enables the waiting room by default, see
 * `tests/two-users-meeting.spec.ts` for the admit flow.
 */
async function clickJoinAndEnterGrid(page: Page): Promise<void> {
  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  await expect(joinButton).toBeVisible({ timeout: 20_000 });
  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
}

/**
 * Open the diagnostics sidebar via the "Open Diagnostics" tooltip button.
 * Mirrors `tests/protocol-selection.spec.ts::openDiagnosticsPanel`.
 */
async function openDiagnosticsPanel(page: Page): Promise<void> {
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await diagButton.click();
  await expect(page.locator("#diagnostics-sidebar")).toBeVisible({ timeout: 10_000 });
  // The Transport Preference section is rendered eagerly when the sidebar
  // opens; waiting on its h3 confirms the panel rendered cleanly.
  await expect(page.locator("h3", { hasText: "Transport Preference" })).toBeVisible({
    timeout: 10_000,
  });
}

test.describe("Diagnostics popup — per-peer transport badge", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("three users see WT/WS transport badge for each remote peer", async ({ baseURL }) => {
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_diag_xport_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-diag@videocall.rs", name: "DiagHost" },
        { email: "guest1-diag@videocall.rs", name: "DiagGuest1" },
        { email: "guest2-diag@videocall.rs", name: "DiagGuest2" },
      ];

      // Spin up three authenticated contexts up front.
      for (let i = 0; i < 3; i++) {
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

      // Host joins first so the meeting becomes "active" before guests arrive.
      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);

      // Guests join sequentially. If the local stack enables the waiting
      // room, the host needs to admit them; we handle both shapes via a
      // race on the post-fill state, similar to `two-users-meeting.spec.ts`.
      for (let i = 1; i < 3; i++) {
        members[i].page = await joinMeetingAs(members[i].context, meetingId, profiles[i].name);

        const joinButton = members[i].page.getByText(/Start Meeting|Join Meeting/);
        const waitingRoom = members[i].page.getByText("Waiting to be admitted");

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

        await clickJoinAndEnterGrid(members[i].page);
      }

      // Wait for the three-way mesh to settle — peer discovery + at least
      // one HeartbeatMetadata cycle so `peer_transport` flows through to the
      // diagnostics subscriber.
      await members[0].page.waitForTimeout(8000);

      // Each user should see two remote peer tiles in the grid.
      for (const m of members) {
        await expect(m.page.locator("#grid-container .canvas-container")).toHaveCount(2, {
          timeout: 30_000,
        });
      }

      // Open the diagnostics sidebar on the host and assert the per-peer
      // transport badge.
      await openDiagnosticsPanel(members[0].page);

      // The "Per-Peer Summary" section is gated on `available_peers.len() > 2`,
      // i.e. more than one remote peer. With three users the host has two
      // remote peers, so the section is rendered.
      const summarySection = members[0].page.locator(".diagnostics-section", {
        has: members[0].page.locator("h3", { hasText: "Per-Peer Summary" }),
      });
      await expect(summarySection).toBeVisible({ timeout: 15_000 });

      const peerItems = summarySection.locator(".peer-summary-item");
      await expect(peerItems).toHaveCount(2, { timeout: 15_000 });

      // Each peer row must contain a `.connection-type` badge. The badge
      // should resolve to either "WT" or "WS" once the first remote
      // heartbeat has been processed — never the em-dash placeholder.
      // We poll on text content so we wait through the first heartbeat
      // tick rather than introducing a fixed `waitForTimeout`.
      const expectedTransports = /^(WT|WS)$/;

      for (let i = 0; i < 2; i++) {
        const row = peerItems.nth(i);
        const badge = row.locator(".connection-type");
        await expect(badge).toBeVisible({ timeout: 15_000 });
        await expect(badge).toHaveText(expectedTransports, { timeout: 15_000 });

        // The badge should also carry the matching CSS modifier class, so
        // we don't regress to a stale "transport unknown" badge whose
        // textContent happened to include WT/WS via a child node.
        const cls = (await badge.getAttribute("class")) || "";
        expect(cls).toMatch(/\btype-(webtransport|websocket)\b/);

        // Title attribute should match the transport family, used as the
        // hover tooltip.
        const title = (await badge.getAttribute("title")) || "";
        expect(title).toMatch(/^(WebTransport|WebSocket)$/);
      }

      // In Playwright every browser defaults to Auto -> WebTransport, so we
      // expect both remote peers to report WT. We assert this as a stronger
      // sanity check; if the default ever flips to WS this assertion will
      // need to be relaxed back to the WT|WS regex above.
      for (let i = 0; i < 2; i++) {
        const badge = peerItems.nth(i).locator(".connection-type");
        await expect(badge).toHaveText("WT");
        await expect(badge).toHaveClass(/type-webtransport/);
      }
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
