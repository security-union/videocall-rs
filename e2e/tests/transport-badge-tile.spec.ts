import { test, expect, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { setTransportBadgeFlag } from "../helpers/transport-badge-config";
import { chromium } from "@playwright/test";

/**
 * Per-tile transport badge (issue #1483).
 *
 * When the server-side `transportBadgeEnabled` flag is ON, every REMOTE peer
 * tile whose transport is known renders a small "WT"/"WS" pill inside
 * `.tile-top-icons`, adjacent to the `.signal-indicator` button:
 *
 *   - span.transport-badge.transport-badge--wt → text "WT",
 *     aria-label "Transport reported by peer: WebTransport"
 *   - span.transport-badge.transport-badge--ws → text "WS",
 *     aria-label "Transport reported by peer: WebSocket"
 *
 * Source of truth & gating (verified against
 * `dioxus-ui/src/components/peer_tile.rs` + `canvas_generator.rs`):
 *   1. SERVER-GATED, default OFF — the badge renders ONLY when
 *      `transport_badge_enabled()` (parsing `__APP_CONFIG.transportBadgeEnabled`
 *      through `videocall_types::truthy`) is true. The committed
 *      `dioxus-ui/scripts/config.js` ships `"false"`, so by default NO
 *      `.transport-badge` element exists anywhere.
 *   2. KNOWN-TRANSPORT-ONLY — the transport is read from the REMOTE
 *      `peer_status`/`peer_transport` diagnostics metric emitted by the decode
 *      pipeline. `webtransport` → `--wt`/"WT", `websocket` → `--ws`/"WS";
 *      anything else (`unknown`, empty, or not-yet-reported) → NO badge.
 *   3. REMOTE-PEER-ONLY — the local user's own session is filtered out of the
 *      peer-tile list (`attendants.rs`: `display_peers` excludes
 *      `get_own_session_id()`), and the local self-view is a `.host-video-wrapper`
 *      with NO `.tile-top-icons` and NO `.signal-indicator`. The local transport
 *      is not exposed on the public client API, so the local tile NEVER shows a
 *      badge — even with the flag ON.
 *
 * ## Why REAL camera-on browser peers (not addMockPeers)
 *
 * Mock peers ("mock-N") are video-OFF layout-only placeholders that never run
 * the decode pipeline, so they NEVER emit `peer_status`/`peer_transport` → no
 * badge ever appears on a mock tile. Like the other transport / video-tile
 * specs, this spec uses two REAL authenticated browser contexts joining the
 * same room, each with the camera seeded ON (`vc_prejoin_camera_on="true"`)
 * BEFORE navigation, so they actually publish, get decoded by the peer, and
 * trigger the remote `peer_status` emit that carries the transport.
 *
 * ## Which transport (WT vs WS) the badge shows
 *
 * Authenticated contexts default to WebSocket: `createAuthenticatedContext`
 * injects `DEFAULT_WEBSOCKET_TRANSPORT_INIT_SCRIPT`, which seeds
 * `vc_transport_preference="websocket"` when unset. But WebTransport is enabled
 * in the stack (`config.js: webTransportEnabled "true"`) and a context CAN end
 * up on WT, and the local dev WT cert may or may not be reachable in CI. The
 * existing transport specs therefore accept EITHER transport, and so do we: the
 * positive test asserts the badge is exactly one of the two valid, mutually
 * exclusive states — never an unclassified/empty badge — and that its class,
 * text, and aria-label all agree on the SAME transport.
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
  opts: { ensureCameraOn?: boolean } = {},
): Promise<Page> {
  const page = await context.newPage();
  if (opts.ensureCameraOn) {
    await page.addInitScript(() => {
      try {
        window.localStorage.setItem("vc_prejoin_camera_on", "true");
      } catch {
        /* storage may be unavailable before origin navigation */
      }
    });
  }

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

async function ensurePrejoinCameraOn(page: Page): Promise<void> {
  const allow = page.locator('[data-testid="prejoin-permission-allow"]');
  if (await allow.isVisible().catch(() => false)) {
    await allow.click();
    await page
      .locator('[data-testid="prejoin-permission-prompt"]')
      .waitFor({ state: "hidden", timeout: 15_000 })
      .catch(() => {
        /* already granted / prompt absent */
      });
  }

  const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
  if (!(await cameraToggle.isVisible().catch(() => false))) {
    return;
  }

  if ((await cameraToggle.getAttribute("aria-pressed")) !== "true") {
    await cameraToggle.click();
  }
  await expect(cameraToggle).toHaveAttribute("aria-pressed", "true", { timeout: 5_000 });

  await expect
    .poll(
      async () =>
        page
          .locator('[data-testid="prejoin-camera-preview"]')
          .evaluate((el) => {
            const v = el as HTMLVideoElement;
            const s = v.srcObject as MediaStream | null;
            return s ? s.getVideoTracks().filter((t) => t.readyState === "live").length : 0;
          })
          .catch(() => 0),
      { timeout: 15_000 },
    )
    .toBeGreaterThan(0);
}

/**
 * Click the "Start Meeting" / "Join Meeting" button (when present) and wait for
 * the meeting grid to appear. Mirrors `signal-quality-peer-transport.spec.ts`.
 */
async function clickJoinAndEnterGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "join") {
    await ensurePrejoinCameraOn(page);
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/**
 * Drive the guest from the prejoin/waiting-room state into the meeting grid,
 * admitting from the host page if a waiting room appears. Mirrors the
 * host+guest handshake in `signal-quality-peer-transport.spec.ts`.
 */
async function admitAndEnterGrid(hostPage: Page, guestPage: Page): Promise<void> {
  const joinButton = guestPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = guestPage.getByText("Waiting to be admitted");
  const guestGrid = guestPage.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);
  }

  if (result !== "auto-joined") {
    await clickJoinAndEnterGrid(guestPage);
  } else {
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }
}

/**
 * Bring up two authenticated browser contexts (host + guest), optionally
 * flipping `transportBadgeEnabled` ON for both BEFORE their first navigation,
 * and join them into `meetingId` with cameras ON. Returns the two members; the
 * caller is responsible for tearing down `members` + `browsers` in a `finally`.
 */
async function bringUpTwoPeerMeeting(
  uiURL: string,
  meetingId: string,
  profiles: { email: string; name: string }[],
  opts: { enableBadgeFlag: boolean },
): Promise<{ members: MeetingMember[]; browsers: Awaited<ReturnType<typeof chromium.launch>>[] }> {
  const browsers = await Promise.all([
    chromium.launch({ args: BROWSER_ARGS }),
    chromium.launch({ args: BROWSER_ARGS }),
  ]);

  const members: MeetingMember[] = [];

  for (let i = 0; i < 2; i++) {
    const ctx = await createAuthenticatedContext(
      browsers[i],
      profiles[i].email,
      profiles[i].name,
      uiURL,
    );
    // Flip the flag BEFORE any navigation so the very first `/config.js`
    // request is intercepted (the route is context-scoped).
    if (opts.enableBadgeFlag) {
      await setTransportBadgeFlag(ctx, "true");
    }
    members.push({
      page: null as unknown as Page,
      context: ctx,
      email: profiles[i].email,
      name: profiles[i].name,
    });
  }

  // Host joins first so the meeting becomes "active" before the guest arrives.
  members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name, {
    ensureCameraOn: true,
  });
  await clickJoinAndEnterGrid(members[0].page);

  // Guest joins and is admitted (handles waiting-room or direct-join).
  members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name, {
    ensureCameraOn: true,
  });
  await admitAndEnterGrid(members[0].page, members[1].page);

  return { members, browsers };
}

async function tearDown(
  members: MeetingMember[],
  browsers: Awaited<ReturnType<typeof chromium.launch>>[],
): Promise<void> {
  for (const m of members) {
    if (m.page) {
      await m.page.close().catch(() => undefined);
    }
    await m.context.close().catch(() => undefined);
  }
  await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
}

test.describe("Per-tile transport badge (#1483)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("flag ON: a remote peer tile shows a WT/WS transport badge with matching class, text and aria-label", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_xport_badge_on_${Date.now()}`;

    const profiles = [
      { email: "host-xbadge@videocall.rs", name: "XBadgeHost" },
      { email: "guest-xbadge@videocall.rs", name: "XBadgeGuest" },
    ];

    const { members, browsers } = await bringUpTwoPeerMeeting(uiURL, meetingId, profiles, {
      enableBadgeFlag: true,
    });

    try {
      const hostPage = members[0].page;

      // Host should see exactly one REMOTE peer tile in the grid (the local
      // user's own session is filtered out of the peer-tile list).
      const remoteTile = hostPage.locator("#grid-container .canvas-container");
      await expect(remoteTile).toHaveCount(1, { timeout: 30_000 });

      // The badge sits inside the remote tile's `.tile-top-icons` cluster,
      // adjacent to the signal-meter button. Poll until it renders — it only
      // appears after the first remote `peer_status` heartbeat reports the
      // transport, so we wait through that tick rather than using a fixed sleep.
      const badge = remoteTile.locator(".tile-top-icons .transport-badge");
      await expect(badge).toHaveCount(1, { timeout: 60_000 });
      await expect(badge).toBeVisible();

      // It must be EXACTLY one of the two valid, mutually exclusive states —
      // never an unclassified badge. Read the resolved transport from the
      // modifier class, then assert class + text + aria-label all agree on the
      // SAME transport (so a regression that crosses the wires — e.g. WT class
      // with "WS" text — fails).
      const cls = (await badge.getAttribute("class")) || "";
      expect(cls).toMatch(/\btransport-badge\b/);
      expect(cls).toMatch(/\btransport-badge--(wt|ws)\b/);

      const isWt = /\btransport-badge--wt\b/.test(cls);
      const isWs = /\btransport-badge--ws\b/.test(cls);
      // XOR: exactly one of the two modifier classes is present.
      expect(isWt).not.toBe(isWs);

      if (isWt) {
        await expect(badge).toHaveText("WT");
        await expect(badge).toHaveAttribute(
          "aria-label",
          "Transport reported by peer: WebTransport",
        );
        // Negative cross-check: the WS modifier must be absent.
        expect(cls).not.toMatch(/\btransport-badge--ws\b/);
      } else {
        await expect(badge).toHaveText("WS");
        await expect(badge).toHaveAttribute("aria-label", "Transport reported by peer: WebSocket");
        expect(cls).not.toMatch(/\btransport-badge--wt\b/);
      }

      // The badge lives next to the signal-meter button in the same icon
      // cluster (its documented placement), proving it rendered in the intended
      // location rather than leaking elsewhere in the tile.
      const iconCluster = remoteTile.locator(".tile-top-icons");
      await expect(iconCluster.locator("button.signal-indicator")).toHaveCount(1);
      await expect(iconCluster.locator(".transport-badge")).toHaveCount(1);
    } finally {
      await tearDown(members, browsers);
    }
  });

  test("flag ON: the local user's own self-view tile shows NO transport badge (remote-only)", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_xport_badge_local_${Date.now()}`;

    const profiles = [
      { email: "host-xblocal@videocall.rs", name: "XBLocalHost" },
      { email: "guest-xblocal@videocall.rs", name: "XBLocalGuest" },
    ];

    const { members, browsers } = await bringUpTwoPeerMeeting(uiURL, meetingId, profiles, {
      enableBadgeFlag: true,
    });

    try {
      const hostPage = members[0].page;

      // Wait until the remote badge has rendered, so we know the flag is ON and
      // the badge code path is live in this session — otherwise "no badge on
      // the local tile" would pass vacuously even if the feature were broken.
      const remoteBadge = hostPage.locator(
        "#grid-container .canvas-container .tile-top-icons .transport-badge",
      );
      await expect(remoteBadge).toHaveCount(1, { timeout: 60_000 });

      // The local self-view is rendered as `.host-video-wrapper` (host.rs); it
      // has no `.tile-top-icons` and must carry NO transport badge even with the
      // flag ON, because the local transport is not exposed to the badge.
      const selfView = hostPage.locator(".host-video-wrapper");
      await expect(selfView).toHaveCount(1, { timeout: 15_000 });
      await expect(selfView.locator(".transport-badge")).toHaveCount(0);

      // And the ONLY badge on the whole PAGE is the single remote one — the
      // local self-view contributes nothing. (Total badge count == remote peers
      // == 1.) Scoping page-wide (not to `#grid-container`) makes this
      // independent of whether the self-view is nested inside the grid or a
      // sibling of it: either way, exactly one badge exists, and it is remote.
      await expect(hostPage.locator(".transport-badge")).toHaveCount(1);
    } finally {
      await tearDown(members, browsers);
    }
  });

  test("flag OFF (default config): NO transport badge renders on any tile — the server gate", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_xport_badge_off_${Date.now()}`;

    const profiles = [
      { email: "host-xboff@videocall.rs", name: "XBOffHost" },
      { email: "guest-xboff@videocall.rs", name: "XBOffGuest" },
    ];

    // enableBadgeFlag: false → use the committed default
    // (`transportBadgeEnabled: "false"`), so the server gate is OFF. If someone
    // deletes the `transport_badge_enabled()` gate in peer_tile.rs, the badge
    // would render here and this test FAILS — that is the mutation this guards.
    const { members, browsers } = await bringUpTwoPeerMeeting(uiURL, meetingId, profiles, {
      enableBadgeFlag: false,
    });

    try {
      const hostPage = members[0].page;

      // Confirm a real remote peer tile actually rendered, so the "no badge"
      // assertion is meaningful (the tile that WOULD carry a badge exists and
      // has its signal-meter button) rather than vacuously true on an empty grid.
      const remoteTile = hostPage.locator("#grid-container .canvas-container");
      await expect(remoteTile).toHaveCount(1, { timeout: 30_000 });
      await expect(remoteTile.locator(".tile-top-icons button.signal-indicator")).toHaveCount(1, {
        timeout: 30_000,
      });

      // Give the remote `peer_status` heartbeats ample time to flow — if the
      // gate were removed the badge would have appeared within this window
      // (the flag-ON test resolves the badge well inside 60s). A short settle
      // here makes the negative assertion robust rather than racing the first
      // heartbeat.
      await hostPage.waitForTimeout(8000);

      // The gate: with the flag OFF, NO `.transport-badge` exists anywhere in
      // the grid, on any tile.
      await expect(hostPage.locator("#grid-container .transport-badge")).toHaveCount(0);
      await expect(hostPage.locator(".transport-badge")).toHaveCount(0);
    } finally {
      await tearDown(members, browsers);
    }
  });
});
