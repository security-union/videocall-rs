import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Peer-leave canvas-node stability regression (issue 508).
 *
 * ## What issue 508 was
 *
 * When a meeting collapsed from 2 visible tiles to 1, the surviving
 * non-screen-share peer's tile used to switch from the REGULAR-GRID `rsx!`
 * template to a SEPARATE FULL-BLEED `rsx!` template (a distinct
 * `return rsx!{...}` branch in `canvas_generator.rs::generate_for_peer`).
 * Dioxus 0.7 diffs by template-pointer IDENTITY, so swapping which template
 * produced the tile tore down the surviving peer's `<canvas>` DOM node and
 * rebuilt the renderer from scratch. The rebuilt canvas came up with
 * `last_width: 0`, took the resize branch on the first frame, and the inbound
 * decode FPS COLLAPSED on every 2<->1 transition.
 *
 * ## The fix
 *
 * The separate full-bleed branch was DELETED. `full_bleed` is now a plain
 * CLASS toggle (`grid-item full-bleed`) WITHIN the single grid template, so
 * the `<canvas>` is emitted from ONE template position. Dioxus therefore
 * diffs the tile IN PLACE across the 2->1 transition and REUSES the same
 * `<canvas>` node — no teardown, no renderer rebuild, no FPS collapse.
 *
 * ## What this spec asserts (and why it is a real regression test)
 *
 * Two REAL camera-ON browser peers join the host's meeting (3 contexts:
 * host + guestA + guestB). Both guests publish video — `vc_prejoin_camera_on`
 * is seeded to `"true"` before boot, mirroring crop-toggle.spec.ts; mock
 * peers are video-OFF placeholders and would render NO `<canvas>`, so they
 * cannot be used here. With both guest tiles up, this spec:
 *
 *   1. Stamps a unique JS property (`__vc508Marker`) directly onto EVERY
 *      camera-on peer `<canvas>` DOM node currently on the host's grid, and
 *      records each canvas id (== peer session_id). Marking BOTH canvases up
 *      front means we never have to guess which guest will survive — whichever
 *      one remains was already marked while it was a 2-up grid tile.
 *   2. Closes ONE guest's page so the host's grid collapses 2 -> 1 and the
 *      survivor's tile becomes the full-bleed single tile.
 *   3. After the collapse, asserts:
 *        a. exactly ONE `.grid-item.full-bleed` exists (the full-bleed CLASS
 *           is preserved by the unified template);
 *        b. that full-bleed tile CONTAINS a `<canvas>` (the camera-on
 *           survivor renders UserVideo, not a placeholder);
 *        c. the survivor's `<canvas>` STILL carries the `__vc508Marker`
 *           property stamped in step 1 — proving it is the SAME DOM node, i.e.
 *           it was NOT torn down and recreated by a template swap.
 *
 * ## How this FAILS if the fix is reverted
 *
 * If the separate full-bleed `return rsx!{...}` branch were restored, the
 * 2->1 collapse would render the survivor's tile from a DIFFERENT template
 * pointer than the grid template that produced it while 2 tiles were up.
 * Dioxus would tear down the old `<canvas>` node and create a fresh one. The
 * fresh node would NOT carry `__vc508Marker` (a JS property set on the old
 * node does not transfer to a newly-created element with the same id). So
 * assertion (c) — `markerSurvived === true` — would be FALSE and the test
 * would FAIL. That lost-marker is the concrete teardown signal issue 508 is
 * about: a new canvas node === renderer rebuilt === FPS collapse. (Assertions
 * (a)/(b) still pass under the reverted code because the old full-bleed
 * template also emitted `.grid-item.full-bleed` + a `<canvas>`; the
 * node-identity marker is what distinguishes "reused" from "rebuilt".)
 *
 * ## CI note
 *
 * Unlike peer-canvas-aspect.spec.ts (#1093), this spec asserts a DOM-node
 * IDENTITY property, NOT painted pixels — it does not need the WebCodecs
 * decode+paint path to flow. It only needs both guests to mount a `<canvas>`
 * tile (camera published), which crop-toggle.spec.ts already exercises green
 * with the same 2/3-context camera-on setup. A fail-loud precondition makes a
 * no-canvas run an explicit failure (not a silent pass) so a missing-camera
 * environment cannot mask a real regression.
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

async function createAuthenticatedContext(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  email: string,
  name: string,
  uiURL: string,
): Promise<BrowserContext> {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });
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

  if (result === "waiting") return "waiting";
  if (result === "waiting-for-meeting") return "waiting-for-meeting";
  if (result === "auto-joined") return "in-meeting";

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult === "in-meeting") return;

  if (guestResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

    const guestJoinButton = guestPage.getByRole("button", {
      name: /Join Meeting|Start Meeting/,
    });
    const guestGrid = guestPage.locator("#grid-container");

    const postAdmit = await Promise.race([
      guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
      guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);

    if (postAdmit === "join-button") {
      await guestPage.waitForTimeout(1000);
      await guestJoinButton.click();
      await guestPage.waitForTimeout(3000);
      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    }
  }
}

interface Member {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

/**
 * Stamp a unique JS marker property on EVERY camera-on peer `<canvas>` DOM
 * node on the host's grid, and return the marked canvas ids. A peer VIDEO
 * canvas has an all-digit id (== peer session_id); screen-share canvases
 * (`screen-share-*`) are excluded. Returns the list of marked ids so the
 * caller can assert at least 2 were present pre-collapse.
 */
async function markAllPeerCanvases(hostPage: Page, markerValue: string): Promise<string[]> {
  return hostPage.evaluate((marker) => {
    const canvases = Array.from(
      document.querySelectorAll<HTMLCanvasElement>(
        "#grid-container .grid-item .canvas-container canvas",
      ),
    );
    const ids: string[] = [];
    for (const c of canvases) {
      if (c.id && !c.id.startsWith("screen-share-") && /^\d+$/.test(c.id)) {
        // Stamp an identity marker directly on the DOM node. A brand-new
        // element created by a template-swap teardown would NOT carry this
        // property, so its survival proves node REUSE (issue 508 fix).
        (c as unknown as Record<string, string>).__vc508Marker = marker;
        ids.push(c.id);
      }
    }
    return ids;
  }, markerValue);
}

/**
 * After the collapse, read the post-state of the single surviving full-bleed
 * tile: how many full-bleed tiles exist, whether the full-bleed tile contains
 * a `<canvas>`, the surviving peer canvas id, and whether that canvas still
 * carries the marker stamped pre-collapse (=> same DOM node, NOT rebuilt).
 */
async function readCollapsedState(
  hostPage: Page,
  expectedMarker: string,
): Promise<{
  fullBleedCount: number;
  fullBleedHasCanvas: boolean;
  survivorCanvasId: string | null;
  survivorMarkerSurvived: boolean;
}> {
  return hostPage.evaluate((marker) => {
    const fullBleedTiles = Array.from(
      document.querySelectorAll<HTMLElement>("#grid-container .grid-item.full-bleed"),
    );
    // The survivor canvas is the peer-video canvas inside a full-bleed tile.
    let survivorCanvasId: string | null = null;
    let survivorMarkerSurvived = false;
    let fullBleedHasCanvas = false;
    for (const tile of fullBleedTiles) {
      const canvas = tile.querySelector<HTMLCanvasElement>(".canvas-container canvas");
      if (canvas) {
        fullBleedHasCanvas = true;
        if (canvas.id && !canvas.id.startsWith("screen-share-") && /^\d+$/.test(canvas.id)) {
          survivorCanvasId = canvas.id;
          survivorMarkerSurvived =
            (canvas as unknown as Record<string, string>).__vc508Marker === marker;
        }
      }
    }
    return {
      fullBleedCount: fullBleedTiles.length,
      fullBleedHasCanvas,
      survivorCanvasId,
      survivorMarkerSurvived,
    };
  }, expectedMarker);
}

test.describe("Peer-leave canvas-node stability (issue 508)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("surviving camera-on tile reuses the SAME <canvas> node across 2->1 collapse", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_508_leave_${Date.now()}`;

    // Three browsers: host + two camera-on guests.
    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: Member[] = [];

    try {
      const profiles = [
        { email: "host-508@videocall.rs", name: "Leave508Host" },
        { email: "guestA-508@videocall.rs", name: "Leave508GuestA" },
        { email: "guestB-508@videocall.rs", name: "Leave508GuestB" },
      ];

      for (let i = 0; i < 3; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        // Seed camera-on BEFORE page creation so load_preferred_camera_on()
        // reads true and the guest publishes VIDEO (=> a real <canvas> tile).
        await ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      // Host joins first, then admits each guest.
      members[0].page = await members[0].context.newPage();
      await navigateToMeeting(members[0].page, meetingId, profiles[0].name);
      const hostResult = await joinMeetingFromPage(members[0].page);
      expect(hostResult).toBe("in-meeting");

      for (let i = 1; i < 3; i++) {
        members[i].page = await members[i].context.newPage();
        await navigateToMeeting(members[i].page, meetingId, profiles[i].name);
        const guestResult = await joinMeetingFromPage(members[i].page);
        await admitGuestIfNeeded(members[0].page, members[i].page, guestResult);
      }

      const hostPage = members[0].page;

      // Wait for BOTH guest tiles to mount on the host's grid. With 2 remote
      // peers the host's container carries `participants-2`.
      const grid2 = hostPage.locator("#grid-container.participants-2");
      await expect(grid2).toBeVisible({ timeout: 60_000 });
      await expect(hostPage.locator("#grid-container .grid-item")).toHaveCount(2, {
        timeout: 60_000,
      });

      // Fail-loud precondition: BOTH camera-on peer tiles must have a live
      // <canvas>. If fewer than 2 are present, the guests' cameras did not all
      // publish (vc_prejoin_camera_on not seeded / fake device denied), and the
      // node-identity assertion below would be vacuous — so fail explicitly
      // here rather than silently pass.
      const canvasTiles = hostPage.locator("#grid-container .grid-item .canvas-container canvas");
      await expect(canvasTiles.first()).toBeVisible({ timeout: 30_000 });

      // Stamp a marker on EVERY camera-on peer canvas while 2 tiles are up.
      // Marking both means whichever guest survives the leave was already
      // marked as a 2-up GRID tile — no need to predict which one stays.
      const marker = `vc508-${Date.now()}`;
      const markedIds = await markAllPeerCanvases(hostPage, marker);
      expect(
        markedIds.length,
        `Precondition failed: expected 2 camera-on peer <canvas> nodes pre-collapse, ` +
          `found ${markedIds.length}. Guest cameras likely did not publish — ` +
          `check vc_prejoin_camera_on seed.`,
      ).toBe(2);

      // Close ONE guest (guestB, the last joiner). The host collapses 2 -> 1.
      await members[2].page.close().catch(() => undefined);
      members[2].page = null as unknown as Page;
      await members[2].context.close().catch(() => undefined);

      // Wait for the host to collapse 2 -> 1: container becomes
      // `participants-1` and exactly one `.grid-item` remains.
      const grid1 = hostPage.locator("#grid-container.participants-1");
      await expect(grid1).toBeVisible({ timeout: 60_000 });
      await expect(hostPage.locator("#grid-container .grid-item")).toHaveCount(1, {
        timeout: 60_000,
      });

      // Give Dioxus a beat to run the 2->1 diff/patch.
      await hostPage.waitForTimeout(2000);

      const state = await readCollapsedState(hostPage, marker);

      // (a) full-bleed CLASS preserved by the unified template: exactly one.
      expect(state.fullBleedCount).toBe(1);

      // (b) the full-bleed survivor renders a <canvas> (camera-on), not a
      //     placeholder.
      expect(state.fullBleedHasCanvas).toBe(true);
      expect(state.survivorCanvasId).not.toBeNull();

      // Sanity: the survivor is one of the two canvases we marked pre-collapse
      // (it must be — it never left), so the marker test below is meaningful.
      expect(markedIds).toContain(state.survivorCanvasId);

      // (c) THE issue-508 ASSERTION: the survivor's <canvas> is the SAME DOM
      //     node it was before the collapse — the marker stamped on the node
      //     survived. If the separate full-bleed template were restored, the
      //     node would be torn down + recreated and this marker would be gone.
      expect(
        state.survivorMarkerSurvived,
        "issue 508 regression: the surviving peer's <canvas> was torn down and " +
          "recreated across the 2->1 collapse (marker lost) — the full-bleed " +
          "template swap is back and the renderer was rebuilt (FPS collapse).",
      ).toBe(true);
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
